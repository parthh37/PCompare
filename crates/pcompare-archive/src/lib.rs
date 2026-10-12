//! Archives, folder snapshots and Subversion repositories as PCompare file
//! systems ([`Vfs`]), so Folder Compare, Folder Sync and the viewers work on
//! them like on any folder.
//!
//! Paths go *through* containers with the platform's usual syntax:
//!
//! - `C:\x\a.zip\dir\file.txt`, `/x/a.tar.gz/dir`, `/x/a.zip/inner.7z/f` —
//!   zip (also jar, war, apk; Office / OpenDocument packages only when a path
//!   names them explicitly), tar (plain, gzip, bzip2, xz), a lone `.gz` /
//!   `.bz2` / `.xz` file, and 7z. Entries are streamed out of the archive,
//!   never extracted to a temp folder (compressed tars are decompressed once
//!   into a cache file so entries can be read in any order). Zip archives
//!   can be written to; the others are read-only.
//! - `/x/home.pcsnap` — a [snapshot](snapshot): names, sizes, dates,
//!   attributes and optional CRCs of a folder tree, without contents.
//! - `svn:https://host/repo/trunk@123` — a Subversion repository through
//!   the `svn` command-line client ([`svn`]).
//! - `git:/src/app@HEAD~1`, `hg:/src/app@tip` — a revision of a Git or
//!   Mercurial working tree through the `git` / `hg` clients ([`vcs`]).
//!
//! [`Archives`] turns such a path into a file system plus the path inside
//! it, caches what it opened, and extracts single entries for the viewers.

pub mod archive;
pub mod compare;
pub mod expand;
mod formats;
pub mod snapshot;
pub mod svn;
mod tree;
pub mod vcs; // araxis:vcs

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pcompare_core::vfs::Vfs;
use pcompare_core::vfs_write::VfsWrite;
use pcompare_core::{CoreError, Result};
use serde::Serialize;

pub use archive::ArchiveFs;
pub use expand::ExpandFs;
pub use snapshot::SnapshotFs;
pub use svn::SvnFs;
pub use vcs::VcsFs; // araxis:vcs

/// The archive formats PCompare reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ArchiveKind {
    Zip,
    Tar,
    TarGz,
    TarBz2,
    TarXz,
    /// One gzip-compressed file.
    Gz,
    Bz2,
    Xz,
    SevenZ,
}

/// Archive file name suffixes, longest first so `.tar.gz` wins over `.gz`.
const SUFFIXES: &[(&str, ArchiveKind)] = &[
    (".tar.gz", ArchiveKind::TarGz),
    (".tar.bz2", ArchiveKind::TarBz2),
    (".tar.xz", ArchiveKind::TarXz),
    (".tgz", ArchiveKind::TarGz),
    (".taz", ArchiveKind::TarGz),
    (".tbz2", ArchiveKind::TarBz2),
    (".tbz", ArchiveKind::TarBz2),
    (".txz", ArchiveKind::TarXz),
    (".tar", ArchiveKind::Tar),
    (".gz", ArchiveKind::Gz),
    (".bz2", ArchiveKind::Bz2),
    (".xz", ArchiveKind::Xz),
    (".7z", ArchiveKind::SevenZ),
    (".zip", ArchiveKind::Zip),
    (".jar", ArchiveKind::Zip),
    (".war", ArchiveKind::Zip),
    (".ear", ArchiveKind::Zip),
    (".apk", ArchiveKind::Zip),
];

/// Zip-based documents and packages: opened as folders only when a path
/// names them explicitly (`report.docx\word\document.xml`, or chosen as a
/// Folder Compare side), never expanded on their own inside a tree.
const EXPLICIT_ZIPS: &[&str] = &[
    "docx", "docm", "dotx", "xlsx", "xlsm", "xltx", "pptx", "pptm", "potx", "odt", "ods", "odp",
    "odg", "epub", "nupkg", "vsix", "xpi", "ipa", "aar", "whl", "crx", "appx", "msix",
];

/// Snapshot files written by [`snapshot::save`].
pub const SNAPSHOT_EXT: &str = "pcsnap";

/// The `svn:` path prefix.
pub const SVN_PREFIX: &str = "svn:";

impl ArchiveKind {
    /// The format a file name says it is. `explicit`: the user named this
    /// file as a folder, so zip-based documents count too.
    pub fn from_name(name: &str, explicit: bool) -> Option<Self> {
        let lower = name.to_ascii_lowercase();
        for (suffix, kind) in SUFFIXES {
            if lower.len() > suffix.len() && lower.ends_with(suffix) {
                return Some(*kind);
            }
        }
        let ext = lower
            .rsplit_once('.')
            .map(|(stem, ext)| (!stem.is_empty()).then_some(ext))??;
        (explicit && EXPLICIT_ZIPS.contains(&ext)).then_some(ArchiveKind::Zip)
    }

    pub fn label(self) -> &'static str {
        match self {
            ArchiveKind::Zip => "zip",
            ArchiveKind::Tar => "tar",
            ArchiveKind::TarGz => "tar.gz",
            ArchiveKind::TarBz2 => "tar.bz2",
            ArchiveKind::TarXz => "tar.xz",
            ArchiveKind::Gz => "gzip",
            ArchiveKind::Bz2 => "bzip2",
            ArchiveKind::Xz => "xz",
            ArchiveKind::SevenZ => "7z",
        }
    }

    /// Only zip archives can be modified.
    pub fn writable(self) -> bool {
        self == ArchiveKind::Zip
    }
}

/// What a container path is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase", tag = "type", content = "format")]
pub enum ContainerKind {
    Archive(ArchiveKind),
    Snapshot,
    Svn,
    // araxis:vcs — `git:` / `hg:` revisions
    Git,
    Hg,
    // araxis:vcs end
}

/// Checksums a container stores for its files (zip and 7z headers,
/// snapshots), so a CRC comparison needn't read them.
pub trait CrcSource: Send + Sync {
    fn stored_crc(&self, path: &str) -> Option<u32>;
    /// Whether `path` belongs to the container (its checksums come from
    /// here, never from a cache of plain files).
    fn covers(&self, _path: &str) -> bool {
        true
    }
}

/// A path resolved into a container.
#[derive(Clone)]
pub struct Resolved {
    pub fs: Arc<dyn VfsWrite>,
    /// The path inside `fs` (`""` for the container's root).
    pub path: String,
    pub crcs: Option<Arc<dyn CrcSource>>,
    pub kind: ContainerKind,
    /// Why writes are refused, for read-only containers.
    pub read_only: Option<String>,
    /// Set for snapshots.
    pub snapshot: Option<snapshot::SnapshotInfo>,
}

/// One container file found in a path: its location, its kind, and the
/// components after it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Split<'a> {
    pub file: &'a str,
    pub kind: SplitKind,
    pub rest: Vec<&'a str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitKind {
    Archive(ArchiveKind),
    Snapshot,
}

fn split_kind(name: &str) -> Option<SplitKind> {
    if name
        .rsplit_once('.')
        .is_some_and(|(stem, ext)| !stem.is_empty() && ext.eq_ignore_ascii_case(SNAPSHOT_EXT))
    {
        return Some(SplitKind::Snapshot);
    }
    ArchiveKind::from_name(name, true).map(SplitKind::Archive)
}

/// Finds the first component of `path` that names a container, where
/// `is_file` confirms it is a file (not a folder that happens to be called
/// `x.zip`). Without components after it, the container itself only counts
/// when `as_folder` (it is being opened as a folder) or the path ends with a
/// separator.
pub fn split_path<'a>(
    path: &'a str,
    as_folder: bool,
    is_file: impl Fn(&str) -> bool,
) -> Option<Split<'a>> {
    let bytes = path.as_bytes();
    let mut start = 0;
    for (i, _) in path.char_indices().chain([(path.len(), ' ')]) {
        let at_sep = i == path.len() || bytes[i] == b'/' || bytes[i] == b'\\';
        if !at_sep {
            continue;
        }
        let name = &path[start..i];
        start = i + 1;
        let Some(kind) = split_kind(name) else {
            continue;
        };
        let rest: Vec<&str> = path
            .get(i + 1..)
            .unwrap_or("")
            .split(['/', '\\'])
            .filter(|p| !p.is_empty())
            .collect();
        let trailing = i < path.len();
        if rest.is_empty() && !trailing && !as_folder {
            return None;
        }
        let file = &path[..i];
        if is_file(file) {
            return Some(Split { file, kind, rest });
        }
    }
    None
}

/// Cheap syntactic test: could this path lead into an archive, snapshot or
/// repository? Used to skip the special handling for ordinary paths.
pub fn may_be_container_path(path: &str) -> bool {
    is_repo_path(path) || path.split(['/', '\\']).any(|c| split_kind(c).is_some())
}

pub fn is_svn(path: &str) -> bool {
    path.get(..SVN_PREFIX.len())
        .is_some_and(|p| p.eq_ignore_ascii_case(SVN_PREFIX))
}

// araxis:vcs
/// `svn:`, `git:` and `hg:` paths: repositories reached through a client.
pub fn is_repo_path(path: &str) -> bool {
    is_svn(path) || vcs::is_vcs(path)
}
// araxis:vcs end

/// Writes `src` to `dest` through a temp file in the same folder, so a
/// crash or a concurrent reader never sees half a file.
pub(crate) fn write_atomic(dest: &Path, src: &mut dyn Read) -> io::Result<u64> {
    let dir = dest.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;
    let mut tmp = tempfile::Builder::new()
        .prefix(".pcompare-")
        .tempfile_in(dir)?;
    let n = io::copy(src, &mut tmp)?;
    tmp.as_file_mut().flush()?;
    tmp.persist(dest).map_err(|e| e.error)?;
    Ok(n)
}

pub(crate) fn hash_key(parts: impl Hash) -> String {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    parts.hash(&mut h);
    format!("{:016x}", h.finish())
}

/// How long a Subversion listing of `HEAD` is reused: long enough for one
/// comparison (and the file opens after it), short enough that a refresh
/// sees new commits.
const SVN_HEAD_TTL: Duration = Duration::from_secs(15);

/// Opens and caches containers. One per app; cheap to share behind an `Arc`.
pub struct Archives {
    cache_dir: PathBuf,
    /// Keyed by file, label and whether opened read-only.
    archives: Mutex<HashMap<(PathBuf, String, bool), Arc<ArchiveFs>>>,
    snapshots: Mutex<HashMap<PathBuf, (snapshot::Stamp, Arc<SnapshotFs>)>>,
    repos: Mutex<HashMap<String, (Instant, Arc<SvnFs>)>>,
    // araxis:vcs — git/hg trees by location@rev, and refs with slashes seen per location
    vcs: Mutex<HashMap<String, (Instant, Arc<VcsFs>)>>,
    vcs_refs: Mutex<HashMap<(vcs::VcsKind, String), Vec<String>>>,
    // araxis:vcs end
}

impl Archives {
    /// `cache_dir` holds decompressed tars, extracted entries and copies of
    /// archives found inside other archives; it may be wiped between runs.
    pub fn new(cache_dir: PathBuf) -> Self {
        Self {
            cache_dir,
            archives: Mutex::new(HashMap::new()),
            snapshots: Mutex::new(HashMap::new()),
            repos: Mutex::new(HashMap::new()),
            vcs: Mutex::new(HashMap::new()),      // araxis:vcs
            vcs_refs: Mutex::new(HashMap::new()), // araxis:vcs
        }
    }

    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }

    /// The archive in a local file, reused while the file is unchanged.
    pub fn open_archive(
        &self,
        file: &Path,
        label: &str,
        kind: ArchiveKind,
        read_only: Option<String>,
    ) -> Result<Arc<ArchiveFs>> {
        let key = (file.to_owned(), label.to_owned(), read_only.is_some());
        let mut map = self.archives.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(fs) = map.get(&key).filter(|fs| fs.kind() == kind) {
            return Ok(fs.clone());
        }
        let scratch = self.cache_dir.join(hash_key(&key));
        let fs = Arc::new(ArchiveFs::new(
            file.to_owned(),
            label.to_owned(),
            kind,
            scratch,
            read_only,
        ));
        map.insert(key, fs.clone());
        Ok(fs)
    }

    /// A snapshot file, reloaded when it changes.
    pub fn open_snapshot(&self, file: &Path, label: &str) -> Result<Arc<SnapshotFs>> {
        let stamp = snapshot::Stamp::of(file)?;
        let mut map = self.snapshots.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((s, fs)) = map.get(file) {
            if *s == stamp {
                return Ok(fs.clone());
            }
        }
        let fs = Arc::new(SnapshotFs::new(snapshot::load(file)?, label.to_owned()));
        map.insert(file.to_owned(), (stamp, fs.clone()));
        Ok(fs)
    }

    /// The repository behind an `svn:` path (listing cached; `HEAD` briefly).
    pub fn open_svn(&self, root: &svn::SvnPath) -> Arc<SvnFs> {
        let key = format!("{}@{}", root.url, root.rev);
        let mut map = self.repos.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((at, fs)) = map.get(&key) {
            if root.pinned() || at.elapsed() < SVN_HEAD_TTL {
                return fs.clone();
            }
        }
        let fs = Arc::new(SvnFs::new(root.clone()));
        map.insert(key, (Instant::now(), fs.clone()));
        fs
    }

    // araxis:vcs
    /// The tree behind a `git:` / `hg:` path (listing cached; moving
    /// revisions such as `HEAD` briefly).
    pub fn open_vcs(&self, root: &vcs::VcsPath) -> Arc<VcsFs> {
        let key = format!("{}{}@{}", root.kind.prefix(), root.location, root.rev);
        let mut map = self.vcs.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((at, fs)) = map.get(&key) {
            if root.pinned() || at.elapsed() < SVN_HEAD_TTL {
                return fs.clone();
            }
        }
        let fs = Arc::new(VcsFs::new(root.clone()));
        map.insert(key, (Instant::now(), fs.clone()));
        fs
    }

    /// Splits a `git:` / `hg:` path into revision and inner path, asking the
    /// client only for refs that may contain slashes (remembered per working
    /// tree, so the files below a compared root resolve without it).
    pub fn resolve_vcs_path(&self, path: &str) -> Result<vcs::VcsPath> {
        let spec = vcs::VcsSpec::parse(path)?;
        let key = (spec.kind, spec.location.clone());
        let known = self
            .vcs_refs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
            .cloned()
            .unwrap_or_default();
        let (rev, inner) = vcs::split_rev(spec.kind, &spec.tail, &known, |c| {
            vcs::rev_exists(spec.kind, &spec.location, c)
        });
        if rev.contains(['/', '\\']) && !known.contains(&rev) {
            self.vcs_refs
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .entry(key)
                .or_default()
                .push(rev.clone());
        }
        let inner = tree::normalize(&inner)
            .ok_or_else(|| CoreError::Other(format!("`{path}` climbs out of the repository")))?;
        Ok(vcs::VcsPath {
            kind: spec.kind,
            location: spec.location,
            rev,
            inner,
        })
    }
    // araxis:vcs end

    /// Resolves a local path into an archive or snapshot, or an `svn:`
    /// path. `None` for ordinary paths (and for a bare archive file unless
    /// `as_folder`, so archives still compare as files where a file is
    /// expected).
    pub fn resolve(&self, path: &str, as_folder: bool) -> Result<Option<Resolved>> {
        if is_svn(path) {
            let p = svn::SvnPath::parse(path)?;
            let fs = self.open_svn(&p);
            return Ok(Some(Resolved {
                fs,
                path: p.inner.clone(),
                crcs: None,
                kind: ContainerKind::Svn,
                read_only: Some(svn::READ_ONLY.into()),
                snapshot: None,
            }));
        }
        // araxis:vcs
        if vcs::is_vcs(path) {
            let p = self.resolve_vcs_path(path)?;
            return Ok(Some(Resolved {
                fs: self.open_vcs(&p),
                path: p.inner.clone(),
                crcs: None,
                kind: match p.kind {
                    vcs::VcsKind::Git => ContainerKind::Git,
                    vcs::VcsKind::Hg => ContainerKind::Hg,
                },
                read_only: Some(p.kind.read_only().into()),
                snapshot: None,
            }));
        }
        // araxis:vcs end
        if !may_be_container_path(path) {
            return Ok(None);
        }
        let Some(split) = split_path(path, as_folder, |p| Path::new(p).is_file()) else {
            return Ok(None);
        };
        self.resolve_split(
            Path::new(split.file),
            split.file,
            split.kind,
            &split.rest,
            as_folder,
            None,
        )
        .map(Some)
    }

    /// Resolves the components `rest` inside a container file `file` (shown
    /// as `label`). Archives found inside archives are copied out to the
    /// cache and opened in turn; they are read-only, as is everything when
    /// `read_only` gives a reason.
    pub fn resolve_split(
        &self,
        file: &Path,
        label: &str,
        kind: SplitKind,
        rest: &[&str],
        as_folder: bool,
        read_only: Option<String>,
    ) -> Result<Resolved> {
        let kind = match kind {
            SplitKind::Snapshot => {
                let fs = self.open_snapshot(file, label)?;
                return Ok(Resolved {
                    fs: fs.clone(),
                    path: rest.join("/"),
                    snapshot: Some(fs.info().clone()),
                    crcs: Some(fs),
                    kind: ContainerKind::Snapshot,
                    read_only: Some(snapshot::READ_ONLY.into()),
                });
            }
            SplitKind::Archive(k) => k,
        };
        let mut fs = self.open_archive(file, label, kind, read_only.clone())?;
        let mut kind = kind;
        let mut start = 0;
        // Descend through archives nested in this one.
        for i in 0..rest.len() {
            let Some(inner_kind) = ArchiveKind::from_name(rest[i], true) else {
                continue;
            };
            let last = i + 1 == rest.len();
            if last && !as_folder {
                break;
            }
            let rel = rest[start..=i].join("/");
            if !fs.is_file(&rel)? {
                continue;
            }
            let copy = self.materialize(&*fs, fs.label(), &rel)?;
            let nested_label = format!("{}/{rel}", fs.label().trim_end_matches(['/', '\\']));
            let why = read_only
                .clone()
                .unwrap_or_else(|| "archives inside other archives are read-only".into());
            fs = self.open_archive(&copy, &nested_label, inner_kind, Some(why))?;
            kind = inner_kind;
            start = i + 1;
        }
        Ok(Resolved {
            fs: fs.clone(),
            path: rest[start..].join("/"),
            crcs: Some(fs.clone()),
            kind: ContainerKind::Archive(kind),
            read_only: fs.read_only_reason(),
            snapshot: None,
        })
    }

    /// Copies one file out of a container (shown as `label`) into the cache
    /// (reused while the entry is unchanged) and returns the copy.
    pub fn materialize(&self, fs: &dyn VfsWrite, label: &str, path: &str) -> Result<PathBuf> {
        let meta = fs
            .stat(path)
            .map_err(|e| CoreError::io(path, e))?
            .ok_or_else(|| CoreError::io(path, io::ErrorKind::NotFound.into()))?;
        let name = path
            .rsplit(['/', '\\'])
            .next()
            .filter(|n| !n.is_empty())
            .unwrap_or("file");
        let key = hash_key((label, path, meta.size, meta.modified_ms));
        self.copy_into_cache(&key, name, || fs.open(path))
    }

    /// Copies what `open` reads into `cache/extract/<key>/<name>`, unless
    /// that is already there.
    pub fn copy_into_cache(
        &self,
        key: &str,
        name: &str,
        open: impl FnOnce() -> Result<Box<dyn Read + Send>>,
    ) -> Result<PathBuf> {
        let dest = self.cache_dir.join("extract").join(key).join(name);
        if dest.is_file() {
            return Ok(dest);
        }
        write_atomic(&dest, &mut *open()?).map_err(|e| CoreError::io(&dest, e))?;
        Ok(dest)
    }

    /// For the viewers: a local file with the contents of a file inside a
    /// container, `None` for ordinary paths. Each version of an entry is
    /// extracted once.
    pub fn local_copy(&self, path: &str) -> Result<Option<PathBuf>> {
        match self.resolve(path, false)? {
            Some(r) => self.extract(&r, path).map(Some),
            None => Ok(None),
        }
    }

    /// Extracts the file a [`Resolved`] points at; `shown` names it in errors.
    pub fn extract(&self, r: &Resolved, shown: &str) -> Result<PathBuf> {
        let meta =
            r.fs.stat(&r.path)
                .map_err(|e| CoreError::io(shown, e))?
                .ok_or_else(|| CoreError::io(shown, io::ErrorKind::NotFound.into()))?;
        if meta.is_dir {
            return Err(CoreError::Other(format!("{shown} is a folder")));
        }
        let name = shown
            .rsplit(['/', '\\'])
            .next()
            .filter(|n| !n.is_empty())
            .unwrap_or("file");
        let key = hash_key((
            shown,
            meta.size,
            meta.modified_ms,
            r.crcs.as_ref().and_then(|c| c.stored_crc(&r.path)),
        ));
        self.copy_into_cache(&key, name, || r.fs.open(&r.path))
    }

    /// Wraps a file system so archives inside it list as folders. `local`:
    /// `inner` is the local disk; `inner_crcs`: checksums `inner` itself
    /// stores (when it is an archive).
    pub fn expand(
        self: &Arc<Self>,
        inner: Arc<dyn Vfs>,
        local: bool,
        inner_crcs: Option<Arc<dyn CrcSource>>,
    ) -> ExpandFs {
        ExpandFs::new(inner, local, inner_crcs, self.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_from_names() {
        assert_eq!(
            ArchiveKind::from_name("a.tar.gz", false),
            Some(ArchiveKind::TarGz)
        );
        assert_eq!(
            ArchiveKind::from_name("A.TGZ", false),
            Some(ArchiveKind::TarGz)
        );
        assert_eq!(
            ArchiveKind::from_name("log.txt.gz", false),
            Some(ArchiveKind::Gz)
        );
        assert_eq!(
            ArchiveKind::from_name("lib.jar", false),
            Some(ArchiveKind::Zip)
        );
        assert_eq!(
            ArchiveKind::from_name("x.7z", false),
            Some(ArchiveKind::SevenZ)
        );
        assert_eq!(ArchiveKind::from_name("report.docx", false), None);
        assert_eq!(
            ArchiveKind::from_name("report.docx", true),
            Some(ArchiveKind::Zip)
        );
        assert_eq!(ArchiveKind::from_name(".zip", false), None);
        assert_eq!(ArchiveKind::from_name("zip", true), None);
    }

    #[test]
    fn splits_paths_at_the_container() {
        let files = ["/x/a.zip", r"C:\x\b.tar.gz", "/x/s.pcsnap"];
        let is_file = |p: &str| files.contains(&p);
        let s = split_path("/x/a.zip/dir/f.txt", false, is_file).unwrap();
        assert_eq!(
            (s.file, s.kind, s.rest),
            (
                "/x/a.zip",
                SplitKind::Archive(ArchiveKind::Zip),
                vec!["dir", "f.txt"]
            )
        );
        let s = split_path(r"C:\x\b.tar.gz\d\e", false, is_file).unwrap();
        assert_eq!((s.file, s.rest), (r"C:\x\b.tar.gz", vec!["d", "e"]));
        // The bare archive is a file unless opened as a folder (or with a trailing separator).
        assert!(split_path("/x/a.zip", false, is_file).is_none());
        assert_eq!(
            split_path("/x/a.zip", true, is_file).unwrap().rest,
            Vec::<&str>::new()
        );
        assert!(split_path("/x/a.zip/", false, is_file).is_some());
        assert_eq!(
            split_path("/x/s.pcsnap/q", false, is_file).unwrap().kind,
            SplitKind::Snapshot
        );
        // A folder named like an archive is just a folder.
        assert!(split_path("/y/a.zip/f", false, is_file).is_none());
        assert!(!may_be_container_path("/home/me/notes.txt"));
        assert!(may_be_container_path("svn:https://h/r"));
    }
}
