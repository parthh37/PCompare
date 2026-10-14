//! Folder synchronization: turn a two-way folder comparison into an ordered
//! list of copy / delete / mkdir operations for a chosen mode, then perform a
//! (possibly user-filtered) subset of them.
//!
//! Planning reuses [`pcompare_core::folder`] so the preview agrees exactly
//! with what a Folder Compare session shows for the same options.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::Instant;

use pcompare_core::folder::{
    self, ChecksumCache, EntryMeta, EntryStatus, FolderCompareOptions, FolderCompareResult,
    FolderEntry, Side,
};
use pcompare_core::vfs::Vfs;
use serde::{Deserialize, Serialize};

use crate::baseline::{self, Baseline, Snap};
pub use crate::matrix::{SyncAction, SyncCategory, SyncMatrix};

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
    /// The user's own [`SyncMatrix`] (`PlanOptions::matrix`). Uses the
    /// baseline like [`SyncMode::TwoWay`], so it can tell deletions from
    /// orphans.
    Custom,
}

impl SyncMode {
    /// The fixed modes, each a preset row of the [`SyncMatrix`].
    pub const PRESETS: [SyncMode; 6] = [
        SyncMode::UpdateLeftToRight,
        SyncMode::UpdateRightToLeft,
        SyncMode::UpdateBoth,
        SyncMode::MirrorLeftToRight,
        SyncMode::MirrorRightToLeft,
        SyncMode::TwoWay,
    ];

    /// Whether planning consults the last sync's baseline.
    pub fn uses_baseline(self) -> bool {
        matches!(self, SyncMode::TwoWay | SyncMode::Custom)
    }
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
    /// State after the last successful sync (for [`SyncMode::TwoWay`] and
    /// [`SyncMode::Custom`]; the other modes ignore it).
    pub baseline: Option<&'a Baseline>,
    pub conflicts: ConflictPolicy,
    /// The matrix [`SyncMode::Custom`] plans with; the fixed modes use
    /// their preset ([`SyncMatrix::for_mode`]).
    pub matrix: SyncMatrix,
    /// Actions the user picked in the preview, by [`FolderEntry::rel_path`].
    /// They win over the matrix and the conflict policy. On a folder,
    /// "delete" removes it whole, and the other actions apply to everything
    /// inside that isn't the same on both sides (or has its own override).
    pub overrides: Option<&'a HashMap<String, SyncAction>>,
    /// Plan only these items and their contents ("Synchronize selection"),
    /// by [`FolderEntry::rel_path`]; `None` plans everything.
    pub scope: Option<&'a [String]>,
    /// Don't create orphan folders that hold no files (empty, or emptied by
    /// the filters) — BC's "Sync empty folders" off.
    pub skip_empty_folders: bool,
}

/// The owned, serializable part of [`PlanOptions`] besides the baseline and
/// conflict policy, as sessions, automation tasks and the shell pass it
/// around (bc5:sync-matrix).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PlanRules {
    /// Used by [`SyncMode::Custom`] only.
    pub matrix: SyncMatrix,
    pub skip_empty_folders: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<Vec<String>>,
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    pub overrides: HashMap<String, SyncAction>,
}

impl PlanRules {
    pub fn options<'a>(&'a self, baseline: Option<&'a Baseline>, conflicts: ConflictPolicy) -> PlanOptions<'a> {
        PlanOptions {
            baseline,
            conflicts,
            matrix: self.matrix,
            overrides: Some(&self.overrides),
            scope: self.scope.as_deref(),
            skip_empty_folders: self.skip_empty_folders,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum WarningKind {
    /// One side is empty while the other (or the baseline) is not — often an
    /// unmounted drive or a disconnected share rather than a real deletion.
    EmptySide,
    /// The plan deletes a large share of the files on one side.
    MassDeletion,
    /// The plan deletes or replaces files on a remote side, which has no
    /// Recycle Bin (bc5:remote-write).
    RemotePermanent,
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
    /// The left side's own relative path when alignment paired differently
    /// named items (case, NFC/NFD, ignored extensions, masks); `rel_path` is
    /// the right side's. `None` when both sides use `rel_path`
    /// (bc5:file-ops — [`FolderEntry::left_rel_path`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub left_rel_path: Option<String>,
    /// The category that chose this op, for the preview's per-row actions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<SyncCategory>,
}

impl SyncOp {
    /// The op's relative path on one side.
    pub fn rel_on(&self, side: Direction) -> &str {
        match side {
            Direction::ToRight => &self.rel_path,
            Direction::ToLeft => self.left_rel_path.as_deref().unwrap_or(&self.rel_path),
        }
    }

    /// Relative path on the side the op writes to (or deletes from).
    pub fn dst_rel(&self) -> &str {
        self.rel_on(self.direction)
    }

    /// Relative path on the side a copy reads from.
    pub fn src_rel(&self) -> &str {
        self.rel_on(match self.direction {
            Direction::ToRight => Direction::ToLeft,
            Direction::ToLeft => Direction::ToRight,
        })
    }
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
    /// How the item compares; `None` for type clashes, read errors and
    /// paths changed on both sides.
    pub category: Option<SyncCategory>,
    /// Which sides have the item, so the preview offers only the actions
    /// that can apply to it.
    pub on_left: bool,
    pub on_right: bool,
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
    /// The matrix the plan followed: the mode's preset, or the custom one.
    pub matrix: SyncMatrix,
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
    /// Only for modes that use one ([`SyncMode::uses_baseline`]).
    baseline: Option<&'a Baseline>,
    conflicts: ConflictPolicy,
    matrix: SyncMatrix,
    overrides: Option<&'a HashMap<String, SyncAction>>,
    scope: Option<Scope>,
    skip_empty_folders: bool,
    /// Left-side relative paths of the folders being walked, innermost last,
    /// so right-only items below an aligned folder land in the left folder it
    /// was paired with (bc5:file-ops).
    left_dirs: Vec<String>,
}

/// The items "Synchronize selection" covers, and the folders above them.
struct Scope {
    paths: HashSet<String>,
    ancestors: HashSet<String>,
}

impl Scope {
    fn new(paths: &[String]) -> Self {
        let mut ancestors = HashSet::new();
        for p in paths {
            let mut rest = p.as_str();
            while let Some(i) = rest.rfind('/') {
                rest = &rest[..i];
                ancestors.insert(rest.to_owned());
            }
        }
        Scope {
            paths: paths.iter().cloned().collect(),
            ancestors,
        }
    }
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

/// An orphan on `side`: new there, or (with a baseline) deleted on the other side.
fn orphan_category(side: Side, deleted_elsewhere: bool) -> SyncCategory {
    match (side, deleted_elsewhere) {
        (Side::Left, false) => SyncCategory::LeftOrphan,
        (Side::Right, false) => SyncCategory::RightOrphan,
        (Side::Left, true) => SyncCategory::RightDeleted,
        (Side::Right, true) => SyncCategory::LeftDeleted,
    }
}

/// Why a copy toward `dir` happens, given what the item's state (`why`) is.
fn copy_reason(cat: SyncCategory, why: &str, dir: Direction) -> String {
    use SyncCategory::*;
    match (cat, dir) {
        (LeftNewer, Direction::ToLeft) => "older on right".into(),
        (RightNewer, Direction::ToRight) => "older on left".into(),
        (LeftDeleted, Direction::ToLeft) | (RightDeleted, Direction::ToRight) => format!("{why}; restored"),
        (Same, _) => format!("{why}; copied anyway"),
        _ => why.into(),
    }
}

impl Planner<'_> {
    /// `e`'s relative path on the left side (bc5:file-ops).
    fn left_rel(&self, e: &FolderEntry) -> String {
        if let Some(p) = &e.left_rel_path {
            return p.clone();
        }
        match self.left_dirs.last() {
            Some(parent) if e.left.is_none() => format!("{parent}/{}", e.name),
            _ => e.rel_path.clone(),
        }
    }

    /// Runs `f` with `e`'s left path as the folder being walked.
    fn inside<T>(&mut self, e: &FolderEntry, f: impl FnOnce(&mut Self) -> T) -> T {
        let dir = self.left_rel(e);
        self.left_dirs.push(dir);
        let out = f(self);
        self.left_dirs.pop();
        out
    }

    fn override_of(&self, e: &FolderEntry) -> Option<SyncAction> {
        self.overrides.and_then(|o| o.get(&e.rel_path).copied())
    }

    #[allow(clippy::too_many_arguments)]
    fn push(
        &mut self,
        kind: OpKind,
        direction: Direction,
        e: &FolderEntry,
        size: u64,
        reason: &str,
        category: Option<SyncCategory>,
    ) {
        let target = target_meta(e, direction);
        let dst_before = match kind {
            OpKind::Mkdir => None,
            OpKind::Copy | OpKind::Delete => target.map(Snap::from),
        };
        // A type clash's sides disagree; a delete removes what its side holds.
        let is_dir = match (kind, target) {
            (OpKind::Delete, Some(m)) => m.is_dir,
            _ => e.is_dir,
        };
        let op = SyncOp {
            id: 0,
            kind,
            direction,
            rel_path: e.rel_path.clone(),
            is_dir,
            size,
            overwrite: kind == OpKind::Copy && e.left.is_some() && e.right.is_some(),
            reason: reason.to_owned(),
            dst_before,
            left_rel_path: Some(self.left_rel(e)).filter(|p| *p != e.rel_path),
            category,
        };
        match kind {
            OpKind::Mkdir => self.mkdirs.push(op),
            OpKind::Copy => self.copies.push(op),
            OpKind::Delete => self.deletes.push(op),
        }
    }

    fn skip(&mut self, e: &FolderEntry, category: Option<SyncCategory>, reason: impl Into<String>, conflict: bool) {
        self.skipped.push(SyncSkip {
            rel_path: e.rel_path.clone(),
            is_dir: e.is_dir,
            reason: reason.into(),
            conflict,
            category,
            on_left: e.left.is_some(),
            on_right: e.right.is_some(),
        });
    }

    fn copy(&mut self, e: &FolderEntry, dir: Direction, cat: SyncCategory, reason: &str) {
        let size = source_meta(e, dir).map_or(0, |m| m.size);
        self.push(OpKind::Copy, dir, e, size, reason, Some(cat))
    }

    /// Recreates an orphan subtree on the other side: one mkdir per folder
    /// (parents before children) and one copy per file. Items inside with
    /// an override of their own follow it instead.
    fn copy_orphan(&mut self, e: &FolderEntry, dir: Direction, cat: SyncCategory, reason: &str) {
        if !e.is_dir {
            return self.copy(e, dir, cat, reason);
        }
        let from = match dir {
            Direction::ToRight => Side::Left,
            Direction::ToLeft => Side::Right,
        };
        if self.skip_empty_folders && count_files(&e.children, from) == 0 {
            return self.skip(e, Some(cat), "empty folder; not created", false);
        }
        self.push(OpKind::Mkdir, dir, e, 0, reason, Some(cat));
        self.inside(e, |p| {
            for c in &e.children {
                match p.override_of(c) {
                    Some(a) => p.act(c, cat, a, reason),
                    None => p.copy_orphan(c, dir, cat, reason),
                }
            }
        });
    }

    fn delete(&mut self, e: &FolderEntry, dir: Direction, cat: Option<SyncCategory>, reason: &str) {
        // A folder is deleted as a whole, so its children need no ops.
        self.push(OpKind::Delete, dir, e, tree_size(e), reason, cat)
    }

    /// Carries out `action` on an item of category `cat`, whose state is
    /// described by `why` ("newer on left", "deleted on right", ...).
    fn act(&mut self, e: &FolderEntry, cat: SyncCategory, action: SyncAction, why: &str) {
        use SyncAction::*;
        let (l, r) = (e.left.is_some(), e.right.is_some());
        if !action.applies(l, r) {
            let what = match action {
                CopyLeftToRight => "no left copy to copy",
                CopyRightToLeft => "no right copy to copy",
                DeleteLeft => "nothing on the left to delete",
                _ => "nothing on the right to delete",
            };
            return self.skip(e, Some(cat), format!("{why}; {what}"), false);
        }
        match action {
            // Identical items aren't worth a row.
            Leave if cat == SyncCategory::Same => {}
            Leave => {
                let reason = match cat {
                    SyncCategory::LeftNewer | SyncCategory::RightNewer | SyncCategory::Different => {
                        format!("{why}; not overwritten")
                    }
                    _ => format!("{why}; left alone"),
                };
                self.skip(e, Some(cat), reason, false)
            }
            Prompt => {
                let reason = match cat {
                    SyncCategory::Different => "differs, but neither side is newer".to_owned(),
                    _ => format!("{why}: choose what to do"),
                };
                self.skip(e, Some(cat), reason, true)
            }
            CopyLeftToRight | CopyRightToLeft => {
                let dir = if action == CopyLeftToRight { Direction::ToRight } else { Direction::ToLeft };
                let reason = copy_reason(cat, why, dir);
                if l && r {
                    self.copy(e, dir, cat, &reason)
                } else {
                    self.copy_orphan(e, dir, cat, &reason)
                }
            }
            DeleteLeft => self.delete(e, Direction::ToLeft, Some(cat), why),
            DeleteRight => self.delete(e, Direction::ToRight, Some(cat), why),
        }
    }

    /// Plans `entries`. `inherited` is a folder override applying to its
    /// contents; `in_scope` is false above the items a scope names.
    fn walk(&mut self, entries: &[FolderEntry], inherited: Option<SyncAction>, in_scope: bool) {
        for e in entries {
            if in_scope {
                self.visit(e, inherited);
                continue;
            }
            let Some(scope) = &self.scope else { continue };
            if scope.paths.contains(&e.rel_path) {
                self.visit(e, inherited);
            } else if e.is_dir && scope.ancestors.contains(&e.rel_path) {
                self.inside(e, |p| p.walk(&e.children, inherited, false));
            }
        }
    }

    fn visit(&mut self, e: &FolderEntry, inherited: Option<SyncAction>) {
        use SyncAction::*;
        let chosen = self.override_of(e);
        match e.status {
            EntryStatus::Same | EntryStatus::Similar | EntryStatus::Different if e.is_dir => match chosen {
                Some(a @ (DeleteLeft | DeleteRight)) => self.act(e, SyncCategory::Different, a, "folder"),
                Some(a) => self.inside(e, |p| p.walk(&e.children, Some(a), true)),
                None => self.inside(e, |p| p.walk(&e.children, inherited, true)),
            },
            // Rules-based "similar" files differ only in unimportant ways.
            // A folder's action never reaches identical files inside it.
            EntryStatus::Same | EntryStatus::Similar => {
                let a = chosen.unwrap_or(self.matrix.same);
                self.act(e, SyncCategory::Same, a, "same")
            }
            EntryStatus::Different => self.changed(e, chosen.or(inherited)),
            EntryStatus::LeftOnly => self.orphan(e, Side::Left, chosen, inherited),
            EntryStatus::RightOnly => self.orphan(e, Side::Right, chosen, inherited),
            EntryStatus::TypeMismatch => match chosen {
                Some(DeleteLeft) => self.delete(e, Direction::ToLeft, None, "type clash"),
                Some(DeleteRight) => self.delete(e, Direction::ToRight, None, "type clash"),
                Some(Leave) => self.skip(e, None, "a file on one side and a folder on the other; left alone", false),
                Some(_) => self.skip(
                    e,
                    None,
                    "a file on one side and a folder on the other; delete one side first",
                    true,
                ),
                None => self.skip(e, None, "a file on one side and a folder on the other", true),
            },
            EntryStatus::Error => {
                let msg = e.error.as_deref().unwrap_or("cannot be compared");
                let left_alone = chosen == Some(Leave);
                let suffix = if left_alone { "; left alone" } else { "" };
                self.skip(e, None, format!("error: {msg}{suffix}"), !left_alone)
            }
        }
    }

    /// Whether each side changed since the baseline, for a file on both sides
    /// that the baseline knows.
    fn changes_since_baseline(&self, e: &FolderEntry) -> Option<(bool, bool)> {
        let (Some(l), Some(r)) = (&e.left, &e.right) else {
            return None;
        };
        let base = self.baseline?.get(&e.rel_path).filter(|b| !b.is_dir)?;
        Some((
            baseline::side_changed(l, &base.left, self.opts),
            baseline::side_changed(r, &base.right, self.opts),
        ))
    }

    /// A file present on both sides with different contents. With a
    /// baseline, the side that changed since the last sync counts as newer.
    fn changed(&mut self, e: &FolderEntry, forced: Option<SyncAction>) {
        let (cat, why) = match self.changes_since_baseline(e) {
            Some((true, false)) => (SyncCategory::LeftNewer, "changed on left"),
            Some((false, true)) => (SyncCategory::RightNewer, "changed on right"),
            Some(_) => return self.changed_on_both(e, forced),
            // Never synced before (or no baseline): newer wins.
            None => match e.newer {
                Some(Side::Left) => (SyncCategory::LeftNewer, "newer on left"),
                Some(Side::Right) => (SyncCategory::RightNewer, "newer on right"),
                None => (SyncCategory::Different, "different"),
            },
        };
        let action = forced.unwrap_or(self.matrix.get(cat));
        self.act(e, cat, action, why)
    }

    /// Changed on both sides since the last sync: the conflict policy
    /// decides, unless the user already did.
    fn changed_on_both(&mut self, e: &FolderEntry, forced: Option<SyncAction>) {
        let why = "changed on both sides since the last sync";
        if let Some(a) = forced {
            return self.act(e, SyncCategory::Different, a, why);
        }
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
                SyncCategory::Different,
                &format!("conflict: {} version kept", side_name(side)),
            ),
            None => self.skip(e, None, why, true),
        }
    }

    /// An item that exists only on `side`. `chosen` is its own override,
    /// `inherited` its folder's.
    fn orphan(&mut self, e: &FolderEntry, side: Side, chosen: Option<SyncAction>, inherited: Option<SyncAction>) {
        let known = self.baseline.and_then(|b| b.get(&e.rel_path)).is_some();
        let deleted_on = side_name(other(side));
        let (cat, why) = if !known {
            let why = match self.baseline {
                Some(_) => format!("new on {}", side_name(side)),
                None => format!("{} only", side_name(side)),
            };
            (orphan_category(side, false), why)
        } else if self.unchanged_since_baseline(e, side) {
            // It was in sync last time, so the other side deleted it.
            (orphan_category(side, true), format!("deleted on {deleted_on}"))
        } else {
            return self.edited_and_deleted(e, side, chosen.or(inherited));
        };
        let action = match (chosen, inherited) {
            (Some(a), _) => a,
            // A folder's action applies where it can; elsewhere it leaves alone.
            (None, Some(a)) if cat.allows(a) => a,
            (None, Some(_)) => SyncAction::Leave,
            (None, None) => self.matrix.get(cat),
        };
        self.act(e, cat, action, &why)
    }

    /// Changed on `side` since the last sync but deleted on the other.
    fn edited_and_deleted(&mut self, e: &FolderEntry, side: Side, forced: Option<SyncAction>) {
        let cat = orphan_category(side, true);
        let to_other = toward(other(side));
        let why = format!("changed on {}, deleted on {}", side_name(side), side_name(other(side)));
        if let Some(a) = forced {
            let a = if cat.allows(a) { a } else { SyncAction::Leave };
            return self.act(e, cat, a, &why);
        }
        let keep_edit = match self.conflicts {
            ConflictPolicy::Skip => None,
            // An edit beats a deletion: losing work is worse than resurrecting a file.
            ConflictPolicy::NewerWins => Some(true),
            ConflictPolicy::LeftWins => Some(side == Side::Left),
            ConflictPolicy::RightWins => Some(side == Side::Right),
        };
        match keep_edit {
            Some(true) => self.copy_orphan(e, to_other, cat, "conflict: edit kept over deletion"),
            Some(false) => self.delete(e, toward(side), Some(cat), "conflict: deletion kept over edit"),
            None => self.skip(e, Some(cat), why, true),
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
    let baseline = plan_opts.baseline.filter(|b| mode.uses_baseline() && !b.is_empty());
    let matrix = SyncMatrix::for_mode(mode, &plan_opts.matrix);
    let mut p = Planner {
        mkdirs: Vec::new(),
        copies: Vec::new(),
        deletes: Vec::new(),
        skipped: Vec::new(),
        opts,
        baseline,
        conflicts: plan_opts.conflicts,
        matrix,
        overrides: plan_opts.overrides.filter(|o| !o.is_empty()),
        scope: plan_opts.scope.map(Scope::new),
        skip_empty_folders: plan_opts.skip_empty_folders,
        left_dirs: Vec::new(),
    };
    let everything = p.scope.is_none();
    p.walk(&cmp.entries, None, everything);
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
        matrix,
        ops,
        skipped: p.skipped,
        summary,
        warnings,
        has_baseline: baseline.is_some(),
        comparison: None,
        elapsed_ms: cmp.elapsed_ms,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::baseline;
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
        // Left-alone items are listed (not conflicts) so the preview can override them.
        let skipped: Vec<_> = p.skipped.iter().map(|s| (s.rel_path.as_str(), s.reason.as_str())).collect();
        assert_eq!(
            skipped,
            [
                ("ronly-dir", "right only; left alone"),
                ("newer-right.txt", "newer on right; not overwritten"),
                ("ronly.txt", "right only; left alone"),
            ]
        );
        assert!(p.skipped.iter().all(|s| !s.conflict));
        assert_eq!((p.skipped[0].on_left, p.skipped[0].on_right), (false, true));

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
            left_rel_path: None,
            category: None,
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
            left_rel_path: None,
            category: None,
        }];
        assert_eq!(run(&t, &ops).succeeded, 1);
    }

    /// Names in a folder, sorted, exactly as the file system returns them.
    fn names(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn aligned_pairs_sync_under_each_sides_own_names() {
        // bc5:file-ops — the left uses NFC and `.htm`, the right NFD and
        // `.html`; alignment pairs them, and the sync must write to the name
        // each side already has instead of creating look-alike duplicates.
        let dir = tempfile::tempdir().unwrap();
        let (left, right) = (dir.path().join("L"), dir.path().join("R"));
        let (nfc, nfd) = ("caf\u{e9}", "cafe\u{301}");
        write(&left, &format!("{nfc}/menu.txt"), "new menu", NEW);
        write(&right, &format!("{nfd}/menu.txt"), "old", OLD);
        write(&left, &format!("{nfc}/left-only.txt"), "l", OLD);
        write(&right, &format!("{nfd}/right-only.txt"), "r", OLD);
        write(&left, "index.htm", "page v2", NEW);
        write(&right, "index.html", "page v1", OLD);
        let opts = FolderCompareOptions {
            align: folder::AlignOptions {
                extension_groups: vec!["htm,html".into()],
                ..Default::default()
            },
            ..opts()
        };
        let p = plan_sync(&left, &right, SyncMode::UpdateBoth, &opts).unwrap();
        let menu = p.ops.iter().find(|o| o.rel_path.ends_with("menu.txt")).unwrap();
        assert_eq!(menu.rel_path, format!("{nfd}/menu.txt"));
        assert_eq!(menu.src_rel(), format!("{nfc}/menu.txt"));
        let back = p.ops.iter().find(|o| o.rel_path.ends_with("right-only.txt")).unwrap();
        assert_eq!(back.direction, Direction::ToLeft);
        assert_eq!(back.dst_rel(), format!("{nfc}/right-only.txt"), "lands in the paired left folder");
        let page = p.ops.iter().find(|o| o.rel_path == "index.html").unwrap();
        assert_eq!(page.src_rel(), "index.htm");

        let r = exec::execute(
            &left,
            &right,
            &p.ops,
            &ExecOptions::default(),
            &PermanentDeleter,
            &mut NullJournal,
            &AtomicBool::new(false),
            &mut |_| {},
        );
        assert_eq!(r.failed, 0, "{:?}", r.results);
        assert_eq!(names(&right), [nfd.to_string(), "index.html".into()]);
        assert_eq!(names(&left), [nfc.to_string(), "index.htm".into()]);
        assert_eq!(fs::read_to_string(right.join(nfd).join("menu.txt")).unwrap(), "new menu");
        assert_eq!(fs::read_to_string(right.join("index.html")).unwrap(), "page v2");
        assert_eq!(fs::read_to_string(right.join(nfd).join("left-only.txt")).unwrap(), "l");
        assert_eq!(fs::read_to_string(left.join(nfc).join("right-only.txt")).unwrap(), "r");
        let again = plan_sync(&left, &right, SyncMode::UpdateBoth, &opts).unwrap();
        assert!(again.ops.is_empty(), "{:?}", describe(&again));
    }

    // ---------- bc5:sync-matrix ----------

    /// One item per category, with a baseline so deletions are known:
    /// ln/rn changed on one side, lo/ro new, ld/rd deleted on one side,
    /// df different with equal times (never synced), sm the same.
    fn categories() -> (Trees, Baseline) {
        let dir = tempfile::tempdir().unwrap();
        let (left, right) = (dir.path().join("L"), dir.path().join("R"));
        for root in [&left, &right] {
            for f in ["ln.txt", "rn.txt", "ld.txt", "rd.txt", "sm.txt"] {
                write(root, f, "v1", OLD);
            }
        }
        let cmp = folder::compare_folders(&left, &right, &opts()).unwrap();
        let base = baseline::update_baseline(&Baseline::default(), &cmp.entries);
        write(&left, "ln.txt", "v2 left", NEW);
        write(&right, "rn.txt", "v2 right", NEW);
        write(&left, "lo.txt", "L", OLD);
        write(&right, "ro.txt", "R", OLD);
        fs::remove_file(left.join("ld.txt")).unwrap();
        fs::remove_file(right.join("rd.txt")).unwrap();
        write(&left, "df.txt", "left", OLD);
        write(&right, "df.txt", "right!", OLD);
        (Trees { _dir: dir, left, right }, base)
    }

    const CATEGORY_FILES: [(SyncCategory, &str); 8] = [
        (SyncCategory::LeftNewer, "ln.txt"),
        (SyncCategory::RightNewer, "rn.txt"),
        (SyncCategory::LeftOrphan, "lo.txt"),
        (SyncCategory::RightOrphan, "ro.txt"),
        (SyncCategory::LeftDeleted, "ld.txt"),
        (SyncCategory::RightDeleted, "rd.txt"),
        (SyncCategory::Different, "df.txt"),
        (SyncCategory::Same, "sm.txt"),
    ];

    fn custom(t: &Trees, base: &Baseline, matrix: SyncMatrix, rules: &PlanRules) -> SyncPlan {
        let po = PlanOptions {
            matrix,
            ..rules.options(Some(base), ConflictPolicy::Skip)
        };
        plan_sync_local(&t.left, &t.right, SyncMode::Custom, &opts(), &po).unwrap()
    }

    /// What the plan does with one path: an op, a skip, or nothing.
    fn outcome(p: &SyncPlan, rel: &str) -> String {
        let ops: Vec<String> = p
            .ops
            .iter()
            .filter(|o| o.rel_path == rel)
            .map(|o| {
                let d = if o.direction == Direction::ToRight { ">" } else { "<" };
                format!("{:?}{d}", o.kind)
            })
            .collect();
        let skips: Vec<String> = p
            .skipped
            .iter()
            .filter(|s| s.rel_path == rel)
            .map(|s| if s.conflict { "Prompt".to_owned() } else { "Leave".to_owned() })
            .collect();
        [ops, skips].concat().join(",")
    }

    #[test]
    fn every_category_takes_every_action() {
        use SyncAction::*;
        let (t, base) = categories();
        for (cat, file) in CATEGORY_FILES {
            for action in SyncAction::ALL {
                let mut matrix = SyncMatrix::default();
                matrix.set(cat, action);
                let p = custom(&t, &base, matrix, &PlanRules::default());
                assert!(p.has_baseline);
                let got = outcome(&p, file);
                let (l, r) = cat.sides();
                let want = match action {
                    _ if !cat.allows(action) => "Leave",
                    Leave if cat == SyncCategory::Same => "",
                    Leave => "Leave",
                    Prompt => "Prompt",
                    CopyLeftToRight => "Copy>",
                    CopyRightToLeft => "Copy<",
                    DeleteLeft => "Delete<",
                    DeleteRight => "Delete>",
                };
                assert_eq!(got, want, "{cat:?} × {action:?}");
                // Rows carry their category, and skips the sides present.
                if let Some(op) = p.ops.iter().find(|o| o.rel_path == file) {
                    assert_eq!(op.category, Some(cat), "{cat:?} × {action:?}");
                    assert_eq!(op.overwrite, op.kind == OpKind::Copy && l && r);
                }
                if let Some(s) = p.skipped.iter().find(|s| s.rel_path == file) {
                    assert_eq!((s.category, s.on_left, s.on_right), (Some(cat), l, r), "{cat:?} × {action:?}");
                    if !cat.allows(action) {
                        assert!(s.reason.contains("no ") || s.reason.contains("nothing"), "{}", s.reason);
                    }
                }
                // No other category is touched: everything else is set to leave.
                assert!(
                    p.ops.iter().all(|o| o.rel_path == file),
                    "{cat:?} × {action:?}: {:?}",
                    describe(&p)
                );
            }
        }
    }

    #[test]
    fn a_custom_matrix_runs_and_settles() {
        use SyncAction::*;
        let (t, mut base) = categories();
        // "Update right, restore deletions, and drop new right-side files."
        let matrix = SyncMatrix {
            left_newer: CopyLeftToRight,
            right_newer: Leave,
            left_orphan: CopyLeftToRight,
            right_orphan: DeleteRight,
            left_deleted: CopyRightToLeft,
            right_deleted: CopyLeftToRight,
            different: Prompt,
            same: Leave,
        };
        let p = custom(&t, &base, matrix, &PlanRules::default());
        assert_eq!(
            describe(&p),
            ["Copy<ld.txt", "Copy>ln.txt", "Copy>lo.txt", "Copy>rd.txt", "Delete>ro.txt"]
        );
        assert_eq!(p.matrix, matrix);
        let conflicts: Vec<_> = p.skipped.iter().filter(|s| s.conflict).map(|s| s.rel_path.as_str()).collect();
        assert_eq!(conflicts, ["df.txt"], "prompt stays unresolved");
        let r = run(&t, &p.ops);
        assert_eq!(r.failed, 0, "{:?}", r.results);
        assert_eq!(fs::read_to_string(t.left.join("ld.txt")).unwrap(), "v1", "restored");
        assert!(!t.right.join("ro.txt").exists());
        assert_eq!(fs::read_to_string(t.right.join("rn.txt")).unwrap(), "v2 right");

        let cmp = folder::compare_folders(&t.left, &t.right, &opts()).unwrap();
        base = baseline::update_baseline(&base, &cmp.entries);
        let again = custom(&t, &base, matrix, &PlanRules::default());
        assert!(again.ops.is_empty(), "{:?}", describe(&again));
    }

    #[test]
    fn presets_plan_like_their_matrix() {
        let (t, base) = categories();
        for mode in SyncMode::PRESETS {
            let po = PlanOptions { baseline: Some(&base), ..Default::default() };
            let fixed = plan_sync_local(&t.left, &t.right, mode, &opts(), &po).unwrap();
            let rules = PlanRules::default();
            let as_custom = PlanOptions {
                matrix: SyncMatrix::preset(mode),
                // Only two-way consults the baseline among the presets.
                baseline: mode.uses_baseline().then_some(&base),
                ..rules.options(None, ConflictPolicy::Skip)
            };
            let custom = plan_sync_local(&t.left, &t.right, SyncMode::Custom, &opts(), &as_custom).unwrap();
            assert_eq!(describe(&fixed), describe(&custom), "{mode:?}");
            assert_eq!(fixed.matrix, SyncMatrix::preset(mode));
        }
    }

    #[test]
    fn row_overrides_win_over_the_matrix_and_conflicts() {
        use SyncAction::*;
        let t = trees();
        write(&t.left, "tie.txt", "aaaa", OLD);
        write(&t.right, "tie.txt", "bbbbbb", OLD);
        write(&t.left, "clash", "file", OLD);
        write(&t.right, "clash/inner.txt", "dir", OLD);
        let rules = PlanRules {
            overrides: HashMap::from([
                ("tie.txt".to_owned(), CopyRightToLeft),       // a conflict row
                ("newer-right.txt".to_owned(), CopyLeftToRight), // a skipped row: overwrite the newer file
                ("ronly.txt".to_owned(), DeleteRight),          // a left-alone orphan
                ("newer-left.txt".to_owned(), Leave),           // an op row
                ("clash".to_owned(), DeleteRight),              // type clash: the right folder goes
                ("lonly-dir/a.txt".to_owned(), Leave),          // inside a copied orphan folder
                ("ronly-dir".to_owned(), CopyLeftToRight),      // can't apply: nothing on the left
            ]),
            ..Default::default()
        };
        let po = rules.options(None, ConflictPolicy::Skip);
        let p = plan_sync_local(&t.left, &t.right, SyncMode::UpdateLeftToRight, &opts(), &po).unwrap();
        assert_eq!(outcome(&p, "tie.txt"), "Copy<");
        assert_eq!(outcome(&p, "newer-right.txt"), "Copy>");
        assert_eq!(outcome(&p, "ronly.txt"), "Delete>");
        assert_eq!(outcome(&p, "newer-left.txt"), "Leave");
        assert_eq!(outcome(&p, "lonly-dir"), "Mkdir>");
        assert_eq!(outcome(&p, "lonly-dir/a.txt"), "Leave");
        assert_eq!(outcome(&p, "lonly-dir/sub/b.txt"), "Copy>", "siblings keep the folder's copy");
        assert_eq!(outcome(&p, "ronly-dir"), "Leave");
        let clash = p.ops.iter().find(|o| o.rel_path == "clash").unwrap();
        assert_eq!((clash.kind, clash.direction, clash.is_dir), (OpKind::Delete, Direction::ToRight, true));
        assert_eq!(p.summary.conflicts, 0);

        let r = run(&t, &p.ops);
        assert_eq!(r.failed, 0, "{:?}", r.results);
        assert_eq!(fs::read_to_string(t.left.join("tie.txt")).unwrap(), "bbbbbb");
        assert_eq!(fs::read_to_string(t.right.join("newer-right.txt")).unwrap(), "left v1");
        assert!(!t.right.join("clash").exists() && t.left.join("clash").is_file());
        assert!(!t.right.join("lonly-dir/a.txt").exists());
    }

    #[test]
    fn folder_overrides_reach_their_contents() {
        use SyncAction::*;
        let t = trees();
        for (side, body, secs) in [(&t.left, "left", OLD), (&t.right, "right", NEW)] {
            write(side, "shared/same.txt", "same", OLD);
            write(side, "shared/diff.txt", body, secs);
        }
        write(&t.left, "shared/lnew.txt", "l", OLD);
        write(&t.right, "shared/rnew.txt", "r", OLD);
        let plan_with = |action| {
            let rules = PlanRules {
                overrides: HashMap::from([("shared".to_owned(), action)]),
                ..Default::default()
            };
            let po = rules.options(None, ConflictPolicy::Skip);
            let p = plan_sync_local(&t.left, &t.right, SyncMode::UpdateBoth, &opts(), &po).unwrap();
            let mut inside: Vec<String> = ["shared", "shared/same.txt", "shared/diff.txt", "shared/lnew.txt", "shared/rnew.txt"]
                .iter()
                .map(|f| format!("{f}={}", outcome(&p, f)))
                .collect();
            inside.retain(|x| !x.ends_with('='));
            inside
        };
        // Copy L→R overwrites the newer right file too, but never touches
        // identical files or items only the right side has.
        assert_eq!(
            plan_with(CopyLeftToRight),
            ["shared/diff.txt=Copy>", "shared/lnew.txt=Copy>", "shared/rnew.txt=Leave"]
        );
        assert_eq!(
            plan_with(Leave),
            ["shared/diff.txt=Leave", "shared/lnew.txt=Leave", "shared/rnew.txt=Leave"]
        );
        assert_eq!(plan_with(DeleteLeft), ["shared=Delete<"]);
    }

    #[test]
    fn scope_limits_the_plan_to_the_selection() {
        let t = trees();
        let scope = vec!["lonly-dir/sub".to_owned(), "newer-right.txt".to_owned(), "ronly.txt".to_owned()];
        let rules = PlanRules { scope: Some(scope), ..Default::default() };
        let p = plan_sync_local(&t.left, &t.right, SyncMode::MirrorLeftToRight, &opts(), &rules.options(None, ConflictPolicy::Skip)).unwrap();
        assert_eq!(
            describe(&p),
            ["Mkdir>lonly-dir/sub", "Copy>lonly-dir/sub/b.txt", "Copy>newer-right.txt", "Delete>ronly.txt"]
        );
        let r = run(&t, &p.ops);
        assert_eq!(r.failed, 0, "{:?}", r.results);
        assert!(t.right.join("lonly-dir/sub/b.txt").is_file());
        assert!(!t.right.join("lonly-dir/a.txt").exists(), "outside the selection");
        assert!(t.right.join("ronly-dir").exists(), "outside the selection");
    }

    #[test]
    fn empty_folders_can_be_left_uncreated() {
        let t = trees();
        fs::create_dir_all(t.left.join("empty/inner")).unwrap();
        fs::create_dir_all(t.left.join("lonly-dir/hollow")).unwrap();
        let both = plan(&t, SyncMode::UpdateLeftToRight);
        assert!(describe(&both).contains(&"Mkdir>empty/inner".to_owned()));
        let rules = PlanRules { skip_empty_folders: true, ..Default::default() };
        let p = plan_sync_local(&t.left, &t.right, SyncMode::UpdateLeftToRight, &opts(), &rules.options(None, ConflictPolicy::Skip)).unwrap();
        let d = describe(&p);
        assert!(!d.iter().any(|x| x.contains("empty") || x.contains("hollow")), "{d:?}");
        assert!(d.contains(&"Mkdir>lonly-dir".to_owned()), "folders with files are still created");
        let skip: Vec<_> = p.skipped.iter().filter(|s| s.reason.starts_with("empty folder")).map(|s| s.rel_path.as_str()).collect();
        assert_eq!(skip, ["empty", "lonly-dir/hollow"]);
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
