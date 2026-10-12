//! MySQL / MariaDB SQL rendering.
//!
//! Model conventions shared with `driver::mysql` (the driver produces them,
//! this dialect renders them back):
//! - `Column::computed` is the whole clause after `GENERATED ALWAYS AS`:
//!   `(expr) STORED` or `(expr) VIRTUAL`. A bare expression from another
//!   engine is rendered as `(expr) STORED`.
//! - `Column::default` is SQL-expression text (`'abc'`, `0`, `(curdate())`,
//!   `CURRENT_TIMESTAMP(3)`); an `ON UPDATE CURRENT_TIMESTAMP` attribute is
//!   folded in (`CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP`) and a column
//!   with only the attribute has `default = "ON UPDATE CURRENT_TIMESTAMP"`.
//! - Index / primary-key parts: a bare column name, `col(10)` for a prefix
//!   (SUB_PART), anything else is a functional key part (expression text,
//!   rendered as `(expr)`).

use super::{is_plain_identifier, Dialect};
use crate::model::*;
use crate::value::Value;

pub struct MysqlDialect;

/// `'it''s \\ x'` — MySQL string literal. Backslashes are escaped too
/// because `NO_BACKSLASH_ESCAPES` is off by default.
fn quote_mysql(s: &str) -> String {
    format!("'{}'", s.replace('\\', "\\\\").replace('\'', "''"))
}

fn starts_with_ci(s: &str, prefix: &str) -> bool {
    s.len() >= prefix.len() && s.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())
}

fn ends_with_ci(s: &str, suffix: &str) -> bool {
    s.len() >= suffix.len() && s.as_bytes()[s.len() - suffix.len()..].eq_ignore_ascii_case(suffix.as_bytes())
}

/// `email(10)` → `("email", "10")`: an index prefix part.
fn split_prefix(part: &str) -> Option<(&str, &str)> {
    let inner = part.strip_suffix(')')?;
    let open = inner.rfind('(')?;
    let (col, len) = (&inner[..open], &inner[open + 1..]);
    (is_plain_identifier(col) && !len.is_empty() && len.bytes().all(|b| b.is_ascii_digit())).then_some((col, len))
}

/// True when the statement creates a routine, trigger or event whose body
/// may contain `;` and therefore needs a `DELIMITER` in the mysql CLI.
fn is_compound(stmt: &str) -> bool {
    let mut words = stmt.split_whitespace();
    if !words.next().is_some_and(|w| w.eq_ignore_ascii_case("CREATE")) {
        return false;
    }
    for w in words.take(8) {
        let u = w.to_ascii_uppercase();
        match u.as_str() {
            "PROCEDURE" | "FUNCTION" | "TRIGGER" | "EVENT" => return true,
            "OR" | "REPLACE" | "AGGREGATE" => {}
            _ if u.starts_with("DEFINER=") || u.starts_with('`') || u.starts_with('@') || u.starts_with('\'') => {}
            _ => return false,
        }
    }
    false
}

impl MysqlDialect {
    /// One index / key part: `` `col` ``, `` `col`(10) `` or `(expr)`.
    fn key_part(&self, name: &str, descending: bool) -> String {
        let part = if is_plain_identifier(name) {
            self.quote_ident(name)
        } else if let Some((col, len)) = split_prefix(name) {
            format!("{}({len})", self.quote_ident(col))
        } else {
            format!("({name})")
        };
        if descending { format!("{part} DESC") } else { part }
    }

    fn key_parts(&self, cols: &[IndexColumn]) -> String {
        cols.iter().map(|c| self.key_part(&c.name, c.descending)).collect::<Vec<_>>().join(", ")
    }

    fn using(idx: &Index) -> &'static str {
        match idx.method.as_deref() {
            Some("hash") => " USING HASH",
            Some("btree") => " USING BTREE",
            _ => "",
        }
    }
}

impl Dialect for MysqlDialect {
    fn engine(&self) -> Engine {
        Engine::Mysql
    }

    fn quote_ident(&self, ident: &str) -> String {
        format!("`{}`", ident.replace('`', "``"))
    }

    fn literal(&self, v: &Value) -> String {
        match v {
            Value::Bool(b) => if *b { "1" } else { "0" }.into(),
            Value::Bytes(b) if b.is_empty() => "''".into(),
            Value::Bytes(b) => Value::hex(b),
            Value::Text(s)
            | Value::Date(s)
            | Value::Time(s)
            | Value::DateTime(s)
            | Value::DateTimeTz(s)
            | Value::Uuid(s)
            | Value::Document(s) => quote_mysql(s),
            Value::Float(f) if f.is_finite() => format!("{f:?}"),
            Value::Float(f) => quote_mysql(&f.to_string()),
            Value::Null => "NULL".into(),
            Value::Int(i) => i.to_string(),
            Value::UInt(u) => u.to_string(),
            Value::Decimal(d) => d.clone(),
        }
    }

    fn begin_transaction(&self) -> &'static str {
        "START TRANSACTION"
    }

    fn transactional_ddl(&self) -> bool {
        false
    }

    fn script_statement(&self, stmt: &str) -> String {
        if !is_compound(stmt) {
            return format!("{stmt};");
        }
        let delim = if stmt.contains("$$") { "//" } else { "$$" };
        format!("DELIMITER {delim}\n{stmt}{delim}\nDELIMITER ;")
    }

    fn column_definition(&self, c: &Column) -> String {
        let mut s = format!("{} {}", self.quote_ident(&c.name), c.data_type);
        if let Some(coll) = &c.collation {
            s.push_str(&format!(" COLLATE {coll}"));
        }
        let null = if c.nullable { " NULL" } else { " NOT NULL" };
        if let Some(expr) = &c.computed {
            let expr = expr.trim();
            if ends_with_ci(expr, " STORED") || ends_with_ci(expr, " VIRTUAL") {
                s.push_str(&format!(" GENERATED ALWAYS AS {expr}"));
            } else {
                s.push_str(&format!(" GENERATED ALWAYS AS ({expr}) STORED"));
            }
            s.push_str(null);
            return s;
        }
        s.push_str(null);
        if let Some(d) = &c.default {
            if starts_with_ci(d, "ON UPDATE ") {
                s.push_str(&format!(" {d}"));
            } else {
                s.push_str(&format!(" DEFAULT {d}"));
            }
        }
        if c.identity.is_some() {
            s.push_str(" AUTO_INCREMENT");
        }
        s
    }

    /// MySQL's primary key is always named `PRIMARY`; a name is never rendered.
    fn primary_key_clause(&self, pk: &KeyConstraint) -> String {
        let parts = pk.columns.iter().map(|c| self.key_part(c, false)).collect::<Vec<_>>().join(", ");
        format!("PRIMARY KEY ({parts})")
    }

    fn unique_clause(&self, idx: &Index) -> String {
        format!(
            "CONSTRAINT {} UNIQUE ({}){}",
            self.quote_ident(&idx.name),
            self.key_parts(&idx.columns),
            Self::using(idx)
        )
    }

    fn rename_table(&self, from: &ObjectName, to_name: &str) -> String {
        let to = ObjectName::new(from.schema.clone(), to_name);
        format!("RENAME TABLE {} TO {}", self.qualified(from), self.qualified(&to))
    }

    fn add_column(&self, table: &ObjectName, c: &Column) -> String {
        format!("ALTER TABLE {} ADD COLUMN {}", self.qualified(table), self.column_definition(c))
    }

    /// One `MODIFY COLUMN` restating the whole target definition (MySQL has
    /// no per-attribute ALTER for type/nullability).
    fn alter_column(&self, table: &ObjectName, _from: &Column, to: &Column) -> Vec<String> {
        vec![format!("ALTER TABLE {} MODIFY COLUMN {}", self.qualified(table), self.column_definition(to))]
    }

    fn drop_primary_key(&self, table: &ObjectName, _pk: &KeyConstraint) -> String {
        format!("ALTER TABLE {} DROP PRIMARY KEY", self.qualified(table))
    }

    /// INCLUDE columns and filters don't exist in MySQL and are not rendered.
    fn create_index(&self, table: &ObjectName, idx: &Index) -> String {
        if idx.constraint {
            return format!("ALTER TABLE {} ADD {}", self.qualified(table), self.unique_clause(idx));
        }
        let kind = match idx.method.as_deref() {
            Some("fulltext") => "FULLTEXT ",
            Some("spatial") => "SPATIAL ",
            _ if idx.unique => "UNIQUE ",
            _ => "",
        };
        format!(
            "CREATE {kind}INDEX {} ON {} ({}){}",
            self.quote_ident(&idx.name),
            self.qualified(table),
            self.key_parts(&idx.columns),
            Self::using(idx)
        )
    }

    fn drop_index(&self, table: &ObjectName, idx: &Index) -> String {
        if idx.constraint {
            format!("ALTER TABLE {} DROP INDEX {}", self.qualified(table), self.quote_ident(&idx.name))
        } else {
            format!("DROP INDEX {} ON {}", self.quote_ident(&idx.name), self.qualified(table))
        }
    }

    fn drop_foreign_key(&self, table: &ObjectName, fk: &ForeignKey) -> String {
        format!("ALTER TABLE {} DROP FOREIGN KEY {}", self.qualified(table), self.quote_ident(&fk.name))
    }

    // drop_check: the provided `DROP CONSTRAINT` works on MySQL 8.0.19+ and
    // MariaDB alike (MySQL's own `DROP CHECK` is not understood by MariaDB).

    /// Views use `CREATE OR REPLACE` so grants survive; routines and
    /// triggers have no portable equivalent and are dropped and recreated.
    fn replace_object(&self, old: &CodeObject, new: &CodeObject) -> Vec<String> {
        let def = new.definition.trim().trim_end_matches(';');
        if new.kind == ObjectKind::View && starts_with_ci(def, "CREATE ") && !starts_with_ci(def, "CREATE OR REPLACE ") {
            return vec![format!("CREATE OR REPLACE {}", def["CREATE ".len()..].trim_start())];
        }
        let mut v = vec![self.drop_object(old)];
        v.extend(self.create_object(new));
        v
    }

    fn disable_foreign_keys(&self, _tables: &[ObjectName]) -> Vec<String> {
        vec!["SET FOREIGN_KEY_CHECKS = 0".into()]
    }

    fn enable_foreign_keys(&self, _tables: &[ObjectName]) -> Vec<String> {
        vec!["SET FOREIGN_KEY_CHECKS = 1".into()]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const D: MysqlDialect = MysqlDialect;

    fn t() -> ObjectName {
        ObjectName::new("", "users")
    }

    fn col(name: &str, ty: &str) -> Column {
        Column { name: name.into(), data_type: ty.into(), nullable: true, ..Default::default() }
    }

    #[test]
    fn quoting_and_literals() {
        assert_eq!(D.quote_ident("a`b"), "`a``b`");
        assert_eq!(D.qualified(&t()), "`users`");
        assert_eq!(D.literal(&Value::Text("it's \\ x".into())), r"'it''s \\ x'");
        assert_eq!(D.literal(&Value::Bytes(vec![0x0a, 0xff])), "0x0aff");
        assert_eq!(D.literal(&Value::Bytes(vec![])), "''");
        assert_eq!(D.literal(&Value::Bool(true)), "1");
        assert_eq!(D.literal(&Value::Bool(false)), "0");
        assert_eq!(D.literal(&Value::Null), "NULL");
        assert_eq!(D.literal(&Value::UInt(u64::MAX)), u64::MAX.to_string());
        assert_eq!(D.literal(&Value::Float(1.5)), "1.5");
        assert_eq!(D.literal(&Value::Decimal("-12.5".into())), "-12.5");
        assert_eq!(D.literal(&Value::DateTime("2024-01-02 03:04:05.5".into())), "'2024-01-02 03:04:05.5'");
    }

    #[test]
    fn transactions_and_fk_toggles() {
        assert!(!D.transactional_ddl());
        assert_eq!(D.begin_transaction(), "START TRANSACTION");
        assert_eq!(D.disable_foreign_keys(&[t()]), vec!["SET FOREIGN_KEY_CHECKS = 0"]);
        assert_eq!(D.enable_foreign_keys(&[t()]), vec!["SET FOREIGN_KEY_CHECKS = 1"]);
    }

    #[test]
    fn script_statement_wraps_compound_bodies() {
        assert_eq!(D.script_statement("DROP TABLE `x`"), "DROP TABLE `x`;");
        assert_eq!(D.script_statement("CREATE VIEW `v` AS select 1"), "CREATE VIEW `v` AS select 1;");
        let p = "CREATE PROCEDURE `p`()\nBEGIN SELECT 1; END";
        assert_eq!(D.script_statement(p), format!("DELIMITER $$\n{p}$$\nDELIMITER ;"));
        let t = "CREATE DEFINER=`root`@`%` TRIGGER `t` BEFORE INSERT ON `x` FOR EACH ROW SET NEW.a = 1";
        assert!(D.script_statement(t).starts_with("DELIMITER $$\n"));
        assert!(D.script_statement("CREATE FUNCTION f() RETURNS int RETURN 1").ends_with("$$\nDELIMITER ;"));
        let dollar = "CREATE PROCEDURE p() SELECT '$$'";
        assert_eq!(D.script_statement(dollar), format!("DELIMITER //\n{dollar}//\nDELIMITER ;"));
    }

    #[test]
    fn column_definitions() {
        let mut id = col("id", "int unsigned");
        id.nullable = false;
        id.identity = Some(Identity::default());
        assert_eq!(D.column_definition(&id), "`id` int unsigned NOT NULL AUTO_INCREMENT");

        let mut name = col("name", "varchar(80)");
        name.collation = Some("utf8mb4_bin".into());
        name.default = Some("'it''s'".into());
        assert_eq!(D.column_definition(&name), "`name` varchar(80) COLLATE utf8mb4_bin NULL DEFAULT 'it''s'");

        let mut ts = col("updated_at", "timestamp(3)");
        ts.default = Some("CURRENT_TIMESTAMP(3) ON UPDATE CURRENT_TIMESTAMP(3)".into());
        assert_eq!(
            D.column_definition(&ts),
            "`updated_at` timestamp(3) NULL DEFAULT CURRENT_TIMESTAMP(3) ON UPDATE CURRENT_TIMESTAMP(3)"
        );
        ts.default = Some("ON UPDATE CURRENT_TIMESTAMP".into());
        assert_eq!(D.column_definition(&ts), "`updated_at` timestamp(3) NULL ON UPDATE CURRENT_TIMESTAMP");

        let mut v = col("len", "int");
        v.computed = Some("(char_length(`name`)) VIRTUAL".into());
        assert_eq!(D.column_definition(&v), "`len` int GENERATED ALWAYS AS (char_length(`name`)) VIRTUAL NULL");
        let mut s = col("lc", "varchar(10)");
        s.nullable = false;
        s.computed = Some("(lower(`email`)) STORED".into());
        assert_eq!(D.column_definition(&s), "`lc` varchar(10) GENERATED ALWAYS AS (lower(`email`)) STORED NOT NULL");
        s.computed = Some("a + b".into());
        assert_eq!(D.column_definition(&s), "`lc` varchar(10) GENERATED ALWAYS AS (a + b) STORED NOT NULL");
    }

    #[test]
    fn create_table_with_constraints() {
        let mut id = col("id", "int");
        id.nullable = false;
        let table = Table {
            name: t(),
            columns: vec![id, col("email", "varchar(50)")],
            primary_key: Some(KeyConstraint { name: Some("PRIMARY".into()), columns: vec!["id".into()], clustered: None }),
            indexes: vec![
                Index {
                    name: "uq_email".into(),
                    columns: vec![IndexColumn { name: "email(10)".into(), descending: false }],
                    unique: true,
                    constraint: true,
                    ..Default::default()
                },
                Index { name: "ix".into(), columns: vec![IndexColumn { name: "email".into(), descending: false }], ..Default::default() },
            ],
            foreign_keys: vec![],
            checks: vec![CheckConstraint { name: "ck".into(), expression: "(`id` > 0)".into() }],
        };
        assert_eq!(
            D.create_table(&table),
            "CREATE TABLE `users` (\n    `id` int NOT NULL,\n    `email` varchar(50) NULL,\n    PRIMARY KEY (`id`),\n    \
             CONSTRAINT `uq_email` UNIQUE (`email`(10)),\n    CONSTRAINT `ck` CHECK ((`id` > 0))\n)"
        );
    }

    #[test]
    fn indexes_round_trip_parts() {
        let idx = Index {
            name: "ix_mix".into(),
            columns: vec![
                IndexColumn { name: "name(10)".into(), descending: false },
                IndexColumn { name: "status".into(), descending: true },
                IndexColumn { name: "lower(`email`)".into(), descending: false },
            ],
            ..Default::default()
        };
        assert_eq!(
            D.create_index(&t(), &idx),
            "CREATE INDEX `ix_mix` ON `users` (`name`(10), `status` DESC, (lower(`email`)))"
        );
        assert_eq!(D.drop_index(&t(), &idx), "DROP INDEX `ix_mix` ON `users`");

        let ft = Index {
            name: "ft".into(),
            columns: vec![IndexColumn { name: "bio".into(), descending: false }],
            method: Some("fulltext".into()),
            ..Default::default()
        };
        assert_eq!(D.create_index(&t(), &ft), "CREATE FULLTEXT INDEX `ft` ON `users` (`bio`)");
        let hash = Index { method: Some("hash".into()), unique: true, ..ft.clone() };
        assert_eq!(D.create_index(&t(), &hash), "CREATE UNIQUE INDEX `ft` ON `users` (`bio`) USING HASH");

        let uq = Index { constraint: true, unique: true, method: None, ..ft };
        assert_eq!(D.create_index(&t(), &uq), "ALTER TABLE `users` ADD CONSTRAINT `ft` UNIQUE (`bio`)");
        assert_eq!(D.drop_index(&t(), &uq), "ALTER TABLE `users` DROP INDEX `ft`");
    }

    #[test]
    fn alter_and_drop_statements() {
        let from = col("a", "int");
        let mut to = col("a", "bigint");
        to.nullable = false;
        to.default = Some("0".into());
        assert_eq!(D.alter_column(&t(), &from, &to), vec!["ALTER TABLE `users` MODIFY COLUMN `a` bigint NOT NULL DEFAULT 0"]);
        assert_eq!(D.add_column(&t(), &from), "ALTER TABLE `users` ADD COLUMN `a` int NULL");
        let pk = KeyConstraint { name: None, columns: vec!["a".into()], clustered: None };
        assert_eq!(D.drop_primary_key(&t(), &pk), "ALTER TABLE `users` DROP PRIMARY KEY");
        assert_eq!(D.add_primary_key(&t(), &pk), "ALTER TABLE `users` ADD PRIMARY KEY (`a`)");
        let fk = ForeignKey {
            name: "fk".into(),
            columns: vec!["a".into()],
            ref_table: ObjectName::new("", "other"),
            ref_columns: vec!["id".into()],
            on_delete: Some("CASCADE".into()),
            on_update: None,
        };
        assert_eq!(
            D.add_foreign_key(&t(), &fk),
            "ALTER TABLE `users` ADD CONSTRAINT `fk` FOREIGN KEY (`a`) REFERENCES `other` (`id`) ON DELETE CASCADE"
        );
        assert_eq!(D.drop_foreign_key(&t(), &fk), "ALTER TABLE `users` DROP FOREIGN KEY `fk`");
        let ck = CheckConstraint { name: "ck".into(), expression: "a > 0".into() };
        assert_eq!(D.drop_check(&t(), &ck), "ALTER TABLE `users` DROP CONSTRAINT `ck`");
        assert_eq!(D.rename_table(&t(), "users_old"), "RENAME TABLE `users` TO `users_old`");
    }

    #[test]
    fn replace_view_keeps_grants() {
        let old = CodeObject {
            kind: ObjectKind::View,
            name: ObjectName::new("", "v"),
            definition: "CREATE VIEW `v` AS select 1".into(),
            table: None,
        };
        let new = CodeObject { definition: "CREATE SQL SECURITY INVOKER VIEW `v` AS select 2".into(), ..old.clone() };
        assert_eq!(D.replace_object(&old, &new), vec!["CREATE OR REPLACE SQL SECURITY INVOKER VIEW `v` AS select 2"]);
        let proc_old = CodeObject { kind: ObjectKind::Procedure, definition: "CREATE PROCEDURE `v`() SELECT 1".into(), ..old };
        assert_eq!(D.replace_object(&proc_old, &proc_old), vec!["DROP PROCEDURE `v`", "CREATE PROCEDURE `v`() SELECT 1"]);
    }
}
