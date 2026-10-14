//! Beyond Compare's finer row statuses and per-folder status counts.
//!
//! [`EntryStatus`] says what a row *is*; [`StatusDetail`] adds what Beyond
//! Compare's `TDirCompareStatus` adds on top: which side of a differing pair
//! is newer, "same contents but the quick tests differ", and "not compared
//! yet". Folders carry [`StatusCounts`] of everything below them (BC's
//! status counts), so a folder can say "orphans *and* right-newer files" at
//! once. Counts are sums of each row's contribution, so a change to one row
//! updates its ancestors by taking the old contribution out and putting the
//! new one in ([`replace_entry`], BC's `UpdateParentStatus`): no rescan and
//! no walk over siblings.

use super::{tally, EntryStatus, FolderCompareResult, FolderEntry, FolderStats, Side};
use serde::Serialize;

/// A row's status at Beyond Compare's granularity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum StatusDetail {
    /// Listed but not compared: a streamed pair awaiting its verdict, or a
    /// folder pair a non-recursive comparison didn't open.
    Unknown,
    Same,
    /// The contents are the same but a quick test (size or timestamp)
    /// differs, e.g. a file that was touched or copied without its date.
    SameDiff,
    Similar,
    /// Different, and the left file is newer.
    LeftNewer,
    /// Different, and the right file is newer.
    RightNewer,
    /// Different with no newer side (timestamps equal or unknown), or a
    /// folder with differences below it.
    Different,
    LeftOnly,
    RightOnly,
    TypeMismatch,
    Error,
}

impl StatusDetail {
    pub const ALL: [Self; 11] = [
        Self::Unknown,
        Self::Same,
        Self::SameDiff,
        Self::Similar,
        Self::LeftNewer,
        Self::RightNewer,
        Self::Different,
        Self::LeftOnly,
        Self::RightOnly,
        Self::TypeMismatch,
        Self::Error,
    ];

    /// A file pair's detail. `quick_differs`: the size or timestamp test
    /// fails, whatever the content comparison concluded.
    pub fn for_file(status: EntryStatus, newer: Option<Side>, quick_differs: bool) -> Self {
        match (status, newer) {
            (EntryStatus::Same, _) if quick_differs => Self::SameDiff,
            (EntryStatus::Different, Some(Side::Left)) => Self::LeftNewer,
            (EntryStatus::Different, Some(Side::Right)) => Self::RightNewer,
            _ => status.into(),
        }
    }

    /// Counts as "same" when a folder's status is worked out. Unknown does,
    /// as the folder pair a non-recursive run didn't open always has.
    pub fn is_same(self) -> bool {
        matches!(self, Self::Unknown | Self::Same | Self::SameDiff)
    }

    fn bit(self) -> u16 {
        1 << self as u16
    }
}

impl From<EntryStatus> for StatusDetail {
    fn from(s: EntryStatus) -> Self {
        match s {
            EntryStatus::Same => Self::Same,
            EntryStatus::Similar => Self::Similar,
            EntryStatus::Different => Self::Different,
            EntryStatus::LeftOnly => Self::LeftOnly,
            EntryStatus::RightOnly => Self::RightOnly,
            EntryStatus::TypeMismatch => Self::TypeMismatch,
            EntryStatus::Error => Self::Error,
        }
    }
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

/// How many rows below a folder have each [`StatusDetail`]. Paired folders
/// don't count themselves (their status comes from these counts); files,
/// orphaned folders and folders that couldn't be compared do. Zero counts
/// are left out of the JSON.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusCounts {
    #[serde(skip_serializing_if = "is_zero")]
    pub unknown: usize,
    #[serde(skip_serializing_if = "is_zero")]
    pub same: usize,
    #[serde(skip_serializing_if = "is_zero")]
    pub same_diff: usize,
    #[serde(skip_serializing_if = "is_zero")]
    pub similar: usize,
    #[serde(skip_serializing_if = "is_zero")]
    pub left_newer: usize,
    #[serde(skip_serializing_if = "is_zero")]
    pub right_newer: usize,
    #[serde(skip_serializing_if = "is_zero")]
    pub different: usize,
    #[serde(skip_serializing_if = "is_zero")]
    pub left_only: usize,
    #[serde(skip_serializing_if = "is_zero")]
    pub right_only: usize,
    #[serde(skip_serializing_if = "is_zero")]
    pub type_mismatch: usize,
    #[serde(skip_serializing_if = "is_zero")]
    pub error: usize,
}

impl StatusCounts {
    fn slot(&mut self, d: StatusDetail) -> &mut usize {
        match d {
            StatusDetail::Unknown => &mut self.unknown,
            StatusDetail::Same => &mut self.same,
            StatusDetail::SameDiff => &mut self.same_diff,
            StatusDetail::Similar => &mut self.similar,
            StatusDetail::LeftNewer => &mut self.left_newer,
            StatusDetail::RightNewer => &mut self.right_newer,
            StatusDetail::Different => &mut self.different,
            StatusDetail::LeftOnly => &mut self.left_only,
            StatusDetail::RightOnly => &mut self.right_only,
            StatusDetail::TypeMismatch => &mut self.type_mismatch,
            StatusDetail::Error => &mut self.error,
        }
    }

    pub fn get(&self, d: StatusDetail) -> usize {
        let mut c = *self;
        *c.slot(d)
    }

    pub fn add(&mut self, d: StatusDetail, n: usize) {
        *self.slot(d) += n;
    }

    /// Adds every count of `other`.
    pub fn merge(&mut self, other: &Self) {
        for d in StatusDetail::ALL {
            self.add(d, other.get(d));
        }
    }

    /// Takes `other` back out (it must have been merged in before).
    pub fn remove(&mut self, other: &Self) {
        for d in StatusDetail::ALL {
            let n = self.slot(d);
            *n = n.saturating_sub(other.get(d));
        }
    }

    pub fn total(&self) -> usize {
        StatusDetail::ALL.iter().map(|&d| self.get(d)).sum()
    }

    /// The statuses present (BC's `TDirCompareStatusSet`).
    pub fn set(&self) -> StatusSet {
        StatusDetail::ALL
            .into_iter()
            .filter(|&d| self.get(d) > 0)
            .fold(StatusSet::default(), |s, d| s.with(d))
    }

    /// The side every newer file below is on; `None` when there are none or
    /// both sides have some.
    pub fn newer(&self) -> Option<Side> {
        match (self.left_newer > 0, self.right_newer > 0) {
            (true, false) => Some(Side::Left),
            (false, true) => Some(Side::Right),
            _ => None,
        }
    }

    /// The status of a paired folder with these counts; agrees with
    /// [`super::folder_status`] over its children.
    pub fn folder_status(&self) -> EntryStatus {
        let same: usize = StatusDetail::ALL
            .iter()
            .filter(|d| d.is_same())
            .map(|&d| self.get(d))
            .sum();
        let total = self.total();
        if total == same {
            EntryStatus::Same
        } else if total == same + self.similar {
            EntryStatus::Similar
        } else {
            EntryStatus::Different
        }
    }
}

/// A set of [`StatusDetail`]s.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct StatusSet(u16);

impl StatusSet {
    pub fn with(self, d: StatusDetail) -> Self {
        Self(self.0 | d.bit())
    }

    pub fn contains(self, d: StatusDetail) -> bool {
        self.0 & d.bit() != 0
    }

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub fn iter(self) -> impl Iterator<Item = StatusDetail> {
        StatusDetail::ALL
            .into_iter()
            .filter(move |&d| self.contains(d))
    }
}

/// A paired folder whose status comes from its counts.
fn is_derived(e: &FolderEntry) -> bool {
    e.is_dir && e.left.is_some() && e.right.is_some() && e.counts.is_some()
}

/// What `e` adds to its parent's counts: its own detail (unless its status
/// is derived from what's below) plus everything below it. Rows the filters
/// leave out (and so everything below them) add nothing, as in `tally`.
pub fn contribution(e: &FolderEntry) -> StatusCounts {
    if e.filtered_out() {
        return StatusCounts::default();
    }
    let mut c = e.counts.unwrap_or_default();
    if !is_derived(e) {
        c.add(e.detail, 1);
    }
    c
}

/// The counts of a folder with these children.
pub fn aggregate(children: &[FolderEntry]) -> StatusCounts {
    children.iter().fold(StatusCounts::default(), |mut c, e| {
        c.merge(&contribution(e));
        c
    })
}

/// What [`replace_entry`] changed.
#[derive(Debug, Clone)]
pub struct Replaced {
    /// The row that was there (with everything below it), if any.
    pub old: Option<FolderEntry>,
    /// The top-level row's contribution before and after: the change to the
    /// totals over the whole tree.
    pub before: StatusCounts,
    pub after: StatusCounts,
}

/// Replaces the row at `rel_path` (with everything below it) by `new`, or
/// removes it, and brings every folder above it up to date: counts, status
/// and detail, from the change alone. A path that isn't there yet is
/// inserted into its parent folder in display order. `None` when neither
/// the row nor its parent folder is in the tree.
pub fn replace_entry(
    entries: &mut Vec<FolderEntry>,
    rel_path: &str,
    new: Option<FolderEntry>,
) -> Option<Replaced> {
    replace_in(entries, "", rel_path, &mut { new })
}

fn replace_in(
    list: &mut Vec<FolderEntry>,
    list_rel: &str,
    rel: &str,
    new: &mut Option<FolderEntry>,
) -> Option<Replaced> {
    if let Some(i) = list.iter().position(|e| e.rel_path == rel) {
        let before = contribution(&list[i]);
        let (old, after) = match new.take() {
            Some(n) => {
                let after = contribution(&n);
                (std::mem::replace(&mut list[i], n), after)
            }
            None => (list.remove(i), StatusCounts::default()),
        };
        return Some(Replaced {
            old: Some(old),
            before,
            after,
        });
    }
    let below = |e: &FolderEntry| {
        e.is_dir
            && rel
                .strip_prefix(e.rel_path.as_str())
                .is_some_and(|rest| rest.starts_with('/'))
    };
    if let Some(dir) = list.iter_mut().find(|e| below(e)) {
        let before = contribution(dir);
        let dir_rel = dir.rel_path.clone();
        let change = replace_in(&mut dir.children, &dir_rel, rel, new)?;
        match &mut dir.counts {
            Some(c) => {
                c.remove(&change.before);
                c.merge(&change.after);
            }
            None => dir.counts = Some(aggregate(&dir.children)),
        }
        if is_derived(dir) && dir.error.is_none() {
            dir.status = dir.counts.unwrap_or_default().folder_status();
            dir.detail = dir.status.into();
        }
        let after = contribution(dir);
        return Some(Replaced {
            old: change.old,
            before,
            after,
        });
    }
    let parent = rel.rsplit_once('/').map_or("", |(p, _)| p);
    let n = new.take().filter(|_| parent == list_rel)?;
    let after = contribution(&n);
    // The order compare_dirs sorts rows in: folders first, then by name.
    let key = |e: &FolderEntry| (!e.is_dir, e.name.to_lowercase());
    let at = list.partition_point(|e| key(e) < key(&n));
    list.insert(at, n);
    Some(Replaced {
        old: None,
        before: StatusCounts::default(),
        after,
    })
}

impl FolderStats {
    /// Takes `minus` out and puts `plus` in (file counts, errors, filtered).
    fn adjust(&mut self, minus: &FolderStats, plus: &FolderStats) {
        macro_rules! apply {
            ($($f:ident),*) => {$(
                self.$f = self.$f.saturating_sub(minus.$f) + plus.$f;
            )*};
        }
        apply!(same, similar, different, left_only, right_only, errors, filtered);
        self.counts.remove(&minus.counts);
        self.counts.merge(&plus.counts);
    }
}

fn stats_of(e: Option<&FolderEntry>) -> FolderStats {
    let mut s = FolderStats::default();
    if let Some(e) = e {
        tally(std::slice::from_ref(e), &mut s);
        s.counts = contribution(e);
    }
    s
}

impl FolderCompareResult {
    /// [`replace_entry`] on this result, keeping [`Self::stats`] right.
    /// Use it after a file operation: compare just the affected pair, then
    /// swap its row in instead of rescanning both folders. Returns false
    /// when the path's parent folder isn't in the tree.
    pub fn replace_entry(&mut self, rel_path: &str, new: Option<FolderEntry>) -> bool {
        let added = stats_of(new.as_ref());
        let Some(change) = replace_entry(&mut self.entries, rel_path, new) else {
            return false;
        };
        let mut removed = stats_of(change.old.as_ref());
        // Only the top-level row's contribution moves the totals; below the
        // root, ancestors absorbed the difference.
        removed.counts = change.before;
        let mut added = added;
        added.counts = change.after;
        self.stats.adjust(&removed, &added);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::super::{compare_folders, ContentMethod, FolderCompareOptions};
    use super::*;
    use std::fs;
    use std::path::Path;
    use std::time::{Duration, SystemTime};

    fn write(root: &Path, rel: &str, contents: &str) {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, contents).unwrap();
    }

    fn age(root: &Path, rel: &str, secs: u64) {
        let f = fs::File::options()
            .write(true)
            .open(root.join(rel))
            .unwrap();
        f.set_modified(SystemTime::now() - Duration::from_secs(secs))
            .unwrap();
    }

    fn find<'a>(entries: &'a [FolderEntry], rel: &str) -> &'a FolderEntry {
        fn go<'a>(list: &'a [FolderEntry], rel: &str) -> Option<&'a FolderEntry> {
            list.iter().find_map(|e| {
                (e.rel_path == rel)
                    .then_some(e)
                    .or_else(|| go(&e.children, rel))
            })
        }
        go(entries, rel).unwrap_or_else(|| panic!("{rel} missing"))
    }

    /// Everything about a tree the incremental update must keep right.
    fn shape(entries: &[FolderEntry]) -> Vec<String> {
        let mut out = Vec::new();
        fn go(list: &[FolderEntry], out: &mut Vec<String>) {
            for e in list {
                out.push(format!(
                    "{} {:?} {:?} {:?}",
                    e.rel_path, e.status, e.detail, e.counts
                ));
                go(&e.children, out);
            }
        }
        go(entries, &mut out);
        out
    }

    fn opts() -> FolderCompareOptions {
        FolderCompareOptions {
            ignore_name_case: false,
            ..Default::default()
        }
    }

    #[test]
    fn counts_aggregate_and_sets_combine() {
        let dir = tempfile::tempdir().unwrap();
        let (l, r) = (dir.path().join("l"), dir.path().join("r"));
        write(&l, "src/a.txt", "one");
        write(&r, "src/a.txt", "one!");
        age(&l, "src/a.txt", 3600); // right newer
        write(&l, "src/only-left.txt", "x");
        write(&l, "src/gone/deep.txt", "x"); // an orphaned folder with a file
        write(&l, "same/b.txt", "b");
        write(&r, "same/b.txt", "b");
        write(&l, "doc.txt", "d");
        write(&r, "doc.txt", "d");

        let res = compare_folders(&l, &r, &opts()).unwrap();
        let src = find(&res.entries, "src");
        let c = src.counts.expect("paired folders carry counts");
        assert_eq!(
            (c.right_newer, c.left_only, c.total()),
            (1, 3, 4),
            "a.txt, only-left.txt, gone/, gone/deep.txt"
        );
        let set = c.set();
        assert!(set.contains(StatusDetail::RightNewer) && set.contains(StatusDetail::LeftOnly));
        assert!(!set.contains(StatusDetail::Same));
        assert_eq!(c.newer(), Some(Side::Right));
        assert_eq!(src.status, EntryStatus::Different);
        assert_eq!(
            c.folder_status(),
            super::super::folder_status(&src.children)
        );

        // The orphaned folder counts itself and what's inside.
        let gone = find(&res.entries, "src/gone");
        assert_eq!(gone.detail, StatusDetail::LeftOnly);
        assert_eq!(contribution(gone).left_only, 2);

        let same = find(&res.entries, "same");
        assert_eq!(
            (same.detail, same.counts.unwrap().same),
            (StatusDetail::Same, 1)
        );
        assert_eq!(
            find(&res.entries, "src/a.txt").detail,
            StatusDetail::RightNewer
        );
        assert_eq!(
            res.entries
                .iter()
                .find(|e| e.name == "doc.txt")
                .unwrap()
                .detail,
            StatusDetail::Same
        );

        // Root totals: 2 same files, 1 right newer, 3 left-only rows.
        let t = res.stats.counts;
        assert_eq!(
            (t.same, t.right_newer, t.left_only, t.total()),
            (2, 1, 3, 6)
        );
        // Zero counts stay out of the JSON.
        let json = serde_json::to_value(src).unwrap();
        assert_eq!(
            json["counts"],
            serde_json::json!({"rightNewer": 1, "leftOnly": 3})
        );
        assert_eq!(json["detail"], "different");
    }

    #[test]
    fn same_contents_with_other_timestamps_is_same_diff() {
        let dir = tempfile::tempdir().unwrap();
        let (l, r) = (dir.path().join("l"), dir.path().join("r"));
        for side in [&l, &r] {
            write(side, "touched.txt", "same bytes");
            write(side, "plain.txt", "same bytes");
        }
        age(&l, "touched.txt", 7200);
        let res = compare_folders(
            &l,
            &r,
            &FolderCompareOptions {
                content: ContentMethod::Binary,
                ..opts()
            },
        )
        .unwrap();
        let touched = find(&res.entries, "touched.txt");
        assert_eq!(
            (touched.status, touched.detail),
            (EntryStatus::Same, StatusDetail::SameDiff)
        );
        assert_eq!(touched.newer, None, "newer stays for differing files only");
        assert_eq!(find(&res.entries, "plain.txt").detail, StatusDetail::Same);
        assert_eq!(res.stats.same, 2);
        assert_eq!((res.stats.counts.same, res.stats.counts.same_diff), (1, 1));
    }

    #[test]
    fn unopened_folder_pairs_are_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let (l, r) = (dir.path().join("l"), dir.path().join("r"));
        write(&l, "sub/a.txt", "a");
        write(&r, "sub/a.txt", "b");
        let res = compare_folders(
            &l,
            &r,
            &FolderCompareOptions {
                recursive: false,
                ..opts()
            },
        )
        .unwrap();
        let sub = find(&res.entries, "sub");
        assert_eq!(
            (sub.status, sub.detail, sub.counts),
            (EntryStatus::Same, StatusDetail::Unknown, None)
        );
        assert_eq!(res.stats.counts.unknown, 1);
    }

    #[test]
    fn replacing_one_row_matches_a_rescan() {
        let dir = tempfile::tempdir().unwrap();
        let (l, r) = (dir.path().join("l"), dir.path().join("r"));
        write(&l, "a/b/changed.txt", "old");
        write(&r, "a/b/changed.txt", "new!");
        age(&l, "a/b/changed.txt", 3600);
        write(&l, "a/b/orphan.txt", "o");
        write(&l, "a/keep.txt", "k");
        write(&r, "a/keep.txt", "k");
        write(&l, "a/b/left-dir/x.txt", "x");
        let o = opts();
        let mut res = compare_folders(&l, &r, &o).unwrap();

        // Each step: a file operation, then the one affected row from a rescan
        // swapped into the old tree. Ancestors and totals must match the rescan.
        let step = |res: &mut FolderCompareResult, op: &dyn Fn(), rel: &str| {
            op();
            let fresh = compare_folders(&l, &r, &o).unwrap();
            let row = {
                fn go(list: &[FolderEntry], rel: &str) -> Option<FolderEntry> {
                    list.iter().find_map(|e| {
                        if e.rel_path == rel {
                            Some(e.clone())
                        } else {
                            go(&e.children, rel)
                        }
                    })
                }
                go(&fresh.entries, rel)
            };
            assert!(res.replace_entry(rel, row), "{rel}");
            assert_eq!(shape(&res.entries), shape(&fresh.entries), "after {rel}");
            let s = |s: &FolderStats| {
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
            assert_eq!(s(&res.stats), s(&fresh.stats), "stats after {rel}");
        };

        // Copy left → right: the newer file becomes the same.
        step(
            &mut res,
            &|| {
                fs::copy(l.join("a/b/changed.txt"), r.join("a/b/changed.txt")).unwrap();
                age(&r, "a/b/changed.txt", 3600);
            },
            "a/b/changed.txt",
        );
        // Delete an orphan: the row goes away.
        step(
            &mut res,
            &|| fs::remove_file(l.join("a/b/orphan.txt")).unwrap(),
            "a/b/orphan.txt",
        );
        // A new file appears on the right only: inserted in order.
        step(
            &mut res,
            &|| write(&r, "a/b/added.txt", "n"),
            "a/b/added.txt",
        );
        // A whole orphaned folder is copied across: a subtree is replaced.
        step(
            &mut res,
            &|| {
                fs::create_dir_all(r.join("a/b/left-dir")).unwrap();
                fs::copy(l.join("a/b/left-dir/x.txt"), r.join("a/b/left-dir/x.txt")).unwrap();
                let t = fs::metadata(l.join("a/b/left-dir/x.txt"))
                    .unwrap()
                    .modified()
                    .unwrap();
                fs::File::options()
                    .write(true)
                    .open(r.join("a/b/left-dir/x.txt"))
                    .unwrap()
                    .set_modified(t)
                    .unwrap();
            },
            "a/b/left-dir",
        );
        assert_eq!(
            find(&res.entries, "a").status,
            EntryStatus::Different,
            "added.txt is still an orphan"
        );
        step(
            &mut res,
            &|| fs::remove_file(r.join("a/b/added.txt")).unwrap(),
            "a/b/added.txt",
        );
        assert_eq!(find(&res.entries, "a").status, EntryStatus::Same);
        assert_eq!(
            res.stats.counts.set().iter().collect::<Vec<_>>(),
            [StatusDetail::Same]
        );

        assert!(
            !res.replace_entry("nowhere/x.txt", None),
            "no parent folder"
        );

        // A row the filters leave out counts nowhere, like in `tally`.
        let mut out = find(&res.entries, "a/keep.txt").clone();
        let (before, same) = (res.stats.counts, res.stats.same);
        out.filtered = Some(super::super::Filtered::Both);
        assert_eq!(contribution(&out), StatusCounts::default());
        assert!(res.replace_entry("a/keep.txt", Some(out)));
        assert_eq!(res.stats.counts.same, before.same - 1);
        assert_eq!((res.stats.same, res.stats.filtered), (same - 1, 1));
    }
}
