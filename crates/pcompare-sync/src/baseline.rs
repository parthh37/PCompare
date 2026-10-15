//! The sync *baseline*: what every path looked like on both sides the last
//! time the pair was in sync.
//!
//! A plain comparison can't tell "created on the left" from "deleted on the
//! right" — both look like a left-only file. Comparing each side against the
//! baseline can, which is what makes a true two-way sync (deletions
//! propagated, edits on both sides reported as conflicts) possible. The
//! baseline is persisted by the app's database between runs.

use std::collections::HashMap;

use pcompare_core::folder::{self, EntryMeta, EntryStatus, FolderCompareOptions, FolderEntry};
use serde::{Deserialize, Serialize};

/// Size and modification time of one side of a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Snap {
    pub size: u64,
    /// Milliseconds since the Unix epoch.
    pub modified_ms: Option<i64>,
}

impl From<&EntryMeta> for Snap {
    fn from(m: &EntryMeta) -> Self {
        Snap {
            size: m.size,
            modified_ms: m.modified_ms,
        }
    }
}

/// One path as it was when last in sync. Both sides are recorded because
/// they can legitimately differ (timestamp tolerance, "similar" text files).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BaselineEntry {
    pub is_dir: bool,
    pub left: Snap,
    pub right: Snap,
}

/// Relative path (`/`-separated, as in [`FolderEntry::rel_path`]) → state.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Baseline {
    pub entries: HashMap<String, BaselineEntry>,
}

impl Baseline {
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn get(&self, rel_path: &str) -> Option<&BaselineEntry> {
        self.entries.get(rel_path)
    }
}

/// Whether one side of a path changed since the baseline. Folders only count
/// as changed when they appeared; their contents are judged file by file.
pub fn side_changed(now: &EntryMeta, then: &Snap, opts: &FolderCompareOptions) -> bool {
    if now.is_dir {
        return false;
    }
    now.size != then.size || !folder::timestamps_match(now.modified_ms, then.modified_ms, opts)
}

/// Builds the baseline to store after a sync from a fresh comparison.
///
/// Paths that are in sync now take their current state. Paths that still
/// differ (skipped conflicts, failed or unticked operations) keep their old
/// entry, so the next run still knows what changed since the last good sync.
/// Paths gone from both sides are dropped.
pub fn update_baseline(old: &Baseline, entries: &[FolderEntry]) -> Baseline {
    let mut next = Baseline::default();
    collect(old, entries, &mut next);
    next
}

fn collect(old: &Baseline, entries: &[FolderEntry], out: &mut Baseline) {
    for e in entries {
        let in_sync = matches!(e.status, EntryStatus::Same | EntryStatus::Similar)
            || (e.is_dir && e.status == EntryStatus::Different);
        match (&e.left, &e.right) {
            (Some(l), Some(r)) if in_sync && l.is_dir == r.is_dir => {
                out.entries.insert(
                    e.rel_path.clone(),
                    BaselineEntry {
                        is_dir: e.is_dir,
                        left: l.into(),
                        right: r.into(),
                    },
                );
            }
            _ => {
                if let Some(prev) = old.get(&e.rel_path) {
                    out.entries.insert(e.rel_path.clone(), *prev);
                }
            }
        }
        collect(old, &e.children, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pcompare_core::folder::ContentMethod;

    fn meta(size: u64, ms: i64) -> EntryMeta {
        EntryMeta {
            size,
            modified_ms: Some(ms),
            is_dir: false,
            ..Default::default()
        }
    }

    fn entry(rel: &str, status: EntryStatus, l: Option<EntryMeta>, r: Option<EntryMeta>) -> FolderEntry {
        FolderEntry {
            name: rel.rsplit('/').next().unwrap().into(),
            rel_path: rel.into(),
            is_dir: false,
            status,
            detail: status.into(),
            newer: None,
            left: l,
            right: r,
            children: vec![],
            error: None,
            content_checked: None::<ContentMethod>,
            crc: None,
            left_rel_path: None,
            filtered: None,
            counts: None,
        }
    }

    #[test]
    fn changed_uses_size_and_timestamp_tolerance() {
        let opts = FolderCompareOptions::default();
        let then = Snap {
            size: 10,
            modified_ms: Some(1_000_000),
        };
        assert!(!side_changed(&meta(10, 1_001_000), &then, &opts), "within 2 s");
        assert!(side_changed(&meta(10, 1_010_000), &then, &opts));
        assert!(side_changed(&meta(11, 1_000_000), &then, &opts));
    }

    #[test]
    fn update_keeps_history_for_unsynced_paths() {
        let mut old = Baseline::default();
        let snap = Snap {
            size: 1,
            modified_ms: Some(5),
        };
        for rel in ["conflict.txt", "gone.txt"] {
            old.entries.insert(
                rel.into(),
                BaselineEntry {
                    is_dir: false,
                    left: snap,
                    right: snap,
                },
            );
        }
        let entries = vec![
            entry("same.txt", EntryStatus::Same, Some(meta(3, 9)), Some(meta(3, 9))),
            entry("conflict.txt", EntryStatus::Different, Some(meta(2, 9)), Some(meta(4, 9))),
            entry("new.txt", EntryStatus::LeftOnly, Some(meta(2, 9)), None),
        ];
        let next = update_baseline(&old, &entries);
        assert_eq!(next.entries.len(), 2);
        assert_eq!(next.get("same.txt").unwrap().left.size, 3);
        assert_eq!(next.get("conflict.txt").unwrap().left, snap, "old entry kept");
        assert!(next.get("new.txt").is_none());
        assert!(next.get("gone.txt").is_none(), "vanished from both sides");
    }
}
