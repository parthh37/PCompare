//! Where saved connections keep their passwords.
//!
//! Remote profiles and database connections both store secrets through this
//! trait; the app plugs in the OS keychain (see `pcompare_remote::profile::
//! KeyringSecrets`) and tests use [`MemorySecrets`].

use crate::Result;
use std::collections::HashMap;
use std::sync::Mutex;

pub trait SecretStore: Send + Sync {
    fn get(&self, profile: &str, key: &str) -> Result<Option<String>>;
    fn set(&self, profile: &str, key: &str, value: &str) -> Result<()>;
    fn delete(&self, profile: &str, key: &str) -> Result<()>;
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
