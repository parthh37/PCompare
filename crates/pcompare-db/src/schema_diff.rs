//! Schema Compare: matches objects of two [`Schema`]s and lists every
//! difference.
//!
//! The structured table diff ([`TableDiff`]) is shared with [`crate::script`],
//! so "what differs" is decided in exactly one place for both the UI and the
//! generated sync scripts.

#[cfg(test)]
pub(crate) mod fixtures;
mod normalize;

use crate::dialect::{self, Dialect};
use crate::model::*;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};

pub(crate) use normalize::{identifiers, normalize_definition, normalize_expr, normalize_type};

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
    let (ld, rd) = (dialect_for(left), dialect_for(right));
    let mut stats = DiffStats::default();
    let objects: Vec<ObjectDiff> = match_objects(left, right, opts)
        .into_iter()
        .map(|m| {
            let (status, changes, left_ddl, right_ddl) = match m.pair {
                Pair::Table(l, r) => {
                    let changes = match (l, r) {
                        (Some(l), Some(r)) => diff_tables(l, r, opts).describe(),
                        _ => Vec::new(),
                    };
                    (status_of(l.is_some(), r.is_some(), &changes), changes, l.map(|t| table_ddl(t, ld)), r.map(|t| table_ddl(t, rd)))
                }
                Pair::Code(l, r) => {
                    let changes = match (l, r) {
                        (Some(l), Some(r)) if !code_equal(l, r, opts) => vec!["Definition differs".to_owned()],
                        _ => Vec::new(),
                    };
                    let ddl = |o: &CodeObject| o.definition.clone();
                    (status_of(l.is_some(), r.is_some(), &changes), changes, l.map(ddl), r.map(ddl))
                }
            };
            match status {
                DiffStatus::Same => stats.same += 1,
                DiffStatus::Different => stats.different += 1,
                DiffStatus::LeftOnly => stats.left_only += 1,
                DiffStatus::RightOnly => stats.right_only += 1,
            }
            ObjectDiff { name: m.name.clone(), id: m.id, kind: m.kind, status, changes, left_ddl, right_ddl }
        })
        .collect();
    (objects, stats)
}

fn status_of(left: bool, right: bool, changes: &[String]) -> DiffStatus {
    match (left, right) {
        (true, false) => DiffStatus::LeftOnly,
        (false, true) => DiffStatus::RightOnly,
        _ if changes.is_empty() => DiffStatus::Same,
        _ => DiffStatus::Different,
    }
}

// ----- matching -----

/// `name`, lower-cased when names are case-insensitive.
pub(crate) fn name_key(name: &str, ignore_case: bool) -> String {
    if ignore_case {
        name.to_lowercase()
    } else {
        name.to_owned()
    }
}

/// `schema.name` key of an object (the data-compare table id).
pub(crate) fn object_key(name: &ObjectName, ignore_case: bool) -> String {
    name_key(&name.to_string(), ignore_case)
}

/// Both sides of one matched object.
pub(crate) enum Pair<'a> {
    Table(Option<&'a Table>, Option<&'a Table>),
    Code(Option<&'a CodeObject>, Option<&'a CodeObject>),
}

pub(crate) struct Matched<'a> {
    pub id: String,
    pub kind: ObjectKind,
    /// Left name when present, else right.
    pub name: &'a ObjectName,
    pub pair: Pair<'a>,
}

/// Left/right object per `(kind, name key)`.
type Sides<'a, T> = BTreeMap<(ObjectKind, String), (Option<&'a T>, Option<&'a T>)>;

/// Pairs objects by (kind, schema, name), honouring `ignore_name_case` and
/// `kinds`, sorted by kind then name.
pub(crate) fn match_objects<'a>(left: &'a Schema, right: &'a Schema, opts: &SchemaCompareOptions) -> Vec<Matched<'a>> {
    let wanted = |k: ObjectKind| opts.kinds.is_empty() || opts.kinds.contains(&k);
    let key = |kind: ObjectKind, name: &ObjectName| (kind, object_key(name, opts.ignore_name_case));
    let mut tables: Sides<'a, Table> = BTreeMap::new();
    let mut code: Sides<'a, CodeObject> = BTreeMap::new();
    if wanted(ObjectKind::Table) {
        for t in &left.tables {
            tables.entry(key(ObjectKind::Table, &t.name)).or_default().0.get_or_insert(t);
        }
        for t in &right.tables {
            tables.entry(key(ObjectKind::Table, &t.name)).or_default().1.get_or_insert(t);
        }
    }
    // A code object can't be a table; skip malformed ones rather than collide.
    let code_wanted = |o: &&CodeObject| o.kind != ObjectKind::Table && wanted(o.kind);
    for o in left.objects.iter().filter(code_wanted) {
        code.entry(key(o.kind, &o.name)).or_default().0.get_or_insert(o);
    }
    for o in right.objects.iter().filter(code_wanted) {
        code.entry(key(o.kind, &o.name)).or_default().1.get_or_insert(o);
    }
    let tables = tables.into_iter().map(|((kind, k), (l, r))| {
        let name = &l.or(r).expect("one side present").name;
        (kind, k, name, Pair::Table(l, r))
    });
    let code = code.into_iter().map(|((kind, k), (l, r))| {
        let name = &l.or(r).expect("one side present").name;
        (kind, k, name, Pair::Code(l, r))
    });
    let mut all: Vec<_> = tables.chain(code).collect();
    all.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
    all.into_iter()
        .map(|(kind, k, name, pair)| Matched { id: format!("{}:{k}", kind.label().to_lowercase()), kind, name, pair })
        .collect()
}

/// Code objects are equal when their normalised definitions are.
pub(crate) fn code_equal(l: &CodeObject, r: &CodeObject, opts: &SchemaCompareOptions) -> bool {
    let norm = |o: &CodeObject| normalize_definition(&o.definition, opts.ignore_comments, opts.ignore_whitespace);
    norm(l) == norm(r)
}

/// A matching key for one pass of [`pair_up`]; `None` = not eligible.
type KeyFn<'f, T> = &'f dyn Fn(&T) -> Option<String>;

/// Pairs items over successive passes; each pass matches still-unpaired
/// items whose keys are equal. Returns pairs (left order), left-only and
/// right-only items.
fn pair_up<'a, T>(left: &'a [T], right: &'a [T], passes: &[KeyFn<'_, T>]) -> (Vec<(&'a T, &'a T)>, Vec<&'a T>, Vec<&'a T>) {
    let mut lmatch: Vec<Option<usize>> = vec![None; left.len()];
    let mut rused = vec![false; right.len()];
    for pass in passes {
        let mut by_key: HashMap<String, Vec<usize>> = HashMap::new();
        for (j, r) in right.iter().enumerate().rev() {
            if let (false, Some(k)) = (rused[j], pass(r)) {
                by_key.entry(k).or_default().push(j);
            }
        }
        for (i, l) in left.iter().enumerate() {
            if lmatch[i].is_some() {
                continue;
            }
            if let Some(j) = pass(l).and_then(|k| by_key.get_mut(&k)).and_then(Vec::pop) {
                lmatch[i] = Some(j);
                rused[j] = true;
            }
        }
    }
    let pairs = lmatch.iter().enumerate().filter_map(|(i, j)| j.map(|j| (&left[i], &right[j]))).collect();
    let lonly = lmatch.iter().enumerate().filter(|(_, j)| j.is_none()).map(|(i, _)| &left[i]).collect();
    let ronly = rused.iter().enumerate().filter(|(_, u)| !**u).map(|(j, _)| &right[j]).collect();
    (pairs, lonly, ronly)
}

// ----- table diff -----

/// One sub-object (column, index, ...) that differs.
#[derive(Debug, Clone)]
pub(crate) enum Change<'a, T> {
    Left(&'a T),
    Right(&'a T),
    Both(&'a T, &'a T),
}

impl<'a, T> Change<'a, T> {
    pub fn left(&self) -> Option<&'a T> {
        match self {
            Change::Left(l) | Change::Both(l, _) => Some(l),
            Change::Right(_) => None,
        }
    }

    pub fn right(&self) -> Option<&'a T> {
        match self {
            Change::Right(r) | Change::Both(_, r) => Some(r),
            Change::Left(_) => None,
        }
    }
}

/// Which aspects of a matched column differ (after the compare options).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ColumnFacets {
    pub data_type: bool,
    pub nullable: bool,
    pub default: bool,
    pub identity: bool,
    pub computed: bool,
    pub collation: bool,
}

impl ColumnFacets {
    pub fn any(&self) -> bool {
        self.data_type || self.nullable || self.default || self.identity || self.computed || self.collation
    }
}

pub(crate) fn column_facets(l: &Column, r: &Column, opts: &SchemaCompareOptions) -> ColumnFacets {
    let expr = |e: &Option<String>| e.as_deref().map(normalize_expr);
    let identity = match (&l.identity, &r.identity) {
        (Some(a), Some(b)) => !opts.ignore_identity_seed && a != b,
        (a, b) => a.is_some() != b.is_some(),
    };
    let collation = !opts.ignore_collation
        && l.collation.as_deref().map(str::to_lowercase) != r.collation.as_deref().map(str::to_lowercase);
    ColumnFacets {
        // A computed column's type follows from its expression (SQL Server
        // reports an inferred one), so only the expression is compared.
        data_type: !(l.computed.is_some() && r.computed.is_some()) && normalize_type(&l.data_type) != normalize_type(&r.data_type),
        nullable: l.nullable != r.nullable,
        default: expr(&l.default) != expr(&r.default),
        identity,
        computed: expr(&l.computed) != expr(&r.computed),
        collation,
    }
}

/// Differences between two matched tables (left → right).
#[derive(Debug, Clone, Default)]
pub(crate) struct TableDiff<'a> {
    pub columns: Vec<Change<'a, Column>>,
    /// Common columns appear in a different order (and order matters).
    pub column_order: bool,
    pub primary_key: Option<Change<'a, KeyConstraint>>,
    /// Secondary indexes and UNIQUE constraints.
    pub indexes: Vec<Change<'a, Index>>,
    pub foreign_keys: Vec<Change<'a, ForeignKey>>,
    pub checks: Vec<Change<'a, CheckConstraint>>,
    /// Human-readable lines, in the same order as above.
    pub changes: Vec<String>,
}

impl TableDiff<'_> {
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }

    pub fn describe(self) -> Vec<String> {
        self.changes
    }
}

pub(crate) fn diff_tables<'a>(l: &'a Table, r: &'a Table, opts: &SchemaCompareOptions) -> TableDiff<'a> {
    let mut d = TableDiff::default();
    diff_columns(&mut d, l, r, opts);
    diff_primary_key(&mut d, l, r, opts);
    diff_indexes(&mut d, l, r, opts);
    diff_foreign_keys(&mut d, l, r, opts);
    diff_checks(&mut d, l, r, opts);
    d
}

fn opt_text(v: &Option<String>) -> String {
    v.clone().unwrap_or_else(|| "(none)".into())
}

fn identity_text(v: &Option<Identity>) -> String {
    v.as_ref().map_or_else(|| "(none)".into(), |i| format!("identity({}, {})", i.seed, i.increment))
}

fn list(names: &[String]) -> String {
    format!("({})", names.join(", "))
}

fn diff_columns<'a>(d: &mut TableDiff<'a>, l: &'a Table, r: &'a Table, opts: &SchemaCompareOptions) {
    let ic = opts.ignore_name_case;
    let by_name = |c: &Column| Some(name_key(&c.name, ic));
    let (pairs, lonly, ronly) = pair_up(&l.columns, &r.columns, &[&by_name]);
    for &(a, b) in &pairs {
        let f = column_facets(a, b, opts);
        if !f.any() {
            continue;
        }
        let mut line = |what: String| d.changes.push(format!("Column {}: {what}", a.name));
        if f.data_type {
            line(format!("{} → {}", a.data_type, b.data_type));
        }
        if f.nullable {
            let n = |c: &Column| if c.nullable { "NULL" } else { "NOT NULL" };
            line(format!("{} → {}", n(a), n(b)));
        }
        if f.default {
            line(format!("default {} → {}", opt_text(&a.default), opt_text(&b.default)));
        }
        if f.identity {
            line(format!("{} → {}", identity_text(&a.identity), identity_text(&b.identity)));
        }
        if f.computed {
            line(format!("computed {} → {}", opt_text(&a.computed), opt_text(&b.computed)));
        }
        if f.collation {
            line(format!("collation {} → {}", opt_text(&a.collation), opt_text(&b.collation)));
        }
        d.columns.push(Change::Both(a, b));
    }
    for c in lonly {
        d.changes.push(format!("Column {}: only in left", c.name));
        d.columns.push(Change::Left(c));
    }
    for c in ronly {
        d.changes.push(format!("Column {}: only in right", c.name));
        d.columns.push(Change::Right(c));
    }
    if !opts.ignore_column_order {
        let rpos = |c: &Column| r.columns.iter().position(|x| std::ptr::eq(x, c));
        let order: Vec<usize> = pairs.iter().filter_map(|(_, b)| rpos(b)).collect();
        if order.windows(2).any(|w| w[0] > w[1]) {
            d.column_order = true;
            d.changes.push("Column order differs".into());
        }
    }
}

fn cols_key(cols: &[String], ic: bool) -> String {
    cols.iter().map(|c| name_key(c, ic)).collect::<Vec<_>>().join(",")
}

fn diff_primary_key<'a>(d: &mut TableDiff<'a>, l: &'a Table, r: &'a Table, opts: &SchemaCompareOptions) {
    let ic = opts.ignore_name_case;
    match (&l.primary_key, &r.primary_key) {
        (Some(a), Some(b)) => {
            let mut lines = Vec::new();
            if cols_key(&a.columns, ic) != cols_key(&b.columns, ic) {
                lines.push(format!("Primary key: {} → {}", list(&a.columns), list(&b.columns)));
            }
            if let (Some(x), Some(y)) = (a.clustered, b.clustered) {
                if x != y {
                    let c = |v: bool| if v { "clustered" } else { "nonclustered" };
                    lines.push(format!("Primary key: {} → {}", c(x), c(y)));
                }
            }
            if let (false, Some(x), Some(y)) = (opts.ignore_constraint_names, &a.name, &b.name) {
                if name_key(x, ic) != name_key(y, ic) {
                    lines.push(format!("Primary key name: {x} → {y}"));
                }
            }
            if !lines.is_empty() {
                d.changes.extend(lines);
                d.primary_key = Some(Change::Both(a, b));
            }
        }
        (Some(a), None) => {
            d.changes.push(format!("Primary key {}: only in left", list(&a.columns)));
            d.primary_key = Some(Change::Left(a));
        }
        (None, Some(b)) => {
            d.changes.push(format!("Primary key {}: only in right", list(&b.columns)));
            d.primary_key = Some(Change::Right(b));
        }
        (None, None) => {}
    }
}

/// Index column for matching: names by case rule, expressions normalised.
fn index_col_key(c: &IndexColumn, ic: bool) -> String {
    let name = if dialect::is_plain_identifier(&c.name) { name_key(&c.name, ic) } else { normalize_expr(&c.name) };
    if c.descending {
        format!("{name} desc")
    } else {
        name
    }
}

fn index_cols_text(idx: &Index) -> String {
    let cols: Vec<String> =
        idx.columns.iter().map(|c| if c.descending { format!("{} DESC", c.name) } else { c.name.clone() }).collect();
    list(&cols)
}

/// Facet lines for a matched index pair; empty = same.
fn index_details(a: &Index, b: &Index, ic: bool) -> Vec<String> {
    let mut out = Vec::new();
    let cols = |i: &Index| i.columns.iter().map(|c| index_col_key(c, ic)).collect::<Vec<_>>();
    if cols(a) != cols(b) {
        out.push(format!("{} → {}", index_cols_text(a), index_cols_text(b)));
    }
    if a.unique != b.unique {
        let u = |v: bool| if v { "unique" } else { "non-unique" };
        out.push(format!("{} → {}", u(a.unique), u(b.unique)));
    }
    if a.constraint != b.constraint {
        let k = |v: bool| if v { "constraint" } else { "index" };
        out.push(format!("{} → {}", k(a.constraint), k(b.constraint)));
    }
    let lower = |v: &Option<String>| v.as_deref().map(str::to_lowercase);
    if lower(&a.method) != lower(&b.method) {
        out.push(format!("method {} → {}", opt_text(&a.method), opt_text(&b.method)));
    }
    let filt = |v: &Option<String>| v.as_deref().map(normalize_expr);
    if filt(&a.filter) != filt(&b.filter) {
        out.push(format!("filter {} → {}", opt_text(&a.filter), opt_text(&b.filter)));
    }
    if cols_key(&a.include, ic) != cols_key(&b.include, ic) {
        out.push(format!("include {} → {}", list(&a.include), list(&b.include)));
    }
    out
}

fn index_label(i: &Index) -> String {
    if i.constraint {
        format!("Unique constraint {}", i.name)
    } else {
        format!("Index {}", i.name)
    }
}

fn index_def_key(i: &Index, ic: bool) -> String {
    let cols: Vec<String> = i.columns.iter().map(|c| index_col_key(c, ic)).collect();
    format!(
        "{}|{}|{}|{:?}|{:?}|{}",
        cols.join(","),
        i.unique,
        i.constraint,
        i.method.as_deref().map(str::to_lowercase),
        i.filter.as_deref().map(normalize_expr),
        cols_key(&i.include, ic)
    )
}

fn diff_indexes<'a>(d: &mut TableDiff<'a>, l: &'a Table, r: &'a Table, opts: &SchemaCompareOptions) {
    let ic = opts.ignore_name_case;
    let by_name = |i: &Index| Some(name_key(&i.name, ic));
    // Index names are user-chosen; constraint names may be system-generated.
    let plain_by_name = |i: &Index| (!i.constraint).then(|| name_key(&i.name, ic));
    let by_def = |i: &Index| Some(index_def_key(i, ic));
    let passes: Vec<KeyFn<'_, Index>> =
        if opts.ignore_constraint_names { vec![&plain_by_name, &by_def, &by_name] } else { vec![&by_name] };
    let (pairs, lonly, ronly) = pair_up(&l.indexes, &r.indexes, &passes);
    for (a, b) in pairs {
        let lines = index_details(a, b, ic);
        if !lines.is_empty() {
            d.changes.extend(lines.into_iter().map(|x| format!("{}: {x}", index_label(a))));
            d.indexes.push(Change::Both(a, b));
        }
    }
    for i in lonly {
        d.changes.push(format!("{}: only in left", index_label(i)));
        d.indexes.push(Change::Left(i));
    }
    for i in ronly {
        d.changes.push(format!("{}: only in right", index_label(i)));
        d.indexes.push(Change::Right(i));
    }
}

/// Referential action with `NO ACTION` folded into "none".
fn action(a: &Option<String>) -> Option<String> {
    a.as_deref().map(|s| s.trim().to_uppercase()).filter(|s| s != "NO ACTION")
}

fn fk_def_key(fk: &ForeignKey, ic: bool) -> String {
    format!("{}|{}|{}", cols_key(&fk.columns, ic), object_key(&fk.ref_table, ic), cols_key(&fk.ref_columns, ic))
}

fn fk_details(a: &ForeignKey, b: &ForeignKey, ic: bool) -> Vec<String> {
    let mut out = Vec::new();
    if cols_key(&a.columns, ic) != cols_key(&b.columns, ic) {
        out.push(format!("columns {} → {}", list(&a.columns), list(&b.columns)));
    }
    if object_key(&a.ref_table, ic) != object_key(&b.ref_table, ic) || cols_key(&a.ref_columns, ic) != cols_key(&b.ref_columns, ic) {
        out.push(format!("references {}{} → {}{}", a.ref_table, list(&a.ref_columns), b.ref_table, list(&b.ref_columns)));
    }
    for (verb, x, y) in [("DELETE", &a.on_delete, &b.on_delete), ("UPDATE", &a.on_update, &b.on_update)] {
        let (x, y) = (action(x), action(y));
        if x != y {
            let t = |v: Option<String>| v.map_or_else(|| "(none)".into(), |s| format!("ON {verb} {s}"));
            out.push(format!("{} → {}", t(x), t(y)));
        }
    }
    out
}

fn diff_foreign_keys<'a>(d: &mut TableDiff<'a>, l: &'a Table, r: &'a Table, opts: &SchemaCompareOptions) {
    let ic = opts.ignore_name_case;
    let by_name = |f: &ForeignKey| Some(name_key(&f.name, ic));
    let by_def = |f: &ForeignKey| Some(fk_def_key(f, ic));
    let passes: Vec<KeyFn<'_, ForeignKey>> =
        if opts.ignore_constraint_names { vec![&by_def, &by_name] } else { vec![&by_name] };
    let (pairs, lonly, ronly) = pair_up(&l.foreign_keys, &r.foreign_keys, &passes);
    for (a, b) in pairs {
        let lines = fk_details(a, b, ic);
        if !lines.is_empty() {
            d.changes.extend(lines.into_iter().map(|x| format!("Foreign key {}: {x}", a.name)));
            d.foreign_keys.push(Change::Both(a, b));
        }
    }
    for f in lonly {
        d.changes.push(format!("Foreign key {}: only in left", f.name));
        d.foreign_keys.push(Change::Left(f));
    }
    for f in ronly {
        d.changes.push(format!("Foreign key {}: only in right", f.name));
        d.foreign_keys.push(Change::Right(f));
    }
}

fn diff_checks<'a>(d: &mut TableDiff<'a>, l: &'a Table, r: &'a Table, opts: &SchemaCompareOptions) {
    let ic = opts.ignore_name_case;
    let by_name = |c: &CheckConstraint| Some(name_key(&c.name, ic));
    let by_def = |c: &CheckConstraint| Some(normalize_expr(&c.expression));
    let passes: Vec<KeyFn<'_, CheckConstraint>> =
        if opts.ignore_constraint_names { vec![&by_def, &by_name] } else { vec![&by_name] };
    let (pairs, lonly, ronly) = pair_up(&l.checks, &r.checks, &passes);
    for (a, b) in pairs {
        if normalize_expr(&a.expression) != normalize_expr(&b.expression) {
            d.changes.push(format!("Check {}: {} → {}", a.name, a.expression, b.expression));
            d.checks.push(Change::Both(a, b));
        }
    }
    for c in lonly {
        d.changes.push(format!("Check {}: only in left", c.name));
        d.checks.push(Change::Left(c));
    }
    for c in ronly {
        d.changes.push(format!("Check {}: only in right", c.name));
        d.checks.push(Change::Right(c));
    }
}

// ----- DDL -----

/// Plain ANSI rendering for schemas whose engine is unknown.
pub(crate) struct AnsiDialect;

impl Dialect for AnsiDialect {
    fn engine(&self) -> Engine {
        // Never used to pick behaviour: every method keeps its ANSI default.
        Engine::Postgres
    }
}

/// The dialect a schema's SQL is written in (ANSI when unknown).
pub(crate) fn dialect_for(schema: &Schema) -> &'static dyn Dialect {
    match schema.engine {
        Some(e) => dialect::dialect(e),
        None => &AnsiDialect,
    }
}

/// CREATE TABLE plus its plain indexes and (unless inline) foreign keys,
/// as script text.
pub(crate) fn table_ddl(t: &Table, d: &dyn Dialect) -> String {
    let mut stmts = vec![d.create_table(t)];
    stmts.extend(t.indexes.iter().filter(|i| !i.constraint).map(|i| d.create_index(&t.name, i)));
    if !d.inline_foreign_keys() {
        stmts.extend(t.foreign_keys.iter().map(|fk| d.add_foreign_key(&t.name, fk)));
    }
    stmts.iter().map(|s| d.script_statement(s)).collect::<Vec<_>>().join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;

    fn run(l: &Schema, r: &Schema) -> Vec<ObjectDiff> {
        compare_schemas(l, r, &SchemaCompareOptions::default()).0
    }

    fn find<'a>(objs: &'a [ObjectDiff], id: &str) -> &'a ObjectDiff {
        objs.iter().find(|o| o.id == id).unwrap_or_else(|| panic!("no {id} in {:?}", objs.iter().map(|o| &o.id).collect::<Vec<_>>()))
    }

    fn table_mut<'a>(s: &'a mut Schema, name: &str) -> &'a mut Table {
        s.tables.iter_mut().find(|t| t.name.name == name).unwrap()
    }

    #[test]
    fn identical_schemas_are_all_same() {
        let (objs, stats) = compare_schemas(&shop(), &shop(), &SchemaCompareOptions::default());
        assert_eq!(objs.len(), 3);
        assert!(objs.iter().all(|o| o.status == DiffStatus::Same && o.changes.is_empty()));
        assert_eq!(stats, DiffStats { same: 3, ..Default::default() });
        let ids: Vec<&str> = objs.iter().map(|o| o.id.as_str()).collect();
        assert_eq!(ids, ["table:dbo.customers", "table:dbo.orders", "view:dbo.bigorders"]);
        let orders = find(&objs, "table:dbo.orders");
        assert_eq!(orders.left_ddl, orders.right_ddl);
        let ddl = orders.left_ddl.as_deref().unwrap();
        assert!(ddl.starts_with("CREATE TABLE \"dbo\".\"Orders\" (\n"), "{ddl}");
        assert!(ddl.contains(";\n\nALTER TABLE \"dbo\".\"Orders\" ADD CONSTRAINT \"FK_Order_Customer\" FOREIGN KEY"), "{ddl}");
        let customers = find(&objs, "table:dbo.customers").left_ddl.clone().unwrap();
        assert!(customers.contains("CREATE UNIQUE INDEX \"IX_Customers_Email\""), "{customers}");
        assert_eq!(find(&objs, "view:dbo.bigorders").left_ddl.as_deref(), Some(shop().objects[0].definition.as_str()));
    }

    #[test]
    fn column_changes_are_described_left_to_right() {
        let mut r = shop();
        let c = &mut table_mut(&mut r, "Customers").columns[1];
        c.data_type = "nvarchar(100)".into();
        c.nullable = false;
        c.default = Some("''".into());
        table_mut(&mut r, "Orders").columns.push(col("Notes", "text"));
        let objs = run(&shop(), &r);
        let cust = find(&objs, "table:dbo.customers");
        assert_eq!(cust.status, DiffStatus::Different);
        assert_eq!(
            cust.changes,
            ["Column Email: nvarchar(50) → nvarchar(100)", "Column Email: NULL → NOT NULL", "Column Email: default (none) → ''"]
        );
        assert_eq!(find(&objs, "table:dbo.orders").changes, ["Column Notes: only in right"]);
    }

    #[test]
    fn cosmetic_differences_are_ignored() {
        let mut r = shop();
        let orders = table_mut(&mut r, "Orders");
        orders.columns[2].default = Some("0".into());
        orders.columns[2].data_type = "DECIMAL(10, 2)".into();
        orders.checks[0].expression = "Total >= 0".into();
        let cust = table_mut(&mut r, "Customers");
        cust.columns[0].identity = Some(Identity { seed: 1000, increment: 1 });
        assert!(run(&shop(), &r).iter().all(|o| o.status == DiffStatus::Same));

        let strict = SchemaCompareOptions { ignore_identity_seed: false, ..Default::default() };
        let objs = compare_schemas(&shop(), &r, &strict).0;
        assert_eq!(find(&objs, "table:dbo.customers").changes, ["Column Id: identity(1, 1) → identity(1000, 1)"]);
    }

    #[test]
    fn computed_columns_compare_by_expression_only() {
        let with_computed = |ty: &str, expr: &str| {
            let mut c = col("Amount", ty);
            c.computed = Some(expr.into());
            schema(vec![table("T", vec![c])], vec![])
        };
        let a = with_computed("int", "[Qty]*[Price] PERSISTED");
        assert_eq!(run(&a, &with_computed("numeric(18,2)", "[qty] * [price] PERSISTED"))[0].status, DiffStatus::Same);
        let objs = run(&a, &with_computed("int", "[Qty]*[Price]"));
        assert_eq!(objs[0].changes, ["Column Amount: computed [Qty]*[Price] PERSISTED → [Qty]*[Price]"]);
    }

    #[test]
    fn system_constraint_names_match_by_definition() {
        let mut r = shop();
        let orders = table_mut(&mut r, "Orders");
        orders.primary_key.as_mut().unwrap().name = Some("PK__Orders__3214EC07".into());
        orders.foreign_keys[0].name = "FK__Orders__Custom__38996AB5".into();
        orders.checks[0].name = "CK__Orders__Total__398D8EEE".into();
        assert!(run(&shop(), &r).iter().all(|o| o.status == DiffStatus::Same));

        let strict = SchemaCompareOptions { ignore_constraint_names: false, ..Default::default() };
        let objs = compare_schemas(&shop(), &r, &strict).0;
        let changes = &find(&objs, "table:dbo.orders").changes;
        assert!(changes.contains(&"Primary key name: PK_Orders → PK__Orders__3214EC07".to_owned()), "{changes:?}");
        assert!(changes.contains(&"Foreign key FK_Order_Customer: only in left".to_owned()), "{changes:?}");
        assert!(changes.contains(&"Check CK__Orders__Total__398D8EEE: only in right".to_owned()), "{changes:?}");
    }

    #[test]
    fn index_differences() {
        let mut r = shop();
        table_mut(&mut r, "Customers").indexes.clear();
        let objs = run(&shop(), &r);
        assert_eq!(find(&objs, "table:dbo.customers").changes, ["Index IX_Customers_Email: only in left"]);

        table_mut(&mut r, "Customers").indexes.push(index("IX_Customers_Email", &["Email"], false));
        let objs = run(&shop(), &r);
        assert_eq!(find(&objs, "table:dbo.customers").changes, ["Index IX_Customers_Email: unique → non-unique"]);

        // Renamed but identical: matched by definition.
        table_mut(&mut r, "Customers").indexes[0] = index("UX_Email", &["Email"], true);
        assert_eq!(find(&run(&shop(), &r), "table:dbo.customers").status, DiffStatus::Same);
        let strict = SchemaCompareOptions { ignore_constraint_names: false, ..Default::default() };
        assert_eq!(find(&compare_schemas(&shop(), &r, &strict).0, "table:dbo.customers").changes.len(), 2);
    }

    #[test]
    fn foreign_key_action_change() {
        let mut l = shop();
        table_mut(&mut l, "Orders").foreign_keys[0].on_delete = Some("CASCADE".into());
        let mut r = shop();
        table_mut(&mut r, "Orders").foreign_keys[0].on_delete = Some("NO ACTION".into());
        let objs = run(&l, &r);
        assert_eq!(find(&objs, "table:dbo.orders").changes, ["Foreign key FK_Order_Customer: ON DELETE CASCADE → (none)"]);
        table_mut(&mut r, "Orders").foreign_keys[0].ref_columns = vec!["Email".into()];
        table_mut(&mut r, "Orders").foreign_keys[0].on_delete = Some("CASCADE".into());
        let objs = run(&l, &r);
        assert_eq!(
            find(&objs, "table:dbo.orders").changes,
            ["Foreign key FK_Order_Customer: references dbo.Customers(Id) → dbo.Customers(Email)"]
        );
    }

    #[test]
    fn primary_key_and_column_order() {
        let mut r = shop();
        let orders = table_mut(&mut r, "Orders");
        orders.primary_key.as_mut().unwrap().columns.push("CustomerId".into());
        orders.columns.swap(0, 1);
        let objs = run(&shop(), &r);
        assert_eq!(find(&objs, "table:dbo.orders").changes, ["Primary key: (Id) → (Id, CustomerId)"]);
        let ordered = SchemaCompareOptions { ignore_column_order: false, ..Default::default() };
        let objs = compare_schemas(&shop(), &r, &ordered).0;
        assert_eq!(find(&objs, "table:dbo.orders").changes[0], "Column order differs");
    }

    #[test]
    fn name_case_sensitivity() {
        let mut r = shop();
        let cust = table_mut(&mut r, "Customers");
        cust.name = ObjectName::new("DBO", "CUSTOMERS");
        cust.columns[1].name = "EMAIL".into();
        cust.indexes[0].columns[0].name = "email".into();
        let objs = run(&shop(), &r);
        let c = find(&objs, "table:dbo.customers");
        assert_eq!((c.status, c.name.name.as_str()), (DiffStatus::Same, "Customers"));

        let strict = SchemaCompareOptions { ignore_name_case: false, ..Default::default() };
        let (objs, stats) = compare_schemas(&shop(), &r, &strict);
        assert_eq!(find(&objs, "table:dbo.Customers").status, DiffStatus::LeftOnly);
        assert_eq!(find(&objs, "table:DBO.CUSTOMERS").status, DiffStatus::RightOnly);
        assert_eq!((stats.left_only, stats.right_only), (1, 1));
    }

    #[test]
    fn code_object_normalisation() {
        let view = |def: &str| schema(vec![], vec![code(ObjectKind::View, "V", def)]);
        let base = view("CREATE VIEW V AS SELECT a, '-- not a comment' AS s FROM t");
        let spaced = view("create or replace view V AS\n    SELECT a,\n           '-- not a comment' AS s\n    FROM t;");
        let commented = view("CREATE VIEW V AS SELECT a, /* why */ '-- not a comment' AS s -- note\nFROM t");
        let literal = view("CREATE VIEW V AS SELECT a, '-- another' AS s FROM t");
        let status = |l: &Schema, r: &Schema, o: &SchemaCompareOptions| compare_schemas(l, r, o).0[0].status;
        let opts = SchemaCompareOptions::default();
        let with_comments = SchemaCompareOptions { ignore_comments: true, ..Default::default() };
        let exact = SchemaCompareOptions { ignore_whitespace: false, ..Default::default() };
        assert_eq!(status(&base, &spaced, &opts), DiffStatus::Same);
        assert_eq!(status(&base, &spaced, &exact), DiffStatus::Different);
        assert_eq!(status(&base, &commented, &opts), DiffStatus::Different);
        assert_eq!(status(&base, &commented, &with_comments), DiffStatus::Same);
        assert_eq!(status(&base, &literal, &with_comments), DiffStatus::Different);
        assert_eq!(compare_schemas(&base, &literal, &opts).0[0].changes, ["Definition differs"]);
    }

    #[test]
    fn sorted_by_kind_then_name_and_filtered() {
        let mut l = shop();
        l.objects.push(code(ObjectKind::Procedure, "Archive", "CREATE PROCEDURE Archive AS SELECT 1"));
        l.objects.push(trigger("TR_Orders", "Orders", "CREATE TRIGGER TR_Orders ON Orders AFTER INSERT AS SELECT 1"));
        l.objects.push(code(ObjectKind::View, "AllOrders", "CREATE VIEW AllOrders AS SELECT * FROM Orders"));
        let (objs, stats) = compare_schemas(&l, &shop(), &SchemaCompareOptions::default());
        let ids: Vec<&str> = objs.iter().map(|o| o.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "table:dbo.customers",
                "table:dbo.orders",
                "view:dbo.allorders",
                "view:dbo.bigorders",
                "procedure:dbo.archive",
                "trigger:dbo.tr_orders"
            ]
        );
        assert_eq!((stats.same, stats.left_only), (3, 3));
        assert!(objs[2].right_ddl.is_none() && objs[2].left_ddl.is_some());
        let views = SchemaCompareOptions { kinds: vec![ObjectKind::View], ..Default::default() };
        assert_eq!(compare_schemas(&l, &shop(), &views).0.len(), 2);
    }
}
