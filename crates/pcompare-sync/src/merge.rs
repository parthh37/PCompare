//! Folder merge: classify every path in `left` and `right` against their
//! common ancestor `base`, propose an action per item, then build the merged
//! tree in an output folder from the (user-adjusted) decisions.
//!
//! The base is optional. Without it the merge is two-way: items are judged
//! by left vs right alone (only on one side, same, newer, different). With
//! it, text files changed on both sides are merged line by line
//! ([`pcompare_merge`]); when that leaves no conflicts the merged text is
//! proposed as [`MergeAction::MergeText`].
//!
//! The walk goes through [`Vfs`], so inputs may be remote for comparison;
//! execution needs local folders.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use pcompare_core::folder::{self, ContentMethod, EntryMeta, EntryStatus, FolderCompareOptions};
use pcompare_core::text::TextDiffOptions;
use pcompare_core::textfile::{self, TextFormat};
use pcompare_core::vfs::{LocalFs, Vfs};
use pcompare_core::{CoreError, Result};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};

use crate::fsops::{self, Deleter};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MergeClass {
    Unchanged,
    LeftChanged,
    RightChanged,
    BothChangedSame,
    /// Both sides changed the file differently.
    Conflict,
    LeftAdded,
    RightAdded,
    BothAddedSame,
    BothAddedConflict,
    LeftDeleted,
    RightDeleted,
    BothDeleted,
    /// Deleted on one side, modified on the other.
    DeleteModifyConflict,
    /// A file on some side and a folder on another.
    TypeConflict,
    /// Folders only: both sides changed different descendants, no conflicts.
    Merged,
    /// The item could not be read or compared.
    Error,
    /// Two-way only: present on the left side only.
    LeftOnly,
    /// Two-way only: present on the right side only.
    RightOnly,
    /// Two-way only: the files differ and the left one is newer.
    LeftNewer,
    /// Two-way only: the files differ and the right one is newer.
    RightNewer,
    /// Two-way only: the files differ and neither is newer (or "take newer"
    /// is off); folders: something inside differs that needs a decision.
    Different,
    /// A text file changed on both sides whose line-by-line merge has no
    /// conflicts.
    AutoMerged,
}

impl MergeClass {
    pub fn is_conflict(self) -> bool {
        matches!(
            self,
            MergeClass::Conflict
                | MergeClass::BothAddedConflict
                | MergeClass::DeleteModifyConflict
                | MergeClass::TypeConflict
                | MergeClass::Error
                | MergeClass::Different
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MergeAction {
    TakeLeft,
    TakeRight,
    TakeBase,
    /// Ensure the item is absent from the output.
    Delete,
    /// Leave the output untouched (unresolved conflicts default to this).
    Skip,
    /// Write the line-by-line merge of the three text versions; offered
    /// when that merge has no conflicts ("Merged automatically").
    MergeText,
}

/// Settings of a folder merge beyond the comparison options.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct FolderMergeOptions {
    /// Three-way: merge text files changed on both sides line by line and
    /// propose the result when no conflicts remain.
    pub auto_merge_text: bool,
    /// Two-way: propose the newer file where the sides differ; otherwise
    /// every difference is a conflict.
    pub take_newer: bool,
}

impl Default for FolderMergeOptions {
    fn default() -> Self {
        Self {
            auto_merge_text: true,
            take_newer: true,
        }
    }
}

/// How a line-by-line text merge of a file went.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TextMergeSummary {
    /// Changes merged without user input.
    pub changes: usize,
    /// Conflicting regions left for the user (a Text Merge session).
    pub conflicts: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FolderMergeEntry {
    pub name: String,
    pub rel_path: String,
    pub is_dir: bool,
    pub class: MergeClass,
    /// The proposed default; for folders it governs only the folder itself.
    pub action: MergeAction,
    pub base: Option<EntryMeta>,
    pub left: Option<EntryMeta>,
    pub right: Option<EntryMeta>,
    pub children: Vec<FolderMergeEntry>,
    pub error: Option<String>,
    /// Set when the file was merged line by line (both sides changed text).
    pub text_merge: Option<TextMergeSummary>,
    /// Conflicting items at or below this entry.
    pub conflicts: usize,
    /// Changed (non-unchanged) items at or below this entry.
    pub changes: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FolderMergeResult {
    /// Empty in a two-way merge.
    pub base_root: String,
    pub left_root: String,
    pub right_root: String,
    pub entries: Vec<FolderMergeEntry>,
    /// File-level counts per classification.
    pub counts: BTreeMap<MergeClass, usize>,
    pub conflicts: usize,
    pub changes: usize,
    /// No base folder: items were classified by left vs right alone.
    pub two_way: bool,
    pub elapsed_ms: u128,
}

const BASE: usize = 0;
const LEFT: usize = 1;
const RIGHT: usize = 2;

struct Ctx<'a> {
    opts: &'a FolderCompareOptions,
    merge_opts: &'a FolderMergeOptions,
    /// No base: `fs[BASE]` is a placeholder that is never read.
    two_way: bool,
    exclude: GlobSet,
    include_files: GlobSet,
    exclude_files: GlobSet,
    exclude_folders: GlobSet,
    fs: [&'a dyn Vfs; 3],
}

struct Listed {
    name: String,
    meta: EntryMeta,
}

/// Same matching rules as the two-way folder engine, so a merge session and
/// a folder session with the same filters see the same items.
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

fn join_rel(rel: &str, name: &str) -> String {
    if rel.is_empty() {
        name.to_owned()
    } else {
        format!("{rel}/{name}")
    }
}

fn list_dir(ctx: &Ctx, side: usize, dir: &str, rel: &str) -> Result<BTreeMap<String, Listed>> {
    let mut out = BTreeMap::new();
    for entry in ctx.fs[side].read_dir(dir)? {
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

type Sides<T> = [Option<T>; 3];

fn side_meta(items: &Sides<Listed>, side: usize) -> Option<&EntryMeta> {
    items[side].as_ref().map(|x| &x.meta)
}

fn walk_dir(dirs: &Sides<String>, rel: &str, ctx: &Ctx) -> Result<Vec<FolderMergeEntry>> {
    let mut keyed: BTreeMap<String, Sides<Listed>> = BTreeMap::new();
    for side in [BASE, LEFT, RIGHT] {
        if let Some(dir) = &dirs[side] {
            for (key, item) in list_dir(ctx, side, dir, rel)? {
                keyed.entry(key).or_default()[side] = Some(item);
            }
        }
    }
    let filters = &ctx.opts.filters;
    let mut entries: Vec<FolderMergeEntry> = keyed
        .into_values()
        // Size/date filters judge the left/right pair; where a side deleted
        // the file, its base version stands in.
        .filter(|items| {
            items.iter().flatten().any(|x| x.meta.is_dir)
                || folder::pair_passes(
                    side_meta(items, LEFT).or(side_meta(items, BASE)),
                    side_meta(items, RIGHT).or(side_meta(items, BASE)),
                    filters,
                )
        })
        .collect::<Vec<_>>()
        .into_par_iter()
        .map(|items| node(dirs, rel, items, ctx))
        .collect();
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    Ok(entries)
}

/// Equality of two present files: the quick tests (size, timestamp), then
/// the content method, exactly as a two-way folder compare would decide.
/// Rules-based "similar" files count as equal.
fn same_file(
    ctx: &Ctx,
    paths: &Sides<String>,
    items: &Sides<Listed>,
    a: usize,
    b: usize,
) -> Result<bool> {
    let (ma, mb) = match (&items[a], &items[b]) {
        (Some(x), Some(y)) => (&x.meta, &y.meta),
        _ => return Ok(false),
    };
    let opts = ctx.opts;
    let quick_same = (!opts.compare_size || ma.size == mb.size)
        && (!opts.compare_timestamp
            || folder::timestamps_match(ma.modified_ms, mb.modified_ms, opts));
    if opts.content == ContentMethod::Off || (!quick_same && !opts.override_quick_tests) {
        return Ok(quick_same);
    }
    let (pa, pb) = (paths[a].as_deref().unwrap(), paths[b].as_deref().unwrap());
    let verdict = folder::compare_contents(
        ctx.fs[a],
        pa,
        ma.size,
        ctx.fs[b],
        pb,
        mb.size,
        opts.content,
        &opts.rules,
    )?;
    Ok(matches!(
        verdict.status,
        EntryStatus::Same | EntryStatus::Similar
    ))
}

/// Largest file merged line by line; bigger ones stay conflicts.
const AUTO_MERGE_MAX_BYTES: u64 = 16 << 20;

/// A line-by-line merge of the three versions of a text file.
struct TextMerged {
    summary: TextMergeSummary,
    /// The merged file in the left version's encoding, BOM and line-ending
    /// style; `None` while conflicts remain or when the merged text cannot be
    /// represented in that encoding.
    bytes: Option<Vec<u8>>,
}

/// Merges three file versions line by line; `None` when any looks binary.
/// Lines are compared exactly: what counts as "the same change" must not
/// depend on the comparison's whitespace or ignore rules.
fn merge_text_bytes(base: &[u8], left: &[u8], right: &[u8]) -> Option<TextMerged> {
    if [base, left, right]
        .iter()
        .any(|b| textfile::is_probably_binary(b))
    {
        return None;
    }
    let [(b, _, _), (l, encoding, has_bom), (r, _, _)] = [base, left, right].map(textfile::decode);
    let merged = pcompare_merge::merge_text(
        Some(&b),
        &l,
        &r,
        &TextDiffOptions::default(),
        &pcompare_merge::MergeRules::default(),
    )
    .ok()?;
    let stats = &merged.merge.stats;
    let summary = TextMergeSummary {
        changes: stats.auto_merged(),
        conflicts: stats.conflicts,
    };
    let bytes = if stats.conflicts == 0 {
        let format = TextFormat {
            encoding: encoding.name().to_owned(),
            has_bom,
            line_ending: textfile::save_line_ending(&l),
        };
        textfile::encode_text(&merged.text, &format).ok()
    } else {
        None
    };
    Some(TextMerged { summary, bytes })
}

fn read_all(fs: &dyn Vfs, path: &str) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    fs.open(path)?
        .read_to_end(&mut buf)
        .map_err(|e| CoreError::io(path, e))?;
    Ok(buf)
}

/// The automatic text merge of a file changed differently on both sides,
/// when enabled and the file is small enough.
fn try_text_merge(
    ctx: &Ctx,
    paths: &Sides<String>,
    items: &Sides<Listed>,
) -> Result<Option<TextMerged>> {
    if !ctx.merge_opts.auto_merge_text
        || items
            .iter()
            .flatten()
            .any(|x| x.meta.size > AUTO_MERGE_MAX_BYTES)
    {
        return Ok(None);
    }
    let read = |s: usize| read_all(ctx.fs[s], paths[s].as_deref().unwrap_or_default());
    Ok(merge_text_bytes(&read(BASE)?, &read(LEFT)?, &read(RIGHT)?))
}

/// Left vs right only: orphans are copied across, the newer of two
/// differing files is proposed (when enabled), anything else conflicts.
fn classify_two_way(
    ctx: &Ctx,
    paths: &Sides<String>,
    items: &Sides<Listed>,
) -> Result<(MergeClass, MergeAction)> {
    use MergeAction::*;
    use MergeClass::*;
    Ok(match (&items[LEFT], &items[RIGHT]) {
        (Some(_), None) => (LeftOnly, TakeLeft),
        (None, Some(_)) => (RightOnly, TakeRight),
        (Some(_), Some(_)) if same_file(ctx, paths, items, LEFT, RIGHT)? => (Unchanged, TakeLeft),
        (Some(l), Some(r)) => match (l.meta.modified_ms, r.meta.modified_ms) {
            (Some(lm), Some(rm))
                if ctx.merge_opts.take_newer
                    && !folder::timestamps_match(Some(lm), Some(rm), ctx.opts) =>
            {
                if lm > rm {
                    (LeftNewer, TakeLeft)
                } else {
                    (RightNewer, TakeRight)
                }
            }
            _ => (Different, Skip),
        },
        (None, None) => unreachable!("an entry exists on at least one side"),
    })
}

fn classify_file(
    ctx: &Ctx,
    paths: &Sides<String>,
    items: &Sides<Listed>,
) -> Result<(MergeClass, MergeAction, Option<TextMergeSummary>)> {
    use MergeAction::*;
    use MergeClass::*;
    if ctx.two_way {
        let (class, action) = classify_two_way(ctx, paths, items)?;
        return Ok((class, action, None));
    }
    let eq = |a, b| same_file(ctx, paths, items, a, b);
    let has = |s: usize| items[s].is_some();
    let mut text = None;
    let (class, action) = match (has(BASE), has(LEFT), has(RIGHT)) {
        (true, true, true) => match (eq(LEFT, BASE)?, eq(RIGHT, BASE)?) {
            (true, true) => (Unchanged, TakeLeft),
            (false, true) => (LeftChanged, TakeLeft),
            (true, false) => (RightChanged, TakeRight),
            (false, false) if eq(LEFT, RIGHT)? => (BothChangedSame, TakeLeft),
            (false, false) => match try_text_merge(ctx, paths, items)? {
                Some(merged) => {
                    text = Some(merged.summary);
                    if merged.bytes.is_some() {
                        (AutoMerged, MergeText)
                    } else {
                        (Conflict, Skip)
                    }
                }
                None => (Conflict, Skip),
            },
        },
        (true, true, false) if eq(LEFT, BASE)? => (RightDeleted, Delete),
        (true, false, true) if eq(RIGHT, BASE)? => (LeftDeleted, Delete),
        (true, true, false) | (true, false, true) => (DeleteModifyConflict, Skip),
        (true, false, false) => (BothDeleted, Delete),
        (false, true, true) if eq(LEFT, RIGHT)? => (BothAddedSame, TakeLeft),
        (false, true, true) => (BothAddedConflict, Skip),
        (false, true, false) => (LeftAdded, TakeLeft),
        (false, false, true) => (RightAdded, TakeRight),
        (false, false, false) => unreachable!("an entry exists on at least one side"),
    };
    Ok((class, action, text))
}

/// Which sides a (non-conflicting) change came from.
fn change_sides(class: MergeClass) -> (bool, bool, bool) {
    use MergeClass::*;
    match class {
        LeftChanged | LeftAdded | LeftDeleted | LeftOnly | LeftNewer => (true, false, false),
        RightChanged | RightAdded | RightDeleted | RightOnly | RightNewer => (false, true, false),
        BothChangedSame | BothAddedSame | BothDeleted => (false, false, true),
        Merged | AutoMerged => (true, true, false),
        _ => (false, false, false),
    }
}

fn classify_folder(
    present: (bool, bool, bool),
    children: &[FolderMergeEntry],
) -> (MergeClass, MergeAction) {
    use MergeAction::*;
    use MergeClass::*;
    let conflicts = children.iter().any(|c| c.conflicts > 0);
    let (mut l, mut r, mut same) = (false, false, false);
    for c in children {
        let (cl, cr, cs) = change_sides(c.class);
        l |= cl;
        r |= cr;
        same |= cs;
    }
    let keep = if present.1 {
        TakeLeft
    } else if present.2 {
        TakeRight
    } else {
        TakeBase
    };
    match present {
        (true, true, true) if conflicts => (Conflict, keep),
        (true, true, true) if l && r => (Merged, keep),
        (true, true, true) if l => (LeftChanged, keep),
        (true, true, true) if r => (RightChanged, keep),
        (true, true, true) if same => (BothChangedSame, keep),
        (true, true, true) => (Unchanged, keep),
        (false, true, false) => (LeftAdded, keep),
        (false, false, true) => (RightAdded, keep),
        (false, true, true) if conflicts => (BothAddedConflict, keep),
        (false, true, true) => (BothAddedSame, keep),
        (true, false, true) | (true, true, false) if conflicts => (DeleteModifyConflict, Skip),
        (true, false, true) => (LeftDeleted, Delete),
        (true, true, false) => (RightDeleted, Delete),
        (true, false, false) => (BothDeleted, Delete),
        (false, false, false) => unreachable!("an entry exists on at least one side"),
    }
}

/// Two-way folders: orphans are copied across; a folder on both sides sums
/// up its contents.
fn classify_folder_two_way(
    present: (bool, bool),
    children: &[FolderMergeEntry],
) -> (MergeClass, MergeAction) {
    use MergeAction::*;
    use MergeClass::*;
    match present {
        (true, false) => (LeftOnly, TakeLeft),
        (false, true) => (RightOnly, TakeRight),
        _ if children.iter().any(|c| c.conflicts > 0) => (Different, TakeLeft),
        _ if children.iter().any(|c| c.class != Unchanged) => (Merged, TakeLeft),
        _ => (Unchanged, TakeLeft),
    }
}

fn node(dirs: &Sides<String>, rel: &str, items: Sides<Listed>, ctx: &Ctx) -> FolderMergeEntry {
    let name = [LEFT, RIGHT, BASE]
        .iter()
        .find_map(|&s| items[s].as_ref().map(|x| x.name.clone()))
        .unwrap_or_default();
    let rel_path = join_rel(rel, &name);
    let meta = |s: usize| items[s].as_ref().map(|x| x.meta.clone());
    let paths: Sides<String> = std::array::from_fn(|s| {
        let dir = dirs[s].as_deref()?;
        items[s].as_ref().map(|x| ctx.fs[s].join(dir, &x.name))
    });
    let mut entry = FolderMergeEntry {
        name,
        rel_path: rel_path.clone(),
        is_dir: false,
        class: MergeClass::Unchanged,
        action: MergeAction::Skip,
        base: meta(BASE),
        left: meta(LEFT),
        right: meta(RIGHT),
        children: Vec::new(),
        error: None,
        text_merge: None,
        conflicts: 0,
        changes: 0,
    };

    let kinds: Vec<bool> = items.iter().flatten().map(|x| x.meta.is_dir).collect();
    let fail = |entry: &mut FolderMergeEntry, class: MergeClass, msg: Option<String>| {
        entry.class = class;
        entry.action = MergeAction::Skip;
        entry.error = msg;
        entry.conflicts = 1;
        entry.changes = 1;
    };

    if kinds.iter().any(|&d| d != kinds[0]) {
        fail(&mut entry, MergeClass::TypeConflict, None);
    } else if kinds[0] {
        entry.is_dir = true;
        match walk_dir(&paths, &rel_path, ctx) {
            Ok(children) => {
                let present = (
                    items[BASE].is_some(),
                    items[LEFT].is_some(),
                    items[RIGHT].is_some(),
                );
                (entry.class, entry.action) = if ctx.two_way {
                    classify_folder_two_way((present.1, present.2), &children)
                } else {
                    classify_folder(present, &children)
                };
                entry.conflicts = children.iter().map(|c| c.conflicts).sum();
                entry.changes = children.iter().map(|c| c.changes).sum();
                // An empty folder that was itself added or deleted is a change.
                if entry.changes == 0 && entry.class != MergeClass::Unchanged {
                    entry.changes = 1;
                }
                entry.children = children;
            }
            Err(e) => fail(&mut entry, MergeClass::Error, Some(e.to_string())),
        }
    } else {
        match classify_file(ctx, &paths, &items) {
            Ok((class, action, text)) => {
                entry.class = class;
                entry.action = action;
                entry.text_merge = text;
                entry.conflicts = class.is_conflict() as usize;
                entry.changes = (class != MergeClass::Unchanged) as usize;
            }
            Err(e) => fail(&mut entry, MergeClass::Error, Some(e.to_string())),
        }
    }
    entry
}

fn tally(entries: &[FolderMergeEntry], counts: &mut BTreeMap<MergeClass, usize>) {
    for e in entries {
        if !e.is_dir || e.class == MergeClass::Error {
            *counts.entry(e.class).or_default() += 1;
        }
        tally(&e.children, counts);
    }
}

/// Merge comparison of folders that may live on different file systems:
/// three-way against `base`, or two-way when `base` is `None`.
pub fn compare_merge_with(
    base: Option<(&dyn Vfs, &str)>,
    left: (&dyn Vfs, &str),
    right: (&dyn Vfs, &str),
    opts: &FolderCompareOptions,
    merge_opts: &FolderMergeOptions,
) -> Result<FolderMergeResult> {
    let started = Instant::now();
    // Fail fast on bad ignore-pattern regexes rather than once per file.
    if opts.content == ContentMethod::Rules {
        pcompare_core::text::LineNormalizer::new(&opts.rules)?;
    }
    let ci = opts.ignore_name_case;
    let ctx = Ctx {
        opts,
        merge_opts,
        two_way: base.is_none(),
        exclude: build_globset(&opts.exclude, ci)?,
        include_files: build_globset(&opts.filters.include_files, ci)?,
        exclude_files: build_globset(&opts.filters.exclude_files, ci)?,
        exclude_folders: build_globset(&opts.filters.exclude_folders, ci)?,
        fs: [base.map_or(left.0, |b| b.0), left.0, right.0],
    };
    let roots = [
        base.map(|b| b.1.to_owned()),
        Some(left.1.to_owned()),
        Some(right.1.to_owned()),
    ];
    let entries = walk_dir(&roots, "", &ctx)?;
    let mut counts = BTreeMap::new();
    tally(&entries, &mut counts);
    Ok(FolderMergeResult {
        base_root: base.map(|b| b.1.to_owned()).unwrap_or_default(),
        left_root: left.1.to_owned(),
        right_root: right.1.to_owned(),
        conflicts: entries.iter().map(|e| e.conflicts).sum(),
        changes: entries.iter().map(|e| e.changes).sum(),
        two_way: ctx.two_way,
        entries,
        counts,
        elapsed_ms: started.elapsed().as_millis(),
    })
}

/// Local-disk convenience wrapper around [`compare_merge_with`].
pub fn compare_merge(
    base: Option<&Path>,
    left: impl AsRef<Path>,
    right: impl AsRef<Path>,
    opts: &FolderCompareOptions,
    merge_opts: &FolderMergeOptions,
) -> Result<FolderMergeResult> {
    let s = |p: &Path| p.to_string_lossy().into_owned();
    let (b, l, r) = (base.map(s), s(left.as_ref()), s(right.as_ref()));
    compare_merge_with(
        b.as_deref().map(|b| (&LocalFs as &dyn Vfs, b)),
        (&LocalFs, &l),
        (&LocalFs, &r),
        opts,
        merge_opts,
    )
}

// ---------- execution ----------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MergeDecision {
    pub rel_path: String,
    pub is_dir: bool,
    pub action: MergeAction,
}

/// The default decisions of a comparison, parents before children.
pub fn default_decisions(result: &FolderMergeResult) -> Vec<MergeDecision> {
    fn walk(entries: &[FolderMergeEntry], out: &mut Vec<MergeDecision>) {
        for e in entries {
            out.push(MergeDecision {
                rel_path: e.rel_path.clone(),
                is_dir: e.is_dir,
                action: e.action,
            });
            walk(&e.children, out);
        }
    }
    let mut out = Vec::new();
    walk(&result.entries, &mut out);
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum MergeOutcome {
    Written,
    Created,
    Deleted,
    Skipped,
    Failed,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MergeItemResult {
    pub rel_path: String,
    pub is_dir: bool,
    pub outcome: MergeOutcome,
    pub error: Option<String>,
    /// Where the overwritten version went (in a versioning folder). When it
    /// couldn't be disposed of, `error` says where it was kept instead.
    pub backup: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MergeReport {
    /// Every item that changed the output, was skipped, or failed. Items that
    /// already matched are only counted in `unchanged`.
    pub results: Vec<MergeItemResult>,
    pub written: usize,
    pub created: usize,
    pub deleted: usize,
    pub unchanged: usize,
    pub skipped: usize,
    pub failed: usize,
    pub bytes_written: u64,
    pub elapsed_ms: u128,
    /// Stopped by the user; the remaining decisions were not applied.
    pub cancelled: bool,
}

/// Progress of [`execute_merge_with`], sent after every decision.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MergeProgress {
    /// Decisions handled so far, out of `total`.
    pub done: usize,
    pub total: usize,
    /// The item being worked on; `None` once finished.
    pub current: Option<String>,
    pub bytes_written: u64,
}

/// Cancellation and progress for a running folder merge.
pub struct MergeRun<'a> {
    /// Set to stop at the next item (or inside a large copy).
    pub cancel: &'a AtomicBool,
    pub on_progress: &'a mut dyn FnMut(&MergeProgress),
}

impl MergeReport {
    fn record(
        &mut self,
        rel_path: &str,
        is_dir: bool,
        outcome: MergeOutcome,
        error: Option<String>,
    ) {
        self.record_with_backup(rel_path, is_dir, outcome, error, None);
    }

    fn record_with_backup(
        &mut self,
        rel_path: &str,
        is_dir: bool,
        outcome: MergeOutcome,
        error: Option<String>,
        backup: Option<String>,
    ) {
        match outcome {
            MergeOutcome::Written => self.written += 1,
            MergeOutcome::Created => self.created += 1,
            MergeOutcome::Deleted => self.deleted += 1,
            MergeOutcome::Skipped => self.skipped += 1,
            MergeOutcome::Failed => self.failed += 1,
        }
        self.results.push(MergeItemResult {
            rel_path: rel_path.to_owned(),
            is_dir,
            outcome,
            error,
            backup,
        });
    }
}

fn other(msg: impl Into<String>) -> CoreError {
    CoreError::Other(msg.into())
}

/// Rejects outputs that overlap an input without being identical to it:
/// writing into `left/merged` would make the merge read its own output.
fn validate_output(inputs: &[(&str, &Path)], output: &Path) -> Result<PathBuf> {
    if output.as_os_str().is_empty() {
        return Err(other("Choose an output folder for the merge."));
    }
    let out = fsops::canonical_lenient(output).map_err(|e| CoreError::io(output, e))?;
    if out.is_file() {
        return Err(other(format!(
            "The output {} is a file, not a folder.",
            output.display()
        )));
    }
    for (label, input) in inputs {
        let inp = input.canonicalize().map_err(|e| CoreError::io(*input, e))?;
        if out != inp && out.starts_with(&inp) {
            return Err(other(format!(
                "The output folder is inside the {label} folder."
            )));
        }
        if out != inp && inp.starts_with(&out) {
            return Err(other(format!(
                "The {label} folder is inside the output folder."
            )));
        }
    }
    Ok(out)
}

/// The outcome of one decision (`None` when the output already matched) and
/// the bytes it wrote.
type Step = (Option<MergeOutcome>, u64);

/// Builds the merged tree in `output` from `decisions` (parents before
/// children, as produced by [`default_decisions`]).
///
/// `base` is `None` for a two-way merge. `output` may be empty, new, or one of
/// the inputs ("merge into left"); when it already holds the chosen version of
/// a file nothing is written. [`MergeAction::MergeText`] merges the three
/// versions again and writes the result, failing the item if the merge now
/// has conflicts (the files changed since the comparison). Deleted
/// items go through `deleter`; a folder whose whole subtree is deleted is
/// removed in one step. Items marked [`MergeAction::Skip`] are left untouched
/// and reported. Per-item failures are reported without stopping the merge.
///
/// Files the output already holds are never simply overwritten: the old
/// version is moved aside and, once the new one is in place, handed to
/// `deleter` (Trash, a versioning folder under `output/`, or gone when the
/// user chose permanent deletion).
pub fn execute_merge(
    base: Option<&Path>,
    left: &Path,
    right: &Path,
    output: &Path,
    decisions: &[MergeDecision],
    deleter: &dyn Deleter,
) -> Result<MergeReport> {
    let cancel = AtomicBool::new(false);
    let run = MergeRun {
        cancel: &cancel,
        on_progress: &mut |_| {},
    };
    execute_merge_with(base, left, right, output, decisions, deleter, run)
}

/// [`execute_merge`] with progress reports and cancellation.
pub fn execute_merge_with(
    base: Option<&Path>,
    left: &Path,
    right: &Path,
    output: &Path,
    decisions: &[MergeDecision],
    deleter: &dyn Deleter,
    run: MergeRun,
) -> Result<MergeReport> {
    let started = Instant::now();
    let mut inputs = vec![("left", left), ("right", right)];
    if let Some(base) = base {
        inputs.insert(0, ("base", base));
    }
    let out_canon = validate_output(&inputs, output)?;
    fs::create_dir_all(output).map_err(|e| CoreError::io(output, e))?;
    // Inputs that *are* the output need no copying for "take that side".
    fn root<'p>(p: &'p Path, out: &Path) -> (&'p Path, bool) {
        (p, p.canonicalize().is_ok_and(|c| c == out))
    }
    let roots: Roots = [
        base.map(|b| root(b, &out_canon)),
        Some(root(left, &out_canon)),
        Some(root(right, &out_canon)),
    ];

    // Sorted view for "is every descendant deleted?" range queries: paths
    // sharing a prefix are contiguous in lexicographic order.
    let mut sorted: Vec<(&str, MergeAction)> = decisions
        .iter()
        .map(|d| (d.rel_path.as_str(), d.action))
        .collect();
    sorted.sort_unstable_by(|a, b| a.0.cmp(b.0));
    let all_deleted_below = |rel: &str| {
        let prefix = format!("{rel}/");
        let start = sorted.partition_point(|(p, _)| *p < prefix.as_str());
        sorted[start..]
            .iter()
            .take_while(|(p, _)| p.starts_with(&prefix))
            .all(|(_, a)| *a == MergeAction::Delete)
    };

    let mut report = MergeReport::default();
    let mut removed: Vec<&str> = Vec::new();
    let mut prune: Vec<&MergeDecision> = Vec::new();
    let MergeRun { cancel, on_progress } = run;
    let mut progress = MergeProgress {
        total: decisions.len(),
        ..Default::default()
    };
    let cancelled = || cancel.load(Ordering::Relaxed);
    let mut out = Output {
        deleter,
        cancel,
        backup: None,
        note: None,
    };

    for d in decisions {
        if cancelled() {
            report.cancelled = true;
            break;
        }
        progress.current = Some(d.rel_path.clone());
        progress.bytes_written = report.bytes_written;
        on_progress(&progress);
        progress.done += 1;
        if removed.iter().any(|p| {
            d.rel_path
                .strip_prefix(p)
                .is_some_and(|rest| rest.starts_with('/'))
        }) {
            continue; // its folder already went to the trash as a whole
        }
        let dst = match fsops::resolve(output, &d.rel_path) {
            Ok(p) => p,
            Err(e) => {
                report.record(
                    &d.rel_path,
                    d.is_dir,
                    MergeOutcome::Failed,
                    Some(e.to_string()),
                );
                continue;
            }
        };
        let step = match d.action {
            MergeAction::Skip => Ok(((!d.is_dir).then_some(MergeOutcome::Skipped), 0)),
            MergeAction::Delete if d.is_dir && !all_deleted_below(&d.rel_path) => {
                prune.push(d); // emptied by its children; removed afterwards
                Ok((None, 0))
            }
            MergeAction::Delete => {
                let r = delete_if_present(&dst, out.deleter);
                if d.is_dir && matches!(r, Ok((Some(_), _))) {
                    removed.push(&d.rel_path);
                }
                r
            }
            MergeAction::TakeLeft | MergeAction::TakeRight | MergeAction::TakeBase => {
                let (label, side) = match d.action {
                    MergeAction::TakeLeft => ("left", LEFT),
                    MergeAction::TakeRight => ("right", RIGHT),
                    _ => ("base", BASE),
                };
                match roots[side] {
                    Some((root, root_is_output)) => {
                        take(root, root_is_output, label, d, &dst, &mut out)
                    }
                    None => Err(no_base()),
                }
            }
            MergeAction::MergeText => write_text_merge(&roots, d, &dst, &mut out),
        };
        let (backup, note) = (out.backup.take(), out.note.take());
        match step {
            Err(e) if e.kind() == io::ErrorKind::Interrupted && cancelled() => {
                // Stopped inside a copy; the old version was put back.
                report.cancelled = true;
                break;
            }
            Ok((Some(outcome), bytes)) => {
                report.bytes_written += bytes;
                report.record_with_backup(&d.rel_path, d.is_dir, outcome, note, backup);
            }
            Ok((None, _)) => {
                if !d.is_dir && d.action != MergeAction::Skip {
                    report.unchanged += 1;
                }
            }
            Err(e) => report.record(
                &d.rel_path,
                d.is_dir,
                MergeOutcome::Failed,
                Some(e.to_string()),
            ),
        }
    }

    // Deepest first, so emptied parents can go too. Folders still holding
    // kept or skipped items stay. A cancelled run leaves them be.
    if report.cancelled {
        prune.clear();
    }
    for d in prune.iter().rev() {
        if let Ok(dst) = fsops::resolve(output, &d.rel_path) {
            if dst.is_dir() && fs::read_dir(&dst).is_ok_and(|mut it| it.next().is_none()) {
                match fs::remove_dir(&dst) {
                    Ok(()) => report.record(&d.rel_path, true, MergeOutcome::Deleted, None),
                    Err(e) => {
                        report.record(&d.rel_path, true, MergeOutcome::Failed, Some(e.to_string()))
                    }
                }
            }
        }
    }

    report.elapsed_ms = started.elapsed().as_millis();
    progress.current = None;
    progress.bytes_written = report.bytes_written;
    on_progress(&progress);
    Ok(report)
}

/// Where a merge writes: disposal of old versions, cancellation, and the
/// backup made by the current item.
struct Output<'a> {
    deleter: &'a dyn Deleter,
    cancel: &'a AtomicBool,
    backup: Option<String>,
    /// A problem that didn't stop the current item.
    note: Option<String>,
}

impl Output<'_> {
    /// Writes `dst` with `write`, first moving an existing file aside; once
    /// the new version is in place the old one goes to the deleter (Trash or
    /// the versioning folder). A failed write puts the old version back.
    fn replace(&mut self, dst: &Path, rel: &str, write: impl FnOnce(&Path) -> io::Result<u64>) -> io::Result<u64> {
        if !fs::symlink_metadata(dst).is_ok_and(|m| m.is_file()) {
            return write(dst);
        }
        let staged = fsops::stage_aside(dst)?;
        match write(dst) {
            Ok(bytes) => {
                match self.deleter.retire(&staged, "output", rel) {
                    Ok(kept) => self.backup = kept.map(|p| p.to_string_lossy().into_owned()),
                    Err(e) => {
                        self.note = Some(format!(
                            "written, but the old version couldn't be disposed of ({e}); it was kept as {}",
                            staged.display()
                        ))
                    }
                }
                Ok(bytes)
            }
            Err(e) => {
                let _ = fs::remove_file(dst);
                let _ = fs::rename(&staged, dst);
                Err(e)
            }
        }
    }
}

/// The input roots by side, each with whether it is the output folder.
type Roots<'a> = [Option<(&'a Path, bool)>; 3];

fn no_base() -> io::Error {
    io::Error::other("a two-way merge has no base folder")
}

/// Merges the three versions of a text file again and writes the result.
fn write_text_merge(
    roots: &Roots,
    d: &MergeDecision,
    dst: &Path,
    out: &mut Output,
) -> io::Result<Step> {
    let read = |side: usize| -> io::Result<Vec<u8>> {
        let (root, _) = roots[side].ok_or_else(no_base)?;
        fs::read(fsops::resolve(root, &d.rel_path)?)
    };
    let (base, left, right) = (read(BASE)?, read(LEFT)?, read(RIGHT)?);
    let merged = merge_text_bytes(&base, &left, &right)
        .ok_or_else(|| io::Error::other("one of the versions is no longer a text file"))?;
    let bytes = match merged.bytes {
        Some(bytes) => bytes,
        None if merged.summary.conflicts > 0 => {
            return Err(io::Error::other(format!(
                "the files changed since the comparison: merging them now leaves {} conflict(s)",
                merged.summary.conflicts
            )))
        }
        None => {
            return Err(io::Error::other(
                "the merged text cannot be saved in the left file's encoding",
            ))
        }
    };
    match fs::symlink_metadata(dst) {
        Ok(m) if m.is_dir() => out.deleter.delete(dst)?,
        Ok(_) if fs::read(dst)? == bytes => return Ok((None, 0)),
        _ => {}
    }
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)?;
    }
    // Write beside the target and rename it into place, so a failed write
    // never leaves a truncated file behind.
    let name = dst.file_name().unwrap_or_default().to_string_lossy();
    let tmp = dst.with_file_name(format!(".{name}.pcompare-merge"));
    let written = out.replace(dst, &d.rel_path, |dst| {
        fs::write(&tmp, &bytes).and_then(|()| fs::rename(&tmp, dst))?;
        Ok(bytes.len() as u64)
    });
    if written.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    Ok((Some(MergeOutcome::Written), written?))
}

fn delete_if_present(dst: &Path, deleter: &dyn Deleter) -> io::Result<Step> {
    match fs::symlink_metadata(dst) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok((None, 0)),
        Err(e) => Err(e),
        Ok(_) => deleter
            .delete(dst)
            .map(|_| (Some(MergeOutcome::Deleted), 0)),
    }
}

fn take(
    root: &Path,
    root_is_output: bool,
    label: &str,
    d: &MergeDecision,
    dst: &Path,
    out: &mut Output,
) -> io::Result<Step> {
    let src = fsops::resolve(root, &d.rel_path)?;
    let src_meta = fs::metadata(&src).map_err(|e| match e.kind() {
        io::ErrorKind::NotFound => {
            io::Error::new(e.kind(), format!("the {label} folder has no such item"))
        }
        _ => e,
    })?;
    if root_is_output {
        return Ok((None, 0)); // the output already is that side
    }
    let dst_meta = fs::symlink_metadata(dst).ok();
    let dst_is_dir = dst_meta.as_ref().is_some_and(|m| m.is_dir());

    if src_meta.is_dir() {
        if d.is_dir {
            // A regular folder row: make sure it exists; children follow.
            if dst_is_dir {
                return Ok((None, 0));
            }
            if dst_meta.is_some() {
                out.deleter.delete(dst)?;
            }
            fs::create_dir_all(dst)?;
            return Ok((Some(MergeOutcome::Created), 0));
        }
        // A type conflict resolved in favour of the folder: replace wholesale.
        if dst_meta.is_some() {
            out.deleter.delete(dst)?;
        }
        let bytes = fsops::copy_tree(&src, dst)?;
        return Ok((Some(MergeOutcome::Written), bytes));
    }

    if dst_is_dir {
        out.deleter.delete(dst)?; // type conflict resolved in favour of the file
    } else if fsops::same_contents(&src, dst)? {
        return Ok((None, 0));
    }
    let cancel = out.cancel;
    let bytes = out.replace(dst, &d.rel_path, |dst| {
        fsops::copy_file(&src, dst, &mut |_| {
            if cancel.load(Ordering::Relaxed) {
                Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"))
            } else {
                Ok(())
            }
        })
    })?;
    Ok((Some(MergeOutcome::Written), bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fsops::PermanentDeleter;
    use filetime::FileTime;
    use std::collections::HashMap;

    fn write(root: &Path, rel: &str, body: &str) {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, body).unwrap();
        filetime::set_file_mtime(&p, FileTime::from_unix_time(1_600_000_000, 0)).unwrap();
    }

    fn read(root: &Path, rel: &str) -> Option<String> {
        fs::read_to_string(root.join(rel)).ok()
    }

    fn opts() -> FolderCompareOptions {
        FolderCompareOptions {
            content: ContentMethod::Binary,
            ignore_name_case: false,
            ..Default::default()
        }
    }

    struct Trees {
        dir: tempfile::TempDir,
        base: PathBuf,
        left: PathBuf,
        right: PathBuf,
    }

    fn trees() -> Trees {
        let dir = tempfile::tempdir().unwrap();
        let [base, left, right] = ["base", "left", "right"].map(|n| dir.path().join(n));
        for (rel, b, l, r) in [
            ("unchanged.txt", Some("u"), Some("u"), Some("u")),
            ("lchg.txt", Some("b"), Some("L"), Some("b")),
            ("rchg.txt", Some("b"), Some("b"), Some("R")),
            ("bothsame.txt", Some("b"), Some("S"), Some("S")),
            ("conflict.txt", Some("b"), Some("L"), Some("R")),
            ("ladd.txt", None, Some("A"), None),
            ("radd.txt", None, None, Some("B")),
            ("addsame.txt", None, Some("same"), Some("same")),
            ("addconf.txt", None, Some("L"), Some("R")),
            ("ldel.txt", Some("x"), None, Some("x")),
            ("rdel.txt", Some("x"), Some("x"), None),
            ("bothdel.txt", Some("x"), None, None),
            ("delmod.txt", Some("b"), None, Some("M")),
            ("clash", None, Some("file"), None),
            ("clash/inner.txt", None, None, Some("dir")),
            ("sub/deep.txt", Some("b"), Some("L"), Some("b")),
            ("sub/other.txt", Some("b"), Some("b"), Some("R")),
            ("gone/a.txt", Some("a"), None, Some("a")),
            ("gone/nested/b.txt", Some("b"), None, Some("b")),
            ("newdir/n.txt", None, None, Some("n")),
        ] {
            for (root, body) in [(&base, b), (&left, l), (&right, r)] {
                if let Some(body) = body {
                    write(root, rel, body);
                }
            }
        }
        Trees {
            dir,
            base,
            left,
            right,
        }
    }

    fn flatten(entries: &[FolderMergeEntry], out: &mut HashMap<String, (MergeClass, MergeAction)>) {
        for e in entries {
            out.insert(e.rel_path.clone(), (e.class, e.action));
            flatten(&e.children, out);
        }
    }

    fn compare(
        t: &Trees,
    ) -> (
        FolderMergeResult,
        HashMap<String, (MergeClass, MergeAction)>,
    ) {
        let res = compare_merge(
            Some(t.base.as_path()),
            &t.left,
            &t.right,
            &opts(),
            &FolderMergeOptions::default(),
        )
        .unwrap();
        let mut map = HashMap::new();
        flatten(&res.entries, &mut map);
        (res, map)
    }

    #[test]
    fn classifies_every_case() {
        use MergeAction::*;
        use MergeClass::*;
        let t = trees();
        let (res, map) = compare(&t);
        let expect = [
            ("unchanged.txt", Unchanged, TakeLeft),
            ("lchg.txt", LeftChanged, TakeLeft),
            ("rchg.txt", RightChanged, TakeRight),
            ("bothsame.txt", BothChangedSame, TakeLeft),
            ("conflict.txt", Conflict, Skip),
            ("ladd.txt", LeftAdded, TakeLeft),
            ("radd.txt", RightAdded, TakeRight),
            ("addsame.txt", BothAddedSame, TakeLeft),
            ("addconf.txt", BothAddedConflict, Skip),
            ("ldel.txt", LeftDeleted, Delete),
            ("rdel.txt", RightDeleted, Delete),
            ("bothdel.txt", BothDeleted, Delete),
            ("delmod.txt", DeleteModifyConflict, Skip),
            ("clash", TypeConflict, Skip),
            ("sub", Merged, TakeLeft),
            ("gone", LeftDeleted, Delete),
            ("gone/nested", LeftDeleted, Delete),
            ("gone/a.txt", LeftDeleted, Delete),
            ("newdir", RightAdded, TakeRight),
            ("newdir/n.txt", RightAdded, TakeRight),
        ];
        for (rel, class, action) in expect {
            assert_eq!(map.get(rel), Some(&(class, action)), "{rel}");
        }
        assert!(
            !map.contains_key("clash/inner.txt"),
            "type conflicts are not descended"
        );
        assert_eq!(res.conflicts, 4);
        assert_eq!(res.counts[&Conflict], 1);
        assert_eq!(res.counts[&LeftDeleted], 3);
        assert_eq!(res.counts.get(&Merged), None, "folders are not counted");
        // Every file except unchanged.txt is a change.
        assert_eq!(res.changes, 18);
    }

    #[test]
    fn folder_with_modified_child_on_deleting_side_conflicts() {
        let t = trees();
        write(&t.right, "gone/a.txt", "edited");
        let (_, map) = compare(&t);
        assert_eq!(
            map["gone"],
            (MergeClass::DeleteModifyConflict, MergeAction::Skip)
        );
        assert_eq!(map["gone/a.txt"].0, MergeClass::DeleteModifyConflict);
        assert_eq!(map["gone/nested/b.txt"].0, MergeClass::LeftDeleted);
    }

    #[test]
    fn size_and_timestamp_criteria() {
        let t = trees();
        let o = FolderCompareOptions {
            content: ContentMethod::Off,
            ..opts()
        };
        // Same size and time but different content: invisible to this mode.
        let res = compare_merge(
            Some(t.base.as_path()),
            &t.left,
            &t.right,
            &o,
            &FolderMergeOptions::default(),
        )
        .unwrap();
        let mut map = HashMap::new();
        flatten(&res.entries, &mut map);
        assert_eq!(map["lchg.txt"].0, MergeClass::Unchanged);
        write(&t.left, "lchg.txt", "longer");
        let res = compare_merge(
            Some(t.base.as_path()),
            &t.left,
            &t.right,
            &o,
            &FolderMergeOptions::default(),
        )
        .unwrap();
        map.clear();
        flatten(&res.entries, &mut map);
        assert_eq!(map["lchg.txt"].0, MergeClass::LeftChanged);
    }

    #[test]
    fn honours_exclude_and_folder_filters() {
        let t = trees();
        let o = FolderCompareOptions {
            exclude: vec!["*.txt".into()],
            ..opts()
        };
        let res = compare_merge(
            Some(t.base.as_path()),
            &t.left,
            &t.right,
            &o,
            &FolderMergeOptions::default(),
        )
        .unwrap();
        let mut map = HashMap::new();
        flatten(&res.entries, &mut map);
        assert!(map.keys().all(|k| !k.ends_with(".txt")), "{:?}", map.keys());
        assert!(
            map.contains_key("clash"),
            "the type conflict has no extension"
        );

        let mut o = opts();
        o.filters.exclude_folders = vec!["gone".into()];
        let res = compare_merge(
            Some(t.base.as_path()),
            &t.left,
            &t.right,
            &o,
            &FolderMergeOptions::default(),
        )
        .unwrap();
        map.clear();
        flatten(&res.entries, &mut map);
        assert!(!map.contains_key("gone") && map.contains_key("sub"));
    }

    #[test]
    fn executes_into_empty_output() {
        let t = trees();
        let (res, _) = compare(&t);
        let out = t.dir.path().join("out");
        let report = execute_merge(
            Some(t.base.as_path()),
            &t.left,
            &t.right,
            &out,
            &default_decisions(&res),
            &PermanentDeleter,
        )
        .unwrap();
        assert_eq!(report.failed, 0, "{:?}", report.results);

        assert_eq!(read(&out, "unchanged.txt").as_deref(), Some("u"));
        assert_eq!(read(&out, "lchg.txt").as_deref(), Some("L"));
        assert_eq!(read(&out, "rchg.txt").as_deref(), Some("R"));
        assert_eq!(read(&out, "bothsame.txt").as_deref(), Some("S"));
        assert_eq!(read(&out, "ladd.txt").as_deref(), Some("A"));
        assert_eq!(read(&out, "radd.txt").as_deref(), Some("B"));
        assert_eq!(read(&out, "addsame.txt").as_deref(), Some("same"));
        assert_eq!(read(&out, "sub/deep.txt").as_deref(), Some("L"));
        assert_eq!(read(&out, "sub/other.txt").as_deref(), Some("R"));
        assert_eq!(read(&out, "newdir/n.txt").as_deref(), Some("n"));
        for absent in [
            "conflict.txt",
            "addconf.txt",
            "ldel.txt",
            "rdel.txt",
            "bothdel.txt",
            "delmod.txt",
            "clash",
            "gone",
        ] {
            assert!(!out.join(absent).exists(), "{absent}");
        }
        // Copies keep their timestamps.
        let mt = |p: &Path| fs::metadata(p).unwrap().modified().unwrap();
        assert_eq!(mt(&out.join("rchg.txt")), mt(&t.right.join("rchg.txt")));

        let mut skipped: Vec<_> = report
            .results
            .iter()
            .filter(|r| r.outcome == MergeOutcome::Skipped)
            .map(|r| r.rel_path.as_str())
            .collect();
        skipped.sort();
        assert_eq!(
            skipped,
            ["addconf.txt", "clash", "conflict.txt", "delmod.txt"]
        );
        assert_eq!(report.deleted, 0, "nothing to delete in a fresh output");
    }

    #[test]
    fn executes_into_left_in_place() {
        let t = trees();
        let (res, _) = compare(&t);
        let mut decisions = default_decisions(&res);
        // Resolve one conflict explicitly.
        decisions
            .iter_mut()
            .find(|d| d.rel_path == "conflict.txt")
            .unwrap()
            .action = MergeAction::TakeRight;
        let report = execute_merge(
            Some(t.base.as_path()),
            &t.left,
            &t.right,
            &t.left,
            &decisions,
            &PermanentDeleter,
        )
        .unwrap();
        assert_eq!(report.failed, 0, "{:?}", report.results);

        let l = &t.left;
        assert_eq!(read(l, "rchg.txt").as_deref(), Some("R"));
        assert_eq!(read(l, "conflict.txt").as_deref(), Some("R"));
        assert_eq!(read(l, "radd.txt").as_deref(), Some("B"));
        assert_eq!(read(l, "sub/other.txt").as_deref(), Some("R"));
        assert_eq!(read(l, "newdir/n.txt").as_deref(), Some("n"));
        assert!(!l.join("rdel.txt").exists());
        // Unresolved conflicts keep the left version untouched.
        assert_eq!(read(l, "addconf.txt").as_deref(), Some("L"));
        assert_eq!(read(l, "clash").as_deref(), Some("file"));

        // Only changed files were written: lchg, unchanged etc. were not.
        let written: Vec<_> = report
            .results
            .iter()
            .filter(|r| r.outcome == MergeOutcome::Written)
            .map(|r| r.rel_path.as_str())
            .collect();
        assert!(!written.contains(&"lchg.txt") && !written.contains(&"unchanged.txt"));
        assert!(written.contains(&"rchg.txt"));
        assert!(report.unchanged > 0);

        // A second compare sees left matching the merge result.
        let (_, map) = compare(&t);
        assert_eq!(map["rchg.txt"].0, MergeClass::BothChangedSame);
        assert_eq!(map["conflict.txt"].0, MergeClass::BothChangedSame);
    }

    #[test]
    fn executes_into_right_deletes_whole_folder_at_once() {
        let t = trees();
        let (res, _) = compare(&t);
        let decisions = default_decisions(&res);
        let report = execute_merge(
            Some(t.base.as_path()),
            &t.left,
            &t.right,
            &t.right,
            &decisions,
            &PermanentDeleter,
        )
        .unwrap();
        assert_eq!(report.failed, 0, "{:?}", report.results);
        assert!(!t.right.join("gone").exists());
        assert!(!t.right.join("ldel.txt").exists());
        let gone: Vec<_> = report
            .results
            .iter()
            .filter(|r| r.rel_path.starts_with("gone"))
            .collect();
        assert_eq!(gone.len(), 1, "one delete for the folder: {gone:?}");
        assert_eq!(gone[0].outcome, MergeOutcome::Deleted);
        assert_eq!(read(&t.right, "lchg.txt").as_deref(), Some("L"));
    }

    #[test]
    fn partial_folder_delete_keeps_non_empty_folder() {
        let t = trees();
        let (res, _) = compare(&t);
        let mut decisions = default_decisions(&res);
        decisions
            .iter_mut()
            .find(|d| d.rel_path == "gone/a.txt")
            .unwrap()
            .action = MergeAction::TakeBase;
        let report = execute_merge(
            Some(t.base.as_path()),
            &t.left,
            &t.right,
            &t.right,
            &decisions,
            &PermanentDeleter,
        )
        .unwrap();
        assert_eq!(report.failed, 0, "{:?}", report.results);
        assert_eq!(read(&t.right, "gone/a.txt").as_deref(), Some("a"));
        assert!(
            !t.right.join("gone/nested").exists(),
            "emptied subfolder removed"
        );
    }

    #[test]
    fn type_conflict_resolved_to_folder() {
        let t = trees();
        let (res, _) = compare(&t);
        let mut decisions = default_decisions(&res);
        decisions
            .iter_mut()
            .find(|d| d.rel_path == "clash")
            .unwrap()
            .action = MergeAction::TakeRight;
        let out = t.dir.path().join("out");
        execute_merge(
            Some(t.base.as_path()),
            &t.left,
            &t.right,
            &out,
            &decisions,
            &PermanentDeleter,
        )
        .unwrap();
        assert_eq!(read(&out, "clash/inner.txt").as_deref(), Some("dir"));
        // And into left, where a file stands in the way.
        execute_merge(
            Some(t.base.as_path()),
            &t.left,
            &t.right,
            &t.left,
            &decisions,
            &PermanentDeleter,
        )
        .unwrap();
        assert_eq!(read(&t.left, "clash/inner.txt").as_deref(), Some("dir"));
    }

    #[test]
    fn missing_side_fails_item_only() {
        let t = trees();
        let decisions = [
            MergeDecision {
                rel_path: "ladd.txt".into(),
                is_dir: false,
                action: MergeAction::TakeRight,
            },
            MergeDecision {
                rel_path: "radd.txt".into(),
                is_dir: false,
                action: MergeAction::TakeRight,
            },
        ];
        let out = t.dir.path().join("out");
        let report = execute_merge(
            Some(t.base.as_path()),
            &t.left,
            &t.right,
            &out,
            &decisions,
            &PermanentDeleter,
        )
        .unwrap();
        assert_eq!((report.failed, report.written), (1, 1));
        assert!(report.results[0]
            .error
            .as_deref()
            .unwrap()
            .contains("right folder has no"));
    }

    fn take_right(rel: &str) -> MergeDecision {
        MergeDecision {
            rel_path: rel.into(),
            is_dir: false,
            action: MergeAction::TakeRight,
        }
    }

    #[test]
    fn overwritten_output_files_are_backed_up() {
        let dir = tempfile::tempdir().unwrap();
        let [left, right, versions] = ["left", "right", "versions"].map(|n| dir.path().join(n));
        write(&left, "a.txt", "left a");
        write(&left, "sub/b.txt", "left b");
        write(&right, "a.txt", "right a");
        write(&right, "sub/b.txt", "right b");
        write(&right, "new.txt", "new");
        let decisions = [take_right("a.txt"), take_right("sub/b.txt"), take_right("new.txt")];
        // Merging in place into the left folder replaces two of its files.
        let deleter = fsops::VersioningDeleter::new(&versions, "run1");
        let report = execute_merge(None, &left, &right, &left, &decisions, &deleter).unwrap();
        assert_eq!((report.written, report.failed), (3, 0), "{:?}", report.results);
        assert_eq!(read(&left, "a.txt").as_deref(), Some("right a"));
        assert_eq!(read(&left, "sub/b.txt").as_deref(), Some("right b"));
        let kept = versions.join("run1").join("output");
        assert_eq!(read(&kept, "a.txt").as_deref(), Some("left a"));
        assert_eq!(read(&kept, "sub/b.txt").as_deref(), Some("left b"));
        let backup = |rel: &str| report.results.iter().find(|r| r.rel_path == rel).unwrap().backup.clone();
        assert_eq!(backup("a.txt").map(PathBuf::from), Some(kept.join("a.txt")));
        // Nothing was there to back up.
        assert_eq!(backup("new.txt"), None);
        // No staged leftovers next to the replaced files.
        let names: Vec<_> = fs::read_dir(&left).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert!(names.iter().all(|n| !n.to_string_lossy().contains(".pcompare-")), "{names:?}");
    }

    #[test]
    fn text_merge_output_backs_up_the_old_file() {
        let t = text_trees();
        let res = compare_merge(Some(t.base.as_path()), &t.left, &t.right, &opts(), &FolderMergeOptions::default()).unwrap();
        let decisions: Vec<_> = default_decisions(&res).into_iter().filter(|d| d.rel_path == "clean.txt").collect();
        assert_eq!(decisions[0].action, MergeAction::MergeText);
        let versions = t.dir.path().join("versions");
        let deleter = fsops::VersioningDeleter::new(&versions, "r");
        let report =
            execute_merge(Some(t.base.as_path()), &t.left, &t.right, &t.left, &decisions, &deleter).unwrap();
        assert_eq!(report.written, 1, "{:?}", report.results);
        assert_eq!(read(&t.left, "clean.txt").as_deref(), Some("A\nb\nc\nD\n"));
        assert_eq!(read(&versions.join("r/output"), "clean.txt").as_deref(), Some("A\nb\nc\nd\n"));
    }

    #[test]
    fn merge_reports_progress_and_stops_when_cancelled() {
        let dir = tempfile::tempdir().unwrap();
        let [left, right, out] = ["left", "right", "out"].map(|n| dir.path().join(n));
        let names = ["1.txt", "2.txt", "3.txt", "4.txt"];
        for n in names {
            write(&right, n, n);
        }
        write(&left, "keep.txt", "k");
        let decisions: Vec<_> = names.iter().map(|n| take_right(n)).collect();
        let cancel = AtomicBool::new(false);
        let mut seen = Vec::new();
        let run = MergeRun {
            cancel: &cancel,
            on_progress: &mut |p: &MergeProgress| {
                seen.push((p.done, p.total, p.current.clone()));
                // Stop once the second item has been written.
                if p.done == 2 {
                    cancel.store(true, Ordering::Relaxed);
                }
            },
        };
        let report = execute_merge_with(None, &left, &right, &out, &decisions, &PermanentDeleter, run).unwrap();
        assert!(report.cancelled);
        assert_eq!(report.written, 2);
        assert!(out.join("2.txt").exists() && !out.join("3.txt").exists());
        assert_eq!(seen[0], (0, 4, Some("1.txt".into())));
        assert_eq!(seen[1], (1, 4, Some("2.txt".into())));
        // The final report has no current item.
        assert_eq!(seen.last().unwrap().2, None);

        // Uncancelled: every item, and progress ends at total.
        let cancel = AtomicBool::new(false);
        let mut last = MergeProgress::default();
        let run = MergeRun {
            cancel: &cancel,
            on_progress: &mut |p: &MergeProgress| last = p.clone(),
        };
        let out2 = dir.path().join("out2");
        let report = execute_merge_with(None, &left, &right, &out2, &decisions, &PermanentDeleter, run).unwrap();
        assert!(!report.cancelled);
        assert_eq!((last.done, last.total, report.written), (4, 4, 4));
        assert_eq!(last.bytes_written, report.bytes_written);
    }

    #[test]
    fn cancelling_inside_a_copy_keeps_the_old_file() {
        let dir = tempfile::tempdir().unwrap();
        let [left, right] = ["left", "right"].map(|n| dir.path().join(n));
        write(&left, "big.bin", "old");
        fs::create_dir_all(&right).unwrap();
        fs::write(right.join("big.bin"), vec![7u8; 3 << 20]).unwrap();
        // Cancelled while the copy runs: its first chunk sees the flag.
        let cancel = AtomicBool::new(true);
        let mut out = Output {
            deleter: &PermanentDeleter,
            cancel: &cancel,
            backup: None,
            note: None,
        };
        let dst = left.join("big.bin");
        let err = take(&right, false, "right", &take_right("big.bin"), &dst, &mut out).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Interrupted);
        assert_eq!(read(&left, "big.bin").as_deref(), Some("old"));
        assert!(out.backup.is_none());
        let names: Vec<_> = fs::read_dir(&left).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(names.len(), 1, "{names:?}");
    }

    fn flat_map(res: &FolderMergeResult) -> HashMap<String, (MergeClass, MergeAction)> {
        let mut map = HashMap::new();
        flatten(&res.entries, &mut map);
        map
    }

    fn touch(root: &Path, rel: &str, unix: i64) {
        filetime::set_file_mtime(root.join(rel), FileTime::from_unix_time(unix, 0)).unwrap();
    }

    /// Left and right folders without a base.
    fn two_way_trees() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let [left, right] = ["left", "right"].map(|n| dir.path().join(n));
        for (rel, l, r) in [
            ("same.txt", Some("s"), Some("s")),
            ("lonly.txt", Some("L"), None),
            ("ronly.txt", None, Some("R")),
            ("lnewer.txt", Some("new"), Some("old")),
            ("rnewer.txt", Some("old"), Some("new")),
            ("differ.txt", Some("one"), Some("two")),
            ("ldir/x.txt", Some("x"), None),
            ("sub/a.txt", Some("a"), Some("a")),
            ("sub/b.txt", None, Some("b")),
            ("cdir/c.txt", Some("c1"), Some("c2")),
        ] {
            for (root, body) in [(&left, l), (&right, r)] {
                if let Some(body) = body {
                    write(root, rel, body);
                }
            }
        }
        touch(&left, "lnewer.txt", 1_700_000_000);
        touch(&right, "rnewer.txt", 1_700_000_000);
        (dir, left, right)
    }

    #[test]
    fn two_way_classification() {
        use MergeAction::*;
        use MergeClass::*;
        let (_dir, left, right) = two_way_trees();
        let res =
            compare_merge(None, &left, &right, &opts(), &FolderMergeOptions::default()).unwrap();
        assert!(res.two_way);
        assert_eq!(res.base_root, "");
        let map = flat_map(&res);
        for (rel, class, action) in [
            ("same.txt", Unchanged, TakeLeft),
            ("lonly.txt", LeftOnly, TakeLeft),
            ("ronly.txt", RightOnly, TakeRight),
            ("lnewer.txt", LeftNewer, TakeLeft),
            ("rnewer.txt", RightNewer, TakeRight),
            ("differ.txt", Different, Skip),
            ("ldir", LeftOnly, TakeLeft),
            ("ldir/x.txt", LeftOnly, TakeLeft),
            ("sub", Merged, TakeLeft),
            ("sub/a.txt", Unchanged, TakeLeft),
            ("sub/b.txt", RightOnly, TakeRight),
            ("cdir", Different, TakeLeft),
            ("cdir/c.txt", Different, Skip),
        ] {
            assert_eq!(map.get(rel), Some(&(class, action)), "{rel}");
        }
        assert_eq!(res.conflicts, 2);
        assert_eq!(res.counts[&LeftOnly], 2);

        // Without "take newer" every difference needs a decision.
        let strict = FolderMergeOptions {
            take_newer: false,
            ..Default::default()
        };
        let res = compare_merge(None, &left, &right, &opts(), &strict).unwrap();
        let map = flat_map(&res);
        assert_eq!(map["lnewer.txt"], (Different, Skip));
        assert_eq!(map["rnewer.txt"], (Different, Skip));
        assert_eq!(res.conflicts, 4);
    }

    #[test]
    fn two_way_executes_without_base() {
        let (dir, left, right) = two_way_trees();
        let res =
            compare_merge(None, &left, &right, &opts(), &FolderMergeOptions::default()).unwrap();
        let mut decisions = default_decisions(&res);
        decisions
            .iter_mut()
            .find(|d| d.rel_path == "differ.txt")
            .unwrap()
            .action = MergeAction::TakeRight;
        let out = dir.path().join("out");
        let report =
            execute_merge(None, &left, &right, &out, &decisions, &PermanentDeleter).unwrap();
        assert_eq!(report.failed, 0, "{:?}", report.results);
        for (rel, body) in [
            ("same.txt", "s"),
            ("lonly.txt", "L"),
            ("ronly.txt", "R"),
            ("lnewer.txt", "new"),
            ("rnewer.txt", "new"),
            ("differ.txt", "two"),
            ("ldir/x.txt", "x"),
            ("sub/b.txt", "b"),
        ] {
            assert_eq!(read(&out, rel).as_deref(), Some(body), "{rel}");
        }
        assert!(
            !out.join("cdir/c.txt").exists(),
            "unresolved conflicts are skipped"
        );

        // Merging into left copies the right side's contributions only.
        let report =
            execute_merge(None, &left, &right, &left, &decisions, &PermanentDeleter).unwrap();
        assert_eq!(report.failed, 0, "{:?}", report.results);
        assert_eq!(read(&left, "ronly.txt").as_deref(), Some("R"));
        assert_eq!(read(&left, "rnewer.txt").as_deref(), Some("new"));
        assert_eq!(read(&left, "cdir/c.txt").as_deref(), Some("c1"));

        // There is no base to take from.
        let base_take = [MergeDecision {
            rel_path: "same.txt".into(),
            is_dir: false,
            action: MergeAction::TakeBase,
        }];
        let report =
            execute_merge(None, &left, &right, &out, &base_take, &PermanentDeleter).unwrap();
        assert_eq!(report.failed, 1);
        assert!(report.results[0]
            .error
            .as_deref()
            .unwrap()
            .contains("no base"));
    }

    /// Base, left and right trees holding text files changed on both sides.
    fn text_trees() -> Trees {
        let dir = tempfile::tempdir().unwrap();
        let [base, left, right] = ["base", "left", "right"].map(|n| dir.path().join(n));
        for (rel, b, l, r) in [
            ("clean.txt", "a\nb\nc\nd\n", "A\nb\nc\nd\n", "a\nb\nc\nD\n"),
            ("clash.txt", "a\nb\n", "left\nb\n", "right\nb\n"),
            // Changes two lines apart: past the default conflict scan lines.
            ("crlf.txt", "a\nb\nc\nd\n", "A\r\nb\r\nc\r\nd\r\n", "a\nb\nc\nD\n"),
            ("bin.dat", "a\0b\nc\n", "A\0b\nc\n", "a\0b\nC\n"),
            ("deep/t.txt", "1\n2\n3\n4\n", "one\n2\n3\n4\n", "1\n2\n3\nfour\n"),
        ] {
            for (root, body) in [(&base, b), (&left, l), (&right, r)] {
                write(root, rel, body);
            }
        }
        Trees {
            dir,
            base,
            left,
            right,
        }
    }

    fn find<'a>(entries: &'a [FolderMergeEntry], rel: &str) -> Option<&'a FolderMergeEntry> {
        entries.iter().find_map(|e| {
            (e.rel_path == rel)
                .then_some(e)
                .or_else(|| find(&e.children, rel))
        })
    }

    fn text_entry<'a>(entries: &'a [FolderMergeEntry], rel: &str) -> &'a FolderMergeEntry {
        find(entries, rel).unwrap_or_else(|| panic!("no entry {rel}"))
    }

    #[test]
    fn auto_merges_text_changed_on_both_sides() {
        use MergeAction::*;
        use MergeClass::*;
        let t = text_trees();
        let res = compare_merge(
            Some(t.base.as_path()),
            &t.left,
            &t.right,
            &opts(),
            &FolderMergeOptions::default(),
        )
        .unwrap();
        let map = flat_map(&res);
        assert_eq!(map["clean.txt"], (AutoMerged, MergeText));
        assert_eq!(map["crlf.txt"], (AutoMerged, MergeText));
        assert_eq!(map["deep/t.txt"], (AutoMerged, MergeText));
        assert_eq!(map["deep"], (Merged, TakeLeft));
        assert_eq!(map["clash.txt"], (Conflict, Skip));
        assert_eq!(
            map["bin.dat"],
            (Conflict, Skip),
            "binary files are not merged"
        );
        assert_eq!(res.conflicts, 2);
        assert_eq!(res.counts[&AutoMerged], 3);
        let summary = |rel| text_entry(&res.entries, rel).text_merge;
        assert_eq!(
            summary("clean.txt"),
            Some(TextMergeSummary {
                changes: 2,
                conflicts: 0
            })
        );
        assert_eq!(summary("clash.txt").map(|s| s.conflicts), Some(1));
        assert_eq!(summary("bin.dat"), None);

        let out = t.dir.path().join("out");
        let report = execute_merge(
            Some(t.base.as_path()),
            &t.left,
            &t.right,
            &out,
            &default_decisions(&res),
            &PermanentDeleter,
        )
        .unwrap();
        assert_eq!(report.failed, 0, "{:?}", report.results);
        assert_eq!(read(&out, "clean.txt").as_deref(), Some("A\nb\nc\nD\n"));
        assert_eq!(read(&out, "deep/t.txt").as_deref(), Some("one\n2\n3\nfour\n"));
        // The left file's line endings are kept.
        assert_eq!(read(&out, "crlf.txt").as_deref(), Some("A\r\nb\r\nc\r\nD\r\n"));
        assert!(!out.join("clash.txt").exists());

        // In place into left: written once, then already up to date.
        let decisions = default_decisions(&res);
        let base = Some(t.base.as_path());
        let first = execute_merge(
            base,
            &t.left,
            &t.right,
            &t.left,
            &decisions,
            &PermanentDeleter,
        )
        .unwrap();
        assert_eq!(read(&t.left, "clean.txt").as_deref(), Some("A\nb\nc\nD\n"));
        assert!(first.written >= 3, "{:?}", first.results);
    }

    #[test]
    fn auto_merge_can_be_turned_off_and_rechecks_on_execute() {
        let t = text_trees();
        let base = Some(t.base.as_path());
        let off = FolderMergeOptions {
            auto_merge_text: false,
            ..Default::default()
        };
        let res = compare_merge(base, &t.left, &t.right, &opts(), &off).unwrap();
        assert_eq!(
            flat_map(&res)["clean.txt"],
            (MergeClass::Conflict, MergeAction::Skip)
        );
        assert_eq!(text_entry(&res.entries, "clean.txt").text_merge, None);

        // The right file changes after the comparison so that it now conflicts.
        let res = compare_merge(
            base,
            &t.left,
            &t.right,
            &opts(),
            &FolderMergeOptions::default(),
        )
        .unwrap();
        write(&t.right, "clean.txt", "X\nb\nc\nd\n");
        let out = t.dir.path().join("out");
        let report = execute_merge(
            base,
            &t.left,
            &t.right,
            &out,
            &default_decisions(&res),
            &PermanentDeleter,
        )
        .unwrap();
        let failed: Vec<_> = report
            .results
            .iter()
            .filter(|r| r.outcome == MergeOutcome::Failed)
            .collect();
        assert_eq!(failed.len(), 1, "{:?}", report.results);
        assert_eq!(failed[0].rel_path, "clean.txt");
        assert!(failed[0].error.as_deref().unwrap().contains("1 conflict"));
        assert!(!out.join("clean.txt").exists());
    }

    #[test]
    fn rejects_overlapping_output() {
        let t = trees();
        let inside = t.left.join("merged");
        let err = execute_merge(
            Some(t.base.as_path()),
            &t.left,
            &t.right,
            &inside,
            &[],
            &PermanentDeleter,
        )
        .unwrap_err();
        assert!(err.to_string().contains("inside the left folder"), "{err}");
        assert!(!inside.exists());
        let err = execute_merge(
            Some(t.base.as_path()),
            &t.left,
            &t.right,
            t.dir.path(),
            &[],
            &PermanentDeleter,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("inside the output folder"),
            "{err}"
        );
        let err = execute_merge(
            Some(t.base.as_path()),
            &t.left,
            &t.right,
            Path::new(""),
            &[],
            &PermanentDeleter,
        )
        .unwrap_err();
        assert!(err.to_string().contains("Choose an output"));
    }
}
