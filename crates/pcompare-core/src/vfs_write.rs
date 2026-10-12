//! Write access to a [`Vfs`]: what Folder Sync needs to copy into, delete
//! from and tidy up remote folders (SFTP, FTP, S3, cloud drives, ...).
//!
//! Errors are plain [`io::Error`]s with a meaningful [`io::ErrorKind`] (not
//! found, permission denied, connection reset, ...) so the sync executor can
//! classify them exactly like local failures: retry network hiccups, report
//! missing files, stop on a full disk.
//!
//! Back ends differ a lot in what they can do — object stores can't rename,
//! most cloud APIs can't set a modification time — so callers check
//! [`VfsWrite::caps`] and pick the safest strategy available.

use std::fs;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::time::{Duration, UNIX_EPOCH};

use serde::Serialize;

use crate::folder::EntryMeta;
use crate::vfs::{meta_from_std, LocalFs, Vfs};

/// What a writable file system supports beyond plain whole-file writes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WriteCaps {
    /// Files and folders can be renamed / moved within the file system, so a
    /// new file can be written under a temp name and swapped into place.
    pub rename: bool,
    /// A file's modification time can be set after it was written.
    pub set_modified: bool,
    /// A write can continue an existing file at an offset (resumable uploads).
    pub resume_write: bool,
    /// Reads can start at an offset (resumable downloads).
    pub range_read: bool,
    /// Folders are real and must exist before files are written into them
    /// (unlike object stores, where they are implied by key prefixes).
    pub folders: bool,
}

/// A [`Vfs`] that can also be written to. Paths use the file system's own
/// syntax, built with [`Vfs::join`].
pub trait VfsWrite: Vfs {
    fn caps(&self) -> WriteCaps;

    /// Metadata of `path` (following links), `None` if nothing is there.
    fn stat(&self, path: &str) -> io::Result<Option<EntryMeta>>;

    /// Opens a file for reading from byte `offset` on. An offset other than
    /// 0 needs [`WriteCaps::range_read`].
    fn open_at(&self, path: &str, offset: u64) -> io::Result<Box<dyn Read + Send>>;

    /// Writes everything `src` yields into `path`, creating or truncating it
    /// — or, with `offset > 0` (needs [`WriteCaps::resume_write`]),
    /// continuing a file that already holds exactly `offset` bytes.
    /// `on_bytes` gets chunk sizes as they go out; an error from it aborts
    /// the write. Returns the number of bytes written by this call.
    fn write(
        &self,
        path: &str,
        offset: u64,
        src: &mut dyn Read,
        on_bytes: &mut dyn FnMut(u64) -> io::Result<()>,
    ) -> io::Result<u64>;

    /// Creates a folder and any missing parents (no-op if it exists).
    fn create_dir_all(&self, path: &str) -> io::Result<()>;

    /// Deletes one file. Deleting something already gone succeeds.
    fn remove_file(&self, path: &str) -> io::Result<()>;

    /// Deletes a folder and everything in it. Already gone succeeds.
    fn remove_dir_all(&self, path: &str) -> io::Result<()>;

    /// Renames / moves within the file system ([`WriteCaps::rename`]). The
    /// target must not exist.
    fn rename(&self, from: &str, to: &str) -> io::Result<()>;

    /// Sets the modification time ([`WriteCaps::set_modified`]).
    fn set_modified(&self, path: &str, modified_ms: i64) -> io::Result<()>;
}

/// The error back ends return for an operation their caps rule out.
pub fn unsupported(what: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        format!("{what} isn't supported by this server"),
    )
}

fn not_found_is_ok(r: io::Result<()>) -> io::Result<()> {
    match r {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

/// Copies `src` into `out` in 1 MiB chunks, reporting each chunk.
pub fn pump(
    src: &mut dyn Read,
    out: &mut dyn Write,
    on_bytes: &mut dyn FnMut(u64) -> io::Result<()>,
) -> io::Result<u64> {
    let mut buf = vec![0u8; 1 << 20];
    let mut total = 0u64;
    loop {
        let n = match src.read(&mut buf) {
            Ok(0) => return Ok(total),
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        out.write_all(&buf[..n])?;
        total += n as u64;
        on_bytes(n as u64)?;
    }
}

/// The local disk as a writable VFS. The sync executor treats local folders
/// natively (Recycle Bin, staged replacement); this exists so the remote code
/// paths can be exercised against plain folders in tests.
impl VfsWrite for LocalFs {
    fn caps(&self) -> WriteCaps {
        WriteCaps {
            rename: true,
            set_modified: true,
            resume_write: true,
            range_read: true,
            folders: true,
        }
    }

    fn stat(&self, path: &str) -> io::Result<Option<EntryMeta>> {
        match fs::metadata(path) {
            Ok(m) => Ok(Some(meta_from_std(&m))),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn open_at(&self, path: &str, offset: u64) -> io::Result<Box<dyn Read + Send>> {
        let mut f = fs::File::open(path)?;
        if offset > 0 {
            f.seek(SeekFrom::Start(offset))?;
        }
        Ok(Box::new(f))
    }

    fn write(
        &self,
        path: &str,
        offset: u64,
        src: &mut dyn Read,
        on_bytes: &mut dyn FnMut(u64) -> io::Result<()>,
    ) -> io::Result<u64> {
        let mut out = if offset == 0 {
            fs::File::create(path)?
        } else {
            let mut f = fs::OpenOptions::new().write(true).open(path)?;
            if f.metadata()?.len() != offset {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "the partial file has an unexpected size",
                ));
            }
            f.seek(SeekFrom::Start(offset))?;
            f
        };
        let n = pump(src, &mut out, on_bytes)?;
        out.sync_all()?;
        Ok(n)
    }

    fn create_dir_all(&self, path: &str) -> io::Result<()> {
        fs::create_dir_all(path)
    }

    fn remove_file(&self, path: &str) -> io::Result<()> {
        not_found_is_ok(fs::remove_file(path))
    }

    fn remove_dir_all(&self, path: &str) -> io::Result<()> {
        not_found_is_ok(fs::remove_dir_all(path))
    }

    fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        if fs::symlink_metadata(to).is_ok() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("{to} already exists"),
            ));
        }
        fs::rename(from, to)
    }

    fn set_modified(&self, path: &str, modified_ms: i64) -> io::Result<()> {
        let when = UNIX_EPOCH + Duration::from_millis(modified_ms.max(0) as u64);
        fs::OpenOptions::new().write(true).open(path)?.set_modified(when)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_fs_round_trip_and_resume() {
        let dir = tempfile::tempdir().unwrap();
        let fs_ = LocalFs;
        let sub = fs_.join(&dir.path().to_string_lossy(), "a");
        let sub = fs_.join(&sub, "b");
        fs_.create_dir_all(&sub).unwrap();
        let file = fs_.join(&sub, "f.txt");

        let mut seen = 0;
        let n = fs_
            .write(&file, 0, &mut &b"hello "[..], &mut |b| {
                seen += b;
                Ok(())
            })
            .unwrap();
        assert_eq!((n, seen), (6, 6));
        // Continue where the first write stopped.
        fs_.write(&file, 6, &mut &b"world"[..], &mut |_| Ok(())).unwrap();
        assert!(
            fs_.write(&file, 3, &mut &b"x"[..], &mut |_| Ok(())).is_err(),
            "offset must match the size"
        );

        let mut tail = String::new();
        fs_.open_at(&file, 6).unwrap().read_to_string(&mut tail).unwrap();
        assert_eq!(tail, "world");

        fs_.set_modified(&file, 1_600_000_000_000).unwrap();
        let meta = fs_.stat(&file).unwrap().unwrap();
        assert_eq!((meta.size, meta.modified_ms), (11, Some(1_600_000_000_000)));

        let moved = fs_.join(&sub, "g.txt");
        fs_.rename(&file, &moved).unwrap();
        assert!(fs_.stat(&file).unwrap().is_none());
        fs_.remove_file(&file).unwrap(); // already gone is fine
        let top = fs_.join(&dir.path().to_string_lossy(), "a");
        fs_.remove_dir_all(&top).unwrap();
        assert!(fs_.stat(&top).unwrap().is_none());
    }
}
