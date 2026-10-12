//! SQL Server (T-SQL) rendering.
//!
//! Differences from the ANSI defaults that matter for sync scripts:
//! - `[bracket]` identifiers, `N'...'` strings, `0x..` binary, `1`/`0` bits,
//!   and date literals in the only formats immune to `SET DATEFORMAT` /
//!   `SET LANGUAGE` (`'YYYYMMDD'`, `'YYYY-MM-DDTHH:MM:SS'`).
//! - Defaults are named constraints. Inline ones get `DF_<table>_<column>`;
//!   existing ones are dropped through a catalog lookup, because their
//!   (often system-generated) names differ per database and the model only
//!   stores the expression.
//! - Model conventions shared with the driver: a computed column's
//!   expression ends in ` PERSISTED` when persisted; index `method` is
//!   `None` (nonclustered rowstore), `clustered`, `columnstore` or
//!   `clustered columnstore`.

use super::{quote_string, Dialect};
use crate::model::*;
use crate::value::Value;

pub struct MssqlDialect;

/// Suffix the driver appends to `Column::computed` for PERSISTED columns.
const PERSISTED: &str = " PERSISTED";

impl MssqlDialect {
    /// Normalises a stored module definition: trims it, drops a trailing `;`
    /// and turns a leading `CREATE OR ALTER` into `CREATE` (SQL Server keeps
    /// the text as submitted, so objects deployed by our own `replace_object`
    /// would otherwise differ from their source forever).
    pub(crate) fn normalize_definition(def: &str) -> String {
        let def = def.trim().trim_end_matches(';').trim_end();
        match create_keyword(def).and_then(|(_, end)| or_alter_end(def, end).map(|alter| (end, alter))) {
            Some((end, alter)) => format!("{}{}", &def[..end], &def[alter..]),
            None => def.to_owned(),
        }
    }

    /// `N'...'` literal.
    fn nstring(s: &str) -> String {
        format!("N{}", quote_string(s))
    }

    /// Name of the inline default constraint for `table.column`.
    fn default_name(table: &ObjectName, column: &str) -> String {
        format!("DF_{}_{}", table.name, column)
    }

    /// One column with its default rendered as a named constraint when the
    /// table is known (CREATE TABLE / ADD), unnamed otherwise.
    fn column_def(&self, table: Option<&ObjectName>, c: &Column) -> String {
        let name = self.quote_ident(&c.name);
        if let Some(expr) = &c.computed {
            let (expr, persisted) = split_persisted(expr);
            let mut s = format!("{name} AS ({expr})");
            if persisted {
                s.push_str(PERSISTED);
                // Only persisted computed columns may be declared NOT NULL.
                if !c.nullable {
                    s.push_str(" NOT NULL");
                }
            }
            return s;
        }
        let mut s = format!("{name} {}", c.data_type);
        if let Some(coll) = &c.collation {
            s.push_str(&format!(" COLLATE {coll}"));
        }
        if let Some(id) = &c.identity {
            s.push_str(&format!(" IDENTITY({},{})", id.seed, id.increment));
        }
        s.push_str(if c.nullable { " NULL" } else { " NOT NULL" });
        if let Some(d) = &c.default {
            match table {
                Some(t) => s.push_str(&format!(
                    " CONSTRAINT {} DEFAULT ({d})",
                    self.quote_ident(&Self::default_name(t, &c.name))
                )),
                None => s.push_str(&format!(" DEFAULT ({d})")),
            }
        }
        s
    }

    /// One batch that drops `column`'s default constraint, whatever it is
    /// called in the target database (a no-op when there is none).
    fn drop_default(&self, table: &ObjectName, column: &str) -> String {
        let t = self.qualified(table);
        format!(
            "DECLARE @df sysname;\n\
             SELECT @df = dc.name FROM sys.default_constraints dc \
             JOIN sys.columns c ON c.object_id = dc.parent_object_id AND c.column_id = dc.parent_column_id \
             WHERE dc.parent_object_id = OBJECT_ID({}) AND c.name = {};\n\
             IF @df IS NOT NULL\n\
             BEGIN\n    \
                 DECLARE @sql nvarchar(max) = {} + QUOTENAME(@df);\n    \
                 EXEC sp_executesql @sql;\n\
             END",
            Self::nstring(&t),
            Self::nstring(column),
            Self::nstring(&format!("ALTER TABLE {t} DROP CONSTRAINT ")),
        )
    }

    fn add_default(&self, table: &ObjectName, column: &str, expr: &str) -> String {
        format!(
            "ALTER TABLE {} ADD CONSTRAINT {} DEFAULT ({expr}) FOR {}",
            self.qualified(table),
            self.quote_ident(&Self::default_name(table, column)),
            self.quote_ident(column),
        )
    }

    fn key_columns(&self, cols: &[IndexColumn]) -> String {
        cols.iter()
            .map(|c| {
                let n = self.quote_ident(&c.name);
                if c.descending { format!("{n} DESC") } else { n }
            })
            .collect::<Vec<_>>()
            .join(", ")
    }
}

impl Dialect for MssqlDialect {
    fn engine(&self) -> Engine {
        Engine::Mssql
    }

    fn quote_ident(&self, ident: &str) -> String {
        format!("[{}]", ident.replace(']', "]]"))
    }

    fn literal(&self, v: &Value) -> String {
        match v {
            Value::Null => "NULL".into(),
            Value::Bool(b) => if *b { "1" } else { "0" }.into(),
            Value::Int(i) => i.to_string(),
            Value::UInt(u) => u.to_string(),
            Value::Float(f) if f.is_finite() => format!("{f:?}"),
            // SQL Server has no NaN/Infinity: emit text so the statement fails
            // loudly instead of silently writing a different number.
            Value::Float(f) => Self::nstring(&f.to_string()),
            Value::Decimal(d) => d.clone(),
            Value::Bytes(b) => Value::hex(b),
            // `YYYYMMDD` is the one date format every type reads the same way
            // under any DATEFORMAT/LANGUAGE ('YYYY-MM-DD' is read as
            // year-day-month by `datetime` under e.g. SET LANGUAGE british).
            Value::Date(s) => quote_string(&s.replace('-', "")),
            // ISO 8601 with `T` is unambiguous for datetime and datetime2.
            // Note `datetime` rejects more than 3 fractional digits.
            Value::DateTime(s) | Value::DateTimeTz(s) => quote_string(&s.replacen(' ', "T", 1)),
            Value::Time(s) | Value::Uuid(s) => quote_string(s),
            Value::Text(s) | Value::Document(s) => Self::nstring(s),
        }
    }

    fn statement_terminator(&self) -> &'static str {
        "\nGO"
    }

    /// XACT_ABORT makes any run-time error roll the whole transaction back
    /// rather than just failing the statement and carrying on.
    fn begin_transaction(&self) -> &'static str {
        "SET XACT_ABORT ON;\nBEGIN TRANSACTION"
    }

    fn commit(&self) -> &'static str {
        "COMMIT TRANSACTION"
    }

    // ----- tables -----

    fn column_definition(&self, c: &Column) -> String {
        self.column_def(None, c)
    }

    fn primary_key_clause(&self, pk: &KeyConstraint) -> String {
        let kind = match pk.clustered {
            Some(true) => " CLUSTERED",
            Some(false) => " NONCLUSTERED",
            None => "",
        };
        let cols = self.ident_list(&pk.columns);
        match &pk.name {
            Some(n) => format!("CONSTRAINT {} PRIMARY KEY{kind} ({cols})", self.quote_ident(n)),
            None => format!("PRIMARY KEY{kind} ({cols})"),
        }
    }

    fn unique_clause(&self, idx: &Index) -> String {
        let kind = if is_method(idx, "clustered") { "CLUSTERED" } else { "NONCLUSTERED" };
        format!("CONSTRAINT {} UNIQUE {kind} ({})", self.quote_ident(&idx.name), self.key_columns(&idx.columns))
    }

    fn create_table(&self, t: &Table) -> String {
        let mut parts: Vec<String> = t.columns.iter().map(|c| self.column_def(Some(&t.name), c)).collect();
        if let Some(pk) = &t.primary_key {
            parts.push(self.primary_key_clause(pk));
        }
        parts.extend(t.indexes.iter().filter(|i| i.constraint).map(|i| self.unique_clause(i)));
        parts.extend(t.checks.iter().map(|c| self.check_clause(c)));
        format!("CREATE TABLE {} (\n    {}\n)", self.qualified(&t.name), parts.join(",\n    "))
    }

    fn rename_table(&self, from: &ObjectName, to_name: &str) -> String {
        // The new name is taken literally: brackets would become part of it.
        format!("EXEC sp_rename {}, {}", Self::nstring(&self.qualified(from)), Self::nstring(to_name))
    }

    fn add_column(&self, table: &ObjectName, c: &Column) -> String {
        format!("ALTER TABLE {} ADD {}", self.qualified(table), self.column_def(Some(table), c))
    }

    fn drop_column(&self, table: &ObjectName, column: &str) -> String {
        format!(
            "{};\nALTER TABLE {} DROP COLUMN {}",
            self.drop_default(table, column),
            self.qualified(table),
            self.quote_ident(column)
        )
    }

    /// Computed columns can't be altered, so they are dropped and re-added.
    /// Adding or removing IDENTITY needs a table rebuild (empty result); a
    /// different seed/increment alone is ignored (it can't be altered either).
    /// A default constraint blocks type changes, so it is dropped first and
    /// re-created afterwards.
    fn alter_column(&self, table: &ObjectName, from: &Column, to: &Column) -> Vec<String> {
        if from.computed.is_some() || to.computed.is_some() {
            if from.computed == to.computed && from.nullable == to.nullable {
                return Vec::new();
            }
            return vec![self.drop_column(table, &from.name), self.add_column(table, to)];
        }
        if from.identity.is_some() != to.identity.is_some() {
            return Vec::new();
        }
        let shape_changed =
            from.data_type != to.data_type || from.nullable != to.nullable || from.collation != to.collation;
        let default_changed = from.default != to.default;
        let mut out = Vec::new();
        let drop_default = from.default.is_some() && (default_changed || shape_changed);
        if drop_default {
            out.push(self.drop_default(table, &from.name));
        }
        if shape_changed {
            let mut s = format!(
                "ALTER TABLE {} ALTER COLUMN {} {}",
                self.qualified(table),
                self.quote_ident(&to.name),
                to.data_type
            );
            if let Some(coll) = &to.collation {
                s.push_str(&format!(" COLLATE {coll}"));
            }
            s.push_str(if to.nullable { " NULL" } else { " NOT NULL" });
            out.push(s);
        }
        if let Some(d) = &to.default {
            if default_changed || drop_default {
                out.push(self.add_default(table, &to.name, d));
            }
        }
        out
    }

    fn drop_primary_key(&self, table: &ObjectName, pk: &KeyConstraint) -> String {
        let t = self.qualified(table);
        match &pk.name {
            Some(n) => format!("ALTER TABLE {t} DROP CONSTRAINT {}", self.quote_ident(n)),
            None => format!(
                "DECLARE @pk sysname;\n\
                 SELECT @pk = name FROM sys.key_constraints WHERE parent_object_id = OBJECT_ID({}) AND type = 'PK';\n\
                 IF @pk IS NOT NULL\n\
                 BEGIN\n    \
                     DECLARE @sql nvarchar(max) = {} + QUOTENAME(@pk);\n    \
                     EXEC sp_executesql @sql;\n\
                 END",
                Self::nstring(&t),
                Self::nstring(&format!("ALTER TABLE {t} DROP CONSTRAINT ")),
            ),
        }
    }

    fn create_index(&self, table: &ObjectName, idx: &Index) -> String {
        let t = self.qualified(table);
        if idx.constraint {
            return format!("ALTER TABLE {t} ADD {}", self.unique_clause(idx));
        }
        let name = self.quote_ident(&idx.name);
        if is_method(idx, "clustered columnstore") {
            return format!("CREATE CLUSTERED COLUMNSTORE INDEX {name} ON {t}");
        }
        let mut s = if is_method(idx, "columnstore") || is_method(idx, "nonclustered columnstore") {
            // Columnstore indexes have no key order and no INCLUDE list.
            let mut cols: Vec<String> = idx.columns.iter().map(|c| c.name.clone()).collect();
            cols.extend(idx.include.iter().cloned());
            format!("CREATE NONCLUSTERED COLUMNSTORE INDEX {name} ON {t} ({})", self.ident_list(&cols))
        } else {
            let kind = if is_method(idx, "clustered") { "CLUSTERED" } else { "NONCLUSTERED" };
            let unique = if idx.unique { "UNIQUE " } else { "" };
            let mut s = format!("CREATE {unique}{kind} INDEX {name} ON {t} ({})", self.key_columns(&idx.columns));
            if !idx.include.is_empty() {
                s.push_str(&format!(" INCLUDE ({})", self.ident_list(&idx.include)));
            }
            s
        };
        if let Some(f) = &idx.filter {
            s.push_str(&format!(" WHERE {f}"));
        }
        s
    }

    fn drop_index(&self, table: &ObjectName, idx: &Index) -> String {
        let t = self.qualified(table);
        if idx.constraint {
            return format!("ALTER TABLE {t} DROP CONSTRAINT {}", self.quote_ident(&idx.name));
        }
        format!("DROP INDEX {} ON {t}", self.quote_ident(&idx.name))
    }

    // ----- views, routines, triggers, sequences -----

    /// `CREATE OR ALTER` (SQL Server 2016 SP1+) keeps permissions and
    /// dependants. Falls back to drop + create for sequences, kind changes
    /// and scalar <-> table-valued function changes, which ALTER rejects.
    fn replace_object(&self, old: &CodeObject, new: &CodeObject) -> Vec<String> {
        let alterable = matches!(
            new.kind,
            ObjectKind::View | ObjectKind::Procedure | ObjectKind::Function | ObjectKind::Trigger
        );
        let same_shape = old.kind == new.kind
            && (new.kind != ObjectKind::Function || function_shape(&old.definition) == function_shape(&new.definition));
        if alterable && same_shape {
            if let Some(def) = create_or_alter(&new.definition) {
                return vec![def];
            }
        }
        let mut v = vec![self.drop_object(old)];
        v.extend(self.create_object(new));
        v
    }

    // ----- data -----

    fn identity_insert(&self, table: &ObjectName, on: bool) -> Option<String> {
        Some(format!("SET IDENTITY_INSERT {} {}", self.qualified(table), if on { "ON" } else { "OFF" }))
    }

    fn disable_foreign_keys(&self, tables: &[ObjectName]) -> Vec<String> {
        tables.iter().map(|t| format!("ALTER TABLE {} NOCHECK CONSTRAINT ALL", self.qualified(t))).collect()
    }

    /// `WITH CHECK` re-validates existing rows so the constraints stay trusted.
    fn enable_foreign_keys(&self, tables: &[ObjectName]) -> Vec<String> {
        tables
            .iter()
            .map(|t| format!("ALTER TABLE {} WITH CHECK CHECK CONSTRAINT ALL", self.qualified(t)))
            .collect()
    }
}

fn is_method(idx: &Index, m: &str) -> bool {
    idx.method.as_deref().is_some_and(|x| x.eq_ignore_ascii_case(m))
}

/// `"[a]*[b] PERSISTED"` → `("[a]*[b]", true)`.
fn split_persisted(expr: &str) -> (&str, bool) {
    let t = expr.trim_end();
    let n = PERSISTED.len();
    if t.len() >= n && t.is_char_boundary(t.len() - n) && t[t.len() - n..].eq_ignore_ascii_case(PERSISTED) {
        (t[..t.len() - n].trim_end(), true)
    } else {
        (t, false)
    }
}

/// Byte offset after leading whitespace and `--` / (nested) `/* */` comments.
fn skip_trivia(s: &str) -> usize {
    let b = s.as_bytes();
    let mut i = 0;
    loop {
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        if b[i..].starts_with(b"--") {
            i = s[i..].find('\n').map_or(b.len(), |n| i + n + 1);
        } else if b[i..].starts_with(b"/*") {
            let mut depth = 0;
            while i < b.len() {
                if b[i..].starts_with(b"/*") {
                    depth += 1;
                    i += 2;
                } else if b[i..].starts_with(b"*/") {
                    depth -= 1;
                    i += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    i += 1;
                }
            }
        } else {
            return i;
        }
    }
}

/// Case-insensitive keyword at `at`, followed by a non-word character.
fn keyword_at(s: &str, at: usize, kw: &str) -> bool {
    let b = s.as_bytes();
    let end = at + kw.len();
    end <= b.len()
        && b[at..end].eq_ignore_ascii_case(kw.as_bytes())
        && b.get(end).is_none_or(|c| !(c.is_ascii_alphanumeric() || *c == b'_'))
}

/// Span of the leading `CREATE` keyword, after comments/whitespace.
fn create_keyword(def: &str) -> Option<(usize, usize)> {
    let i = skip_trivia(def);
    keyword_at(def, i, "create").then_some((i, i + "create".len()))
}

/// End of `OR ALTER` when it directly follows the CREATE ending at `after`.
fn or_alter_end(def: &str, after: usize) -> Option<usize> {
    let j = after + skip_trivia(&def[after..]);
    if !keyword_at(def, j, "or") {
        return None;
    }
    let k = j + 2 + skip_trivia(&def[j + 2..]);
    keyword_at(def, k, "alter").then_some(k + "alter".len())
}

/// The definition with its leading `CREATE` turned into `CREATE OR ALTER`;
/// `None` when it doesn't start with CREATE.
fn create_or_alter(def: &str) -> Option<String> {
    let def = def.trim().trim_end_matches(';').trim_end();
    let (_, end) = create_keyword(def)?;
    if or_alter_end(def, end).is_some() {
        return Some(def.to_owned());
    }
    Some(format!("{} OR ALTER{}", &def[..end], &def[end..]))
}

#[derive(Debug, PartialEq, Eq)]
enum FunctionShape {
    Scalar,
    InlineTable,
    MultiStatementTable,
}

/// Classifies a function by what follows its first `RETURNS` keyword,
/// skipping comments, string literals and bracketed names.
fn function_shape(def: &str) -> FunctionShape {
    let mut words = Words { s: def, i: 0 };
    while let Some(w) = words.next() {
        if w.eq_ignore_ascii_case("returns") {
            return match words.next() {
                Some(n) if n.eq_ignore_ascii_case("table") => FunctionShape::InlineTable,
                Some(n) if n.starts_with('@') => FunctionShape::MultiStatementTable,
                _ => FunctionShape::Scalar,
            };
        }
    }
    FunctionShape::Scalar
}

/// Word tokens (identifiers, `@vars`, keywords) of T-SQL text.
struct Words<'a> {
    s: &'a str,
    i: usize,
}

impl<'a> Iterator for Words<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<&'a str> {
        let b = self.s.as_bytes();
        loop {
            self.i += skip_trivia(&self.s[self.i..]);
            let c = *b.get(self.i)?;
            let skip_to = |i: usize, close: u8| {
                // Doubled close characters are escapes ('it''s', [a]]b]).
                let mut j = i + 1;
                while j < b.len() {
                    if b[j] == close {
                        if b.get(j + 1) == Some(&close) {
                            j += 2;
                            continue;
                        }
                        return j + 1;
                    }
                    j += 1;
                }
                b.len()
            };
            match c {
                b'\'' => self.i = skip_to(self.i, b'\''),
                b'[' => self.i = skip_to(self.i, b']'),
                b'"' => self.i = skip_to(self.i, b'"'),
                c if c.is_ascii_alphanumeric() || c == b'_' || c == b'@' || c == b'#' => {
                    let start = self.i;
                    while self.i < b.len()
                        && (b[self.i].is_ascii_alphanumeric() || matches!(b[self.i], b'_' | b'@' | b'#' | b'$'))
                    {
                        self.i += 1;
                    }
                    return Some(&self.s[start..self.i]);
                }
                _ => self.i += self.s[self.i..].chars().next().map_or(1, char::len_utf8),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const D: MssqlDialect = MssqlDialect;

    fn t(name: &str) -> ObjectName {
        ObjectName::new("dbo", name)
    }

    fn col(name: &str, ty: &str, nullable: bool) -> Column {
        Column { name: name.into(), data_type: ty.into(), nullable, ..Default::default() }
    }

    fn obj(kind: ObjectKind, name: &str, def: &str) -> CodeObject {
        CodeObject { kind, name: t(name), definition: def.into(), table: None }
    }

    #[test]
    fn quoting_and_names() {
        assert_eq!(D.quote_ident("a]b"), "[a]]b]");
        assert_eq!(D.qualified(&ObjectName::new("sales", "Order Lines")), "[sales].[Order Lines]");
        assert_eq!(D.ident_list(&["a".into(), "b c".into()]), "[a], [b c]");
    }

    #[test]
    fn literals() {
        assert_eq!(D.literal(&Value::Null), "NULL");
        assert_eq!(D.literal(&Value::Bool(true)), "1");
        assert_eq!(D.literal(&Value::Bool(false)), "0");
        assert_eq!(D.literal(&Value::Int(-5)), "-5");
        assert_eq!(D.literal(&Value::Float(1.5)), "1.5");
        assert_eq!(D.literal(&Value::Float(f64::NAN)), "N'NaN'");
        assert_eq!(D.literal(&Value::Decimal("12.5".into())), "12.5");
        assert_eq!(D.literal(&Value::Text("it's ü".into())), "N'it''s ü'");
        assert_eq!(D.literal(&Value::Document("<a/>".into())), "N'<a/>'");
        assert_eq!(D.literal(&Value::Bytes(vec![0xde, 0xad, 0x01])), "0xdead01");
        assert_eq!(D.literal(&Value::Bytes(vec![])), "0x");
        assert_eq!(D.literal(&Value::Date("2024-01-02".into())), "'20240102'");
        assert_eq!(D.literal(&Value::DateTime("2024-01-02 10:00:00.123".into())), "'2024-01-02T10:00:00.123'");
        assert_eq!(D.literal(&Value::DateTimeTz("2024-01-02 10:00:00+05:30".into())), "'2024-01-02T10:00:00+05:30'");
        assert_eq!(D.literal(&Value::Time("10:11:12.5".into())), "'10:11:12.5'");
        assert_eq!(
            D.literal(&Value::Uuid("6f9619ff-8b86-d011-b42d-00c04fc964ff".into())),
            "'6f9619ff-8b86-d011-b42d-00c04fc964ff'"
        );
    }

    #[test]
    fn script_text_pieces() {
        assert_eq!(D.script_statement("SELECT 1"), "SELECT 1\nGO");
        assert_eq!(D.begin_transaction(), "SET XACT_ABORT ON;\nBEGIN TRANSACTION");
        assert_eq!(D.commit(), "COMMIT TRANSACTION");
        assert!(D.transactional_ddl());
        assert!(D.supports_alter_column());
        assert!(!D.inline_foreign_keys());
    }

    fn sample_table() -> Table {
        Table {
            name: t("Orders"),
            columns: vec![
                Column { identity: Some(Identity { seed: 10, increment: 2 }), ..col("Id", "int", false) },
                Column { collation: Some("Latin1_General_CS_AS".into()), ..col("Name", "nvarchar(50)", true) },
                Column { default: Some("getdate()".into()), ..col("Created", "datetime2(3)", false) },
                Column { computed: Some("[Qty]*[Price] PERSISTED".into()), ..col("Total", "decimal(21,2)", false) },
                Column { computed: Some("[Price]/(10)".into()), ..col("Tenth", "decimal(18,6)", true) },
            ],
            primary_key: Some(KeyConstraint { name: Some("PK_Orders".into()), columns: vec!["Id".into()], clustered: Some(true) }),
            indexes: vec![
                Index {
                    name: "UQ_Orders_Name".into(),
                    columns: vec![IndexColumn { name: "Name".into(), descending: true }],
                    unique: true,
                    constraint: true,
                    ..Default::default()
                },
                Index { name: "IX_Plain".into(), columns: vec![IndexColumn { name: "Created".into(), descending: false }], ..Default::default() },
            ],
            foreign_keys: vec![],
            checks: vec![CheckConstraint { name: "CK_Qty".into(), expression: "[Qty]>(0)".into() }],
        }
    }

    #[test]
    fn create_table_renders_tsql() {
        assert_eq!(
            D.create_table(&sample_table()),
            "CREATE TABLE [dbo].[Orders] (\n    \
             [Id] int IDENTITY(10,2) NOT NULL,\n    \
             [Name] nvarchar(50) COLLATE Latin1_General_CS_AS NULL,\n    \
             [Created] datetime2(3) NOT NULL CONSTRAINT [DF_Orders_Created] DEFAULT (getdate()),\n    \
             [Total] AS ([Qty]*[Price]) PERSISTED NOT NULL,\n    \
             [Tenth] AS ([Price]/(10)),\n    \
             CONSTRAINT [PK_Orders] PRIMARY KEY CLUSTERED ([Id]),\n    \
             CONSTRAINT [UQ_Orders_Name] UNIQUE NONCLUSTERED ([Name] DESC),\n    \
             CONSTRAINT [CK_Qty] CHECK ([Qty]>(0))\n)"
        );
    }

    #[test]
    fn column_definition_without_table_is_unnamed_default() {
        let c = Column { default: Some("0".into()), ..col("Qty", "int", false) };
        assert_eq!(D.column_definition(&c), "[Qty] int NOT NULL DEFAULT (0)");
        assert_eq!(D.add_column(&t("T"), &c), "ALTER TABLE [dbo].[T] ADD [Qty] int NOT NULL CONSTRAINT [DF_T_Qty] DEFAULT (0)");
    }

    #[test]
    fn primary_keys() {
        let mut pk = KeyConstraint { name: Some("PK_T".into()), columns: vec!["a".into(), "b".into()], clustered: Some(false) };
        assert_eq!(D.primary_key_clause(&pk), "CONSTRAINT [PK_T] PRIMARY KEY NONCLUSTERED ([a], [b])");
        assert_eq!(D.add_primary_key(&t("T"), &pk), "ALTER TABLE [dbo].[T] ADD CONSTRAINT [PK_T] PRIMARY KEY NONCLUSTERED ([a], [b])");
        assert_eq!(D.drop_primary_key(&t("T"), &pk), "ALTER TABLE [dbo].[T] DROP CONSTRAINT [PK_T]");
        pk.name = None;
        pk.clustered = None;
        assert_eq!(D.primary_key_clause(&pk), "PRIMARY KEY ([a], [b])");
        let drop = D.drop_primary_key(&t("T"), &pk);
        assert!(drop.contains("sys.key_constraints WHERE parent_object_id = OBJECT_ID(N'[dbo].[T]') AND type = 'PK'"));
        assert!(drop.contains("N'ALTER TABLE [dbo].[T] DROP CONSTRAINT ' + QUOTENAME(@pk)"));
    }

    #[test]
    fn indexes() {
        let tb = t("T");
        let mut ix = Index {
            name: "IX_T".into(),
            columns: vec![IndexColumn { name: "a".into(), descending: true }, IndexColumn { name: "b".into(), descending: false }],
            include: vec!["c".into()],
            filter: Some("([a]>(0))".into()),
            ..Default::default()
        };
        assert_eq!(
            D.create_index(&tb, &ix),
            "CREATE NONCLUSTERED INDEX [IX_T] ON [dbo].[T] ([a] DESC, [b]) INCLUDE ([c]) WHERE ([a]>(0))"
        );
        assert_eq!(D.drop_index(&tb, &ix), "DROP INDEX [IX_T] ON [dbo].[T]");

        ix.unique = true;
        ix.method = Some("clustered".into());
        ix.include.clear();
        ix.filter = None;
        assert_eq!(D.create_index(&tb, &ix), "CREATE UNIQUE CLUSTERED INDEX [IX_T] ON [dbo].[T] ([a] DESC, [b])");

        // Foreign access methods (from another engine) fall back to nonclustered.
        ix.method = Some("btree".into());
        assert!(D.create_index(&tb, &ix).starts_with("CREATE UNIQUE NONCLUSTERED INDEX"));

        ix.unique = false;
        ix.method = Some("columnstore".into());
        ix.include = vec!["c".into()];
        assert_eq!(D.create_index(&tb, &ix), "CREATE NONCLUSTERED COLUMNSTORE INDEX [IX_T] ON [dbo].[T] ([a], [b], [c])");

        ix.method = Some("clustered columnstore".into());
        assert_eq!(D.create_index(&tb, &ix), "CREATE CLUSTERED COLUMNSTORE INDEX [IX_T] ON [dbo].[T]");

        let uq = Index {
            name: "UQ_T".into(),
            columns: vec![IndexColumn { name: "a".into(), descending: false }],
            unique: true,
            constraint: true,
            method: Some("clustered".into()),
            ..Default::default()
        };
        assert_eq!(D.create_index(&tb, &uq), "ALTER TABLE [dbo].[T] ADD CONSTRAINT [UQ_T] UNIQUE CLUSTERED ([a])");
        assert_eq!(D.drop_index(&tb, &uq), "ALTER TABLE [dbo].[T] DROP CONSTRAINT [UQ_T]");
    }

    #[test]
    fn foreign_keys_and_checks() {
        let fk = ForeignKey {
            name: "FK_O_C".into(),
            columns: vec!["CustomerId".into()],
            ref_table: t("Customers"),
            ref_columns: vec!["Id".into()],
            on_delete: Some("CASCADE".into()),
            on_update: Some("SET NULL".into()),
        };
        assert_eq!(
            D.add_foreign_key(&t("Orders"), &fk),
            "ALTER TABLE [dbo].[Orders] ADD CONSTRAINT [FK_O_C] FOREIGN KEY ([CustomerId]) REFERENCES [dbo].[Customers] ([Id]) ON DELETE CASCADE ON UPDATE SET NULL"
        );
        assert_eq!(D.drop_foreign_key(&t("Orders"), &fk), "ALTER TABLE [dbo].[Orders] DROP CONSTRAINT [FK_O_C]");
        let ck = CheckConstraint { name: "CK".into(), expression: "[a]>(0)".into() };
        assert_eq!(D.add_check(&t("T"), &ck), "ALTER TABLE [dbo].[T] ADD CONSTRAINT [CK] CHECK ([a]>(0))");
    }

    #[test]
    fn rename_and_drop_table() {
        assert_eq!(D.rename_table(&t("Old"), "New"), "EXEC sp_rename N'[dbo].[Old]', N'New'");
        assert_eq!(D.rename_table(&t("O'Brien"), "N'x"), "EXEC sp_rename N'[dbo].[O''Brien]', N'N''x'");
        assert_eq!(D.drop_table(&t("T")), "DROP TABLE [dbo].[T]");
    }

    #[test]
    fn drop_column_drops_its_default_first() {
        let sql = D.drop_column(&t("T"), "Created");
        assert!(sql.starts_with("DECLARE @df sysname;"));
        assert!(sql.contains("WHERE dc.parent_object_id = OBJECT_ID(N'[dbo].[T]') AND c.name = N'Created';"));
        assert!(sql.contains("DECLARE @sql nvarchar(max) = N'ALTER TABLE [dbo].[T] DROP CONSTRAINT ' + QUOTENAME(@df);"));
        assert!(sql.contains("EXEC sp_executesql @sql;"));
        assert!(sql.ends_with("END;\nALTER TABLE [dbo].[T] DROP COLUMN [Created]"));
    }

    #[test]
    fn alter_column_type_and_nullability() {
        let from = col("Name", "nvarchar(50)", true);
        let to = Column { collation: Some("Latin1_General_BIN2".into()), ..col("Name", "nvarchar(100)", false) };
        assert_eq!(
            D.alter_column(&t("T"), &from, &to),
            vec!["ALTER TABLE [dbo].[T] ALTER COLUMN [Name] nvarchar(100) COLLATE Latin1_General_BIN2 NOT NULL"]
        );
        assert!(D.alter_column(&t("T"), &from, &from).is_empty());
    }

    #[test]
    fn alter_column_defaults() {
        let tb = t("T");
        let plain = col("Qty", "int", false);
        let with = |d: &str| Column { default: Some(d.into()), ..plain.clone() };

        // Added: no drop needed.
        assert_eq!(D.alter_column(&tb, &plain, &with("1")), vec!["ALTER TABLE [dbo].[T] ADD CONSTRAINT [DF_T_Qty] DEFAULT (1) FOR [Qty]"]);

        // Changed: dynamic drop, then add.
        let v = D.alter_column(&tb, &with("0"), &with("1"));
        assert_eq!(v.len(), 2);
        assert!(v[0].contains("FROM sys.default_constraints dc") && v[0].contains("c.name = N'Qty'"));
        assert_eq!(v[1], "ALTER TABLE [dbo].[T] ADD CONSTRAINT [DF_T_Qty] DEFAULT (1) FOR [Qty]");

        // Removed: only the drop.
        let v = D.alter_column(&tb, &with("0"), &plain);
        assert_eq!(v.len(), 1);
        assert!(v[0].starts_with("DECLARE @df sysname;"));

        // Type change with an unchanged default: drop, alter, re-add.
        let to = Column { data_type: "bigint".into(), ..with("0") };
        let v = D.alter_column(&tb, &with("0"), &to);
        assert_eq!(v.len(), 3);
        assert!(v[0].starts_with("DECLARE @df sysname;"));
        assert_eq!(v[1], "ALTER TABLE [dbo].[T] ALTER COLUMN [Qty] bigint NOT NULL");
        assert_eq!(v[2], "ALTER TABLE [dbo].[T] ADD CONSTRAINT [DF_T_Qty] DEFAULT (0) FOR [Qty]");
    }

    #[test]
    fn alter_column_computed_and_identity() {
        let tb = t("T");
        let from = Column { computed: Some("[a]+(1)".into()), ..col("c", "int", true) };
        let to = Column { computed: Some("[a]+(1) PERSISTED".into()), ..col("c", "int", true) };
        let v = D.alter_column(&tb, &from, &to);
        assert_eq!(v.len(), 2);
        assert!(v[0].ends_with("ALTER TABLE [dbo].[T] DROP COLUMN [c]"));
        assert_eq!(v[1], "ALTER TABLE [dbo].[T] ADD [c] AS ([a]+(1)) PERSISTED");
        // Only the (ignored) data type of a computed column differs.
        let same = Column { data_type: "bigint".into(), ..from.clone() };
        assert!(D.alter_column(&tb, &from, &same).is_empty());

        let id = Column { identity: Some(Identity::default()), ..col("Id", "int", false) };
        assert!(D.alter_column(&tb, &col("Id", "int", false), &id).is_empty(), "identity change needs a rebuild");
        let reseeded = Column { identity: Some(Identity { seed: 100, increment: 1 }), ..id.clone() };
        assert!(D.alter_column(&tb, &id, &reseeded).is_empty());
    }

    #[test]
    fn persisted_suffix() {
        assert_eq!(split_persisted("[a]*[b] PERSISTED"), ("[a]*[b]", true));
        assert_eq!(split_persisted("[a]*[b] persisted "), ("[a]*[b]", true));
        assert_eq!(split_persisted("[a]*[b]"), ("[a]*[b]", false));
        assert_eq!(split_persisted("[NOTPERSISTED]"), ("[NOTPERSISTED]", false));
    }

    #[test]
    fn replace_object_uses_create_or_alter() {
        let old = obj(ObjectKind::View, "v", "CREATE VIEW dbo.v AS SELECT 1 AS a");
        let new = obj(ObjectKind::View, "v", "-- header\n/* note /* nested */ */\ncreate   view dbo.v AS SELECT 2 AS a;");
        assert_eq!(
            D.replace_object(&old, &new),
            vec!["-- header\n/* note /* nested */ */\ncreate OR ALTER   view dbo.v AS SELECT 2 AS a"]
        );
        let p = obj(ObjectKind::Procedure, "p", "CREATE PROCEDURE dbo.p AS SELECT 1");
        assert_eq!(D.replace_object(&p, &p), vec!["CREATE OR ALTER PROCEDURE dbo.p AS SELECT 1"]);
        let already = obj(ObjectKind::Trigger, "tr", "CREATE OR ALTER TRIGGER dbo.tr ON dbo.t AFTER INSERT AS SELECT 1");
        assert_eq!(D.replace_object(&already, &already), vec![already.definition.clone()]);
    }

    #[test]
    fn replace_object_falls_back_to_drop_create() {
        let s1 = obj(ObjectKind::Sequence, "s", "CREATE SEQUENCE [dbo].[s] AS bigint START WITH 1 INCREMENT BY 1");
        let s2 = obj(ObjectKind::Sequence, "s", "CREATE SEQUENCE [dbo].[s] AS bigint START WITH 5 INCREMENT BY 1");
        assert_eq!(D.replace_object(&s1, &s2), vec!["DROP SEQUENCE [dbo].[s]".to_owned(), s2.definition.clone()]);

        let scalar = obj(ObjectKind::Function, "f", "CREATE FUNCTION dbo.f() RETURNS int AS BEGIN RETURN 1 END");
        let inline = obj(ObjectKind::Function, "f", "CREATE FUNCTION dbo.f() RETURNS TABLE AS RETURN SELECT 1 AS a");
        assert_eq!(D.replace_object(&scalar, &inline), vec!["DROP FUNCTION [dbo].[f]".to_owned(), inline.definition.clone()]);
        let scalar2 = obj(ObjectKind::Function, "f", "CREATE FUNCTION dbo.f() RETURNS bigint AS BEGIN RETURN 2 END");
        assert_eq!(D.replace_object(&scalar, &scalar2), vec!["CREATE OR ALTER FUNCTION dbo.f() RETURNS bigint AS BEGIN RETURN 2 END"]);

        // Not a CREATE statement: drop + create verbatim.
        let odd = obj(ObjectKind::View, "v", "ALTER VIEW dbo.v AS SELECT 1 AS a");
        assert_eq!(D.replace_object(&odd, &odd)[0], "DROP VIEW [dbo].[v]");
    }

    #[test]
    fn function_shapes() {
        assert_eq!(function_shape("CREATE FUNCTION f(@a int) RETURNS int AS BEGIN RETURN @a END"), FunctionShape::Scalar);
        assert_eq!(function_shape("CREATE FUNCTION f() RETURNS TABLE AS RETURN (SELECT 1 AS a)"), FunctionShape::InlineTable);
        assert_eq!(
            function_shape("CREATE FUNCTION f() RETURNS @t TABLE (a int) AS BEGIN INSERT @t VALUES (1); RETURN END"),
            FunctionShape::MultiStatementTable
        );
        // RETURNS inside comments, strings and brackets doesn't count.
        assert_eq!(
            function_shape("CREATE FUNCTION [returns table]() /* RETURNS TABLE */ -- RETURNS @x\nRETURNS nvarchar(10) AS BEGIN RETURN 'RETURNS TABLE' END"),
            FunctionShape::Scalar
        );
    }

    #[test]
    fn normalize_definition_strips_or_alter() {
        assert_eq!(MssqlDialect::normalize_definition("  CREATE OR ALTER VIEW v AS SELECT 1;\r\n"), "CREATE VIEW v AS SELECT 1");
        assert_eq!(MssqlDialect::normalize_definition("/* c */ create or\n alter proc p AS SELECT 1"), "/* c */ create proc p AS SELECT 1");
        assert_eq!(MssqlDialect::normalize_definition("CREATE VIEW orders AS SELECT 1"), "CREATE VIEW orders AS SELECT 1");
        assert_eq!(MssqlDialect::normalize_definition("CREATE PROCEDURE ordering AS SELECT 1"), "CREATE PROCEDURE ordering AS SELECT 1");
    }

    #[test]
    fn trivia_skipping() {
        assert_eq!(skip_trivia("  -- a\n  /* b */ X"), 17);
        assert_eq!(skip_trivia("-- only a comment"), 17);
        assert_eq!(skip_trivia("/* unterminated"), 15);
        assert!(create_keyword("CREATEVIEW").is_none());
        assert_eq!(create_keyword("\n\tCreate VIEW"), Some((2, 8)));
    }

    #[test]
    fn data_statements() {
        let tb = t("T");
        assert_eq!(D.identity_insert(&tb, true).as_deref(), Some("SET IDENTITY_INSERT [dbo].[T] ON"));
        assert_eq!(D.identity_insert(&tb, false).as_deref(), Some("SET IDENTITY_INSERT [dbo].[T] OFF"));
        assert_eq!(D.disable_foreign_keys(std::slice::from_ref(&tb)), vec!["ALTER TABLE [dbo].[T] NOCHECK CONSTRAINT ALL"]);
        assert_eq!(D.enable_foreign_keys(std::slice::from_ref(&tb)), vec!["ALTER TABLE [dbo].[T] WITH CHECK CHECK CONSTRAINT ALL"]);
        assert_eq!(
            D.insert(&tb, &["a".into(), "b".into()], &[Value::Bool(true), Value::Text("x".into())]),
            "INSERT INTO [dbo].[T] ([a], [b]) VALUES (1, N'x')"
        );
        assert_eq!(
            D.update(&tb, &[("b".into(), Value::Null)], &[("a".into(), Value::Int(1))]),
            "UPDATE [dbo].[T] SET [b] = NULL WHERE [a] = 1"
        );
        assert_eq!(D.delete(&tb, &[("a".into(), Value::Date("2024-01-02".into()))]), "DELETE FROM [dbo].[T] WHERE [a] = '20240102'");
    }

    #[test]
    fn code_objects() {
        let v = obj(ObjectKind::View, "v", "CREATE VIEW dbo.v AS SELECT 1 AS a;  ");
        assert_eq!(D.create_object(&v), vec!["CREATE VIEW dbo.v AS SELECT 1 AS a"]);
        assert_eq!(D.drop_object(&v), "DROP VIEW [dbo].[v]");
        assert_eq!(D.drop_object(&obj(ObjectKind::Trigger, "tr", "")), "DROP TRIGGER [dbo].[tr]");
        assert_eq!(D.drop_object(&obj(ObjectKind::Sequence, "s", "")), "DROP SEQUENCE [dbo].[s]");
    }
}
