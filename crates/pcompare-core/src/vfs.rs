//! A minimal file-system abstraction so the folder engine can walk local
//! disks and remote stores (S3, SFTP, OneDrive, ...) with the same code.
//!
//! Paths are plain strings in the file system's own syntax; only the
//! implementation knows how to join and interpret them. Methods are blocking
//! because the folder engine runs on the rayon pool — remote implementations
//! drive their own async runtime internally.

use crate::folder::EntryMeta;
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
    }
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
        let mut out = Vec::new();
        for entry in fs::read_dir(dir).map_err(|e| CoreError::io(dir, e))? {
            let entry = entry.map_err(|e| CoreError::io(dir, e))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            // `fs::metadata` follows symlinks so linked files compare by target.
            let meta = match fs::metadata(entry.path()) {
                Ok(m) => m,
                Err(_) => match entry.metadata() {
                    Ok(m) => m, // dangling link: fall back to the link itself
                    Err(e) => return Err(CoreError::io(entry.path(), e)),
                },
            };
            out.push(VfsEntry {
                hidden: is_hidden(&name, &meta),
                meta: meta_from_std(&meta),
                name,
            });
        }
        Ok(out)
    }

    fn open(&self, path: &str) -> Result<Box<dyn Read + Send>> {
        let f = fs::File::open(path).map_err(|e| CoreError::io(path, e))?;
        Ok(Box::new(f))
    }

    fn join(&self, dir: &str, name: &str) -> String {
        Path::new(dir).join(name).to_string_lossy().into_owned()
    }
}
