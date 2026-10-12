//! Three-way folder merge: classify every path in `left` and `right` against
//! their common ancestor `base`, propose an action per item, then build the
//! merged tree in an output folder from the (user-adjusted) decisions.
//!
//! The walk goes through [`Vfs`], so inputs may be remote for comparison;
//! execution needs local folders.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Instant;

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use pcompare_core::folder::{self, ContentMethod, EntryMeta, EntryStatus, FolderCompareOptions};
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
    /// Conflicting items at or below this entry.
    pub conflicts: usize,
    /// Changed (non-unchanged) items at or below this entry.
    pub changes: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FolderMergeResult {
    pub base_root: String,
    pub left_root: String,
    pub right_root: String,
    pub entries: Vec<FolderMergeEntry>,
    /// File-level counts per classification.
    pub counts: BTreeMap<MergeClass, usize>,
    pub conflicts: usize,
    pub changes: usize,
    pub elapsed_ms: u128,
}

const BASE: usize = 0;
const LEFT: usize = 1;
const RIGHT: usize = 2;

struct Ctx<'a> {
    opts: &'a FolderCompareOptions,
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

fn classify_file(
    ctx: &Ctx,
    paths: &Sides<String>,
    items: &Sides<Listed>,
) -> Result<(MergeClass, MergeAction)> {
    use MergeAction::*;
    use MergeClass::*;
    let eq = |a, b| same_file(ctx, paths, items, a, b);
    let has = |s: usize| items[s].is_some();
    Ok(match (has(BASE), has(LEFT), has(RIGHT)) {
        (true, true, true) => match (eq(LEFT, BASE)?, eq(RIGHT, BASE)?) {
            (true, true) => (Unchanged, TakeLeft),
            (false, true) => (LeftChanged, TakeLeft),
            (true, false) => (RightChanged, TakeRight),
            (false, false) if eq(LEFT, RIGHT)? => (BothChangedSame, TakeLeft),
            (false, false) => (Conflict, Skip),
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
    })
}

/// Which sides a (non-conflicting) change came from.
fn change_sides(class: MergeClass) -> (bool, bool, bool) {
    use MergeClass::*;
    match class {
        LeftChanged | LeftAdded | LeftDeleted => (true, false, false),
        RightChanged | RightAdded | RightDeleted => (false, true, false),
        BothChangedSame | BothAddedSame | BothDeleted => (false, false, true),
        Merged => (true, true, false),
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
                (entry.class, entry.action) = classify_folder(present, &children);
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
            Ok((class, action)) => {
                entry.class = class;
                entry.action = action;
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

/// Three-way comparison of folders that may live on different file systems.
pub fn compare_merge_with(
    base: (&dyn Vfs, &str),
    left: (&dyn Vfs, &str),
    right: (&dyn Vfs, &str),
    opts: &FolderCompareOptions,
) -> Result<FolderMergeResult> {
    let started = Instant::now();
    // Fail fast on bad ignore-pattern regexes rather than once per file.
    if opts.content == ContentMethod::Rules {
        pcompare_core::text::LineNormalizer::new(&opts.rules)?;
    }
    let ci = opts.ignore_name_case;
    let ctx = Ctx {
        opts,
        exclude: build_globset(&opts.exclude, ci)?,
        include_files: build_globset(&opts.filters.include_files, ci)?,
        exclude_files: build_globset(&opts.filters.exclude_files, ci)?,
        exclude_folders: build_globset(&opts.filters.exclude_folders, ci)?,
        fs: [base.0, left.0, right.0],
    };
    let roots = [
        Some(base.1.to_owned()),
        Some(left.1.to_owned()),
        Some(right.1.to_owned()),
    ];
    let entries = walk_dir(&roots, "", &ctx)?;
    let mut counts = BTreeMap::new();
    tally(&entries, &mut counts);
    Ok(FolderMergeResult {
        base_root: base.1.to_owned(),
        left_root: left.1.to_owned(),
        right_root: right.1.to_owned(),
        conflicts: entries.iter().map(|e| e.conflicts).sum(),
        changes: entries.iter().map(|e| e.changes).sum(),
        entries,
        counts,
        elapsed_ms: started.elapsed().as_millis(),
    })
}

/// Local-disk convenience wrapper around [`compare_merge_with`].
pub fn compare_merge(
    base: impl AsRef<Path>,
    left: impl AsRef<Path>,
    right: impl AsRef<Path>,
    opts: &FolderCompareOptions,
) -> Result<FolderMergeResult> {
    let [b, l, r] =
        [base.as_ref(), left.as_ref(), right.as_ref()].map(|p| p.to_string_lossy().into_owned());
    compare_merge_with((&LocalFs, &b), (&LocalFs, &l), (&LocalFs, &r), opts)
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
}

impl MergeReport {
    fn record(
        &mut self,
        rel_path: &str,
        is_dir: bool,
        outcome: MergeOutcome,
        error: Option<String>,
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
/// `output` may be empty, new, or one of the inputs ("merge into left"); when
/// it already holds the chosen version of a file nothing is written. Deleted
/// items go through `deleter`; a folder whose whole subtree is deleted is
/// removed in one step. Items marked [`MergeAction::Skip`] are left untouched
/// and reported. Per-item failures are reported without stopping the merge.
pub fn execute_merge(
    base: &Path,
    left: &Path,
    right: &Path,
    output: &Path,
    decisions: &[MergeDecision],
    deleter: &dyn Deleter,
) -> Result<MergeReport> {
    let started = Instant::now();
    let out_canon = validate_output(&[("base", base), ("left", left), ("right", right)], output)?;
    fs::create_dir_all(output).map_err(|e| CoreError::io(output, e))?;
    // Inputs that *are* the output need no copying for "take that side".
    let is_output = |p: &Path| p.canonicalize().is_ok_and(|c| c == out_canon);
    let roots = [
        (base, is_output(base)),
        (left, is_output(left)),
        (right, is_output(right)),
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

    for d in decisions {
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
                let r = delete_if_present(&dst, deleter);
                if d.is_dir && matches!(r, Ok((Some(_), _))) {
                    removed.push(&d.rel_path);
                }
                r
            }
            MergeAction::TakeLeft | MergeAction::TakeRight | MergeAction::TakeBase => {
                let (label, (root, root_is_output)) = match d.action {
                    MergeAction::TakeLeft => ("left", roots[LEFT]),
                    MergeAction::TakeRight => ("right", roots[RIGHT]),
                    _ => ("base", roots[BASE]),
                };
                take(root, root_is_output, label, d, &dst, deleter)
            }
        };
        match step {
            Ok((Some(outcome), bytes)) => {
                report.bytes_written += bytes;
                report.record(&d.rel_path, d.is_dir, outcome, None);
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
    // kept or skipped items stay.
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
    Ok(report)
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
    deleter: &dyn Deleter,
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
                deleter.delete(dst)?;
            }
            fs::create_dir_all(dst)?;
            return Ok((Some(MergeOutcome::Created), 0));
        }
        // A type conflict resolved in favour of the folder: replace wholesale.
        if dst_meta.is_some() {
            deleter.delete(dst)?;
        }
        let bytes = fsops::copy_tree(&src, dst)?;
        return Ok((Some(MergeOutcome::Written), bytes));
    }

    if dst_is_dir {
        deleter.delete(dst)?; // type conflict resolved in favour of the file
    } else if fsops::same_contents(&src, dst)? {
        return Ok((None, 0));
    }
    let bytes = fsops::copy_file(&src, dst, &mut |_| Ok(()))?;
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
        let res = compare_merge(&t.base, &t.left, &t.right, &opts()).unwrap();
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
        let res = compare_merge(&t.base, &t.left, &t.right, &o).unwrap();
        let mut map = HashMap::new();
        flatten(&res.entries, &mut map);
        assert_eq!(map["lchg.txt"].0, MergeClass::Unchanged);
        write(&t.left, "lchg.txt", "longer");
        let res = compare_merge(&t.base, &t.left, &t.right, &o).unwrap();
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
        let res = compare_merge(&t.base, &t.left, &t.right, &o).unwrap();
        let mut map = HashMap::new();
        flatten(&res.entries, &mut map);
        assert!(map.keys().all(|k| !k.ends_with(".txt")), "{:?}", map.keys());
        assert!(
            map.contains_key("clash"),
            "the type conflict has no extension"
        );

        let mut o = opts();
        o.filters.exclude_folders = vec!["gone".into()];
        let res = compare_merge(&t.base, &t.left, &t.right, &o).unwrap();
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
            &t.base,
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
            &t.base,
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
            &t.base,
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
            &t.base,
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
            &t.base,
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
            &t.base,
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
            &t.base,
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

    #[test]
    fn rejects_overlapping_output() {
        let t = trees();
        let inside = t.left.join("merged");
        let err =
            execute_merge(&t.base, &t.left, &t.right, &inside, &[], &PermanentDeleter).unwrap_err();
        assert!(err.to_string().contains("inside the left folder"), "{err}");
        assert!(!inside.exists());
        let err = execute_merge(
            &t.base,
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
            &t.base,
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
