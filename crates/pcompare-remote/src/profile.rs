//! Connection profiles: named, saved remote connections.
//!
//! Non-secret settings live in `profiles.json`; every [`FieldKind::Secret`]
//! value (passwords, keys, OAuth refresh tokens) goes to the OS keychain —
//! Windows Credential Manager, macOS Keychain, or Secret Service on Linux.
//! Secret values flow UI → Rust only; the UI just learns *which* secrets are
//! stored so it can show "saved" placeholders.

use crate::provider::{spec, FieldKind, ProviderKind, REFRESH_TOKEN};
use pcompare_core::store::{now_ms, read_json, write_json};
use pcompare_core::{CoreError, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::Mutex;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Profile {
    /// Stable slug used in `remote://<id>/path`; survives renames.
    pub id: String,
    pub name: String,
    pub kind: ProviderKind,
    #[serde(default)]
    pub settings: BTreeMap<String, String>,
    /// Keys of secrets currently held in the keychain (values never leave Rust).
    #[serde(default)]
    pub stored_secrets: BTreeSet<String>,
    pub created_ms: i64,
    pub modified_ms: i64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileInput {
    pub id: Option<String>,
    pub name: String,
    pub kind: ProviderKind,
    #[serde(default)]
    pub settings: BTreeMap<String, String>,
    /// Only secrets the user changed: a value replaces, `""` deletes, absent keeps.
    #[serde(default)]
    pub secrets: BTreeMap<String, String>,
}

pub trait SecretStore: Send + Sync {
    fn get(&self, profile: &str, key: &str) -> Result<Option<String>>;
    fn set(&self, profile: &str, key: &str, value: &str) -> Result<()>;
    fn delete(&self, profile: &str, key: &str) -> Result<()>;
}

/// The platform keychain.
pub struct KeyringSecrets;

const KEYRING_SERVICE: &str = "PCompare";

fn keyring_entry(profile: &str, key: &str) -> Result<keyring::Entry> {
    keyring::Entry::new(KEYRING_SERVICE, &format!("profile:{profile}:{key}"))
        .map_err(|e| CoreError::Other(format!("keychain: {e}")))
}

impl SecretStore for KeyringSecrets {
    fn get(&self, profile: &str, key: &str) -> Result<Option<String>> {
        match keyring_entry(profile, key)?.get_password() {
            Ok(v) => Ok(Some(v)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(CoreError::Other(format!("keychain: {e}"))),
        }
    }
    fn set(&self, profile: &str, key: &str, value: &str) -> Result<()> {
        keyring_entry(profile, key)?
            .set_password(value)
            .map_err(|e| CoreError::Other(format!("keychain: {e}")))
    }
    fn delete(&self, profile: &str, key: &str) -> Result<()> {
        match keyring_entry(profile, key)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(CoreError::Other(format!("keychain: {e}"))),
        }
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

#[derive(Default, Serialize, Deserialize)]
struct ProfileFile {
    #[serde(default)]
    profiles: Vec<Profile>,
}

/// A profile with all of its secrets loaded, ready to connect.
#[derive(Clone)]
pub struct ResolvedProfile {
    pub profile: Profile,
    pub secrets: HashMap<String, String>,
}

impl ResolvedProfile {
    /// Setting or secret value, empty strings treated as unset.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.secrets
            .get(key)
            .or_else(|| self.profile.settings.get(key))
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
    }
}

pub struct ProfileStore {
    path: PathBuf,
    secrets: Box<dyn SecretStore>,
    lock: Mutex<()>,
}

/// `"My S3 (prod)"` → `"my-s3-prod"`: safe inside a `remote://` URI.
pub fn slugify(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    let out = out.trim_end_matches('-').to_owned();
    if out.is_empty() {
        "remote".into()
    } else {
        out
    }
}

impl ProfileStore {
    pub fn new(path: impl Into<PathBuf>, secrets: Box<dyn SecretStore>) -> Self {
        Self {
            path: path.into(),
            secrets,
            lock: Mutex::new(()),
        }
    }

    fn guard(&self) -> std::sync::MutexGuard<'_, ()> {
        self.lock.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn list(&self) -> Result<Vec<Profile>> {
        let _g = self.guard();
        let mut file: ProfileFile = read_json(&self.path)?;
        file.profiles.sort_by_key(|p| p.name.to_lowercase());
        Ok(file.profiles)
    }

    pub fn get(&self, id: &str) -> Result<Profile> {
        self.list()?
            .into_iter()
            .find(|p| p.id == id)
            .ok_or_else(|| CoreError::Other(format!("no connection profile `{id}`")))
    }

    pub fn resolve(&self, id: &str) -> Result<ResolvedProfile> {
        let profile = self.get(id)?;
        let mut secrets = HashMap::new();
        for key in &profile.stored_secrets {
            if let Some(v) = self.secrets.get(&profile.id, key)? {
                secrets.insert(key.clone(), v);
            }
        }
        Ok(ResolvedProfile { profile, secrets })
    }

    /// Builds the profile a save *would* produce, merging unsaved edits with
    /// stored secrets — used by "Test connection" before the user saves.
    pub fn preview(&self, input: &ProfileInput) -> Result<ResolvedProfile> {
        let mut resolved = match &input.id {
            Some(id) => self.resolve(id)?,
            None => ResolvedProfile {
                profile: Profile {
                    id: slugify(&input.name),
                    name: input.name.clone(),
                    kind: input.kind,
                    settings: BTreeMap::new(),
                    stored_secrets: BTreeSet::new(),
                    created_ms: 0,
                    modified_ms: 0,
                },
                secrets: HashMap::new(),
            },
        };
        resolved.profile.kind = input.kind;
        resolved.profile.settings = input.settings.clone();
        for (k, v) in &input.secrets {
            if v.is_empty() {
                resolved.secrets.remove(k);
            } else {
                resolved.secrets.insert(k.clone(), v.clone());
            }
        }
        Ok(resolved)
    }

    pub fn save(&self, input: ProfileInput) -> Result<Profile> {
        let name = input.name.trim().to_owned();
        if name.is_empty() {
            return Err(CoreError::Other("profile name cannot be empty".into()));
        }
        let spec = spec(input.kind);
        let secret_keys: BTreeSet<&str> = spec
            .fields
            .iter()
            .filter(|f| f.kind == FieldKind::Secret)
            .map(|f| f.key)
            .chain(spec.oauth.as_ref().map(|_| REFRESH_TOKEN))
            .collect();

        let _g = self.guard();
        let mut file: ProfileFile = read_json(&self.path)?;
        let now = now_ms();
        let idx = match &input.id {
            Some(id) => file
                .profiles
                .iter()
                .position(|p| &p.id == id)
                .ok_or_else(|| CoreError::Other(format!("no connection profile `{id}`")))?,
            None => {
                let base = slugify(&name);
                let mut id = base.clone();
                let mut n = 2;
                while file.profiles.iter().any(|p| p.id == id) {
                    id = format!("{base}-{n}");
                    n += 1;
                }
                file.profiles.push(Profile {
                    id,
                    name: name.clone(),
                    kind: input.kind,
                    settings: BTreeMap::new(),
                    stored_secrets: BTreeSet::new(),
                    created_ms: now,
                    modified_ms: now,
                });
                file.profiles.len() - 1
            }
        };
        let p = &mut file.profiles[idx];

        // Switching vendor invalidates every secret of the old one.
        if p.kind != input.kind {
            for key in std::mem::take(&mut p.stored_secrets) {
                self.secrets.delete(&p.id, &key)?;
            }
        }
        for (key, value) in &input.secrets {
            if !secret_keys.contains(key.as_str()) {
                return Err(CoreError::Other(format!("`{key}` is not a secret field")));
            }
            if value.is_empty() {
                self.secrets.delete(&p.id, key)?;
                p.stored_secrets.remove(key);
            } else {
                self.secrets.set(&p.id, key, value)?;
                p.stored_secrets.insert(key.clone());
            }
        }
        p.name = name;
        p.kind = input.kind;
        // Secrets must never leak into the JSON file, even if the UI sends them.
        p.settings = input
            .settings
            .into_iter()
            .filter(|(k, _)| !secret_keys.contains(k.as_str()))
            .collect();
        p.modified_ms = now;
        let saved = p.clone();
        write_json(&self.path, &file)?;
        Ok(saved)
    }

    /// Updates one non-secret setting (e.g. a newly trusted SSH host key).
    pub fn set_setting(&self, id: &str, key: &str, value: &str) -> Result<()> {
        let _g = self.guard();
        let mut file: ProfileFile = read_json(&self.path)?;
        if let Some(p) = file.profiles.iter_mut().find(|p| p.id == id) {
            p.settings.insert(key.into(), value.into());
            write_json(&self.path, &file)?;
        }
        Ok(())
    }

    /// Stores one secret outside a form save (e.g. a rotated refresh token).
    pub fn set_secret(&self, id: &str, key: &str, value: &str) -> Result<()> {
        let _g = self.guard();
        let mut file: ProfileFile = read_json(&self.path)?;
        let p = file
            .profiles
            .iter_mut()
            .find(|p| p.id == id)
            .ok_or_else(|| CoreError::Other(format!("no connection profile `{id}`")))?;
        self.secrets.set(id, key, value)?;
        if p.stored_secrets.insert(key.into()) {
            write_json(&self.path, &file)?;
        }
        Ok(())
    }

    pub fn delete(&self, id: &str) -> Result<()> {
        let _g = self.guard();
        let mut file: ProfileFile = read_json(&self.path)?;
        if let Some(p) = file.profiles.iter().find(|p| p.id == id) {
            for key in &p.stored_secrets {
                self.secrets.delete(id, key)?;
            }
        }
        file.profiles.retain(|p| p.id != id);
        write_json(&self.path, &file)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(dir: &tempfile::TempDir) -> ProfileStore {
        ProfileStore::new(dir.path().join("profiles.json"), Box::<MemorySecrets>::default())
    }

    fn sftp_input(name: &str) -> ProfileInput {
        ProfileInput {
            id: None,
            name: name.into(),
            kind: ProviderKind::Sftp,
            settings: [("host".into(), "example.com".into()), ("user".into(), "me".into())].into(),
            secrets: [("password".into(), "hunter2".into())].into(),
        }
    }

    #[test]
    fn secrets_go_to_keychain_not_json() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(&dir);
        let mut input = sftp_input("Build Server");
        // A misbehaving UI that puts a secret in settings must not leak it.
        input.settings.insert("password".into(), "leaked".into());
        let p = s.save(input).unwrap();
        assert_eq!(p.id, "build-server");
        assert!(p.stored_secrets.contains("password"));

        let json = std::fs::read_to_string(dir.path().join("profiles.json")).unwrap();
        assert!(!json.contains("hunter2") && !json.contains("leaked"), "{json}");
        assert_eq!(s.resolve(&p.id).unwrap().get("password"), Some("hunter2"));
    }

    #[test]
    fn absent_secret_keeps_empty_deletes() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(&dir);
        let p = s.save(sftp_input("box")).unwrap();

        let mut keep = sftp_input("box renamed");
        keep.id = Some(p.id.clone());
        keep.secrets.clear();
        let p2 = s.save(keep).unwrap();
        assert_eq!(p2.id, "box", "id is stable across renames");
        assert_eq!(s.resolve("box").unwrap().get("password"), Some("hunter2"));

        let mut clear = sftp_input("box");
        clear.id = Some(p.id);
        clear.secrets = [("password".into(), String::new())].into();
        assert!(s.save(clear).unwrap().stored_secrets.is_empty());
        assert_eq!(s.resolve("box").unwrap().get("password"), None);
    }

    #[test]
    fn duplicate_names_get_unique_ids_and_delete_cleans_secrets() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(&dir);
        let a = s.save(sftp_input("prod")).unwrap();
        let b = s.save(sftp_input("prod")).unwrap();
        assert_eq!((a.id.as_str(), b.id.as_str()), ("prod", "prod-2"));
        s.delete("prod").unwrap();
        assert!(s.get("prod").is_err());
        assert!(s.secrets.get("prod", "password").unwrap().is_none());
    }

    #[test]
    fn slugify_is_uri_safe() {
        assert_eq!(slugify("My S3 (prod)"), "my-s3-prod");
        assert_eq!(slugify("  ///  "), "remote");
    }
}
