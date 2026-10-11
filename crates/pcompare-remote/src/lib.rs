//! Remote file systems for PCompare.
//!
//! A remote location is written `remote://<profile-id>/<path>`, where the
//! profile holds the vendor, endpoint and credentials. [`RemoteManager`]
//! turns such a path into a [`Vfs`] the folder engine can walk, or into a
//! local cached copy for the text and hex engines.

mod opendal_fs;
pub mod oauth;
pub mod profile;
pub mod provider;
mod sftp_fs;

use pcompare_core::store::now_ms;
use pcompare_core::vfs::{LocalFs, Vfs, VfsEntry};
use pcompare_core::{CoreError, Result};
use profile::{Profile, ProfileInput, ProfileStore, ResolvedProfile, SecretStore};
use provider::{spec, ProviderKind, REFRESH_TOKEN};
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

pub const SCHEME: &str = "remote://";

/// A dedicated runtime for network I/O. Engines call into remote file
/// systems from rayon and `spawn_blocking` threads, never from async code.
pub(crate) fn rt() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .thread_name("pcompare-remote")
            .enable_all()
            .build()
            .expect("failed to start the network runtime")
    })
}

/// Splits `remote://id/some/path` into `("id", "some/path")`.
pub fn parse_remote(path: &str) -> Option<(&str, &str)> {
    let rest = path
        .get(..SCHEME.len())
        .filter(|p| p.eq_ignore_ascii_case(SCHEME))
        .map(|_| &path[SCHEME.len()..])?;
    let (id, rel) = rest.split_once('/').unwrap_or((rest, ""));
    (!id.is_empty()).then(|| (id, rel.trim_matches('/')))
}

pub fn is_remote(path: &str) -> bool {
    parse_remote(path).is_some()
}

struct Cached {
    fs: Arc<dyn Vfs>,
    created: Instant,
}

pub struct RemoteManager {
    pub profiles: ProfileStore,
    conns: Mutex<HashMap<String, Cached>>,
    cache_dir: PathBuf,
}

/// How long a live connection is reused before reconnecting. OAuth access
/// tokens expire after ~1h; stateful sessions are recycled sooner so a
/// server-side idle timeout surfaces as a reconnect, not an error.
fn ttl(kind: ProviderKind) -> Duration {
    match kind {
        ProviderKind::Onedrive | ProviderKind::Dropbox | ProviderKind::Gdrive => Duration::from_secs(45 * 60),
        ProviderKind::Sftp | ProviderKind::Ftp => Duration::from_secs(5 * 60),
        _ => Duration::from_secs(30 * 60),
    }
}

impl RemoteManager {
    pub fn new(config_dir: &Path, cache_dir: PathBuf, secrets: Box<dyn SecretStore>) -> Self {
        Self {
            profiles: ProfileStore::new(config_dir.join("profiles.json"), secrets),
            conns: Mutex::new(HashMap::new()),
            cache_dir,
        }
    }

    /// Drops any cached connection, e.g. after the profile was edited.
    pub fn invalidate(&self, id: &str) {
        self.conns.lock().unwrap().remove(id);
    }

    /// Drops the connection behind a path after an error, so the next
    /// attempt reconnects instead of reusing a dead session.
    pub fn forget(&self, path: &str) {
        if let Some((id, _)) = parse_remote(path) {
            self.invalidate(id);
        }
    }

    /// `pin_host_key`: remember an SFTP server's key on first use. Off for
    /// "Test connection", which runs on unsaved form values.
    fn connect(&self, p: &ResolvedProfile, pin_host_key: bool) -> Result<Arc<dyn Vfs>> {
        let kind = p.profile.kind;
        if kind == ProviderKind::Sftp {
            let conn = sftp_fs::SftpFs::connect(p)?;
            if pin_host_key && p.get("host_key").is_none() {
                self.profiles.set_setting(&p.profile.id, "host_key", &conn.host_key)?;
            }
            return Ok(Arc::new(conn.fs));
        }
        let token = match spec(kind).oauth {
            Some(oauth) => {
                let refresh_token = p.get(REFRESH_TOKEN).ok_or_else(|| {
                    CoreError::Other(format!("`{}` is not signed in — open Profiles and click Sign in", p.profile.name))
                })?;
                let client_id = p.get("client_id").unwrap_or_default();
                let tokens = rt().block_on(oauth::refresh(&oauth, client_id, p.get("client_secret"), refresh_token))?;
                // Providers may rotate refresh tokens; keep the newest.
                if let Some(new) = tokens.refresh_token.as_deref().filter(|t| *t != refresh_token) {
                    self.profiles.set_secret(&p.profile.id, REFRESH_TOKEN, new)?;
                }
                Some(tokens.access_token)
            }
            None => None,
        };
        Ok(Arc::new(opendal_fs::OpendalFs::connect(p, token.as_deref())?))
    }

    /// The live file system for a saved profile (cached).
    pub fn fs(&self, id: &str) -> Result<Arc<dyn Vfs>> {
        let resolved = self.profiles.resolve(id)?;
        if let Some(c) = self.conns.lock().unwrap().get(id) {
            if c.created.elapsed() < ttl(resolved.profile.kind) {
                return Ok(c.fs.clone());
            }
        }
        let fs = self.connect(&resolved, true)?;
        self.conns.lock().unwrap().insert(
            id.to_owned(),
            Cached {
                fs: fs.clone(),
                created: Instant::now(),
            },
        );
        Ok(fs)
    }

    /// Resolves any path — local or `remote://` — to a file system + path.
    pub fn target(&self, path: &str) -> Result<(Arc<dyn Vfs>, String)> {
        match parse_remote(path) {
            Some((id, rel)) => Ok((self.fs(id)?, rel.to_owned())),
            None => Ok((Arc::new(LocalFs), path.to_owned())),
        }
    }

    pub fn list(&self, path: &str) -> Result<Vec<VfsEntry>> {
        let (fs, rel) = self.target(path)?;
        let mut entries = fs.read_dir(&rel)?;
        entries.sort_by(|a, b| b.meta.is_dir.cmp(&a.meta.is_dir).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
        Ok(entries)
    }

    /// Connects with unsaved form values and lists the root folder.
    pub fn test(&self, input: &ProfileInput) -> Result<String> {
        let resolved = self.profiles.preview(input)?;
        let fs = self.connect(&resolved, false)?;
        let n = fs.read_dir("")?.len();
        Ok(format!("Connected — {n} item{} in the root folder.", if n == 1 { "" } else { "s" }))
    }

    /// Interactive OAuth sign-in for a saved profile; stores the refresh token.
    pub fn sign_in(&self, id: &str, open_browser: impl FnOnce(&str) -> Result<()>) -> Result<Profile> {
        let p = self.profiles.resolve(id)?;
        let oauth = spec(p.profile.kind)
            .oauth
            .ok_or_else(|| CoreError::Other(format!("{:?} does not use sign-in", p.profile.kind)))?;
        let client_id = p.get("client_id").ok_or_else(|| CoreError::Other("enter a Client ID first".into()))?;
        let tokens = rt().block_on(oauth::authorize(&oauth, client_id, p.get("client_secret"), open_browser))?;
        let refresh = tokens.refresh_token.ok_or_else(|| {
            CoreError::Other("the provider did not return a refresh token; check the app's offline-access permission".into())
        })?;
        self.profiles.set_secret(id, REFRESH_TOKEN, &refresh)?;
        self.invalidate(id);
        self.profiles.get(id)
    }

    /// Downloads a remote file into the cache and returns the local copy, so
    /// the text and hex engines can work on it. Local paths pass through.
    /// With `refresh = false` an existing copy is reused (hex paging calls
    /// this for every 4 KB page).
    pub fn local_copy(&self, path: &str, refresh: bool) -> Result<PathBuf> {
        let Some((id, rel)) = parse_remote(path) else {
            return Ok(PathBuf::from(path));
        };
        let mut h = std::collections::hash_map::DefaultHasher::new();
        path.hash(&mut h);
        let name = rel.rsplit('/').next().filter(|n| !n.is_empty()).unwrap_or("file");
        let dir = self.cache_dir.join(format!("{:016x}", h.finish()));
        let dest = dir.join(name);
        if !refresh && dest.exists() {
            return Ok(dest);
        }
        let fs = self.fs(id)?;
        std::fs::create_dir_all(&dir).map_err(|e| CoreError::io(&dir, e))?;
        let tmp = dir.join(format!(".{name}.{}.part", now_ms()));
        let result = (|| {
            let mut reader = fs.open(rel)?;
            let mut out = std::fs::File::create(&tmp).map_err(|e| CoreError::io(&tmp, e))?;
            std::io::copy(&mut reader, &mut out).map_err(|e| CoreError::remote(path, e))?;
            std::fs::rename(&tmp, &dest).map_err(|e| CoreError::io(&dest, e))
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        result.map(|_| dest)
    }

    /// Removes all downloaded copies (called on startup).
    pub fn clear_cache(&self) {
        let _ = std::fs::remove_dir_all(&self.cache_dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_remote_paths() {
        assert_eq!(parse_remote("remote://my-s3/a/b/"), Some(("my-s3", "a/b")));
        assert_eq!(parse_remote("REMOTE://box"), Some(("box", "")));
        assert_eq!(parse_remote("remote:///x"), None);
        assert_eq!(parse_remote(r"C:\Users\me"), None);
        assert_eq!(parse_remote("/home/me"), None);
    }

    #[test]
    fn local_paths_pass_through() {
        let dir = tempfile::tempdir().unwrap();
        let m = RemoteManager::new(dir.path(), dir.path().join("cache"), Box::<profile::MemorySecrets>::default());
        assert_eq!(m.local_copy("/tmp/x.txt", true).unwrap(), PathBuf::from("/tmp/x.txt"));
        assert!(m.local_copy("remote://nope/x.txt", true).is_err());
    }
}
