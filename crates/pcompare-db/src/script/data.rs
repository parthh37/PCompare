//! Data sync planning: DELETE (children first), UPDATE (changed columns
//! only) and INSERT (parents first) statements from row diffs.

use super::{topo_order, ScriptOptions, SyncDirection};
use crate::data_diff::{is_rowversion, DataRow, RowStatus, TableDataDiff};
use crate::dialect::Dialect;
use crate::model::*;
use crate::value::Value;

/// One table's work, resolved against the target schema.
struct Job<'a> {
    diff: &'a TableDataDiff,
    /// Target table name and column spellings (index-aligned with the row values).
    name: &'a ObjectName,
    cols: &'a [String],
    table: Option<&'a Table>,
}

impl Job<'_> {
    fn key_len(&self) -> usize {
        self.diff.summary.key.len()
    }

    fn column(&self, i: usize) -> Option<&Column> {
        self.table.and_then(|t| t.column(&self.cols[i]))
    }

    /// Computed and row-version columns can't be written.
    fn writable(&self, i: usize, engine: Engine) -> bool {
        self.column(i).is_none_or(|c| c.computed.is_none() && !is_rowversion(Some(engine), c))
    }

    fn is_identity(&self, i: usize) -> bool {
        self.column(i).is_some_and(|c| c.identity.is_some())
    }

    fn key_pairs(&self, values: &[Value]) -> Vec<(String, Value)> {
        self.cols.iter().zip(values).take(self.key_len()).map(|(c, v)| (c.clone(), v.clone())).collect()
    }
}

/// `(source values, target values)` of a row for the direction.
fn sides(row: &DataRow, dir: SyncDirection) -> (Option<&Vec<Value>>, Option<&Vec<Value>>) {
    match dir {
        SyncDirection::LeftToRight => (row.left.as_ref(), row.right.as_ref()),
        SyncDirection::RightToLeft => (row.right.as_ref(), row.left.as_ref()),
    }
}

pub(super) fn statements(diffs: &[TableDataDiff], target: &Schema, opts: &ScriptOptions, d: &dyn Dialect) -> (Vec<String>, Vec<String>) {
    let engine = d.engine();
    let mut warnings = Vec::new();
    let jobs: Vec<Job> = diffs
        .iter()
        .filter(|t| t.summary.error.is_none() && (opts.ids.is_empty() || opts.ids.contains(&t.summary.id)))
        .filter_map(|diff| {
            let s = &diff.summary;
            let (name, cols) = match opts.direction {
                SyncDirection::LeftToRight => (s.right.as_ref()?, &s.right_columns),
                SyncDirection::RightToLeft => (s.left.as_ref()?, &s.columns),
            };
            let table = target.tables.iter().find(|t| same_name(&t.name, name));
            if table.is_none() {
                warnings.push(format!("{name} is not in the target schema; computed and identity columns can't be detected"));
            }
            Some(Job { diff, name, cols, table })
        })
        .collect();
    let (order, _) = topo_order(jobs.len(), |i| {
        let refs: Vec<&ObjectName> = jobs[i].table.map(|t| t.foreign_keys.iter().map(|fk| &fk.ref_table).collect()).unwrap_or_default();
        refs.iter().filter_map(|r| jobs.iter().position(|j| same_name(j.name, r))).collect()
    });

    let mut out = Vec::new();
    if opts.include_drops {
        for &i in order.iter().rev() {
            out.extend(deletes(&jobs[i], opts.direction, d));
        }
    }
    if opts.include_updates {
        for &i in &order {
            out.extend(updates(&jobs[i], opts.direction, d, engine));
        }
    }
    if opts.include_inserts {
        for &i in &order {
            out.extend(inserts(&jobs[i], opts.direction, d, engine));
        }
    }
    if opts.disable_foreign_keys && !out.is_empty() {
        let names: Vec<ObjectName> = order.iter().map(|&i| jobs[i].name.clone()).collect();
        let mut wrapped = d.disable_foreign_keys(&names);
        wrapped.append(&mut out);
        wrapped.extend(d.enable_foreign_keys(&names));
        out = wrapped;
    }
    (out, warnings)
}

fn same_name(a: &ObjectName, b: &ObjectName) -> bool {
    a.schema.eq_ignore_ascii_case(&b.schema) && a.name.eq_ignore_ascii_case(&b.name)
}

/// Rows present only in the target.
fn deletes(job: &Job, dir: SyncDirection, d: &dyn Dialect) -> Vec<String> {
    job.diff
        .rows
        .iter()
        .filter_map(|row| match sides(row, dir) {
            (None, Some(t)) => Some(d.delete(job.name, &job.key_pairs(t))),
            _ => None,
        })
        .collect()
}

/// Changed non-key columns of differing rows, keyed by the target's key values.
fn updates(job: &Job, dir: SyncDirection, d: &dyn Dialect, engine: Engine) -> Vec<String> {
    let k = job.key_len();
    // Engines that need IDENTITY_INSERT also refuse to UPDATE identity columns.
    let fixed_identity = d.identity_insert(job.name, true).is_some();
    let settable = |i: usize| i >= k && job.writable(i, engine) && !(fixed_identity && job.is_identity(i));
    let mut out = Vec::new();
    for row in job.diff.rows.iter().filter(|r| r.status == RowStatus::Different) {
        let (Some(src), Some(tgt)) = sides(row, dir) else { continue };
        let set: Vec<(String, Value)> = row
            .diffs
            .iter()
            .copied()
            .filter(|&i| settable(i) && i < src.len())
            .map(|i| (job.cols[i].clone(), src[i].clone()))
            .collect();
        if !set.is_empty() {
            out.push(d.update(job.name, &set, &job.key_pairs(tgt)));
        }
    }
    out
}

/// Rows present only in the source, with explicit identity values.
fn inserts(job: &Job, dir: SyncDirection, d: &dyn Dialect, engine: Engine) -> Vec<String> {
    let idx: Vec<usize> = (0..job.cols.len()).filter(|&i| job.writable(i, engine)).collect();
    let cols: Vec<String> = idx.iter().map(|&i| job.cols[i].clone()).collect();
    let mut out: Vec<String> = job
        .diff
        .rows
        .iter()
        .filter_map(|row| match sides(row, dir) {
            (Some(s), None) => Some(d.insert(job.name, &cols, &idx.iter().map(|&i| s[i].clone()).collect::<Vec<_>>())),
            _ => None,
        })
        .collect();
    if !out.is_empty() && idx.iter().any(|&i| job.is_identity(i)) {
        if let (Some(on), Some(off)) = (d.identity_insert(job.name, true), d.identity_insert(job.name, false)) {
            out.insert(0, on);
            out.push(off);
        }
    }
    out
}
