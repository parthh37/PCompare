//! Sync scripts: the statements that make the target side match the source.
//!
//! Both generators are pure: [`schema_script`] re-runs the schema diff with
//! source/target roles and orders DDL so each statement's dependencies
//! exist ([`schema`]); [`data_script`] turns row diffs into DML ([`data`]).
//! Rendering always uses the *target* engine's dialect.

mod data;
mod schema;

use crate::data_diff::TableDataDiff;
use crate::dialect::Dialect;
use crate::model::*;
use crate::schema_diff::{dialect_for, SchemaCompareOptions};
use crate::value::Value;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

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
    /// Data: table id → keys of the only rows to sync in that table, each key
    /// as the row's display values (`null` = SQL NULL, as `DataRow.key`
    /// serializes). Tables without an entry sync every differing row.
    pub rows: HashMap<String, Vec<Vec<Option<String>>>>,
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
            rows: HashMap::new(),
        }
    }
}

/// Drops the rows of `diff` that `opts.rows` doesn't pick for its table.
pub fn keep_picked_rows(diff: &mut TableDataDiff, opts: &ScriptOptions) {
    let Some(keys) = opts.rows.get(&diff.summary.id) else { return };
    let picked: HashSet<&[Option<String>]> = keys.iter().map(Vec::as_slice).collect();
    diff.rows.retain(|row| {
        let key: Vec<Option<String>> = row.key.iter().map(Value::display_text).collect();
        picked.contains(key.as_slice())
    });
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

/// `(source, target)` for a direction.
fn roles<'a>(left: &'a Schema, right: &'a Schema, direction: SyncDirection) -> (&'a Schema, &'a Schema) {
    match direction {
        SyncDirection::LeftToRight => (left, right),
        SyncDirection::RightToLeft => (right, left),
    }
}

/// Statements turning the target schema into the source schema for the
/// selected objects, dependency-ordered.
pub fn schema_script(left: &Schema, right: &Schema, cmp: &SchemaCompareOptions, opts: &ScriptOptions) -> SyncScript {
    let target = roles(left, right, opts.direction).1;
    schema_script_in(left, right, cmp, opts, dialect_for(target))
}

/// [`schema_script`] rendered through an explicit dialect.
fn schema_script_in(left: &Schema, right: &Schema, cmp: &SchemaCompareOptions, opts: &ScriptOptions, d: &dyn Dialect) -> SyncScript {
    let (source, target) = roles(left, right, opts.direction);
    let mut warnings = Vec::new();
    if let (Some(s), Some(t)) = (source.engine, target.engine) {
        if s != t {
            warnings.push(format!(
                "Source is {} but target is {}: column types, defaults and object bodies are copied verbatim and may need editing",
                s.label(),
                t.label()
            ));
        }
    }
    let plan = schema::plan(source, target, cmp, opts, d);
    warnings.extend(plan.warnings);
    let mut transactional = opts.transactional;
    if transactional && !d.transactional_ddl() && !plan.statements.is_empty() {
        let engine = target.engine.map_or("The target engine", Engine::label);
        warnings.push(format!("{engine} commits each DDL statement implicitly, so this script can't be rolled back"));
        transactional = false;
    }
    finish(d, target.engine, "Schema", opts.direction, plan.statements, warnings, transactional)
}

/// INSERT/UPDATE/DELETE statements applying `diffs` to the target.
/// `target_schema` provides identity columns and FK order for the target.
pub fn data_script(diffs: &[TableDataDiff], target_schema: &Schema, opts: &ScriptOptions) -> SyncScript {
    data_script_in(diffs, target_schema, opts, dialect_for(target_schema))
}

/// [`data_script`] rendered through an explicit dialect.
fn data_script_in(diffs: &[TableDataDiff], target_schema: &Schema, opts: &ScriptOptions, d: &dyn Dialect) -> SyncScript {
    let (statements, warnings) = data::statements(diffs, target_schema, opts, d);
    finish(d, target_schema.engine, "Data", opts.direction, statements, warnings, opts.transactional)
}

/// Assembles the script text: header comment, optional BEGIN/COMMIT, and
/// every statement with its terminator, separated by blank lines.
fn finish(
    d: &dyn Dialect,
    target_engine: Option<Engine>,
    kind: &str,
    direction: SyncDirection,
    statements: Vec<String>,
    warnings: Vec<String>,
    transactional: bool,
) -> SyncScript {
    let direction = match direction {
        SyncDirection::LeftToRight => "left → right (makes the right database match the left)",
        SyncDirection::RightToLeft => "right → left (makes the left database match the right)",
    };
    let mut header = vec![
        format!("{kind} sync script generated by PCompare"),
        format!("Direction: {direction}"),
        format!("Target engine: {}", target_engine.map_or("unknown (ANSI SQL)", Engine::label)),
        format!("Statements: {}", statements.len()),
    ];
    header.extend(warnings.iter().map(|w| format!("Warning: {w}")));
    let mut text = d.comment(&header.join("\n"));
    text.push_str("\n\n");
    if statements.is_empty() {
        let note = if warnings.is_empty() { "No differences" } else { "No executable statements; see the warnings above" };
        text.push_str(&d.comment(note));
    } else {
        let mut parts = Vec::with_capacity(statements.len() + 2);
        if transactional {
            parts.push(d.script_statement(d.begin_transaction()));
        }
        parts.extend(statements.iter().map(|s| d.script_statement(s)));
        if transactional {
            parts.push(d.script_statement(d.commit()));
        }
        text.push_str(&parts.join("\n\n"));
    }
    text.push('\n');
    SyncScript { target_engine, statements, text, warnings, transactional }
}

/// Stable topological order: `deps(i)` lists items that must precede `i`.
/// Returns the order and whether a cycle forced some items to the end (in
/// their original order).
fn topo_order(n: usize, deps: impl Fn(usize) -> Vec<usize>) -> (Vec<usize>, bool) {
    let deps: Vec<Vec<usize>> = (0..n).map(|i| deps(i).into_iter().filter(|&j| j != i && j < n).collect()).collect();
    let mut done = vec![false; n];
    let mut order = Vec::with_capacity(n);
    // Restart from the lowest index after each pick to keep the order stable.
    while let Some(i) = (0..n).find(|&i| !done[i] && deps[i].iter().all(|&j| done[j])) {
        done[i] = true;
        order.push(i);
    }
    let cycle = order.len() < n;
    order.extend((0..n).filter(|&i| !done[i]));
    (order, cycle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data_diff::{DataRow, RowStatus, TableDataSummary};
    use crate::schema_diff::fixtures::*;
    use crate::schema_diff::AnsiDialect;
    use crate::value::Value::{self, *};

    /// SQLite-like: no ALTER COLUMN, FKs only inside CREATE TABLE.
    struct Lite;
    impl Dialect for Lite {
        fn engine(&self) -> Engine {
            Engine::Sqlite
        }
        fn supports_alter_column(&self) -> bool {
            false
        }
        fn inline_foreign_keys(&self) -> bool {
            true
        }
        fn alter_column(&self, _: &ObjectName, _: &Column, _: &Column) -> Vec<String> {
            Vec::new()
        }
        fn disable_foreign_keys(&self, _: &[ObjectName]) -> Vec<String> {
            vec!["PRAGMA defer_foreign_keys = ON".into()]
        }
    }

    /// SQL Server-like identity and FK switches; MySQL-like DDL commits.
    struct Mss;
    impl Dialect for Mss {
        fn engine(&self) -> Engine {
            Engine::Mssql
        }
        fn transactional_ddl(&self) -> bool {
            false
        }
        fn identity_insert(&self, t: &ObjectName, on: bool) -> Option<String> {
            Some(format!("SET IDENTITY_INSERT {} {}", self.qualified(t), if on { "ON" } else { "OFF" }))
        }
        fn disable_foreign_keys(&self, tables: &[ObjectName]) -> Vec<String> {
            tables.iter().map(|t| format!("ALTER TABLE {} NOCHECK CONSTRAINT ALL", self.qualified(t))).collect()
        }
        fn enable_foreign_keys(&self, tables: &[ObjectName]) -> Vec<String> {
            tables.iter().map(|t| format!("ALTER TABLE {} CHECK CONSTRAINT ALL", self.qualified(t))).collect()
        }
    }

    fn cmp() -> SchemaCompareOptions {
        SchemaCompareOptions::default()
    }

    fn ansi(l: &Schema, r: &Schema, o: &ScriptOptions) -> SyncScript {
        schema_script_in(l, r, &cmp(), o, &AnsiDialect)
    }

    fn table_mut<'a>(s: &'a mut Schema, name: &str) -> &'a mut Table {
        s.tables.iter_mut().find(|t| t.name.name == name).unwrap()
    }

    /// Index of the first statement containing every needle.
    fn pos(s: &SyncScript, needles: &[&str]) -> usize {
        s.statements
            .iter()
            .position(|st| needles.iter().all(|n| st.contains(n)))
            .unwrap_or_else(|| panic!("no statement with {needles:?} in:\n{}", s.statements.join("\n")))
    }

    #[test]
    fn empty_diff_says_no_differences() {
        let s = schema_script(&shop(), &shop(), &cmp(), &ScriptOptions::default());
        assert!(s.statements.is_empty());
        assert!(s.text.contains("-- No differences"), "{}", s.text);
        assert!(!s.text.contains("BEGIN"));
        let d = data_script(&[], &shop(), &ScriptOptions::default());
        assert!(d.statements.is_empty() && d.text.contains("-- No differences"));
    }

    #[test]
    fn script_text_has_header_transaction_and_terminators() {
        let mut l = shop();
        table_mut(&mut l, "Orders").columns.push(col("Notes", "text"));
        let s = schema_script(&l, &shop(), &cmp(), &ScriptOptions::default());
        assert_eq!(s.statements, ["ALTER TABLE \"dbo\".\"Orders\" ADD \"Notes\" text NULL"]);
        assert!(s.transactional);
        let expected = "-- Schema sync script generated by PCompare\n\
            -- Direction: left → right (makes the right database match the left)\n\
            -- Target engine: unknown (ANSI SQL)\n\
            -- Statements: 1\n\n\
            BEGIN TRANSACTION;\n\n\
            ALTER TABLE \"dbo\".\"Orders\" ADD \"Notes\" text NULL;\n\n\
            COMMIT;\n";
        assert_eq!(s.text, expected);
        let plain = schema_script(&l, &shop(), &cmp(), &ScriptOptions { transactional: false, ..Default::default() });
        assert!(!plain.text.contains("BEGIN") && !plain.transactional);
    }

    #[test]
    fn non_transactional_ddl_and_cross_engine_warn() {
        let mut l = shop();
        l.engine = Some(Engine::Postgres);
        table_mut(&mut l, "Orders").columns.push(col("Notes", "text"));
        let mut r = shop();
        r.engine = Some(Engine::Mssql);
        let s = schema_script_in(&l, &r, &cmp(), &ScriptOptions::default(), &Mss);
        assert!(!s.transactional && !s.text.contains("BEGIN"));
        assert_eq!(s.warnings.len(), 2, "{:?}", s.warnings);
        assert!(s.warnings[0].contains("Source is PostgreSQL but target is SQL Server"));
        assert!(s.warnings[1].contains("can't be rolled back"));
        assert!(s.text.contains("-- Warning: Source is PostgreSQL"));
    }

    #[test]
    fn fk_dropped_before_referenced_column_is_altered() {
        let mut l = shop();
        table_mut(&mut l, "Customers").columns[0].data_type = "bigint".into();
        table_mut(&mut l, "Orders").columns[1].data_type = "bigint".into();
        let s = ansi(&l, &shop(), &ScriptOptions::default());
        let drop_fk = pos(&s, &["\"Orders\" DROP CONSTRAINT \"FK_Order_Customer\""]);
        let drop_pk = pos(&s, &["\"Customers\" DROP CONSTRAINT \"PK_Customers\""]);
        let alter_parent = pos(&s, &["\"Customers\" ALTER COLUMN \"Id\" TYPE bigint"]);
        let alter_child = pos(&s, &["\"Orders\" ALTER COLUMN \"CustomerId\" TYPE bigint"]);
        let add_pk = pos(&s, &["\"Customers\" ADD CONSTRAINT \"PK_Customers\" PRIMARY KEY"]);
        let add_fk = pos(&s, &["\"Orders\" ADD CONSTRAINT \"FK_Order_Customer\""]);
        assert!(drop_fk < drop_pk && drop_pk < alter_parent && alter_parent.max(alter_child) < add_pk && add_pk < add_fk);
        // Exactly one drop/add of the FK, and the cosmetic `((0))` default is untouched.
        assert_eq!(s.statements.iter().filter(|x| x.contains("FK_Order_Customer")).count(), 2);
        assert!(!s.statements.iter().any(|x| x.contains("DEFAULT")));
    }

    #[test]
    fn index_changes_drop_then_create() {
        let mut l = shop();
        table_mut(&mut l, "Customers").indexes[0].unique = false;
        let s = ansi(&l, &shop(), &ScriptOptions::default());
        assert_eq!(
            s.statements,
            ["DROP INDEX \"dbo\".\"IX_Customers_Email\"", "CREATE INDEX \"IX_Customers_Email\" ON \"dbo\".\"Customers\" (\"Email\")"]
        );
    }

    #[test]
    fn new_tables_parents_first_then_fks_views_and_triggers_last() {
        let l = {
            let mut s = shop();
            s.objects.push(trigger("TR_Orders", "Orders", "CREATE TRIGGER TR_Orders AFTER INSERT ON Orders BEGIN SELECT 1; END"));
            // Mentions BigOrders, so it must come after it despite sorting first.
            s.objects.push(code(ObjectKind::View, "AAA", "CREATE VIEW AAA AS SELECT * FROM BigOrders"));
            s
        };
        let s = ansi(&l, &schema(vec![], vec![]), &ScriptOptions::default());
        let customers = pos(&s, &["CREATE TABLE \"dbo\".\"Customers\""]);
        let orders = pos(&s, &["CREATE TABLE \"dbo\".\"Orders\""]);
        let fk = pos(&s, &["ADD CONSTRAINT \"FK_Order_Customer\""]);
        let index = pos(&s, &["CREATE UNIQUE INDEX \"IX_Customers_Email\""]);
        let big = pos(&s, &["CREATE VIEW dbo.BigOrders"]);
        let aaa = pos(&s, &["CREATE VIEW AAA"]);
        let trig = pos(&s, &["CREATE TRIGGER"]);
        assert!(customers < orders && orders < index && index < fk && fk < big && big < aaa && aaa < trig);
        assert_eq!(trig, s.statements.len() - 1);
    }

    #[test]
    fn drops_children_first_and_respect_include_drops() {
        let s = ansi(&schema(vec![], vec![]), &shop(), &ScriptOptions::default());
        let view = pos(&s, &["DROP VIEW"]);
        let orders = pos(&s, &["DROP TABLE \"dbo\".\"Orders\""]);
        let customers = pos(&s, &["DROP TABLE \"dbo\".\"Customers\""]);
        assert!(view < orders && orders < customers);
        let keep = ansi(&schema(vec![], vec![]), &shop(), &ScriptOptions { include_drops: false, ..Default::default() });
        assert!(keep.statements.is_empty());
    }

    #[test]
    fn dropping_a_parent_drops_fks_of_kept_children() {
        let mut l = shop();
        l.tables.retain(|t| t.name.name != "Customers");
        table_mut(&mut l, "Orders").foreign_keys.clear();
        let s = ansi(&l, &shop(), &ScriptOptions::default());
        let fk = pos(&s, &["DROP CONSTRAINT \"FK_Order_Customer\""]);
        assert!(fk < pos(&s, &["DROP TABLE \"dbo\".\"Customers\""]));
        assert_eq!(s.statements.iter().filter(|x| x.contains("FK_Order_Customer")).count(), 1);
    }

    #[test]
    fn right_to_left_mirrors_left_to_right() {
        let mut l = shop();
        table_mut(&mut l, "Customers").columns[1].data_type = "nvarchar(100)".into();
        l.objects.push(code(ObjectKind::View, "Extra", "CREATE VIEW Extra AS SELECT 1"));
        let r = shop();
        let ltr = ansi(&l, &r, &ScriptOptions::default());
        let rtl = ansi(&r, &l, &ScriptOptions { direction: SyncDirection::RightToLeft, ..Default::default() });
        assert!(!ltr.statements.is_empty());
        assert_eq!(ltr.statements, rtl.statements);
        assert!(rtl.text.contains("right → left"));
    }

    #[test]
    fn ids_select_objects() {
        let mut l = shop();
        table_mut(&mut l, "Customers").columns[1].data_type = "nvarchar(100)".into();
        table_mut(&mut l, "Orders").columns.push(col("Notes", "text"));
        let only = ScriptOptions { ids: vec!["table:dbo.orders".into()], ..Default::default() };
        let s = ansi(&l, &shop(), &only);
        assert_eq!(s.statements.len(), 1);
        assert!(s.statements[0].contains("\"Notes\""));
    }

    #[test]
    fn changed_view_is_dropped_early_and_recreated_late() {
        let mut l = shop();
        l.objects[0].definition = "CREATE VIEW dbo.BigOrders AS SELECT * FROM dbo.Orders WHERE Total > 500".into();
        table_mut(&mut l, "Orders").columns.retain(|c| c.name != "Total");
        table_mut(&mut l, "Orders").checks.clear();
        let s = ansi(&l, &shop(), &ScriptOptions::default());
        let drop_view = pos(&s, &["DROP VIEW \"dbo\".\"BigOrders\""]);
        let drop_col = pos(&s, &["DROP COLUMN \"Total\""]);
        let create_view = pos(&s, &["CREATE VIEW dbo.BigOrders", "500"]);
        assert!(drop_view < drop_col && drop_col < create_view);
        assert!(pos(&s, &["DROP CONSTRAINT \"CK_Orders_Total\""]) < drop_col);
    }

    #[test]
    fn sqlite_rebuilds_tables_it_cannot_alter() {
        let mut l = shop();
        table_mut(&mut l, "Orders").columns[2].data_type = "numeric(12,2)".into();
        table_mut(&mut l, "Orders").columns.push(col("Notes", "text"));
        table_mut(&mut l, "Orders").indexes.push(index("IX_Orders_Customer", &["CustomerId"], false));
        let tr = trigger("TR_Orders", "Orders", "CREATE TRIGGER TR_Orders AFTER INSERT ON Orders BEGIN SELECT 1; END");
        l.objects.push(tr.clone());
        let mut r = shop();
        r.objects.push(tr);
        let s = schema_script_in(&l, &r, &cmp(), &ScriptOptions::default(), &Lite);
        let expected_prefix = [
            "PRAGMA defer_foreign_keys = ON".to_owned(),
            "CREATE TABLE \"dbo\".\"_pcompare_new_Orders\" (".to_owned(),
        ];
        assert_eq!(s.statements[0], expected_prefix[0]);
        assert!(s.statements[1].starts_with(&expected_prefix[1]), "{}", s.statements[1]);
        // Inline FK in the new definition.
        assert!(s.statements[1].contains("FOREIGN KEY (\"CustomerId\") REFERENCES \"dbo\".\"Customers\""));
        assert_eq!(
            s.statements[2..],
            [
                "INSERT INTO \"dbo\".\"_pcompare_new_Orders\" (\"Id\", \"CustomerId\", \"Total\") SELECT \"Id\", \"CustomerId\", \"Total\" FROM \"dbo\".\"Orders\"",
                "DROP TABLE \"dbo\".\"Orders\"",
                "ALTER TABLE \"dbo\".\"_pcompare_new_Orders\" RENAME TO \"Orders\"",
                "CREATE INDEX \"IX_Orders_Customer\" ON \"dbo\".\"Orders\" (\"CustomerId\")",
                "CREATE TRIGGER TR_Orders AFTER INSERT ON Orders BEGIN SELECT 1; END",
            ]
        );
        assert!(s.warnings.iter().any(|w| w.contains("dbo.Orders is rebuilt")));

        // Adding a nullable column alone needs no rebuild.
        let mut l = shop();
        table_mut(&mut l, "Orders").columns.push(col("Notes", "text"));
        let s = schema_script_in(&l, &shop(), &cmp(), &ScriptOptions::default(), &Lite);
        assert_eq!(s.statements, ["ALTER TABLE \"dbo\".\"Orders\" ADD \"Notes\" text NULL"]);
        // ... but a NOT NULL column without default does.
        table_mut(&mut l, "Orders").columns.last_mut().unwrap().nullable = false;
        let s = schema_script_in(&l, &shop(), &cmp(), &ScriptOptions::default(), &Lite);
        assert!(s.statements.iter().any(|x| x.contains("_pcompare_new_Orders")));
    }

    #[test]
    fn real_sqlite_dialect_takes_the_rebuild_path() {
        let lite = |mut s: Schema| {
            s.engine = Some(Engine::Sqlite);
            for t in &mut s.tables {
                t.name.schema.clear();
                for fk in &mut t.foreign_keys {
                    fk.ref_table.schema.clear();
                }
            }
            s
        };
        let mut l = shop();
        table_mut(&mut l, "Orders").columns[1].nullable = true;
        let s = schema_script(&lite(l), &lite(shop()), &cmp(), &ScriptOptions::default());
        assert_eq!(s.target_engine, Some(Engine::Sqlite));
        let create = pos(&s, &["CREATE TABLE", "_pcompare_new_Orders"]);
        assert!(create < pos(&s, &["INSERT INTO", "SELECT"]));
        assert!(pos(&s, &["DROP TABLE"]) < pos(&s, &["RENAME TO"]));
    }

    #[test]
    fn computed_column_change_drops_and_readds() {
        let mut l = shop();
        let mut r = shop();
        let mut c = col("Doubled", "decimal(10,2)");
        c.computed = Some("(Total * 2) STORED".into());
        table_mut(&mut r, "Orders").columns.push(c.clone());
        c.computed = Some("(Total * 2) VIRTUAL".into());
        table_mut(&mut l, "Orders").columns.push(c);
        let s = ansi(&l, &r, &ScriptOptions::default());
        assert_eq!(s.statements.len(), 2);
        assert!(s.statements[0].contains("DROP COLUMN \"Doubled\""));
        assert!(s.statements[1].contains("ADD \"Doubled\"") && s.statements[1].contains("VIRTUAL"));
    }

    #[test]
    fn identity_change_warns_instead_of_silence() {
        let mut l = shop();
        table_mut(&mut l, "Orders").columns[0].identity = Some(Identity::default());
        let s = ansi(&l, &shop(), &ScriptOptions::default());
        assert!(s.statements.is_empty());
        assert!(s.warnings.iter().any(|w| w.contains("Identity change on dbo.Orders.Id")), "{:?}", s.warnings);
        assert!(!s.text.contains("No differences"));
    }

    #[test]
    fn dropping_an_index_backing_an_fk_drops_and_readds_the_fk() {
        let mut r = shop();
        table_mut(&mut r, "Orders").indexes.push(index("IX_Orders_Customer", &["CustomerId"], false));
        let s = ansi(&shop(), &r, &ScriptOptions::default());
        let drop_fk = pos(&s, &["DROP CONSTRAINT \"FK_Order_Customer\""]);
        let drop_ix = pos(&s, &["DROP INDEX \"dbo\".\"IX_Orders_Customer\""]);
        let add_fk = pos(&s, &["ADD CONSTRAINT \"FK_Order_Customer\""]);
        assert!(drop_fk < drop_ix && drop_ix < add_fk);
    }

    // ----- data -----

    fn orders_diff() -> TableDataDiff {
        let cols: Vec<String> = ["Id", "CustomerId", "Total"].map(String::from).to_vec();
        TableDataDiff {
            summary: TableDataSummary {
                id: "dbo.orders".into(),
                left: Some(ObjectName::new("dbo", "Orders")),
                right: Some(ObjectName::new("dbo", "ORDERS")),
                key: vec!["Id".into()],
                right_columns: cols.iter().map(|c| c.to_lowercase()).collect(),
                columns: cols,
                ..Default::default()
            },
            rows: vec![
                row(RowStatus::Different, Some(vec![Int(1), Int(7), Decimal("5".into())]), Some(vec![Int(1), Int(7), Decimal("6".into())]), vec![2]),
                row(RowStatus::LeftOnly, Some(vec![Int(2), Int(8), Null]), None, vec![]),
                row(RowStatus::RightOnly, None, Some(vec![Int(3), Int(9), Null]), vec![]),
            ],
            truncated: false,
        }
    }

    fn customers_diff() -> TableDataDiff {
        let cols: Vec<String> = ["Id", "Email"].map(String::from).to_vec();
        TableDataDiff {
            summary: TableDataSummary {
                id: "dbo.customers".into(),
                left: Some(ObjectName::new("dbo", "Customers")),
                right: Some(ObjectName::new("dbo", "Customers")),
                key: vec!["Id".into()],
                right_columns: cols.clone(),
                columns: cols,
                ..Default::default()
            },
            rows: vec![
                row(RowStatus::LeftOnly, Some(vec![Int(8), Text("new@x".into())]), None, vec![]),
                row(RowStatus::RightOnly, None, Some(vec![Int(9), Text("old@x".into())]), vec![]),
            ],
            truncated: false,
        }
    }

    fn row(status: RowStatus, left: Option<Vec<Value>>, right: Option<Vec<Value>>, diffs: Vec<usize>) -> DataRow {
        let key = left.as_ref().or(right.as_ref()).unwrap()[..1].to_vec();
        DataRow { status, key, left, right, diffs }
    }

    #[test]
    fn data_script_orders_deletes_children_first_and_inserts_parents_first() {
        let diffs = [orders_diff(), customers_diff()];
        let opts = ScriptOptions { disable_foreign_keys: false, ..Default::default() };
        let s = data_script_in(&diffs, &shop(), &opts, &AnsiDialect);
        assert_eq!(
            s.statements,
            [
                "DELETE FROM \"dbo\".\"ORDERS\" WHERE \"id\" = 3",
                "DELETE FROM \"dbo\".\"Customers\" WHERE \"Id\" = 9",
                "UPDATE \"dbo\".\"ORDERS\" SET \"total\" = 5 WHERE \"id\" = 1",
                "INSERT INTO \"dbo\".\"Customers\" (\"Id\", \"Email\") VALUES (8, 'new@x')",
                "INSERT INTO \"dbo\".\"ORDERS\" (\"id\", \"customerid\", \"total\") VALUES (2, 8, NULL)",
            ]
        );
        assert!(s.text.contains("-- Data sync script generated by PCompare") && s.text.contains("BEGIN TRANSACTION;"));
        let no_drops = data_script_in(&diffs, &shop(), &ScriptOptions { include_drops: false, include_updates: false, ..opts }, &AnsiDialect);
        assert_eq!(no_drops.statements.len(), 2);
        assert!(no_drops.statements.iter().all(|x| x.starts_with("INSERT")));
    }

    #[test]
    fn data_script_right_to_left_uses_left_spelling_and_right_values() {
        let opts = ScriptOptions { direction: SyncDirection::RightToLeft, disable_foreign_keys: false, ..Default::default() };
        let s = data_script_in(&[orders_diff()], &shop(), &opts, &AnsiDialect);
        assert_eq!(
            s.statements,
            [
                "DELETE FROM \"dbo\".\"Orders\" WHERE \"Id\" = 2",
                "UPDATE \"dbo\".\"Orders\" SET \"Total\" = 6 WHERE \"Id\" = 1",
                "INSERT INTO \"dbo\".\"Orders\" (\"Id\", \"CustomerId\", \"Total\") VALUES (3, 9, NULL)",
            ]
        );
    }

    #[test]
    fn data_script_identity_insert_computed_and_fk_switches() {
        let mut target = shop();
        let mut c = col("Label", "nvarchar(60)");
        c.computed = Some("Email + '!'".into());
        table_mut(&mut target, "Customers").columns.push(c);
        let mut diff = customers_diff();
        diff.summary.columns.push("Label".into());
        diff.summary.right_columns.push("Label".into());
        for r in &mut diff.rows {
            for side in [&mut r.left, &mut r.right].into_iter().flatten() {
                side.push(Text("computed".into()));
            }
        }
        diff.rows.push(row(
            RowStatus::Different,
            Some(vec![Int(5), Text("a@x".into()), Text("l".into())]),
            Some(vec![Int(5), Text("b@x".into()), Text("r".into())]),
            vec![1, 2],
        ));
        let s = data_script_in(&[diff], &target, &ScriptOptions::default(), &Mss);
        assert_eq!(
            s.statements,
            [
                "ALTER TABLE \"dbo\".\"Customers\" NOCHECK CONSTRAINT ALL",
                "DELETE FROM \"dbo\".\"Customers\" WHERE \"Id\" = 9",
                "UPDATE \"dbo\".\"Customers\" SET \"Email\" = 'a@x' WHERE \"Id\" = 5",
                "SET IDENTITY_INSERT \"dbo\".\"Customers\" ON",
                "INSERT INTO \"dbo\".\"Customers\" (\"Id\", \"Email\") VALUES (8, 'new@x')",
                "SET IDENTITY_INSERT \"dbo\".\"Customers\" OFF",
                "ALTER TABLE \"dbo\".\"Customers\" CHECK CONSTRAINT ALL",
            ]
        );
        // Data scripts stay transactional even where DDL isn't.
        assert!(s.transactional);
    }

    #[test]
    fn data_script_skips_errored_and_unselected_tables() {
        let mut bad = orders_diff();
        bad.summary.error = Some("Key is not unique".into());
        let s = data_script_in(&[bad, customers_diff()], &shop(), &ScriptOptions { disable_foreign_keys: false, ..Default::default() }, &AnsiDialect);
        assert!(s.statements.iter().all(|x| x.contains("Customers")));
        let only = ScriptOptions { ids: vec!["dbo.orders".into()], ..Default::default() };
        let s = data_script_in(&[orders_diff(), customers_diff()], &shop(), &only, &AnsiDialect);
        assert!(s.statements.iter().all(|x| x.contains("ORDERS")));
    }

    #[test]
    fn picked_rows_limit_a_table_to_those_keys() {
        let mut diff = customers_diff();
        let id = diff.summary.id.clone();
        let opts = ScriptOptions { rows: HashMap::from([(id, vec![vec![Some("9".into())]])]), ..Default::default() };
        keep_picked_rows(&mut diff, &opts);
        assert_eq!(diff.rows.len(), 1);
        assert_eq!(diff.rows[0].key, [Int(9)]);
        // Tables without an entry keep every row.
        let mut other = orders_diff();
        let before = other.rows.len();
        keep_picked_rows(&mut other, &opts);
        assert_eq!(other.rows.len(), before);
    }
}
