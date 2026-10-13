//! Persisted channel list (`notify-channels.json` in the app config dir)
//! plus their secrets in a [`SecretStore`].

use std::path::PathBuf;
use std::sync::Mutex;

use pcompare_core::secrets::SecretStore;
use pcompare_core::store::{new_id, now_ms, read_json, write_json};

use crate::channel::{ChannelConfig, ChannelSecrets, ChannelSettings};
use crate::{email, http, NotifyError, Result};

pub(crate) const KEY_URL: &str = "url";
pub(crate) const KEY_PASSWORD: &str = "password";
pub(crate) const KEY_SIGNING: &str = "signingKey";

/// The [`SecretStore`] profile holding a channel's secrets.
pub(crate) fn secret_profile(id: &str) -> String {
    format!("notify:{id}")
}

#[derive(serde::Serialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SecretsPresent {
    pub url: bool,
    pub password: bool,
    pub signing_key: bool,
}

pub struct ChannelStore {
    path: PathBuf,
    secrets: Box<dyn SecretStore>,
    /// Serialises read-modify-write of the file (the UI and the scheduler
    /// share one store).
    lock: Mutex<()>,
}

/// What saving does to one stored secret.
enum SecretChange {
    /// Leave it; `bool` = whether one is stored.
    Keep(bool),
    Set(String),
    Remove,
}

impl SecretChange {
    fn present(&self) -> bool {
        match self {
            SecretChange::Keep(p) => *p,
            SecretChange::Set(_) => true,
            SecretChange::Remove => false,
        }
    }
}

impl ChannelStore {
    pub fn new(path: PathBuf, secrets: Box<dyn SecretStore>) -> Self {
        ChannelStore {
            path,
            secrets,
            lock: Mutex::new(()),
        }
    }

    pub fn secrets(&self) -> &dyn SecretStore {
        &*self.secrets
    }

    fn guard(&self) -> std::sync::MutexGuard<'_, ()> {
        // A panic elsewhere while holding the lock leaves nothing half-done
        // in memory (the file is replaced atomically), so carry on.
        self.lock.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn read(&self) -> Result<Vec<ChannelConfig>> {
        Ok(read_json(&self.path)?)
    }

    /// Sorted by name.
    pub fn list(&self) -> Result<Vec<ChannelConfig>> {
        let _g = self.guard();
        let mut list = self.read()?;
        list.sort_by_cached_key(|c| (c.name.to_lowercase(), c.id.clone()));
        Ok(list)
    }

    pub fn get(&self, id: &str) -> Result<Option<ChannelConfig>> {
        let _g = self.guard();
        Ok(self.read()?.into_iter().find(|c| c.id == id))
    }

    /// Creates (empty `id`) or updates a channel, validating the settings
    /// (e.g. email needs host, from and at least one recipient; webhook
    /// kinds need a URL either already stored or in `secrets`). Returns the
    /// saved config with its id and timestamps.
    ///
    /// Secrets the new kind doesn't use (say, a webhook URL after switching
    /// the channel to email) are removed from the keychain.
    pub fn save(
        &self,
        id: &str,
        name: &str,
        enabled: bool,
        settings: ChannelSettings,
        secrets: &ChannelSecrets,
    ) -> Result<ChannelConfig> {
        let _g = self.guard();
        let mut list = self.read()?;
        let name = name.trim();
        if name.is_empty() {
            return Err(NotifyError::Config("Give the channel a name.".into()));
        }
        let index = if id.is_empty() {
            None
        } else {
            Some(list.iter().position(|c| c.id == id).ok_or_else(|| {
                NotifyError::Config(
                    "This channel no longer exists; it may have been deleted in another window."
                        .into(),
                )
            })?)
        };
        let lower = name.to_lowercase();
        if list
            .iter()
            .any(|c| c.id != id && c.name.trim().to_lowercase() == lower)
        {
            return Err(NotifyError::Config(format!(
                "There is already a channel called \u{201c}{name}\u{201d}."
            )));
        }
        let settings = normalize(settings)?;

        let id = if id.is_empty() {
            new_id()
        } else {
            id.to_string()
        };
        let profile = secret_profile(&id);
        let uses_url = !matches!(settings, ChannelSettings::Email { .. });
        let uses_password = matches!(settings, ChannelSettings::Email { .. });
        let uses_signing = matches!(settings, ChannelSettings::Webhook { .. });
        let url = self.plan(
            &profile,
            KEY_URL,
            secrets.url.as_deref().map(str::trim),
            uses_url,
        )?;
        let password = self.plan(
            &profile,
            KEY_PASSWORD,
            secrets.password.as_deref(),
            uses_password,
        )?;
        let signing = self.plan(
            &profile,
            KEY_SIGNING,
            secrets.signing_key.as_deref().map(str::trim),
            uses_signing,
        )?;
        if let SecretChange::Set(u) = &url {
            http::validate_url(u)?;
        }
        if uses_url && !url.present() {
            return Err(NotifyError::MissingSecret("webhook URL"));
        }
        if let ChannelSettings::Email { username, .. } = &settings {
            if !username.is_empty() && !password.present() {
                return Err(NotifyError::MissingSecret("password"));
            }
        }

        let now = now_ms();
        let config = ChannelConfig {
            id: id.clone(),
            name: name.to_string(),
            enabled,
            settings,
            created_ms: index.map_or(now, |i| list[i].created_ms),
            updated_ms: now,
        };
        for (key, change) in [
            (KEY_URL, &url),
            (KEY_PASSWORD, &password),
            (KEY_SIGNING, &signing),
        ] {
            match change {
                SecretChange::Keep(_) => {}
                SecretChange::Set(v) => self.secrets.set(&profile, key, v)?,
                SecretChange::Remove => self.secrets.delete(&profile, key)?,
            }
        }
        match index {
            Some(i) => list[i] = config.clone(),
            None => list.push(config.clone()),
        }
        if let Err(e) = write_json(&self.path, &list) {
            if index.is_none() {
                // Don't leave keychain entries behind for a channel that
                // was never saved.
                for key in [KEY_URL, KEY_PASSWORD, KEY_SIGNING] {
                    let _ = self.secrets.delete(&profile, key);
                }
            }
            return Err(e.into());
        }
        Ok(config)
    }

    /// Applies the save semantics (`None` keep, `Some("")` remove) to one
    /// secret; secrets the channel kind doesn't use are removed.
    fn plan(
        &self,
        profile: &str,
        key: &str,
        incoming: Option<&str>,
        used: bool,
    ) -> Result<SecretChange> {
        let stored = || -> Result<bool> {
            Ok(self
                .secrets
                .get(profile, key)?
                .is_some_and(|v| !v.is_empty()))
        };
        Ok(match incoming {
            _ if !used => {
                if stored()? {
                    SecretChange::Remove
                } else {
                    SecretChange::Keep(false)
                }
            }
            None => SecretChange::Keep(stored()?),
            Some("") => SecretChange::Remove,
            Some(v) => SecretChange::Set(v.to_string()),
        })
    }

    /// Removes the channel and its secrets.
    pub fn delete(&self, id: &str) -> Result<()> {
        let _g = self.guard();
        let mut list = self.read()?;
        let before = list.len();
        list.retain(|c| c.id != id);
        if list.len() != before {
            write_json(&self.path, &list)?;
        }
        let profile = secret_profile(id);
        for key in [KEY_URL, KEY_PASSWORD, KEY_SIGNING] {
            self.secrets.delete(&profile, key)?;
        }
        Ok(())
    }

    /// Which secrets are stored, so the UI can show "saved" without reading them.
    pub fn secrets_present(&self, id: &str) -> Result<SecretsPresent> {
        let profile = secret_profile(id);
        let has = |key| -> Result<bool> {
            Ok(self
                .secrets
                .get(&profile, key)?
                .is_some_and(|v| !v.is_empty()))
        };
        Ok(SecretsPresent {
            url: has(KEY_URL)?,
            password: has(KEY_PASSWORD)?,
            signing_key: has(KEY_SIGNING)?,
        })
    }
}

/// Validates settings and tidies them (trimmed fields, blank recipients and
/// header rows dropped) so what is stored is what will be used.
fn normalize(settings: ChannelSettings) -> Result<ChannelSettings> {
    Ok(match settings {
        ChannelSettings::Teams {} => ChannelSettings::Teams {},
        ChannelSettings::Slack {} => ChannelSettings::Slack {},
        ChannelSettings::Webhook { headers } => {
            let headers: Vec<(String, String)> = headers
                .into_iter()
                .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
                .filter(|(k, v)| !(k.is_empty() && v.is_empty()))
                .collect();
            http::validate_headers(&headers)?;
            ChannelSettings::Webhook { headers }
        }
        ChannelSettings::Email {
            host,
            port,
            security,
            username,
            from,
            to,
            cc,
        } => {
            email::validate_host(&host)?;
            let tidy = |list: Vec<String>| -> Vec<String> {
                list.iter()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            };
            let (to, cc) = (tidy(to), tidy(cc));
            email::validate_addresses(&from, &to, &cc)?;
            ChannelSettings::Email {
                host: host.trim().to_string(),
                port,
                security,
                username: username.trim().to_string(),
                from: from.trim().to_string(),
                to,
                cc,
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channel::EmailSecurity;
    use pcompare_core::secrets::MemorySecrets;
    use std::sync::Arc;

    /// Shares one MemorySecrets between the store and the test.
    struct Shared(Arc<MemorySecrets>);
    impl SecretStore for Shared {
        fn get(&self, p: &str, k: &str) -> pcompare_core::Result<Option<String>> {
            self.0.get(p, k)
        }
        fn set(&self, p: &str, k: &str, v: &str) -> pcompare_core::Result<()> {
            self.0.set(p, k, v)
        }
        fn delete(&self, p: &str, k: &str) -> pcompare_core::Result<()> {
            self.0.delete(p, k)
        }
    }

    fn store() -> (tempfile::TempDir, ChannelStore, Arc<MemorySecrets>) {
        let dir = tempfile::tempdir().unwrap();
        let secrets = Arc::new(MemorySecrets::default());
        let s = ChannelStore::new(
            dir.path().join("notify-channels.json"),
            Box::new(Shared(Arc::clone(&secrets))),
        );
        (dir, s, secrets)
    }

    fn url(u: &str) -> ChannelSecrets {
        ChannelSecrets {
            url: Some(u.into()),
            ..Default::default()
        }
    }

    fn email_settings() -> ChannelSettings {
        ChannelSettings::Email {
            host: " smtp.example.com ".into(),
            port: 0,
            security: EmailSecurity::StartTls,
            username: String::new(),
            from: "PCompare <pc@example.com>".into(),
            to: vec!["ops@example.com".into(), "  ".into()],
            cc: vec![],
        }
    }

    #[test]
    fn create_list_get_update_delete() {
        let (_d, s, secrets) = store();
        let t = s
            .save(
                "",
                "Ops (Teams)",
                true,
                ChannelSettings::Teams {},
                &url("https://example.com/hook"),
            )
            .unwrap();
        assert!(!t.id.is_empty());
        assert_eq!(t.created_ms, t.updated_ms);
        let e = s
            .save(
                "",
                "alerts",
                false,
                email_settings(),
                &ChannelSecrets::default(),
            )
            .unwrap();
        // Normalized on save.
        let ChannelSettings::Email { host, to, .. } = &e.settings else {
            panic!()
        };
        assert_eq!(host, "smtp.example.com");
        assert_eq!(to, &vec!["ops@example.com".to_string()]);

        let names: Vec<String> = s.list().unwrap().into_iter().map(|c| c.name).collect();
        assert_eq!(
            names,
            ["alerts", "Ops (Teams)"],
            "sorted case-insensitively"
        );
        assert_eq!(s.get(&t.id).unwrap().unwrap(), t);
        assert!(s.get("nope").unwrap().is_none());

        // Update keeps created_ms and the stored URL (None = keep).
        std::thread::sleep(std::time::Duration::from_millis(2));
        let t2 = s
            .save(
                &t.id,
                "Ops",
                false,
                ChannelSettings::Teams {},
                &ChannelSecrets::default(),
            )
            .unwrap();
        assert_eq!(t2.created_ms, t.created_ms);
        assert!(t2.updated_ms > t.updated_ms);
        assert!(!t2.enabled);
        assert_eq!(
            secrets
                .get(&secret_profile(&t.id), "url")
                .unwrap()
                .as_deref(),
            Some("https://example.com/hook")
        );
        assert!(s.secrets_present(&t.id).unwrap().url);

        // The file holds no secrets.
        let file = std::fs::read_to_string(s.path.clone()).unwrap();
        assert!(!file.contains("example.com/hook"));

        s.delete(&t.id).unwrap();
        assert!(s.get(&t.id).unwrap().is_none());
        assert_eq!(s.secrets_present(&t.id).unwrap(), SecretsPresent::default());
        assert_eq!(s.list().unwrap().len(), 1);
        s.delete(&t.id).unwrap(); // idempotent
    }

    #[test]
    fn secret_save_semantics() {
        let (_d, s, _secrets) = store();
        let w = s
            .save(
                "",
                "Hook",
                true,
                ChannelSettings::Webhook { headers: vec![] },
                &ChannelSecrets {
                    url: Some("https://example.com/h".into()),
                    signing_key: Some("k".into()),
                    password: None,
                },
            )
            .unwrap();
        assert_eq!(
            s.secrets_present(&w.id).unwrap(),
            SecretsPresent {
                url: true,
                password: false,
                signing_key: true
            }
        );
        // Some("") removes the signing key; None keeps the URL.
        let keep_url_drop_key = ChannelSecrets {
            signing_key: Some(String::new()),
            ..Default::default()
        };
        s.save(
            &w.id,
            "Hook",
            true,
            ChannelSettings::Webhook { headers: vec![] },
            &keep_url_drop_key,
        )
        .unwrap();
        assert_eq!(
            s.secrets_present(&w.id).unwrap(),
            SecretsPresent {
                url: true,
                password: false,
                signing_key: false
            }
        );
        // Removing the URL of a webhook channel is refused.
        let err = s
            .save(
                &w.id,
                "Hook",
                true,
                ChannelSettings::Webhook { headers: vec![] },
                &url(""),
            )
            .unwrap_err();
        assert!(matches!(err, NotifyError::MissingSecret("webhook URL")));
        // Switching to email drops the now-unused URL.
        s.save(
            &w.id,
            "Hook",
            true,
            email_settings(),
            &ChannelSecrets::default(),
        )
        .unwrap();
        assert_eq!(s.secrets_present(&w.id).unwrap(), SecretsPresent::default());
    }

    #[test]
    fn validation_errors() {
        let (_d, s, _) = store();
        let none = ChannelSecrets::default();
        assert!(matches!(
            s.save(
                "",
                "  ",
                true,
                ChannelSettings::Slack {},
                &url("https://x.test/h")
            ),
            Err(NotifyError::Config(_))
        ));
        assert!(matches!(
            s.save("", "Slack", true, ChannelSettings::Slack {}, &none),
            Err(NotifyError::MissingSecret("webhook URL"))
        ));
        assert!(matches!(
            s.save(
                "",
                "Slack",
                true,
                ChannelSettings::Slack {},
                &url("http://example.com/h")
            ),
            Err(NotifyError::Config(_))
        ));
        s.save(
            "",
            "Slack",
            true,
            ChannelSettings::Slack {},
            &url("https://hooks.slack.com/services/x"),
        )
        .unwrap();
        let dup = s
            .save(
                "",
                " SLACK ",
                true,
                ChannelSettings::Slack {},
                &url("https://hooks.slack.com/services/y"),
            )
            .unwrap_err();
        assert_eq!(
            dup.to_string(),
            "There is already a channel called \u{201c}SLACK\u{201d}."
        );
        assert!(matches!(
            s.save(
                "missing-id",
                "Other",
                true,
                ChannelSettings::Teams {},
                &none
            ),
            Err(NotifyError::Config(_))
        ));

        let mut bad = email_settings();
        if let ChannelSettings::Email { to, .. } = &mut bad {
            to.clear();
        }
        assert!(s
            .save("", "Mail", true, bad, &none)
            .unwrap_err()
            .to_string()
            .contains("at least one recipient"));
        let mut bad = email_settings();
        if let ChannelSettings::Email { host, .. } = &mut bad {
            host.clear();
        }
        assert!(matches!(
            s.save("", "Mail", true, bad, &none),
            Err(NotifyError::Config(_))
        ));
        let mut auth = email_settings();
        if let ChannelSettings::Email { username, .. } = &mut auth {
            *username = "me".into();
        }
        assert!(matches!(
            s.save("", "Mail", true, auth.clone(), &none),
            Err(NotifyError::MissingSecret("password"))
        ));
        let pw = ChannelSecrets {
            password: Some(" spaced pw ".into()),
            ..Default::default()
        };
        let saved = s.save("", "Mail", true, auth, &pw).unwrap();
        assert_eq!(
            s.secrets()
                .get(&secret_profile(&saved.id), "password")
                .unwrap()
                .as_deref(),
            Some(" spaced pw "),
            "passwords are kept verbatim"
        );

        let reserved = ChannelSettings::Webhook {
            headers: vec![("Content-Type".into(), "text/plain".into())],
        };
        assert!(matches!(
            s.save("", "Hook", true, reserved, &url("https://x.test/h")),
            Err(NotifyError::Config(_))
        ));
        // Nothing invalid was written.
        assert_eq!(s.list().unwrap().len(), 2);
    }

    #[test]
    fn file_round_trips_as_camel_case_json() {
        let (_d, s, _) = store();
        s.save(
            "",
            "Hook",
            true,
            ChannelSettings::Webhook {
                headers: vec![("X-A".into(), "1".into()), (" ".into(), "".into())],
            },
            &url("https://x.test/h"),
        )
        .unwrap();
        let raw: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&s.path).unwrap()).unwrap();
        let c = &raw[0];
        assert_eq!(c["settings"]["kind"], "webhook");
        assert_eq!(c["settings"]["headers"], serde_json::json!([["X-A", "1"]]));
        assert!(c["createdMs"].is_i64() && c["updatedMs"].is_i64());
        let reopened = ChannelStore::new(s.path.clone(), Box::new(MemorySecrets::default()));
        assert_eq!(reopened.list().unwrap(), s.list().unwrap());
    }

    #[test]
    fn concurrent_saves_are_not_lost() {
        let (_d, s, _) = store();
        let s = Arc::new(s);
        let handles: Vec<_> = (0..8)
            .map(|i| {
                let s = Arc::clone(&s);
                std::thread::spawn(move || {
                    s.save(
                        "",
                        &format!("C{i}"),
                        true,
                        ChannelSettings::Teams {},
                        &url("https://x.test/h"),
                    )
                    .unwrap();
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(s.list().unwrap().len(), 8);
    }
}
