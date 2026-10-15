//! The sync matrix: one action per category of item, as in a
//! custom Folder Sync (one action per comparison result).
//!
//! Every fixed [`SyncMode`] is a preset row of this table
//! ([`SyncMatrix::preset`]), so the planner only ever consults a matrix;
//! [`SyncMode::Custom`] uses the one the user built.

use serde::{Deserialize, Serialize};

use crate::sync::SyncMode;

/// What to do with the items of one [`SyncCategory`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SyncAction {
    /// No operation; the item is listed as left alone.
    #[default]
    Leave,
    CopyLeftToRight,
    CopyRightToLeft,
    DeleteLeft,
    DeleteRight,
    /// Left unresolved in the plan (as a conflict) for the user to decide
    /// in the preview.
    Prompt,
}

impl SyncAction {
    pub const ALL: [SyncAction; 6] = [
        SyncAction::Leave,
        SyncAction::CopyLeftToRight,
        SyncAction::CopyRightToLeft,
        SyncAction::DeleteLeft,
        SyncAction::DeleteRight,
        SyncAction::Prompt,
    ];

    /// Whether the action can do anything to an item present on the given
    /// sides: a copy needs its source, a delete its target.
    pub fn applies(self, on_left: bool, on_right: bool) -> bool {
        match self {
            SyncAction::Leave | SyncAction::Prompt => true,
            SyncAction::CopyLeftToRight | SyncAction::DeleteLeft => on_left,
            SyncAction::CopyRightToLeft | SyncAction::DeleteRight => on_right,
        }
    }

    pub fn deletes(self) -> bool {
        matches!(self, SyncAction::DeleteLeft | SyncAction::DeleteRight)
    }
}

/// How an item compares, for choosing its action. Folders present on both
/// sides have no category of their own: their contents are planned item by
/// item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SyncCategory {
    /// Different, and the left copy is newer (or, with a baseline, only the
    /// left one changed since the last sync).
    LeftNewer,
    RightNewer,
    /// Only on the left, and not known from the last sync (new, or no
    /// baseline to tell).
    LeftOrphan,
    RightOrphan,
    /// Only on the right, but both sides had it at the last sync: it was
    /// deleted on the left (two-way baseline).
    LeftDeleted,
    /// Only on the left, deleted on the right since the last sync.
    RightDeleted,
    /// Different, with neither side newer.
    Different,
    /// Same (or similar under the comparison's rules).
    Same,
}

impl SyncCategory {
    pub const ALL: [SyncCategory; 8] = [
        SyncCategory::LeftNewer,
        SyncCategory::RightNewer,
        SyncCategory::LeftOrphan,
        SyncCategory::RightOrphan,
        SyncCategory::LeftDeleted,
        SyncCategory::RightDeleted,
        SyncCategory::Different,
        SyncCategory::Same,
    ];

    /// Which sides hold items of this category: `(left, right)`.
    pub fn sides(self) -> (bool, bool) {
        match self {
            SyncCategory::LeftOrphan | SyncCategory::RightDeleted => (true, false),
            SyncCategory::RightOrphan | SyncCategory::LeftDeleted => (false, true),
            _ => (true, true),
        }
    }

    /// Whether `action` makes sense here (see [`SyncAction::applies`]).
    pub fn allows(self, action: SyncAction) -> bool {
        let (l, r) = self.sides();
        action.applies(l, r)
    }
}

/// One action per category. Missing fields read as [`SyncAction::Leave`],
/// so a partial matrix never deletes anything it doesn't name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SyncMatrix {
    pub left_newer: SyncAction,
    pub right_newer: SyncAction,
    pub left_orphan: SyncAction,
    pub right_orphan: SyncAction,
    pub left_deleted: SyncAction,
    pub right_deleted: SyncAction,
    pub different: SyncAction,
    pub same: SyncAction,
}

impl SyncMatrix {
    pub fn get(&self, cat: SyncCategory) -> SyncAction {
        match cat {
            SyncCategory::LeftNewer => self.left_newer,
            SyncCategory::RightNewer => self.right_newer,
            SyncCategory::LeftOrphan => self.left_orphan,
            SyncCategory::RightOrphan => self.right_orphan,
            SyncCategory::LeftDeleted => self.left_deleted,
            SyncCategory::RightDeleted => self.right_deleted,
            SyncCategory::Different => self.different,
            SyncCategory::Same => self.same,
        }
    }

    pub fn set(&mut self, cat: SyncCategory, action: SyncAction) {
        let slot = match cat {
            SyncCategory::LeftNewer => &mut self.left_newer,
            SyncCategory::RightNewer => &mut self.right_newer,
            SyncCategory::LeftOrphan => &mut self.left_orphan,
            SyncCategory::RightOrphan => &mut self.right_orphan,
            SyncCategory::LeftDeleted => &mut self.left_deleted,
            SyncCategory::RightDeleted => &mut self.right_deleted,
            SyncCategory::Different => &mut self.different,
            SyncCategory::Same => &mut self.same,
        };
        *slot = action;
    }

    /// The matrix a fixed mode stands for; `Custom` has none and gets
    /// `custom`. Without a baseline nothing is ever "deleted", so the
    /// one-way presets treat deleted items like the orphans they look like.
    pub fn for_mode(mode: SyncMode, custom: &SyncMatrix) -> SyncMatrix {
        use SyncAction::*;
        let m = |left_newer, right_newer, left_orphan, right_orphan, left_deleted, right_deleted, different| SyncMatrix {
            left_newer,
            right_newer,
            left_orphan,
            right_orphan,
            left_deleted,
            right_deleted,
            different,
            same: Leave,
        };
        match mode {
            SyncMode::UpdateLeftToRight => m(CopyLeftToRight, Leave, CopyLeftToRight, Leave, Leave, CopyLeftToRight, Prompt),
            SyncMode::UpdateRightToLeft => m(Leave, CopyRightToLeft, Leave, CopyRightToLeft, CopyRightToLeft, Leave, Prompt),
            SyncMode::UpdateBoth => m(
                CopyLeftToRight,
                CopyRightToLeft,
                CopyLeftToRight,
                CopyRightToLeft,
                CopyRightToLeft,
                CopyLeftToRight,
                Prompt,
            ),
            SyncMode::MirrorLeftToRight => m(
                CopyLeftToRight,
                CopyLeftToRight,
                CopyLeftToRight,
                DeleteRight,
                DeleteRight,
                CopyLeftToRight,
                CopyLeftToRight,
            ),
            SyncMode::MirrorRightToLeft => m(
                CopyRightToLeft,
                CopyRightToLeft,
                DeleteLeft,
                CopyRightToLeft,
                CopyRightToLeft,
                DeleteLeft,
                CopyRightToLeft,
            ),
            SyncMode::TwoWay => m(
                CopyLeftToRight,
                CopyRightToLeft,
                CopyLeftToRight,
                CopyRightToLeft,
                DeleteRight,
                DeleteLeft,
                Prompt,
            ),
            SyncMode::Custom => *custom,
        }
    }

    /// The preset for a fixed mode (`Custom` gives the all-leave matrix).
    pub fn preset(mode: SyncMode) -> SyncMatrix {
        SyncMatrix::for_mode(mode, &SyncMatrix::default())
    }

    /// The fixed mode this matrix is exactly the preset of, if any.
    pub fn as_preset(&self) -> Option<SyncMode> {
        SyncMode::PRESETS.into_iter().find(|&m| SyncMatrix::preset(m) == *self)
    }

    /// Whether any category deletes.
    pub fn deletes(&self) -> bool {
        SyncCategory::ALL.iter().any(|&c| self.get(c).deletes())
    }

    /// Whether any category copies toward the right (`true`) / left.
    pub fn copies_to(&self, right: bool) -> bool {
        let want = if right { SyncAction::CopyLeftToRight } else { SyncAction::CopyRightToLeft };
        SyncCategory::ALL.iter().any(|&c| self.get(c) == want)
    }

    /// How many categories wait for the user's decision.
    pub fn prompt_cells(&self) -> usize {
        SyncCategory::ALL.iter().filter(|&&c| self.get(c) == SyncAction::Prompt).count()
    }

    /// Cells naming an action their category can't take (e.g. "copy right
    /// to left" for left orphans). The planner leaves such items alone.
    pub fn invalid_cells(&self) -> Vec<SyncCategory> {
        SyncCategory::ALL.into_iter().filter(|&c| !c.allows(self.get(c))).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_round_trip_and_are_valid() {
        for mode in SyncMode::PRESETS {
            let m = SyncMatrix::preset(mode);
            assert_eq!(m.as_preset(), Some(mode), "{mode:?}");
            assert!(m.invalid_cells().is_empty(), "{mode:?}: {:?}", m.invalid_cells());
            assert_eq!(m.same, SyncAction::Leave);
        }
        assert_eq!(SyncMatrix::default().as_preset(), None);
    }

    #[test]
    fn partial_json_leaves_the_rest_alone() {
        let m: SyncMatrix = serde_json::from_str(r#"{"leftNewer":"copyLeftToRight","rightOrphan":"deleteRight"}"#).unwrap();
        assert_eq!(m.left_newer, SyncAction::CopyLeftToRight);
        assert_eq!(m.right_orphan, SyncAction::DeleteRight);
        assert_eq!(m.different, SyncAction::Leave);
        assert!(m.deletes() && m.copies_to(true) && !m.copies_to(false));
        let json = serde_json::to_value(SyncMatrix::preset(SyncMode::TwoWay)).unwrap();
        assert_eq!(json["leftDeleted"], "deleteRight");
        assert_eq!(json["different"], "prompt");
    }

    #[test]
    fn applicability_follows_the_sides_present() {
        use SyncAction::*;
        assert!(SyncCategory::LeftOrphan.allows(CopyLeftToRight));
        assert!(SyncCategory::LeftOrphan.allows(DeleteLeft));
        assert!(!SyncCategory::LeftOrphan.allows(CopyRightToLeft));
        assert!(!SyncCategory::LeftOrphan.allows(DeleteRight));
        assert!(SyncCategory::LeftDeleted.allows(CopyRightToLeft), "restore");
        assert!(!SyncCategory::LeftDeleted.allows(DeleteLeft));
        assert!(SyncCategory::ALL.iter().all(|c| c.allows(Leave) && c.allows(Prompt)));
        let mut m = SyncMatrix::default();
        m.set(SyncCategory::RightOrphan, CopyLeftToRight);
        assert_eq!(m.invalid_cells(), [SyncCategory::RightOrphan]);
    }
}
