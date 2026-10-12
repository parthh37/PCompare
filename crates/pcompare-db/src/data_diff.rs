//! Data Compare: row-by-row comparison of matching tables, keyed by the
//! primary key (or a unique index / user-chosen key).

use crate::model::*;
use crate::schema_diff::{name_key, object_key, DiffStatus};
use crate::value::{compare_values, values_equal, Value, ValueRules};
use serde::{Deserialize, Serialize};
use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, HashMap};

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
/// Tables present on both sides become plans; tables present on one side
/// only are returned as finished `LeftOnly`/`RightOnly` summaries (row
/// counts 0 = not read).
pub fn plan_tables(left: &Schema, right: &Schema, opts: &DataCompareOptions) -> (Vec<TablePlan>, Vec<TableDataSummary>) {
    let ic = opts.ignore_name_case;
    let wanted = |id: &str| opts.tables.is_empty() || opts.tables.iter().any(|t| name_key(t, ic) == id);
    let mut sides: BTreeMap<String, (Option<&Table>, Option<&Table>)> = BTreeMap::new();
    for t in &left.tables {
        sides.entry(object_key(&t.name, ic)).or_default().0.get_or_insert(t);
    }
    for t in &right.tables {
        sides.entry(object_key(&t.name, ic)).or_default().1.get_or_insert(t);
    }
    let (mut plans, mut one_sided) = (Vec::new(), Vec::new());
    for (id, pair) in sides.into_iter().filter(|(id, _)| wanted(id)) {
        match pair {
            (Some(l), Some(r)) => plans.push(plan_table(id, (left.engine, l), (right.engine, r), opts)),
            (Some(l), None) => one_sided.push(TableDataSummary {
                id,
                left: Some(l.name.clone()),
                columns: l.columns.iter().map(|c| c.name.clone()).collect(),
                status: Some(DiffStatus::LeftOnly),
                ..Default::default()
            }),
            (None, Some(r)) => one_sided.push(TableDataSummary {
                id,
                right: Some(r.name.clone()),
                right_columns: r.columns.iter().map(|c| c.name.clone()).collect(),
                status: Some(DiffStatus::RightOnly),
                ..Default::default()
            }),
            (None, None) => {}
        }
    }
    (plans, one_sided)
}

/// SQL Server `rowversion` (alias `timestamp`): database-specific values
/// that can be neither compared meaningfully nor written.
pub(crate) fn is_rowversion(engine: Option<Engine>, c: &Column) -> bool {
    engine == Some(Engine::Mssql) && matches!(c.data_type.as_str(), "timestamp" | "rowversion")
}

fn plan_table(id: String, (le, l): (Option<Engine>, &Table), (re, r): (Option<Engine>, &Table), opts: &DataCompareOptions) -> TablePlan {
    let ic = opts.ignore_name_case;
    let eq = |a: &str, b: &str| if ic { a.eq_ignore_ascii_case(b) || a.to_lowercase() == b.to_lowercase() } else { a == b };
    let find = |t: &'_ Table, name: &str| t.columns.iter().find(|c| eq(&c.name, name)).cloned();
    let ignored = opts.ignored_columns.get(&id).cloned().unwrap_or_default();
    // (left column, right column) for every column on both sides, left order.
    let common: Vec<(Column, Column)> = l
        .columns
        .iter()
        .filter_map(|c| find(r, &c.name).map(|rc| (c.clone(), rc)))
        .filter(|(lc, rc)| !is_rowversion(le, lc) && !is_rowversion(re, rc))
        .collect();
    let left_only_columns = l.columns.iter().filter(|c| find(r, &c.name).is_none()).map(|c| c.name.clone()).collect();
    let right_only_columns = r.columns.iter().filter(|c| find(l, &c.name).is_none()).map(|c| c.name.clone()).collect();

    let key = choose_key(&id, l, opts, &common, &eq);
    let (key, error) = match key {
        Ok(k) => (k, None),
        Err(e) => (Vec::new(), Some(e)),
    };
    let is_key = |n: &str| key.iter().any(|k| eq(k, n));
    let mut pairs: Vec<&(Column, Column)> =
        key.iter().filter_map(|k| common.iter().find(|(lc, _)| eq(&lc.name, k))).collect();
    pairs.extend(common.iter().filter(|(lc, _)| !is_key(&lc.name) && !ignored.iter().any(|x| eq(x, &lc.name))));
    TablePlan {
        id,
        left: l.name.clone(),
        right: r.name.clone(),
        key: pairs[..key.len()].iter().map(|(lc, _)| lc.name.clone()).collect(),
        columns: pairs.iter().map(|(lc, _)| lc.name.clone()).collect(),
        right_columns: pairs.iter().map(|(_, rc)| rc.name.clone()).collect(),
        left_only_columns,
        right_only_columns,
        error,
    }
}

/// Key columns (left spelling): override → primary key → first usable
/// unique index (all columns on both sides and NOT NULL on both).
fn choose_key(
    id: &str,
    l: &Table,
    opts: &DataCompareOptions,
    common: &[(Column, Column)],
    eq: &dyn Fn(&str, &str) -> bool,
) -> Result<Vec<String>, String> {
    let on_both = |n: &str| common.iter().find(|(lc, _)| eq(&lc.name, n));
    if let Some(cols) = opts.keys.get(id).filter(|k| !k.is_empty()) {
        return cols
            .iter()
            .map(|c| on_both(c).map(|(lc, _)| lc.name.clone()).ok_or_else(|| format!("Key column `{c}` is not on both sides")))
            .collect();
    }
    if let Some(pk) = l.primary_key.as_ref().filter(|pk| !pk.columns.is_empty()) {
        if pk.columns.iter().all(|c| on_both(c).is_some()) {
            return Ok(pk.columns.iter().map(|c| on_both(c).expect("checked").0.name.clone()).collect());
        }
    }
    let usable = |idx: &&Index| {
        idx.unique
            && idx.filter.is_none()
            && !idx.columns.is_empty()
            && idx.columns.iter().all(|c| on_both(&c.name).is_some_and(|(lc, rc)| !lc.nullable && !rc.nullable))
    };
    match l.indexes.iter().find(usable) {
        Some(idx) => Ok(idx.columns.iter().map(|c| on_both(&c.name).expect("checked").0.name.clone()).collect()),
        None => Err("No primary key or unique index — choose key columns".into()),
    }
}

/// Join key of a row: canonical key values joined with U+001F, so `Int(1)`
/// and `Decimal("1")` meet, and case/padding follow the value rules.
fn row_key(row: &[Value], k: usize, rules: &ValueRules) -> String {
    let mut s = String::new();
    for (i, v) in row.iter().take(k).enumerate() {
        if i > 0 {
            s.push('\u{1f}');
        }
        let int = v.as_i128().or(match v {
            Value::Float(f) if f.is_finite() && f.fract() == 0.0 && f.abs() < 1e18 => Some(*f as i128),
            _ => None,
        });
        match (v, int) {
            (Value::Null, _) => s.push('\u{0}'),
            (_, Some(n)) => s.push_str(&n.to_string()),
            (Value::Decimal(d), None) => s.push_str(&Value::canonical_decimal(d)),
            _ => {
                let text = v.to_string();
                let text = if rules.trim_whitespace { text.trim() } else { &text };
                if rules.ignore_case {
                    s.push_str(&text.to_lowercase());
                } else {
                    s.push_str(text);
                }
            }
        }
    }
    s
}

/// Join state of one key.
struct Slot {
    right: Option<usize>,
    left_seen: bool,
}

/// Compares two row sets (values in `plan.columns` / `plan.right_columns`
/// order). Returns every non-identical row sorted by key; callers truncate
/// for display. Duplicate keys are counted into `summary.error` (only the
/// first row per key takes part in the comparison).
pub fn diff_rows(plan: &TablePlan, left: Vec<Vec<Value>>, right: Vec<Vec<Value>>, rules: &ValueRules) -> TableDataDiff {
    let mut summary = TableDataSummary {
        id: plan.id.clone(),
        left: Some(plan.left.clone()),
        right: Some(plan.right.clone()),
        key: plan.key.clone(),
        columns: plan.columns.clone(),
        right_columns: plan.right_columns.clone(),
        left_only_columns: plan.left_only_columns.clone(),
        right_only_columns: plan.right_only_columns.clone(),
        left_rows: left.len(),
        right_rows: right.len(),
        error: plan.error.clone(),
        ..Default::default()
    };
    if plan.error.is_some() {
        return TableDataDiff { summary, ..Default::default() };
    }
    let k = plan.key.len();
    let mut slots: HashMap<String, Slot> = HashMap::with_capacity(right.len());
    let (mut left_dups, mut right_dups, mut first_dup) = (0usize, 0usize, None::<String>);
    for (j, row) in right.iter().enumerate() {
        if let Entry::Vacant(e) = slots.entry(row_key(row, k, rules)) {
            e.insert(Slot { right: Some(j), left_seen: false });
        } else {
            right_dups += 1;
            first_dup.get_or_insert_with(|| display_key(row, k));
        }
    }
    let mut right: Vec<Option<Vec<Value>>> = right.into_iter().map(Some).collect();
    let mut rows = Vec::new();
    for l in left {
        let slot = match slots.entry(row_key(&l, k, rules)) {
            Entry::Occupied(e) => e.into_mut(),
            Entry::Vacant(e) => e.insert(Slot { right: None, left_seen: false }),
        };
        if slot.left_seen {
            left_dups += 1;
            first_dup.get_or_insert_with(|| display_key(&l, k));
            continue;
        }
        slot.left_seen = true;
        match slot.right.and_then(|j| right[j].take()) {
            Some(r) => {
                let n = l.len().min(r.len());
                let diffs: Vec<usize> = (k..n).filter(|&i| !values_equal(&l[i], &r[i], rules)).collect();
                if diffs.is_empty() {
                    summary.same += 1;
                } else {
                    summary.different += 1;
                    rows.push(DataRow { status: RowStatus::Different, key: l[..k.min(n)].to_vec(), left: Some(l), right: Some(r), diffs });
                }
            }
            None => {
                summary.left_only += 1;
                rows.push(DataRow { status: RowStatus::LeftOnly, key: l[..k.min(l.len())].to_vec(), left: Some(l), right: None, diffs: Vec::new() });
            }
        }
    }
    drop(slots);
    for r in right.into_iter().flatten() {
        summary.right_only += 1;
        rows.push(DataRow { status: RowStatus::RightOnly, key: r[..k.min(r.len())].to_vec(), left: None, right: Some(r), diffs: Vec::new() });
    }
    rows.sort_by(|a, b| {
        a.key.iter().zip(&b.key).map(|(x, y)| compare_values(x, y)).find(|o| o.is_ne()).unwrap_or(std::cmp::Ordering::Equal)
    });
    let differs = summary.different + summary.left_only + summary.right_only > 0;
    summary.status = Some(if differs { DiffStatus::Different } else { DiffStatus::Same });
    if left_dups + right_dups > 0 {
        summary.error = Some(format!(
            "Key is not unique: {left_dups} duplicate row(s) on the left, {right_dups} on the right (e.g. key {}) — choose key columns",
            first_dup.unwrap_or_default()
        ));
    }
    TableDataDiff { summary, rows, truncated: false }
}

fn display_key(row: &[Value], k: usize) -> String {
    row.iter().take(k).map(|v| v.to_string()).collect::<Vec<_>>().join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema_diff::fixtures::*;
    use Value::*;

    fn opts() -> DataCompareOptions {
        DataCompareOptions::default()
    }

    fn users(name: &str) -> Table {
        with_pk(
            table(name, vec![not_null(col("Id", "int")), col("Email", "text"), col("Name", "text"), col("Extra", "text")]),
            "PK",
            &["Id"],
        )
    }

    #[test]
    fn plan_uses_primary_key_and_aligns_columns() {
        let l = schema(vec![users("Users")], vec![]);
        let mut rt = users("USERS");
        rt.columns = vec![col("name", "text"), col("Other", "int"), not_null(col("ID", "int")), col("EMAIL", "text")];
        let r = schema(vec![rt], vec![]);
        let mut o = opts();
        o.ignored_columns.insert("dbo.users".into(), vec!["email".into()]);
        let (plans, one_sided) = plan_tables(&l, &r, &o);
        assert!(one_sided.is_empty());
        let p = &plans[0];
        assert_eq!(p.id, "dbo.users");
        assert_eq!(p.error, None);
        assert_eq!(p.key, ["Id"]);
        assert_eq!(p.columns, ["Id", "Name"]);
        assert_eq!(p.right_columns, ["ID", "name"]);
        assert_eq!(p.left_only_columns, ["Extra"]);
        assert_eq!(p.right_only_columns, ["Other"]);
    }

    #[test]
    fn plan_key_fallbacks() {
        let mut t = table("T", vec![col("Code", "text"), not_null(col("Sku", "text")), col("Qty", "int")]);
        t.indexes.push(index("UX_Code", &["Code"], true)); // nullable: unusable
        t.indexes.push(index("IX_Qty", &["Qty"], false)); // not unique
        let s = schema(vec![t.clone()], vec![]);
        let (plans, _) = plan_tables(&s, &s, &opts());
        assert_eq!(plans[0].error.as_deref(), Some("No primary key or unique index — choose key columns"));
        assert!(plans[0].key.is_empty());

        t.indexes.push(index("UX_Sku", &["Sku"], true));
        let s = schema(vec![t], vec![]);
        let (plans, _) = plan_tables(&s, &s, &opts());
        assert_eq!((plans[0].key.clone(), plans[0].columns.clone()), (vec!["Sku".to_owned()], vec!["Sku".into(), "Code".into(), "Qty".into()]));

        let mut o = opts();
        o.keys.insert("dbo.t".into(), vec!["qty".into(), "code".into()]);
        let (plans, _) = plan_tables(&s, &s, &o);
        assert_eq!(plans[0].key, ["Qty", "Code"]);
        assert_eq!(plans[0].columns, ["Qty", "Code", "Sku"]);
        o.keys.insert("dbo.t".into(), vec!["Nope".into()]);
        let (plans, _) = plan_tables(&s, &s, &o);
        assert_eq!(plans[0].error.as_deref(), Some("Key column `Nope` is not on both sides"));
    }

    #[test]
    fn plan_reports_one_sided_tables_and_filters() {
        let l = schema(vec![users("A"), users("Both")], vec![]);
        let r = schema(vec![users("Both"), users("Z")], vec![]);
        let (plans, one) = plan_tables(&l, &r, &opts());
        assert_eq!(plans.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(), ["dbo.both"]);
        assert_eq!(one.len(), 2);
        assert_eq!((one[0].id.as_str(), one[0].status, one[0].left_rows), ("dbo.a", Some(DiffStatus::LeftOnly), 0));
        assert_eq!((one[1].id.as_str(), one[1].status), ("dbo.z", Some(DiffStatus::RightOnly)));
        let mut o = opts();
        o.tables = vec!["dbo.z".into()];
        let (plans, one) = plan_tables(&l, &r, &o);
        assert!(plans.is_empty());
        assert_eq!(one.len(), 1);
    }

    #[test]
    fn plan_skips_rowversion_columns() {
        let mut t = users("T");
        t.columns.push(not_null(col("RowVer", "rowversion")));
        let mut s = schema(vec![t], vec![]);
        s.engine = Some(Engine::Mssql);
        let (plans, _) = plan_tables(&s, &s, &opts());
        assert_eq!(plans[0].columns, ["Id", "Email", "Name", "Extra"]);
    }

    fn plan(cols: &[&str], key: usize) -> TablePlan {
        let cols: Vec<String> = cols.iter().map(|c| c.to_string()).collect();
        TablePlan {
            id: "dbo.t".into(),
            left: ObjectName::new("dbo", "t"),
            right: ObjectName::new("dbo", "t"),
            key: cols[..key].to_vec(),
            right_columns: cols.clone(),
            columns: cols,
            left_only_columns: vec![],
            right_only_columns: vec![],
            error: None,
        }
    }

    #[test]
    fn diff_rows_joins_mixed_numeric_keys_and_sorts() {
        let p = plan(&["id", "name", "note"], 1);
        let left = vec![
            vec![Int(10), Text("ten".into()), Null],
            vec![Int(2), Text("two".into()), Null],
            vec![Int(1), Text("one".into()), Text("x".into())],
            vec![Int(5), Text("five".into()), Null],
        ];
        let right = vec![
            vec![Decimal("1".into()), Text("one".into()), Text("x".into())],
            vec![Decimal("2".into()), Text("TWO".into()), Null],
            vec![UInt(10), Text("ten".into()), Text("".into())],
            vec![Int(7), Text("seven".into()), Null],
        ];
        let d = diff_rows(&p, left, right, &ValueRules::default());
        let s = &d.summary;
        assert_eq!((s.same, s.different, s.left_only, s.right_only), (1, 2, 1, 1));
        assert_eq!((s.left_rows, s.right_rows, s.status, s.error.clone()), (4, 4, Some(DiffStatus::Different), None));
        let keys: Vec<String> = d.rows.iter().map(|r| r.key[0].to_string()).collect();
        assert_eq!(keys, ["2", "5", "7", "10"]);
        assert_eq!((d.rows[0].status, d.rows[0].diffs.clone()), (RowStatus::Different, vec![1]));
        assert_eq!(d.rows[1].status, RowStatus::LeftOnly);
        assert_eq!((d.rows[2].status, d.rows[2].key.clone()), (RowStatus::RightOnly, vec![Int(7)]));
        // NULL vs '' differs; NULL vs NULL does not.
        assert_eq!(d.rows[3].diffs, [2]);

        let rules = ValueRules { ignore_case: true, ..Default::default() };
        let left = vec![vec![Text("A".into()), Text("TWO".into()), Null]];
        let right = vec![vec![Text("a".into()), Text("two".into()), Null]];
        let d = diff_rows(&p, left, right, &rules);
        assert_eq!((d.summary.same, d.summary.status), (1, Some(DiffStatus::Same)));
        assert!(d.rows.is_empty());
    }

    #[test]
    fn diff_rows_reports_duplicate_keys() {
        let p = plan(&["a", "b", "v"], 2);
        let left = vec![vec![Int(1), Int(1), Int(0)], vec![Int(1), Int(1), Int(9)], vec![Int(1), Int(2), Int(0)]];
        let right = vec![vec![Int(1), Int(1), Int(0)], vec![Int(1), Int(2), Int(0)], vec![Int(1), Int(2), Int(5)]];
        let d = diff_rows(&p, left, right, &ValueRules::default());
        let err = d.summary.error.unwrap();
        assert!(err.contains("1 duplicate row(s) on the left, 1 on the right"), "{err}");
        assert!(err.contains("key 1, 2") || err.contains("key 1, 1"), "{err}");
        assert_eq!(d.summary.same, 2);
    }

    #[test]
    fn diff_rows_passes_plan_errors_through() {
        let mut p = plan(&["a"], 0);
        p.error = Some("No primary key or unique index — choose key columns".into());
        let d = diff_rows(&p, vec![], vec![], &ValueRules::default());
        assert_eq!(d.summary.error, p.error);
        assert_eq!(d.summary.status, None);
        assert_eq!(d.summary.left, Some(p.left.clone()));
    }
}
