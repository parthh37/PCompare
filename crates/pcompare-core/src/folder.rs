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

use crate::text::{self, TextDiffOptions};
use crate::textfile;
use crate::vfs::{LocalFs, Vfs};
use crate::{CoreError, Result};
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
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
}

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
    /// Allowed modification-time skew in seconds (FAT stores 2s resolution).
    pub time_tolerance_secs: u64,
    /// Treat timestamps exactly one hour apart as equal (DST shifts on FAT/exFAT).
    pub ignore_dst_shift: bool,
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
            time_tolerance_secs: 2,
            ignore_dst_shift: false,
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

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EntryMeta {
    pub size: u64,
    /// Milliseconds since the Unix epoch.
    pub modified_ms: Option<i64>,
    pub is_dir: bool,
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

struct Ctx<'a> {
    opts: &'a FolderCompareOptions,
    exclude: GlobSet,
    include_files: GlobSet,
    exclude_files: GlobSet,
    exclude_folders: GlobSet,
    left: &'a dyn Vfs,
    right: &'a dyn Vfs,
    cache: Option<&'a dyn ChecksumCache>,
    reused: AtomicUsize,
}

impl Ctx<'_> {
    fn fs(&self, side: Side) -> &dyn Vfs {
        match side {
            Side::Left => self.left,
            Side::Right => self.right,
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

struct Listed {
    name: String,
    meta: EntryMeta,
}

/// Lists one directory, applying hidden/exclude filters. Keys are the pairing
/// keys (lower-cased when names are matched case-insensitively).
fn list_dir(fs: &dyn Vfs, dir: &str, rel: &str, ctx: &Ctx) -> Result<BTreeMap<String, Listed>> {
    let mut out = BTreeMap::new();
    for entry in fs.read_dir(dir)? {
        if !ctx.opts.include_hidden && entry.hidden {
            continue;
        }
        let rel_path = join_rel(rel, &entry.name);
        let hit = |set: &GlobSet| set.is_match(&entry.name) || set.is_match(&rel_path);
        let excluded = hit(&ctx.exclude)
            || if entry.meta.is_dir {
                hit(&ctx.exclude_folders)
            } else {
                hit(&ctx.exclude_files) || (!ctx.include_files.is_empty() && !hit(&ctx.include_files))
            };
        if excluded {
            continue;
        }
        let key = if ctx.opts.ignore_name_case {
            entry.name.to_lowercase()
        } else {
            entry.name.clone()
        };
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
    fs.open(path)?.read_to_end(&mut buf).map_err(|e| CoreError::io(path, e))?;
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
    let same_or_diff = |equal: bool| if equal { EntryStatus::Same } else { EntryStatus::Different };
    let binary = || -> Result<ContentVerdict> {
        let equal = left_size == right_size && {
            let (mut lf, mut rf) = (left_fs.open(left)?, right_fs.open(right)?);
            streams_equal(&mut lf, left, &mut rf, right)?
        };
        Ok(ContentVerdict { status: same_or_diff(equal), crc: None })
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
            let diff = text::diff_text(&lt, &rt, rules)?;
            let status = if diff.hunks.is_empty() {
                EntryStatus::Similar
            } else {
                EntryStatus::Different
            };
            Ok(ContentVerdict { status, crc: None })
        }
    }
}

/// Quick tests on metadata alone.
fn quick_tests_pass(l: &EntryMeta, r: &EntryMeta, opts: &FolderCompareOptions) -> bool {
    (!opts.compare_size || l.size == r.size)
        && (!opts.compare_timestamp || timestamps_match(l.modified_ms, r.modified_ms, opts))
}

/// CRC-32 of one side, from the cache when it still describes the file.
fn cached_crc(ctx: &Ctx, side: Side, path: &str, meta: &EntryMeta) -> Result<u32> {
    if let Some(hit) = ctx.cache.and_then(|c| c.get(side, path, meta)) {
        ctx.reused.fetch_add(1, Ordering::Relaxed);
        return Ok(hit);
    }
    let crc = crc32(ctx.fs(side), path)?;
    if let Some(cache) = ctx.cache {
        cache.put(side, path, meta, crc);
    }
    Ok(crc)
}

/// Cached checksums of both sides, without reading anything.
fn cached_pair(ctx: &Ctx, lp: &str, rp: &str, l: &EntryMeta, r: &EntryMeta) -> Option<[u32; 2]> {
    let cache = ctx.cache?;
    let pair = [cache.get(Side::Left, lp, l)?, cache.get(Side::Right, rp, r)?];
    ctx.reused.fetch_add(2, Ordering::Relaxed);
    Some(pair)
}

fn compare_file_pair(lp: &str, rp: &str, l: &EntryMeta, r: &EntryMeta, ctx: &Ctx) -> Result<ContentVerdict> {
    let opts = ctx.opts;
    let quick_same = quick_tests_pass(l, r, opts);
    let quick = ContentVerdict {
        status: if quick_same { EntryStatus::Same } else { EntryStatus::Different },
        crc: None,
    };
    // Without override, a failed quick test is final — no need to read anything.
    if opts.content == ContentMethod::Off || (!quick_same && !opts.override_quick_tests) {
        return Ok(quick);
    }
    let diff = |crc| ContentVerdict { status: EntryStatus::Different, crc };
    match opts.content {
        ContentMethod::Crc if ctx.cache.is_some() => {
            let crc = [cached_crc(ctx, Side::Left, lp, l)?, cached_crc(ctx, Side::Right, rp, r)?];
            let same = l.size == r.size && crc[0] == crc[1];
            Ok(ContentVerdict {
                status: if same { EntryStatus::Same } else { EntryStatus::Different },
                crc: Some(crc),
            })
        }
        // Different checksums prove different bytes; equal ones prove nothing,
        // so a byte-for-byte compare still has to read both files.
        ContentMethod::Binary => match cached_pair(ctx, lp, rp, l, r) {
            Some(crc) if crc[0] != crc[1] => Ok(diff(Some(crc))),
            _ => compare_contents(ctx.left, lp, l.size, ctx.right, rp, r.size, opts.content, &opts.rules),
        },
        _ => compare_contents(ctx.left, lp, l.size, ctx.right, rp, r.size, opts.content, &opts.rules),
    }
}

/// Whether a file pair survives the size/date filters. `None` is the side
/// where the file doesn't exist (an orphan).
pub fn pair_passes(left: Option<&EntryMeta>, right: Option<&EntryMeta>, filters: &FolderFilters) -> bool {
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
    }
}

/// Lists an orphaned folder so its contents are visible in the tree.
fn orphan_tree(dir: &str, rel: &str, side: Side, ctx: &Ctx) -> Vec<FolderEntry> {
    let fs = ctx.fs(side);
    let listed = match list_dir(fs, dir, rel, ctx) {
        Ok(l) => l,
        Err(e) => return vec![error_entry("<unreadable>".into(), rel.into(), false, e)],
    };
    let passes = |m: &EntryMeta| match side {
        Side::Left => pair_passes(Some(m), None, &ctx.opts.filters),
        Side::Right => pair_passes(None, Some(m), &ctx.opts.filters),
    };
    listed
        .into_par_iter()
        .filter(|(_, item)| item.meta.is_dir || passes(&item.meta))
        .map(|(_, item)| {
            let rel_path = join_rel(rel, &item.name);
            let children = if item.meta.is_dir && ctx.opts.recursive {
                orphan_tree(&fs.join(dir, &item.name), &rel_path, side, ctx)
            } else {
                Vec::new()
            };
            let (left, right, status) = match side {
                Side::Left => (Some(item.meta.clone()), None, EntryStatus::LeftOnly),
                Side::Right => (None, Some(item.meta.clone()), EntryStatus::RightOnly),
            };
            FolderEntry {
                is_dir: item.meta.is_dir,
                name: item.name,
                rel_path,
                status,
                newer: None,
                left,
                right,
                children,
                error: None,
                content_checked: None,
                crc: None,
            }
        })
        .collect()
}

fn compare_dirs(ldir: &str, rdir: &str, rel: &str, ctx: &Ctx) -> Result<Vec<FolderEntry>> {
    let mut left = list_dir(ctx.left, ldir, rel, ctx)?;
    let right = list_dir(ctx.right, rdir, rel, ctx)?;

    let mut pairs: Vec<(Option<Listed>, Option<Listed>)> = Vec::new();
    for (key, r) in right {
        pairs.push((left.remove(&key), Some(r)));
    }
    pairs.extend(left.into_values().map(|l| (Some(l), None)));
    pairs.retain(|(l, r)| {
        let any_dir = l.iter().chain(r.iter()).any(|x| x.meta.is_dir);
        any_dir || pair_passes(l.as_ref().map(|x| &x.meta), r.as_ref().map(|x| &x.meta), &ctx.opts.filters)
    });

    let mut entries: Vec<FolderEntry> = pairs
        .into_par_iter()
        .map(|pair| compare_pair(ldir, rdir, rel, pair, ctx))
        .collect();
    // Folders first, then case-insensitive name order, like Explorer.
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    Ok(entries)
}

fn compare_pair(
    ldir: &str,
    rdir: &str,
    rel: &str,
    pair: (Option<Listed>, Option<Listed>),
    ctx: &Ctx,
) -> FolderEntry {
    let (l, r) = pair;
    let name = r.as_ref().or(l.as_ref()).map(|x| x.name.clone()).unwrap_or_default();
    let rel_path = join_rel(rel, &name);
    let mut entry = FolderEntry {
        name: name.clone(),
        rel_path: rel_path.clone(),
        is_dir: false,
        status: EntryStatus::Same,
        newer: None,
        left: l.as_ref().map(|x| x.meta.clone()),
        right: r.as_ref().map(|x| x.meta.clone()),
        children: Vec::new(),
        error: None,
        content_checked: None,
        crc: None,
    };

    match (&l, &r) {
        (Some(l), None) => {
            entry.status = EntryStatus::LeftOnly;
            entry.is_dir = l.meta.is_dir;
            if l.meta.is_dir && ctx.opts.recursive {
                entry.children = orphan_tree(&ctx.left.join(ldir, &l.name), &rel_path, Side::Left, ctx);
            }
        }
        (None, Some(r)) => {
            entry.status = EntryStatus::RightOnly;
            entry.is_dir = r.meta.is_dir;
            if r.meta.is_dir && ctx.opts.recursive {
                entry.children = orphan_tree(&ctx.right.join(rdir, &r.name), &rel_path, Side::Right, ctx);
            }
        }
        (Some(l), Some(r)) if l.meta.is_dir != r.meta.is_dir => {
            entry.status = EntryStatus::TypeMismatch;
        }
        (Some(l), Some(r)) if l.meta.is_dir => {
            entry.is_dir = true;
            if ctx.opts.recursive {
                let (ld, rd) = (ctx.left.join(ldir, &l.name), ctx.right.join(rdir, &r.name));
                match compare_dirs(&ld, &rd, &rel_path, ctx) {
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
            let (lp, rp) = (ctx.left.join(ldir, &l.name), ctx.right.join(rdir, &r.name));
            match compare_file_pair(&lp, &rp, &l.meta, &r.meta, ctx) {
                Ok(verdict) => {
                    entry.status = verdict.status;
                    entry.crc = verdict.crc;
                    if ctx.opts.content != ContentMethod::Off {
                        entry.content_checked = Some(ctx.opts.content);
                    }
                    if verdict.status != EntryStatus::Same {
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
    let (left, right) = (left.as_ref().to_string_lossy(), right.as_ref().to_string_lossy());
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
    let started = Instant::now();
    // Fail fast on bad regexes rather than once per file pair.
    if opts.content == ContentMethod::Rules {
        text::LineNormalizer::new(&opts.rules)?;
    }
    let ci = opts.ignore_name_case;
    let ctx = Ctx {
        opts,
        exclude: build_globset(&opts.exclude, ci)?,
        include_files: build_globset(&opts.filters.include_files, ci)?,
        exclude_files: build_globset(&opts.filters.exclude_files, ci)?,
        exclude_folders: build_globset(&opts.filters.exclude_folders, ci)?,
        left: left_fs,
        right: right_fs,
        cache,
        reused: AtomicUsize::new(0),
    };
    let entries = compare_dirs(left, right, "", &ctx)?;
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
            self.0.lock().unwrap().insert(key, (meta.size, meta.modified_ms, crc));
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
        let opts = FolderCompareOptions { content: ContentMethod::Crc, ..Default::default() };
        let cache = MemCache::default();
        let (ls, rs) = (l.to_string_lossy(), r.to_string_lossy());
        let run = |opts: &FolderCompareOptions| {
            compare_folders_cached(&LocalFs, &ls, &LocalFs, &rs, opts, Some(&cache)).unwrap()
        };

        let first = run(&opts);
        assert_eq!((first.stats.same, first.stats.different, first.stats.checksums_reused), (1, 1, 0));
        let second = run(&opts);
        assert_eq!((second.stats.same, second.stats.different, second.stats.checksums_reused), (1, 1, 4));

        // Binary mode trusts cached checksums only to prove a difference.
        let binary = run(&FolderCompareOptions { content: ContentMethod::Binary, ..opts.clone() });
        assert_eq!((binary.stats.same, binary.stats.different, binary.stats.checksums_reused), (1, 1, 4));

        // A changed file (new size) misses the cache and is read again.
        std::fs::write(r.join("same.txt"), "abcd").unwrap();
        let third = run(&opts);
        assert_eq!((third.stats.same, third.stats.different, third.stats.checksums_reused), (0, 2, 3));
    }
}
