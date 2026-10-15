//! Recursive folder comparison.
//!
//! Each directory level is listed on both sides, entries are paired by name,
//! and paired files are classified by quick tests and an optional content pass. Work
//! inside a directory runs on the rayon pool, so content comparison of many
//! files and recursion into sibling folders happen in parallel.
//!
//! Classification is two-staged: cheap *quick tests* (size,
//! timestamp) run on metadata, then an optional *content* pass (binary, CRC
//! or rules-based) reads the files and, by default, overrides the quick result.
//!
//! Both sides go through [`Vfs`], so either side may be a remote store.
//!
//! Entries pair by an alignment key (see [`AlignOptions`]): exact names by
//! default, optionally folding case, Unicode normalization, extensions and
//! wildcard rules, so a pair's two names may differ — the left one is then
//! reported in [`FolderEntry::left_rel_path`].
//!
//! The work is staged: folders are *listed*
//! once into a [`FolderScan`], and *alignment*, *filters* and *comparison*
//! re-run over those listings. Changing a filter or a criterion therefore
//! never reads a folder again ([`CompareNeeds`] says what has to be redone),
//! and filters mark entries ([`FolderEntry::filtered`]) instead of dropping
//! them, so a view can show what they hide.

mod align;
mod pe_version;
mod status;

pub use align::{AlignOptions, AlignRule};
pub use pe_version::VERSION_MAX_BYTES;
pub use status::{
    aggregate, contribution, replace_entry, Replaced, StatusCounts, StatusDetail, StatusSet,
};

use crate::text::{self, TextDiffOptions};
use crate::textfile;
use crate::vfs::{ListOptions, LocalFs, Vfs, VfsEntry};
use crate::{CoreError, Result};
use align::Aligner;
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use rayon::prelude::*;
use regex::{Regex, RegexBuilder};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::Read;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

/// How file contents are compared after the quick tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
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
    /// Let the content result replace the quick-test result (the
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

/// Which sides of a row the filters leave out of the comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Filtered {
    /// The left entry is filtered, the right one isn't: the pair is still
    /// shown and compared, so it doesn't pose as a right orphan.
    Left,
    Right,
    /// Every side present is filtered (or a folder above is): the row is out
    /// of the comparison and its totals.
    Both,
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
    /// [`Self::status`] at a finer granularity: newer side,
    /// same-but-quick-tests-differ, not compared yet.
    pub detail: StatusDetail,
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
    /// Folders whose contents were listed: how many rows below have each
    /// detail (see [`StatusCounts`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub counts: Option<StatusCounts>,
    /// Set when filters leave out one side or the whole row. Rows filtered
    /// on [`Filtered::Both`] sides are only reported with
    /// [`ScanControl::keep_filtered`]; their files get quick tests only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filtered: Option<Filtered>,
}

impl FolderEntry {
    /// Whether the filters leave the whole row out.
    pub fn filtered_out(&self) -> bool {
        self.filtered == Some(Filtered::Both)
    }
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
    /// Reported files the filters leave out on every side (see
    /// [`ScanControl::keep_filtered`]); they count nowhere else.
    pub filtered: usize,
    /// Folders read from disk by this run; the rest came from the
    /// [`FolderScan`]'s listings.
    pub dirs_read: usize,
    /// The whole tree's [`StatusCounts`], as for a folder holding both roots.
    pub counts: StatusCounts,
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
    /// The stages this run had to redo on its [`FolderScan`] (`None`
    /// without one).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub needs: Option<CompareNeeds>,
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
    /// Listings and verdicts kept from earlier runs over the same folders,
    /// reused and filled by this one. Without it nothing outlives the run.
    pub scan: Option<&'a FolderScan>,
    /// Report rows the filters leave out (marked [`Filtered::Both`])
    /// instead of dropping them. This also turns a pair excluded by name,
    /// hidden or attribute filters on one side only into a pair marked on
    /// that side; without it, that side is dropped and the other is an
    /// orphan. Size, date and "containing" filters keep such pairs either way.
    pub keep_filtered: bool,
    /// Walk filtered-out folders too, so their contents can be shown. Off,
    /// they're reported (with `keep_filtered`) but never listed.
    pub expand_filtered: bool,
}

/// What changed options make a [`FolderScan`] redo. Alignment, filters and quick tests run over the
/// listings in memory and are cheap; only `list` reads folders again, and
/// `content` / `content_filter` read files again.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompareNeeds {
    /// Folders must be listed again (links are listed differently).
    pub list: bool,
    /// Entries pair up differently.
    pub align: bool,
    /// Name, hidden, attribute, size or date filters changed.
    pub filter: bool,
    /// The "containing" filters changed: the files they read are read again.
    pub content_filter: bool,
    /// Quick tests, or how content results combine with them, changed.
    pub compare: bool,
    /// Content verdicts are stale: the content method or its rules changed.
    pub content: bool,
}

impl CompareNeeds {
    /// Everything: a scan with nothing listed yet.
    pub const ALL: Self = Self {
        list: true,
        align: true,
        filter: true,
        content_filter: true,
        compare: true,
        content: true,
    };

    /// Whether anything at all has to be redone.
    pub fn any(&self) -> bool {
        *self != Self::default()
    }
}

/// The options a [`FolderScan`] has to redo work for, going from `old` to `new`.
pub fn compare_needs(old: &FolderCompareOptions, new: &FolderCompareOptions) -> CompareNeeds {
    let (of, nf) = (&old.filters, &new.filters);
    let containing = |f: &FolderFilters| {
        (
            f.include_containing.clone(),
            f.exclude_containing.clone(),
            f.containing_regex,
            f.containing_match_case,
        )
    };
    let other_filters = |f: &FolderFilters| FolderFilters {
        include_containing: String::new(),
        exclude_containing: String::new(),
        containing_regex: false,
        containing_match_case: false,
        ..f.clone()
    };
    CompareNeeds {
        list: old.follow_links != new.follow_links,
        align: old.ignore_name_case != new.ignore_name_case || old.align != new.align,
        filter: old.recursive != new.recursive
            || old.include_hidden != new.include_hidden
            || old.exclude != new.exclude
            // Name masks fold case along with alignment.
            || old.ignore_name_case != new.ignore_name_case
            || !same_json(&other_filters(of), &other_filters(nf)),
        content_filter: containing(of) != containing(nf),
        compare: old.compare_size != new.compare_size
            || old.compare_timestamp != new.compare_timestamp
            || old.time_tolerance_secs != new.time_tolerance_secs
            || old.ignore_dst_shift != new.ignore_dst_shift
            || old.override_quick_tests != new.override_quick_tests
            || old.compare_attributes != new.compare_attributes
            || old.large_file_threshold != new.large_file_threshold
            || old.large_file_method != new.large_file_method
            || old.content != new.content,
        content: old.content != new.content
            || (new.content == ContentMethod::Rules && !same_json(&old.rules, &new.rules)),
    }
}

/// Equality through serde, for the option structs that don't derive it
/// (the text rules, the filters).
fn same_json<T: Serialize>(a: &T, b: &T) -> bool {
    serde_json::to_string(a).ok() == serde_json::to_string(b).ok()
}

fn fingerprint(value: &impl Hash) -> u64 {
    let mut h = DefaultHasher::new();
    value.hash(&mut h);
    h.finish()
}

fn locked<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // A panic while holding the lock leaves the maps themselves intact.
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Two files' sizes and timestamps, which a remembered verdict must match.
type Stamp = [(u64, Option<i64>); 2];
/// One side's sorted listings, by `follow_links` and the folder's path.
type Listings = Mutex<HashMap<(bool, String), Arc<[VfsEntry]>>>;
/// Both paths, the content method and a fingerprint of what else decides
/// the verdict (the text rules, for rules-based).
type VerdictKey = (String, String, ContentMethod, u64);

/// The listed tree of two folders, kept between comparisons of them.
/// A run lists each folder through it at most
/// once; later runs with other filters, alignment or criteria re-run those
/// stages on the listings in memory, reusing content verdicts that still
/// hold. Every remembered result is keyed by what it depends on, so a
/// stale one is never handed out; [`FolderScan::restage`] frees them.
///
/// Folders changed on disk need a new scan: listings are never re-checked.
pub struct FolderScan {
    listings: [Listings; 2],
    canonical: [Mutex<HashMap<String, Option<String>>>; 2],
    verdicts: Mutex<HashMap<VerdictKey, (Stamp, ContentVerdict)>>,
    /// "Containing" filter results by side, path and filter fingerprint.
    contains: Mutex<HashMap<(usize, String, u64), bool>>,
    /// The options of the latest run.
    last: Mutex<Option<FolderCompareOptions>>,
    /// Off for the throwaway scan of a run without one: nothing is kept.
    retain: bool,
}

impl Default for FolderScan {
    fn default() -> Self {
        Self::new()
    }
}

impl FolderScan {
    pub fn new() -> Self {
        Self {
            listings: Default::default(),
            canonical: Default::default(),
            verdicts: Default::default(),
            contains: Default::default(),
            last: Default::default(),
            retain: true,
        }
    }

    fn transient() -> Self {
        Self {
            retain: false,
            ..Self::new()
        }
    }

    /// Records `opts` as the options of the next run and says what they
    /// make it redo, freeing what they leave stale.
    pub fn restage(&self, opts: &FolderCompareOptions) -> CompareNeeds {
        let mut last = locked(&self.last);
        let needs = last.as_ref().map_or(CompareNeeds::ALL, |old| compare_needs(old, opts));
        if let Some(old) = last.as_ref() {
            if needs.list {
                for side in &self.listings {
                    locked(side).retain(|(follow, _), _| *follow == opts.follow_links);
                }
                // Followed and unfollowed links pair different files.
                locked(&self.verdicts).clear();
            }
            if needs.content_filter {
                locked(&self.contains).clear();
            }
            if !same_json(&old.rules, &opts.rules) {
                locked(&self.verdicts).retain(|(.., method, _), _| *method != ContentMethod::Rules);
            }
        }
        *last = Some(opts.clone());
        needs
    }

    fn canonical_of(&self, fs: &dyn Vfs, side: Side, path: &str) -> Option<String> {
        let cache = &self.canonical[Chain::index(side)];
        if let Some(hit) = locked(cache).get(path) {
            return hit.clone();
        }
        let canonical = fs.canonical(path);
        if self.retain {
            locked(cache).insert(path.to_owned(), canonical.clone());
        }
        canonical
    }

    /// Folders listed so far, both sides together.
    pub fn listed_dirs(&self) -> usize {
        self.listings.iter().map(|side| locked(side).len()).sum()
    }

    /// Drops everything remembered about `path` and what's below it on
    /// `side` — listings, link targets, "containing" results and content
    /// verdicts — so it's read afresh. A verdict's size and timestamp check
    /// can't catch a copy that kept both.
    fn forget(&self, side: Side, path: &str) {
        let at = Chain::index(side);
        let under = |p: &str| {
            p.strip_prefix(path)
                .is_some_and(|rest| rest.is_empty() || rest.starts_with(['/', '\\']))
        };
        locked(&self.listings[at]).retain(|(_, p), _| !under(p));
        locked(&self.canonical[at]).retain(|p, _| !under(p));
        locked(&self.contains).retain(|(s, p, _), _| *s != at || !under(p));
        locked(&self.verdicts).retain(|(lp, rp, ..), _| !under(if at == 0 { lp } else { rp }));
    }

    /// Drops one folder's listing on `side`, so what was added to or
    /// removed from it shows.
    fn forget_listing(&self, side: Side, dir: &str) {
        locked(&self.listings[Chain::index(side)]).retain(|(_, p), _| p != dir);
    }
}

/// The rows a partial comparison ([`compare_entries`]) is limited to, by
/// name within one folder: `None` compares that row in full, `Some` only
/// the named rows below it.
#[derive(Debug, Default)]
struct Focus(BTreeMap<String, Option<Focus>>);

impl Focus {
    /// Adds a `/`-separated path below this folder; a row compared in full
    /// already covers everything below it.
    fn insert(&mut self, rel: &str) {
        let (name, rest) = match rel.split_once('/') {
            Some((name, rest)) => (name, Some(rest)),
            None => (rel, None),
        };
        match rest {
            None => {
                self.0.insert(name.to_owned(), None);
            }
            Some(rest) => {
                if let Some(below) = self
                    .0
                    .entry(name.to_owned())
                    .or_insert_with(|| Some(Focus::default()))
                {
                    below.insert(rest);
                }
            }
        }
    }

    fn wants(&self, name: &str) -> bool {
        self.0.contains_key(name)
    }

    /// Whether `name` is compared in full.
    fn is_target(&self, name: &str) -> bool {
        matches!(self.0.get(name), Some(None))
    }

    /// Whether some row right in this folder is compared in full, so the
    /// folder's listing has to be read afresh.
    fn has_targets(&self) -> bool {
        self.0.values().any(Option::is_none)
    }

    /// What to compare below `name`: `None` is everything.
    fn below(&self, name: &str) -> Option<&Focus> {
        self.0.get(name).and_then(Option::as_ref)
    }
}

/// Forgets what the scan holds for the full-compare targets among `pairs`
/// (both sides' paths below `ldir` / `rdir`).
fn forget_targets<'p>(
    focus: &Focus,
    pairs: impl IntoIterator<Item = (Option<&'p Listed>, Option<&'p Listed>)>,
    ldir: &str,
    rdir: &str,
    ctx: &Ctx,
) {
    for (l, r) in pairs {
        if !focus.is_target(pair_name(l, r)) {
            continue;
        }
        for (side, item, dir) in [(Side::Left, l, ldir), (Side::Right, r, rdir)] {
            if let Some(x) = item {
                ctx.scan.forget(side, &ctx.fs(side).join(dir, &x.name));
            }
        }
    }
}

struct Ctx<'a> {
    opts: &'a FolderCompareOptions,
    exclude: GlobSet,
    include_files: GlobSet,
    exclude_files: GlobSet,
    exclude_folders: GlobSet,
    include_containing: Option<Regex>,
    exclude_containing: Option<Regex>,
    /// Identifies the "containing" filters in [`FolderScan::contains`].
    containing_key: u64,
    /// Identifies the text rules in [`FolderScan::verdicts`].
    rules_key: u64,
    aligner: Aligner,
    left: &'a dyn Vfs,
    right: &'a dyn Vfs,
    control: ScanControl<'a>,
    scan: &'a FolderScan,
    /// Canonical paths of the compared folders, when links are followed.
    roots: [Option<String>; 2],
    reused: AtomicUsize,
    scanned: AtomicUsize,
    folders: AtomicUsize,
    compared: AtomicUsize,
    dirs_read: AtomicUsize,
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

    /// Lists a folder through the scan: from memory when an earlier run (or
    /// pass) listed it already, else from disk. Entries come sorted by name.
    fn read_dir(&self, side: Side, dir: &str) -> Result<Arc<[VfsEntry]>> {
        let key = (self.opts.follow_links, dir.to_owned());
        let cache = &self.scan.listings[Chain::index(side)];
        if let Some(hit) = locked(cache).get(&key) {
            return Ok(hit.clone());
        }
        let mut items = self.fs(side).read_dir_with(
            dir,
            ListOptions {
                follow_links: self.opts.follow_links,
            },
        )?;
        self.dirs_read.fetch_add(1, Ordering::Relaxed);
        // Sorted, so which of two colliding names keeps the folded key is stable.
        items.sort_unstable_by(|a, b| a.name.cmp(&b.name));
        let items: Arc<[VfsEntry]> = items.into();
        // Errors aren't kept: a later run may well read the folder.
        if self.scan.retain {
            locked(cache).insert(key, items.clone());
        }
        Ok(items)
    }

    /// [`Vfs::canonical`], remembered by the scan.
    fn canonical(&self, side: Side, path: &str) -> Option<String> {
        self.scan.canonical_of(self.fs(side), side, path)
    }

    /// Whether rows the filters leave out are reported.
    fn shows(&self, e: &FolderEntry) -> bool {
        self.control.keep_filtered || !e.filtered_out()
    }

    /// Reports a folder's rows to [`ScanControl::levels`], without children.
    fn level<'e>(&self, rel: &str, entries: impl IntoIterator<Item = &'e FolderEntry>, complete: bool) {
        if let Some(report) = self.control.levels {
            let entries = entries
                .into_iter()
                .filter(|e| self.shows(e))
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
    /// The filters leave this side out.
    filtered: bool,
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

impl Ctx<'_> {
    /// Whether the hidden, name or attribute filters exclude an entry. `rel`
    /// is its folder's path on its own side.
    fn excludes(&self, rel: &str, entry: &VfsEntry) -> bool {
        if !self.opts.include_hidden && entry.hidden {
            return true;
        }
        let rel_path = join_rel(rel, &entry.name);
        let hit = |set: &GlobSet| set.is_match(&entry.name) || set.is_match(&rel_path);
        let excluded = hit(&self.exclude)
            || if entry.meta.is_dir {
                hit(&self.exclude_folders)
            } else {
                hit(&self.exclude_files)
                    || (!self.include_files.is_empty() && !hit(&self.include_files))
            };
        excluded || attributes_excluded(&entry.meta, &self.opts.filters)
    }

    /// Whether the "containing" filters leave a file out, remembered by the
    /// scan. Unreadable files stay, so the comparison can report the error.
    fn contents_filter_out(&self, side: Side, path: &str) -> bool {
        if self.include_containing.is_none() && self.exclude_containing.is_none() {
            return false;
        }
        let key = (Chain::index(side), path.to_owned(), self.containing_key);
        if let Some(&hit) = locked(&self.scan.contains).get(&key) {
            return hit;
        }
        let fs = self.fs(side);
        let out = self
            .include_containing
            .as_ref()
            .is_some_and(|re| matches!(file_contains(fs, path, re), Ok(false)))
            || self
                .exclude_containing
                .as_ref()
                .is_some_and(|re| matches!(file_contains(fs, path, re), Ok(true)));
        if self.scan.retain {
            locked(&self.scan.contains).insert(key, out);
        }
        out
    }
}

/// Lists one directory (through the scan) and marks what the filters leave
/// out. Keys are the alignment keys of [`Aligner::key`].
fn list_dir(side: Side, dir: &str, rel: &str, ctx: &Ctx) -> Result<BTreeMap<String, Listed>> {
    ctx.check_cancel()?;
    let items = ctx.read_dir(side, dir)?;
    ctx.listed(rel, items.len());
    let filters = &ctx.opts.filters;
    let marked: Vec<(bool, &VfsEntry)> = items
        .iter()
        .filter_map(|e| {
            let excluded = ctx.excludes(rel, e);
            // Without filtered rows reported, an excluded side isn't there at
            // all (its counterpart is an orphan), as before filters marked
            // rather than dropped: Folder Sync's plans depend on it.
            if excluded && !ctx.control.keep_filtered {
                return None;
            }
            Some((excluded || (!e.meta.is_dir && !meta_passes(&e.meta, filters)), e))
        })
        .collect();
    let mut out = BTreeMap::new();
    // Entries the filters keep claim their keys first, so a filtered name
    // never takes a pairing away from one that's compared.
    for pass in [false, true] {
        for &(filtered, entry) in marked.iter().filter(|(f, _)| *f == pass) {
            let mut key = ctx.aligner.key(side, &entry.name, entry.meta.is_dir);
            if out.contains_key(&key) {
                // Two names fold alike on one side (`a.htm` and `a.html` in one
                // group): the later one pairs only by its exact name.
                key = Aligner::exact_key(&entry.name);
            }
            out.insert(
                key,
                Listed {
                    name: entry.name.clone(),
                    meta: entry.meta.clone(),
                    filtered,
                },
            );
        }
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

/// Classifies a file pair. `quick_only` (rows the filters leave out) stops
/// at the quick tests: their contents are never read.
fn compare_file_pair(
    lp: &str,
    rp: &str,
    l: &EntryMeta,
    r: &EntryMeta,
    ctx: &Ctx,
    quick_only: bool,
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
    if quick_only
        || opts.content == ContentMethod::Off
        || (!quick_same && !opts.override_quick_tests)
    {
        return Ok(quick);
    }
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
    // An earlier run over the same scan may have read this pair already.
    let key: VerdictKey = (
        lp.to_owned(),
        rp.to_owned(),
        content,
        if content == ContentMethod::Rules { ctx.rules_key } else { 0 },
    );
    let stamp: Stamp = [(l.size, l.modified_ms), (r.size, r.modified_ms)];
    if let Some((was, verdict)) = locked(&ctx.scan.verdicts).get(&key) {
        if *was == stamp {
            return Ok(*verdict);
        }
    }
    let verdict = read_contents(lp, rp, l, r, content, ctx)?;
    if ctx.scan.retain {
        locked(&ctx.scan.verdicts).insert(key, (stamp, verdict));
    }
    Ok(verdict)
}

/// Reads a file pair's contents with `content`, using the checksum cache.
fn read_contents(
    lp: &str,
    rp: &str,
    l: &EntryMeta,
    r: &EntryMeta,
    content: ContentMethod,
    ctx: &Ctx,
) -> Result<ContentVerdict> {
    let opts = ctx.opts;
    let diff = |crc| ContentVerdict {
        status: EntryStatus::Different,
        crc,
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

/// Whether one file survives the size/date filters.
fn meta_passes(m: &EntryMeta, filters: &FolderFilters) -> bool {
    // An unknown timestamp (some remote stores) can't be shown to be out
    // of range, so date filters keep it rather than hide it.
    let time_ok = |bound: Option<i64>, ok: fn(i64, i64) -> bool| {
        bound.is_none_or(|b| m.modified_ms.is_none_or(|t| ok(t, b)))
    };
    filters.min_size.is_none_or(|min| m.size >= min)
        && filters.max_size.is_none_or(|max| m.size <= max)
        && time_ok(filters.modified_after_ms, |t, b| t >= b)
        && time_ok(filters.modified_before_ms, |t, b| t <= b)
}

/// Whether a file pair survives the size/date filters. `None` is the side
/// where the file doesn't exist (an orphan).
pub fn pair_passes(
    left: Option<&EntryMeta>,
    right: Option<&EntryMeta>,
    filters: &FolderFilters,
) -> bool {
    // Either side matching keeps the pair, so a file that just crossed a
    // threshold (e.g. grew past max_size) is listed instead of vanishing.
    let passes = |m: &EntryMeta| meta_passes(m, filters);
    left.is_some_and(passes) || right.is_some_and(passes)
}

/// What the filters leave out of a pair: the sides they marked, or
/// everything below a filtered-out folder (`inherited`).
fn pair_filtered(inherited: bool, l: Option<&Listed>, r: Option<&Listed>) -> Option<Filtered> {
    if inherited {
        return Some(Filtered::Both);
    }
    match (l.map(|x| x.filtered), r.map(|x| x.filtered)) {
        (Some(true), Some(false)) => Some(Filtered::Left),
        (Some(false), Some(true)) => Some(Filtered::Right),
        (Some(true), Some(true) | None) | (None, Some(true)) => Some(Filtered::Both),
        _ => None,
    }
}

fn error_entry(name: String, rel_path: String, is_dir: bool, err: CoreError) -> FolderEntry {
    FolderEntry {
        name,
        rel_path,
        is_dir,
        status: EntryStatus::Error,
        detail: StatusDetail::Error,
        newer: None,
        left: None,
        right: None,
        children: Vec::new(),
        error: Some(err.to_string()),
        content_checked: None,
        crc: None,
        left_rel_path: None,
        filtered: None,
        counts: None,
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
            ctx.canonical(side, path)
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
/// is the tree's relative path, `side_rel` the side's own; `filtered` is set
/// below a folder the filters leave out, `focus` limits it to some rows.
#[allow(clippy::too_many_arguments)]
fn orphan_tree(
    dir: &str,
    rel: &str,
    side_rel: &str,
    side: Side,
    chain: &Chain,
    filtered: bool,
    focus: Option<&Focus>,
    ctx: &Ctx,
) -> Vec<FolderEntry> {
    let fs = ctx.fs(side);
    if focus.is_some_and(Focus::has_targets) {
        ctx.scan.forget_listing(side, dir);
    }
    let mut listed = match list_dir(side, dir, side_rel, ctx) {
        Ok(l) => l,
        Err(e) => {
            let entries = vec![error_entry("<unreadable>".into(), rel.into(), false, e)];
            ctx.level(rel, &entries, true);
            return entries;
        }
    };
    if let Some(focus) = focus {
        listed.retain(|_, item| focus.wants(&item.name));
        let items = listed.values().map(|item| match side {
            Side::Left => (Some(item), None),
            Side::Right => (None, Some(item)),
        });
        forget_targets(focus, items, dir, dir, ctx);
    }
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
            detail: status.into(),
            newer: None,
            left,
            right,
            children: Vec::new(),
            error: None,
            content_checked: None,
            crc: None,
            filtered: (filtered || item.filtered).then_some(Filtered::Both),
            counts: None,
        }
    };
    // One-sided rows are final as soon as they're listed, so the folder
    // reports complete before its subfolders are walked.
    let rows: Vec<(FolderEntry, Option<Descend>)> = listed
        .into_par_iter()
        .map(|(_, mut item)| {
            if !filtered && !item.filtered && !item.meta.is_dir {
                item.filtered = ctx.contents_filter_out(side, &fs.join(dir, &item.name));
            }
            let mut entry = orphan(&item);
            let mut descend = None;
            let walk = ctx.control.expand_filtered || !entry.filtered_out();
            if item.meta.is_dir && ctx.opts.recursive && walk {
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
        .filter(|(e, _)| ctx.shows(e))
        .map(|(mut entry, descend)| {
            if let Some(Descend { path, canonical }) = descend {
                let mut sub = Chain {
                    canonical: [None, None],
                    parent: Some(chain),
                };
                sub.canonical[Chain::index(side)] = canonical;
                let own_rel = entry.left_rel_path.clone().unwrap_or_else(|| entry.rel_path.clone());
                let inherited = entry.filtered_out();
                let focus = focus.and_then(|f| f.below(&entry.name));
                entry.children = orphan_tree(
                    &path,
                    &entry.rel_path,
                    &own_rel,
                    side,
                    &sub,
                    inherited,
                    focus,
                    ctx,
                );
                entry.counts = Some(aggregate(&entry.children));
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
    /// The folders sit below one the filters leave out.
    filtered: bool,
    /// Only these rows are compared (see [`compare_entries`]); `None` is all.
    focus: Option<&'a Focus>,
}

fn compare_dirs(at: &DirPair, chain: &Chain, ctx: &Ctx) -> Result<Vec<FolderEntry>> {
    if at.focus.is_some_and(Focus::has_targets) {
        ctx.scan.forget_listing(Side::Left, at.ldir);
        ctx.scan.forget_listing(Side::Right, at.rdir);
    }
    let mut left = list_dir(Side::Left, at.ldir, at.lrel, ctx)?;
    let right = list_dir(Side::Right, at.rdir, at.rel, ctx)?;

    let mut pairs: Vec<(Option<Listed>, Option<Listed>)> = Vec::new();
    for (key, r) in right {
        pairs.push((left.remove(&key), Some(r)));
    }
    pairs.extend(left.into_values().map(|l| (Some(l), None)));
    if let Some(focus) = at.focus {
        pairs.retain(|(l, r)| focus.wants(pair_name(l.as_ref(), r.as_ref())));
        let pairs = pairs.iter().map(|(l, r)| (l.as_ref(), r.as_ref()));
        forget_targets(focus, pairs, at.ldir, at.rdir, ctx);
    }

    if ctx.control.levels.is_some() {
        let shallow: Vec<_> = pairs
            .iter()
            .map(|(l, r)| pair_entry(at, l.as_ref(), r.as_ref()))
            .collect();
        ctx.level(at.rel, &shallow, false);
    }
    let mut entries: Vec<FolderEntry> = pairs
        .into_par_iter()
        .map(|pair| compare_pair(at, chain, pair, ctx))
        .collect();
    // Folders first, then case-insensitive name order, like Explorer.
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    ctx.level(at.rel, &entries, true);
    entries.retain(|e| ctx.shows(e));
    Ok(entries)
}

/// A pair's row before anything below it is compared: one-sided rows and
/// type mismatches are final, two-sided ones still read `Same`.
/// A pair's name in the tree: the right side's, as for its `rel_path`.
fn pair_name<'a>(l: Option<&'a Listed>, r: Option<&'a Listed>) -> &'a str {
    r.or(l).map_or("", |x| &x.name)
}

fn pair_entry(at: &DirPair, l: Option<&Listed>, r: Option<&Listed>) -> FolderEntry {
    let name = pair_name(l, r).to_owned();
    let rel_path = join_rel(at.rel, &name);
    let (status, is_dir) = match (l, r) {
        (Some(l), None) => (EntryStatus::LeftOnly, l.meta.is_dir),
        (None, Some(r)) => (EntryStatus::RightOnly, r.meta.is_dir),
        (Some(l), Some(r)) if l.meta.is_dir != r.meta.is_dir => (EntryStatus::TypeMismatch, false),
        (Some(l), _) => (EntryStatus::Same, l.meta.is_dir),
        (None, None) => unreachable!("pairs always have at least one side"),
    };
    // Two-sided rows are settled by compare_pair.
    let detail = match status {
        EntryStatus::Same => StatusDetail::Unknown,
        s => s.into(),
    };
    FolderEntry {
        left_rel_path: l
            .map(|x| join_rel(at.lrel, &x.name))
            .filter(|p| *p != rel_path),
        name,
        rel_path,
        is_dir,
        status,
        detail,
        newer: None,
        left: l.map(|x| x.meta.clone()),
        right: r.map(|x| x.meta.clone()),
        children: Vec::new(),
        error: None,
        content_checked: None,
        crc: None,
        filtered: pair_filtered(at.filtered, l, r),
        counts: None,
    }
}

fn compare_pair(
    at: &DirPair,
    chain: &Chain,
    pair: (Option<Listed>, Option<Listed>),
    ctx: &Ctx,
) -> FolderEntry {
    let (mut l, mut r) = pair;
    // The "containing" filters read files, so they run here, in parallel,
    // and only on sides no cheaper filter has left out already.
    if !at.filtered {
        for (side, item, dir) in [
            (Side::Left, l.as_mut(), at.ldir),
            (Side::Right, r.as_mut(), at.rdir),
        ] {
            if let Some(x) = item.filter(|x| !x.filtered && !x.meta.is_dir) {
                x.filtered = ctx.contents_filter_out(side, &ctx.fs(side).join(dir, &x.name));
            }
        }
    }
    let mut entry = pair_entry(at, l.as_ref(), r.as_ref());
    let rel_path = entry.rel_path.clone();
    let left_rel = l
        .as_ref()
        .map(|x| join_rel(at.lrel, &x.name))
        .unwrap_or_default();
    // Rows the filters leave out are compared on metadata only, and their
    // folders walked only when asked to show what's in them.
    let inherited = entry.filtered_out();
    let descend = ctx.opts.recursive && (ctx.control.expand_filtered || !inherited);
    let focus = at.focus.and_then(|f| f.below(&entry.name));

    match (&l, &r) {
        (Some(l), None) => {
            if l.meta.is_dir && descend {
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
                        orphan_tree(&path, &rel_path, &left_rel, Side::Left, &sub, inherited, focus, ctx);
                    entry.counts = Some(aggregate(&entry.children));
                }
            }
        }
        (None, Some(r)) => {
            if r.meta.is_dir && descend {
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
                        orphan_tree(&path, &rel_path, &rel_path, Side::Right, &sub, inherited, focus, ctx);
                    entry.counts = Some(aggregate(&entry.children));
                }
            }
        }
        (Some(l), Some(r)) if l.meta.is_dir != r.meta.is_dir => {}
        (Some(l), Some(r)) if l.meta.is_dir => {
            if descend {
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
                    entry.detail = entry.status.into();
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
                    entry.detail = entry.status.into();
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
                    filtered: inherited,
                    focus,
                };
                match compare_dirs(&at, &sub, ctx) {
                    Ok(children) => {
                        entry.status = folder_status(&children);
                        entry.detail = entry.status.into();
                        entry.counts = Some(aggregate(&children));
                        entry.children = children;
                    }
                    Err(e) => {
                        entry.status = EntryStatus::Error;
                        entry.detail = StatusDetail::Error;
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
            match compare_file_pair(&lp, &rp, &l.meta, &r.meta, ctx, inherited) {
                Ok(verdict) => {
                    entry.status = verdict.status;
                    entry.crc = verdict.crc;
                    let as_links = !ctx.opts.follow_links
                        && (l.meta.link_target.is_some() || r.meta.link_target.is_some());
                    if ctx.opts.content != ContentMethod::Off && !as_links && !inherited {
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
                    let quick_differs = l.meta.size != r.meta.size
                        || !timestamps_match(l.meta.modified_ms, r.meta.modified_ms, ctx.opts);
                    entry.detail = StatusDetail::for_file(entry.status, entry.newer, quick_differs);
                }
                Err(e) => {
                    entry.status = EntryStatus::Error;
                    entry.detail = StatusDetail::Error;
                    entry.error = Some(e.to_string());
                }
            }
        }
        (None, None) => unreachable!("pairs always have at least one side"),
    }
    entry
}

/// A folder is the same when everything inside is, similar when the only
/// deviations are similar files, and different otherwise. Rows the filters
/// leave out don't count.
pub fn folder_status(children: &[FolderEntry]) -> EntryStatus {
    let counted = || children.iter().filter(|c| !c.filtered_out());
    if counted().all(|c| c.status == EntryStatus::Same) {
        EntryStatus::Same
    } else if counted().all(|c| matches!(c.status, EntryStatus::Same | EntryStatus::Similar)) {
        EntryStatus::Similar
    } else {
        EntryStatus::Different
    }
}

fn tally(entries: &[FolderEntry], stats: &mut FolderStats) {
    for e in entries {
        if e.filtered_out() {
            // Everything below is filtered too.
            if !e.is_dir {
                stats.filtered += 1;
            }
        } else if !e.is_dir {
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

/// Runs `f` with a comparison context over the two folders: the options
/// checked and compiled, and `control.scan` (or a throwaway one) to list
/// through.
fn with_ctx<R>(
    left_fs: &dyn Vfs,
    left: &str,
    right_fs: &dyn Vfs,
    right: &str,
    opts: &FolderCompareOptions,
    control: ScanControl,
    f: impl FnOnce(&Ctx, &Chain) -> Result<R>,
) -> Result<R> {
    // Fail fast on bad regexes rather than once per file pair.
    if opts.content == ContentMethod::Rules {
        text::LineNormalizer::new(&opts.rules)?;
    }
    let ci = opts.ignore_name_case;
    let filters = &opts.filters;
    let transient;
    let scan = match control.scan {
        Some(scan) => scan,
        None => {
            transient = FolderScan::transient();
            &transient
        }
    };
    let ctx = Ctx {
        opts,
        exclude: build_globset(&opts.exclude, ci)?,
        include_files: build_globset(&filters.include_files, ci)?,
        exclude_files: build_globset(&filters.exclude_files, ci)?,
        exclude_folders: build_globset(&filters.exclude_folders, ci)?,
        include_containing: containing_regex(&filters.include_containing, filters)?,
        exclude_containing: containing_regex(&filters.exclude_containing, filters)?,
        containing_key: fingerprint(&(
            &filters.include_containing,
            &filters.exclude_containing,
            filters.containing_regex,
            filters.containing_match_case,
        )),
        rules_key: fingerprint(&serde_json::to_string(&opts.rules).unwrap_or_default()),
        aligner: Aligner::new(&opts.align, ci)?,
        left: left_fs,
        right: right_fs,
        control,
        scan,
        roots: if opts.follow_links {
            [
                scan.canonical_of(left_fs, Side::Left, left),
                scan.canonical_of(right_fs, Side::Right, right),
            ]
        } else {
            [None, None]
        },
        reused: AtomicUsize::new(0),
        scanned: AtomicUsize::new(0),
        folders: AtomicUsize::new(0),
        compared: AtomicUsize::new(0),
        dirs_read: AtomicUsize::new(0),
    };
    let root = Chain {
        canonical: ctx.roots.clone(),
        parent: None,
    };
    f(&ctx, &root)
}

/// [`compare_folders_cached`] with progress reports and cancellation. With
/// [`ScanControl::scan`], only the stages `opts` change since the scan's
/// last run read anything (see [`FolderScan`]); the scan must belong to
/// these two folders.
pub fn compare_folders_controlled(
    left_fs: &dyn Vfs,
    left: &str,
    right_fs: &dyn Vfs,
    right: &str,
    opts: &FolderCompareOptions,
    control: ScanControl,
) -> Result<FolderCompareResult> {
    let started = Instant::now();
    with_ctx(left_fs, left, right_fs, right, opts, control, |ctx, root| {
        // Only once the options are known to be valid.
        let needs = control.scan.map(|scan| scan.restage(opts));
        let at = DirPair {
            ldir: left,
            rdir: right,
            rel: "",
            lrel: "",
            filtered: false,
            focus: None,
        };
        let entries = compare_dirs(&at, root, ctx)?;
        // Folders below the top level record a cancellation as their own error.
        ctx.check_cancel()?;
        let mut stats = FolderStats::default();
        tally(&entries, &mut stats);
        stats.counts = aggregate(&entries);
        stats.checksums_reused = ctx.reused.load(Ordering::Relaxed);
        stats.dirs_read = ctx.dirs_read.load(Ordering::Relaxed);
        Ok(FolderCompareResult {
            left_root: left.to_owned(),
            right_root: right.to_owned(),
            entries,
            stats,
            elapsed_ms: started.elapsed().as_millis(),
            needs,
        })
    })
}

/// Compares just the rows at `rels` — relative paths in the tree, `/`
/// separated, with the right side's names as in [`FolderEntry::rel_path`]
/// — each with everything below it, exactly as a full comparison with the
/// same options and `control` would report them: alignment, filters, links
/// and the checksum cache all apply. `None` where a full comparison would
/// have no such row (gone on both sides, or filtered out and not kept).
///
/// For after a file operation: swap each row into the old tree with
/// [`FolderCompareResult::replace_entry`] instead of rescanning. Only the
/// folders on the way down are listed (from [`ScanControl::scan`] when it
/// has them), and what the scan remembers about each row and the listing of
/// the folder holding it is dropped first, so changes there show. When an
/// operation creates or removes a folder on one side, pass that folder, not
/// just what's inside it: its own row changes. `levels` aren't reported.
pub fn compare_entries(
    left_fs: &dyn Vfs,
    left: &str,
    right_fs: &dyn Vfs,
    right: &str,
    rels: &[String],
    opts: &FolderCompareOptions,
    control: ScanControl,
) -> Result<Vec<Option<FolderEntry>>> {
    let mut focus = Focus::default();
    for rel in rels {
        if rel.split('/').any(str::is_empty) {
            return Err(CoreError::Other(format!(
                "\"{rel}\" isn't a path below the compared folders"
            )));
        }
        focus.insert(rel);
    }
    let control = ScanControl {
        levels: None,
        ..control
    };
    with_ctx(left_fs, left, right_fs, right, opts, control, |ctx, root| {
        let at = DirPair {
            ldir: left,
            rdir: right,
            rel: "",
            lrel: "",
            filtered: false,
            focus: Some(&focus),
        };
        // Only the paths down to each row: their folders' statuses are
        // partial and dropped.
        let spine = compare_dirs(&at, root, ctx)?;
        ctx.check_cancel()?;
        Ok(rels.iter().map(|rel| find_entry(&spine, rel).cloned()).collect())
    })
}

/// The row at `rel` in a tree.
fn find_entry<'a>(entries: &'a [FolderEntry], rel: &str) -> Option<&'a FolderEntry> {
    entries.iter().find_map(|e| {
        if e.rel_path == rel {
            Some(e)
        } else if e.is_dir && rel.strip_prefix(e.rel_path.as_str()).is_some_and(|r| r.starts_with('/')) {
            find_entry(&e.children, rel)
        } else {
            None
        }
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
        // The two counters advance separately, so a single report from parallel
        // listing may pair one's total with the other's previous value; each
        // still peaks at the full count.
        let scanned = seen.iter().map(|s| s.scanned).max().unwrap();
        let folders = seen.iter().map(|s| s.folders).max().unwrap();
        assert_eq!((scanned, folders), (20, 12));

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

#[cfg(test)]
mod staged_tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{Duration, SystemTime};

    /// The local file system, counting folder listings and file opens.
    #[derive(Default)]
    pub(super) struct CountingFs {
        listed: AtomicUsize,
        opened: AtomicUsize,
    }

    impl CountingFs {
        /// Listings and opens since the last call.
        pub(super) fn take(&self) -> (usize, usize) {
            (self.listed.swap(0, Ordering::Relaxed), self.opened.swap(0, Ordering::Relaxed))
        }
    }

    impl Vfs for CountingFs {
        fn read_dir(&self, dir: &str) -> Result<Vec<VfsEntry>> {
            self.read_dir_with(dir, ListOptions::default())
        }
        fn read_dir_with(&self, dir: &str, opts: ListOptions) -> Result<Vec<VfsEntry>> {
            self.listed.fetch_add(1, Ordering::Relaxed);
            LocalFs.read_dir_with(dir, opts)
        }
        fn open(&self, path: &str) -> Result<Box<dyn Read + Send>> {
            self.opened.fetch_add(1, Ordering::Relaxed);
            LocalFs.open(path)
        }
        fn join(&self, dir: &str, name: &str) -> String {
            LocalFs.join(dir, name)
        }
        fn canonical(&self, path: &str) -> Option<String> {
            LocalFs.canonical(path)
        }
    }

    struct Tree {
        _dir: tempfile::TempDir,
        l: String,
        r: String,
        /// Folders per side, the root included.
        folders: usize,
    }

    fn write(root: &str, rel: &str, contents: &[u8], modified_s: u64) {
        let p = PathBuf::from(root).join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, contents).unwrap();
        let t = SystemTime::UNIX_EPOCH + Duration::from_secs(modified_s);
        fs::File::options().write(true).open(&p).unwrap().set_modified(t).unwrap();
    }

    /// Twenty folders of a source file, a log and a file whose right copy
    /// is 5 s newer; `skip/` holds more of the same.
    fn tree() -> Tree {
        let dir = tempfile::tempdir().unwrap();
        let (l, r) = (dir.path().join("l"), dir.path().join("r"));
        let (l, r) = (l.to_string_lossy().into_owned(), r.to_string_lossy().into_owned());
        for d in (0..20).map(|i| format!("d{i}")).chain(["skip".into()]) {
            for root in [&l, &r] {
                write(root, &format!("{d}/main.rs"), b"fn main() {}", 1_000);
                write(root, &format!("{d}/run.log"), b"log", 1_000);
            }
            write(&l, &format!("{d}/late.txt"), b"same", 1_000);
            write(&r, &format!("{d}/late.txt"), b"same", 1_005);
        }
        Tree { _dir: dir, l, r, folders: 22 }
    }

    fn opts() -> FolderCompareOptions {
        FolderCompareOptions {
            ignore_name_case: false,
            exclude: vec!["skip".into()],
            ..Default::default()
        }
    }

    fn run(t: &Tree, fs: &CountingFs, scan: &FolderScan, opts: &FolderCompareOptions) -> FolderCompareResult {
        let control = ScanControl {
            scan: Some(scan),
            ..Default::default()
        };
        compare_folders_controlled(fs, &t.l, fs, &t.r, opts, control).unwrap()
    }

    #[test]
    fn filters_and_tolerance_restage_without_listing() {
        let t = tree();
        let (fs, scan) = (CountingFs::default(), FolderScan::new());
        let first = run(&t, &fs, &scan, &opts());
        // `skip` is excluded, so never listed.
        let listed = 2 * (t.folders - 1);
        assert_eq!(fs.take().0, listed);
        assert_eq!(first.stats.dirs_read, listed);
        assert_eq!(first.needs, Some(CompareNeeds::ALL));
        assert_eq!((first.stats.same, first.stats.different), (40, 20));

        // A name filter: re-filtered in memory.
        let no_logs = FolderCompareOptions {
            filters: FolderFilters {
                exclude_files: vec!["*.log".into()],
                ..Default::default()
            },
            ..opts()
        };
        let res = run(&t, &fs, &scan, &no_logs);
        assert_eq!(fs.take(), (0, 0), "a name filter must not read the disk");
        assert_eq!(res.stats.dirs_read, 0);
        let needs = res.needs.unwrap();
        assert!(needs.filter && !needs.list && !needs.compare);
        // Dropped rows aren't tallied at all.
        assert_eq!((res.stats.same, res.stats.different, res.stats.filtered), (20, 20, 0));

        // The timestamp tolerance: re-compared in memory.
        let lenient = FolderCompareOptions {
            time_tolerance_secs: 10,
            ..no_logs.clone()
        };
        let res = run(&t, &fs, &scan, &lenient);
        assert_eq!(fs.take(), (0, 0), "a quick-test change must not read the disk");
        let needs = res.needs.unwrap();
        assert!(needs.compare && !needs.list && !needs.filter && !needs.content);
        assert_eq!((res.stats.same, res.stats.different), (40, 0));

        // Unchanged options redo nothing.
        let res = run(&t, &fs, &scan, &lenient);
        assert!(!res.needs.unwrap().any());

        // Following links differently is the one change that lists again.
        let res = run(&t, &fs, &scan, &FolderCompareOptions { follow_links: false, ..lenient });
        assert!(res.needs.unwrap().list);
        assert_eq!(fs.take().0, listed);
    }

    #[test]
    fn content_pass_reuses_the_quick_listing() {
        let t = tree();
        let (fs, scan) = (CountingFs::default(), FolderScan::new());
        let quick = run(&t, &fs, &scan, &opts());
        assert_eq!(fs.take(), (2 * (t.folders - 1), 0));
        assert!(quick.entries.iter().all(|e| e.content_checked.is_none()));

        let crc = FolderCompareOptions {
            content: ContentMethod::Crc,
            ..opts()
        };
        let full = run(&t, &fs, &scan, &crc);
        let (listed, opened) = fs.take();
        assert_eq!(listed, 0, "the content pass lists nothing again");
        assert_eq!(opened, 2 * 3 * 20, "every pair read once");
        assert!(full.needs.unwrap().content);
        // Same bytes, so the newer copy no longer counts as different.
        assert_eq!((full.stats.same, full.stats.different), (60, 0));

        // A filter change keeps the verdicts: nothing is read again.
        let no_logs = FolderCompareOptions {
            filters: FolderFilters {
                exclude_files: vec!["*.log".into()],
                ..Default::default()
            },
            ..crc
        };
        let res = run(&t, &fs, &scan, &no_logs);
        assert_eq!(fs.take(), (0, 0));
        assert_eq!(res.stats.same, 40);
    }

    #[test]
    fn suppressed_filters_show_what_they_hide() {
        let t = tree();
        write(&t.l, "page.htm", b"<p>", 1_000);
        write(&t.r, "page.html", b"<p>", 1_000);
        let (fs, scan) = (CountingFs::default(), FolderScan::new());
        let opts = FolderCompareOptions {
            align: AlignOptions {
                extension_groups: vec!["htm,html".into()],
                ..Default::default()
            },
            filters: FolderFilters {
                exclude_files: vec!["*.htm".into()],
                ..Default::default()
            },
            ..opts()
        };
        let suppress = |expand_filtered| {
            let control = ScanControl {
                scan: Some(&scan),
                keep_filtered: true,
                expand_filtered,
                ..Default::default()
            };
            compare_folders_controlled(&fs, &t.l, &fs, &t.r, &opts, control).unwrap()
        };

        let res = suppress(false);
        let skip = res.entries.iter().find(|e| e.name == "skip").unwrap();
        assert_eq!(skip.filtered, Some(Filtered::Both));
        assert!(skip.children.is_empty(), "not walked unless asked");
        // One side filtered: the pair stays a pair, marked on that side.
        let page = res.entries.iter().find(|e| e.name == "page.html").unwrap();
        assert_eq!(page.filtered, Some(Filtered::Left));
        assert_eq!(page.status, EntryStatus::Same);
        assert!(page.left.is_some() && page.right.is_some());
        assert_eq!(res.stats.same, 41, "a half-filtered pair still counts");
        fs.take();

        // Walking filtered folders lists only the ones never listed.
        let res = suppress(true);
        assert_eq!(fs.take(), (2, 0), "just skip/, once per side");
        let skip = res.entries.iter().find(|e| e.name == "skip").unwrap();
        assert_eq!(skip.children.len(), 3);
        assert!(skip.children.iter().all(|c| c.filtered == Some(Filtered::Both)));
        assert_eq!(skip.children.iter().find(|c| c.name == "late.txt").unwrap().status, EntryStatus::Different);
        // Filtered rows count apart, and not in their folder's status.
        assert_eq!(res.stats.filtered, 3);
        assert_eq!(skip.status, EntryStatus::Same);

        // Without keep_filtered they're dropped, as before staging, and a
        // side excluded by name leaves its counterpart an orphan (what
        // Folder Sync plans by).
        let plain = run(&t, &fs, &scan, &opts);
        assert!(plain.entries.iter().all(|e| e.name != "skip"));
        assert_eq!(plain.stats.filtered, 0);
        let page = plain.entries.iter().find(|e| e.name == "page.html").unwrap();
        assert_eq!((page.status, page.filtered), (EntryStatus::RightOnly, None));
        assert!(plain.entries.iter().all(|e| e.name != "page.htm"));
    }

    #[test]
    fn needs_follow_the_option_that_changed() {
        let base = FolderCompareOptions::default();
        let with = |f: fn(&mut FolderCompareOptions)| {
            let mut o = base.clone();
            f(&mut o);
            compare_needs(&base, &o)
        };
        assert!(!compare_needs(&base, &base.clone()).any());
        assert_eq!(with(|o| o.follow_links = !o.follow_links), CompareNeeds { list: true, ..Default::default() });
        assert_eq!(with(|o| o.align.ignore_extensions = true), CompareNeeds { align: true, ..Default::default() });
        assert_eq!(with(|o| o.filters.min_size = Some(1)), CompareNeeds { filter: true, ..Default::default() });
        assert_eq!(
            with(|o| o.filters.include_containing = "x".into()),
            CompareNeeds { content_filter: true, ..Default::default() }
        );
        assert_eq!(with(|o| o.ignore_dst_shift = true), CompareNeeds { compare: true, ..Default::default() });
        assert_eq!(
            with(|o| o.content = ContentMethod::Binary),
            CompareNeeds { compare: true, content: true, ..Default::default() }
        );
        // Text rules only matter to rules-based content comparison.
        assert!(!with(|o| o.rules.ignore_case = true).any());
        let rules = FolderCompareOptions { content: ContentMethod::Rules, ..base.clone() };
        let mut ignoring = rules.clone();
        ignoring.rules.ignore_case = true;
        assert!(compare_needs(&rules, &ignoring).content);
    }
}

#[cfg(test)]
mod partial_tests {
    use super::staged_tests::CountingFs;
    use super::*;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, SystemTime};

    fn write(root: &Path, rel: &str, contents: &[u8], modified_s: u64) {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, contents).unwrap();
        stamp(root, rel, modified_s);
    }

    fn stamp(root: &Path, rel: &str, modified_s: u64) {
        let t = SystemTime::UNIX_EPOCH + Duration::from_secs(modified_s);
        fs::File::options()
            .write(true)
            .open(root.join(rel))
            .unwrap()
            .set_modified(t)
            .unwrap();
    }

    /// A comparison on screen, kept up to date row by row the way the
    /// folder view does after file operations.
    struct View {
        _dir: tempfile::TempDir,
        l: PathBuf,
        r: PathBuf,
        fs: CountingFs,
        scan: FolderScan,
        opts: FolderCompareOptions,
        res: FolderCompareResult,
    }

    impl View {
        fn sides(&self) -> (String, String) {
            (
                self.l.to_string_lossy().into_owned(),
                self.r.to_string_lossy().into_owned(),
            )
        }

        fn control(&self) -> ScanControl<'_> {
            ScanControl {
                scan: Some(&self.scan),
                keep_filtered: true,
                ..Default::default()
            }
        }

        /// Compares `rels` alone, swaps them in, and checks the tree and
        /// totals against a fresh full comparison.
        fn refresh(&mut self, rels: &[&str]) {
            let (l, r) = self.sides();
            let owned: Vec<String> = rels.iter().map(|s| s.to_string()).collect();
            let rows =
                compare_entries(&self.fs, &l, &self.fs, &r, &owned, &self.opts, self.control())
                    .unwrap();
            for (rel, row) in rels.iter().zip(rows) {
                assert!(self.res.replace_entry(rel, row), "{rel}: parent not in the tree");
            }
            let fresh = compare_folders_controlled(
                &LocalFs,
                &l,
                &LocalFs,
                &r,
                &self.opts,
                ScanControl {
                    keep_filtered: true,
                    ..Default::default()
                },
            )
            .unwrap();
            assert_eq!(
                tree_json(&self.res.entries),
                tree_json(&fresh.entries),
                "after refreshing {rels:?}"
            );
            let totals = |s: &FolderStats| {
                (
                    s.same,
                    s.similar,
                    s.different,
                    s.left_only,
                    s.right_only,
                    s.errors,
                    s.filtered,
                    s.counts,
                )
            };
            assert_eq!(totals(&self.res.stats), totals(&fresh.stats), "totals after {rels:?}");
        }

        fn row(&self, rel: &str) -> Option<&FolderEntry> {
            find_entry(&self.res.entries, rel)
        }
    }

    /// Every row as JSON, minus folders' own timestamps: a change inside a
    /// folder moves them, and [`compare_entries`] leaves the rows above the
    /// ones it compares as they were listed.
    fn tree_json(entries: &[FolderEntry]) -> serde_json::Value {
        fn strip(v: &mut serde_json::Value) {
            if let Some(rows) = v.as_array_mut() {
                for row in rows {
                    if row["isDir"] == true {
                        for side in ["left", "right"] {
                            if let Some(meta) = row[side].as_object_mut() {
                                meta.remove("modifiedMs");
                            }
                        }
                    }
                    strip(&mut row["children"]);
                }
            }
        }
        let mut v = serde_json::to_value(entries).unwrap();
        strip(&mut v);
        v
    }

    fn view() -> View {
        let dir = tempfile::tempdir().unwrap();
        let (l, r) = (dir.path().join("l"), dir.path().join("r"));
        write(&l, "a/b/changed.txt", b"old", 1_000);
        write(&r, "a/b/changed.txt", b"new!", 2_000);
        write(&l, "a/b/orphan.txt", b"o", 1_000);
        write(&l, "a/b/left-dir/x.txt", b"x", 1_000);
        write(&l, "a/keep.txt", b"k", 1_000);
        write(&r, "a/keep.txt", b"k", 1_000);
        // Filtered out, reported because the view keeps such rows.
        write(&l, "a/run.log", b"one", 1_000);
        write(&r, "a/run.log", b"two", 1_000);
        // Differently named folders, paired by an alignment rule.
        write(&l, "Apples/2024.csv", b"1", 1_000);
        write(&r, "Bananas/2024.csv", b"22", 1_000);
        write(&l, "same.bin", b"abcd", 1_000);
        write(&r, "same.bin", b"abcd", 1_000);
        write(&r, "ronly/inner/deep.txt", b"d", 1_000);
        // Differs only in what the filters leave out.
        write(&l, "logs/x.log", b"1", 1_000);
        write(&r, "logs/x.log", b"22", 2_000);
        write(&l, "logs/ok.txt", b"ok", 1_000);
        write(&r, "logs/ok.txt", b"ok", 1_000);
        let opts = FolderCompareOptions {
            ignore_name_case: false,
            content: ContentMethod::Binary,
            align: align::AlignOptions {
                rules: vec![align::AlignRule {
                    left: "Apples*".into(),
                    right: "Bananas*".into(),
                }],
                ..Default::default()
            },
            filters: FolderFilters {
                exclude_files: vec!["*.log".into()],
                ..Default::default()
            },
            ..Default::default()
        };
        let mut v = View {
            _dir: dir,
            l,
            r,
            fs: CountingFs::default(),
            scan: FolderScan::new(),
            opts,
            res: FolderCompareResult {
                left_root: String::new(),
                right_root: String::new(),
                entries: Vec::new(),
                stats: FolderStats::default(),
                elapsed_ms: 0,
                needs: None,
            },
        };
        let (l, r) = v.sides();
        v.res = compare_folders_controlled(&v.fs, &l, &v.fs, &r, &v.opts, v.control()).unwrap();
        v.fs.take();
        v
    }

    #[test]
    fn one_row_refreshed_matches_a_rescan() {
        let mut v = view();
        assert_eq!(v.row("a/b/changed.txt").unwrap().detail, StatusDetail::RightNewer);

        // Copy left → right, keeping the date: only a/b is listed again.
        write(&v.r, "a/b/changed.txt", b"old", 1_000);
        v.refresh(&["a/b/changed.txt"]);
        assert_eq!(v.fs.take().0, 2, "a/b on both sides, nothing else");
        assert_eq!(v.row("a/b/changed.txt").unwrap().status, EntryStatus::Same);

        // Delete an orphan: its row goes.
        fs::remove_file(v.l.join("a/b/orphan.txt")).unwrap();
        v.refresh(&["a/b/orphan.txt"]);
        assert!(v.row("a/b/orphan.txt").is_none());

        // A new folder on the right: inserted in order.
        fs::create_dir(v.r.join("a/b/new-folder")).unwrap();
        v.refresh(&["a/b/new-folder"]);
        assert_eq!(v.row("a/b/new-folder").unwrap().status, EntryStatus::RightOnly);

        // An orphaned folder copied across: the subtree is replaced.
        write(&v.r, "a/b/left-dir/x.txt", b"x", 1_000);
        v.refresh(&["a/b/left-dir"]);
        assert_eq!(v.row("a/b/left-dir/x.txt").unwrap().status, EntryStatus::Same);

        // Then deleted on the left: the folder turns right-only.
        fs::remove_dir_all(v.l.join("a/b/left-dir")).unwrap();
        v.refresh(&["a/b/left-dir"]);
        assert_eq!(v.row("a/b/left-dir").unwrap().status, EntryStatus::RightOnly);
    }

    #[test]
    fn alignment_filters_and_kept_verdicts_hold() {
        let mut v = view();
        // A row below aligned folders goes by the right side's names.
        write(&v.r, "Bananas/2024.csv", b"1", 1_000);
        v.refresh(&["Bananas/2024.csv"]);
        let csv = v.row("Bananas/2024.csv").unwrap();
        assert_eq!(
            (csv.status, csv.left_rel_path.as_deref()),
            (EntryStatus::Same, Some("Apples/2024.csv"))
        );

        // Same size and date, other bytes: the kept verdict must not be reused.
        write(&v.r, "same.bin", b"abce", 1_000);
        v.refresh(&["same.bin"]);
        assert_eq!(v.row("same.bin").unwrap().status, EntryStatus::Different);

        // A filtered row stays marked and out of the counts.
        write(&v.l, "a/run.log", b"three", 3_000);
        v.refresh(&["a/run.log"]);
        assert_eq!(v.row("a/run.log").unwrap().filtered, Some(Filtered::Both));
        assert_eq!(v.row("a").unwrap().status, EntryStatus::Different, "b/ still differs");
        write(&v.r, "logs/ok.txt", b"ok", 1_000);
        v.refresh(&["logs/ok.txt"]);
        assert_eq!(v.row("logs").unwrap().status, EntryStatus::Same, "x.log doesn't count");

        // Touched: the same bytes with another date.
        stamp(&v.l, "a/keep.txt", 5_000);
        v.refresh(&["a/keep.txt"]);
        assert_eq!(v.row("a/keep.txt").unwrap().detail, StatusDetail::SameDiff);
    }

    #[test]
    fn folders_created_by_a_copy_refresh_from_the_top_one() {
        let mut v = view();
        // Copying a file out of a right-only folder creates its folders on
        // the left: the topmost one's row changes, so that's what's refreshed.
        write(&v.l, "ronly/inner/deep.txt", b"d", 1_000);
        v.refresh(&["ronly"]);
        assert_eq!(v.row("ronly").unwrap().status, EntryStatus::Same);

        // Several rows at once, one inside another and one gone.
        write(&v.l, "ronly/inner/more.txt", b"m", 1_000);
        fs::remove_file(v.l.join("same.bin")).unwrap();
        fs::remove_file(v.r.join("same.bin")).unwrap();
        v.refresh(&["ronly/inner", "ronly/inner/more.txt", "same.bin"]);
        assert!(v.row("same.bin").is_none());
    }

    #[test]
    fn paths_must_be_below_the_roots() {
        let v = view();
        let (l, r) = v.sides();
        for bad in ["", "a//b", "/a"] {
            let rels = [bad.to_string()];
            assert!(
                compare_entries(&LocalFs, &l, &LocalFs, &r, &rels, &v.opts, v.control()).is_err(),
                "{bad:?}"
            );
        }
    }
}
