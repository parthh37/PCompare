//! What a script needs from the program around it: turning the paths it
//! loads into folder sides (local folders, archives, snapshots, remote
//! profiles), saved sessions by name, local copies of files to report on,
//! and the file format rules.
//!
//! The app's command line plugs in its own [`Host`] built on the same side
//! resolution the Folder Compare view uses (remote profiles included);
//! [`LocalHost`] covers local folders, archives, snapshots and `svn:` /
//! `git:` paths without the app.

use pcompare_archive::{Archives, CrcSource};
use pcompare_convert::ConvertConfig;
use pcompare_core::session::{SavedSession, SessionStore};
use pcompare_core::vfs::{LocalFs, Vfs};
use pcompare_core::vfs_write::VfsWrite;
use pcompare_core::PathKind;
use pcompare_sync::remote::Root;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// One loaded base folder.
#[derive(Clone)]
pub struct Side {
    /// How the folder is shown (an absolute path, `remote://…`, …).
    pub shown: String,
    pub fs: Arc<dyn Vfs>,
    /// The folder's path inside `fs`.
    pub path: String,
    /// Set for a plain local folder.
    pub local: Option<PathBuf>,
    /// How to write to a side that isn't a plain local folder (remote
    /// folders, zip archives; read-only containers refuse writes).
    pub write: Option<(Arc<dyn VfsWrite>, String)>,
    /// Checksums the side stores (zip / 7z headers, snapshots).
    pub crcs: Option<Arc<dyn CrcSource>>,
    /// For a snapshot: whether it holds CRCs.
    pub snapshot: Option<bool>,
}

impl Side {
    /// A plain local folder.
    pub fn local(path: &Path) -> Side {
        let shown = path.to_string_lossy().into_owned();
        Side {
            fs: Arc::new(LocalFs),
            path: shown.clone(),
            shown,
            local: Some(path.to_path_buf()),
            write: None,
            crcs: None,
            snapshot: None,
        }
    }

    /// The side as the sync and file-operation executors take it.
    pub fn root(&self) -> Result<Root<'_>, String> {
        if let Some(p) = &self.local {
            return Ok(Root::Local(p));
        }
        match &self.write {
            Some((fs, path)) => Ok(Root::Remote {
                fs: &**fs,
                path,
                label: &self.shown,
            }),
            None => Err(format!("{} is read-only", self.shown)),
        }
    }

    /// `rel` inside the side, in its file system's syntax.
    pub fn join(&self, rel: &str) -> String {
        rel.split('/')
            .filter(|c| !c.is_empty())
            .fold(self.path.clone(), |dir, c| {
                if dir.is_empty() {
                    c.to_owned()
                } else {
                    self.fs.join(&dir, c)
                }
            })
    }
}

/// The program around a script.
pub trait Host {
    /// What `path` is: a folder (or something that opens as one), a file,
    /// or nothing.
    fn kind(&self, path: &str) -> PathKind;
    /// Opens `path` as a base folder.
    fn folder(&self, path: &str) -> Result<Side, String>;
    /// Creates a missing base folder (`load create:`).
    fn create_folder(&self, path: &str) -> Result<(), String>;
    /// A local copy of a file (remote files are downloaded, files inside
    /// archives extracted); local files as they are.
    fn local_file(&self, path: &str) -> Result<PathBuf, String>;
    /// A saved session by name (case-insensitive).
    fn session(&self, name: &str) -> Option<SavedSession>;
    /// The "File formats" rules.
    fn file_formats(&self) -> ConvertConfig;
    /// Archives, for snapshots and `copyto` into zip files.
    fn archives(&self) -> &Arc<Archives>;
}

/// A host for local paths: folders, archives, snapshots and `svn:` /
/// `git:` / `hg:` revisions. Remote profiles need the app's connections.
pub struct LocalHost {
    archives: Arc<Archives>,
    sessions: Option<SessionStore>,
    formats: ConvertConfig,
}

impl LocalHost {
    /// `scratch` holds extracted archive entries (removed on drop).
    pub fn new(scratch: PathBuf) -> Self {
        Self {
            archives: Arc::new(Archives::new(scratch)),
            sessions: None,
            formats: ConvertConfig::default(),
        }
    }

    /// Looks sessions up in a sessions file.
    pub fn with_sessions(mut self, store: SessionStore) -> Self {
        self.sessions = Some(store);
        self
    }

    pub fn with_file_formats(mut self, formats: ConvertConfig) -> Self {
        self.formats = formats;
        self
    }

    /// A host with a fresh scratch folder in the system temp folder.
    pub fn temporary() -> Self {
        Self::new(std::env::temp_dir().join(format!(
            "pcompare-script-{}-{}",
            std::process::id(),
            unique()
        )))
    }
}

fn unique() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    N.fetch_add(1, Ordering::Relaxed)
}

impl Drop for LocalHost {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(self.archives.cache_dir());
    }
}

/// Beyond Compare's snapshot extension; PCompare reads its own snapshots
/// saved under that name (scripts written for BC name them `.bcss`).
pub const BC_SNAPSHOT_EXT: &str = "bcss";

pub fn is_snapshot_name(path: &str) -> bool {
    Path::new(path).extension().is_some_and(|e| {
        e.eq_ignore_ascii_case(BC_SNAPSHOT_EXT)
            || e.eq_ignore_ascii_case(pcompare_archive::SNAPSHOT_EXT)
    })
}

/// Opens `path` as a side through `archives` (archives, snapshots and
/// repository paths), or as a local folder. Shared by [`LocalHost`] and
/// hosts that add remote folders.
pub fn container_or_local(archives: &Archives, path: &str) -> Result<Side, String> {
    let p = Path::new(path);
    if p.is_dir() {
        return Ok(Side::local(p));
    }
    if p.is_file() && is_snapshot_name(path) {
        let fs = archives.open_snapshot(p, path).map_err(|e| {
            format!("{path}: {e} (only snapshots saved by PCompare can be read)")
        })?;
        let crcs = fs.info().include_crcs;
        return Ok(Side {
            shown: path.to_owned(),
            fs: fs.clone(),
            path: String::new(),
            local: None,
            write: Some((fs.clone(), String::new())),
            crcs: Some(fs),
            snapshot: Some(crcs),
        });
    }
    match archives.resolve(path, true).map_err(|e| e.to_string())? {
        Some(r) => Ok(Side {
            shown: path.to_owned(),
            fs: r.fs.clone(),
            path: r.path.clone(),
            local: None,
            write: Some((r.fs, r.path)),
            crcs: r.crcs,
            snapshot: r.snapshot.map(|s| s.include_crcs),
        }),
        None => Err(format!("{path}: no such folder")),
    }
}

/// [`Host::kind`] for local paths: archive and snapshot files open as
/// folders.
pub fn local_kind(path: &str) -> PathKind {
    match pcompare_core::path_kind(Path::new(path)) {
        PathKind::File
            if is_snapshot_name(path)
                || pcompare_archive::ArchiveKind::from_name(path, false).is_some() =>
        {
            PathKind::Directory
        }
        PathKind::Missing if pcompare_archive::is_repo_path(path) => PathKind::Directory,
        PathKind::Missing
            if pcompare_archive::may_be_container_path(path)
                && pcompare_archive::split_path(path, true, |p| Path::new(p).is_file())
                    .is_some() =>
        {
            PathKind::Directory
        }
        k => k,
    }
}

/// Finds a session by name in `store`.
pub fn find_session(store: &SessionStore, name: &str) -> Option<SavedSession> {
    store
        .list()
        .ok()?
        .into_iter()
        .find(|s| s.name.eq_ignore_ascii_case(name.trim()))
}

impl Host for LocalHost {
    fn kind(&self, path: &str) -> PathKind {
        local_kind(path)
    }

    fn folder(&self, path: &str) -> Result<Side, String> {
        if path.contains("://") {
            return Err(format!(
                "{path}: remote folders need a saved connection; run the script with the pcompare app"
            ));
        }
        container_or_local(&self.archives, path)
    }

    fn create_folder(&self, path: &str) -> Result<(), String> {
        std::fs::create_dir_all(path).map_err(|e| format!("{path}: {e}"))
    }

    fn local_file(&self, path: &str) -> Result<PathBuf, String> {
        if Path::new(path).is_file() {
            return Ok(PathBuf::from(path));
        }
        match self.archives.local_copy(path) {
            Ok(Some(p)) => Ok(p),
            Ok(None) => Err(format!("{path}: no such file")),
            Err(e) => Err(e.to_string()),
        }
    }

    fn session(&self, name: &str) -> Option<SavedSession> {
        find_session(self.sessions.as_ref()?, name)
    }

    fn file_formats(&self) -> ConvertConfig {
        self.formats.clone()
    }

    fn archives(&self) -> &Arc<Archives> {
        &self.archives
    }
}
