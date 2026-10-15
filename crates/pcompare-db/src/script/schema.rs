//! Schema sync planning: turns the structured schema diff (source = "left",
//! target = "right") into DDL for the target dialect, grouped into phases
//! so drops run before the objects they block are changed and creates run
//! after everything they reference exists.

use super::{topo_order, ScriptOptions};
use crate::dialect::Dialect;
use crate::model::*;
use crate::schema_diff::{self as sd, Change, ColumnFacets, Pair, SchemaCompareOptions, TableDiff};
use std::collections::HashSet;

pub(super) struct Plan {
    pub statements: Vec<String>,
    pub warnings: Vec<String>,
}

/// Statements per phase; the script is these vectors concatenated in
/// declaration order.
#[derive(Default)]
struct Phases {
    drop_triggers: Vec<String>,
    /// Views, functions and procedures, dependents first.
    drop_routines: Vec<String>,
    drop_fks: Vec<String>,
    /// Indexes, UNIQUE constraints and checks.
    drop_indexes: Vec<String>,
    drop_pks: Vec<String>,
    /// Children before parents.
    drop_tables: Vec<String>,
    drop_sequences: Vec<String>,
    /// Sequence creates/replaces, before any table that may use them.
    sequences: Vec<String>,
    /// Parents before children.
    create_tables: Vec<String>,
    add_columns: Vec<String>,
    alter_columns: Vec<String>,
    drop_columns: Vec<String>,
    rebuilds: Vec<String>,
    add_pks: Vec<String>,
    add_indexes: Vec<String>,
    add_fks: Vec<String>,
    /// Views, functions and procedures, dependencies first.
    routines: Vec<String>,
    triggers: Vec<String>,
}

impl Phases {
    fn into_statements(self) -> Vec<String> {
        [
            self.drop_triggers,
            self.drop_routines,
            self.drop_fks,
            self.drop_indexes,
            self.drop_pks,
            self.drop_tables,
            self.drop_sequences,
            self.sequences,
            self.create_tables,
            self.add_columns,
            self.alter_columns,
            self.drop_columns,
            self.rebuilds,
            self.add_pks,
            self.add_indexes,
            self.add_fks,
            self.routines,
            self.triggers,
        ]
        .concat()
    }
}

/// A table present on both sides that needs changes.
struct Changed<'a> {
    s: &'a Table,
    t: &'a Table,
    diff: TableDiff<'a>,
    /// Per changed column: in-place statements (empty for collation-only
    /// changes the dialect can't express).
    alters: Vec<(&'a Column, &'a Column, ColumnFacets, Vec<String>)>,
    rebuild: bool,
}

struct Planner<'a> {
    cmp: &'a SchemaCompareOptions,
    d: &'a dyn Dialect,
    ph: Phases,
    warnings: Vec<String>,
    /// `(table key, fk name key)` of FKs already dropped.
    fks_dropped: HashSet<(String, String)>,
}

pub(super) fn plan(source: &Schema, target: &Schema, cmp: &SchemaCompareOptions, opts: &ScriptOptions, d: &dyn Dialect) -> Plan {
    let mut new_tables = Vec::new();
    let mut dropped_tables = Vec::new();
    let mut changed = Vec::new();
    let mut new_code = Vec::new();
    let mut dropped_code = Vec::new();
    let mut changed_code = Vec::new();
    for m in sd::match_objects(source, target, cmp) {
        if !opts.ids.is_empty() && !opts.ids.contains(&m.id) {
            continue;
        }
        match m.pair {
            Pair::Table(Some(s), None) => new_tables.push(s),
            Pair::Table(None, Some(t)) if opts.include_drops => dropped_tables.push(t),
            Pair::Table(Some(s), Some(t)) => {
                let diff = sd::diff_tables(s, t, cmp);
                if !diff.is_empty() {
                    changed.push(Changed { s, t, diff, alters: Vec::new(), rebuild: false });
                }
            }
            Pair::Code(Some(s), None) => new_code.push(s),
            Pair::Code(None, Some(t)) if opts.include_drops => dropped_code.push(t),
            Pair::Code(Some(s), Some(t)) if !sd::code_equal(s, t, cmp) => changed_code.push((s, t)),
            _ => {}
        }
    }

    let mut p = Planner { cmp, d, ph: Phases::default(), warnings: Vec::new(), fks_dropped: HashSet::new() };
    for c in &mut changed {
        p.prepare(c);
    }
    if !d.inline_foreign_keys() {
        p.drop_blocking_fks(target, &changed, &dropped_tables);
    }
    for c in changed.iter().filter(|c| !c.rebuild) {
        p.alter_in_place(c);
    }
    for c in changed.iter().filter(|c| c.rebuild) {
        let skip: Vec<&CodeObject> = dropped_code.iter().copied().chain(changed_code.iter().map(|(_, t)| *t)).collect();
        let triggers = target
            .objects
            .iter()
            .filter(|o| o.kind == ObjectKind::Trigger && o.table.as_ref().is_some_and(|tn| p.key(tn) == p.key(&c.t.name)))
            .filter(|o| !skip.iter().any(|x| std::ptr::eq(*x, *o)))
            .collect();
        p.rebuild(c.s, c.t, triggers);
    }
    p.drop_tables(&dropped_tables);
    p.create_tables(&new_tables);
    p.code_objects(&new_code, &dropped_code, &changed_code);
    Plan { statements: p.ph.into_statements(), warnings: p.warnings }
}

/// True when the column can be added in place on an engine that can't
/// alter columns (SQLite: no NOT NULL without default, no stored generated
/// columns, no identity).
fn addable(c: &Column) -> bool {
    c.computed.is_none() && c.identity.is_none() && (c.nullable || c.default.is_some())
}

impl<'a> Planner<'a> {
    fn key(&self, name: &ObjectName) -> String {
        sd::object_key(name, self.cmp.ignore_name_case)
    }

    fn col_key(&self, name: &str) -> String {
        sd::name_key(name, self.cmp.ignore_name_case)
    }

    /// Computes in-place column statements and decides whether the table
    /// must be rebuilt instead.
    fn prepare(&mut self, c: &mut Changed<'a>) {
        let d = self.d;
        let diff = &c.diff;
        let constraint_index = diff.indexes.iter().any(|ch| ch.left().or(ch.right()).is_some_and(|i| i.constraint));
        c.rebuild = (d.inline_foreign_keys() && !diff.foreign_keys.is_empty())
            || (!d.supports_alter_column()
                && (diff.primary_key.is_some()
                    || !diff.checks.is_empty()
                    || constraint_index
                    || diff.columns.iter().any(|ch| !matches!(ch, Change::Left(col) if addable(col)))));
        if c.rebuild {
            return;
        }
        for ch in &diff.columns {
            let Change::Both(sc, tc) = *ch else { continue };
            let f = sd::column_facets(sc, tc, self.cmp);
            let stmts = if f.computed {
                // Computed columns hold no data: drop and re-add.
                let mut to = sc.clone();
                to.name = tc.name.clone();
                vec![d.drop_column(&c.t.name, &tc.name), d.add_column(&c.t.name, &to)]
            } else {
                d.alter_column(&c.t.name, tc, &altered(sc, tc, f))
            };
            if stmts.is_empty() && (f.data_type || f.nullable || f.default) {
                c.rebuild = true;
                c.alters.clear();
                return;
            }
            c.alters.push((sc, tc, f, stmts));
        }
    }

    fn drop_fk(&mut self, table: &ObjectName, fk: &ForeignKey) {
        if self.fks_dropped.insert((self.key(table), self.col_key(&fk.name))) {
            self.ph.drop_fks.push(self.d.drop_foreign_key(table, fk));
        }
    }

    /// Drops (and re-adds afterwards) unchanged FKs that would block the
    /// script: FKs referencing tables being dropped or rebuilt, or columns,
    /// primary keys and unique indexes being altered or dropped.
    fn drop_blocking_fks(&mut self, target: &Schema, changed: &[Changed<'a>], dropped: &[&Table]) {
        let mut touched_tables: HashSet<String> = dropped.iter().map(|t| self.key(&t.name)).collect();
        let mut touched: HashSet<(String, String)> = HashSet::new();
        let mut gone: HashSet<(String, String)> = HashSet::new();
        // Columns of indexes being dropped/changed: MySQL won't drop an index backing an FK.
        let mut indexed: HashSet<(String, String)> = HashSet::new();
        let mut handled: Vec<&ForeignKey> = Vec::new();
        let ic = self.cmp.ignore_name_case;
        for c in changed {
            let tk = self.key(&c.t.name);
            if c.rebuild {
                touched_tables.insert(tk);
                continue;
            }
            let touch = |cols: &mut dyn Iterator<Item = &String>, set: &mut HashSet<(String, String)>| {
                for col in cols {
                    set.insert((tk.clone(), sd::name_key(col, ic)));
                }
            };
            for (_, tc, f, _) in &c.alters {
                if f.data_type || f.nullable || f.computed {
                    touch(&mut std::iter::once(&tc.name), &mut touched);
                }
            }
            for ch in &c.diff.columns {
                if let Change::Right(tc) = ch {
                    touch(&mut std::iter::once(&tc.name), &mut touched);
                    touch(&mut std::iter::once(&tc.name), &mut gone);
                }
            }
            if let Some(pk) = c.diff.primary_key.as_ref().and_then(Change::right) {
                touch(&mut pk.columns.iter(), &mut touched);
            }
            for idx in c.diff.indexes.iter().filter_map(Change::right) {
                touch(&mut idx.columns.iter().map(|ic| &ic.name), &mut indexed);
                if idx.unique {
                    touch(&mut idx.columns.iter().map(|ic| &ic.name), &mut touched);
                }
            }
            handled.extend(c.diff.foreign_keys.iter().filter_map(Change::right));
        }
        let hit = |set: &HashSet<(String, String)>, table: &str, cols: &[String]| {
            cols.iter().any(|col| set.contains(&(table.to_owned(), sd::name_key(col, ic))))
        };
        for t in &target.tables {
            let tk = self.key(&t.name);
            if touched_tables.contains(&tk) {
                continue; // dropped or rebuilt: its own FKs go with it
            }
            for fk in &t.foreign_keys {
                if handled.iter().any(|h| std::ptr::eq(*h, fk)) {
                    continue;
                }
                let rk = self.key(&fk.ref_table);
                let blocks = touched_tables.contains(&rk)
                    || hit(&touched, &rk, &fk.ref_columns)
                    || hit(&touched, &tk, &fk.columns)
                    || hit(&indexed, &tk, &fk.columns);
                if !blocks {
                    continue;
                }
                self.drop_fk(&t.name, fk);
                let still_valid = !dropped.iter().any(|x| self.key(&x.name) == rk)
                    && !hit(&gone, &rk, &fk.ref_columns)
                    && !hit(&gone, &tk, &fk.columns);
                if still_valid {
                    self.ph.add_fks.push(self.d.add_foreign_key(&t.name, fk));
                }
            }
        }
    }

    fn alter_in_place(&mut self, c: &Changed<'a>) {
        let d = self.d;
        let tn = &c.t.name;
        for ch in &c.diff.columns {
            match *ch {
                Change::Left(sc) => self.ph.add_columns.push(d.add_column(tn, sc)),
                Change::Right(tc) => self.ph.drop_columns.push(d.drop_column(tn, &tc.name)),
                Change::Both(..) => {}
            }
        }
        let mut altered_cols = HashSet::new();
        for (_, tc, f, stmts) in &c.alters {
            if f.identity {
                self.warnings.push(format!("Identity change on {tn}.{} is not scripted; apply it manually", tc.name));
            }
            if stmts.is_empty() && f.collation {
                self.warnings.push(format!("Collation change on {tn}.{} is not scripted for this engine", tc.name));
            }
            if f.data_type || f.nullable {
                altered_cols.insert(self.col_key(&tc.name));
            }
            self.ph.alter_columns.extend(stmts.iter().cloned());
        }
        if c.diff.column_order {
            self.warnings.push(format!("Column order of {tn} differs; reordering columns is not scripted"));
        }
        let ic = self.cmp.ignore_name_case;
        let covers = |cols: &mut dyn Iterator<Item = &String>| {
            let mut any = false;
            for n in cols {
                any |= altered_cols.contains(&sd::name_key(n, ic));
            }
            any
        };

        // Primary key: changed, or unchanged but covering an altered column.
        match &c.diff.primary_key {
            Some(ch) => {
                if let Some(tpk) = ch.right() {
                    self.ph.drop_pks.push(d.drop_primary_key(tn, tpk));
                }
                if let Some(spk) = ch.left() {
                    self.ph.add_pks.push(d.add_primary_key(tn, spk));
                }
            }
            None => {
                if let Some(pk) = c.t.primary_key.as_ref().filter(|pk| covers(&mut pk.columns.iter())) {
                    self.ph.drop_pks.push(d.drop_primary_key(tn, pk));
                    self.ph.add_pks.push(d.add_primary_key(tn, pk));
                }
            }
        }
        for ch in &c.diff.indexes {
            if let Some(ti) = ch.right() {
                self.ph.drop_indexes.push(d.drop_index(tn, ti));
            }
            if let Some(si) = ch.left() {
                self.ph.add_indexes.push(d.create_index(tn, si));
            }
        }
        // Engines refuse to alter a column an index depends on.
        let in_diff = |i: &Index| c.diff.indexes.iter().any(|ch| ch.right().is_some_and(|x| std::ptr::eq(x, i)));
        for idx in c.t.indexes.iter().filter(|i| !in_diff(i)) {
            if covers(&mut idx.columns.iter().map(|ic| &ic.name)) {
                self.ph.drop_indexes.push(d.drop_index(tn, idx));
                self.ph.add_indexes.push(d.create_index(tn, idx));
            }
        }
        for ch in &c.diff.checks {
            if let Some(tc) = ch.right() {
                self.ph.drop_indexes.push(d.drop_check(tn, tc));
            }
            if let Some(sc) = ch.left() {
                self.ph.add_indexes.push(d.add_check(tn, sc));
            }
        }
        for ch in &c.diff.foreign_keys {
            if let Some(tfk) = ch.right() {
                self.drop_fk(tn, tfk);
            }
            if let Some(sfk) = ch.left() {
                self.ph.add_fks.push(d.add_foreign_key(tn, sfk));
            }
        }
    }

    /// Recreates `t` with the source definition, copying the common
    /// columns, for changes the engine can't make in place (SQLite).
    fn rebuild(&mut self, s: &Table, t: &Table, triggers: Vec<&CodeObject>) {
        let d = self.d;
        self.warnings.push(format!("{} is rebuilt (copied into a new table) because the engine can't alter it in place", t.name));
        let tmp = ObjectName::new(t.name.schema.clone(), format!("_pcompare_new_{}", t.name.name));
        let mut new_t = s.clone();
        new_t.name = tmp.clone();
        let pairs: Vec<(&Column, &Column)> = s
            .columns
            .iter()
            .filter(|sc| sc.computed.is_none())
            .filter_map(|sc| {
                let tc = t.columns.iter().find(|tc| self.col_key(&tc.name) == self.col_key(&sc.name))?;
                tc.computed.is_none().then_some((sc, tc))
            })
            .collect();
        let tables = std::slice::from_ref(&t.name);
        let mut out = d.disable_foreign_keys(tables);
        out.push(d.create_table(&new_t));
        if !pairs.is_empty() {
            let into: Vec<String> = pairs.iter().map(|(sc, _)| sc.name.clone()).collect();
            let from: Vec<String> = pairs.iter().map(|(_, tc)| tc.name.clone()).collect();
            let identity = pairs.iter().any(|(sc, _)| sc.identity.is_some());
            let on = if identity { d.identity_insert(&tmp, true) } else { None };
            let wrapped = on.is_some();
            out.extend(on);
            out.push(format!(
                "INSERT INTO {} ({}) SELECT {} FROM {}",
                d.qualified(&tmp),
                d.ident_list(&into),
                d.ident_list(&from),
                d.qualified(&t.name)
            ));
            if wrapped {
                out.extend(d.identity_insert(&tmp, false));
            }
        }
        out.push(d.drop_table(&t.name));
        out.extend(d.rebuild_rename(&tmp, &t.name.name));
        out.extend(s.indexes.iter().filter(|i| !i.constraint).map(|i| d.create_index(&t.name, i)));
        out.extend(triggers.iter().flat_map(|tr| d.create_object(tr)));
        out.extend(d.enable_foreign_keys(tables));
        self.ph.rebuilds.extend(out);
        if !d.inline_foreign_keys() {
            self.ph.add_fks.extend(s.foreign_keys.iter().map(|fk| d.add_foreign_key(&t.name, fk)));
        }
    }

    /// Indices of `tables` in FK order (referenced tables first).
    fn fk_order(&self, tables: &[&Table]) -> (Vec<usize>, bool) {
        let keys: Vec<String> = tables.iter().map(|t| self.key(&t.name)).collect();
        topo_order(tables.len(), |i| {
            tables[i]
                .foreign_keys
                .iter()
                .filter_map(|fk| {
                    let rk = self.key(&fk.ref_table);
                    keys.iter().position(|k| *k == rk)
                })
                .collect()
        })
    }

    fn drop_tables(&mut self, tables: &[&Table]) {
        let (order, cycle) = self.fk_order(tables);
        if cycle && !self.d.inline_foreign_keys() {
            // Break the cycle: drop FKs between the doomed tables first.
            let keys: HashSet<String> = tables.iter().map(|t| self.key(&t.name)).collect();
            let inner: Vec<(&ObjectName, &ForeignKey)> = tables
                .iter()
                .flat_map(|t| t.foreign_keys.iter().map(move |fk| (&t.name, fk)))
                .filter(|(_, fk)| keys.contains(&self.key(&fk.ref_table)))
                .collect();
            for (name, fk) in inner {
                self.drop_fk(name, fk);
            }
        }
        for &i in order.iter().rev() {
            self.ph.drop_tables.push(self.d.drop_table(&tables[i].name));
        }
    }

    fn create_tables(&mut self, tables: &[&Table]) {
        let d = self.d;
        let (order, _) = self.fk_order(tables);
        for i in order {
            let t = tables[i];
            self.ph.create_tables.push(d.create_table(t));
            self.ph.add_indexes.extend(t.indexes.iter().filter(|i| !i.constraint).map(|i| d.create_index(&t.name, i)));
            if !d.inline_foreign_keys() {
                self.ph.add_fks.extend(t.foreign_keys.iter().map(|fk| d.add_foreign_key(&t.name, fk)));
            }
        }
    }

    fn code_objects(&mut self, new: &[&CodeObject], dropped: &[&CodeObject], changed: &[(&CodeObject, &CodeObject)]) {
        let d = self.d;
        let mut routine_drops: Vec<(&CodeObject, String)> = Vec::new();
        let mut routine_creates: Vec<(&CodeObject, Vec<String>)> = Vec::new();
        for &t in dropped {
            let stmt = d.drop_object(t);
            match t.kind {
                ObjectKind::Trigger => self.ph.drop_triggers.push(stmt),
                ObjectKind::Sequence => self.ph.drop_sequences.push(stmt),
                _ => routine_drops.push((t, stmt)),
            }
        }
        for &(s, t) in changed {
            let mut stmts = d.replace_object(t, s);
            if s.kind == ObjectKind::Sequence {
                self.ph.sequences.extend(stmts);
                continue;
            }
            // A drop + create replacement drops early, so table changes
            // aren't blocked by the old definition's dependencies.
            if stmts.first() == Some(&d.drop_object(t)) {
                let drop = stmts.remove(0);
                match t.kind {
                    ObjectKind::Trigger => self.ph.drop_triggers.push(drop),
                    _ => routine_drops.push((t, drop)),
                }
            }
            match s.kind {
                ObjectKind::Trigger => self.ph.triggers.extend(stmts),
                _ => routine_creates.push((s, stmts)),
            }
        }
        for &s in new {
            let stmts = d.create_object(s);
            match s.kind {
                ObjectKind::Trigger => self.ph.triggers.extend(stmts),
                ObjectKind::Sequence => self.ph.sequences.extend(stmts),
                _ => routine_creates.push((s, stmts)),
            }
        }
        let rank = |k: ObjectKind| match k {
            ObjectKind::View => 0,
            ObjectKind::Function => 1,
            _ => 2,
        };
        routine_drops.sort_by_key(|(o, _)| rank(o.kind));
        let (order, _) = mention_order(&routine_drops.iter().map(|(o, _)| *o).collect::<Vec<_>>());
        for &i in order.iter().rev() {
            self.ph.drop_routines.push(routine_drops[i].1.clone());
        }
        routine_creates.sort_by_key(|(o, _)| rank(o.kind));
        let (order, _) = mention_order(&routine_creates.iter().map(|(o, _)| *o).collect::<Vec<_>>());
        for i in order {
            self.ph.routines.extend(routine_creates[i].1.iter().cloned());
        }
    }
}

/// Best-effort dependency order of code objects: one whose definition
/// mentions another's name comes after it.
fn mention_order(objs: &[&CodeObject]) -> (Vec<usize>, bool) {
    let names: Vec<String> = objs.iter().map(|o| o.name.name.to_lowercase()).collect();
    let idents: Vec<HashSet<String>> = objs.iter().map(|o| sd::identifiers(&o.definition)).collect();
    topo_order(objs.len(), |i| (0..objs.len()).filter(|&j| j != i && names[j] != names[i] && idents[i].contains(&names[j])).collect())
}

/// `to` for `alter_column`: the target column with only the facets that
/// really differ (per the compare options) taken from the source, so
/// cosmetic differences (`((0))` vs `0`) don't produce statements.
fn altered(sc: &Column, tc: &Column, f: ColumnFacets) -> Column {
    let mut to = tc.clone();
    if f.data_type {
        to.data_type = sc.data_type.clone();
    }
    if f.nullable {
        to.nullable = sc.nullable;
    }
    if f.default {
        to.default = sc.default.clone();
    }
    if f.collation {
        to.collation = sc.collation.clone();
    }
    to
}
