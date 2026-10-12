//! Folder synchronization: turn a two-way folder comparison into an ordered
//! list of copy / delete / mkdir operations for a chosen mode, then perform a
//! (possibly user-filtered) subset of them.
//!
//! Planning reuses [`pcompare_core::folder`] so the preview agrees exactly
//! with what a Folder Compare session shows for the same options.

use std::path::Path;
use std::time::Instant;

use pcompare_core::folder::{
    self, ChecksumCache, EntryMeta, EntryStatus, FolderCompareOptions, FolderCompareResult,
    FolderEntry, Side,
};
use pcompare_core::vfs::Vfs;
use serde::{Deserialize, Serialize};

use crate::baseline::{self, Baseline, Snap};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SyncMode {
    /// Copy newer and left-only items to the right; never delete.
    #[default]
    UpdateLeftToRight,
    /// Copy newer and right-only items to the left; never delete.
    UpdateRightToLeft,
    /// Newer wins in each direction; orphans are copied both ways.
    UpdateBoth,
    /// Make the right side identical to the left, deleting right-only items.
    MirrorLeftToRight,
    /// Make the left side identical to the right, deleting left-only items.
    MirrorRightToLeft,
    /// True two-way sync against the baseline from the last run: changes
    /// and deletions on either side are carried over, and paths changed on
    /// both sides are conflicts. Without a baseline it behaves like
    /// [`SyncMode::UpdateBoth`].
    TwoWay,
}

/// How [`SyncMode::TwoWay`] settles paths that changed on both sides.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ConflictPolicy {
    /// Leave them alone and list them as conflicts.
    #[default]
    Skip,
    /// The newer file wins; an edit always beats a deletion.
    NewerWins,
    LeftWins,
    RightWins,
}

/// Inputs to planning besides the comparison and its options.
#[derive(Debug, Clone, Copy, Default)]
pub struct PlanOptions<'a> {
    /// State after the last successful sync (for [`SyncMode::TwoWay`]).
    pub baseline: Option<&'a Baseline>,
    pub conflicts: ConflictPolicy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum WarningKind {
    /// One side is empty while the other (or the baseline) is not — often an
    /// unmounted drive or a disconnected share rather than a real deletion.
    EmptySide,
    /// The plan deletes a large share of the files on one side.
    MassDeletion,
}

/// Something about a plan the user must acknowledge before running it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanWarning {
    pub kind: WarningKind,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum OpKind {
    Copy,
    Delete,
    Mkdir,
}

/// The side an operation writes to (or deletes from).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Direction {
    ToLeft,
    ToRight,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncOp {
    pub id: u32,
    pub kind: OpKind,
    pub direction: Direction,
    pub rel_path: String,
    pub is_dir: bool,
    /// Bytes copied for a copy; total bytes removed for a delete.
    pub size: u64,
    /// A copy that replaces an existing file (both sides have it).
    #[serde(default)]
    pub overwrite: bool,
    pub reason: String,
    /// The target (the file being replaced or deleted) as the preview saw
    /// it. The executor refuses to touch it if it has changed since.
    #[serde(default)]
    pub dst_before: Option<Snap>,
}

/// An item the plan deliberately leaves alone, with the reason why.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncSkip {
    pub rel_path: String,
    pub is_dir: bool,
    pub reason: String,
    /// True when the user should look at it (type clash, read error,
    /// ambiguous change); false for intentional skips such as "newer on the
    /// target side" in a one-way update.
    pub conflict: bool,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncSummary {
    pub copy_to_right: usize,
    pub copy_to_left: usize,
    pub delete_right: usize,
    pub delete_left: usize,
    pub mkdir_right: usize,
    pub mkdir_left: usize,
    pub bytes_to_copy: u64,
    pub conflicts: usize,
    pub skipped: usize,
}

impl SyncSummary {
    pub fn of(ops: &[SyncOp], skipped: &[SyncSkip]) -> Self {
        let mut s = SyncSummary {
            conflicts: skipped.iter().filter(|x| x.conflict).count(),
            skipped: skipped.len(),
            ..Default::default()
        };
        for op in ops {
            let right = op.direction == Direction::ToRight;
            let slot = match (op.kind, right) {
                (OpKind::Copy, true) => &mut s.copy_to_right,
                (OpKind::Copy, false) => &mut s.copy_to_left,
                (OpKind::Delete, true) => &mut s.delete_right,
                (OpKind::Delete, false) => &mut s.delete_left,
                (OpKind::Mkdir, true) => &mut s.mkdir_right,
                (OpKind::Mkdir, false) => &mut s.mkdir_left,
            };
            *slot += 1;
            if op.kind == OpKind::Copy {
                s.bytes_to_copy += op.size;
            }
        }
        s
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncPlan {
    pub left_root: String,
    pub right_root: String,
    pub mode: SyncMode,
    /// Ordered: folders are created first, then files copied, then deletions.
    pub ops: Vec<SyncOp>,
    pub skipped: Vec<SyncSkip>,
    pub summary: SyncSummary,
    pub warnings: Vec<PlanWarning>,
    /// Whether a baseline from an earlier sync informed the plan.
    pub has_baseline: bool,
    /// The comparison the plan was built from, for updating the baseline.
    #[serde(skip)]
    pub comparison: Option<FolderCompareResult>,
    pub elapsed_ms: u128,
}

// ---------- planning ----------

/// Compares two folders and plans the operations for `mode`. The checksum
/// cache, when given, lets content comparisons skip unchanged files.
#[allow(clippy::too_many_arguments)]
pub fn plan_sync_with(
    left_fs: &dyn Vfs,
    left: &str,
    right_fs: &dyn Vfs,
    right: &str,
    mode: SyncMode,
    opts: &FolderCompareOptions,
    plan_opts: &PlanOptions,
    cache: Option<&dyn ChecksumCache>,
) -> pcompare_core::Result<SyncPlan> {
    let started = Instant::now();
    let opts = FolderCompareOptions {
        recursive: true,
        ..opts.clone()
    };
    let cmp = folder::compare_folders_cached(left_fs, left, right_fs, right, &opts, cache)?;
    let mut plan = plan_from_comparison_with(&cmp, mode, &opts, plan_opts);
    plan.comparison = Some(cmp);
    plan.elapsed_ms = started.elapsed().as_millis();
    Ok(plan)
}

/// Local-disk convenience wrapper around [`plan_sync_with`].
pub fn plan_sync(
    left: impl AsRef<Path>,
    right: impl AsRef<Path>,
    mode: SyncMode,
    opts: &FolderCompareOptions,
) -> pcompare_core::Result<SyncPlan> {
    plan_sync_local(left, right, mode, opts, &PlanOptions::default())
}

/// [`plan_sync`] with a baseline and conflict policy.
pub fn plan_sync_local(
    left: impl AsRef<Path>,
    right: impl AsRef<Path>,
    mode: SyncMode,
    opts: &FolderCompareOptions,
    plan_opts: &PlanOptions,
) -> pcompare_core::Result<SyncPlan> {
    let fs = pcompare_core::vfs::LocalFs;
    let (l, r) = (
        left.as_ref().to_string_lossy(),
        right.as_ref().to_string_lossy(),
    );
    plan_sync_with(&fs, &l, &fs, &r, mode, opts, plan_opts, None)
}

struct Planner<'a> {
    mkdirs: Vec<SyncOp>,
    copies: Vec<SyncOp>,
    deletes: Vec<SyncOp>,
    skipped: Vec<SyncSkip>,
    opts: &'a FolderCompareOptions,
    baseline: Option<&'a Baseline>,
    conflicts: ConflictPolicy,
}

/// The side an operation toward `dir` overwrites or deletes.
fn target_meta(e: &FolderEntry, dir: Direction) -> Option<&EntryMeta> {
    match dir {
        Direction::ToRight => e.right.as_ref(),
        Direction::ToLeft => e.left.as_ref(),
    }
}

fn toward(side: Side) -> Direction {
    match side {
        Side::Left => Direction::ToLeft,
        Side::Right => Direction::ToRight,
    }
}

fn other(side: Side) -> Side {
    match side {
        Side::Left => Side::Right,
        Side::Right => Side::Left,
    }
}

fn side_name(side: Side) -> &'static str {
    match side {
        Side::Left => "left",
        Side::Right => "right",
    }
}

impl Planner<'_> {
    fn push(
        &mut self,
        kind: OpKind,
        direction: Direction,
        e: &FolderEntry,
        size: u64,
        reason: &str,
    ) {
        let dst_before = match kind {
            OpKind::Mkdir => None,
            OpKind::Copy | OpKind::Delete => target_meta(e, direction).map(Snap::from),
        };
        let op = SyncOp {
            id: 0,
            kind,
            direction,
            rel_path: e.rel_path.clone(),
            is_dir: e.is_dir,
            size,
            overwrite: kind == OpKind::Copy && e.left.is_some() && e.right.is_some(),
            reason: reason.to_owned(),
            dst_before,
        };
        match kind {
            OpKind::Mkdir => self.mkdirs.push(op),
            OpKind::Copy => self.copies.push(op),
            OpKind::Delete => self.deletes.push(op),
        }
    }

    fn skip(&mut self, e: &FolderEntry, reason: impl Into<String>, conflict: bool) {
        self.skipped.push(SyncSkip {
            rel_path: e.rel_path.clone(),
            is_dir: e.is_dir,
            reason: reason.into(),
            conflict,
        });
    }

    fn copy(&mut self, e: &FolderEntry, dir: Direction, reason: &str) {
        let size = source_meta(e, dir).map_or(0, |m| m.size);
        self.push(OpKind::Copy, dir, e, size, reason)
    }

    /// Recreates an orphan subtree on the other side: one mkdir per folder
    /// (parents before children) and one copy per file.
    fn copy_orphan(&mut self, e: &FolderEntry, dir: Direction, reason: &str) {
        if e.is_dir {
            self.push(OpKind::Mkdir, dir, e, 0, reason);
            for c in &e.children {
                self.copy_orphan(c, dir, reason);
            }
        } else {
            self.copy(e, dir, reason);
        }
    }

    fn delete(&mut self, e: &FolderEntry, dir: Direction, reason: &str) {
        // A folder is deleted as a whole, so its children need no ops.
        self.push(OpKind::Delete, dir, e, tree_size(e), reason);
    }

    fn walk(&mut self, entries: &[FolderEntry], mode: SyncMode) {
        use Direction::{ToLeft, ToRight};
        use SyncMode::*;
        for e in entries {
            match e.status {
                EntryStatus::Same | EntryStatus::Similar | EntryStatus::Different if e.is_dir => {
                    self.walk(&e.children, mode)
                }
                // Rules-based "similar" files differ only in unimportant ways.
                EntryStatus::Same | EntryStatus::Similar => {}
                EntryStatus::LeftOnly if mode == TwoWay => self.two_way_orphan(e, Side::Left),
                EntryStatus::RightOnly if mode == TwoWay => self.two_way_orphan(e, Side::Right),
                EntryStatus::LeftOnly => match mode {
                    UpdateLeftToRight | UpdateBoth | MirrorLeftToRight | TwoWay => {
                        self.copy_orphan(e, ToRight, "orphan")
                    }
                    MirrorRightToLeft => self.delete(e, ToLeft, "mirror: not in source"),
                    UpdateRightToLeft => {}
                },
                EntryStatus::RightOnly => match mode {
                    UpdateRightToLeft | UpdateBoth | MirrorRightToLeft | TwoWay => {
                        self.copy_orphan(e, ToLeft, "orphan")
                    }
                    MirrorLeftToRight => self.delete(e, ToRight, "mirror: not in source"),
                    UpdateLeftToRight => {}
                },
                EntryStatus::Different if mode == TwoWay => self.two_way_changed(e),
                EntryStatus::Different => self.different_file(e, mode),
                EntryStatus::TypeMismatch => {
                    self.skip(e, "a file on one side and a folder on the other", true)
                }
                EntryStatus::Error => {
                    let msg = e.error.as_deref().unwrap_or("cannot be compared");
                    self.skip(e, format!("error: {msg}"), true)
                }
            }
        }
    }

    fn different_file(&mut self, e: &FolderEntry, mode: SyncMode) {
        use Direction::{ToLeft, ToRight};
        use SyncMode::*;
        match (mode, e.newer) {
            (UpdateLeftToRight | UpdateBoth | TwoWay, Some(Side::Left)) => {
                self.copy(e, ToRight, "newer on left")
            }
            (UpdateRightToLeft | UpdateBoth | TwoWay, Some(Side::Right)) => {
                self.copy(e, ToLeft, "newer on right")
            }
            (UpdateLeftToRight, Some(Side::Right)) => {
                self.skip(e, "newer on right; not overwritten", false)
            }
            (UpdateRightToLeft, Some(Side::Left)) => {
                self.skip(e, "newer on left; not overwritten", false)
            }
            (UpdateLeftToRight | UpdateRightToLeft | UpdateBoth | TwoWay, None) => {
                self.skip(e, "differs, but neither side is newer", true)
            }
            (MirrorLeftToRight, newer) => {
                let reason = match newer {
                    Some(Side::Left) => "newer on left",
                    Some(Side::Right) => "mirror: older on left",
                    None => "different",
                };
                self.copy(e, ToRight, reason)
            }
            (MirrorRightToLeft, newer) => {
                let reason = match newer {
                    Some(Side::Right) => "newer on right",
                    Some(Side::Left) => "mirror: older on right",
                    None => "different",
                };
                self.copy(e, ToLeft, reason)
            }
        }
    }

    /// Two-way: a file present on both sides with different contents.
    fn two_way_changed(&mut self, e: &FolderEntry) {
        let (Some(l), Some(r)) = (&e.left, &e.right) else {
            return;
        };
        let Some(base) = self.baseline.and_then(|b| b.get(&e.rel_path)).filter(|b| !b.is_dir) else {
            // Never synced before: fall back to "newer wins".
            return self.different_file(e, SyncMode::TwoWay);
        };
        let lc = baseline::side_changed(l, &base.left, self.opts);
        let rc = baseline::side_changed(r, &base.right, self.opts);
        match (lc, rc) {
            (true, false) => self.copy(e, Direction::ToRight, "changed on left"),
            (false, true) => self.copy(e, Direction::ToLeft, "changed on right"),
            _ => {
                let winner = match self.conflicts {
                    ConflictPolicy::Skip => None,
                    ConflictPolicy::NewerWins => e.newer,
                    ConflictPolicy::LeftWins => Some(Side::Left),
                    ConflictPolicy::RightWins => Some(Side::Right),
                };
                match winner {
                    Some(side) => self.copy(
                        e,
                        toward(other(side)),
                        &format!("conflict: {} version kept", side_name(side)),
                    ),
                    None => self.skip(e, "changed on both sides since the last sync", true),
                }
            }
        }
    }

    /// Two-way: an item that exists only on `side`.
    fn two_way_orphan(&mut self, e: &FolderEntry, side: Side) {
        let to_other = toward(other(side));
        if self.baseline.and_then(|b| b.get(&e.rel_path)).is_none() {
            let reason = format!("new on {}", side_name(side));
            return self.copy_orphan(e, to_other, &reason);
        }
        // It was in sync last time, so the other side deleted it.
        let deleted_on = side_name(other(side));
        if self.unchanged_since_baseline(e, side) {
            return self.delete(e, toward(side), &format!("deleted on {deleted_on}"));
        }
        let keep_edit = match self.conflicts {
            ConflictPolicy::Skip => None,
            // An edit beats a deletion: losing work is worse than resurrecting a file.
            ConflictPolicy::NewerWins => Some(true),
            ConflictPolicy::LeftWins => Some(side == Side::Left),
            ConflictPolicy::RightWins => Some(side == Side::Right),
        };
        match keep_edit {
            Some(true) => self.copy_orphan(e, to_other, "conflict: edit kept over deletion"),
            Some(false) => self.delete(e, toward(side), "conflict: deletion kept over edit"),
            None => self.skip(
                e,
                format!("changed on {}, deleted on {deleted_on}", side_name(side)),
                true,
            ),
        }
    }

    /// Whether an orphan (and, for folders, everything inside) is exactly as
    /// it was at the last sync — i.e. safe to treat as deleted elsewhere.
    fn unchanged_since_baseline(&self, e: &FolderEntry, side: Side) -> bool {
        let Some(base) = self.baseline.and_then(|b| b.get(&e.rel_path)) else {
            return false; // new since the baseline
        };
        let Some(now) = (match side {
            Side::Left => e.left.as_ref(),
            Side::Right => e.right.as_ref(),
        }) else {
            return false;
        };
        if now.is_dir != base.is_dir {
            return false;
        }
        if now.is_dir {
            return e.children.iter().all(|c| self.unchanged_since_baseline(c, side));
        }
        let then = match side {
            Side::Left => &base.left,
            Side::Right => &base.right,
        };
        !baseline::side_changed(now, then, self.opts)
    }
}

/// Metadata of the side a copy toward `dir` reads from.
fn source_meta(e: &FolderEntry, dir: Direction) -> Option<&folder::EntryMeta> {
    match dir {
        Direction::ToRight => e.left.as_ref(),
        Direction::ToLeft => e.right.as_ref(),
    }
}

fn tree_size(e: &FolderEntry) -> u64 {
    if e.is_dir {
        e.children.iter().map(tree_size).sum()
    } else {
        e.left.as_ref().or(e.right.as_ref()).map_or(0, |m| m.size)
    }
}

/// Files present on `side` in a tree.
fn count_files(entries: &[FolderEntry], side: Side) -> usize {
    entries
        .iter()
        .map(|e| {
            let present = match side {
                Side::Left => e.left.is_some(),
                Side::Right => e.right.is_some(),
            };
            match (present, e.is_dir) {
                (false, _) => 0,
                (true, true) => count_files(&e.children, side),
                (true, false) => 1,
            }
        })
        .sum()
}

/// Files a delete op removes (1 for a file, the whole tree for a folder).
fn files_in(cmp: &[FolderEntry], rel: &str) -> usize {
    fn find<'a>(entries: &'a [FolderEntry], rel: &str) -> Option<&'a FolderEntry> {
        entries.iter().find_map(|e| {
            if e.rel_path == rel {
                Some(e)
            } else if rel.starts_with(&e.rel_path) && rel.as_bytes().get(e.rel_path.len()) == Some(&b'/') {
                find(&e.children, rel)
            } else {
                None
            }
        })
    }
    match find(cmp, rel) {
        Some(e) if e.is_dir => count_files(&e.children, Side::Left) + count_files(&e.children, Side::Right),
        _ => 1,
    }
}

/// Deleting at least this many files *and* this share of a side is flagged.
const MASS_DELETE_MIN_FILES: usize = 20;
const MASS_DELETE_SHARE: f64 = 0.5;

/// Safety checks against the classic catastrophe: mirroring (or two-way
/// syncing) from a drive that isn't mounted, which looks exactly like
/// "everything was deleted".
fn plan_warnings(cmp: &FolderCompareResult, ops: &[SyncOp]) -> Vec<PlanWarning> {
    let mut warnings = Vec::new();
    for (side, dir) in [(Side::Left, Direction::ToLeft), (Side::Right, Direction::ToRight)] {
        let deleted: usize = ops
            .iter()
            .filter(|o| o.kind == OpKind::Delete && o.direction == dir)
            .map(|o| if o.is_dir { files_in(&cmp.entries, &o.rel_path) } else { 1 })
            .sum();
        if deleted == 0 {
            continue;
        }
        let name = side_name(side);
        let present_other = count_files(&cmp.entries, other(side));
        let present_here = count_files(&cmp.entries, side);
        if present_other == 0 {
            warnings.push(PlanWarning {
                kind: WarningKind::EmptySide,
                message: format!(
                    "The {} folder is empty, so this sync deletes everything on the {name} ({deleted} files).                      If it is on a drive or network share that isn't connected, stop now.",
                    side_name(other(side)),
                ),
            });
        } else if deleted >= MASS_DELETE_MIN_FILES && deleted as f64 >= present_here as f64 * MASS_DELETE_SHARE {
            warnings.push(PlanWarning {
                kind: WarningKind::MassDeletion,
                message: format!(
                    "This sync deletes {deleted} of the {present_here} files on the {name}.",
                ),
            });
        }
    }
    warnings
}

/// Plans operations from an existing comparison (which must be recursive).
pub fn plan_from_comparison(cmp: &FolderCompareResult, mode: SyncMode) -> SyncPlan {
    plan_from_comparison_with(cmp, mode, &FolderCompareOptions::default(), &PlanOptions::default())
}

/// [`plan_from_comparison`] with the compare options (whose timestamp
/// tolerance decides what counts as changed since the baseline), a baseline
/// and a conflict policy.
pub fn plan_from_comparison_with(
    cmp: &FolderCompareResult,
    mode: SyncMode,
    opts: &FolderCompareOptions,
    plan_opts: &PlanOptions,
) -> SyncPlan {
    let baseline = plan_opts.baseline.filter(|b| !b.is_empty());
    let mut p = Planner {
        mkdirs: Vec::new(),
        copies: Vec::new(),
        deletes: Vec::new(),
        skipped: Vec::new(),
        opts,
        baseline,
        conflicts: plan_opts.conflicts,
    };
    p.walk(&cmp.entries, mode);
    let mut ops = p.mkdirs;
    ops.append(&mut p.copies);
    ops.append(&mut p.deletes);
    for (i, op) in ops.iter_mut().enumerate() {
        op.id = i as u32;
    }
    let summary = SyncSummary::of(&ops, &p.skipped);
    let warnings = plan_warnings(cmp, &ops);
    SyncPlan {
        left_root: cmp.left_root.clone(),
        right_root: cmp.right_root.clone(),
        mode,
        ops,
        skipped: p.skipped,
        summary,
        warnings,
        has_baseline: mode == SyncMode::TwoWay && baseline.is_some(),
        comparison: None,
        elapsed_ms: cmp.elapsed_ms,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::{self, ExecOptions, ExecReport};
    use crate::fsops::PermanentDeleter;
    use crate::journal::NullJournal;
    use filetime::FileTime;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, SystemTime};

    const OLD: u64 = 1_600_000_000;
    const NEW: u64 = 1_700_000_000;

    fn write(root: &Path, rel: &str, body: &str, secs: u64) {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, body).unwrap();
        filetime::set_file_mtime(&p, FileTime::from_unix_time(secs as i64, 0)).unwrap();
    }

    fn mtime(p: &Path) -> SystemTime {
        fs::metadata(p).unwrap().modified().unwrap()
    }

    fn opts() -> FolderCompareOptions {
        FolderCompareOptions {
            ignore_name_case: false,
            ..Default::default()
        }
    }

    struct Trees {
        _dir: tempfile::TempDir,
        left: PathBuf,
        right: PathBuf,
    }

    /// left:  same.txt, newer-left.txt (new), newer-right.txt (old), lonly.txt, lonly-dir/{a, sub/b}
    /// right: same.txt, newer-left.txt (old), newer-right.txt (new), ronly.txt, ronly-dir/{c, d}
    fn trees() -> Trees {
        let dir = tempfile::tempdir().unwrap();
        let (left, right) = (dir.path().join("L"), dir.path().join("R"));
        fs::create_dir_all(&left).unwrap();
        fs::create_dir_all(&right).unwrap();
        write(&left, "same.txt", "same", OLD);
        write(&right, "same.txt", "same", OLD);
        write(&left, "newer-left.txt", "left v2", NEW);
        write(&right, "newer-left.txt", "right v1", OLD);
        write(&left, "newer-right.txt", "left v1", OLD);
        write(&right, "newer-right.txt", "right v2", NEW);
        write(&left, "lonly.txt", "L", OLD);
        write(&left, "lonly-dir/a.txt", "aaa", OLD);
        write(&left, "lonly-dir/sub/b.txt", "bb", OLD);
        write(&right, "ronly.txt", "R", OLD);
        write(&right, "ronly-dir/c.txt", "c", OLD);
        write(&right, "ronly-dir/d.txt", "d", OLD);
        Trees {
            _dir: dir,
            left,
            right,
        }
    }

    fn plan(t: &Trees, mode: SyncMode) -> SyncPlan {
        plan_sync(&t.left, &t.right, mode, &opts()).unwrap()
    }

    fn run(t: &Trees, ops: &[SyncOp]) -> ExecReport {
        exec::execute(
            &t.left,
            &t.right,
            ops,
            &ExecOptions::default(),
            &PermanentDeleter,
            &mut NullJournal,
            &AtomicBool::new(false),
            &mut |_| {},
        )
    }

    fn describe(p: &SyncPlan) -> Vec<String> {
        p.ops
            .iter()
            .map(|o| {
                let d = if o.direction == Direction::ToRight {
                    ">"
                } else {
                    "<"
                };
                format!("{:?}{d}{}", o.kind, o.rel_path)
            })
            .collect()
    }

    #[test]
    fn update_left_to_right_never_deletes() {
        let t = trees();
        let p = plan(&t, SyncMode::UpdateLeftToRight);
        assert_eq!(
            describe(&p),
            [
                "Mkdir>lonly-dir",
                "Mkdir>lonly-dir/sub",
                "Copy>lonly-dir/a.txt",
                "Copy>lonly-dir/sub/b.txt",
                "Copy>lonly.txt",
                "Copy>newer-left.txt",
            ]
        );
        assert!(p.ops.iter().enumerate().all(|(i, o)| o.id == i as u32));
        let overwrites: Vec<_> = p
            .ops
            .iter()
            .filter(|o| o.overwrite)
            .map(|o| o.rel_path.as_str())
            .collect();
        assert_eq!(overwrites, ["newer-left.txt"]);
        assert_eq!(p.summary.copy_to_right, 4);
        assert_eq!(p.summary.mkdir_right, 2);
        assert_eq!(p.summary.bytes_to_copy, 3 + 2 + 1 + 7);
        assert_eq!(p.skipped.len(), 1);
        assert_eq!(p.skipped[0].rel_path, "newer-right.txt");
        assert!(!p.skipped[0].conflict);

        let r = run(&t, &p.ops);
        assert_eq!((r.succeeded, r.failed), (6, 0));
        assert_eq!(
            fs::read_to_string(t.right.join("newer-left.txt")).unwrap(),
            "left v2"
        );
        assert_eq!(
            fs::read_to_string(t.right.join("newer-right.txt")).unwrap(),
            "right v2"
        );
        assert!(t.right.join("ronly.txt").exists(), "update must not delete");
        assert_eq!(
            mtime(&t.right.join("lonly-dir/sub/b.txt")),
            mtime(&t.left.join("lonly-dir/sub/b.txt"))
        );

        // Re-planning after the run leaves only the intentional skip.
        let again = plan(&t, SyncMode::UpdateLeftToRight);
        assert!(again.ops.is_empty(), "{:?}", describe(&again));
    }

    #[test]
    fn update_right_to_left() {
        let t = trees();
        let p = plan(&t, SyncMode::UpdateRightToLeft);
        assert_eq!(
            describe(&p),
            [
                "Mkdir<ronly-dir",
                "Copy<ronly-dir/c.txt",
                "Copy<ronly-dir/d.txt",
                "Copy<newer-right.txt",
                "Copy<ronly.txt"
            ]
        );
        run(&t, &p.ops);
        assert_eq!(
            fs::read_to_string(t.left.join("newer-right.txt")).unwrap(),
            "right v2"
        );
        assert_eq!(
            mtime(&t.left.join("newer-right.txt")),
            SystemTime::UNIX_EPOCH + Duration::from_secs(NEW)
        );
        assert!(t.left.join("lonly.txt").exists());
    }

    #[test]
    fn update_both_newer_wins_each_way() {
        let t = trees();
        let p = plan(&t, SyncMode::UpdateBoth);
        assert_eq!(p.summary.copy_to_right, 4);
        assert_eq!(p.summary.copy_to_left, 4);
        assert_eq!(p.summary.delete_left + p.summary.delete_right, 0);
        let r = run(&t, &p.ops);
        assert_eq!(r.failed, 0);
        let again = plan(&t, SyncMode::UpdateBoth);
        assert!(
            again.ops.is_empty() && again.skipped.is_empty(),
            "{:?}",
            describe(&again)
        );
    }

    #[test]
    fn mirror_left_to_right_deletes_and_subsumes_folders() {
        let t = trees();
        let p = plan(&t, SyncMode::MirrorLeftToRight);
        let d = describe(&p);
        // One delete for the whole folder, none for its children.
        assert!(d.contains(&"Delete>ronly-dir".to_string()));
        assert!(!d.iter().any(|x| x.starts_with("Delete>ronly-dir/")));
        assert!(d.contains(&"Delete>ronly.txt".to_string()));
        // Older-on-left files are overwritten too.
        assert!(d.contains(&"Copy>newer-right.txt".to_string()));
        let del = p.ops.iter().find(|o| o.rel_path == "ronly-dir").unwrap();
        assert_eq!(del.size, 2);
        assert!(del.is_dir);
        // Order: mkdir, copy, delete.
        let kinds: Vec<_> = p.ops.iter().map(|o| o.kind).collect();
        let first_copy = kinds.iter().position(|k| *k == OpKind::Copy).unwrap();
        let first_del = kinds.iter().position(|k| *k == OpKind::Delete).unwrap();
        assert!(kinds[..first_copy].iter().all(|k| *k == OpKind::Mkdir));
        assert!(kinds[first_del..].iter().all(|k| *k == OpKind::Delete));

        let r = run(&t, &p.ops);
        assert_eq!(r.failed, 0, "{:?}", r.results);
        assert!(!t.right.join("ronly-dir").exists());
        assert!(!t.right.join("ronly.txt").exists());
        let again = plan(&t, SyncMode::MirrorLeftToRight);
        assert!(
            again.ops.is_empty() && again.skipped.is_empty(),
            "{:?}",
            describe(&again)
        );
    }

    #[test]
    fn mirror_right_to_left() {
        let t = trees();
        let p = plan(&t, SyncMode::MirrorRightToLeft);
        let d = describe(&p);
        assert!(d.contains(&"Delete<lonly-dir".to_string()));
        assert!(d.contains(&"Delete<lonly.txt".to_string()));
        assert!(d.contains(&"Copy<newer-left.txt".to_string()));
        run(&t, &p.ops);
        assert_eq!(
            fs::read_to_string(t.left.join("newer-left.txt")).unwrap(),
            "right v1"
        );
        assert!(plan(&t, SyncMode::MirrorRightToLeft).ops.is_empty());
    }

    #[test]
    fn type_mismatch_and_ambiguous_changes_are_conflicts() {
        let t = trees();
        write(&t.left, "clash", "file", OLD);
        write(&t.right, "clash/inner.txt", "dir", OLD);
        write(&t.left, "tie.txt", "aaaa", OLD);
        write(&t.right, "tie.txt", "bbbbbb", OLD);
        let p = plan(&t, SyncMode::UpdateBoth);
        let conflicts: Vec<_> = p
            .skipped
            .iter()
            .filter(|s| s.conflict)
            .map(|s| s.rel_path.as_str())
            .collect();
        assert_eq!(conflicts, ["clash", "tie.txt"]);
        assert_eq!(p.summary.conflicts, 2);
        assert!(!p
            .ops
            .iter()
            .any(|o| o.rel_path.starts_with("clash") || o.rel_path == "tie.txt"));
    }

    #[test]
    fn mirror_copies_differing_files_with_equal_times() {
        let t = trees();
        write(&t.left, "tie.txt", "aaaa", OLD);
        write(&t.right, "tie.txt", "bbbbbb", OLD);
        let p = plan(&t, SyncMode::MirrorLeftToRight);
        let op = p.ops.iter().find(|o| o.rel_path == "tie.txt").unwrap();
        assert_eq!((op.kind, op.reason.as_str()), (OpKind::Copy, "different"));
    }

    #[test]
    fn errors_do_not_abort_the_batch() {
        let t = trees();
        let mut p = plan(&t, SyncMode::UpdateLeftToRight);
        // A vanished source, an unsafe path, then a valid copy.
        fs::remove_file(t.left.join("lonly.txt")).unwrap();
        p.ops.push(SyncOp {
            id: 99,
            kind: OpKind::Copy,
            direction: Direction::ToRight,
            rel_path: "../escape.txt".into(),
            is_dir: false,
            size: 0,
            overwrite: false,
            reason: String::new(),
            dst_before: None,
        });
        let r = run(&t, &p.ops);
        assert_eq!(r.failed, 2);
        assert_eq!(r.succeeded, p.ops.len() - 2);
        let failed: Vec<_> = r
            .results
            .iter()
            .filter(|x| !x.ok)
            .map(|x| x.rel_path.as_str())
            .collect();
        assert_eq!(failed, ["lonly.txt", "../escape.txt"]);
        assert!(r
            .results
            .iter()
            .filter(|x| !x.ok)
            .all(|x| x.error.is_some()));
        assert!(t.right.join("newer-left.txt").exists());
        assert!(!t.left.parent().unwrap().join("escape.txt").exists());
    }

    #[test]
    fn filtered_ops_still_create_parents() {
        let t = trees();
        let p = plan(&t, SyncMode::UpdateLeftToRight);
        // The user unticked the mkdir rows but kept a nested copy.
        let only: Vec<_> = p
            .ops
            .iter()
            .filter(|o| o.rel_path == "lonly-dir/sub/b.txt")
            .cloned()
            .collect();
        let r = run(&t, &only);
        assert_eq!(r.succeeded, 1);
        assert!(t.right.join("lonly-dir/sub/b.txt").is_file());
        assert!(!t.right.join("lonly-dir/a.txt").exists());
    }

    #[test]
    fn cancellation_stops_before_next_op_and_reports_progress() {
        let t = trees();
        let p = plan(&t, SyncMode::MirrorLeftToRight);
        let cancel = AtomicBool::new(false);
        let mut events = Vec::new();
        let r = exec::execute(
            &t.left,
            &t.right,
            &p.ops,
            &ExecOptions::default(),
            &PermanentDeleter,
            &mut NullJournal,
            &cancel,
            &mut |s| {
                events.push(s.clone());
                if s.done == 2 {
                    cancel.store(true, Ordering::Relaxed);
                }
            },
        );
        assert!(r.cancelled);
        assert_eq!(r.results.len(), 2);
        let last = events.last().unwrap();
        assert_eq!(
            (last.done, last.total, last.current.clone()),
            (2, p.ops.len(), None)
        );
    }

    #[test]
    fn delete_of_missing_item_is_ok() {
        let t = trees();
        let ops = [SyncOp {
            id: 0,
            kind: OpKind::Delete,
            direction: Direction::ToRight,
            rel_path: "not-there.txt".into(),
            is_dir: false,
            size: 0,
            overwrite: false,
            reason: String::new(),
            dst_before: None,
        }];
        assert_eq!(run(&t, &ops).succeeded, 1);
    }

    #[test]
    #[ignore = "moves a file to the real Recycle Bin / Trash"]
    fn trash_deleter_removes_path() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("trash-me.txt");
        fs::write(&p, "x").unwrap();
        // The trash may be unavailable on headless CI; only assert when it worked.
        if crate::fsops::Deleter::delete(&crate::fsops::TrashDeleter::new(), &p).is_ok() {
            assert!(!p.exists());
        }
    }
}
