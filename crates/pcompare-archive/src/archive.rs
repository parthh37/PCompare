//! An archive file as a [`Vfs`], and — for zip — a [`VfsWrite`].
//!
//! The listing is built once per version of the archive file (size and
//! modification time) and entries are streamed on demand: stored and
//! deflated zip entries straight from the file, tar entries from their
//! offset (compressed tars are decompressed once into the cache), 7z
//! archives decoded in one pass into the cache on first read.
//!
//! Writes rewrite the zip into a temp file beside it — existing entries are
//! copied without recompressing — and swap it into place, so the archive is
//! never left half-written.

use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use pcompare_core::folder::EntryMeta;
use pcompare_core::vfs::{Vfs, VfsEntry};
use pcompare_core::vfs_write::{pump, VfsWrite, WriteCaps};
use pcompare_core::{CoreError, Result};

use crate::formats::{self, other, ZipPlan};
use crate::tree::{self, normalize, shown, Loc, Tree};
use crate::{ArchiveKind, CrcSource};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Stamp {
    size: u64,
    modified: Option<SystemTime>,
}

impl Stamp {
    fn of(file: &Path) -> io::Result<Self> {
        let m = fs::metadata(file)?;
        if m.is_dir() {
            return Err(io::Error::other("this is a folder, not an archive"));
        }
        Ok(Stamp {
            size: m.len(),
            modified: m.modified().ok(),
        })
    }

    fn modified_ms(&self) -> Option<i64> {
        let d = self.modified?.duration_since(UNIX_EPOCH).ok()?;
        Some(d.as_millis() as i64)
    }
}

/// One indexed version of the archive.
struct Loaded {
    stamp: Stamp,
    tree: Tree,
    data: Data,
}

enum Data {
    /// The parsed central directory, for entries the zip crate must decode.
    Zip(Mutex<zip::ZipArchive<File>>),
    /// The uncompressed tar stream the offsets point into.
    Tar(PathBuf),
    Single,
    /// Folder the entries are decoded into on first read.
    SevenZ(Mutex<Option<std::result::Result<PathBuf, String>>>),
}

pub struct ArchiveFs {
    file: PathBuf,
    label: String,
    kind: ArchiveKind,
    /// Scratch space for this archive (decompressed tar, 7z entries).
    scratch: PathBuf,
    read_only: Option<String>,
    loaded: Mutex<Option<Arc<Loaded>>>,
    /// Serializes rewrites of the archive.
    writing: Mutex<()>,
}

/// Turns an archive failure into an I/O error naming the entry.
fn io_err(label: &str, path: &str, e: io::Error) -> CoreError {
    CoreError::io(shown(label, path), e)
}

impl ArchiveFs {
    pub(crate) fn new(
        file: PathBuf,
        label: String,
        kind: ArchiveKind,
        scratch: PathBuf,
        read_only: Option<String>,
    ) -> Self {
        Self {
            file,
            label,
            kind,
            scratch,
            read_only,
            loaded: Mutex::new(None),
            writing: Mutex::new(()),
        }
    }

    pub fn kind(&self) -> ArchiveKind {
        self.kind
    }

    /// The archive's path as the user wrote it.
    pub fn label(&self) -> &str {
        &self.label
    }

    /// Why this archive can't be modified, `None` if it can.
    pub fn read_only_reason(&self) -> Option<String> {
        if let Some(why) = &self.read_only {
            return Some(why.clone());
        }
        (!self.kind.writable()).then(|| {
            format!(
                "{} archives are read-only — only zip archives can be modified",
                self.kind.label()
            )
        })
    }

    /// The current index, rebuilt when the archive file changed.
    fn load(&self) -> Result<Arc<Loaded>> {
        let stamp = Stamp::of(&self.file).map_err(|e| CoreError::io(&self.label, e))?;
        let mut slot = self.loaded.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(l) = slot.as_ref().filter(|l| l.stamp == stamp) {
            return Ok(l.clone());
        }
        let old = slot.take();
        drop(old);
        let loaded = Arc::new(self.index(stamp).map_err(|e| {
            CoreError::Other(format!(
                "{}: can't read the {} archive: {e}",
                self.label,
                self.kind.label()
            ))
        })?);
        *slot = Some(loaded.clone());
        Ok(loaded)
    }

    fn index(&self, stamp: Stamp) -> io::Result<Loaded> {
        // Cache files of an older version of the archive are stale.
        let _ = fs::remove_dir_all(&self.scratch);
        let (tree, data) = match self.kind {
            ArchiveKind::Zip => {
                let (tree, zip) = formats::zip_index(&self.file)?;
                (tree, Data::Zip(Mutex::new(zip)))
            }
            ArchiveKind::Tar => (
                formats::tar_index(&self.file)?,
                Data::Tar(self.file.clone()),
            ),
            ArchiveKind::TarGz | ArchiveKind::TarBz2 | ArchiveKind::TarXz => {
                let plain = self.scratch.join("archive.tar");
                let mut src = formats::decoder(self.kind, File::open(&self.file)?);
                crate::write_atomic(&plain, &mut src)?;
                (formats::tar_index(&plain)?, Data::Tar(plain))
            }
            ArchiveKind::Gz | ArchiveKind::Bz2 | ArchiveKind::Xz => {
                let name = self
                    .file
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let inner = formats::single_name(&name);
                (
                    formats::single_index(&self.file, self.kind, &inner, stamp.modified_ms())?,
                    Data::Single,
                )
            }
            ArchiveKind::SevenZ => (
                formats::sevenz_index(&self.file)?,
                Data::SevenZ(Mutex::new(None)),
            ),
        };
        Ok(Loaded { stamp, tree, data })
    }

    /// Whether `path` is a file in the archive.
    pub fn is_file(&self, path: &str) -> Result<bool> {
        Ok(self.load()?.tree.get(path).is_some_and(|n| !n.meta.is_dir))
    }

    fn open_entry(&self, path: &str) -> Result<Box<dyn Read + Send>> {
        let loaded = self.load()?;
        let node = loaded
            .tree
            .get(path)
            .ok_or_else(|| tree::not_found(&self.label, path))?;
        if node.meta.is_dir {
            return Err(CoreError::Other(format!(
                "{} is a folder",
                shown(&self.label, path)
            )));
        }
        let err = |e| io_err(&self.label, path, e);
        let crc = node.crc;
        let reader: Box<dyn Read + Send> = match (node.loc, &loaded.data) {
            (Loc::None, _) => Box::new(io::empty()),
            (Loc::Zip(i), Data::Zip(zip)) => {
                let plan = formats::zip_plan(&mut zip.lock().unwrap_or_else(|e| e.into_inner()), i)
                    .map_err(err)?;
                let raw: Box<dyn Read + Send> = match plan {
                    ZipPlan::Stored { start, len } => {
                        Box::new(formats::slice(&self.file, start, len).map_err(err)?)
                    }
                    ZipPlan::Deflated { start, len } => {
                        Box::new(flate2::read::DeflateDecoder::new(
                            formats::slice(&self.file, start, len).map_err(err)?,
                        ))
                    }
                    ZipPlan::Other => {
                        // Rare methods (bzip2, LZMA, deflate64...): decoded by the
                        // zip crate into the cache, as its reader borrows the archive.
                        let dest = self.scratch.join(format!("zip-{i}"));
                        if !dest.is_file() {
                            let mut zip = zip.lock().unwrap_or_else(|e| e.into_inner());
                            let mut entry = zip.by_index(i).map_err(|e| err(other(e)))?;
                            crate::write_atomic(&dest, &mut entry).map_err(err)?;
                        }
                        return Ok(Box::new(File::open(&dest).map_err(err)?));
                    }
                };
                match crc {
                    Some(c) => Box::new(formats::CrcCheck::new(raw, c, &shown(&self.label, path))),
                    None => raw,
                }
            }
            (Loc::Tar(offset), Data::Tar(plain)) => {
                Box::new(formats::slice(plain, offset, node.meta.size).map_err(err)?)
            }
            (Loc::Single, Data::Single) => {
                formats::decoder(self.kind, File::open(&self.file).map_err(err)?)
            }
            (Loc::SevenZ(i), Data::SevenZ(done)) => {
                let mut done = done.lock().unwrap_or_else(|e| e.into_inner());
                let extracted = done.get_or_insert_with(|| {
                    let dir = self.scratch.join("7z");
                    formats::sevenz_extract_all(&self.file, &dir)
                        .map(|()| dir)
                        .map_err(|e| e.to_string())
                });
                let dir = extracted.as_ref().map_err(|msg| {
                    CoreError::Other(format!("{}: {msg}", shown(&self.label, path)))
                })?;
                let f = dir.join(i.to_string());
                if !f.exists() && node.meta.size == 0 {
                    Box::new(io::empty())
                } else {
                    Box::new(File::open(f).map_err(err)?)
                }
            }
            _ => {
                return Err(CoreError::Other(format!(
                    "{}: unreadable entry",
                    shown(&self.label, path)
                )))
            }
        };
        Ok(reader)
    }

    // ---------- zip rewriting ----------

    fn check_writable(&self) -> io::Result<()> {
        match self.read_only_reason() {
            Some(why) => Err(io::Error::new(io::ErrorKind::Unsupported, why)),
            None => Ok(()),
        }
    }

    /// Rewrites the zip: every existing entry goes through `each` (keep,
    /// drop, rename or re-date it — copied without recompressing), then
    /// `append` adds new entries, and the result replaces the archive.
    fn rewrite(
        &self,
        mut each: impl FnMut(&str) -> Action,
        append: impl FnOnce(&mut zip::ZipWriter<&File>) -> io::Result<()>,
    ) -> io::Result<()> {
        self.check_writable()?;
        let _one_at_a_time = self.writing.lock().unwrap_or_else(|e| e.into_inner());
        let dir = self.file.parent().unwrap_or(Path::new("."));
        let tmp = tempfile::Builder::new()
            .prefix(".pcompare-")
            .suffix(".zip.tmp")
            .tempfile_in(dir)?;
        {
            let mut src = zip::ZipArchive::new(File::open(&self.file)?).map_err(other)?;
            let mut out = zip::ZipWriter::new(tmp.as_file()).set_auto_large_file();
            for i in 0..src.len() {
                let entry = src.by_index_raw(i).map_err(other)?;
                let name = entry.name().to_owned();
                match each(&name) {
                    Action::Keep => out.raw_copy_file(entry),
                    Action::Drop => continue,
                    Action::Rename(to) => out.raw_copy_file_rename(entry, to),
                    Action::Touch(when) => {
                        let mode = entry.unix_mode();
                        out.raw_copy_file_touch(entry, when, mode)
                    }
                }
                .map_err(other)?;
            }
            append(&mut out)?;
            out.set_raw_comment(src.comment().into()).map_err(other)?;
            out.finish().map_err(other)?;
        }
        tmp.as_file().sync_all()?;
        // Keep the archive's permissions (temp files are private).
        if let Ok(m) = fs::metadata(&self.file) {
            let _ = fs::set_permissions(tmp.path(), m.permissions());
        }
        // Let go of the old archive's handle before replacing it.
        *self.loaded.lock().unwrap_or_else(|e| e.into_inner()) = None;
        tmp.persist(&self.file).map_err(|e| e.error)?;
        Ok(())
    }

    fn stat_node(&self, path: &str) -> io::Result<Option<EntryMeta>> {
        let loaded = self.load().map_err(other)?;
        Ok(loaded.tree.get(path).map(|n| n.meta.clone()))
    }
}

enum Action {
    Keep,
    Drop,
    Rename(String),
    Touch(zip::DateTime),
}

/// Whether zip entry `name` is `path` itself or inside folder `path`.
fn covers(name: &str, path: &str) -> bool {
    let name = name.trim_end_matches('/');
    name == path || name.strip_prefix(path).is_some_and(|r| r.starts_with('/'))
}

fn entry_path(path: &str) -> io::Result<String> {
    match normalize(path) {
        Some(p) if !p.is_empty() => Ok(p),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid archive path `{path}`"),
        )),
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

impl Vfs for ArchiveFs {
    fn read_dir(&self, dir: &str) -> Result<Vec<VfsEntry>> {
        self.load()?.tree.list(&self.label, dir)
    }

    fn open(&self, path: &str) -> Result<Box<dyn Read + Send>> {
        self.open_entry(path)
    }

    fn join(&self, dir: &str, name: &str) -> String {
        tree::join(dir, name)
    }
}

impl CrcSource for ArchiveFs {
    fn stored_crc(&self, path: &str) -> Option<u32> {
        let loaded = self.load().ok()?;
        loaded.tree.get(path).and_then(|n| n.crc)
    }
}

impl VfsWrite for ArchiveFs {
    fn caps(&self) -> WriteCaps {
        WriteCaps {
            rename: self.read_only_reason().is_none(),
            set_modified: self.read_only_reason().is_none(),
            resume_write: false,
            range_read: true,
            folders: true,
        }
    }

    fn stat(&self, path: &str) -> io::Result<Option<EntryMeta>> {
        self.stat_node(path)
    }

    fn open_at(&self, path: &str, offset: u64) -> io::Result<Box<dyn Read + Send>> {
        let mut r = self.open_entry(path).map_err(other)?;
        if offset > 0 {
            // Archive streams can't seek; skip ahead instead.
            let skipped = io::copy(&mut (&mut r).take(offset), &mut io::sink())?;
            if skipped < offset {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "offset beyond the end of the file",
                ));
            }
        }
        Ok(r)
    }

    fn write(
        &self,
        path: &str,
        offset: u64,
        src: &mut dyn Read,
        on_bytes: &mut dyn FnMut(u64) -> io::Result<()>,
    ) -> io::Result<u64> {
        self.check_writable()?;
        if offset > 0 {
            return Err(pcompare_core::vfs_write::unsupported(
                "resuming a write into a zip archive",
            ));
        }
        let path = entry_path(path)?;
        if self.stat_node(&path)?.is_some_and(|m| m.is_dir) {
            return Err(io::Error::new(
                io::ErrorKind::IsADirectory,
                format!("{path} is a folder"),
            ));
        }
        let mut written = 0;
        self.rewrite(
            |name| {
                if name == path {
                    Action::Drop
                } else {
                    Action::Keep
                }
            },
            |out| {
                let opts = zip::write::SimpleFileOptions::default()
                    .compression_method(zip::CompressionMethod::Deflated)
                    .last_modified_time(formats::zip_time(now_ms()))
                    .unix_permissions(0o644);
                out.start_file(path.as_str(), opts).map_err(other)?;
                written = pump(src, out, on_bytes)?;
                Ok(())
            },
        )?;
        Ok(written)
    }

    fn create_dir_all(&self, path: &str) -> io::Result<()> {
        self.check_writable()?;
        let path = match normalize(path) {
            Some(p) if p.is_empty() => return Ok(()),
            Some(p) => p,
            None => return entry_path(path).map(drop),
        };
        match self.stat_node(&path)? {
            Some(m) if m.is_dir => return Ok(()),
            Some(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("{path} is a file"),
                ));
            }
            None => {}
        }
        // Declare every missing folder, so other tools list them too.
        let mut missing = Vec::new();
        let mut cur = path.as_str();
        while !cur.is_empty() && self.stat_node(cur)?.is_none() {
            missing.push(format!("{cur}/"));
            cur = tree::split_parent(cur).0;
        }
        self.rewrite(
            |_| Action::Keep,
            |out| {
                for dir in missing.iter().rev() {
                    let opts = zip::write::SimpleFileOptions::default()
                        .last_modified_time(formats::zip_time(now_ms()))
                        .unix_permissions(0o755);
                    out.add_directory(dir.as_str(), opts).map_err(other)?;
                }
                Ok(())
            },
        )
    }

    fn remove_file(&self, path: &str) -> io::Result<()> {
        self.check_writable()?;
        let path = entry_path(path)?;
        if self.stat_node(&path)?.is_none() {
            return Ok(());
        }
        self.rewrite(
            |name| {
                if name == path {
                    Action::Drop
                } else {
                    Action::Keep
                }
            },
            |_| Ok(()),
        )
    }

    fn remove_dir_all(&self, path: &str) -> io::Result<()> {
        self.check_writable()?;
        let path = entry_path(path)?;
        if self.stat_node(&path)?.is_none() {
            return Ok(());
        }
        self.rewrite(
            |name| {
                if covers(name, &path) {
                    Action::Drop
                } else {
                    Action::Keep
                }
            },
            |_| Ok(()),
        )
    }

    fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        self.check_writable()?;
        let (from, to) = (entry_path(from)?, entry_path(to)?);
        if self.stat_node(&from)?.is_none() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("{from} doesn't exist"),
            ));
        }
        if self.stat_node(&to)?.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("{to} already exists"),
            ));
        }
        self.rewrite(
            |name| {
                if covers(name, &from) {
                    Action::Rename(format!("{to}{}", &name[from.len()..]))
                } else {
                    Action::Keep
                }
            },
            |_| Ok(()),
        )
    }

    fn set_modified(&self, path: &str, modified_ms: i64) -> io::Result<()> {
        self.check_writable()?;
        let path = entry_path(path)?;
        if self.stat_node(&path)?.is_none() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("{path} doesn't exist"),
            ));
        }
        let when = formats::zip_time(modified_ms);
        self.rewrite(
            |name| {
                if name.trim_end_matches('/') == path {
                    Action::Touch(when)
                } else {
                    Action::Keep
                }
            },
            |_| Ok(()),
        )
    }
}
