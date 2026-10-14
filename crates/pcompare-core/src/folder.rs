//! Recursive folder comparison.
//!
//! Each directory level is listed on both sides, entries are paired by name,
//! and paired files are classified by quick tests and an optional content pass. Work
//! inside a directory runs on the rayon pool, so content comparison of many
//! files and recursion into sibling folders happen in parallel.
//!
//! Classification follows Beyond Compare's model: cheap *quick tests* (size,
//! timestamp) run on metadata, then an optional *content* pass (binary, CRC
//! or rules-based) reads the files and, by default, overrides the quick result.
//!
//! Both sides go through [`Vfs`], so either side may be a remote store.
//!
//! Entries pair by an alignment key (see [`AlignOptions`]): exact names by
//! default, optionally folding case, Unicode normalization, extensions and
//! wildcard rules, so a pair's two names may differ — the left one is then
//! reported in [`FolderEntry::left_rel_path`].

mod align;
mod pe_version;

pub use align::{AlignOptions, AlignRule};
pub use pe_version::VERSION_MAX_BYTES;

use crate::text::{self, TextDiffOptions};
use crate::textfile;
use crate::vfs::{ListOptions, LocalFs, Vfs};
use crate::{CoreError, Result};
use align::Aligner;
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use rayon::prelude::*;
use regex::{Regex, RegexBuilder};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Instant;

/// How file contents are compared after the quick tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ContentMethod {
    /// Metadata only — the quick tests decide.
    #[default]
    Off,
    /// Byte-for-byte; stops reading at the first difference.
    Binary,
    /// CRC-32 of both files; reads everything but yields checksums to show.
    Crc,
    /// Decodes text and compares it with the text rules, so encoding,
    /// line-ending and ignored-pattern differences count as *similar*.
    /// Binary files fall back to a binary comparison.
    Rules,
    /// Executables (.exe, .dll, …) compare by their version resource: equal
    /// version info is the same even if the bytes differ. Files without one
    /// fall back to a binary comparison.
    Version,
}

/// What a content comparison does with files above
/// [`FolderCompareOptions::large_file_threshold`] instead of the configured
/// [`ContentMethod`] — reading a multi-GB pair as text (or even fully for a
/// CRC) is rarely worth it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum LargeFileMethod {
    /// Byte-by-byte: exact, stops reading at the first difference.
    #[default]
    Binary,
    /// Equal sizes are the same; nothing is read.
    Size,
    /// Equal sizes and timestamps (within tolerance) are the same; nothing is read.
    SizeTimestamp,
}

/// DOS file attribute bits, as reported in [`EntryMeta::attributes`].
pub mod attr {
    pub const READONLY: u32 = 0x1;
    pub const HIDDEN: u32 = 0x2;
    pub const SYSTEM: u32 = 0x4;
    pub const ARCHIVE: u32 = 0x20;
    pub const DOS_MASK: u32 = READONLY | HIDDEN | SYSTEM | ARCHIVE;
    /// What "compare attributes" looks at. Archive is left out: it flips on
    /// every write and every backup, so it would flag nearly everything.
    pub const COMPARED: u32 = READONLY | HIDDEN | SYSTEM;
}

/// Name, size and date filters. Name masks are globs matched against the
/// entry name and its relative path; size/date filters only apply to files.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct FolderFilters {
    /// When non-empty, only files matching one of these are compared.
    pub include_files: Vec<String>,
    pub exclude_files: Vec<String>,
    pub exclude_folders: Vec<String>,
    pub min_size: Option<u64>,
    pub max_size: Option<u64>,
    /// Milliseconds since the Unix epoch.
    pub modified_after_ms: Option<i64>,
    pub modified_before_ms: Option<i64>,
    /// Skip entries with any of these [`attr`] bits. Hidden and system apply
    /// to folders too; read-only and archive to files only.
    pub exclude_attributes: u32,
    /// Skip files with a Unix execute bit set.
    pub exclude_executable: bool,
    /// Keep only files whose text contains this (empty: no filter). Reads up
    /// to [`CONTAINS_MAX_BYTES`]; binary files never contain anything.
    pub include_containing: String,
    /// Skip files whose text contains this.
    pub exclude_containing: String,
    /// The two "containing" filters are regexes rather than plain text.
    pub containing_regex: bool,
    pub containing_match_case: bool,
}

/// Content filters read at most this much of each file.
pub const CONTAINS_MAX_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct FolderCompareOptions {
    /// Quick test: sizes must match.
    pub compare_size: bool,
    /// Quick test: timestamps must match (within tolerance).
    pub compare_timestamp: bool,
    pub content: ContentMethod,
    /// Let the content result replace the quick-test result (Beyond Compare's
    /// default). When off, a pair is the same only if both agree.
    pub override_quick_tests: bool,
    /// Text rules used by [`ContentMethod::Rules`].
    pub rules: TextDiffOptions,
    pub recursive: bool,
    /// Glob patterns excluded everywhere (files and folders).
    pub exclude: Vec<String>,
    pub filters: FolderFilters,
    pub include_hidden: bool,
    /// Pair `Readme.md` with `README.md`. Defaults to on for Windows.
    pub ignore_name_case: bool,
    /// Further alignment overrides: Unicode forms, extensions, wildcard rules.
    pub align: AlignOptions,
    /// Differing attributes (read-only, hidden, system; Unix permission
    /// bits) make an otherwise identical file pair different.
    pub compare_attributes: bool,
    /// Descend into linked folders and compare linked files by their target
    /// (link loops are detected and cut). When off, links are compared as
    /// links: by the path they point to.
    pub follow_links: bool,
    /// Allowed modification-time skew in seconds (FAT stores 2s resolution).
    pub time_tolerance_secs: u64,
    /// Treat timestamps exactly one hour apart as equal (DST shifts on FAT/exFAT).
    pub ignore_dst_shift: bool,
    /// Pairs where either file is larger than this many bytes skip
    /// [`Self::content`] and use [`Self::large_file_method`]. `None`: no limit.
    pub large_file_threshold: Option<u64>,
    pub large_file_method: LargeFileMethod,
}

impl Default for FolderCompareOptions {
    fn default() -> Self {
        Self {
            compare_size: true,
            compare_timestamp: true,
            content: ContentMethod::Off,
            override_quick_tests: true,
            rules: TextDiffOptions::default(),
            recursive: true,
            exclude: Vec::new(),
            filters: FolderFilters::default(),
            include_hidden: true,
            ignore_name_case: cfg!(windows),
            align: AlignOptions::default(),
            compare_attributes: false,
            follow_links: true,
            time_tolerance_secs: 2,
            ignore_dst_shift: false,
            large_file_threshold: None,
            large_file_method: LargeFileMethod::Binary,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum EntryStatus {
    Same,
    /// Bytes differ but a rules-based comparison found only unimportant
    /// differences (encoding, line endings, whitespace, ignored patterns).
    Similar,
    Different,
    LeftOnly,
    RightOnly,
    /// A file on one side and a folder with the same name on the other.
    TypeMismatch,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Side {
    Left,
    Right,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EntryMeta {
    pub size: u64,
    /// Milliseconds since the Unix epoch.
    pub modified_ms: Option<i64>,
    pub is_dir: bool,
    /// DOS [`attr`] bits, where the file system has them. Local Unix files
    /// report read-only and hidden (dot files) here too.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attributes: Option<u32>,
    /// Unix permission bits (`0o7777`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<u32>,
    /// Set when the entry is a symbolic link or NTFS junction: the path it
    /// points to (empty if unreadable).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub link_target: Option<String>,
    /// An id that is equal exactly when the bytes are (a git blob id), on
    /// file systems that know one. Two files that both have one compare by
    /// it without being read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FolderEntry {
    pub name: String,
    pub rel_path: String,
    pub is_dir: bool,
    pub status: EntryStatus,
    /// For differing files, which side carries the newer timestamp.
    pub newer: Option<Side>,
    pub left: Option<EntryMeta>,
    pub right: Option<EntryMeta>,
    pub children: Vec<FolderEntry>,
    pub error: Option<String>,
    /// Which content method produced the status, if contents were read.
    pub content_checked: Option<ContentMethod>,
    /// Left and right CRC-32, when computed.
    pub crc: Option<[u32; 2]>,
    /// The left side's own relative path, when alignment paired it with a
    /// differently named right entry (or it sits below such a folder);
    /// `rel_path` and `name` describe the right side.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub left_rel_path: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FolderStats {
    pub same: usize,
    pub similar: usize,
    pub different: usize,
    pub left_only: usize,
    pub right_only: usize,
    pub errors: usize,
    /// Checksums answered by the [`ChecksumCache`] instead of reading files.
    pub checksums_reused: usize,
}

/// Remembers file checksums between scans so unchanged files need not be
/// read again. Implementations key entries on the path *and* its size and
/// modification time, and return `None` whenever either has changed.
///
/// `side` lets one cache serve both sides of a comparison while keeping
/// their key spaces apart (or skip a side, e.g. a remote store).
pub trait ChecksumCache: Send + Sync {
    fn get(&self, side: Side, path: &str, meta: &EntryMeta) -> Option<u32>;
    fn put(&self, side: Side, path: &str, meta: &EntryMeta, crc: u32);
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FolderCompareResult {
    pub left_root: String,
    pub right_root: String,
    pub entries: Vec<FolderEntry>,
    pub stats: FolderStats,
    pub elapsed_ms: u128,
}

/// Progress of a running comparison, reported after each listed folder.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanProgress {
    /// Entries listed so far, both sides together.
    pub scanned: usize,
    /// Folders listed so far, both sides together.
    pub folders: usize,
    /// File pairs classified so far.
    pub compared: usize,
    /// Relative path of the folder listed last.
    pub current: String,
}

/// One folder's rows, streamed while a comparison runs so a view can fill in
/// before the whole tree is done. A folder present on both sides is reported
/// twice: `complete: false` straight after listing (pairs found, statuses of
/// two-sided rows not known yet), then `complete: true` with its final rows
/// once everything below it is compared. A one-sided folder's rows are final
/// when listed, so it reports once, complete, before its subfolders. Entries
/// carry no `children`: those arrive as their own levels, keyed by the
/// folder's `rel_path`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FolderLevel {
    /// The folder's relative path (right-hand names); `""` is the root.
    pub rel: String,
    pub entries: Vec<FolderEntry>,
    pub complete: bool,
}

/// The error message of a comparison stopped through [`ScanControl::cancel`].
pub const CANCELLED: &str = "Folder comparison cancelled";

/// Optional hooks into a running comparison.
#[derive(Clone, Copy, Default)]
pub struct ScanControl<'a> {
    /// Checksums remembered from earlier scans.
    pub cache: Option<&'a dyn ChecksumCache>,
    /// Set to stop the comparison; it then fails with [`CANCELLED`].
    pub cancel: Option<&'a AtomicBool>,
    /// Called from worker threads after every listed folder; throttle in
    /// the callback if forwarding is expensive.
    pub progress: Option<&'a (dyn Fn(&ScanProgress) + Sync)>,
    /// Called from worker threads with every folder's rows (see
    /// [`FolderLevel`]); batch in the callback if forwarding is expensive.
    pub levels: Option<&'a (dyn Fn(FolderLevel) + Sync)>,
}

struct Ctx<'a> {
    opts: &'a FolderCompareOptions,
    exclude: GlobSet,
    include_files: GlobSet,
    exclude_files: GlobSet,
    exclude_folders: GlobSet,
    include_containing: Option<Regex>,
    exclude_containing: Option<Regex>,
    aligner: Aligner,
    left: &'a dyn Vfs,
    right: &'a dyn Vfs,
    control: ScanControl<'a>,
    /// Canonical paths of the compared folders, when links are followed.
    roots: [Option<String>; 2],
    reused: AtomicUsize,
    scanned: AtomicUsize,
    folders: AtomicUsize,
    compared: AtomicUsize,
}

impl Ctx<'_> {
    fn fs(&self, side: Side) -> &dyn Vfs {
        match side {
            Side::Left => self.left,
            Side::Right => self.right,
        }
    }

    fn check_cancel(&self) -> Result<()> {
        match self.control.cancel {
            Some(flag) if flag.load(Ordering::Relaxed) => Err(CoreError::Other(CANCELLED.into())),
            _ => Ok(()),
        }
    }

    /// Reports a folder's rows to [`ScanControl::levels`], without children.
    fn level<'e>(&self, rel: &str, entries: impl IntoIterator<Item = &'e FolderEntry>, complete: bool) {
        if let Some(report) = self.control.levels {
            let entries = entries
                .into_iter()
                .map(|e| FolderEntry {
                    children: Vec::new(),
                    ..e.clone()
                })
                .collect();
            report(FolderLevel {
                rel: rel.to_owned(),
                entries,
                complete,
            });
        }
    }

    /// For a linked folder that resolves inside its own side's compared
    /// folder, the target relative to that folder. Its contents already
    /// show at the real location, so walking it again would only repeat
    /// them (pnpm `node_modules` and app bundles link like this thousands of
    /// times, multiplying a scan many times over).
    fn link_inside_root(&self, side: Side, item: &Listed, canonical: &Option<String>) -> Option<String> {
        item.meta.link_target.as_ref()?;
        let root = self.roots[Chain::index(side)].as_deref()?;
        let rest = canonical.as_deref()?.strip_prefix(root)?;
        let rest = rest
            .strip_prefix(['/', '\\'])
            .or_else(|| root.ends_with(['/', '\\']).then_some(rest))?;
        Some(rest.replace('\\', "/"))
    }

    fn listed(&self, rel: &str, count: usize) {
        let scanned = self.scanned.fetch_add(count, Ordering::Relaxed) + count;
        let folders = self.folders.fetch_add(1, Ordering::Relaxed) + 1;
        if let Some(report) = self.control.progress {
            report(&ScanProgress {
                scanned,
                folders,
                compared: self.compared.load(Ordering::Relaxed),
                current: rel.to_owned(),
            });
        }
    }
}

fn build_globset(patterns: &[String], case_insensitive: bool) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for p in patterns.iter().map(|p| p.trim()).filter(|p| !p.is_empty()) {
        let glob = GlobBuilder::new(p)
            .case_insensitive(case_insensitive)
            .build()
            .map_err(|e| CoreError::BadPattern {
                pattern: p.to_owned(),
                message: e.to_string(),
            })?;
        builder.add(glob);
    }
    builder.build().map_err(|e| CoreError::BadPattern {
        pattern: patterns.join(", "),
        message: e.to_string(),
    })
}

/// Compiles a "files containing" filter; `None` when it's empty.
fn containing_regex(text: &str, filters: &FolderFilters) -> Result<Option<Regex>> {
    if text.trim().is_empty() {
        return Ok(None);
    }
    let pattern = if filters.containing_regex {
        text.to_owned()
    } else {
        regex::escape(text)
    };
    RegexBuilder::new(&pattern)
        .case_insensitive(!filters.containing_match_case)
        .build()
        .map(Some)
        .map_err(|e| CoreError::BadPattern {
            pattern: text.to_owned(),
            message: e.to_string(),
        })
}

struct Listed {
    name: String,
    meta: EntryMeta,
}

/// Whether an entry's attributes put it out of the comparison.
fn attributes_excluded(m: &EntryMeta, filters: &FolderFilters) -> bool {
    let mask = if m.is_dir {
        filters.exclude_attributes & (attr::HIDDEN | attr::SYSTEM)
    } else {
        filters.exclude_attributes
    };
    m.attributes.is_some_and(|a| a & mask != 0)
        || (!m.is_dir && filters.exclude_executable && m.mode.is_some_and(|mode| mode & 0o111 != 0))
}

/// Lists one directory, applying hidden/exclude/attribute filters. Keys are
/// the alignment keys of [`Aligner::key`].
fn list_dir(
    fs: &dyn Vfs,
    side: Side,
    dir: &str,
    rel: &str,
    ctx: &Ctx,
) -> Result<BTreeMap<String, Listed>> {
    ctx.check_cancel()?;
    let mut items = fs.read_dir_with(
        dir,
        ListOptions {
            follow_links: ctx.opts.follow_links,
        },
    )?;
    ctx.listed(rel, items.len());
    // Sorted, so which of two colliding names keeps the folded key is stable.
    items.sort_unstable_by(|a, b| a.name.cmp(&b.name));
    let mut out = BTreeMap::new();
    for entry in items {
        if !ctx.opts.include_hidden && entry.hidden {
            continue;
        }
        let rel_path = join_rel(rel, &entry.name);
        let hit = |set: &GlobSet| set.is_match(&entry.name) || set.is_match(&rel_path);
        let excluded = hit(&ctx.exclude)
            || if entry.meta.is_dir {
                hit(&ctx.exclude_folders)
            } else {
                hit(&ctx.exclude_files)
                    || (!ctx.include_files.is_empty() && !hit(&ctx.include_files))
            };
        if excluded || attributes_excluded(&entry.meta, &ctx.opts.filters) {
            continue;
        }
        let mut key = ctx.aligner.key(side, &entry.name, entry.meta.is_dir);
        if out.contains_key(&key) {
            // Two names fold alike on one side (`a.htm` and `a.html` in one
            // group): the later one pairs only by its exact name.
            key = Aligner::exact_key(&entry.name);
        }
        out.insert(
            key,
            Listed {
                name: entry.name,
                meta: entry.meta,
            },
        );
    }
    Ok(out)
}

/// Whether a file's text matches `re`, reading at most [`CONTAINS_MAX_BYTES`].
fn file_contains(fs: &dyn Vfs, path: &str, re: &Regex) -> Result<bool> {
    let mut buf = Vec::new();
    fs.open(path)?
        .take(CONTAINS_MAX_BYTES)
        .read_to_end(&mut buf)
        .map_err(|e| CoreError::io(path, e))?;
    if textfile::is_probably_binary(&buf) {
        return Ok(false);
    }
    Ok(re.is_match(&textfile::decode(&buf).0))
}

/// The "files containing" filters for one side of a pair. Unreadable files
/// stay, so the comparison can report the error.
fn content_passes(ctx: &Ctx, side: Side, path: &str) -> bool {
    let fs = ctx.fs(side);
    if let Some(re) = &ctx.include_containing {
        if matches!(file_contains(fs, path, re), Ok(false)) {
            return false;
        }
    }
    if let Some(re) = &ctx.exclude_containing {
        if matches!(file_contains(fs, path, re), Ok(true)) {
            return false;
        }
    }
    true
}

fn join_rel(rel: &str, name: &str) -> String {
    if rel.is_empty() {
        name.to_owned()
    } else {
        format!("{rel}/{name}")
    }
}

/// Fills `buf` as far as possible; returns bytes read (short only at EOF).
fn read_full(reader: &mut (impl Read + ?Sized), buf: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(filled)
}

/// Streams both readers in lock-step; stops at the first differing chunk.
pub fn streams_equal(
    lf: &mut dyn Read,
    left_name: &str,
    rf: &mut dyn Read,
    right_name: &str,
) -> Result<bool> {
    const CHUNK: usize = 128 * 1024;
    let (mut lb, mut rb) = (vec![0u8; CHUNK], vec![0u8; CHUNK]);
    loop {
        let ln = read_full(lf, &mut lb).map_err(|e| CoreError::io(left_name, e))?;
        let rn = read_full(rf, &mut rb).map_err(|e| CoreError::io(right_name, e))?;
        if ln != rn || lb[..ln] != rb[..rn] {
            return Ok(false);
        }
        if ln == 0 {
            return Ok(true);
        }
    }
}

/// Byte-for-byte comparison of two local files.
pub fn files_equal(left: &Path, right: &Path) -> Result<bool> {
    let (l, r) = (left.to_string_lossy(), right.to_string_lossy());
    let (mut lf, mut rf) = (LocalFs.open(&l)?, LocalFs.open(&r)?);
    streams_equal(&mut lf, &l, &mut rf, &r)
}

/// Decides whether two modification times should be considered "the same".
///
/// Called for every file pair under `SizeAndTimestamp`, and to decide which
/// side is newer when files differ.
pub fn timestamps_match(
    left_ms: Option<i64>,
    right_ms: Option<i64>,
    opts: &FolderCompareOptions,
) -> bool {
    // A side without a timestamp (FTP without MLSD, S3 prefixes) only matches
    // another missing one: unknown vs known is treated as a difference, which
    // keeps a changed same-size file from passing as Same.
    let (Some(l), Some(r)) = (left_ms, right_ms) else {
        return left_ms.is_none() && right_ms.is_none();
    };
    let tolerance = opts.time_tolerance_secs.saturating_mul(1000);
    let diff = l.abs_diff(r);
    diff <= tolerance || (opts.ignore_dst_shift && diff.abs_diff(HOUR_MS) <= tolerance)
}

/// FAT/exFAT store local time, so a DST change shifts every file by an hour.
const HOUR_MS: u64 = 3_600_000;

fn newer_side(l: &EntryMeta, r: &EntryMeta, opts: &FolderCompareOptions) -> Option<Side> {
    match (l.modified_ms, r.modified_ms) {
        (Some(a), Some(b)) if !timestamps_match(Some(a), Some(b), opts) => {
            Some(if a > b { Side::Left } else { Side::Right })
        }
        _ => None,
    }
}

/// Outcome of reading two files' contents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContentVerdict {
    pub status: EntryStatus,
    pub crc: Option<[u32; 2]>,
}

/// Files larger than this are never decoded for a rules-based comparison;
/// they fall back to binary so a stray multi-GB log can't exhaust memory.
pub const RULES_MAX_BYTES: u64 = 32 * 1024 * 1024;

fn read_all(fs: &dyn Vfs, path: &str) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    fs.open(path)?
        .read_to_end(&mut buf)
        .map_err(|e| CoreError::io(path, e))?;
    Ok(buf)
}

fn crc32(fs: &dyn Vfs, path: &str) -> Result<u32> {
    let mut reader = fs.open(path)?;
    let mut hasher = crc32fast::Hasher::new();
    let mut buf = vec![0u8; 128 * 1024];
    loop {
        let n = read_full(&mut reader, &mut buf).map_err(|e| CoreError::io(path, e))?;
        if n == 0 {
            return Ok(hasher.finalize());
        }
        hasher.update(&buf[..n]);
    }
}

/// The last component of a local or remote path.
fn file_name(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

/// Compares two files' contents with `method`. Sizes are passed in so the
/// binary paths can short-circuit without opening anything.
#[allow(clippy::too_many_arguments)]
pub fn compare_contents(
    left_fs: &dyn Vfs,
    left: &str,
    left_size: u64,
    right_fs: &dyn Vfs,
    right: &str,
    right_size: u64,
    method: ContentMethod,
    rules: &TextDiffOptions,
) -> Result<ContentVerdict> {
    let same_or_diff = |equal: bool| {
        if equal {
            EntryStatus::Same
        } else {
            EntryStatus::Different
        }
    };
    let binary = || -> Result<ContentVerdict> {
        let equal = left_size == right_size && {
            let (mut lf, mut rf) = (left_fs.open(left)?, right_fs.open(right)?);
            streams_equal(&mut lf, left, &mut rf, right)?
        };
        Ok(ContentVerdict {
            status: same_or_diff(equal),
            crc: None,
        })
    };
    match method {
        ContentMethod::Off | ContentMethod::Binary => binary(),
        ContentMethod::Crc => {
            let crc = [crc32(left_fs, left)?, crc32(right_fs, right)?];
            Ok(ContentVerdict {
                status: same_or_diff(left_size == right_size && crc[0] == crc[1]),
                crc: Some(crc),
            })
        }
        ContentMethod::Rules => {
            // Binary first: identical bytes are the common case and cheap.
            let quick = binary()?;
            if quick.status == EntryStatus::Same || left_size.max(right_size) > RULES_MAX_BYTES {
                return Ok(quick);
            }
            let (lb, rb) = (read_all(left_fs, left)?, read_all(right_fs, right)?);
            if textfile::is_probably_binary(&lb) || textfile::is_probably_binary(&rb) {
                return Ok(quick);
            }
            let (lt, rt) = (textfile::decode(&lb).0, textfile::decode(&rb).0);
            // File names pick a language grammar (comments, strings) per side.
            let diff = text::diff_named_text(
                &lt,
                Some(file_name(left)),
                &rt,
                Some(file_name(right)),
                rules,
            )?;
            let status = if diff.hunks.is_empty() {
                EntryStatus::Similar
            } else {
                EntryStatus::Different
            };
            Ok(ContentVerdict { status, crc: None })
        }
        ContentMethod::Version => {
            let l = pe_version::read(left_fs, left, left_size)?;
            match (l, pe_version::read(right_fs, right, right_size)?) {
                (Some(l), Some(r)) => Ok(ContentVerdict {
                    status: same_or_diff(l == r),
                    crc: None,
                }),
                _ => binary(),
            }
        }
    }
}

/// Quick tests on metadata alone.
fn quick_tests_pass(l: &EntryMeta, r: &EntryMeta, opts: &FolderCompareOptions) -> bool {
    (!opts.compare_size || l.size == r.size)
        && (!opts.compare_timestamp || timestamps_match(l.modified_ms, r.modified_ms, opts))
}

/// The content method a pair is compared with: [`FolderCompareOptions::content`],
/// or the large-file fallback when either file exceeds the threshold. `None`
/// means the fallback is metadata-only (size, or size and timestamp).
fn content_method_for(
    l: &EntryMeta,
    r: &EntryMeta,
    opts: &FolderCompareOptions,
) -> Option<ContentMethod> {
    let large = opts
        .large_file_threshold
        .is_some_and(|limit| l.size.max(r.size) > limit);
    if !large || opts.content == ContentMethod::Off {
        return Some(opts.content);
    }
    match opts.large_file_method {
        LargeFileMethod::Binary => Some(ContentMethod::Binary),
        LargeFileMethod::Size | LargeFileMethod::SizeTimestamp => None,
    }
}

/// CRC-32 of one side, from the cache when it still describes the file.
fn cached_crc(ctx: &Ctx, side: Side, path: &str, meta: &EntryMeta) -> Result<u32> {
    if let Some(hit) = ctx.control.cache.and_then(|c| c.get(side, path, meta)) {
        ctx.reused.fetch_add(1, Ordering::Relaxed);
        return Ok(hit);
    }
    let crc = crc32(ctx.fs(side), path)?;
    if let Some(cache) = ctx.control.cache {
        cache.put(side, path, meta, crc);
    }
    Ok(crc)
}

/// Cached checksums of both sides, without reading anything.
fn cached_pair(ctx: &Ctx, lp: &str, rp: &str, l: &EntryMeta, r: &EntryMeta) -> Option<[u32; 2]> {
    let cache = ctx.control.cache?;
    let pair = [
        cache.get(Side::Left, lp, l)?,
        cache.get(Side::Right, rp, r)?,
    ];
    ctx.reused.fetch_add(2, Ordering::Relaxed);
    Some(pair)
}

fn compare_file_pair(
    lp: &str,
    rp: &str,
    l: &EntryMeta,
    r: &EntryMeta,
    ctx: &Ctx,
) -> Result<ContentVerdict> {
    let opts = ctx.opts;
    ctx.check_cancel()?;
    ctx.compared.fetch_add(1, Ordering::Relaxed);
    if !opts.follow_links && (l.link_target.is_some() || r.link_target.is_some()) {
        // Links listed as themselves are the same when they point to the same place.
        let same = l.link_target.is_some() && l.link_target == r.link_target;
        return Ok(ContentVerdict {
            status: if same {
                EntryStatus::Same
            } else {
                EntryStatus::Different
            },
            crc: None,
        });
    }
    if let (Some(a), Some(b)) = (&l.content_id, &r.content_id) {
        // Equal ids prove equal bytes. Different ones prove different bytes,
        // which only a rules or version compare may still call similar/same.
        if a == b || !matches!(opts.content, ContentMethod::Rules | ContentMethod::Version) {
            let status = if a == b { EntryStatus::Same } else { EntryStatus::Different };
            return Ok(ContentVerdict { status, crc: None });
        }
    }
    let quick_same = quick_tests_pass(l, r, opts);
    let quick = ContentVerdict {
        status: if quick_same {
            EntryStatus::Same
        } else {
            EntryStatus::Different
        },
        crc: None,
    };
    // Without override, a failed quick test is final — no need to read anything.
    if opts.content == ContentMethod::Off || (!quick_same && !opts.override_quick_tests) {
        return Ok(quick);
    }
    let diff = |crc| ContentVerdict {
        status: EntryStatus::Different,
        crc,
    };
    let Some(content) = content_method_for(l, r, opts) else {
        // Too large to read: the configured metadata fallback decides.
        let same = l.size == r.size
            && (opts.large_file_method != LargeFileMethod::SizeTimestamp
                || timestamps_match(l.modified_ms, r.modified_ms, opts));
        return Ok(ContentVerdict {
            status: if same {
                EntryStatus::Same
            } else {
                EntryStatus::Different
            },
            crc: None,
        });
    };
    match content {
        ContentMethod::Crc if ctx.control.cache.is_some() => {
            let crc = [
                cached_crc(ctx, Side::Left, lp, l)?,
                cached_crc(ctx, Side::Right, rp, r)?,
            ];
            let same = l.size == r.size && crc[0] == crc[1];
            Ok(ContentVerdict {
                status: if same {
                    EntryStatus::Same
                } else {
                    EntryStatus::Different
                },
                crc: Some(crc),
            })
        }
        // Different checksums prove different bytes; equal ones prove nothing,
        // so a byte-for-byte compare still has to read both files.
        ContentMethod::Binary => match cached_pair(ctx, lp, rp, l, r) {
            Some(crc) if crc[0] != crc[1] => Ok(diff(Some(crc))),
            _ => compare_contents(
                ctx.left,
                lp,
                l.size,
                ctx.right,
                rp,
                r.size,
                content,
                &opts.rules,
            ),
        },
        _ => compare_contents(
            ctx.left,
            lp,
            l.size,
            ctx.right,
            rp,
            r.size,
            content,
            &opts.rules,
        ),
    }
}

/// Whether a file pair survives the size/date filters. `None` is the side
/// where the file doesn't exist (an orphan).
pub fn pair_passes(
    left: Option<&EntryMeta>,
    right: Option<&EntryMeta>,
    filters: &FolderFilters,
) -> bool {
    let passes = |m: &EntryMeta| {
        // An unknown timestamp (some remote stores) can't be shown to be out
        // of range, so date filters keep it rather than hide it.
        let time_ok = |bound: Option<i64>, ok: fn(i64, i64) -> bool| {
            bound.is_none_or(|b| m.modified_ms.is_none_or(|t| ok(t, b)))
        };
        filters.min_size.is_none_or(|min| m.size >= min)
            && filters.max_size.is_none_or(|max| m.size <= max)
            && time_ok(filters.modified_after_ms, |t, b| t >= b)
            && time_ok(filters.modified_before_ms, |t, b| t <= b)
    };
    // Either side matching keeps the pair, so a file that just crossed a
    // threshold (e.g. grew past max_size) is listed instead of vanishing.
    left.is_some_and(passes) || right.is_some_and(passes)
}

fn error_entry(name: String, rel_path: String, is_dir: bool, err: CoreError) -> FolderEntry {
    FolderEntry {
        name,
        rel_path,
        is_dir,
        status: EntryStatus::Error,
        newer: None,
        left: None,
        right: None,
        children: Vec::new(),
        error: Some(err.to_string()),
        content_checked: None,
        crc: None,
        left_rel_path: None,
    }
}

/// Whether two files' attributes differ in what "compare attributes"
/// checks: the [`attr::COMPARED`] bits and Unix permission bits. Sides that
/// don't report attributes (remote stores) never differ.
pub fn attributes_differ(l: &EntryMeta, r: &EntryMeta) -> bool {
    let dos =
        matches!((l.attributes, r.attributes), (Some(a), Some(b)) if (a ^ b) & attr::COMPARED != 0);
    let unix = matches!((l.mode, r.mode), (Some(a), Some(b)) if a != b);
    dos || unix
}

/// The folders being compared above the current one, by canonical path per
/// side, so a link back to one of them is caught instead of recursed into.
struct Chain<'a> {
    canonical: [Option<String>; 2],
    parent: Option<&'a Chain<'a>>,
}

impl Chain<'_> {
    fn index(side: Side) -> usize {
        match side {
            Side::Left => 0,
            Side::Right => 1,
        }
    }

    /// Canonical path of the subfolder `name` (at `path`): links resolve on
    /// the file system, plain folders extend their parent's path.
    fn child(&self, ctx: &Ctx, side: Side, path: &str, item: &Listed) -> Option<String> {
        if !ctx.opts.follow_links {
            return None;
        }
        if item.meta.link_target.is_some() {
            ctx.fs(side).canonical(path)
        } else {
            let parent = self.canonical[Self::index(side)].as_deref()?;
            Some(ctx.fs(side).join(parent, &item.name))
        }
    }

    /// Whether `canonical` is this folder or one above it on `side`.
    fn contains(&self, side: Side, canonical: &str) -> bool {
        let mut link = Some(self);
        while let Some(c) = link {
            if c.canonical[Self::index(side)].as_deref() == Some(canonical) {
                return true;
            }
            link = c.parent;
        }
        false
    }

    /// Whether descending into `item` would revisit a folder above it.
    fn loops(&self, side: Side, item: &Listed, canonical: &Option<String>) -> bool {
        item.meta.link_target.is_some()
            && canonical.as_deref().is_some_and(|c| self.contains(side, c))
    }
}

const LOOP_MESSAGE: &str = "Not expanded: a link here points back to a folder that contains it";

fn inside_message(target: &str) -> String {
    format!("Not expanded: a link to {target}, which is listed at its own place in this comparison")
}

/// A one-sided subfolder still to walk: its path and canonical path.
struct Descend {
    path: String,
    canonical: Option<String>,
}

/// Lists an orphaned folder so its contents are visible in the tree. `rel`
/// is the tree's relative path, `side_rel` the side's own.
fn orphan_tree(
    dir: &str,
    rel: &str,
    side_rel: &str,
    side: Side,
    chain: &Chain,
    ctx: &Ctx,
) -> Vec<FolderEntry> {
    let fs = ctx.fs(side);
    let listed = match list_dir(fs, side, dir, side_rel, ctx) {
        Ok(l) => l,
        Err(e) => {
            let entries = vec![error_entry("<unreadable>".into(), rel.into(), false, e)];
            ctx.level(rel, &entries, true);
            return entries;
        }
    };
    let passes = |m: &EntryMeta| match side {
        Side::Left => pair_passes(Some(m), None, &ctx.opts.filters),
        Side::Right => pair_passes(None, Some(m), &ctx.opts.filters),
    };
    let orphan = |item: &Listed| {
        let rel_path = join_rel(rel, &item.name);
        let own_rel = join_rel(side_rel, &item.name);
        let (left, right, status) = match side {
            Side::Left => (Some(item.meta.clone()), None, EntryStatus::LeftOnly),
            Side::Right => (None, Some(item.meta.clone()), EntryStatus::RightOnly),
        };
        FolderEntry {
            is_dir: item.meta.is_dir,
            name: item.name.clone(),
            left_rel_path: (side == Side::Left && own_rel != rel_path).then_some(own_rel),
            rel_path,
            status,
            newer: None,
            left,
            right,
            children: Vec::new(),
            error: None,
            content_checked: None,
            crc: None,
        }
    };
    // One-sided rows are final as soon as they're listed, so the folder
    // reports complete before its subfolders are walked.
    let rows: Vec<(FolderEntry, Option<Descend>)> = listed
        .into_par_iter()
        .filter(|(_, item)| {
            item.meta.is_dir
                || (passes(&item.meta) && content_passes(ctx, side, &fs.join(dir, &item.name)))
        })
        .map(|(_, item)| {
            let mut entry = orphan(&item);
            let mut descend = None;
            if item.meta.is_dir && ctx.opts.recursive {
                let path = fs.join(dir, &item.name);
                let canonical = chain.child(ctx, side, &path, &item);
                if chain.loops(side, &item, &canonical) {
                    entry.error = Some(LOOP_MESSAGE.to_owned());
                } else if let Some(target) = ctx.link_inside_root(side, &item, &canonical) {
                    entry.error = Some(inside_message(&target));
                } else {
                    descend = Some(Descend { path, canonical });
                }
            }
            (entry, descend)
        })
        .collect();
    ctx.level(rel, rows.iter().map(|(e, _)| e), true);
    rows.into_par_iter()
        .map(|(mut entry, descend)| {
            if let Some(Descend { path, canonical }) = descend {
                let mut sub = Chain {
                    canonical: [None, None],
                    parent: Some(chain),
                };
                sub.canonical[Chain::index(side)] = canonical;
                let own_rel = entry.left_rel_path.clone().unwrap_or_else(|| entry.rel_path.clone());
                entry.children = orphan_tree(&path, &entry.rel_path, &own_rel, side, &sub, ctx);
            }
            entry
        })
        .collect()
}

/// Where the two folders of a pair are: their paths on each file system,
/// the tree's relative path (right-hand names) and the left's own.
struct DirPair<'a> {
    ldir: &'a str,
    rdir: &'a str,
    rel: &'a str,
    lrel: &'a str,
}

fn compare_dirs(at: &DirPair, chain: &Chain, ctx: &Ctx) -> Result<Vec<FolderEntry>> {
    let mut left = list_dir(ctx.left, Side::Left, at.ldir, at.lrel, ctx)?;
    let right = list_dir(ctx.right, Side::Right, at.rdir, at.rel, ctx)?;

    let mut pairs: Vec<(Option<Listed>, Option<Listed>)> = Vec::new();
    for (key, r) in right {
        pairs.push((left.remove(&key), Some(r)));
    }
    pairs.extend(left.into_values().map(|l| (Some(l), None)));
    pairs.retain(|(l, r)| {
        let any_dir = l.iter().chain(r.iter()).any(|x| x.meta.is_dir);
        any_dir
            || pair_passes(
                l.as_ref().map(|x| &x.meta),
                r.as_ref().map(|x| &x.meta),
                &ctx.opts.filters,
            )
    });

    if ctx.control.levels.is_some() {
        let shallow: Vec<_> = pairs
            .iter()
            .map(|(l, r)| pair_entry(at, l.as_ref(), r.as_ref()))
            .collect();
        ctx.level(at.rel, &shallow, false);
    }
    let filter_contents = ctx.include_containing.is_some() || ctx.exclude_containing.is_some();
    let mut entries: Vec<FolderEntry> = pairs
        .into_par_iter()
        .filter(|(l, r)| {
            // Either side containing the text keeps the pair, like size/date filters.
            let any_dir = l.iter().chain(r.iter()).any(|x| x.meta.is_dir);
            any_dir
                || !filter_contents
                || l.as_ref().is_some_and(|x| {
                    content_passes(ctx, Side::Left, &ctx.left.join(at.ldir, &x.name))
                })
                || r.as_ref().is_some_and(|x| {
                    content_passes(ctx, Side::Right, &ctx.right.join(at.rdir, &x.name))
                })
        })
        .map(|pair| compare_pair(at, chain, pair, ctx))
        .collect();
    // Folders first, then case-insensitive name order, like Explorer.
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    ctx.level(at.rel, &entries, true);
    Ok(entries)
}

/// A pair's row before anything below it is compared: one-sided rows and
/// type mismatches are final, two-sided ones still read `Same`.
fn pair_entry(at: &DirPair, l: Option<&Listed>, r: Option<&Listed>) -> FolderEntry {
    let name = r.or(l).map(|x| x.name.clone()).unwrap_or_default();
    let rel_path = join_rel(at.rel, &name);
    let (status, is_dir) = match (l, r) {
        (Some(l), None) => (EntryStatus::LeftOnly, l.meta.is_dir),
        (None, Some(r)) => (EntryStatus::RightOnly, r.meta.is_dir),
        (Some(l), Some(r)) if l.meta.is_dir != r.meta.is_dir => (EntryStatus::TypeMismatch, false),
        (Some(l), _) => (EntryStatus::Same, l.meta.is_dir),
        (None, None) => unreachable!("pairs always have at least one side"),
    };
    FolderEntry {
        left_rel_path: l
            .map(|x| join_rel(at.lrel, &x.name))
            .filter(|p| *p != rel_path),
        name,
        rel_path,
        is_dir,
        status,
        newer: None,
        left: l.map(|x| x.meta.clone()),
        right: r.map(|x| x.meta.clone()),
        children: Vec::new(),
        error: None,
        content_checked: None,
        crc: None,
    }
}

fn compare_pair(
    at: &DirPair,
    chain: &Chain,
    pair: (Option<Listed>, Option<Listed>),
    ctx: &Ctx,
) -> FolderEntry {
    let (l, r) = pair;
    let mut entry = pair_entry(at, l.as_ref(), r.as_ref());
    let rel_path = entry.rel_path.clone();
    let left_rel = l
        .as_ref()
        .map(|x| join_rel(at.lrel, &x.name))
        .unwrap_or_default();

    match (&l, &r) {
        (Some(l), None) => {
            if l.meta.is_dir && ctx.opts.recursive {
                let path = ctx.left.join(at.ldir, &l.name);
                let canonical = chain.child(ctx, Side::Left, &path, l);
                if chain.loops(Side::Left, l, &canonical) {
                    entry.error = Some(LOOP_MESSAGE.into());
                } else if let Some(target) = ctx.link_inside_root(Side::Left, l, &canonical) {
                    entry.error = Some(inside_message(&target));
                } else {
                    let sub = Chain {
                        canonical: [canonical, None],
                        parent: Some(chain),
                    };
                    entry.children =
                        orphan_tree(&path, &rel_path, &left_rel, Side::Left, &sub, ctx);
                }
            }
        }
        (None, Some(r)) => {
            if r.meta.is_dir && ctx.opts.recursive {
                let path = ctx.right.join(at.rdir, &r.name);
                let canonical = chain.child(ctx, Side::Right, &path, r);
                if chain.loops(Side::Right, r, &canonical) {
                    entry.error = Some(LOOP_MESSAGE.into());
                } else if let Some(target) = ctx.link_inside_root(Side::Right, r, &canonical) {
                    entry.error = Some(inside_message(&target));
                } else {
                    let sub = Chain {
                        canonical: [None, canonical],
                        parent: Some(chain),
                    };
                    entry.children =
                        orphan_tree(&path, &rel_path, &rel_path, Side::Right, &sub, ctx);
                }
            }
        }
        (Some(l), Some(r)) if l.meta.is_dir != r.meta.is_dir => {}
        (Some(l), Some(r)) if l.meta.is_dir => {
            if ctx.opts.recursive {
                let (ld, rd) = (
                    ctx.left.join(at.ldir, &l.name),
                    ctx.right.join(at.rdir, &r.name),
                );
                let lc = chain.child(ctx, Side::Left, &ld, l);
                let rc = chain.child(ctx, Side::Right, &rd, r);
                let loops = [
                    chain.loops(Side::Left, l, &lc),
                    chain.loops(Side::Right, r, &rc),
                ];
                if loops.iter().any(|&x| x) {
                    // Both sides looping back alike is the same structure; one alone is not.
                    entry.status = if loops == [true, true] {
                        EntryStatus::Same
                    } else {
                        EntryStatus::Error
                    };
                    entry.error = Some(LOOP_MESSAGE.into());
                    return entry;
                }
                // Both sides link inside their own trees: the targets are
                // compared where they really are, so only where they point counts.
                if let (Some(lt), Some(rt)) = (
                    ctx.link_inside_root(Side::Left, l, &lc),
                    ctx.link_inside_root(Side::Right, r, &rc),
                ) {
                    entry.status = if lt == rt {
                        EntryStatus::Same
                    } else {
                        EntryStatus::Different
                    };
                    entry.error = Some(inside_message(&rt));
                    return entry;
                }
                let sub = Chain {
                    canonical: [lc, rc],
                    parent: Some(chain),
                };
                let at = DirPair {
                    ldir: &ld,
                    rdir: &rd,
                    rel: &rel_path,
                    lrel: &left_rel,
                };
                match compare_dirs(&at, &sub, ctx) {
                    Ok(children) => {
                        entry.status = folder_status(&children);
                        entry.children = children;
                    }
                    Err(e) => {
                        entry.status = EntryStatus::Error;
                        entry.error = Some(e.to_string());
                    }
                }
            }
        }
        (Some(l), Some(r)) => {
            let (lp, rp) = (
                ctx.left.join(at.ldir, &l.name),
                ctx.right.join(at.rdir, &r.name),
            );
            match compare_file_pair(&lp, &rp, &l.meta, &r.meta, ctx) {
                Ok(verdict) => {
                    entry.status = verdict.status;
                    entry.crc = verdict.crc;
                    let as_links = !ctx.opts.follow_links
                        && (l.meta.link_target.is_some() || r.meta.link_target.is_some());
                    if ctx.opts.content != ContentMethod::Off && !as_links {
                        // What actually ran: large files may have used the fallback.
                        entry.content_checked = Some(
                            content_method_for(&l.meta, &r.meta, ctx.opts)
                                .unwrap_or(ContentMethod::Off),
                        );
                    }
                    if ctx.opts.compare_attributes
                        && entry.status != EntryStatus::Different
                        && attributes_differ(&l.meta, &r.meta)
                    {
                        entry.status = EntryStatus::Different;
                    }
                    if entry.status != EntryStatus::Same {
                        entry.newer = newer_side(&l.meta, &r.meta, ctx.opts);
                    }
                }
                Err(e) => {
                    entry.status = EntryStatus::Error;
                    entry.error = Some(e.to_string());
                }
            }
        }
        (None, None) => unreachable!("pairs always have at least one side"),
    }
    entry
}

/// A folder is the same when everything inside is, similar when the only
/// deviations are similar files, and different otherwise.
pub fn folder_status(children: &[FolderEntry]) -> EntryStatus {
    if children.iter().all(|c| c.status == EntryStatus::Same) {
        EntryStatus::Same
    } else if children
        .iter()
        .all(|c| matches!(c.status, EntryStatus::Same | EntryStatus::Similar))
    {
        EntryStatus::Similar
    } else {
        EntryStatus::Different
    }
}

fn tally(entries: &[FolderEntry], stats: &mut FolderStats) {
    for e in entries {
        if !e.is_dir {
            match e.status {
                EntryStatus::Same => stats.same += 1,
                EntryStatus::Similar => stats.similar += 1,
                EntryStatus::Different | EntryStatus::TypeMismatch => stats.different += 1,
                EntryStatus::LeftOnly => stats.left_only += 1,
                EntryStatus::RightOnly => stats.right_only += 1,
                EntryStatus::Error => stats.errors += 1,
            }
        } else if e.status == EntryStatus::Error {
            stats.errors += 1;
        }
        tally(&e.children, stats);
    }
}

pub fn compare_folders(
    left: impl AsRef<Path>,
    right: impl AsRef<Path>,
    opts: &FolderCompareOptions,
) -> Result<FolderCompareResult> {
    let (left, right) = (
        left.as_ref().to_string_lossy(),
        right.as_ref().to_string_lossy(),
    );
    compare_folders_with(&LocalFs, &left, &LocalFs, &right, opts)
}

/// Compares two folders that may live on different file systems.
pub fn compare_folders_with(
    left_fs: &dyn Vfs,
    left: &str,
    right_fs: &dyn Vfs,
    right: &str,
    opts: &FolderCompareOptions,
) -> Result<FolderCompareResult> {
    compare_folders_cached(left_fs, left, right_fs, right, opts, None)
}

/// [`compare_folders_with`], reusing and filling a checksum cache so CRC and
/// binary comparisons skip files that haven't changed since the last scan.
pub fn compare_folders_cached(
    left_fs: &dyn Vfs,
    left: &str,
    right_fs: &dyn Vfs,
    right: &str,
    opts: &FolderCompareOptions,
    cache: Option<&dyn ChecksumCache>,
) -> Result<FolderCompareResult> {
    let control = ScanControl {
        cache,
        ..Default::default()
    };
    compare_folders_controlled(left_fs, left, right_fs, right, opts, control)
}

/// [`compare_folders_cached`] with progress reports and cancellation.
pub fn compare_folders_controlled(
    left_fs: &dyn Vfs,
    left: &str,
    right_fs: &dyn Vfs,
    right: &str,
    opts: &FolderCompareOptions,
    control: ScanControl,
) -> Result<FolderCompareResult> {
    let started = Instant::now();
    // Fail fast on bad regexes rather than once per file pair.
    if opts.content == ContentMethod::Rules {
        text::LineNormalizer::new(&opts.rules)?;
    }
    let ci = opts.ignore_name_case;
    let f = &opts.filters;
    let ctx = Ctx {
        opts,
        exclude: build_globset(&opts.exclude, ci)?,
        include_files: build_globset(&f.include_files, ci)?,
        exclude_files: build_globset(&f.exclude_files, ci)?,
        exclude_folders: build_globset(&f.exclude_folders, ci)?,
        include_containing: containing_regex(&f.include_containing, f)?,
        exclude_containing: containing_regex(&f.exclude_containing, f)?,
        aligner: Aligner::new(&opts.align, ci)?,
        left: left_fs,
        right: right_fs,
        control,
        roots: if opts.follow_links {
            [left_fs.canonical(left), right_fs.canonical(right)]
        } else {
            [None, None]
        },
        reused: AtomicUsize::new(0),
        scanned: AtomicUsize::new(0),
        folders: AtomicUsize::new(0),
        compared: AtomicUsize::new(0),
    };
    let root = Chain {
        canonical: ctx.roots.clone(),
        parent: None,
    };
    let at = DirPair {
        ldir: left,
        rdir: right,
        rel: "",
        lrel: "",
    };
    let entries = compare_dirs(&at, &root, &ctx)?;
    // Folders below the top level record a cancellation as their own error.
    ctx.check_cancel()?;
    let mut stats = FolderStats::default();
    tally(&entries, &mut stats);
    stats.checksums_reused = ctx.reused.load(Ordering::Relaxed);
    Ok(FolderCompareResult {
        left_root: left.to_owned(),
        right_root: right.to_owned(),
        entries,
        stats,
        elapsed_ms: started.elapsed().as_millis(),
    })
}

#[cfg(test)]
mod timestamp_tests {
    use super::*;

    const HOUR_MS: i64 = super::HOUR_MS as i64;

    #[test]
    fn tolerance_absorbs_fat_rounding() {
        let opts = FolderCompareOptions::default(); // 2 s tolerance
        let t = 1_700_000_000_000;
        assert!(timestamps_match(Some(t), Some(t), &opts));
        assert!(timestamps_match(Some(t), Some(t + 2_000), &opts));
        assert!(timestamps_match(Some(t + 2_000), Some(t), &opts));
        assert!(!timestamps_match(Some(t), Some(t + 2_001), &opts));
    }

    #[test]
    fn dst_shift_is_exactly_one_hour_within_tolerance() {
        let t = 1_700_000_000_000;
        let mut opts = FolderCompareOptions::default();
        assert!(!timestamps_match(Some(t), Some(t + HOUR_MS), &opts));
        opts.ignore_dst_shift = true;
        assert!(timestamps_match(Some(t), Some(t + HOUR_MS), &opts));
        assert!(timestamps_match(Some(t + HOUR_MS + 1_500), Some(t), &opts));
        assert!(!timestamps_match(Some(t), Some(t + 2 * HOUR_MS), &opts));
    }

    #[test]
    fn missing_timestamps_only_match_each_other() {
        let opts = FolderCompareOptions::default();
        assert!(timestamps_match(None, None, &opts));
        assert!(!timestamps_match(Some(0), None, &opts));
        assert!(!timestamps_match(None, Some(0), &opts));
        // Extreme values must not overflow.
        assert!(!timestamps_match(Some(i64::MIN), Some(i64::MAX), &opts));
    }

    /// In-memory cache that counts the files it had to be told about.
    type CacheKey = (bool, String);
    type CacheVal = (u64, Option<i64>, u32);

    #[derive(Default)]
    struct MemCache(std::sync::Mutex<std::collections::HashMap<CacheKey, CacheVal>>);

    impl ChecksumCache for MemCache {
        fn get(&self, side: Side, path: &str, meta: &EntryMeta) -> Option<u32> {
            let map = self.0.lock().unwrap();
            let &(size, mtime, crc) = map.get(&(side == Side::Left, path.to_owned()))?;
            (size == meta.size && mtime == meta.modified_ms).then_some(crc)
        }
        fn put(&self, side: Side, path: &str, meta: &EntryMeta, crc: u32) {
            let key = (side == Side::Left, path.to_owned());
            self.0
                .lock()
                .unwrap()
                .insert(key, (meta.size, meta.modified_ms, crc));
        }
    }

    #[test]
    fn checksum_cache_skips_unchanged_files() {
        let dir = tempfile::tempdir().unwrap();
        let (l, r) = (dir.path().join("l"), dir.path().join("r"));
        std::fs::create_dir_all(&l).unwrap();
        std::fs::create_dir_all(&r).unwrap();
        for (name, a, b) in [("same.txt", "abc", "abc"), ("diff.txt", "abc", "abd")] {
            std::fs::write(l.join(name), a).unwrap();
            std::fs::write(r.join(name), b).unwrap();
        }
        let opts = FolderCompareOptions {
            content: ContentMethod::Crc,
            ..Default::default()
        };
        let cache = MemCache::default();
        let (ls, rs) = (l.to_string_lossy(), r.to_string_lossy());
        let run = |opts: &FolderCompareOptions| {
            compare_folders_cached(&LocalFs, &ls, &LocalFs, &rs, opts, Some(&cache)).unwrap()
        };

        let first = run(&opts);
        assert_eq!(
            (
                first.stats.same,
                first.stats.different,
                first.stats.checksums_reused
            ),
            (1, 1, 0)
        );
        let second = run(&opts);
        assert_eq!(
            (
                second.stats.same,
                second.stats.different,
                second.stats.checksums_reused
            ),
            (1, 1, 4)
        );

        // Binary mode trusts cached checksums only to prove a difference.
        let binary = run(&FolderCompareOptions {
            content: ContentMethod::Binary,
            ..opts.clone()
        });
        assert_eq!(
            (
                binary.stats.same,
                binary.stats.different,
                binary.stats.checksums_reused
            ),
            (1, 1, 4)
        );

        // A changed file (new size) misses the cache and is read again.
        std::fs::write(r.join("same.txt"), "abcd").unwrap();
        let third = run(&opts);
        assert_eq!(
            (
                third.stats.same,
                third.stats.different,
                third.stats.checksums_reused
            ),
            (0, 2, 3)
        );
    }
}

#[cfg(test)]
mod feature_tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    struct Pair {
        _dir: tempfile::TempDir,
        l: PathBuf,
        r: PathBuf,
    }

    fn pair() -> Pair {
        let dir = tempfile::tempdir().unwrap();
        let (l, r) = (dir.path().join("l"), dir.path().join("r"));
        fs::create_dir_all(&l).unwrap();
        fs::create_dir_all(&r).unwrap();
        Pair { _dir: dir, l, r }
    }

    fn write(root: &Path, rel: &str, contents: &[u8]) {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, contents).unwrap();
    }

    fn opts() -> FolderCompareOptions {
        FolderCompareOptions {
            ignore_name_case: false,
            compare_timestamp: false,
            ..Default::default()
        }
    }

    fn run(p: &Pair, opts: &FolderCompareOptions) -> FolderCompareResult {
        compare_folders(&p.l, &p.r, opts).unwrap()
    }

    fn find<'a>(entries: &'a [FolderEntry], rel: &str) -> Option<&'a FolderEntry> {
        entries.iter().find_map(|e| {
            if e.rel_path == rel {
                Some(e)
            } else {
                find(&e.children, rel)
            }
        })
    }

    fn status(res: &FolderCompareResult, rel: &str) -> EntryStatus {
        find(&res.entries, rel)
            .unwrap_or_else(|| panic!("{rel} missing"))
            .status
    }

    #[test]
    fn name_case_sensitivity_is_a_choice() {
        let p = pair();
        write(&p.l, "Readme.md", b"x");
        write(&p.r, "README.md", b"x");
        let sensitive = run(&p, &opts());
        assert_eq!(status(&sensitive, "Readme.md"), EntryStatus::LeftOnly);
        assert_eq!(status(&sensitive, "README.md"), EntryStatus::RightOnly);

        let insensitive = run(
            &p,
            &FolderCompareOptions {
                ignore_name_case: true,
                ..opts()
            },
        );
        let e = find(&insensitive.entries, "README.md").unwrap();
        assert_eq!(e.status, EntryStatus::Same);
        // The left file keeps its own spelling for opening it.
        assert_eq!(e.left_rel_path.as_deref(), Some("Readme.md"));
    }

    #[test]
    fn unicode_normalization_forms_align() {
        let p = pair();
        write(&p.l, "caf\u{e9}/menu.txt", b"x");
        write(&p.r, "cafe\u{301}/menu.txt", b"x");
        let res = run(&p, &opts());
        assert_eq!(
            res.entries.len(),
            1,
            "NFC and NFD folders pair: {:?}",
            res.entries
        );
        assert_eq!(res.stats.same, 1);
        let off = run(
            &p,
            &FolderCompareOptions {
                align: AlignOptions {
                    unicode: false,
                    ..Default::default()
                },
                ..opts()
            },
        );
        // APFS may hand back either form, so only check they no longer pair.
        assert_eq!(off.stats.same, 0);
    }

    #[test]
    fn alignment_overrides_pair_differently_named_files() {
        let p = pair();
        write(&p.l, "index.htm", b"page");
        write(&p.r, "index.html", b"page");
        write(&p.l, "Apples/2024.csv", b"1");
        write(&p.r, "Bananas/2024.csv", b"22");
        let align = AlignOptions {
            extension_groups: vec!["htm,html".into()],
            rules: vec![AlignRule {
                left: "Apples*".into(),
                right: "Bananas*".into(),
            }],
            ..Default::default()
        };
        let res = run(&p, &FolderCompareOptions { align, ..opts() });
        let page = find(&res.entries, "index.html").unwrap();
        assert_eq!(page.status, EntryStatus::Same);
        assert_eq!(page.left_rel_path.as_deref(), Some("index.htm"));
        // Children of an aligned folder carry the left folder's path too.
        let csv = find(&res.entries, "Bananas/2024.csv").unwrap();
        assert_eq!(csv.status, EntryStatus::Different);
        assert_eq!(csv.left_rel_path.as_deref(), Some("Apples/2024.csv"));
        assert_eq!(status(&res, "Bananas"), EntryStatus::Different);

        let ignore_ext = AlignOptions {
            ignore_extensions: true,
            ..Default::default()
        };
        let res = run(
            &p,
            &FolderCompareOptions {
                align: ignore_ext,
                ..opts()
            },
        );
        assert_eq!(status(&res, "index.html"), EntryStatus::Same);
    }

    #[test]
    fn colliding_folded_names_keep_both_entries() {
        let p = pair();
        write(&p.l, "a.htm", b"1");
        write(&p.l, "a.html", b"2");
        write(&p.r, "a.html", b"2");
        let align = AlignOptions {
            extension_groups: vec!["htm html".into()],
            ..Default::default()
        };
        let res = run(&p, &FolderCompareOptions { align, ..opts() });
        let files = res.stats.same + res.stats.different + res.stats.left_only;
        assert_eq!(files, 2, "{:?}", res.entries);
    }

    #[cfg(unix)]
    fn set_mode(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn attributes_are_read_and_compared() {
        let p = pair();
        write(&p.l, "tool.sh", b"echo");
        write(&p.r, "tool.sh", b"echo");
        set_mode(&p.l.join("tool.sh"), 0o755);
        set_mode(&p.r.join("tool.sh"), 0o644);
        let res = run(&p, &opts());
        let e = find(&res.entries, "tool.sh").unwrap();
        assert_eq!(
            e.status,
            EntryStatus::Same,
            "attributes don't count unless asked"
        );
        assert_eq!(e.left.as_ref().unwrap().mode, Some(0o755));
        assert_eq!(e.right.as_ref().unwrap().attributes, Some(0));

        let res = run(
            &p,
            &FolderCompareOptions {
                compare_attributes: true,
                ..opts()
            },
        );
        assert_eq!(status(&res, "tool.sh"), EntryStatus::Different);

        set_mode(&p.r.join("tool.sh"), 0o444);
        let res = run(&p, &opts());
        assert_eq!(
            find(&res.entries, "tool.sh")
                .unwrap()
                .right
                .as_ref()
                .unwrap()
                .attributes,
            Some(attr::READONLY)
        );
        set_mode(&p.r.join("tool.sh"), 0o644);
    }

    #[test]
    fn attributes_compare_only_known_bits() {
        let m = |attributes, mode| EntryMeta {
            attributes,
            mode,
            ..Default::default()
        };
        assert!(attributes_differ(
            &m(Some(attr::READONLY), None),
            &m(Some(0), None)
        ));
        assert!(!attributes_differ(
            &m(Some(attr::ARCHIVE), None),
            &m(Some(0), None)
        ));
        assert!(!attributes_differ(
            &m(Some(attr::HIDDEN), None),
            &m(None, None)
        ));
        assert!(attributes_differ(
            &m(None, Some(0o755)),
            &m(None, Some(0o644))
        ));
    }

    #[test]
    fn attribute_filters_skip_matching_entries() {
        let f = FolderFilters {
            exclude_attributes: attr::READONLY | attr::HIDDEN,
            exclude_executable: true,
            ..Default::default()
        };
        let file = |attributes, mode| EntryMeta {
            attributes: Some(attributes),
            mode,
            ..Default::default()
        };
        let dir = |attributes| EntryMeta {
            attributes: Some(attributes),
            is_dir: true,
            ..Default::default()
        };
        assert!(attributes_excluded(&file(attr::READONLY, None), &f));
        assert!(attributes_excluded(&file(0, Some(0o700)), &f));
        assert!(!attributes_excluded(&file(attr::ARCHIVE, Some(0o644)), &f));
        // Read-only and execute bits mean little on folders; hidden does.
        assert!(!attributes_excluded(
            &EntryMeta {
                mode: Some(0o755),
                ..dir(attr::READONLY)
            },
            &f
        ));
        assert!(attributes_excluded(&dir(attr::HIDDEN), &f));
    }

    #[cfg(unix)]
    #[test]
    fn attribute_filters_apply_to_listings() {
        let p = pair();
        write(&p.l, "run", b"x");
        write(&p.l, "data", b"x");
        set_mode(&p.l.join("run"), 0o755);
        set_mode(&p.l.join("data"), 0o644);
        let filters = FolderFilters {
            exclude_executable: true,
            ..Default::default()
        };
        let res = run(&p, &FolderCompareOptions { filters, ..opts() });
        let names: Vec<_> = res.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["data"]);
    }

    #[test]
    fn content_filters_read_text_and_skip_binaries() {
        let p = pair();
        write(&p.l, "todo.txt", b"remember the TODO list");
        write(&p.r, "todo.txt", b"done");
        write(&p.l, "plain.txt", b"nothing here");
        write(&p.r, "plain.txt", b"nothing here");
        write(&p.l, "blob.bin", b"TODO\0\0\0\x01\x02binary");
        write(&p.r, "only-right.md", b"a todo item");
        let names = |filters: FolderFilters| {
            let res = run(&p, &FolderCompareOptions { filters, ..opts() });
            let mut v: Vec<String> = res.entries.iter().map(|e| e.name.clone()).collect();
            v.sort();
            v
        };
        // Case-insensitive plain text by default; either side containing it keeps the pair.
        let include = FolderFilters {
            include_containing: "todo".into(),
            ..Default::default()
        };
        assert_eq!(names(include.clone()), ["only-right.md", "todo.txt"]);
        let matching_case = FolderFilters {
            containing_match_case: true,
            ..include
        };
        assert_eq!(names(matching_case), ["only-right.md"]);
        let exclude = FolderFilters {
            exclude_containing: "nothing".into(),
            ..Default::default()
        };
        assert_eq!(names(exclude), ["blob.bin", "only-right.md", "todo.txt"]);
        let regex = FolderFilters {
            include_containing: r"^a \w+ item$".into(),
            containing_regex: true,
            ..Default::default()
        };
        assert_eq!(names(regex), ["only-right.md"]);
        // A plain-text filter is literal, so regex syntax in it is harmless.
        let literal = FolderFilters {
            include_containing: "(unclosed".into(),
            ..Default::default()
        };
        assert!(names(literal).is_empty());
        let bad = FolderFilters {
            include_containing: "(unclosed".into(),
            containing_regex: true,
            ..Default::default()
        };
        let err = compare_folders(
            &p.l,
            &p.r,
            &FolderCompareOptions {
                filters: bad,
                ..opts()
            },
        );
        assert!(matches!(err, Err(CoreError::BadPattern { .. })));
    }

    #[test]
    fn version_method_ignores_rebuild_noise() {
        use super::pe_version::tests::pe_with_version;
        let p = pair();
        write(&p.l, "app.dll", &pe_with_version([2, 0, 0, 1], "App", 0));
        write(&p.r, "app.dll", &pe_with_version([2, 0, 0, 1], "App", 0x5A));
        write(&p.l, "lib.dll", &pe_with_version([2, 0, 0, 1], "App", 0));
        write(&p.r, "lib.dll", &pe_with_version([2, 0, 0, 2], "App", 0));
        write(&p.l, "notes.txt", b"same size");
        write(&p.r, "notes.txt", b"SAME size");
        let version = run(
            &p,
            &FolderCompareOptions {
                content: ContentMethod::Version,
                ..opts()
            },
        );
        assert_eq!(status(&version, "app.dll"), EntryStatus::Same);
        assert_eq!(status(&version, "lib.dll"), EntryStatus::Different);
        // Files without a version resource fall back to binary.
        assert_eq!(status(&version, "notes.txt"), EntryStatus::Different);
        assert_eq!(
            find(&version.entries, "app.dll").unwrap().content_checked,
            Some(ContentMethod::Version)
        );

        let binary = run(
            &p,
            &FolderCompareOptions {
                content: ContentMethod::Binary,
                ..opts()
            },
        );
        assert_eq!(status(&binary, "app.dll"), EntryStatus::Different);
    }

    #[cfg(unix)]
    #[test]
    fn links_are_followed_with_loop_protection() {
        use std::os::unix::fs::symlink;
        let p = pair();
        for root in [&p.l, &p.r] {
            write(root, "real/a.txt", b"a");
            symlink("..", root.join("real/up")).unwrap();
            symlink("real", root.join("alias")).unwrap();
        }
        write(&p.l, "target-1.txt", b"one");
        write(&p.r, "target-2.txt", b"three");
        symlink("target-1.txt", p.l.join("file-link")).unwrap();
        symlink("target-2.txt", p.r.join("file-link")).unwrap();

        let res = run(&p, &opts());
        // A link to a folder already in the tree isn't walked twice; both
        // sides pointing at the same place makes it the same.
        let alias = find(&res.entries, "alias").unwrap();
        assert!(alias.is_dir && alias.children.is_empty());
        assert_eq!(alias.status, EntryStatus::Same);
        assert!(alias.error.as_deref().is_some_and(|e| e.contains("real")));
        assert_eq!(status(&res, "real/a.txt"), EntryStatus::Same);
        // The loop back up is cut.
        let up = find(&res.entries, "real/up").unwrap();
        assert!(up.is_dir && up.children.is_empty());
        assert_eq!(up.status, EntryStatus::Same);
        assert!(up.error.as_deref().is_some_and(|e| e.contains("link")));
        assert!(find(&res.entries, "alias")
            .unwrap()
            .left
            .as_ref()
            .unwrap()
            .link_target
            .is_some());
        // Followed file links compare by their targets' contents.
        assert_eq!(status(&res, "file-link"), EntryStatus::Different);

        let unfollowed = run(
            &p,
            &FolderCompareOptions {
                follow_links: false,
                ..opts()
            },
        );
        let alias = find(&unfollowed.entries, "alias").unwrap();
        assert!(!alias.is_dir && alias.children.is_empty());
        assert_eq!(
            alias.left.as_ref().unwrap().link_target.as_deref(),
            Some("real")
        );
        assert_eq!(alias.status, EntryStatus::Same, "same link target");
        assert_eq!(status(&unfollowed, "file-link"), EntryStatus::Different);
        assert_eq!(status(&unfollowed, "real/up"), EntryStatus::Same);
    }

    #[cfg(unix)]
    #[test]
    fn a_loop_on_one_side_only_is_an_error() {
        use std::os::unix::fs::symlink;
        let p = pair();
        write(&p.l, "d/x.txt", b"x");
        write(&p.r, "d/x.txt", b"x");
        symlink("..", p.l.join("d/back")).unwrap();
        fs::create_dir(p.r.join("d/back")).unwrap();
        let res = run(&p, &opts());
        assert_eq!(status(&res, "d/back"), EntryStatus::Error);
    }

    #[cfg(unix)]
    #[test]
    fn links_inside_the_tree_expand_only_against_a_real_folder() {
        use std::os::unix::fs::symlink;
        let p = pair();
        for root in [&p.l, &p.r] {
            write(root, "real/a.txt", b"a");
            write(root, "other/b.txt", b"b");
        }
        // Left links to a sibling, right has a real folder of that name.
        symlink("real", p.l.join("mixed")).unwrap();
        write(&p.r, "mixed/a.txt", b"a");
        // Both link, to different places.
        symlink("real", p.l.join("moved")).unwrap();
        symlink("other", p.r.join("moved")).unwrap();
        // An orphan link is listed, not walked.
        symlink("other", p.l.join("lonely")).unwrap();

        let res = run(&p, &opts());
        assert_eq!(status(&res, "mixed/a.txt"), EntryStatus::Same);
        assert_eq!(status(&res, "moved"), EntryStatus::Different);
        let lonely = find(&res.entries, "lonely").unwrap();
        assert!(lonely.children.is_empty() && lonely.error.is_some());
        assert_eq!(lonely.status, EntryStatus::LeftOnly);
    }

    #[test]
    fn levels_stream_every_folder_listed_then_complete() {
        let p = pair();
        write(&p.l, "sub/same.txt", b"x");
        write(&p.r, "sub/same.txt", b"x");
        write(&p.l, "sub/diff.txt", b"x");
        write(&p.r, "sub/diff.txt", b"yy");
        write(&p.l, "gone/deep/f.txt", b"x");
        write(&p.r, "new.txt", b"x");
        let (ls, rs) = (p.l.to_string_lossy(), p.r.to_string_lossy());
        let seen = std::sync::Mutex::new(Vec::new());
        let report = |l: FolderLevel| seen.lock().unwrap().push(l);
        let control = ScanControl {
            levels: Some(&report),
            ..Default::default()
        };
        let res =
            compare_folders_controlled(&LocalFs, &ls, &LocalFs, &rs, &opts(), control).unwrap();
        let seen = seen.into_inner().unwrap();
        // Two-sided folders: listed, then completed.
        for rel in ["", "sub"] {
            let at = |complete| {
                seen.iter()
                    .position(|l| l.rel == rel && l.complete == complete)
                    .unwrap_or_else(|| panic!("{rel:?} complete={complete} missing"))
            };
            assert!(at(false) < at(true), "{rel:?} completed before it was listed");
        }
        // One-sided folders: complete straight away, before what's inside.
        assert_eq!(seen.len(), 6);
        assert!(seen.iter().filter(|l| l.rel.starts_with("gone")).all(|l| l.complete));
        // Levels carry no children; a two-sided parent completes after its
        // folders, a one-sided one before them.
        assert!(seen.iter().all(|l| l.entries.iter().all(|e| e.children.is_empty())));
        let done = |rel: &str| seen.iter().position(|l| l.rel == rel && l.complete).unwrap();
        assert!(done("sub") < done("") && done("gone") < done("gone/deep"));
        // The completed levels add up to the returned tree.
        let sub = seen.iter().find(|l| l.rel == "sub" && l.complete).unwrap();
        let statuses: Vec<_> = sub.entries.iter().map(|e| (e.name.as_str(), e.status)).collect();
        assert_eq!(
            statuses,
            [("diff.txt", EntryStatus::Different), ("same.txt", EntryStatus::Same)]
        );
        assert_eq!(res.stats.left_only, 1);
        assert_eq!(res.stats.right_only, 1);
    }

    #[test]
    fn progress_is_reported_and_cancel_stops_the_scan() {
        let p = pair();
        for i in 0..5 {
            write(&p.l, &format!("sub{i}/f.txt"), b"x");
            write(&p.r, &format!("sub{i}/f.txt"), b"x");
        }
        let (ls, rs) = (p.l.to_string_lossy(), p.r.to_string_lossy());
        let seen = std::sync::Mutex::new(Vec::new());
        let report = |s: &ScanProgress| seen.lock().unwrap().push(s.clone());
        let control = ScanControl {
            progress: Some(&report),
            ..Default::default()
        };
        let res =
            compare_folders_controlled(&LocalFs, &ls, &LocalFs, &rs, &opts(), control).unwrap();
        assert_eq!(res.stats.same, 5);
        let seen = seen.into_inner().unwrap();
        // Root plus five subfolders, on both sides.
        assert_eq!(seen.len(), 12);
        let last = seen.iter().max_by_key(|s| s.scanned).unwrap();
        assert_eq!((last.scanned, last.folders), (20, 12));

        let cancel = AtomicBool::new(true);
        let control = ScanControl {
            cancel: Some(&cancel),
            ..Default::default()
        };
        let err =
            compare_folders_controlled(&LocalFs, &ls, &LocalFs, &rs, &opts(), control).unwrap_err();
        assert_eq!(err.to_string(), CANCELLED);
    }

    #[test]
    fn large_files_fall_back_from_rules_based() {
        let p = pair();
        // Same size, differing only in case: rules-based (ignoring case) calls these similar.
        write(&p.l, "small.txt", b"Alpha\nBeta\n");
        write(&p.r, "small.txt", b"alpha\nbeta\n");
        write(&p.l, "big.txt", &b"Line\n".repeat(400));
        write(&p.r, "big.txt", &b"line\n".repeat(400));
        // Sizes differ: every fallback calls these different.
        write(&p.l, "grown.txt", &b"x\n".repeat(600));
        write(&p.r, "grown.txt", &b"x\n".repeat(700));
        let old = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000_000);
        fs::File::options()
            .write(true)
            .open(p.l.join("big.txt"))
            .unwrap()
            .set_modified(old)
            .unwrap();
        let rules = FolderCompareOptions {
            content: ContentMethod::Rules,
            rules: TextDiffOptions {
                ignore_case: true,
                ..Default::default()
            },
            large_file_threshold: Some(1024),
            ..opts()
        };
        let checked =
            |res: &FolderCompareResult, rel: &str| find(&res.entries, rel).unwrap().content_checked;

        // Byte-by-byte for the big pair; the small one is still rules-based.
        let res = run(&p, &rules);
        assert_eq!(status(&res, "small.txt"), EntryStatus::Similar);
        assert_eq!(checked(&res, "small.txt"), Some(ContentMethod::Rules));
        assert_eq!(status(&res, "big.txt"), EntryStatus::Different);
        assert_eq!(checked(&res, "big.txt"), Some(ContentMethod::Binary));
        assert_eq!(status(&res, "grown.txt"), EntryStatus::Different);

        // Size only: equal sizes are the same without reading anything.
        let res = run(
            &p,
            &FolderCompareOptions {
                large_file_method: LargeFileMethod::Size,
                ..rules.clone()
            },
        );
        assert_eq!(status(&res, "big.txt"), EntryStatus::Same);
        assert_eq!(checked(&res, "big.txt"), Some(ContentMethod::Off));
        assert_eq!(status(&res, "grown.txt"), EntryStatus::Different);
        assert_eq!(status(&res, "small.txt"), EntryStatus::Similar);

        // Size and timestamp: the left big file is decades older.
        let res = run(
            &p,
            &FolderCompareOptions {
                large_file_method: LargeFileMethod::SizeTimestamp,
                ..rules.clone()
            },
        );
        assert_eq!(status(&res, "big.txt"), EntryStatus::Different);
        assert_eq!(status(&res, "small.txt"), EntryStatus::Similar);

        // No threshold: everything is rules-based.
        let res = run(
            &p,
            &FolderCompareOptions {
                large_file_threshold: None,
                ..rules
            },
        );
        assert_eq!(status(&res, "big.txt"), EntryStatus::Similar);
    }

    #[test]
    fn options_saved_before_the_large_file_fallback_still_load() {
        let o: FolderCompareOptions = serde_json::from_str(r#"{"content":"rules"}"#).unwrap();
        assert_eq!(o.large_file_threshold, None);
        assert_eq!(o.large_file_method, LargeFileMethod::Binary);
        let o: FolderCompareOptions =
            serde_json::from_str(r#"{"largeFileThreshold":10,"largeFileMethod":"sizeTimestamp"}"#)
                .unwrap();
        assert_eq!(
            (o.large_file_threshold, o.large_file_method),
            (Some(10), LargeFileMethod::SizeTimestamp)
        );
    }
}

#[cfg(test)]
mod content_id_tests {
    use super::*;
    use crate::vfs::VfsEntry;
    use std::io::Read;

    /// A flat folder whose files carry content ids and can't be read.
    struct IdFs(Vec<(&'static str, &'static str, i64)>);

    impl Vfs for IdFs {
        fn read_dir(&self, _dir: &str) -> Result<Vec<VfsEntry>> {
            Ok(self
                .0
                .iter()
                .map(|(name, id, modified_ms)| VfsEntry {
                    name: (*name).into(),
                    hidden: false,
                    meta: EntryMeta {
                        size: 3,
                        modified_ms: Some(*modified_ms),
                        content_id: Some((*id).into()),
                        ..Default::default()
                    },
                })
                .collect())
        }
        fn open(&self, path: &str) -> Result<Box<dyn Read + Send>> {
            panic!("{path} was read; content ids should have decided");
        }
        fn join(&self, dir: &str, name: &str) -> String {
            format!("{dir}/{name}")
        }
    }

    #[test]
    fn content_ids_decide_without_reading_or_timestamps() {
        // Same size everywhere and an hour apart, as two branches' commits are.
        let left = IdFs(vec![("same.txt", "a1", 0), ("changed.txt", "b1", 0)]);
        let right = IdFs(vec![("same.txt", "a1", 3_600_000), ("changed.txt", "b2", 3_600_000)]);
        for content in [ContentMethod::Off, ContentMethod::Binary, ContentMethod::Crc] {
            let opts = FolderCompareOptions { content, ..Default::default() };
            let res = compare_folders_with(&left, "", &right, "", &opts).unwrap();
            let status = |n: &str| res.entries.iter().find(|e| e.name == n).unwrap().status;
            assert_eq!(status("same.txt"), EntryStatus::Same, "{content:?}");
            assert_eq!(status("changed.txt"), EntryStatus::Different, "{content:?}");
        }
    }
}
