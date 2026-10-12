//! Piece-table edit overlay for the hex editor.
//!
//! The file on disk is never loaded whole: the edited content is a list of
//! pieces, each a range of either the original file or an append-only buffer
//! of typed/pasted bytes. Edits rebuild only the piece list, undo snapshots
//! are cheap `Arc` clones of it, and saving streams the pieces to a new file.

use crate::{CoreError, Result};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

/// Bytes copied per read/write while saving or scanning.
pub(crate) const IO_CHUNK: usize = 1 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    Original,
    Added,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Piece {
    src: Source,
    start: u64,
    len: u64,
}

impl Piece {
    fn sub(self, skip: u64, len: u64) -> Piece {
        Piece {
            src: self.src,
            start: self.start + skip,
            len,
        }
    }
}

/// An immutable piece list; cloning one is how undo snapshots are taken.
pub type Pieces = Arc<Vec<Piece>>;

/// File length and modification time, used to notice files changed on disk.
type Stamp = (u64, Option<SystemTime>);

fn stamp(path: &Path) -> Result<Stamp> {
    let meta = std::fs::metadata(path).map_err(|e| CoreError::io(path, e))?;
    Ok((meta.len(), meta.modified().ok()))
}

fn open(path: &Path) -> Result<File> {
    File::open(path).map_err(|e| CoreError::io(path, e))
}

/// Anything that can be read at random offsets: an edit buffer or a
/// snapshot of one taken for background work.
pub trait ByteSource {
    fn len(&self) -> u64;
    /// Fills as much of `buf` as there is content from `offset` on.
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<usize>;

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn read_vec(&mut self, offset: u64, len: usize) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; len];
        let n = self.read_at(offset, &mut buf)?;
        buf.truncate(n);
        Ok(buf)
    }
}

fn read_file_at(file: &mut File, path: &Path, offset: u64, buf: &mut [u8]) -> Result<usize> {
    file.seek(SeekFrom::Start(offset)).map_err(|e| CoreError::io(path, e))?;
    let mut filled = 0;
    while filled < buf.len() {
        match file.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) => return Err(CoreError::io(path, e)),
        }
    }
    Ok(filled)
}

fn prefix_starts(pieces: &[Piece]) -> Vec<u64> {
    let mut at = 0;
    pieces
        .iter()
        .map(|p| {
            let s = at;
            at += p.len;
            s
        })
        .collect()
}

/// Reads logical `offset..` of a piece list into `buf`.
fn read_pieces(
    pieces: &[Piece],
    starts: &[u64],
    file: &mut File,
    path: &Path,
    added: &[u8],
    offset: u64,
    buf: &mut [u8],
) -> Result<usize> {
    let mut filled = 0usize;
    let mut i = starts.partition_point(|&s| s <= offset).saturating_sub(1);
    while filled < buf.len() && i < pieces.len() {
        let (p, s) = (pieces[i], starts[i]);
        let at = offset + filled as u64;
        if at >= s + p.len {
            i += 1;
            continue;
        }
        let skip = at - s;
        let n = ((p.len - skip) as usize).min(buf.len() - filled);
        let dst = &mut buf[filled..filled + n];
        match p.src {
            Source::Added => {
                let from = (p.start + skip) as usize;
                dst.copy_from_slice(&added[from..from + n]);
            }
            Source::Original => {
                let got = read_file_at(file, path, p.start + skip, dst)?;
                if got < n {
                    // The file shrank underneath us; report what we have.
                    return Ok(filled + got);
                }
            }
        }
        filled += n;
        i += 1;
    }
    Ok(filled)
}

/// One file opened for editing.
pub struct EditBuffer {
    path: PathBuf,
    /// `None` only while a save swaps files around.
    file: Option<File>,
    stamp: Stamp,
    added: Arc<Vec<u8>>,
    pieces: Pieces,
    /// `starts[i]` is the logical offset where `pieces[i]` begins.
    starts: Vec<u64>,
    len: u64,
    /// The piece list matching the file on disk.
    saved: Pieces,
}

impl EditBuffer {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let file = open(&path)?;
        let stamp = stamp(&path)?;
        let pieces: Pieces = Arc::new(if stamp.0 == 0 {
            Vec::new()
        } else {
            vec![Piece {
                src: Source::Original,
                start: 0,
                len: stamp.0,
            }]
        });
        Ok(EditBuffer {
            starts: prefix_starts(&pieces),
            len: stamp.0,
            path,
            file: Some(file),
            stamp,
            added: Arc::new(Vec::new()),
            saved: pieces.clone(),
            pieces,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn is_dirty(&self) -> bool {
        !Arc::ptr_eq(&self.pieces, &self.saved) && *self.pieces != *self.saved
    }

    /// Whether the file on disk no longer matches what was opened (or last saved).
    pub fn changed_on_disk(&self) -> bool {
        stamp(&self.path).map_or(true, |s| s != self.stamp)
    }

    /// The current content, for undo.
    pub fn pieces(&self) -> Pieces {
        self.pieces.clone()
    }

    /// Returns to content captured earlier with [`EditBuffer::pieces`].
    pub fn restore(&mut self, pieces: Pieces) {
        self.starts = prefix_starts(&pieces);
        self.len = pieces.iter().map(|p| p.len).sum();
        self.pieces = pieces;
    }

    /// Replaces `remove` bytes at `offset` with `insert`. Offsets past the
    /// end clamp to it, so overwriting at the end appends.
    pub fn replace(&mut self, offset: u64, remove: u64, insert: &[u8]) {
        let offset = offset.min(self.len);
        let remove = remove.min(self.len - offset);
        self.replace_many(&[offset], remove, insert);
    }

    /// Replaces `remove` bytes at each of `offsets` (ascending, non-overlapping,
    /// in bounds) with `insert`, in one pass over the piece list.
    pub fn replace_many(&mut self, offsets: &[u64], remove: u64, insert: &[u8]) {
        if offsets.is_empty() || (remove == 0 && insert.is_empty()) {
            return;
        }
        let new_piece = (!insert.is_empty()).then(|| {
            let added = Arc::make_mut(&mut self.added);
            let start = added.len() as u64;
            added.extend_from_slice(insert);
            Piece {
                src: Source::Added,
                start,
                len: insert.len() as u64,
            }
        });
        let old = &self.pieces;
        let mut out: Vec<Piece> = Vec::with_capacity(old.len() + 2 * offsets.len());
        let push = |out: &mut Vec<Piece>, p: Piece| {
            if p.len == 0 {
                return;
            }
            match out.last_mut() {
                Some(last) if last.src == p.src && last.start + last.len == p.start => last.len += p.len,
                _ => out.push(p),
            }
        };
        // Copies logical `from..to` of the old content; `pi` only moves forward.
        let mut pi = 0usize;
        let mut copy = |out: &mut Vec<Piece>, from: u64, to: u64| {
            while pi < old.len() && self.starts[pi] + old[pi].len <= from {
                pi += 1;
            }
            let mut i = pi;
            while i < old.len() && self.starts[i] < to {
                let (s, p) = (self.starts[i], old[i]);
                let lo = from.max(s);
                let hi = to.min(s + p.len);
                if hi > lo {
                    push(out, p.sub(lo - s, hi - lo));
                }
                i += 1;
            }
        };
        let mut cursor = 0u64;
        for &at in offsets {
            let at = at.max(cursor).min(self.len);
            copy(&mut out, cursor, at);
            if let Some(p) = new_piece {
                push(&mut out, p);
            }
            cursor = (at + remove).min(self.len);
        }
        copy(&mut out, cursor, self.len);
        self.restore(Arc::new(out));
    }

    /// Writes the edited content over the file. With `backup`, the previous
    /// file is kept next to it as `<name>.bak`. Refuses when the file changed
    /// on disk since it was opened, since unedited ranges are read from it.
    pub fn save(&mut self, backup: bool) -> Result<()> {
        if self.changed_on_disk() {
            return Err(CoreError::Other(format!(
                "{} changed on disk since it was opened; reload it before saving",
                self.path.display()
            )));
        }
        let name = self
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let tmp = self.path.with_file_name(format!("{name}.pcompare-save"));
        let written = self.write_to(&tmp);
        if let Err(e) = written {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
        if let Ok(meta) = std::fs::metadata(&self.path) {
            let _ = std::fs::set_permissions(&tmp, meta.permissions());
        }
        // Windows can't rename a file that is still open.
        self.file = None;
        let path = self.path.clone();
        let bak = path.with_file_name(format!("{name}.bak"));
        let swapped = (|| {
            if backup {
                match std::fs::remove_file(&bak) {
                    Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(CoreError::io(&bak, e)),
                    _ => {}
                }
                std::fs::rename(&path, &bak).map_err(|e| CoreError::io(&bak, e))?;
            }
            std::fs::rename(&tmp, &path).map_err(|e| {
                if backup {
                    // Put the original back rather than leave no file at all.
                    let _ = std::fs::rename(&bak, &path);
                }
                CoreError::io(&path, e)
            })
        })();
        if let Err(e) = swapped {
            let _ = std::fs::remove_file(&tmp);
            // Keep editing against whatever is on disk now.
            self.file = open(&path).ok();
            return Err(e);
        }
        *self = EditBuffer::open(&path)?;
        Ok(())
    }

    fn write_to(&mut self, dest: &Path) -> Result<()> {
        let mut out = File::create(dest).map_err(|e| CoreError::io(dest, e))?;
        let mut buf = vec![0u8; IO_CHUNK];
        let mut offset = 0;
        while offset < self.len {
            let n = self.read_at(offset, &mut buf)?;
            if n == 0 {
                return Err(CoreError::Other(format!(
                    "{}: file shrank while saving",
                    self.path.display()
                )));
            }
            out.write_all(&buf[..n]).map_err(|e| CoreError::io(dest, e))?;
            offset += n as u64;
        }
        out.sync_all().map_err(|e| CoreError::io(dest, e))
    }

    /// A read-only copy of the current content with its own file handle, for
    /// work that runs without holding the session.
    pub fn snapshot(&self) -> Result<Snapshot> {
        Ok(Snapshot {
            file: open(&self.path)?,
            path: self.path.clone(),
            added: self.added.clone(),
            pieces: self.pieces.clone(),
            starts: self.starts.clone(),
            len: self.len,
        })
    }
}

impl ByteSource for EditBuffer {
    fn len(&self) -> u64 {
        self.len
    }

    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<usize> {
        let file = match &mut self.file {
            Some(f) => f,
            None => self.file.insert(open(&self.path)?),
        };
        read_pieces(&self.pieces, &self.starts, file, &self.path, &self.added, offset, buf)
    }
}

/// See [`EditBuffer::snapshot`].
pub struct Snapshot {
    file: File,
    path: PathBuf,
    added: Arc<Vec<u8>>,
    pieces: Pieces,
    starts: Vec<u64>,
    len: u64,
}

impl Snapshot {
    /// The piece list this snapshot was taken from, to tell whether the
    /// buffer has been edited since.
    pub fn pieces(&self) -> &Pieces {
        &self.pieces
    }
}

impl ByteSource for Snapshot {
    fn len(&self) -> u64 {
        self.len
    }

    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<usize> {
        read_pieces(
            &self.pieces,
            &self.starts,
            &mut self.file,
            &self.path,
            &self.added,
            offset,
            buf,
        )
    }
}

/// Reads the whole content of `src` (callers check the size first).
pub fn read_all(src: &mut dyn ByteSource) -> Result<Vec<u8>> {
    let len = src.len() as usize;
    let mut buf = vec![0u8; len];
    let mut filled = 0;
    while filled < len {
        let n = src.read_at(filled as u64, &mut buf[filled..])?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    buf.truncate(filled);
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buffer(content: &[u8]) -> (tempfile::TempDir, EditBuffer) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.bin");
        std::fs::write(&path, content).unwrap();
        let buf = EditBuffer::open(&path).unwrap();
        (dir, buf)
    }

    fn content(b: &mut EditBuffer) -> Vec<u8> {
        read_all(b).unwrap()
    }

    #[test]
    fn overwrite_insert_delete() {
        let (_d, mut b) = buffer(b"0123456789");
        b.replace(2, 2, b"ab"); // overwrite
        assert_eq!(content(&mut b), b"01ab456789");
        b.replace(5, 0, b"XYZ"); // insert
        assert_eq!(content(&mut b), b"01ab4XYZ56789");
        b.replace(0, 3, b""); // delete
        assert_eq!(content(&mut b), b"b4XYZ56789");
        b.replace(100, 1, b"!"); // past the end appends
        assert_eq!(content(&mut b), b"b4XYZ56789!");
        assert_eq!(b.len(), 11);
        assert_eq!(b.read_vec(3, 4).unwrap(), b"YZ56");
        assert!(b.is_dirty());
    }

    #[test]
    fn sequential_typing_coalesces_pieces() {
        let (_d, mut b) = buffer(b"abc");
        for (i, c) in b"hello".iter().enumerate() {
            b.replace(3 + i as u64, 0, &[*c]);
        }
        assert_eq!(content(&mut b), b"abchello");
        assert_eq!(b.pieces().len(), 2);
    }

    #[test]
    fn restore_snapshots_and_dirty_tracking() {
        let (_d, mut b) = buffer(b"abcdef");
        let before = b.pieces();
        b.replace(1, 1, b"Z");
        assert!(b.is_dirty());
        let after = b.pieces();
        b.restore(before);
        assert_eq!(content(&mut b), b"abcdef");
        assert!(!b.is_dirty());
        b.restore(after);
        assert_eq!(content(&mut b), b"aZcdef");
    }

    #[test]
    fn replace_many_in_one_pass() {
        let (_d, mut b) = buffer(b"aXbXcX");
        b.replace_many(&[1, 3, 5], 1, b"--");
        assert_eq!(content(&mut b), b"a--b--c--");
        b.replace_many(&[0, 3], 3, b"");
        assert_eq!(content(&mut b), b"c--");
    }

    #[test]
    fn save_writes_backup_and_resets() {
        let (dir, mut b) = buffer(b"hello world");
        b.replace(0, 5, b"HOWDY");
        b.replace(11, 0, b"!");
        b.save(true).unwrap();
        assert_eq!(std::fs::read(dir.path().join("f.bin")).unwrap(), b"HOWDY world!");
        assert_eq!(std::fs::read(dir.path().join("f.bin.bak")).unwrap(), b"hello world");
        assert!(!b.is_dirty());
        assert_eq!(content(&mut b), b"HOWDY world!");
        assert!(!dir.path().join("f.bin.pcompare-save").exists());
    }

    #[test]
    fn save_refuses_when_changed_on_disk() {
        let (dir, mut b) = buffer(b"one");
        b.replace(0, 1, b"O");
        std::fs::write(dir.path().join("f.bin"), b"something else").unwrap();
        assert!(b.changed_on_disk());
        assert!(b.save(false).is_err());
    }

    #[test]
    fn snapshot_reads_independently() {
        let (_d, mut b) = buffer(b"abcdef");
        b.replace(3, 0, b"123");
        let mut snap = b.snapshot().unwrap();
        b.replace(0, 6, b"");
        assert_eq!(read_all(&mut snap).unwrap(), b"abc123def");
        assert_eq!(content(&mut b), b"def");
    }
}
