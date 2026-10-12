//! A minimal file-system abstraction so the folder engine can walk local
//! disks and remote stores (S3, SFTP, OneDrive, ...) with the same code.
//!
//! Paths are plain strings in the file system's own syntax; only the
//! implementation knows how to join and interpret them. Methods are blocking
//! because the folder engine runs on the rayon pool — remote implementations
//! drive their own async runtime internally.

use crate::folder::{attr, EntryMeta};
use crate::{CoreError, Result};
use std::fs::{self, Metadata};
use std::io::Read;
use std::path::Path;
use std::time::UNIX_EPOCH;

/// One child of a listed directory.
#[derive(Debug, Clone)]
pub struct VfsEntry {
    pub name: String,
    pub meta: EntryMeta,
    pub hidden: bool,
}

pub trait Vfs: Send + Sync {
    /// Lists the direct children of `dir` (never `.` or `..`).
    fn read_dir(&self, dir: &str) -> Result<Vec<VfsEntry>>;
    /// Opens a file for sequential reading.
    fn open(&self, path: &str) -> Result<Box<dyn Read + Send>>;
    /// Appends a child name to a directory path.
    fn join(&self, dir: &str, name: &str) -> String;
    /// [`read_dir`](Vfs::read_dir) with listing options. File systems
    /// without links ignore `follow_links` and list as usual.
    fn read_dir_with(&self, dir: &str, _opts: ListOptions) -> Result<Vec<VfsEntry>> {
        self.read_dir(dir)
    }
    /// The canonical form of a directory path, links resolved, so the folder
    /// engine can spot link loops. `None` where there are no links.
    fn canonical(&self, _path: &str) -> Option<String> {
        None
    }
}

/// How [`Vfs::read_dir_with`] treats symbolic links and NTFS junctions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ListOptions {
    /// List a link with its target's metadata (a linked folder lists as a
    /// folder). When off, links list as files of size 0 carrying
    /// [`EntryMeta::link_target`].
    pub follow_links: bool,
}

impl Default for ListOptions {
    fn default() -> Self {
        Self { follow_links: true }
    }
}

/// The machine's own file system.
pub struct LocalFs;

/// A file system with nothing in it. Stands in for the side of a folder
/// compare that has no folder chosen yet, so the other side lists on its own.
pub struct EmptyFs;

impl Vfs for EmptyFs {
    fn read_dir(&self, _dir: &str) -> Result<Vec<VfsEntry>> {
        Ok(Vec::new())
    }

    fn open(&self, path: &str) -> Result<Box<dyn Read + Send>> {
        Err(CoreError::io(path, std::io::ErrorKind::NotFound.into()))
    }

    fn join(&self, dir: &str, name: &str) -> String {
        format!("{dir}/{name}")
    }
}

pub(crate) fn meta_from_std(m: &Metadata) -> EntryMeta {
    let modified_ms = m
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64);
    EntryMeta {
        size: if m.is_dir() { 0 } else { m.len() },
        modified_ms,
        is_dir: m.is_dir(),
        ..Default::default()
    }
}

/// DOS attribute bits and, on Unix, the permission bits of a local file.
#[cfg(windows)]
fn attributes(_hidden: bool, m: &Metadata) -> (u32, Option<u32>) {
    use std::os::windows::fs::MetadataExt;
    (m.file_attributes() & attr::DOS_MASK, None)
}

#[cfg(not(windows))]
fn attributes(hidden: bool, m: &Metadata) -> (u32, Option<u32>) {
    let mut bits = 0;
    if m.permissions().readonly() {
        bits |= attr::READONLY;
    }
    if hidden {
        bits |= attr::HIDDEN;
    }
    #[cfg(unix)]
    let mode = {
        use std::os::unix::fs::PermissionsExt;
        Some(m.permissions().mode() & 0o7777)
    };
    #[cfg(not(unix))]
    let mode = None;
    (bits, mode)
}

#[cfg(windows)]
fn is_hidden(name: &str, meta: &Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
    name.starts_with('.') || meta.file_attributes() & FILE_ATTRIBUTE_HIDDEN != 0
}

#[cfg(not(windows))]
fn is_hidden(name: &str, _meta: &Metadata) -> bool {
    name.starts_with('.')
}

impl Vfs for LocalFs {
    fn read_dir(&self, dir: &str) -> Result<Vec<VfsEntry>> {
        self.read_dir_with(dir, ListOptions::default())
    }

    fn read_dir_with(&self, dir: &str, opts: ListOptions) -> Result<Vec<VfsEntry>> {
        let mut out = Vec::new();
        for entry in fs::read_dir(dir).map_err(|e| CoreError::io(dir, e))? {
            let entry = entry.map_err(|e| CoreError::io(dir, e))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let path = entry.path();
            // Junctions count as links on Windows too.
            let is_link = entry.file_type().is_ok_and(|t| t.is_symlink());
            // `DirEntry::metadata` describes the link itself.
            let own = || entry.metadata().map_err(|e| CoreError::io(&path, e));
            let meta = if is_link && !opts.follow_links {
                own()?
            } else {
                // `fs::metadata` follows symlinks so linked files compare by
                // target; a dangling link falls back to the link itself.
                match fs::metadata(&path) {
                    Ok(m) => m,
                    Err(_) => own()?,
                }
            };
            let hidden = is_hidden(&name, &meta);
            let mut m = meta_from_std(&meta);
            if is_link {
                m.link_target = Some(
                    fs::read_link(&path)
                        .map_or_else(|_| String::new(), |t| t.to_string_lossy().into_owned()),
                );
                if !opts.follow_links {
                    m.size = 0;
                    m.is_dir = false;
                }
            }
            let (bits, mode) = attributes(hidden, &meta);
            m.attributes = Some(bits);
            m.mode = mode;
            out.push(VfsEntry {
                hidden,
                meta: m,
                name,
            });
        }
        Ok(out)
    }

    fn canonical(&self, path: &str) -> Option<String> {
        fs::canonicalize(path)
            .ok()
            .map(|p| p.to_string_lossy().into_owned())
    }

    fn open(&self, path: &str) -> Result<Box<dyn Read + Send>> {
        let f = fs::File::open(path).map_err(|e| CoreError::io(path, e))?;
        Ok(Box::new(f))
    }

    fn join(&self, dir: &str, name: &str) -> String {
        Path::new(dir).join(name).to_string_lossy().into_owned()
    }
}
