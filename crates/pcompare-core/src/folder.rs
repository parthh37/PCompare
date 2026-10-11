//! Recursive folder comparison.
//!
//! Each directory level is listed on both sides, entries are paired by name,
//! and paired files are classified according to [`CompareCriteria`]. Work
//! inside a directory runs on the rayon pool, so content comparison of many
//! files and recursion into sibling folders happen in parallel.

use crate::{CoreError, Result};
use globset::{Glob, GlobSet, GlobSetBuilder};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, Metadata};
use std::io::Read;
use std::path::Path;
use std::time::{Instant, UNIX_EPOCH};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CompareCriteria {
    /// Fast: equal size and equal (within tolerance) modification time.
    #[default]
    SizeAndTimestamp,
    /// Equal size only.
    Size,
    /// Byte-for-byte comparison (sizes are checked first).
    Content,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct FolderCompareOptions {
    pub criteria: CompareCriteria,
    pub recursive: bool,
    /// Glob patterns matched against both the entry name and its relative path.
    pub exclude: Vec<String>,
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
            criteria: CompareCriteria::default(),
            recursive: true,
            exclude: Vec::new(),
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
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FolderStats {
    pub same: usize,
    pub different: usize,
    pub left_only: usize,
    pub right_only: usize,
    pub errors: usize,
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
}

fn build_globset(patterns: &[String]) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for p in patterns.iter().map(|p| p.trim()).filter(|p| !p.is_empty()) {
        let glob = Glob::new(p).map_err(|e| CoreError::BadPattern {
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

fn to_meta(m: &Metadata) -> EntryMeta {
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

struct Listed {
    name: String,
    meta: EntryMeta,
}

/// Lists one directory, applying hidden/exclude filters. Keys are the pairing
/// keys (lower-cased when names are matched case-insensitively).
fn list_dir(dir: &Path, rel: &str, ctx: &Ctx) -> Result<BTreeMap<String, Listed>> {
    let mut out = BTreeMap::new();
    let read = fs::read_dir(dir).map_err(|e| CoreError::io(dir, e))?;
    for entry in read {
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
        if !ctx.opts.include_hidden && is_hidden(&name, &meta) {
            continue;
        }
        let rel_path = join_rel(rel, &name);
        if ctx.exclude.is_match(&name) || ctx.exclude.is_match(&rel_path) {
            continue;
        }
        let key = if ctx.opts.ignore_name_case {
            name.to_lowercase()
        } else {
            name.clone()
        };
        out.insert(
            key,
            Listed {
                name,
                meta: to_meta(&meta),
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
fn read_full(reader: &mut impl Read, buf: &mut [u8]) -> std::io::Result<usize> {
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

/// Streams both files in lock-step; stops at the first differing chunk.
pub fn files_equal(left: &Path, right: &Path) -> Result<bool> {
    const CHUNK: usize = 128 * 1024;
    let mut lf = fs::File::open(left).map_err(|e| CoreError::io(left, e))?;
    let mut rf = fs::File::open(right).map_err(|e| CoreError::io(right, e))?;
    let (mut lb, mut rb) = (vec![0u8; CHUNK], vec![0u8; CHUNK]);
    loop {
        let ln = read_full(&mut lf, &mut lb).map_err(|e| CoreError::io(left, e))?;
        let rn = read_full(&mut rf, &mut rb).map_err(|e| CoreError::io(right, e))?;
        if ln != rn || lb[..ln] != rb[..rn] {
            return Ok(false);
        }
        if ln == 0 {
            return Ok(true);
        }
    }
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
    // TODO(you): implement the tolerance policy. See the design notes in the
    // PR / chat: consider `opts.time_tolerance_secs`, `opts.ignore_dst_shift`,
    // and what to do when one side has no timestamp at all.
    let _ = opts;
    left_ms == right_ms
}

fn newer_side(l: &EntryMeta, r: &EntryMeta, opts: &FolderCompareOptions) -> Option<Side> {
    match (l.modified_ms, r.modified_ms) {
        (Some(a), Some(b)) if !timestamps_match(Some(a), Some(b), opts) => {
            Some(if a > b { Side::Left } else { Side::Right })
        }
        _ => None,
    }
}

fn compare_file_pair(lp: &Path, rp: &Path, l: &EntryMeta, r: &EntryMeta, ctx: &Ctx) -> Result<bool> {
    Ok(match ctx.opts.criteria {
        CompareCriteria::Size => l.size == r.size,
        CompareCriteria::SizeAndTimestamp => {
            l.size == r.size && timestamps_match(l.modified_ms, r.modified_ms, ctx.opts)
        }
        CompareCriteria::Content => l.size == r.size && files_equal(lp, rp)?,
    })
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
    }
}

/// Lists an orphaned folder so its contents are visible in the tree.
fn orphan_tree(dir: &Path, rel: &str, side: Side, ctx: &Ctx) -> Vec<FolderEntry> {
    let listed = match list_dir(dir, rel, ctx) {
        Ok(l) => l,
        Err(e) => return vec![error_entry("<unreadable>".into(), rel.into(), false, e)],
    };
    listed
        .into_par_iter()
        .map(|(_, item)| {
            let rel_path = join_rel(rel, &item.name);
            let children = if item.meta.is_dir && ctx.opts.recursive {
                orphan_tree(&dir.join(&item.name), &rel_path, side, ctx)
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
            }
        })
        .collect()
}

fn compare_dirs(ldir: &Path, rdir: &Path, rel: &str, ctx: &Ctx) -> Result<Vec<FolderEntry>> {
    let mut left = list_dir(ldir, rel, ctx)?;
    let right = list_dir(rdir, rel, ctx)?;

    let mut pairs: Vec<(Option<Listed>, Option<Listed>)> = Vec::new();
    for (key, r) in right {
        pairs.push((left.remove(&key), Some(r)));
    }
    pairs.extend(left.into_values().map(|l| (Some(l), None)));

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
    ldir: &Path,
    rdir: &Path,
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
    };

    match (&l, &r) {
        (Some(l), None) => {
            entry.status = EntryStatus::LeftOnly;
            entry.is_dir = l.meta.is_dir;
            if l.meta.is_dir && ctx.opts.recursive {
                entry.children = orphan_tree(&ldir.join(&l.name), &rel_path, Side::Left, ctx);
            }
        }
        (None, Some(r)) => {
            entry.status = EntryStatus::RightOnly;
            entry.is_dir = r.meta.is_dir;
            if r.meta.is_dir && ctx.opts.recursive {
                entry.children = orphan_tree(&rdir.join(&r.name), &rel_path, Side::Right, ctx);
            }
        }
        (Some(l), Some(r)) if l.meta.is_dir != r.meta.is_dir => {
            entry.status = EntryStatus::TypeMismatch;
        }
        (Some(l), Some(r)) if l.meta.is_dir => {
            entry.is_dir = true;
            if ctx.opts.recursive {
                match compare_dirs(&ldir.join(&l.name), &rdir.join(&r.name), &rel_path, ctx) {
                    Ok(children) => {
                        let all_same = children.iter().all(|c| c.status == EntryStatus::Same);
                        entry.status = if all_same {
                            EntryStatus::Same
                        } else {
                            EntryStatus::Different
                        };
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
            let (lp, rp) = (ldir.join(&l.name), rdir.join(&r.name));
            match compare_file_pair(&lp, &rp, &l.meta, &r.meta, ctx) {
                Ok(true) => entry.status = EntryStatus::Same,
                Ok(false) => {
                    entry.status = EntryStatus::Different;
                    entry.newer = newer_side(&l.meta, &r.meta, ctx.opts);
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

fn tally(entries: &[FolderEntry], stats: &mut FolderStats) {
    for e in entries {
        if !e.is_dir {
            match e.status {
                EntryStatus::Same => stats.same += 1,
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
    let started = Instant::now();
    let (left, right) = (left.as_ref(), right.as_ref());
    let ctx = Ctx {
        opts,
        exclude: build_globset(&opts.exclude)?,
    };
    let entries = compare_dirs(left, right, "", &ctx)?;
    let mut stats = FolderStats::default();
    tally(&entries, &mut stats);
    Ok(FolderCompareResult {
        left_root: left.display().to_string(),
        right_root: right.display().to_string(),
        entries,
        stats,
        elapsed_ms: started.elapsed().as_millis(),
    })
}
