//! Archives, snapshots and Subversion repositories behind the same path
//! routing as remote profiles (bc5:archives): `C:\x\a.zip\dir\f.txt`,
//! `/x/home.pcsnap`, `svn:https://host/repo/trunk@12` and archives stored
//! on a remote (`remote://s3/backups/site.tar.gz/index.html`, downloaded
//! once per version and read-only) all resolve to a file system + path,
//! and to a local copy for the viewers.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use pcompare_archive::{split_path, Archives, Resolved};
use pcompare_core::vfs::Vfs;
use pcompare_core::vfs_write::VfsWrite;
use pcompare_core::Result;

use crate::{parse_remote, RemoteManager, SCHEME};

pub(crate) struct Containers {
    archives: Arc<Archives>,
    /// Size and date of archives downloaded from remotes, to fetch them
    /// again when they change.
    stamps: Mutex<HashMap<String, (u64, Option<i64>)>>,
}

impl Containers {
    pub fn new(cache_dir: PathBuf) -> Self {
        Self {
            archives: Arc::new(Archives::new(cache_dir)),
            stamps: Mutex::new(HashMap::new()),
        }
    }
}

const REMOTE_READ_ONLY: &str = "archives on remote profiles are read-only";

impl RemoteManager {
    /// The shared archive / snapshot / repository cache.
    pub fn archives(&self) -> &Arc<Archives> {
        &self.containers.archives
    }

    /// Resolves a path into an archive, snapshot or repository. `as_folder`:
    /// the path is being opened as a folder, so a bare archive file counts
    /// (elsewhere it stays a file that compares as a whole). `None` for
    /// every other path.
    pub fn resolve_container(&self, path: &str, as_folder: bool) -> Result<Option<Resolved>> {
        if !pcompare_archive::may_be_container_path(path) {
            return Ok(None);
        }
        let Some((id, rel)) = parse_remote(path) else {
            return self.containers.archives.resolve(path, as_folder);
        };
        let fs = self.fs_write(id)?;
        let stat = |p: &str| fs.stat(p).ok().flatten();
        let Some(split) = split_path(rel, as_folder, |p| stat(p).is_some_and(|m| !m.is_dir)) else {
            return Ok(None);
        };
        let remote = format!("{SCHEME}{id}/{}", split.file);
        let meta = stat(split.file).map(|m| (m.size, m.modified_ms));
        let refresh = {
            let mut stamps = self
                .containers
                .stamps
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let changed = meta.is_none() || stamps.get(&remote) != meta.as_ref();
            if let Some(m) = meta {
                stamps.insert(remote.clone(), m);
            }
            changed
        };
        let local = self.local_copy(&remote, refresh)?;
        self.containers
            .archives
            .resolve_split(
                &local,
                &remote,
                split.kind,
                &split.rest,
                as_folder,
                Some(REMOTE_READ_ONLY.into()),
            )
            .map(Some)
    }

    /// [`target`](Self::target) for a path opened as a folder: an archive or
    /// snapshot file lists its contents.
    pub fn target_folder(&self, path: &str) -> Result<(Arc<dyn Vfs>, String)> {
        match self.resolve_container(path, true)? {
            Some(r) => Ok((r.fs, r.path)),
            None => self.target(path),
        }
    }

    /// [`target_write`](Self::target_write) for a folder (a Folder Sync side).
    pub fn target_write_folder(&self, path: &str) -> Result<Option<(Arc<dyn VfsWrite>, String)>> {
        match self.resolve_container(path, true)? {
            Some(r) => Ok(Some((r.fs, r.path))),
            None => self.target_write(path),
        }
    }

    /// A local copy of a file inside a container, for the viewers.
    pub(crate) fn container_copy(&self, path: &str) -> Result<Option<PathBuf>> {
        match self.resolve_container(path, false)? {
            Some(r) => self.containers.archives.extract(&r, path).map(Some),
            None => Ok(None),
        }
    }
}

/// Whether a local path is (or is inside) an archive or snapshot file, or an
/// `svn:` path — paths [`crate::is_remote`] doesn't cover but that aren't
/// plain files either.
pub fn is_container_path(path: &str) -> bool {
    pcompare_archive::is_repo_path(path) // araxis:vcs — svn:, git: and hg:
        || (pcompare_archive::may_be_container_path(path)
            && split_path(path, true, |p| Path::new(p).is_file()).is_some())
}

/// Whether a path points *into* an archive, snapshot or repository (not at
/// a bare archive file). Such files are read-only in the viewers: they open
/// from an extracted copy.
pub fn is_inside_container(path: &str) -> bool {
    pcompare_archive::is_repo_path(path) // araxis:vcs — svn:, git: and hg:
        || (pcompare_archive::may_be_container_path(path)
            && split_path(path, false, |p| Path::new(p).is_file()).is_some())
}

/// The error for saving a file inside a container.
pub fn read_only_inside(path: &str) -> String {
    format!("{path}: files inside archives, snapshots and repositories are read-only here")
}
