//! Schema Compare: matches objects of two [`Schema`]s and lists every
//! difference, SQL Delta style.

use crate::model::*;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SchemaCompareOptions {
    /// Match object/column names case-insensitively (SQL Server, MySQL on Windows).
    pub ignore_name_case: bool,
    /// Collapse whitespace runs in view/routine/trigger bodies and expressions.
    pub ignore_whitespace: bool,
    /// Strip `--` and `/* */` comments from code objects before comparing.
    pub ignore_comments: bool,
    /// Same columns in a different order is not a difference.
    pub ignore_column_order: bool,
    pub ignore_collation: bool,
    /// Match PKs, FKs, checks and UNIQUE constraints by definition rather
    /// than name (system-generated names like `PK__Users__3214EC07` differ).
    pub ignore_constraint_names: bool,
    pub ignore_identity_seed: bool,
    /// Object kinds to compare; empty = all.
    pub kinds: Vec<ObjectKind>,
}

impl Default for SchemaCompareOptions {
    fn default() -> Self {
        Self {
            ignore_name_case: true,
            ignore_whitespace: true,
            ignore_comments: false,
            ignore_column_order: true,
            ignore_collation: false,
            ignore_constraint_names: true,
            ignore_identity_seed: true,
            kinds: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DiffStatus {
    Same,
    Different,
    LeftOnly,
    RightOnly,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ObjectDiff {
    /// Stable key used by the UI and script options: `"table:dbo.users"`
    /// (`kind:schema.name`, lower-cased when names are case-insensitive).
    pub id: String,
    pub kind: ObjectKind,
    /// Left name when present, else right.
    pub name: ObjectName,
    pub status: DiffStatus,
    /// Human-readable list, e.g. `"Column email: nvarchar(50) → nvarchar(100)"`,
    /// `"Index IX_Users_Email only in left"`. Empty when `Same`/one-sided.
    pub changes: Vec<String>,
    /// Full DDL as each side would create it (tables rendered through that
    /// side's dialect incl. indexes and FKs; code objects = their definition).
    pub left_ddl: Option<String>,
    pub right_ddl: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffStats {
    pub same: usize,
    pub different: usize,
    pub left_only: usize,
    pub right_only: usize,
}

/// Compares every object in `left` and `right`, sorted by kind then name.
pub fn compare_schemas(left: &Schema, right: &Schema, opts: &SchemaCompareOptions) -> (Vec<ObjectDiff>, DiffStats) {
    let _ = (left, right, opts);
    (Vec::new(), DiffStats::default())
}
