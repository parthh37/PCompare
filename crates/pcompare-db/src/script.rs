//! Sync scripts: the statements that make the target side match the source.

use crate::data_diff::TableDataDiff;
use crate::model::*;
use crate::schema_diff::SchemaCompareOptions;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SyncDirection {
    /// Make the right database match the left.
    #[default]
    LeftToRight,
    /// Make the left database match the right.
    RightToLeft,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ScriptOptions {
    pub direction: SyncDirection,
    /// Object ids (schema) or table ids (data) to include; empty = every difference.
    pub ids: Vec<String>,
    /// Schema: drop objects that exist only in the target. Data: delete target-only rows.
    pub include_drops: bool,
    /// Data: insert source-only rows.
    pub include_inserts: bool,
    /// Data: update differing rows.
    pub include_updates: bool,
    pub transactional: bool,
    /// Data: disable FK checks while applying rows (where the engine allows).
    pub disable_foreign_keys: bool,
}

impl Default for ScriptOptions {
    fn default() -> Self {
        Self {
            direction: SyncDirection::LeftToRight,
            ids: Vec::new(),
            include_drops: true,
            include_inserts: true,
            include_updates: true,
            transactional: true,
            disable_foreign_keys: true,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncScript {
    pub target_engine: Option<Engine>,
    /// Executable statements in order, without terminators. Execution runs
    /// exactly these (transaction control is applied by the executor).
    pub statements: Vec<String>,
    /// Human-readable script: header comment, BEGIN/COMMIT when
    /// transactional, every statement followed by the dialect terminator.
    pub text: String,
    /// E.g. "MySQL can't roll back DDL", "column type copied from SQL Server".
    pub warnings: Vec<String>,
    pub transactional: bool,
}

/// Statements turning the target schema into the source schema for the
/// selected objects, dependency-ordered.
pub fn schema_script(left: &Schema, right: &Schema, cmp: &SchemaCompareOptions, opts: &ScriptOptions) -> SyncScript {
    let _ = (left, right, cmp, opts);
    SyncScript::default()
}

/// INSERT/UPDATE/DELETE statements applying `diffs` to the target.
/// `target_schema` provides identity columns and FK order for the target.
pub fn data_script(diffs: &[TableDataDiff], target_schema: &Schema, opts: &ScriptOptions) -> SyncScript {
    let _ = (diffs, target_schema, opts);
    SyncScript::default()
}
