//! SQLite SQL rendering.
//!
//! SQLite has no schemas, declares foreign keys only inside CREATE TABLE and
//! can't alter columns or constraints in place: [`Dialect::alter_column`] is
//! empty and the script engine rebuilds the table instead. Constraint
//! add/drop methods keep the trait's ANSI defaults, which SQLite rejects —
//! callers must check [`Dialect::supports_alter_column`] first.

use super::{quote_string, Dialect};
use crate::model::*;
use crate::value::Value;

pub struct SqliteDialect;

impl Dialect for SqliteDialect {
    fn engine(&self) -> Engine {
        Engine::Sqlite
    }

    /// Always schema-less, even for names that came from another engine.
    fn qualified(&self, name: &ObjectName) -> String {
        self.quote_ident(&name.name)
    }

    fn literal(&self, v: &Value) -> String {
        match v {
            Value::Bool(b) => if *b { "1" } else { "0" }.into(),
            // SQLite has no NaN; out-of-range literals become ±Inf.
            Value::Float(f) if f.is_nan() => "NULL".into(),
            Value::Float(f) if f.is_infinite() => if *f > 0.0 { "9e999" } else { "-9e999" }.into(),
            Value::Bytes(b) => format!("X'{}'", &Value::hex(b)[2..]),
            Value::Decimal(d) if d.parse::<f64>().is_err() => quote_string(d),
            v => default_literal(v),
        }
    }

    /// `INTEGER PRIMARY KEY AUTOINCREMENT` is only valid inline, on a column
    /// typed exactly `INTEGER`; [`Dialect::create_table`] then skips the
    /// table-level primary key.
    fn column_definition(&self, c: &Column) -> String {
        let name = self.quote_ident(&c.name);
        if c.identity.is_some() {
            return format!("{name} INTEGER PRIMARY KEY AUTOINCREMENT{}", if c.nullable { "" } else { " NOT NULL" });
        }
        let mut s = name;
        if !c.data_type.is_empty() {
            s.push(' ');
            s.push_str(&c.data_type);
        }
        if let Some(expr) = &c.computed {
            // VIRTUAL (SQLite's default) so the same text also works in
            // ALTER TABLE ADD COLUMN, which rejects STORED columns.
            s.push_str(&format!(" GENERATED ALWAYS AS ({expr})"));
            return s;
        }
        if let Some(coll) = &c.collation {
            s.push_str(&format!(" COLLATE {coll}"));
        }
        if let Some(d) = &c.default {
            s.push_str(&format!(" DEFAULT {d}"));
        }
        if !c.nullable {
            s.push_str(" NOT NULL");
        }
        s
    }

    /// Autoindex-backed UNIQUE constraints have no real name in SQLite.
    fn unique_clause(&self, idx: &Index) -> String {
        let cols: Vec<String> = idx.columns.iter().map(|c| c.name.clone()).collect();
        format!("UNIQUE ({})", self.ident_list(&cols))
    }

    /// Unnamed: SQLite doesn't report FK names, so the driver synthesises them.
    fn foreign_key_clause(&self, fk: &ForeignKey) -> String {
        let mut s = format!(
            "FOREIGN KEY ({}) REFERENCES {} ({})",
            self.ident_list(&fk.columns),
            self.qualified(&fk.ref_table),
            self.ident_list(&fk.ref_columns),
        );
        if let Some(a) = &fk.on_delete {
            s.push_str(&format!(" ON DELETE {a}"));
        }
        if let Some(a) = &fk.on_update {
            s.push_str(&format!(" ON UPDATE {a}"));
        }
        s
    }

    fn create_table(&self, t: &Table) -> String {
        let mut parts: Vec<String> = t.columns.iter().map(|c| self.column_definition(c)).collect();
        if let Some(pk) = &t.primary_key {
            let inline = matches!(pk.columns.as_slice(), [only] if t.column(only).is_some_and(|c| c.identity.is_some()));
            if !inline {
                parts.push(self.primary_key_clause(pk));
            }
        }
        parts.extend(t.indexes.iter().filter(|i| i.constraint).map(|i| self.unique_clause(i)));
        parts.extend(t.checks.iter().map(|c| self.check_clause(c)));
        parts.extend(t.foreign_keys.iter().map(|fk| self.foreign_key_clause(fk)));
        format!("CREATE TABLE {} (\n    {}\n)", self.qualified(&t.name), parts.join(",\n    "))
    }

    fn inline_foreign_keys(&self) -> bool {
        true
    }

    fn alter_column(&self, _table: &ObjectName, _from: &Column, _to: &Column) -> Vec<String> {
        Vec::new()
    }

    fn supports_alter_column(&self) -> bool {
        false
    }

    /// A UNIQUE *constraint* can't be added after the fact; a unique index
    /// enforces the same rule. INCLUDE columns don't exist in SQLite.
    fn create_index(&self, table: &ObjectName, idx: &Index) -> String {
        let cols = idx
            .columns
            .iter()
            .map(|c| {
                let name = if super::is_plain_identifier(&c.name) { self.quote_ident(&c.name) } else { c.name.clone() };
                if c.descending { format!("{name} DESC") } else { name }
            })
            .collect::<Vec<_>>()
            .join(", ");
        let mut s = format!(
            "CREATE {}INDEX {} ON {} ({cols})",
            if idx.unique || idx.constraint { "UNIQUE " } else { "" },
            self.quote_ident(&idx.name),
            self.qualified(table),
        );
        if let Some(f) = &idx.filter {
            s.push_str(&format!(" WHERE {f}"));
        }
        s
    }

    fn drop_index(&self, _table: &ObjectName, idx: &Index) -> String {
        format!("DROP INDEX {}", self.quote_ident(&idx.name))
    }

    /// `foreign_keys` can't change inside a transaction; deferring moves every
    /// FK check to COMMIT instead (and resets itself there).
    fn disable_foreign_keys(&self, _tables: &[ObjectName]) -> Vec<String> {
        vec!["PRAGMA defer_foreign_keys = ON".into()]
    }

    /// A modern RENAME re-checks every view and trigger, and those still
    /// naming the dropped table fail it. The legacy rename skips that check;
    /// they are valid again once the copy has the name.
    fn rebuild_rename(&self, from: &ObjectName, to_name: &str) -> Vec<String> {
        vec![
            "PRAGMA legacy_alter_table = ON".into(),
            self.rename_table(from, to_name),
            "PRAGMA legacy_alter_table = OFF".into(),
        ]
    }
}

/// The trait's provided `literal`, for the cases SQLite doesn't change.
fn default_literal(v: &Value) -> String {
    struct Ansi;
    impl Dialect for Ansi {
        fn engine(&self) -> Engine {
            Engine::Sqlite
        }
    }
    Ansi.literal(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(name: &str, ty: &str) -> Column {
        Column { name: name.into(), data_type: ty.into(), nullable: true, ..Default::default() }
    }

    #[test]
    fn create_table_inlines_autoincrement_and_fks() {
        let t = Table {
            name: ObjectName::new("main", "orders"),
            columns: vec![
                Column { identity: Some(Identity::default()), nullable: false, ..col("id", "integer") },
                Column { nullable: false, default: Some("'x'".into()), ..col("code", "text") },
                Column { computed: Some("id * 2".into()), ..col("twice", "integer") },
                col("user_id", ""),
            ],
            primary_key: Some(KeyConstraint { name: None, columns: vec!["id".into()], clustered: None }),
            indexes: vec![Index {
                name: "orders_code_key".into(),
                columns: vec![IndexColumn { name: "code".into(), descending: false }],
                unique: true,
                constraint: true,
                ..Default::default()
            }],
            foreign_keys: vec![ForeignKey {
                name: "fk_orders_1".into(),
                columns: vec!["user_id".into()],
                ref_table: ObjectName::new("", "users"),
                ref_columns: vec!["id".into()],
                on_delete: Some("CASCADE".into()),
                on_update: None,
            }],
            checks: vec![CheckConstraint { name: "orders_check1".into(), expression: "length(code) = 3".into() }],
        };
        assert_eq!(
            SqliteDialect.create_table(&t),
            "CREATE TABLE \"orders\" (\n    \
             \"id\" INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL,\n    \
             \"code\" text DEFAULT 'x' NOT NULL,\n    \
             \"twice\" integer GENERATED ALWAYS AS (id * 2),\n    \
             \"user_id\",\n    \
             UNIQUE (\"code\"),\n    \
             CONSTRAINT \"orders_check1\" CHECK (length(code) = 3),\n    \
             FOREIGN KEY (\"user_id\") REFERENCES \"users\" (\"id\") ON DELETE CASCADE\n)"
        );
    }

    #[test]
    fn literals_and_indexes() {
        let d = SqliteDialect;
        assert_eq!(d.literal(&Value::Bool(true)), "1");
        assert_eq!(d.literal(&Value::Bytes(vec![0, 255])), "X'00ff'");
        assert_eq!(d.literal(&Value::Float(f64::INFINITY)), "9e999");
        assert_eq!(d.literal(&Value::Text("it's".into())), "'it''s'");
        let t = ObjectName::new("", "t");
        let idx = Index {
            name: "ix".into(),
            columns: vec![
                IndexColumn { name: "lower(email)".into(), descending: false },
                IndexColumn { name: "a".into(), descending: true },
            ],
            filter: Some("a > 0".into()),
            ..Default::default()
        };
        assert_eq!(d.create_index(&t, &idx), "CREATE INDEX \"ix\" ON \"t\" (lower(email), \"a\" DESC) WHERE a > 0");
        assert_eq!(d.drop_index(&ObjectName::new("s", "t"), &idx), "DROP INDEX \"ix\"");
        assert!(d.alter_column(&t, &col("a", "int"), &col("a", "text")).is_empty());
    }
}
