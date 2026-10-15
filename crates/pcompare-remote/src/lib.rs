//! Remote file systems for PCompare.
//!
//! A remote location is written `remote://<profile-id>/<path>`, where the
//! profile holds the vendor, endpoint and credentials. [`RemoteManager`]
//! turns such a path into a [`Vfs`] the folder engine can walk, a
//! [`VfsWrite`] Folder Sync can write to, or a local cached copy for the
//! text and hex engines.

mod containers;
mod dropbox;
mod github_fs;
mod graph_fs;
mod opendal_fs;
pub mod oauth;
pub mod profile;
pub mod prompt;
pub mod provider;
mod sftp_fs;

use pcompare_core::vfs::{LocalFs, Vfs, VfsEntry};
use pcompare_core::vfs_write::VfsWrite;
use pcompare_core::{CoreError, Result};
use profile::{Profile, ProfileInput, ProfileStore, ResolvedProfile, SecretStore};
use prompt::Prompter;
use provider::{spec, OAuthSpec, ProviderKind, ACCESS_TOKEN, REFRESH_TOKEN};
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::io::{self, Read, Write};
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

pub use github_fs::{decode_ref as decode_github_ref, encode_ref as encode_github_ref};
pub use containers::{is_container_path, is_inside_container, read_only_inside};

/// Errors from a remote byte stream: anything but a clear refusal means the
/// link failed, which callers retry (and resume) rather than give up on.
pub(crate) fn net_err(e: io::Error) -> io::Error {
    use io::ErrorKind as K;
    match e.kind() {
        K::NotFound | K::PermissionDenied | K::Interrupted | K::InvalidInput | K::Unsupported => e,
        _ => io::Error::new(K::ConnectionReset, e.to_string()),
    }
}

/// A remote download stream, with failures classified by [`net_err`].
pub(crate) struct NetRead<R>(pub R);

impl<R: Read> Read for NetRead<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.read(buf).map_err(net_err)
    }
}

fn is_network(e: &io::Error) -> bool {
    use io::ErrorKind as K;
    matches!(
        e.kind(),
        K::ConnectionReset | K::ConnectionAborted | K::TimedOut | K::BrokenPipe | K::UnexpectedEof | K::NetworkDown
    )
}

/// The OAuth app a profile signs in through: its own registration when it
/// has a Client ID, otherwise the one built into this release. A refresh
/// token only works with the app that issued it, so switching apps means
/// signing in again.
fn oauth_client<'a>(p: &'a ResolvedProfile, oauth: &OAuthSpec) -> Result<(&'a str, Option<&'a str>)> {
    if let Some(id) = p.get("client_id") {
        return Ok((id, p.get("client_secret")));
    }
    match oauth.builtin {
        Some(app) => Ok((app.id, app.secret)),
        None => Err(CoreError::Other("enter a Client ID first".into())),
    }
}

/// Attempts at a download before giving up; each resumes where the last
/// one stopped when the server supports ranged reads.
const DOWNLOAD_ATTEMPTS: u32 = 4;

struct Cached {
    fs: Arc<dyn VfsWrite>,
    created: Instant,
}

pub struct RemoteManager {
    pub profiles: ProfileStore,
    conns: Mutex<HashMap<String, Cached>>,
    cache_dir: PathBuf,
    /// Answers sign-in questions (keyboard-interactive) through the UI.
    prompter: Mutex<Option<Arc<dyn Prompter>>>,
    /// One download at a time per cached file, so concurrent requests for
    /// the same file don't write the same partial file.
    downloads: Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>,
    /// Archives, snapshots and svn: repositories (containers.rs).
    containers: containers::Containers,
    /// The sign-in waiting for the browser, if any: its number and the
    /// switch that cancels it (sending, or dropping the sender, cancels).
    sign_in: Mutex<Option<(u64, tokio::sync::oneshot::Sender<()>)>>,
}

/// How long a live connection is reused before reconnecting. OAuth access
/// tokens expire after ~1h; FTP sessions are recycled sooner so a
/// server-side idle timeout surfaces as a reconnect, not an error. SFTP
/// reconnects by itself when the link drops, so it is kept longer — a
/// sign-in that asks for a one-time code shouldn't come back every few
/// minutes.
fn ttl(kind: ProviderKind) -> Duration {
    match kind {
        ProviderKind::Onedrive | ProviderKind::Dropbox | ProviderKind::Gdrive | ProviderKind::Sharepoint => {
            Duration::from_secs(45 * 60)
        }
        ProviderKind::Ftp => Duration::from_secs(5 * 60),
        ProviderKind::Sftp => Duration::from_secs(60 * 60),
        _ => Duration::from_secs(30 * 60),
    }
}

impl RemoteManager {
    pub fn new(config_dir: &Path, cache_dir: PathBuf, secrets: Box<dyn SecretStore>) -> Self {
        Self {
            profiles: ProfileStore::new(config_dir.join("profiles.json"), secrets),
            conns: Mutex::new(HashMap::new()),
            containers: containers::Containers::new(cache_dir.join("archives")),
            cache_dir,
            prompter: Mutex::new(None),
            downloads: Mutex::new(HashMap::new()),
            sign_in: Mutex::new(None),
        }
    }

    /// Installs the UI hook that answers servers' sign-in questions.
    pub fn set_prompter(&self, prompter: Arc<dyn Prompter>) {
        *self.prompter.lock().unwrap() = Some(prompter);
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
    fn connect(&self, p: &ResolvedProfile, pin_host_key: bool) -> Result<Arc<dyn VfsWrite>> {
        let kind = p.profile.kind;
        if kind == ProviderKind::Github {
            return Ok(Arc::new(github_fs::GithubFs::connect(p)?));
        }
        if kind == ProviderKind::Sftp {
            let prompter = self.prompter.lock().unwrap().clone();
            let conn = sftp_fs::SftpFs::connect(p, prompter)?;
            if pin_host_key && p.get("host_key").is_none() {
                self.profiles.set_setting(&p.profile.id, "host_key", &conn.host_key)?;
            }
            return Ok(Arc::new(conn.fs));
        }
        let token = match (spec(kind).oauth, p.get(ACCESS_TOKEN)) {
            // A pasted token is used as is (sign_in removes it again).
            (Some(_), Some(pasted)) => Some(pasted.to_owned()),
            (Some(oauth), None) => {
                let refresh_token = p.get(REFRESH_TOKEN).ok_or_else(|| {
                    let paste = spec(kind).fields.iter().any(|f| f.key == ACCESS_TOKEN);
                    CoreError::Other(format!(
                        "`{}` is not signed in — open Profiles and click Sign in{}",
                        p.profile.name,
                        if paste { " or paste an access token" } else { "" }
                    ))
                })?;
                let (client_id, client_secret) = oauth_client(p, &oauth)?;
                let tokens = rt().block_on(oauth::refresh(&oauth, client_id, client_secret, refresh_token))?;
                // Providers may rotate refresh tokens; keep the newest.
                if let Some(new) = tokens.refresh_token.as_deref().filter(|t| *t != refresh_token) {
                    self.profiles.set_secret(&p.profile.id, REFRESH_TOKEN, new)?;
                }
                Some(tokens.access_token)
            }
            (None, _) => None,
        };
        if kind == ProviderKind::Sharepoint {
            return Ok(Arc::new(graph_fs::GraphFs::connect(p, token.as_deref().unwrap_or_default())?));
        }
        Ok(Arc::new(opendal_fs::OpendalFs::connect(p, token.as_deref())?))
    }

    /// The live file system for a saved profile (cached). One connection per
    /// profile is shared by every caller; the back ends handle concurrent
    /// requests (OpenDAL pools HTTP/FTP connections, SFTP spreads requests
    /// over several channels).
    pub fn fs(&self, id: &str) -> Result<Arc<dyn Vfs>> {
        Ok(self.fs_write(id)?)
    }

    /// [`fs`](Self::fs) with write access, for Folder Sync and the remote browser.
    pub fn fs_write(&self, id: &str) -> Result<Arc<dyn VfsWrite>> {
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
        // inside archives, snapshots and svn: repositories
        if let Some(r) = self.resolve_container(path, false)? {
            return Ok((r.fs, r.path));
        }
        match parse_remote(path) {
            Some((id, rel)) => Ok((self.fs(id)?, rel.to_owned())),
            None => Ok((Arc::new(LocalFs), path.to_owned())),
        }
    }

    /// The writable file system and relative path behind a `remote://` path;
    /// `None` for local paths.
    pub fn target_write(&self, path: &str) -> Result<Option<(Arc<dyn VfsWrite>, String)>> {
        // zip archives are writable, other containers refuse writes
        if let Some(r) = self.resolve_container(path, false)? {
            return Ok(Some((r.fs, r.path)));
        }
        match parse_remote(path) {
            Some((id, rel)) => Ok(Some((self.fs_write(id)?, rel.to_owned()))),
            None => Ok(None),
        }
    }

    fn remote_target(&self, path: &str) -> Result<(Arc<dyn VfsWrite>, String)> {
        self.target_write(path)?
            .ok_or_else(|| CoreError::Other(format!("{path} is not a remote path")))
    }

    /// Creates a folder (and missing parents) on a remote.
    pub fn create_dir(&self, path: &str) -> Result<()> {
        let (fs, rel) = self.remote_target(path)?;
        fs.create_dir_all(&rel).map_err(|e| CoreError::remote(path, e))
    }

    /// Deletes a remote file or folder tree — permanently: remotes have no
    /// Recycle Bin.
    pub fn delete(&self, path: &str) -> Result<()> {
        let (fs, rel) = self.remote_target(path)?;
        if rel.is_empty() {
            return Err(CoreError::Other("refusing to delete a profile's root folder".into()));
        }
        let is_dir = fs.stat(&rel).map_err(|e| CoreError::remote(path, e))?.is_some_and(|m| m.is_dir);
        if is_dir { fs.remove_dir_all(&rel) } else { fs.remove_file(&rel) }.map_err(|e| CoreError::remote(path, e))
    }

    /// Renames a remote file or folder within its folder.
    pub fn rename(&self, path: &str, new_name: &str) -> Result<()> {
        let (fs, rel) = self.remote_target(path)?;
        let bad = new_name.is_empty() || new_name.contains(['/', '\\']) || new_name == "." || new_name == "..";
        if rel.is_empty() || bad {
            return Err(CoreError::Other(format!("can't rename to `{new_name}`")));
        }
        if !fs.caps().rename {
            return Err(CoreError::Other("this server can't rename files".into()));
        }
        let parent = rel.rsplit_once('/').map_or("", |(p, _)| p);
        let to = fs.join(parent, new_name);
        if fs.stat(&to).map_err(|e| CoreError::remote(path, e))?.is_some() {
            return Err(CoreError::Other(format!("`{new_name}` already exists")));
        }
        fs.rename(&rel, &to).map_err(|e| CoreError::remote(path, e))
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

    /// Lists the folders in `dir` with unsaved form values, ignoring the
    /// profile's Root folder, so the user can pick one. `dir` is relative
    /// to the account's top level.
    pub fn browse(&self, input: &ProfileInput, dir: &str) -> Result<Vec<VfsEntry>> {
        let mut resolved = self.profiles.preview(input)?;
        resolved.profile.settings.remove("root");
        let fs = self.connect(&resolved, false)?;
        let dir = dir.trim_matches('/');
        let listed = fs.read_dir(dir)?;
        // Some services (Dropbox) list a missing folder as empty.
        if listed.is_empty() && fs.stat(dir).map_err(|e| CoreError::remote(dir, e))?.is_none_or(|m| !m.is_dir) {
            return Err(CoreError::Other(format!("folder `/{dir}` not found")));
        }
        let mut folders: Vec<VfsEntry> = listed.into_iter().filter(|e| e.meta.is_dir).collect();
        folders.sort_by_key(|e| e.name.to_lowercase());
        Ok(folders)
    }

    /// Interactive OAuth sign-in for a saved profile; stores the refresh
    /// token. Starting a second sign-in cancels the first.
    pub fn sign_in(&self, id: &str, open_browser: impl FnOnce(&str) -> Result<()>) -> Result<Profile> {
        let p = self.profiles.resolve(id)?;
        let oauth = spec(p.profile.kind)
            .oauth
            .ok_or_else(|| CoreError::Other(format!("{:?} does not use sign-in", p.profile.kind)))?;
        let (client_id, client_secret) = oauth_client(&p, &oauth)?;

        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let ticket = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();
        *self.sign_in.lock().unwrap() = Some((ticket, cancel_tx));
        let cancelled = async {
            let _ = cancel_rx.await;
        };
        let result = rt().block_on(oauth::authorize(&oauth, client_id, client_secret, open_browser, cancelled));
        {
            let mut slot = self.sign_in.lock().unwrap();
            if slot.as_ref().is_some_and(|(t, _)| *t == ticket) {
                *slot = None;
            }
        }

        let refresh = result?.refresh_token.ok_or_else(|| {
            CoreError::Other("the provider did not return a refresh token; check the app's offline-access permission".into())
        })?;
        self.profiles.set_secret(id, REFRESH_TOKEN, &refresh)?;
        // The newest way of authenticating wins: drop a pasted token.
        self.profiles.remove_secret(id, ACCESS_TOKEN)?;
        self.invalidate(id);
        self.profiles.get(id)
    }

    /// Stops a sign-in that is waiting for the browser. Harmless when none is.
    pub fn cancel_sign_in(&self) {
        if let Some((_, tx)) = self.sign_in.lock().unwrap().take() {
            let _ = tx.send(());
        }
    }

    /// Downloads a remote file into the cache and returns the local copy, so
    /// the text and hex engines can work on it. Local paths pass through.
    /// With `refresh = false` an existing copy is reused (hex paging calls
    /// this for every 4 KB page).
    ///
    /// Interrupted downloads resume: the partial file is kept (with the
    /// remote file's size and timestamp beside it) and continued with a
    /// ranged read — by the retries here after a dropped connection, and by
    /// the next call if every attempt failed.
    pub fn local_copy(&self, path: &str, refresh: bool) -> Result<PathBuf> {
        // a file inside an archive or repository
        if let Some(copy) = self.container_copy(path)? {
            return Ok(copy);
        }
        let Some((id, rel)) = parse_remote(path) else {
            return Ok(PathBuf::from(path));
        };
        let mut h = std::collections::hash_map::DefaultHasher::new();
        path.hash(&mut h);
        let name = rel.rsplit('/').next().filter(|n| !n.is_empty()).unwrap_or("file");
        let dir = self.cache_dir.join(format!("{:016x}", h.finish()));
        let dest = dir.join(name);
        let lock = self.downloads.lock().unwrap().entry(dest.clone()).or_default().clone();
        let _one_at_a_time = lock.lock().unwrap_or_else(|p| p.into_inner());
        if !refresh && dest.exists() {
            return Ok(dest);
        }
        std::fs::create_dir_all(&dir).map_err(|e| CoreError::io(&dir, e))?;
        let part = Partial {
            data: dir.join(format!(".{name}.part")),
            stamp: dir.join(format!(".{name}.part.json")),
        };
        let mut attempt = 1;
        loop {
            let fs = self.fs_write(id)?;
            match part.download(&*fs, rel) {
                Ok(()) => {
                    std::fs::rename(&part.data, &dest).map_err(|e| CoreError::io(&dest, e))?;
                    let _ = std::fs::remove_file(&part.stamp);
                    return Ok(dest);
                }
                Err(e) if is_network(&e) && attempt < DOWNLOAD_ATTEMPTS => {
                    // Reconnect and carry on from where the data stopped.
                    self.invalidate(id);
                    std::thread::sleep(Duration::from_millis(500 << attempt));
                    attempt += 1;
                }
                Err(e) => return Err(CoreError::remote(path, e)),
            }
        }
    }

    /// Removes all downloaded copies (called on startup).
    pub fn clear_cache(&self) {
        let _ = std::fs::remove_dir_all(&self.cache_dir);
    }
}

/// A download in progress: the data so far, and the remote file's identity
/// (`size:modified`) the data belongs to.
struct Partial {
    data: PathBuf,
    stamp: PathBuf,
}

impl Partial {
    /// Fetches what is missing of `rel` into the partial file. The partial
    /// data is only continued if the remote file is unchanged and the server
    /// can read from an offset; otherwise the download starts over.
    fn download(&self, fs: &dyn VfsWrite, rel: &str) -> io::Result<()> {
        let meta = fs
            .stat(rel)?
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "the file doesn't exist"))?;
        if meta.is_dir {
            return Err(io::Error::new(io::ErrorKind::IsADirectory, "this is a folder"));
        }
        // Without a timestamp an edit of the same size would go unnoticed.
        let identity = meta.modified_ms.map(|ms| format!("{}:{ms}", meta.size));
        let have = std::fs::metadata(&self.data).map(|m| m.len()).unwrap_or(0);
        let resumable = fs.caps().range_read
            && identity.is_some()
            && std::fs::read_to_string(&self.stamp).ok() == identity
            && have <= meta.size;
        let offset = if resumable { have } else { 0 };
        let mut out = if offset == 0 {
            match &identity {
                Some(id) => std::fs::write(&self.stamp, id)?,
                None => {
                    let _ = std::fs::remove_file(&self.stamp);
                }
            }
            std::fs::File::create(&self.data)?
        } else {
            std::fs::OpenOptions::new().append(true).open(&self.data)?
        };
        if offset < meta.size || meta.size == 0 {
            let mut src = fs.open_at(rel, offset)?;
            let copied = io::copy(&mut src, &mut out);
            out.flush()?;
            copied?;
        }
        out.sync_all()?;
        let got = out.metadata()?.len();
        if got != meta.size {
            // The file changed under us; the next attempt starts afresh.
            let _ = std::fs::remove_file(&self.stamp);
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!("downloaded {got} of {} bytes", meta.size),
            ));
        }
        Ok(())
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
    fn partial_downloads_resume_only_for_the_same_file() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.bin");
        std::fs::write(&src, "0123456789").unwrap();
        let src = src.to_string_lossy().into_owned();
        let part = Partial {
            data: dir.path().join(".src.bin.part"),
            stamp: dir.path().join(".src.bin.part.json"),
        };
        part.download(&LocalFs, &src).unwrap();
        assert_eq!(std::fs::read_to_string(&part.data).unwrap(), "0123456789");

        // A cut-off earlier attempt: the stamp matches, so only the rest is
        // fetched (the marker bytes prove the prefix was kept).
        std::fs::write(&part.data, "ABCD").unwrap();
        part.download(&LocalFs, &src).unwrap();
        assert_eq!(std::fs::read_to_string(&part.data).unwrap(), "ABCD456789");

        // The remote file changed since: start over.
        std::fs::write(&part.data, "ABCD").unwrap();
        std::fs::write(&part.stamp, "10:1").unwrap();
        part.download(&LocalFs, &src).unwrap();
        assert_eq!(std::fs::read_to_string(&part.data).unwrap(), "0123456789");
    }

    #[test]
    fn local_paths_pass_through() {
        let dir = tempfile::tempdir().unwrap();
        let m = RemoteManager::new(dir.path(), dir.path().join("cache"), Box::<profile::MemorySecrets>::default());
        assert_eq!(m.local_copy("/tmp/x.txt", true).unwrap(), PathBuf::from("/tmp/x.txt"));
        assert!(m.local_copy("remote://nope/x.txt", true).is_err());
    }
}
