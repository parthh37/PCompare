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
//!
//! The passwords and tokens themselves are sealed the same way, in
//! `secrets.vault` ([`VaultSecrets`]), so the file key is PCompare's only
//! keychain entry.

use crate::secrets::SecretStore;
use crate::store::{now_ms, read_json};
use crate::{CoreError, Result};
use aes_gcm::aead::{Aead, Generate, Key, KeyInit, Nonce, Payload};
use aes_gcm::Aes256Gcm;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};

const MAGIC: &[u8; 8] = b"PCVAULT1";
const NONCE_LEN: usize = 12;
/// Keychain slot of the file key. `~` never appears in a profile id, so it
/// can't collide with a connection's own secrets. Every `~vault` entry
/// stays in the keychain itself (see [`VaultSecrets`]).
pub const KEY_OWNER: &str = "~vault";
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

/// Connection secrets sealed in one file under the file key, instead of one
/// keychain entry each.
///
/// Why: on macOS every keychain entry has its own access list, and a build
/// without a stable signing identity is trusted by its exact binary hash.
/// So after each update, every entry asked again, one dialog per entry, and
/// "Always Allow" covered only the entry it was clicked on. With a single
/// entry (the file key) there is at most one dialog per build.
///
/// Secrets that older versions kept as separate keychain entries move into
/// the file the first time they're read.
pub struct VaultSecrets<K> {
    /// Holds the file key, plus entries older versions wrote.
    keychain: K,
    file: SealedFile,
}

#[derive(Default, Serialize, Deserialize)]
struct SecretFile {
    /// owner → key → value. `null` marks a secret deleted here, so an old
    /// keychain entry that couldn't be removed doesn't come back.
    #[serde(default)]
    secrets: BTreeMap<String, BTreeMap<String, Option<String>>>,
}

impl SecretFile {
    fn slot(&self, owner: &str, key: &str) -> Option<&Option<String>> {
        self.secrets.get(owner)?.get(key)
    }

    /// Stores `value` (`None` = a deletion marker); returns what was there.
    fn put(&mut self, owner: &str, key: &str, value: Option<String>) -> Option<Option<String>> {
        self.secrets.entry(owner.into()).or_default().insert(key.into(), value)
    }

    /// Forgets the slot entirely; true if there was one.
    fn remove(&mut self, owner: &str, key: &str) -> bool {
        let Some(keys) = self.secrets.get_mut(owner) else {
            return false;
        };
        let had = keys.remove(key).is_some();
        if keys.is_empty() {
            self.secrets.remove(owner);
        }
        had
    }
}

impl<K: SecretStore> VaultSecrets<K> {
    /// Secrets in `<dir>/secrets.vault`.
    pub fn new(dir: &Path, keychain: K) -> Self {
        Self {
            keychain,
            file: SealedFile::new(dir.join("secrets.json")),
        }
    }

    /// Every store in the process shares the file; each change is a
    /// read-modify-write under this lock. Values aren't cached, so two
    /// stores never see stale copies of each other's writes.
    fn guard() -> MutexGuard<'static, ()> {
        static LOCK: Mutex<()> = Mutex::new(());
        LOCK.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn read(&self) -> Result<SecretFile> {
        self.file.read(&self.keychain)
    }

    fn write(&self, file: &SecretFile) -> Result<()> {
        self.file.write(&self.keychain, file)
    }

    /// A secret now lives in the file; deal with the keychain entry an
    /// older version may have left for it. The file's copy wins on every
    /// read either way.
    ///
    /// `keychain.delete` must not read the entry first: on macOS, reading
    /// an entry another build wrote asks for the password, while deleting
    /// one just fails quietly.
    fn retire_legacy(&self, owner: &str, key: &str) {
        // TODO(you): working default = delete the old entry, best effort.
        // Keeping it instead would let a downgrade to an older version still
        // find the secret, at the cost of a stale copy in the keychain.
        let _ = self.keychain.delete(owner, key);
    }
}

impl<K: SecretStore> SecretStore for VaultSecrets<K> {
    fn get(&self, owner: &str, key: &str) -> Result<Option<String>> {
        if owner == KEY_OWNER {
            return self.keychain.get(owner, key);
        }
        let _g = Self::guard();
        let mut file = self.read()?;
        if let Some(slot) = file.slot(owner, key) {
            return Ok(slot.clone());
        }
        let Some(value) = self.keychain.get(owner, key)? else {
            return Ok(None);
        };
        file.put(owner, key, Some(value.clone()));
        // If the file can't be written, the old entry stays the only copy.
        if self.write(&file).is_ok() {
            self.retire_legacy(owner, key);
        }
        Ok(Some(value))
    }

    fn set(&self, owner: &str, key: &str, value: &str) -> Result<()> {
        if owner == KEY_OWNER {
            return self.keychain.set(owner, key, value);
        }
        let _g = Self::guard();
        let mut file = self.read()?;
        let before = file.put(owner, key, Some(value.into()));
        self.write(&file)?;
        if !matches!(before, Some(Some(_))) {
            self.retire_legacy(owner, key);
        }
        Ok(())
    }

    fn delete(&self, owner: &str, key: &str) -> Result<()> {
        if owner == KEY_OWNER {
            return self.keychain.delete(owner, key);
        }
        let _g = Self::guard();
        let mut file = self.read()?;
        // The user asked for it gone, so an older version's entry goes too.
        // One that can't be removed is masked by a marker instead.
        let changed = if self.keychain.delete(owner, key).is_ok() {
            file.remove(owner, key)
        } else {
            file.put(owner, key, None) != Some(None)
        };
        if changed {
            self.write(&file)?;
        }
        Ok(())
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

    /// A keychain as macOS treats it after an update: every read of an
    /// entry an older build wrote is a dialog, and with `stuck` such entries
    /// can't be deleted either.
    #[derive(Default)]
    struct Keychain {
        entries: Mutex<BTreeMap<(String, String), String>>,
        old: Mutex<std::collections::BTreeSet<(String, String)>>,
        stuck: bool,
        dialogs: std::sync::atomic::AtomicUsize,
    }

    impl Keychain {
        fn with_old(stuck: bool, entries: &[(&str, &str, &str)]) -> Self {
            let k = Keychain { stuck, ..Default::default() };
            for (owner, key, value) in entries {
                let id = (owner.to_string(), key.to_string());
                k.entries.lock().unwrap().insert(id.clone(), value.to_string());
                k.old.lock().unwrap().insert(id);
            }
            k
        }
        fn len(&self) -> usize {
            self.entries.lock().unwrap().len()
        }
        fn dialogs(&self) -> usize {
            self.dialogs.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    impl SecretStore for Keychain {
        fn get(&self, owner: &str, key: &str) -> Result<Option<String>> {
            let id = (owner.to_string(), key.to_string());
            if self.old.lock().unwrap().contains(&id) {
                self.dialogs.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
            Ok(self.entries.lock().unwrap().get(&id).cloned())
        }
        fn set(&self, owner: &str, key: &str, value: &str) -> Result<()> {
            let id = (owner.to_string(), key.to_string());
            self.old.lock().unwrap().remove(&id);
            self.entries.lock().unwrap().insert(id, value.into());
            Ok(())
        }
        fn delete(&self, owner: &str, key: &str) -> Result<()> {
            let id = (owner.to_string(), key.to_string());
            if self.stuck && self.old.lock().unwrap().contains(&id) {
                return Err(CoreError::Other("not this build's entry".into()));
            }
            self.old.lock().unwrap().remove(&id);
            self.entries.lock().unwrap().remove(&id);
            Ok(())
        }
    }

    #[test]
    fn every_secret_shares_the_one_keychain_entry() {
        let dir = tempfile::tempdir().unwrap();
        let v = VaultSecrets::new(dir.path(), Keychain::default());
        // Longer than one Windows Credential Manager entry: no chunks now.
        let token = "t".repeat(5000);
        v.set("dropbox", "refresh_token", &token).unwrap();
        v.set("dropbox", "client_secret", "s3cret").unwrap();
        v.set("db:prod", "password", "hunter2").unwrap();
        assert_eq!(v.get("dropbox", "refresh_token").unwrap().as_deref(), Some(token.as_str()));
        assert_eq!(v.get("db:prod", "password").unwrap().as_deref(), Some("hunter2"));
        assert_eq!(v.get("dropbox", "missing").unwrap(), None);
        assert_eq!(v.keychain.len(), 1, "only the file key");
        let raw = fs::read(dir.path().join("secrets.vault")).unwrap();
        assert!(!String::from_utf8_lossy(&raw).contains("hunter2"));
    }

    #[test]
    fn old_entries_move_into_the_file_on_first_read() {
        let dir = tempfile::tempdir().unwrap();
        let old = [("dropbox", "refresh_token", "tok"), ("dropbox", "client_secret", "sec")];
        let v = VaultSecrets::new(dir.path(), Keychain::with_old(false, &old));
        assert_eq!(v.get("dropbox", "refresh_token").unwrap().as_deref(), Some("tok"));
        assert_eq!(v.get("dropbox", "refresh_token").unwrap().as_deref(), Some("tok"));
        assert_eq!(v.keychain.dialogs(), 1, "one dialog, then the file answers");
        assert_eq!(v.get("dropbox", "client_secret").unwrap().as_deref(), Some("sec"));
        assert_eq!(v.keychain.len(), 1, "old entries retired; only the file key is left");
    }

    #[test]
    fn an_old_entry_that_cannot_be_removed_stays_masked() {
        let dir = tempfile::tempdir().unwrap();
        let old = [("p", "password", "old"), ("p", "token", "old")];
        let v = VaultSecrets::new(dir.path(), Keychain::with_old(true, &old));
        // Deleting never reads it, so no dialog, and it doesn't come back.
        v.delete("p", "password").unwrap();
        assert_eq!(v.get("p", "password").unwrap(), None);
        assert_eq!(v.keychain.dialogs(), 0);
        // A newer value wins over the copy left behind.
        v.set("p", "token", "new").unwrap();
        assert_eq!(v.get("p", "token").unwrap().as_deref(), Some("new"));
        v.delete("p", "token").unwrap();
        assert_eq!(v.get("p", "token").unwrap(), None);
        assert_eq!(v.keychain.dialogs(), 0);
    }

    #[test]
    fn stores_sharing_a_folder_see_each_others_changes() {
        let dir = tempfile::tempdir().unwrap();
        let keychain = Keychain::default();
        let a = VaultSecrets::new(dir.path(), &keychain);
        let b = VaultSecrets::new(dir.path(), &keychain);
        a.set("p", "password", "one").unwrap();
        assert_eq!(b.get("p", "password").unwrap().as_deref(), Some("one"));
        b.set("p", "password", "two").unwrap();
        assert_eq!(a.get("p", "password").unwrap().as_deref(), Some("two"));
        b.delete("p", "password").unwrap();
        assert_eq!(a.get("p", "password").unwrap(), None);
    }

    #[test]
    fn other_sealed_files_take_their_key_from_the_keychain_through_it() {
        let dir = tempfile::tempdir().unwrap();
        let v = VaultSecrets::new(dir.path(), Keychain::default());
        v.set("p", "password", "x").unwrap();
        let f = SealedFile::new(dir.path().join("profiles.json"));
        f.write(&v, &sample()).unwrap();
        assert_eq!(f.read::<Data>(&v).unwrap(), sample());
        assert_eq!(v.keychain.len(), 1, "profiles.vault and secrets.vault share the key");
    }

    #[test]
    fn hex_round_trip() {
        let k = [0xabu8; 32];
        assert_eq!(from_hex(&to_hex(&k)), Some(k));
        assert_eq!(from_hex("zz"), None);
    }
}
