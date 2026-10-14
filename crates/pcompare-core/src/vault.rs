//! Encrypted ("sealed") config files.
//!
//! Saved connections — remote profiles, database connections, notification
//! channels — name hosts, accounts, buckets and email addresses: enough to
//! aim an attack even without the passwords, which already live in the OS
//! keychain. Their lists are sealed with AES-256-GCM under a random key that
//! is itself kept in the keychain (Windows Credential Manager, macOS
//! Keychain, Secret Service), so a copied config folder, a backup or a
//! synced profile directory is unreadable anywhere but this user account on
//! this machine. Tampering is detected too: GCM authenticates every byte.
//!
//! What no user-space app can prevent: malware already running as the
//! signed-in user may ask the keychain for the key just as PCompare does.
//!
//! A store passes the path it always used (`profiles.json`); the sealed file
//! sits beside it as `profiles.vault`, and an existing plaintext file is
//! sealed and removed the first time it is read.

use crate::secrets::SecretStore;
use crate::store::{now_ms, read_json};
use crate::{CoreError, Result};
use aes_gcm::aead::{Aead, Generate, Key, KeyInit, Nonce, Payload};
use aes_gcm::Aes256Gcm;
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

const MAGIC: &[u8; 8] = b"PCVAULT1";
const NONCE_LEN: usize = 12;
/// Keychain slot of the file key. `~` never appears in a profile id, so it
/// can't collide with a connection's own secrets.
const KEY_OWNER: &str = "~vault";
const KEY_NAME: &str = "file-key";

/// One sealed JSON file.
pub struct SealedFile {
    path: PathBuf,
    legacy: PathBuf,
    /// Binds the ciphertext to this file's purpose, so one sealed file can't
    /// be swapped in for another.
    aad: Vec<u8>,
    key: OnceLock<[u8; 32]>,
}

impl SealedFile {
    /// `json_path` is the store's historical plaintext file.
    pub fn new(json_path: impl Into<PathBuf>) -> Self {
        let legacy: PathBuf = json_path.into();
        let stem = legacy.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let mut aad = MAGIC.to_vec();
        aad.extend_from_slice(stem.as_bytes());
        Self {
            path: legacy.with_extension("vault"),
            legacy,
            aad,
            key: OnceLock::new(),
        }
    }

    /// The sealed file on disk.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Reads and decrypts the file; `T::default()` when there is none yet.
    /// A file that can't be opened (tampered, or sealed under a key this
    /// keychain no longer has) is moved aside, never overwritten.
    pub fn read<T: DeserializeOwned + Serialize + Default>(&self, secrets: &dyn SecretStore) -> Result<T> {
        let bytes = match fs::read(&self.path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return self.migrate(secrets),
            Err(e) => return Err(CoreError::io(&self.path, e)),
        };
        let key = self.key(secrets)?;
        match self.open(&key, &bytes).and_then(|plain| serde_json::from_slice(&plain).ok()) {
            Some(v) => Ok(v),
            None => {
                let aside = self.path.with_extension(format!("unreadable-{}.vault", now_ms()));
                fs::rename(&self.path, &aside).map_err(|e| CoreError::io(&self.path, e))?;
                Ok(T::default())
            }
        }
    }

    /// Encrypts and writes atomically (temp file + rename).
    pub fn write<T: Serialize>(&self, secrets: &dyn SecretStore, value: &T) -> Result<()> {
        let plain = serde_json::to_vec(value).map_err(|e| CoreError::Other(e.to_string()))?;
        let sealed = self.seal(&self.key(secrets)?, &plain)?;
        if let Some(dir) = self.path.parent() {
            fs::create_dir_all(dir).map_err(|e| CoreError::io(dir, e))?;
        }
        let tmp = self.path.with_extension("vault.tmp");
        fs::write(&tmp, sealed).map_err(|e| CoreError::io(&tmp, e))?;
        fs::rename(&tmp, &self.path).map_err(|e| CoreError::io(&self.path, e))?;
        // Plaintext from an older version must not outlive the sealed copy.
        match fs::remove_file(&self.legacy) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(CoreError::io(&self.legacy, e)),
        }
    }

    /// First read after upgrading: seal the old plaintext file. If the
    /// keychain refuses, the data is still returned and stays plaintext
    /// until a save succeeds.
    fn migrate<T: DeserializeOwned + Serialize + Default>(&self, secrets: &dyn SecretStore) -> Result<T> {
        if !self.legacy.exists() {
            return Ok(T::default());
        }
        let value: T = read_json(&self.legacy)?;
        let _ = self.write(secrets, &value);
        Ok(value)
    }

    fn seal(&self, key: &[u8; 32], plain: &[u8]) -> Result<Vec<u8>> {
        let nonce = Nonce::<Aes256Gcm>::generate();
        let cipher = Aes256Gcm::new(&Key::<Aes256Gcm>::from(*key));
        let ct = cipher
            .encrypt(&nonce, Payload { msg: plain, aad: &self.aad })
            .map_err(|_| CoreError::Other("encryption failed".into()))?;
        let mut out = Vec::with_capacity(MAGIC.len() + NONCE_LEN + ct.len());
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&ct);
        Ok(out)
    }

    fn open(&self, key: &[u8; 32], bytes: &[u8]) -> Option<Vec<u8>> {
        let rest = bytes.strip_prefix(MAGIC.as_slice())?;
        if rest.len() < NONCE_LEN {
            return None;
        }
        let (nonce, ct) = rest.split_at(NONCE_LEN);
        let nonce = Nonce::<Aes256Gcm>::try_from(nonce).ok()?;
        let cipher = Aes256Gcm::new(&Key::<Aes256Gcm>::from(*key));
        cipher.decrypt(&nonce, Payload { msg: ct, aad: &self.aad }).ok()
    }

    /// The file key, created in the keychain on first use.
    fn key(&self, secrets: &dyn SecretStore) -> Result<[u8; 32]> {
        if let Some(k) = self.key.get() {
            return Ok(*k);
        }
        // Every store shares one key; serialise its creation so two stores
        // starting together can't each mint (and half-lose) a different one.
        static CREATE: Mutex<()> = Mutex::new(());
        let _g = CREATE.lock().unwrap_or_else(|p| p.into_inner());
        let hex = match secrets.get(KEY_OWNER, KEY_NAME)? {
            Some(h) => h,
            None => {
                let fresh: [u8; 32] = Key::<Aes256Gcm>::generate().into();
                secrets.set(KEY_OWNER, KEY_NAME, &to_hex(&fresh))?;
                // Read back: if another process won a race, use its key.
                secrets
                    .get(KEY_OWNER, KEY_NAME)?
                    .ok_or_else(|| CoreError::Other("the keychain did not keep PCompare's file key".into()))?
            }
        };
        let key = from_hex(&hex).ok_or_else(|| CoreError::Other("PCompare's file key in the keychain is damaged".into()))?;
        Ok(*self.key.get_or_init(|| key))
    }
}

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn from_hex(s: &str) -> Option<[u8; 32]> {
    let s = s.trim();
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(s.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secrets::MemorySecrets;
    use std::collections::BTreeMap;

    type Data = BTreeMap<String, String>;

    fn sample() -> Data {
        [("host".to_owned(), "db.internal.example".to_owned())].into()
    }

    #[test]
    fn round_trips_and_hides_the_plaintext() {
        let dir = tempfile::tempdir().unwrap();
        let secrets = MemorySecrets::default();
        let f = SealedFile::new(dir.path().join("profiles.json"));
        f.write(&secrets, &sample()).unwrap();
        let raw = fs::read(f.path()).unwrap();
        assert!(raw.starts_with(MAGIC));
        assert!(!String::from_utf8_lossy(&raw).contains("example"));
        let back: Data = SealedFile::new(dir.path().join("profiles.json")).read(&secrets).unwrap();
        assert_eq!(back, sample());
    }

    #[test]
    fn another_keychain_cannot_open_it_and_the_file_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let f = SealedFile::new(dir.path().join("profiles.json"));
        f.write(&MemorySecrets::default(), &sample()).unwrap();
        // Same file, a different machine's keychain.
        let thief: Data = SealedFile::new(dir.path().join("profiles.json")).read(&MemorySecrets::default()).unwrap();
        assert!(thief.is_empty());
        let aside: Vec<_> = fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert!(aside.iter().any(|n| n.to_string_lossy().contains("unreadable")), "{aside:?}");
    }

    #[test]
    fn tampering_and_swapping_files_are_detected() {
        let dir = tempfile::tempdir().unwrap();
        let secrets = MemorySecrets::default();
        let a = SealedFile::new(dir.path().join("profiles.json"));
        let b = SealedFile::new(dir.path().join("db-connections.json"));
        a.write(&secrets, &sample()).unwrap();
        let mut raw = fs::read(a.path()).unwrap();

        // Swapped in under another store's name: the associated data differs.
        fs::write(b.path(), &raw).unwrap();
        assert!(b.read::<Data>(&secrets).unwrap().is_empty());

        // One flipped bit.
        let last = raw.len() - 1;
        raw[last] ^= 1;
        fs::write(a.path(), &raw).unwrap();
        assert!(a.read::<Data>(&secrets).unwrap().is_empty());
    }

    #[test]
    fn plaintext_from_older_versions_is_sealed_and_removed() {
        let dir = tempfile::tempdir().unwrap();
        let secrets = MemorySecrets::default();
        let legacy = dir.path().join("profiles.json");
        fs::write(&legacy, serde_json::to_string(&sample()).unwrap()).unwrap();
        let f = SealedFile::new(&legacy);
        assert_eq!(f.read::<Data>(&secrets).unwrap(), sample());
        assert!(!legacy.exists());
        assert!(f.path().exists());
        assert_eq!(f.read::<Data>(&secrets).unwrap(), sample());
    }

    #[test]
    fn no_file_needs_no_keychain() {
        struct Refuses;
        impl SecretStore for Refuses {
            fn get(&self, _: &str, _: &str) -> Result<Option<String>> {
                Err(CoreError::Other("locked".into()))
            }
            fn set(&self, _: &str, _: &str, _: &str) -> Result<()> {
                Err(CoreError::Other("locked".into()))
            }
            fn delete(&self, _: &str, _: &str) -> Result<()> {
                Ok(())
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let f = SealedFile::new(dir.path().join("profiles.json"));
        assert!(f.read::<Data>(&Refuses).unwrap().is_empty());
        assert!(f.write(&Refuses, &sample()).is_err(), "a locked keychain refuses the save");
    }

    #[test]
    fn hex_round_trip() {
        let k = [0xabu8; 32];
        assert_eq!(from_hex(&to_hex(&k)), Some(k));
        assert_eq!(from_hex("zz"), None);
    }
}
