//! Data Compare: row-by-row comparison of matching tables, keyed by the
//! primary key (or a unique index / user-chosen key).

use crate::model::*;
use crate::schema_diff::DiffStatus;
use crate::value::{Value, ValueRules};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DataCompareOptions {
    pub rules: ValueRules,
    /// Table ids (`schema.name`, as in [`TablePlan::id`]) to compare; empty = every table on both sides.
    pub tables: Vec<String>,
    /// Per-table key column override (table id → column names).
    pub keys: BTreeMap<String, Vec<String>>,
    /// Per-table columns to leave out of the comparison.
    pub ignored_columns: BTreeMap<String, Vec<String>>,
    /// Match table and column names case-insensitively.
    pub ignore_name_case: bool,
    /// Cap on rows returned per table for display (scripts always use all rows).
    pub max_rows: usize,
}

impl Default for DataCompareOptions {
    fn default() -> Self {
        Self {
            rules: ValueRules::default(),
            tables: Vec::new(),
            keys: BTreeMap::new(),
            ignored_columns: BTreeMap::new(),
            ignore_name_case: true,
            max_rows: 1000,
        }
    }
}

/// How one left table is matched to one right table.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TablePlan {
    /// `schema.name` (lower-cased when names are case-insensitive).
    pub id: String,
    pub left: ObjectName,
    pub right: ObjectName,
    /// Key columns (left spelling), a prefix of `columns`.
    pub key: Vec<String>,
    /// Compared columns present on both sides, key columns first (left spelling).
    pub columns: Vec<String>,
    /// The same columns in right-side spelling, index-aligned with `columns`.
    pub right_columns: Vec<String>,
    pub left_only_columns: Vec<String>,
    pub right_only_columns: Vec<String>,
    /// Why this table can't be compared (e.g. no primary key or unique index).
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RowStatus {
    Different,
    LeftOnly,
    RightOnly,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DataRow {
    pub status: RowStatus,
    /// Key values (as in the left row, else the right).
    pub key: Vec<Value>,
    /// Values in `TablePlan::columns` order.
    pub left: Option<Vec<Value>>,
    pub right: Option<Vec<Value>>,
    /// Indices into `columns` whose values differ.
    pub diffs: Vec<usize>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TableDataSummary {
    pub id: String,
    pub left: Option<ObjectName>,
    pub right: Option<ObjectName>,
    pub key: Vec<String>,
    /// Compared columns, left spelling (key first).
    pub columns: Vec<String>,
    /// The same columns in right-side spelling (index-aligned).
    pub right_columns: Vec<String>,
    pub left_only_columns: Vec<String>,
    pub right_only_columns: Vec<String>,
    /// `LeftOnly`/`RightOnly` = the table exists on one side only.
    pub status: Option<DiffStatus>,
    pub left_rows: usize,
    pub right_rows: usize,
    pub same: usize,
    pub different: usize,
    pub left_only: usize,
    pub right_only: usize,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TableDataDiff {
    pub summary: TableDataSummary,
    /// Non-identical rows sorted by key.
    pub rows: Vec<DataRow>,
    /// `rows` was cut to `max_rows` for display.
    pub truncated: bool,
}

/// Matches tables by name and picks key + compared columns for each.
/// Tables present on one side only are returned via `TableDataSummary`
/// by the caller; this only plans tables present on both sides.
pub fn plan_tables(left: &Schema, right: &Schema, opts: &DataCompareOptions) -> (Vec<TablePlan>, Vec<TableDataSummary>) {
    let _ = (left, right, opts);
    (Vec::new(), Vec::new())
}

/// Compares two row sets (values in `plan.columns` / `plan.right_columns`
/// order). Returns every non-identical row; callers truncate for display.
pub fn diff_rows(plan: &TablePlan, left: Vec<Vec<Value>>, right: Vec<Vec<Value>>, rules: &ValueRules) -> TableDataDiff {
    let _ = (plan, left, right, rules);
    TableDataDiff::default()
}
