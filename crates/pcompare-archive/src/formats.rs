//! Listing and reading the individual archive formats. Everything here works
//! on a local archive file; [`crate::archive::ArchiveFs`] decides when to
//! (re)index and where cache files go.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufReader, Read, Seek, SeekFrom};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use chrono::{Datelike, Local, NaiveDate, TimeZone, Timelike};
use pcompare_core::folder::{attr, EntryMeta};

use crate::tree::{Loc, Tree};
use crate::ArchiveKind;

pub(crate) fn other(e: impl std::fmt::Display) -> io::Error {
    io::Error::other(e.to_string())
}

fn system_ms(t: SystemTime) -> Option<i64> {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => Some(d.as_millis() as i64),
        Err(e) => Some(-(e.duration().as_millis() as i64)),
    }
}

// ---------- zip ----------

/// A zip DOS timestamp is local wall-clock time.
fn dos_ms(t: zip::DateTime) -> Option<i64> {
    let date = NaiveDate::from_ymd_opt(t.year().into(), t.month().into(), t.day().into())?;
    let naive = date.and_hms_opt(t.hour().into(), t.minute().into(), t.second().into())?;
    Local
        .from_local_datetime(&naive)
        .earliest()
        .map(|d| d.timestamp_millis())
}

/// Local time `ms` as a zip timestamp (zip can't store years before 1980).
pub(crate) fn zip_time(ms: i64) -> zip::DateTime {
    let fallback = zip::DateTime::default();
    let Some(t) = Local.timestamp_millis_opt(ms).earliest() else {
        return fallback;
    };
    zip::DateTime::from_date_and_time(
        t.year().clamp(1980, 2107) as u16,
        t.month() as u8,
        t.day() as u8,
        t.hour() as u8,
        t.minute() as u8,
        t.second() as u8,
    )
    .unwrap_or(fallback)
}

pub(crate) fn zip_index(file: &Path) -> io::Result<(Tree, zip::ZipArchive<File>)> {
    let mut zip = zip::ZipArchive::new(File::open(file)?).map_err(other)?;
    let mut tree = Tree::default();
    for i in 0..zip.len() {
        let f = zip.by_index_raw(i).map_err(other)?;
        // The extended timestamp (Info-ZIP, 7-Zip) is UTC and exact; the
        // DOS one is local time in 2 s steps.
        let utc = f.extra_data_fields().find_map(|x| match x {
            zip::extra_fields::ExtraField::ExtendedTimestamp(t) => t.mod_time(),
            _ => None,
        });
        let modified_ms = utc
            .map(|s| i64::from(s) * 1000)
            .or_else(|| f.last_modified().and_then(dos_ms));
        let meta = EntryMeta {
            size: if f.is_dir() { 0 } else { f.size() },
            modified_ms,
            is_dir: f.is_dir(),
            mode: f.unix_mode().map(|m| m & 0o7777),
            ..Default::default()
        };
        let crc = (!f.is_dir()).then(|| f.crc32());
        let name = f.name().to_owned();
        tree.insert(
            &name,
            meta,
            crc,
            if f.is_dir() { Loc::None } else { Loc::Zip(i) },
        );
    }
    Ok((tree, zip))
}

/// How to stream one zip entry without the archive borrowed: where its data
/// starts, how long it is and how it is compressed.
pub(crate) enum ZipPlan {
    Stored {
        start: u64,
        len: u64,
    },
    Deflated {
        start: u64,
        len: u64,
    },
    /// Anything else is decoded by the zip crate itself.
    Other,
}

pub(crate) fn zip_plan(zip: &mut zip::ZipArchive<File>, index: usize) -> io::Result<ZipPlan> {
    let f = zip.by_index_raw(index).map_err(other)?;
    if f.encrypted() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "encrypted zip entries aren't supported",
        ));
    }
    let (Some(start), len) = (f.data_start(), f.compressed_size()) else {
        return Ok(ZipPlan::Other);
    };
    Ok(match f.compression() {
        zip::CompressionMethod::Stored => ZipPlan::Stored { start, len },
        zip::CompressionMethod::Deflated => ZipPlan::Deflated { start, len },
        _ => ZipPlan::Other,
    })
}

/// A fresh handle on `file` positioned at `start`, limited to `len` bytes.
pub(crate) fn slice(file: &Path, start: u64, len: u64) -> io::Result<io::Take<BufReader<File>>> {
    let mut f = File::open(file)?;
    f.seek(SeekFrom::Start(start))?;
    Ok(BufReader::with_capacity(128 * 1024, f).take(len))
}

/// Verifies a stream against the CRC-32 its header promised, so a damaged
/// archive surfaces as an error instead of a silently different file.
pub(crate) struct CrcCheck<R> {
    inner: R,
    hasher: crc32fast::Hasher,
    expected: u32,
    name: String,
}

impl<R> CrcCheck<R> {
    pub fn new(inner: R, expected: u32, name: &str) -> Self {
        Self {
            inner,
            hasher: crc32fast::Hasher::new(),
            expected,
            name: name.to_owned(),
        }
    }
}

impl<R: Read> Read for CrcCheck<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        if n == 0 && !buf.is_empty() {
            let got = self.hasher.clone().finalize();
            if got != self.expected {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{}: checksum mismatch, the archive is damaged", self.name),
                ));
            }
        }
        self.hasher.update(&buf[..n]);
        Ok(n)
    }
}

// ---------- tar ----------

/// The decompressing reader for a compressed tar or a single compressed file.
pub(crate) fn decoder(kind: ArchiveKind, src: impl Read + Send + 'static) -> Box<dyn Read + Send> {
    match kind {
        ArchiveKind::TarGz | ArchiveKind::Gz => {
            Box::new(flate2::read::MultiGzDecoder::new(BufReader::new(src)))
        }
        ArchiveKind::TarBz2 | ArchiveKind::Bz2 => {
            Box::new(bzip2::read::MultiBzDecoder::new(BufReader::new(src)))
        }
        ArchiveKind::TarXz | ArchiveKind::Xz => {
            Box::new(lzma_rust2::XzReader::new(BufReader::new(src), true))
        }
        _ => Box::new(src),
    }
}

/// Lists a plain (uncompressed) tar; entries point at their data offsets.
pub(crate) fn tar_index(tar_file: &Path) -> io::Result<Tree> {
    let mut archive = tar::Archive::new(File::open(tar_file)?);
    let mut tree = Tree::default();
    for entry in archive.entries_with_seek()? {
        let entry = entry?;
        let h = entry.header();
        let kind = h.entry_type();
        let path = entry.path()?.to_string_lossy().into_owned();
        let modified_ms = h.mtime().ok().map(|s| s as i64 * 1000);
        let mode = h.mode().ok().map(|m| m & 0o7777);
        let mut meta = EntryMeta {
            modified_ms,
            mode,
            ..Default::default()
        };
        let loc = if kind.is_dir() {
            meta.is_dir = true;
            Loc::None
        } else if kind.is_file() || kind == tar::EntryType::Continuous {
            meta.size = entry.size();
            Loc::Tar(entry.raw_file_position())
        } else if kind.is_symlink() || kind.is_hard_link() {
            // Links carry no data of their own: compared by their target.
            let target = entry.link_name()?.map(|p| p.to_string_lossy().into_owned());
            meta.link_target = Some(target.unwrap_or_default());
            Loc::None
        } else {
            // Devices, FIFOs and extension headers aren't files.
            continue;
        };
        tree.insert(&path, meta, None, loc);
    }
    Ok(tree)
}

// ---------- single compressed file ----------

/// The name of the file inside `name.gz` (`.bz2`, `.xz`).
pub(crate) fn single_name(archive_name: &str) -> String {
    let lower = archive_name.to_ascii_lowercase();
    for ext in [".gz", ".bz2", ".xz"] {
        if lower.ends_with(ext) && archive_name.len() > ext.len() {
            return archive_name[..archive_name.len() - ext.len()].to_owned();
        }
    }
    format!("{archive_name}.out")
}

/// Lists the one file in a `.gz` / `.bz2` / `.xz`. The stream is decoded
/// once to measure it (gzip's own size field wraps at 4 GiB and only covers
/// the last member); gzip's header supplies the date when it has one.
pub(crate) fn single_index(
    file: &Path,
    kind: ArchiveKind,
    name: &str,
    archive_mtime: Option<i64>,
) -> io::Result<Tree> {
    let mut meta = EntryMeta {
        modified_ms: archive_mtime,
        ..Default::default()
    };
    if kind == ArchiveKind::Gz {
        let mut head = [0u8; 8];
        File::open(file)?.read_exact(&mut head)?;
        if head[..2] != [0x1f, 0x8b] {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "not a gzip file",
            ));
        }
        let mtime = u32::from_le_bytes([head[4], head[5], head[6], head[7]]);
        if mtime != 0 {
            meta.modified_ms = Some(i64::from(mtime) * 1000);
        }
    }
    meta.size = io::copy(&mut decoder(kind, File::open(file)?), &mut io::sink())?;
    let mut tree = Tree::default();
    tree.insert(name, meta, None, Loc::Single);
    Ok(tree)
}

// ---------- 7z ----------

fn seven_err(e: sevenz_rust2::Error) -> io::Error {
    let msg = e.to_string();
    if msg.to_ascii_lowercase().contains("password") {
        io::Error::new(
            io::ErrorKind::Unsupported,
            "encrypted 7z archives aren't supported",
        )
    } else {
        other(msg)
    }
}

pub(crate) fn sevenz_index(file: &Path) -> io::Result<Tree> {
    let reader = sevenz_rust2::ArchiveReader::open(file, sevenz_rust2::Password::empty())
        .map_err(seven_err)?;
    let mut tree = Tree::default();
    for (i, f) in reader.archive().files.iter().enumerate() {
        let attrs = f.has_windows_attributes.then_some(f.windows_attributes);
        let meta = EntryMeta {
            size: if f.is_directory { 0 } else { f.size },
            modified_ms: f
                .has_last_modified_date
                .then(|| system_ms(SystemTime::from(f.last_modified_date)))
                .flatten(),
            is_dir: f.is_directory,
            attributes: attrs.map(|a| a & attr::DOS_MASK),
            // p7zip keeps Unix permissions in the high word.
            mode: attrs
                .filter(|a| a & 0x8000 != 0)
                .map(|a| (a >> 16) & 0o7777),
            ..Default::default()
        };
        let crc = (f.has_crc && !f.is_directory).then_some(f.crc as u32);
        let loc = if f.is_directory {
            Loc::None
        } else {
            Loc::SevenZ(i)
        };
        tree.insert(&f.name, meta, crc, loc);
    }
    Ok(tree)
}

/// Decodes every file of a 7z archive into `dir/<index>` in one pass —
/// solid archives can only be read front to back, so this is far cheaper
/// than decoding up to each entry separately.
pub(crate) fn sevenz_extract_all(file: &Path, dir: &Path) -> io::Result<()> {
    let mut reader = sevenz_rust2::ArchiveReader::open(file, sevenz_rust2::Password::empty())
        .map_err(seven_err)?;
    let mut by_name: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, f) in reader.archive().files.iter().enumerate() {
        by_name.entry(f.name.clone()).or_default().push(i);
    }
    let mut seen: HashMap<String, usize> = HashMap::new();
    std::fs::create_dir_all(dir)?;
    let mut failure = None;
    reader
        .for_each_entries(|entry, data| {
            let nth = seen.entry(entry.name.clone()).or_default();
            let index = by_name.get(&entry.name).and_then(|v| v.get(*nth)).copied();
            *nth += 1;
            if entry.is_directory {
                return Ok(true);
            }
            let Some(index) = index else {
                io::copy(data, &mut io::sink())?;
                return Ok(true);
            };
            let dest = dir.join(index.to_string());
            if let Err(e) = crate::write_atomic(&dest, data) {
                failure = Some(e);
                return Ok(false);
            }
            Ok(true)
        })
        .map_err(seven_err)?;
    failure.map_or(Ok(()), Err)
}
