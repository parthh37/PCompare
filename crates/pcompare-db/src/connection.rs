//! Saved database connections.
//!
//! Like remote profiles, the password goes to the secret store (sealed in
//! `secrets.vault`) and the rest is sealed in `db-connections.vault` (see
//! [`pcompare_core::vault`]). A DB
//! session's "path" is `db://<connection-id>` or `db://<connection-id>/<database>`
//! (the second form lets one saved server compare two of its databases).

use crate::model::Engine;
use pcompare_core::secrets::SecretStore;
use pcompare_core::store::now_ms;
use pcompare_core::vault::SealedFile;
use pcompare_core::{CoreError, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Mutex;

pub const DB_SCHEME: &str = "db://";
const PASSWORD: &str = "password";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AuthMethod {
    /// User name + password.
    #[default]
    Password,
    /// SQL Server Windows (integrated) authentication — Windows only.
    Integrated,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TlsMode {
    Disable,
    /// Encrypt when the server supports it.
    #[default]
    Prefer,
    Require,
}

/// Everything needed to open a connection. `password` is never written to
/// disk or sent back to the UI; the store fills it from the keychain.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ConnectionConfig {
    pub engine: Option<Engine>,
    pub host: String,
    /// `None` = the engine's default port.
    pub port: Option<u16>,
    /// SQL Server named instance (`HOST\INSTANCE`), resolved via SQL Browser.
    pub instance: String,
    pub database: String,
    pub user: String,
    #[serde(skip_serializing)]
    pub password: String,
    pub auth: AuthMethod,
    pub tls: TlsMode,
    /// Accept self-signed server certificates (common on SQL Server).
    pub trust_server_certificate: bool,
    /// SQLite database file.
    pub file: String,
    /// Only compare these schemas (PostgreSQL / SQL Server); empty = all user schemas.
    pub schemas: Vec<String>,
}

impl ConnectionConfig {
    pub fn engine(&self) -> Result<Engine> {
        self.engine.ok_or_else(|| CoreError::Other("connection has no database engine".into()))
    }

    pub fn port_or_default(&self) -> u16 {
        self.port.or(self.engine.and_then(Engine::default_port)).unwrap_or(0)
    }

    /// `server/database` label for headers and errors (no credentials).
    pub fn describe(&self) -> String {
        match self.engine {
            Some(Engine::Sqlite) => self.file.clone(),
            _ => {
                let mut host = self.host.clone();
                if !self.instance.is_empty() {
                    host = format!("{host}\\{}", self.instance);
                } else if let Some(p) = self.port {
                    host = format!("{host}:{p}");
                }
                if self.database.is_empty() { host } else { format!("{host}/{}", self.database) }
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DbConnection {
    pub id: String,
    pub name: String,
    pub config: ConnectionConfig,
    /// A password is held in the keychain (the value itself never leaves Rust).
    #[serde(default)]
    pub has_password: bool,
    pub created_ms: i64,
    pub modified_ms: i64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DbConnectionInput {
    pub id: Option<String>,
    pub name: String,
    pub config: ConnectionConfig,
    /// A value replaces the stored password, `""` deletes it, absent keeps it.
    #[serde(default)]
    pub password: Option<String>,
}

#[derive(Default, Serialize, Deserialize)]
struct ConnectionFile {
    #[serde(default)]
    connections: Vec<DbConnection>,
}

/// Splits `db://<id>[/<database>]`.
pub fn parse_db_path(path: &str) -> Option<(&str, Option<&str>)> {
    let rest = path.strip_prefix(DB_SCHEME)?;
    let (id, db) = match rest.split_once('/') {
        Some((id, db)) => (id, Some(db).filter(|d| !d.is_empty())),
        None => (rest, None),
    };
    (!id.is_empty()).then_some((id, db))
}

pub fn is_db_path(path: &str) -> bool {
    parse_db_path(path).is_some()
}

pub struct ConnectionStore {
    file: SealedFile,
    secrets: Box<dyn SecretStore>,
    lock: Mutex<()>,
}

fn secret_owner(id: &str) -> String {
    format!("db:{id}")
}

impl ConnectionStore {
    pub fn new(path: impl Into<PathBuf>, secrets: Box<dyn SecretStore>) -> Self {
        Self {
            file: SealedFile::new(path),
            secrets,
            lock: Mutex::new(()),
        }
    }

    fn guard(&self) -> std::sync::MutexGuard<'_, ()> {
        self.lock.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn list(&self) -> Result<Vec<DbConnection>> {
        let _g = self.guard();
        let mut file: ConnectionFile = self.file.read(&*self.secrets)?;
        file.connections.sort_by_key(|c| c.name.to_lowercase());
        Ok(file.connections)
    }

    pub fn get(&self, id: &str) -> Result<DbConnection> {
        self.list()?
            .into_iter()
            .find(|c| c.id == id)
            .ok_or_else(|| CoreError::Other(format!("no saved database connection `{id}`")))
    }

    /// The connection's config with its password filled in.
    pub fn resolve(&self, id: &str) -> Result<ConnectionConfig> {
        let conn = self.get(id)?;
        let mut cfg = conn.config;
        if conn.has_password {
            cfg.password = self.secrets.get(&secret_owner(id), PASSWORD)?.unwrap_or_default();
        }
        Ok(cfg)
    }

    /// Resolves a session path; `db://id/other_db` overrides the database.
    pub fn resolve_path(&self, path: &str) -> Result<ConnectionConfig> {
        let (id, db) =
            parse_db_path(path).ok_or_else(|| CoreError::Other(format!("`{path}` is not a database connection")))?;
        let mut cfg = self.resolve(id)?;
        if let Some(db) = db {
            cfg.database = db.to_owned();
        }
        Ok(cfg)
    }

    /// The config a save *would* produce (unsaved edits + stored password),
    /// used by "Test connection" and the database picker before saving.
    pub fn preview(&self, input: &DbConnectionInput) -> Result<ConnectionConfig> {
        let mut cfg = input.config.clone();
        cfg.password = match (&input.password, &input.id) {
            (Some(p), _) => p.clone(),
            (None, Some(id)) => self.secrets.get(&secret_owner(id), PASSWORD)?.unwrap_or_default(),
            (None, None) => String::new(),
        };
        Ok(cfg)
    }

    pub fn save(&self, input: DbConnectionInput) -> Result<DbConnection> {
        let mut saved = self.save_all(vec![input])?;
        Ok(saved.remove(0))
    }

    /// Saves several connections with one read and one write of the vault
    /// (an import can bring dozens). Inputs without an id are added; a name
    /// already in use gets a ` (2)` suffix.
    pub fn save_all(&self, inputs: Vec<DbConnectionInput>) -> Result<Vec<DbConnection>> {
        for input in &inputs {
            if input.name.trim().is_empty() {
                return Err(CoreError::Other("connection name cannot be empty".into()));
            }
            input.config.engine()?;
        }
        let _g = self.guard();
        let mut file: ConnectionFile = self.file.read(&*self.secrets)?;
        let now = now_ms();
        let mut out = Vec::with_capacity(inputs.len());
        for input in inputs {
            let idx = input.id.as_ref().and_then(|id| file.connections.iter().position(|c| &c.id == id));
            let name = match idx {
                Some(_) => input.name.trim().to_owned(),
                None => unique_name(input.name.trim(), &file.connections),
            };
            let id = match idx {
                Some(i) => file.connections[i].id.clone(),
                None => unique_slug(&name, &file.connections),
            };
            let mut has_password = idx.is_some_and(|i| file.connections[i].has_password);
            match input.password.as_deref() {
                Some("") => {
                    self.secrets.delete(&secret_owner(&id), PASSWORD)?;
                    has_password = false;
                }
                Some(p) => {
                    self.secrets.set(&secret_owner(&id), PASSWORD, p)?;
                    has_password = true;
                }
                None => {}
            }
            let mut config = input.config;
            config.password.clear();
            let conn = DbConnection {
                id,
                name,
                config,
                has_password,
                created_ms: idx.map_or(now, |i| file.connections[i].created_ms),
                modified_ms: now,
            };
            match idx {
                Some(i) => file.connections[i] = conn.clone(),
                None => file.connections.push(conn.clone()),
            }
            out.push(conn);
        }
        self.file.write(&*self.secrets, &file)?;
        Ok(out)
    }

    pub fn delete(&self, id: &str) -> Result<()> {
        let _g = self.guard();
        let mut file: ConnectionFile = self.file.read(&*self.secrets)?;
        file.connections.retain(|c| c.id != id);
        self.file.write(&*self.secrets, &file)?;
        self.secrets.delete(&secret_owner(id), PASSWORD)
    }
}

/// `name`, or `name (2)`, `name (3)`... when another connection uses it.
fn unique_name(name: &str, existing: &[DbConnection]) -> String {
    let used = |n: &str| existing.iter().any(|c| c.name.eq_ignore_ascii_case(n));
    if !used(name) {
        return name.to_owned();
    }
    (2..).map(|i| format!("{name} ({i})")).find(|n| !used(n)).expect("finitely many names")
}

/// `"Prod SQL (EU)"` → `"prod-sql-eu"`, suffixed `-2`, `-3`... when taken.
fn unique_slug(name: &str, existing: &[DbConnection]) -> String {
    let mut base = String::new();
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            base.push(c.to_ascii_lowercase());
        } else if !base.is_empty() && !base.ends_with('-') {
            base.push('-');
        }
    }
    let base = match base.trim_end_matches('-') {
        "" => "db".to_owned(),
        b => b.to_owned(),
    };
    let taken = |s: &str| existing.iter().any(|c| c.id == s);
    if !taken(&base) {
        return base;
    }
    (2..).map(|n| format!("{base}-{n}")).find(|s| !taken(s)).expect("finitely many ids")
}

#[cfg(test)]
mod tests {
    use super::*;
    use pcompare_core::secrets::MemorySecrets;

    fn store(dir: &tempfile::TempDir) -> ConnectionStore {
        ConnectionStore::new(dir.path().join("db.json"), Box::<MemorySecrets>::default())
    }

    fn input(name: &str, password: Option<&str>) -> DbConnectionInput {
        DbConnectionInput {
            id: None,
            name: name.into(),
            config: ConnectionConfig {
                engine: Some(Engine::Mysql),
                host: "localhost".into(),
                database: "shop".into(),
                user: "root".into(),
                ..Default::default()
            },
            password: password.map(Into::into),
        }
    }

    #[test]
    fn password_goes_to_secrets_not_disk() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(&dir);
        let c = s.save(input("Prod DB", Some("hunter2"))).unwrap();
        assert_eq!(c.id, "prod-db");
        assert!(c.has_password);
        let raw = std::fs::read(dir.path().join("db.vault")).unwrap();
        let raw = String::from_utf8_lossy(&raw);
        assert!(!raw.contains("hunter2") && !raw.contains("Prod DB"), "sealed, not plaintext");
        let plain: serde_json::Value = s.file.read(&*s.secrets).unwrap();
        assert!(!plain.to_string().contains("hunter2"));
        assert_eq!(s.resolve("prod-db").unwrap().password, "hunter2");

        // Absent password keeps it; "" deletes it.
        let mut keep = input("Prod DB", None);
        keep.id = Some(c.id.clone());
        assert!(s.save(keep).unwrap().has_password);
        let mut clear = input("Prod DB", Some(""));
        clear.id = Some(c.id.clone());
        assert!(!s.save(clear).unwrap().has_password);
        assert_eq!(s.resolve("prod-db").unwrap().password, "");
    }

    #[test]
    fn save_all_writes_once_and_keeps_names_apart() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(&dir);
        s.save(input("Prod DB", None)).unwrap();
        let saved = s
            .save_all(vec![input("Prod DB", Some("pw")), input("Dev", None), input("dev", None)])
            .unwrap();
        let names: Vec<_> = saved.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["Prod DB (2)", "Dev", "dev (2)"]);
        assert_eq!(s.resolve(&saved[0].id).unwrap().password, "pw");
        assert_eq!(s.list().unwrap().len(), 4);
        assert!(s.save_all(vec![input("ok", None), input("  ", None)]).is_err());
        assert_eq!(s.list().unwrap().len(), 4, "nothing saved when one input is invalid");
    }

    #[test]
    fn ids_are_unique_and_paths_resolve() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(&dir);
        s.save(input("Prod DB", None)).unwrap();
        assert_eq!(s.save(input("prod db", None)).unwrap().id, "prod-db-2");
        assert_eq!(parse_db_path("db://prod-db/other"), Some(("prod-db", Some("other"))));
        assert_eq!(parse_db_path("db://prod-db"), Some(("prod-db", None)));
        assert_eq!(parse_db_path("C:\\x"), None);
        assert_eq!(s.resolve_path("db://prod-db/other").unwrap().database, "other");
        assert_eq!(s.resolve_path("db://prod-db").unwrap().database, "shop");
    }
}
