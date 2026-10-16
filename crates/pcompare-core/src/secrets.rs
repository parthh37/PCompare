//! Where saved connections keep their passwords.
//!
//! Remote profiles and database connections both store secrets through this
//! trait; the app plugs in a sealed file keyed from the OS keychain (see
//! `pcompare_remote::profile::KeyringSecrets`) and tests use
//! [`MemorySecrets`].

use crate::Result;
use std::collections::HashMap;
use std::sync::Mutex;

pub trait SecretStore: Send + Sync {
    fn get(&self, profile: &str, key: &str) -> Result<Option<String>>;
    fn set(&self, profile: &str, key: &str, value: &str) -> Result<()>;
    fn delete(&self, profile: &str, key: &str) -> Result<()>;
}

impl<S: SecretStore + ?Sized> SecretStore for &S {
    fn get(&self, profile: &str, key: &str) -> Result<Option<String>> {
        (**self).get(profile, key)
    }
    fn set(&self, profile: &str, key: &str, value: &str) -> Result<()> {
        (**self).set(profile, key, value)
    }
    fn delete(&self, profile: &str, key: &str) -> Result<()> {
        (**self).delete(profile, key)
    }
}

/// Longest value one keychain entry can hold, in UTF-16 code units.
/// Windows Credential Manager caps a credential at 2560 bytes and the
/// keychain stores text as UTF-16; the other platforms allow more.
pub const MAX_ENTRY_UNITS: usize = 1280;

/// Marks a head entry whose value lives in `<key>#1..=n`. Starts with a
/// control character, which no token or password contains.
const CHUNK_MARKER: &str = "\u{1}pcompare-chunks:";

/// Entry holding part `n` (from 1) of a chunked `key`.
pub fn part_key(key: &str, n: usize) -> String {
    format!("{key}#{n}")
}

/// Spreads values too long for one entry (modern OAuth tokens run past
/// 1280 characters) across several, so callers never see the limit.
pub struct ChunkedSecrets<S> {
    inner: S,
    max_units: usize,
}

impl<S: SecretStore> ChunkedSecrets<S> {
    pub fn new(inner: S) -> Self {
        Self::with_limit(inner, MAX_ENTRY_UNITS)
    }

    pub fn with_limit(inner: S, max_units: usize) -> Self {
        assert!(max_units >= CHUNK_MARKER.len() + 10, "limit too small");
        Self { inner, max_units }
    }

    /// How many parts the head entry currently points at (0 = not chunked).
    fn stored_parts(&self, profile: &str, key: &str) -> Result<usize> {
        Ok(self
            .inner
            .get(profile, key)?
            .and_then(|v| v.strip_prefix(CHUNK_MARKER)?.parse().ok())
            .unwrap_or(0))
    }

    /// Splits on char boundaries into pieces of at most `max_units`.
    fn split(&self, value: &str) -> Vec<String> {
        let mut parts = vec![String::new()];
        let mut units = 0;
        for c in value.chars() {
            if units + c.len_utf16() > self.max_units {
                parts.push(String::new());
                units = 0;
            }
            parts.last_mut().unwrap().push(c);
            units += c.len_utf16();
        }
        parts
    }
}

impl<S: SecretStore> SecretStore for ChunkedSecrets<S> {
    fn get(&self, profile: &str, key: &str) -> Result<Option<String>> {
        let Some(head) = self.inner.get(profile, key)? else {
            return Ok(None);
        };
        let Some(count) = head.strip_prefix(CHUNK_MARKER) else {
            return Ok(Some(head));
        };
        let count: usize = count
            .parse()
            .map_err(|_| crate::CoreError::Other(format!("keychain: `{key}` is damaged")))?;
        let mut value = String::new();
        for n in 1..=count {
            match self.inner.get(profile, &part_key(key, n))? {
                Some(part) => value.push_str(&part),
                None => {
                    return Err(crate::CoreError::Other(format!(
                        "keychain: part {n} of `{key}` is missing; enter it again"
                    )))
                }
            }
        }
        Ok(Some(value))
    }

    fn set(&self, profile: &str, key: &str, value: &str) -> Result<()> {
        let old_parts = self.stored_parts(profile, key)?;
        let new_parts = if value.encode_utf16().count() <= self.max_units {
            self.inner.set(profile, key, value)?;
            0
        } else {
            // Parts first, head last: until the head is rewritten, readers
            // still see the old value rather than a half-written one.
            let parts = self.split(value);
            for (i, part) in parts.iter().enumerate() {
                self.inner.set(profile, &part_key(key, i + 1), part)?;
            }
            self.inner.set(profile, key, &format!("{CHUNK_MARKER}{}", parts.len()))?;
            parts.len()
        };
        for n in new_parts + 1..=old_parts {
            self.inner.delete(profile, &part_key(key, n))?;
        }
        Ok(())
    }

    fn delete(&self, profile: &str, key: &str) -> Result<()> {
        for n in 1..=self.stored_parts(profile, key)? {
            self.inner.delete(profile, &part_key(key, n))?;
        }
        self.inner.delete(profile, key)
    }
}

/// In-memory secrets for tests.
#[derive(Default)]
pub struct MemorySecrets(Mutex<HashMap<(String, String), String>>);

impl SecretStore for MemorySecrets {
    fn get(&self, profile: &str, key: &str) -> Result<Option<String>> {
        Ok(self.0.lock().unwrap().get(&(profile.into(), key.into())).cloned())
    }
    fn set(&self, profile: &str, key: &str, value: &str) -> Result<()> {
        self.0.lock().unwrap().insert((profile.into(), key.into()), value.into());
        Ok(())
    }
    fn delete(&self, profile: &str, key: &str) -> Result<()> {
        self.0.lock().unwrap().remove(&(profile.into(), key.into()));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// MemorySecrets that refuses values over `limit` UTF-16 units, like
    /// Windows Credential Manager does.
    struct Limited(MemorySecrets, usize);
    impl SecretStore for Limited {
        fn get(&self, p: &str, k: &str) -> Result<Option<String>> {
            self.0.get(p, k)
        }
        fn set(&self, p: &str, k: &str, v: &str) -> Result<()> {
            if v.encode_utf16().count() > self.1 {
                return Err(crate::CoreError::Other("too long".into()));
            }
            self.0.set(p, k, v)
        }
        fn delete(&self, p: &str, k: &str) -> Result<()> {
            self.0.delete(p, k)
        }
    }

    fn store() -> ChunkedSecrets<Limited> {
        ChunkedSecrets::with_limit(Limited(MemorySecrets::default(), 40), 40)
    }

    fn entries(s: &ChunkedSecrets<Limited>) -> usize {
        s.inner.0 .0.lock().unwrap().len()
    }

    #[test]
    fn short_values_stay_in_one_entry() {
        let s = store();
        s.set("p", "token", "short").unwrap();
        assert_eq!(s.inner.get("p", "token").unwrap().as_deref(), Some("short"));
        assert_eq!(s.get("p", "token").unwrap().as_deref(), Some("short"));
    }

    #[test]
    fn long_values_round_trip_across_parts() {
        let s = store();
        // 1500 chars, like a Dropbox `sl.u.` token, plus a surrogate pair
        // straddling a part boundary.
        let long = format!("sl.u.{}😀{}", "a".repeat(34), "b".repeat(1460));
        s.set("p", "access_token", &long).unwrap();
        assert_eq!(s.get("p", "access_token").unwrap().as_deref(), Some(long.as_str()));
        assert_eq!(s.stored_parts("p", "access_token").unwrap(), entries(&s) - 1);
    }

    #[test]
    fn overwrite_and_delete_leave_no_stale_parts() {
        let s = store();
        s.set("p", "k", &"x".repeat(200)).unwrap();
        s.set("p", "k", &"y".repeat(90)).unwrap();
        assert_eq!(entries(&s), 1 + 3);
        assert_eq!(s.get("p", "k").unwrap(), Some("y".repeat(90)));

        s.set("p", "k", "tiny").unwrap();
        assert_eq!(entries(&s), 1);
        assert_eq!(s.get("p", "k").unwrap().as_deref(), Some("tiny"));

        s.set("p", "k", &"z".repeat(200)).unwrap();
        s.delete("p", "k").unwrap();
        assert_eq!(entries(&s), 0);
        assert_eq!(s.get("p", "k").unwrap(), None);
    }

    #[test]
    fn missing_part_is_an_error_not_a_truncated_secret() {
        let s = store();
        s.set("p", "k", &"x".repeat(100)).unwrap();
        s.inner.delete("p", "k#2").unwrap();
        assert!(s.get("p", "k").is_err());
    }
}
