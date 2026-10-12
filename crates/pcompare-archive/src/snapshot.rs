//! Folder snapshots: a `.pcsnap` file records a folder tree's names, sizes,
//! dates, attributes and (optionally) CRC-32s — but no contents — so a
//! folder can later be compared against how it looked, like Beyond
//! Compare's `.bcss` snapshots.
//!
//! The file is gzip-compressed JSON. Opened as a Folder Compare side it is a
//! read-only [`Vfs`]: listings work as usual, content comparisons use the
//! stored CRCs ([`CrcSource`]) and reading a file reports that its content
//! isn't available.

use std::fs;
use std::io::{self, BufReader, Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use pcompare_core::folder::EntryMeta;
use pcompare_core::vfs::{ListOptions, Vfs, VfsEntry};
use pcompare_core::vfs_write::{VfsWrite, WriteCaps};
use pcompare_core::{CoreError, Result};
use serde::{Deserialize, Serialize};

use crate::tree::{self, shown, Loc, Tree};
use crate::CrcSource;

/// The `format` field every snapshot file carries.
pub const FORMAT: &str = "pcompare-snapshot";
pub const VERSION: u32 = 1;
pub const READ_ONLY: &str = "snapshots are read-only";

/// Starts the error of reading a file in a snapshot.
pub const CONTENT_UNAVAILABLE: &str = "content unavailable";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub format: String,
    pub version: u32,
    /// The folder the snapshot was taken of, as the user named it.
    pub source: String,
    pub created_ms: i64,
    pub include_crcs: bool,
    pub entries: Vec<SnapEntry>,
}

fn is_false(b: &bool) -> bool {
    !*b
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

/// One file or folder. Defaults are left out of the file to keep it small.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SnapEntry {
    pub name: String,
    #[serde(skip_serializing_if = "is_false")]
    pub dir: bool,
    #[serde(skip_serializing_if = "is_zero")]
    pub size: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modified_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attributes: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub link_target: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub crc: Option<u32>,
    #[serde(skip_serializing_if = "is_false")]
    pub hidden: bool,
    /// Why a folder's contents (or a file's CRC) couldn't be recorded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<SnapEntry>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SnapshotOptions {
    /// Read every file to record its CRC-32 (slower, but lets content
    /// comparisons against the snapshot tell changed files apart).
    pub include_crcs: bool,
}

/// What a snapshot holds; also reported as progress while taking one.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotSummary {
    pub files: usize,
    pub folders: usize,
    pub bytes: u64,
    /// Folders that couldn't be listed and files whose CRC couldn't be read.
    pub errors: usize,
}

/// Returned by [`take`] when `cancel` was set.
pub const CANCELLED: &str = "Snapshot cancelled";

struct Walk<'a> {
    fs: &'a dyn Vfs,
    opts: SnapshotOptions,
    sum: SnapshotSummary,
    progress: Option<&'a dyn Fn(&SnapshotSummary)>,
    cancel: Option<&'a AtomicBool>,
}

impl Walk<'_> {
    fn dir(&mut self, path: &str) -> Result<Vec<SnapEntry>> {
        if self.cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
            return Err(CoreError::Other(CANCELLED.into()));
        }
        // Links are recorded as links, which also keeps loops out.
        let mut list = self.fs.read_dir_with(
            path,
            ListOptions {
                follow_links: false,
            },
        )?;
        list.sort_by(|a, b| a.name.cmp(&b.name));
        let mut out = Vec::with_capacity(list.len());
        for VfsEntry { name, meta, hidden } in list {
            let child = self.fs.join(path, &name);
            let mut e = SnapEntry {
                name,
                dir: meta.is_dir,
                size: meta.size,
                modified_ms: meta.modified_ms,
                attributes: meta.attributes,
                mode: meta.mode,
                link_target: meta.link_target.clone(),
                hidden,
                ..Default::default()
            };
            if meta.is_dir {
                self.sum.folders += 1;
                match self.dir(&child) {
                    Ok(kids) => e.children = kids,
                    Err(CoreError::Other(m)) if m == CANCELLED => return Err(CoreError::Other(m)),
                    Err(err) => {
                        self.sum.errors += 1;
                        e.error = Some(err.to_string());
                    }
                }
            } else {
                self.sum.files += 1;
                self.sum.bytes += meta.size;
                if self.opts.include_crcs && meta.link_target.is_none() {
                    match crc32(self.fs, &child) {
                        Ok(c) => e.crc = Some(c),
                        Err(err) => {
                            self.sum.errors += 1;
                            e.error = Some(err.to_string());
                        }
                    }
                }
            }
            out.push(e);
        }
        if let Some(p) = self.progress {
            p(&self.sum);
        }
        Ok(out)
    }
}

/// CRC-32 of a file, read in full.
pub fn crc32(fs: &dyn Vfs, path: &str) -> Result<u32> {
    let mut r = fs.open(path)?;
    let mut h = crc32fast::Hasher::new();
    let mut buf = vec![0u8; 128 * 1024];
    loop {
        match r.read(&mut buf) {
            Ok(0) => return Ok(h.finalize()),
            Ok(n) => h.update(&buf[..n]),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(CoreError::io(path, e)),
        }
    }
}

/// Records the tree under `root` of `fs`. `source` is how the folder is
/// shown (stored in the snapshot). Unreadable subfolders are recorded with
/// their error rather than failing the whole snapshot.
pub fn take(
    fs: &dyn Vfs,
    root: &str,
    source: &str,
    opts: SnapshotOptions,
    progress: Option<&dyn Fn(&SnapshotSummary)>,
    cancel: Option<&AtomicBool>,
) -> Result<(Snapshot, SnapshotSummary)> {
    let mut walk = Walk {
        fs,
        opts,
        sum: SnapshotSummary::default(),
        progress,
        cancel,
    };
    let entries = walk.dir(root)?;
    let created_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64);
    let snap = Snapshot {
        format: FORMAT.into(),
        version: VERSION,
        source: source.to_owned(),
        created_ms,
        include_crcs: opts.include_crcs,
        entries,
    };
    Ok((snap, walk.sum))
}

/// Writes a snapshot as gzip-compressed JSON (atomically).
pub fn save(snap: &Snapshot, out: &Path) -> Result<()> {
    let json = serde_json::to_vec(snap).map_err(|e| CoreError::Other(e.to_string()))?;
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gz.write_all(&json).map_err(|e| CoreError::io(out, e))?;
    let bytes = gz.finish().map_err(|e| CoreError::io(out, e))?;
    crate::write_atomic(out, &mut bytes.as_slice()).map_err(|e| CoreError::io(out, e))?;
    Ok(())
}

/// Reads a snapshot file (gzip-compressed or plain JSON).
pub fn load(path: &Path) -> Result<Snapshot> {
    let f = fs::File::open(path).map_err(|e| CoreError::io(path, e))?;
    let mut r = BufReader::new(f);
    let gz = io::BufRead::fill_buf(&mut r)
        .map_err(|e| CoreError::io(path, e))?
        .starts_with(&[0x1f, 0x8b]);
    let bad = |e: serde_json::Error| {
        CoreError::Other(format!("{}: not a PCompare snapshot ({e})", path.display()))
    };
    let snap: Snapshot = if gz {
        serde_json::from_reader(BufReader::new(flate2::read::GzDecoder::new(r))).map_err(bad)?
    } else {
        serde_json::from_reader(r).map_err(bad)?
    };
    if snap.format != FORMAT {
        return Err(CoreError::Other(format!(
            "{}: not a PCompare snapshot",
            path.display()
        )));
    }
    if snap.version > VERSION {
        return Err(CoreError::Other(format!(
            "{}: snapshot format {} is newer than this version of PCompare understands",
            path.display(),
            snap.version
        )));
    }
    Ok(snap)
}

/// Totals of a loaded snapshot.
pub fn summarize(entries: &[SnapEntry]) -> SnapshotSummary {
    fn walk(list: &[SnapEntry], s: &mut SnapshotSummary) {
        for e in list {
            if e.error.is_some() {
                s.errors += 1;
            }
            if e.dir {
                s.folders += 1;
                walk(&e.children, s);
            } else {
                s.files += 1;
                s.bytes += e.size;
            }
        }
    }
    let mut s = SnapshotSummary::default();
    walk(entries, &mut s);
    s
}

/// Identity of a snapshot file on disk, to reload it when it changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Stamp(u64, Option<SystemTime>);

impl Stamp {
    pub fn of(path: &Path) -> Result<Self> {
        let m = fs::metadata(path).map_err(|e| CoreError::io(path, e))?;
        Ok(Stamp(m.len(), m.modified().ok()))
    }
}

/// Facts about an opened snapshot, for the UI.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotInfo {
    pub source: String,
    pub created_ms: i64,
    pub include_crcs: bool,
    pub summary: SnapshotSummary,
}

/// A snapshot as a read-only file system without contents.
pub struct SnapshotFs {
    label: String,
    info: SnapshotInfo,
    tree: Tree,
}

impl SnapshotFs {
    pub fn new(snap: Snapshot, label: String) -> Self {
        let mut tree = Tree::default();
        fn add(tree: &mut Tree, dir: &str, list: &[SnapEntry]) {
            for e in list {
                let path = tree::join(dir, &e.name);
                let meta = EntryMeta {
                    size: if e.dir { 0 } else { e.size },
                    modified_ms: e.modified_ms,
                    is_dir: e.dir,
                    attributes: e.attributes,
                    mode: e.mode,
                    link_target: e.link_target.clone(),
                };
                tree.insert(&path, meta, e.crc, Loc::None);
                if e.dir {
                    add(tree, &path, &e.children);
                }
            }
        }
        add(&mut tree, "", &snap.entries);
        let info = SnapshotInfo {
            summary: summarize(&snap.entries),
            source: snap.source,
            created_ms: snap.created_ms,
            include_crcs: snap.include_crcs,
        };
        Self { label, info, tree }
    }

    pub fn info(&self) -> &SnapshotInfo {
        &self.info
    }

    fn unavailable(&self, path: &str) -> CoreError {
        CoreError::Other(format!(
            "{}: {CONTENT_UNAVAILABLE} — a snapshot records file details, not contents",
            shown(&self.label, path)
        ))
    }
}

fn read_only() -> io::Error {
    io::Error::new(io::ErrorKind::Unsupported, READ_ONLY)
}

impl Vfs for SnapshotFs {
    fn read_dir(&self, dir: &str) -> Result<Vec<VfsEntry>> {
        self.tree.list(&self.label, dir)
    }

    fn open(&self, path: &str) -> Result<Box<dyn Read + Send>> {
        match self.tree.get(path) {
            None => Err(tree::not_found(&self.label, path)),
            Some(_) => Err(self.unavailable(path)),
        }
    }

    fn join(&self, dir: &str, name: &str) -> String {
        tree::join(dir, name)
    }
}

impl CrcSource for SnapshotFs {
    fn stored_crc(&self, path: &str) -> Option<u32> {
        self.tree.get(path).and_then(|n| n.crc)
    }
}

impl VfsWrite for SnapshotFs {
    fn caps(&self) -> WriteCaps {
        WriteCaps::default()
    }

    fn stat(&self, path: &str) -> io::Result<Option<EntryMeta>> {
        Ok(self.tree.get(path).map(|n| n.meta.clone()))
    }

    fn open_at(&self, path: &str, _offset: u64) -> io::Result<Box<dyn Read + Send>> {
        Err(io::Error::other(self.unavailable(path).to_string()))
    }

    fn write(
        &self,
        _path: &str,
        _offset: u64,
        _src: &mut dyn Read,
        _on_bytes: &mut dyn FnMut(u64) -> io::Result<()>,
    ) -> io::Result<u64> {
        Err(read_only())
    }

    fn create_dir_all(&self, _path: &str) -> io::Result<()> {
        Err(read_only())
    }

    fn remove_file(&self, _path: &str) -> io::Result<()> {
        Err(read_only())
    }

    fn remove_dir_all(&self, _path: &str) -> io::Result<()> {
        Err(read_only())
    }

    fn rename(&self, _from: &str, _to: &str) -> io::Result<()> {
        Err(read_only())
    }

    fn set_modified(&self, _path: &str, _modified_ms: i64) -> io::Result<()> {
        Err(read_only())
    }
}
