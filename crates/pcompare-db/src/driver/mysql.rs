//! MySQL / MariaDB driver over one sqlx `MySqlConnection`.
//!
//! Everything goes through the text protocol (`raw_sql`), so every cell
//! arrives as its server-rendered text and is converted by declared column
//! type. That keeps zero dates (`0000-00-00`), which the binary protocol
//! reports as NULL, and gives floats their shortest decimal form.
//!
//! Model representation (see also the `dialect::mysql` docs):
//! - every object has `schema = ""`; `Schema::database` is the connected db.
//! - `data_type`: lower-cased COLUMN_TYPE (enum/set values keep their case)
//!   with integer display widths stripped (`int(11)` → `int`) except
//!   `tinyint(1)` and ZEROFILL types, so MySQL 8 and MariaDB agree.
//! - `default`: SQL expression text. String literals are re-quoted
//!   (`'abc'`), numbers bare, `CURRENT_TIMESTAMP[(n)]` upper-cased, other
//!   expression defaults parenthesised (`(curdate())`). `ON UPDATE
//!   CURRENT_TIMESTAMP` is folded in (see the dialect docs).
//! - `computed`: `(expr) STORED` / `(expr) VIRTUAL`.
//! - index parts: `col`, `col(10)` for prefixes, or the expression text of a
//!   functional key part. Every UNIQUE index is `constraint: true` (MySQL
//!   doesn't distinguish the two).
//! - `fetch_rows`: integers (incl. `tinyint(1)`, `bit(n)`, `year`) → `Int`,
//!   `UInt` only above `i64::MAX`; zero / partial-zero dates → `Text` with
//!   the server's rendering (`0000-00-00 00:00:00`). Sessions run with
//!   `time_zone = '+00:00'`, so TIMESTAMP values read as UTC.

use crate::connection::{ConnectionConfig, TlsMode};
use crate::dialect::{Dialect, MysqlDialect};
use crate::driver::{db_err, statement_err};
use crate::model::*;
use crate::value::Value;
use pcompare_core::{CoreError, Result};
use sqlx::mysql::{MySqlConnectOptions, MySqlConnection, MySqlRow, MySqlSslMode};
use sqlx::{AssertSqlSafe, ConnectOptions, Row};
use std::collections::BTreeMap;

const SYSTEM_DATABASES: &[&str] = &["information_schema", "mysql", "performance_schema", "sys"];

pub struct Conn {
    conn: MySqlConnection,
    database: String,
    /// `"MySQL 8.0.36"` / `"MariaDB 10.11.6"`.
    label: String,
    mariadb: bool,
    version: (u32, u32, u32),
}

/// A string literal for the queries below (the session has backslash
/// escapes enabled, matching the dialect).
fn lit(s: &str) -> String {
    MysqlDialect.literal(&Value::Text(s.to_owned()))
}

fn ident(s: &str) -> String {
    MysqlDialect.quote_ident(s)
}

/// Cell `i` of a text-protocol row as UTF-8 (lossy), `None` for NULL.
fn cell(row: &MySqlRow, i: usize) -> Result<Option<String>> {
    let v: Option<&[u8]> = row.try_get_unchecked(i).map_err(|e| db_err("read column", e))?;
    Ok(v.map(|b| String::from_utf8_lossy(b).into_owned()))
}

fn text(row: &MySqlRow, i: usize) -> Result<String> {
    Ok(cell(row, i)?.unwrap_or_default())
}

impl Conn {
    pub async fn connect(cfg: &ConnectionConfig) -> Result<Conn> {
        let mut opts = MySqlConnectOptions::new()
            .port(cfg.port_or_default())
            .username(&cfg.user)
            .ssl_mode(match cfg.tls {
                TlsMode::Disable => MySqlSslMode::Disabled,
                TlsMode::Prefer => MySqlSslMode::Preferred,
                TlsMode::Require => MySqlSslMode::Required,
            })
            // `||` must keep its default meaning: routines record the
            // creating session's sql_mode.
            .pipes_as_concat(false)
            .timezone(Some("+00:00".to_owned()));
        if !cfg.host.is_empty() {
            opts = opts.host(&cfg.host);
        }
        if !cfg.password.is_empty() {
            opts = opts.password(&cfg.password);
        }
        if !cfg.database.is_empty() {
            opts = opts.database(&cfg.database);
        }
        let conn = opts.connect().await.map_err(|e| db_err("connect", e))?;
        let mut c = Conn { conn, database: cfg.database.clone(), label: String::new(), mariadb: false, version: (0, 0, 0) };
        // Literals in scripts and in our own queries escape backslashes.
        c.run(
            "SET SESSION sql_mode = TRIM(BOTH ',' FROM REPLACE(CONCAT(',', @@SESSION.sql_mode, ','), \
             ',NO_BACKSLASH_ESCAPES,', ','))",
        )
        .await?;
        let rows = c.rows("SELECT VERSION()".into(), "read server version").await?;
        let raw = match rows.first() {
            Some(r) => text(r, 0)?,
            None => String::new(),
        };
        let (mariadb, version, label) = parse_version(&raw);
        (c.mariadb, c.version, c.label) = (mariadb, version, label);
        Ok(c)
    }

    pub async fn server_version(&mut self) -> Result<String> {
        Ok(self.label.clone())
    }

    pub async fn list_databases(&mut self) -> Result<Vec<String>> {
        let rows = self.rows("SHOW DATABASES".into(), "list databases").await?;
        let mut out = Vec::new();
        for r in &rows {
            let name = text(r, 0)?;
            if !SYSTEM_DATABASES.iter().any(|s| s.eq_ignore_ascii_case(&name)) {
                out.push(name);
            }
        }
        out.sort();
        Ok(out)
    }

    pub async fn load_schema(&mut self) -> Result<Schema> {
        if self.database.is_empty() {
            return Err(CoreError::Other("no database selected: choose a database for this MySQL connection".into()));
        }
        let db = lit(&self.database);
        let mut tables = self.load_tables(&db).await?;
        self.load_indexes(&db, &mut tables).await?;
        self.load_foreign_keys(&db, &mut tables).await?;
        self.load_checks(&db, &mut tables).await?;
        let mut objects = self.load_objects(&db).await?;
        objects.sort_by(|a, b| a.name.cmp(&b.name).then(a.kind.cmp(&b.kind)));
        let mut tables: Vec<Table> = tables.into_values().collect();
        for t in &mut tables {
            t.indexes.sort_by(|a, b| a.name.cmp(&b.name));
            t.foreign_keys.sort_by(|a, b| a.name.cmp(&b.name));
            t.checks.sort_by(|a, b| a.name.cmp(&b.name));
        }
        Ok(Schema {
            engine: Some(Engine::Mysql),
            database: self.database.clone(),
            server_version: self.label.clone(),
            tables,
            objects,
        })
    }

    pub async fn fetch_rows(&mut self, table: &ObjectName, columns: &[String], order_by: &[String]) -> Result<Vec<Vec<Value>>> {
        let schema = if table.schema.is_empty() { &self.database } else { &table.schema };
        let meta = self
            .rows(
                format!(
                    "SELECT COLUMN_NAME, DATA_TYPE, COLUMN_TYPE FROM information_schema.COLUMNS \
                     WHERE TABLE_SCHEMA = {} AND TABLE_NAME = {}",
                    lit(schema),
                    lit(&table.name)
                ),
                "read column types",
            )
            .await?;
        let mut types = Vec::with_capacity(meta.len());
        for r in &meta {
            types.push((text(r, 0)?, text(r, 1)?.to_ascii_lowercase(), text(r, 2)?.to_ascii_lowercase()));
        }
        let mut kinds = Vec::with_capacity(columns.len());
        let mut select = Vec::with_capacity(columns.len());
        for c in columns {
            let (_, data_type, column_type) = types
                .iter()
                .find(|(n, _, _)| n.eq_ignore_ascii_case(c))
                .ok_or_else(|| CoreError::Other(format!("column `{c}` not found in `{table}`")))?;
            let kind = CellKind::of(data_type, column_type);
            select.push(if kind == CellKind::Geometry { format!("ST_AsText({})", ident(c)) } else { ident(c) });
            kinds.push(kind);
        }
        let mut sql = format!("SELECT {} FROM {}", select.join(", "), MysqlDialect.qualified(table));
        if !order_by.is_empty() {
            sql.push_str(&format!(" ORDER BY {}", MysqlDialect.ident_list(order_by)));
        }
        let rows = self.rows(sql, &format!("read rows of `{table}`")).await?;
        let mut out = Vec::with_capacity(rows.len());
        for r in &rows {
            let mut vals = Vec::with_capacity(kinds.len());
            for (i, kind) in kinds.iter().enumerate() {
                let raw: Option<&[u8]> = r.try_get_unchecked(i).map_err(|e| db_err("read column", e))?;
                vals.push(match raw {
                    None => Value::Null,
                    Some(b) => kind.convert(b).map_err(|e| db_err(&format!("column `{}`", columns[i]), e))?,
                });
            }
            out.push(vals);
        }
        Ok(out)
    }

    /// DDL commits implicitly in MySQL, so with `transactional` only data
    /// statements roll back on failure.
    pub async fn execute(&mut self, statements: &[String], transactional: bool) -> Result<()> {
        if transactional {
            self.run("START TRANSACTION").await?;
        }
        for (i, sql) in statements.iter().enumerate() {
            if let Err(e) = sqlx::raw_sql(AssertSqlSafe(sql.as_str())).execute(&mut self.conn).await {
                if transactional {
                    let _ = sqlx::raw_sql("ROLLBACK").execute(&mut self.conn).await;
                }
                return Err(statement_err(i, sql, e));
            }
        }
        if transactional {
            self.run("COMMIT").await?;
        }
        Ok(())
    }

    // ----- helpers -----

    async fn run(&mut self, sql: &'static str) -> Result<()> {
        sqlx::raw_sql(sql).execute(&mut self.conn).await.map_err(|e| db_err(sql, e))?;
        Ok(())
    }

    async fn rows(&mut self, sql: String, context: &str) -> Result<Vec<MySqlRow>> {
        sqlx::raw_sql(AssertSqlSafe(sql)).fetch_all(&mut self.conn).await.map_err(|e| db_err(context, e))
    }

    fn at_least(&self, v: (u32, u32, u32)) -> bool {
        self.version >= v
    }

    /// MariaDB 10.2.7+ reports COLUMN_DEFAULT as SQL (quoted literals, `NULL`).
    fn quoted_defaults(&self) -> bool {
        self.mariadb && self.at_least((10, 2, 7))
    }

    /// MySQL 8 returns expression text in information_schema (defaults,
    /// generated columns, functional indexes, checks) with an extra level of
    /// backslash escaping: `_utf8mb4\'it\\\'s\'`.
    fn expr(&self, s: String) -> String {
        if self.mariadb { s } else { unescape(&s) }
    }

    async fn load_tables(&mut self, db: &str) -> Result<BTreeMap<String, Table>> {
        let rows = self
            .rows(
                format!(
                    "SELECT TABLE_NAME, TABLE_COLLATION FROM information_schema.TABLES \
                     WHERE TABLE_SCHEMA = {db} AND TABLE_TYPE IN ('BASE TABLE', 'SYSTEM VERSIONED')"
                ),
                "list tables",
            )
            .await?;
        let mut tables = BTreeMap::new();
        let mut collations = BTreeMap::new();
        for r in &rows {
            let name = text(r, 0)?;
            collations.insert(name.clone(), cell(r, 1)?);
            tables.insert(name.clone(), Table { name: ObjectName::new("", name), ..Default::default() });
        }
        let rows = self
            .rows(
                format!(
                    "SELECT TABLE_NAME, COLUMN_NAME, COLUMN_TYPE, IS_NULLABLE, COLUMN_DEFAULT, EXTRA, \
                     GENERATION_EXPRESSION, COLLATION_NAME FROM information_schema.COLUMNS \
                     WHERE TABLE_SCHEMA = {db} ORDER BY TABLE_NAME, ORDINAL_POSITION"
                ),
                "read columns",
            )
            .await?;
        for r in &rows {
            let tname = text(r, 0)?;
            let Some(table) = tables.get_mut(&tname) else { continue }; // a view
            let data_type = normalize_type(&text(r, 2)?);
            let extra = text(r, 5)?.to_ascii_lowercase();
            let generation = cell(r, 6)?.filter(|g| !g.is_empty());
            let computed = match generation {
                Some(g) if extra.contains("generated") && !extra.contains("default_generated") => {
                    let kind = if extra.contains("stored") || extra.contains("persistent") { "STORED" } else { "VIRTUAL" };
                    Some(format!("({}) {kind}", self.expr(g)))
                }
                _ => None,
            };
            let mut raw_default = cell(r, 4)?;
            if !self.quoted_defaults() && extra.contains("default_generated") {
                raw_default = raw_default.map(|d| self.expr(d));
            }
            let default = if computed.is_some() {
                None
            } else {
                normalize_default(raw_default.as_deref(), &extra, &data_type, self.quoted_defaults())
            };
            let collation = cell(r, 7)?.filter(|c| collations.get(&tname).is_some_and(|t| t.as_deref() != Some(c)));
            table.columns.push(Column {
                name: text(r, 1)?,
                data_type,
                nullable: text(r, 3)? == "YES",
                default,
                identity: extra.contains("auto_increment").then(Identity::default),
                computed,
                collation,
            });
        }
        Ok(tables)
    }

    async fn load_indexes(&mut self, db: &str, tables: &mut BTreeMap<String, Table>) -> Result<()> {
        let expression = if !self.mariadb && self.at_least((8, 0, 13)) { "EXPRESSION" } else { "NULL" };
        let rows = self
            .rows(
                format!(
                    "SELECT TABLE_NAME, INDEX_NAME, NON_UNIQUE, COLUMN_NAME, SUB_PART, COLLATION, INDEX_TYPE, \
                     {expression} FROM information_schema.STATISTICS WHERE TABLE_SCHEMA = {db} \
                     ORDER BY TABLE_NAME, INDEX_NAME, SEQ_IN_INDEX"
                ),
                "read indexes",
            )
            .await?;
        for r in &rows {
            let Some(table) = tables.get_mut(&text(r, 0)?) else { continue };
            let index_name = text(r, 1)?;
            let part = match (cell(r, 3)?, cell(r, 4)?, cell(r, 7)?) {
                (Some(col), Some(len), _) => format!("{col}({len})"),
                (Some(col), None, _) => col,
                (None, _, Some(expr)) => self.expr(expr),
                (None, _, None) => continue,
            };
            if index_name == "PRIMARY" {
                table
                    .primary_key
                    .get_or_insert_with(|| KeyConstraint { name: None, columns: Vec::new(), clustered: None })
                    .columns
                    .push(part);
                continue;
            }
            let descending = cell(r, 5)?.as_deref() == Some("D");
            let column = IndexColumn { name: part, descending };
            if let Some(idx) = table.indexes.iter_mut().find(|i| i.name == index_name) {
                idx.columns.push(column);
                continue;
            }
            let unique = text(r, 2)? == "0";
            let method = match text(r, 6)?.to_ascii_lowercase().as_str() {
                "btree" | "" => None,
                m => Some(m.to_owned()),
            };
            table.indexes.push(Index {
                name: index_name,
                columns: vec![column],
                unique,
                constraint: unique,
                method,
                filter: None,
                include: Vec::new(),
            });
        }
        Ok(())
    }

    async fn load_foreign_keys(&mut self, db: &str, tables: &mut BTreeMap<String, Table>) -> Result<()> {
        let rows = self
            .rows(
                format!(
                    "SELECT k.TABLE_NAME, k.CONSTRAINT_NAME, k.COLUMN_NAME, k.REFERENCED_TABLE_SCHEMA, \
                     k.REFERENCED_TABLE_NAME, k.REFERENCED_COLUMN_NAME, r.DELETE_RULE, r.UPDATE_RULE \
                     FROM information_schema.KEY_COLUMN_USAGE k \
                     JOIN information_schema.REFERENTIAL_CONSTRAINTS r \
                       ON r.CONSTRAINT_SCHEMA = k.CONSTRAINT_SCHEMA AND r.CONSTRAINT_NAME = k.CONSTRAINT_NAME \
                      AND r.TABLE_NAME = k.TABLE_NAME \
                     WHERE k.TABLE_SCHEMA = {db} AND k.REFERENCED_TABLE_NAME IS NOT NULL \
                     ORDER BY k.TABLE_NAME, k.CONSTRAINT_NAME, k.ORDINAL_POSITION"
                ),
                "read foreign keys",
            )
            .await?;
        for r in &rows {
            let Some(table) = tables.get_mut(&text(r, 0)?) else { continue };
            let name = text(r, 1)?;
            let (column, ref_column) = (text(r, 2)?, text(r, 5)?);
            if let Some(fk) = table.foreign_keys.iter_mut().find(|f| f.name == name) {
                fk.columns.push(column);
                fk.ref_columns.push(ref_column);
                continue;
            }
            // Cross-database references keep their database as the schema.
            let ref_db = text(r, 3)?;
            let ref_schema = if ref_db == self.database { String::new() } else { ref_db };
            table.foreign_keys.push(ForeignKey {
                name,
                columns: vec![column],
                ref_table: ObjectName::new(ref_schema, text(r, 4)?),
                ref_columns: vec![ref_column],
                on_delete: fk_action(&text(r, 6)?),
                on_update: fk_action(&text(r, 7)?),
            });
        }
        Ok(())
    }

    async fn load_checks(&mut self, db: &str, tables: &mut BTreeMap<String, Table>) -> Result<()> {
        let sql = if self.mariadb {
            if !self.at_least((10, 2, 22)) {
                return Ok(());
            }
            format!(
                "SELECT TABLE_NAME, CONSTRAINT_NAME, CHECK_CLAUSE FROM information_schema.CHECK_CONSTRAINTS \
                 WHERE CONSTRAINT_SCHEMA = {db}"
            )
        } else {
            if !self.at_least((8, 0, 16)) {
                return Ok(());
            }
            format!(
                "SELECT t.TABLE_NAME, c.CONSTRAINT_NAME, c.CHECK_CLAUSE \
                 FROM information_schema.TABLE_CONSTRAINTS t \
                 JOIN information_schema.CHECK_CONSTRAINTS c \
                   ON c.CONSTRAINT_SCHEMA = t.CONSTRAINT_SCHEMA AND c.CONSTRAINT_NAME = t.CONSTRAINT_NAME \
                 WHERE t.TABLE_SCHEMA = {db} AND t.CONSTRAINT_TYPE = 'CHECK'"
            )
        };
        for r in &self.rows(sql, "read check constraints").await? {
            let Some(table) = tables.get_mut(&text(r, 0)?) else { continue };
            table.checks.push(CheckConstraint { name: text(r, 1)?, expression: self.expr(text(r, 2)?) });
        }
        Ok(())
    }

    async fn load_objects(&mut self, db: &str) -> Result<Vec<CodeObject>> {
        // (kind, name, trigger table)
        let mut found: Vec<(ObjectKind, String, Option<String>)> = Vec::new();
        for r in &self.rows(format!("SELECT TABLE_NAME FROM information_schema.VIEWS WHERE TABLE_SCHEMA = {db}"), "list views").await? {
            found.push((ObjectKind::View, text(r, 0)?, None));
        }
        let routines = format!(
            "SELECT ROUTINE_NAME, ROUTINE_TYPE FROM information_schema.ROUTINES \
             WHERE ROUTINE_SCHEMA = {db} AND ROUTINE_TYPE IN ('PROCEDURE', 'FUNCTION')"
        );
        for r in &self.rows(routines, "list routines").await? {
            let kind = if text(r, 1)? == "PROCEDURE" { ObjectKind::Procedure } else { ObjectKind::Function };
            found.push((kind, text(r, 0)?, None));
        }
        let triggers = format!("SELECT TRIGGER_NAME, EVENT_OBJECT_TABLE FROM information_schema.TRIGGERS WHERE TRIGGER_SCHEMA = {db}");
        for r in &self.rows(triggers, "list triggers").await? {
            found.push((ObjectKind::Trigger, text(r, 0)?, Some(text(r, 1)?)));
        }

        let qualifier = format!("{}.", ident(&self.database));
        let mut objects = Vec::with_capacity(found.len());
        for (kind, name, table) in found {
            let (show, column) = match kind {
                ObjectKind::View => ("VIEW", "Create View"),
                ObjectKind::Procedure => ("PROCEDURE", "Create Procedure"),
                ObjectKind::Function => ("FUNCTION", "Create Function"),
                _ => ("TRIGGER", "SQL Original Statement"),
            };
            let sql = format!("SHOW CREATE {show} {}.{}", ident(&self.database), ident(&name));
            let rows = self.rows(sql, &format!("read definition of {} `{name}`", kind.label().to_lowercase())).await?;
            let def: Option<&[u8]> = match rows.first() {
                Some(r) => r.try_get_unchecked(column).map_err(|e| db_err("read definition", e))?,
                None => None,
            };
            let Some(def) = def else {
                return Err(CoreError::Other(format!(
                    "cannot read the definition of {} `{name}` (missing privilege?)",
                    kind.label().to_lowercase()
                )));
            };
            let mut definition = strip_create_noise(String::from_utf8_lossy(def).trim());
            if kind == ObjectKind::View {
                definition = definition.replace(&qualifier, "");
            }
            objects.push(CodeObject {
                kind,
                name: ObjectName::new("", name),
                definition,
                table: table.map(|t| ObjectName::new("", t)),
            });
        }
        Ok(objects)
    }
}

/// `(is_mariadb, (major, minor, patch), label)` from `SELECT VERSION()`.
fn parse_version(raw: &str) -> (bool, (u32, u32, u32), String) {
    let mariadb = raw.to_ascii_lowercase().contains("mariadb");
    // Old MariaDB servers prefix a replication-compatibility "5.5.5-".
    let v = if mariadb { raw.strip_prefix("5.5.5-").unwrap_or(raw) } else { raw };
    let numeric: String = v.chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
    let mut parts = numeric.split('.').map(|p| p.parse::<u32>().unwrap_or(0));
    let version = (parts.next().unwrap_or(0), parts.next().unwrap_or(0), parts.next().unwrap_or(0));
    let label = format!("{} {}", if mariadb { "MariaDB" } else { "MySQL" }, numeric.trim_end_matches('.'));
    (mariadb, version, label)
}

/// Lower-cases COLUMN_TYPE outside quoted enum/set values and strips integer
/// display widths (`int(11) unsigned` → `int unsigned`), keeping `tinyint(1)`
/// and ZEROFILL widths.
fn normalize_type(column_type: &str) -> String {
    let mut t = String::with_capacity(column_type.len());
    let mut quoted = false;
    for ch in column_type.trim().chars() {
        if ch == '\'' {
            quoted = !quoted;
        }
        t.push(if quoted { ch } else { ch.to_ascii_lowercase() });
    }
    const INTS: &[&str] = &["tinyint", "smallint", "mediumint", "int", "integer", "bigint", "year"];
    let Some(open) = t.find('(') else { return t };
    let base = &t[..open];
    if !INTS.contains(&base) {
        return t;
    }
    let Some(close) = t[open..].find(')').map(|i| open + i) else { return t };
    let width = &t[open + 1..close];
    let rest = &t[close + 1..];
    if (base == "tinyint" && width == "1") || rest.contains("zerofill") {
        return t;
    }
    format!("{base}{rest}")
}

/// `CURRENT_TIMESTAMP`, `current_timestamp()`, `now(3)`, ... →
/// `CURRENT_TIMESTAMP` / `CURRENT_TIMESTAMP(3)`; `None` for anything else.
fn normalize_timestamp_fn(s: &str) -> Option<String> {
    let s = s.trim().to_ascii_lowercase();
    let (name, args) = match s.find('(') {
        Some(i) => (&s[..i], s[i + 1..].strip_suffix(')')?),
        None => (s.as_str(), ""),
    };
    if !matches!(name.trim(), "current_timestamp" | "now" | "localtimestamp" | "localtime") {
        return None;
    }
    let args = args.trim();
    match args {
        "" | "0" => Some("CURRENT_TIMESTAMP".into()),
        n if n.bytes().all(|b| b.is_ascii_digit()) => Some(format!("CURRENT_TIMESTAMP({n})")),
        _ => None,
    }
}

fn is_number(s: &str) -> bool {
    let s = s.strip_prefix(['-', '+']).unwrap_or(s);
    let mut digits = false;
    let mut chars = s.chars().peekable();
    while chars.peek().is_some_and(|c| c.is_ascii_digit()) {
        chars.next();
        digits = true;
    }
    if chars.peek() == Some(&'.') {
        chars.next();
        while chars.peek().is_some_and(|c| c.is_ascii_digit()) {
            chars.next();
            digits = true;
        }
    }
    if digits && matches!(chars.peek(), Some('e' | 'E')) {
        chars.next();
        if matches!(chars.peek(), Some('-' | '+')) {
            chars.next();
        }
        let mut exp = false;
        while chars.peek().is_some_and(|c| c.is_ascii_digit()) {
            chars.next();
            exp = true;
        }
        return exp && chars.next().is_none();
    }
    digits && chars.next().is_none()
}

fn is_numeric_type(data_type: &str) -> bool {
    const NUMERIC: &[&str] =
        &["tinyint", "smallint", "mediumint", "int", "integer", "bigint", "decimal", "numeric", "float", "double", "real", "year"];
    let base = data_type.split(['(', ' ']).next().unwrap_or("");
    NUMERIC.contains(&base)
}

/// True when `s` is one parenthesised group: `(a + b)` but not `(a) + (b)`.
fn is_wrapped(s: &str) -> bool {
    if !s.starts_with('(') || !s.ends_with(')') {
        return false;
    }
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    let mut chars = s.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        match quote {
            Some(q) if c == '\\' && q != '`' => {
                chars.next();
            }
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None => match c {
                '\'' | '"' | '`' => quote = Some(c),
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 && i != s.len() - 1 {
                        return false;
                    }
                }
                _ => {}
            },
        }
    }
    depth == 0
}

/// Undoes one level of MySQL string escaping (`\'` → `'`, `\\` → `\`, `\n`).
fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('0') => out.push('\0'),
            Some('Z') => out.push('\x1a'),
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}

/// `'it''s'` / `'it\'s'` → `it's` (`s` is quoted, at least 2 chars).
fn unquote(s: &str) -> String {
    let inner = &s[1..s.len() - 1];
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' if chars.peek() == Some(&'\'') => {
                chars.next();
                out.push('\'');
            }
            '\\' => match chars.next() {
                Some('n') => out.push('\n'),
                Some('r') => out.push('\r'),
                Some('t') => out.push('\t'),
                Some('0') => out.push('\0'),
                Some('Z') => out.push('\x1a'),
                Some(o) => out.push(o),
                None => out.push('\\'),
            },
            c => out.push(c),
        }
    }
    out
}

/// COLUMN_DEFAULT + EXTRA → model default text (see the module docs).
/// `quoted` = MariaDB 10.2.7+ style, where COLUMN_DEFAULT is already SQL.
fn normalize_default(raw: Option<&str>, extra: &str, data_type: &str, quoted: bool) -> Option<String> {
    let on_update = extra
        .find("on update ")
        .map(|i| &extra[i + "on update ".len()..])
        .map(|f| {
            let f = f.split_whitespace().next().unwrap_or(f);
            normalize_timestamp_fn(f).unwrap_or_else(|| f.to_ascii_uppercase())
        });
    let base = raw.and_then(|d| {
        let ts = normalize_timestamp_fn(d);
        if quoted {
            if d == "NULL" {
                None
            } else if d.starts_with('\'') && d.len() >= 2 {
                Some(MysqlDialect.literal(&Value::Text(unquote(d))))
            } else if is_number(d) || d.starts_with("b'") || d.starts_with("0x") {
                Some(d.to_owned())
            } else {
                Some(ts.unwrap_or_else(|| if is_wrapped(d) { d.to_owned() } else { format!("({d})") }))
            }
        } else if extra.contains("default_generated") {
            Some(ts.unwrap_or_else(|| if is_wrapped(d) { d.to_owned() } else { format!("({d})") }))
        } else if let Some(ts) = ts.filter(|_| data_type.starts_with("timestamp") || data_type.starts_with("datetime")) {
            Some(ts) // MySQL 5.7 reports CURRENT_TIMESTAMP without DEFAULT_GENERATED
        } else if d.starts_with("b'") || (is_numeric_type(data_type) && is_number(d)) {
            Some(d.to_owned())
        } else {
            Some(MysqlDialect.literal(&Value::Text(d.to_owned())))
        }
    });
    match (base, on_update) {
        (Some(d), Some(u)) => Some(format!("{d} ON UPDATE {u}")),
        (None, Some(u)) => Some(format!("ON UPDATE {u}")),
        (d, None) => d,
    }
}

fn fk_action(rule: &str) -> Option<String> {
    let rule = rule.trim().to_ascii_uppercase();
    // MySQL's default is NO ACTION, MariaDB reports RESTRICT; both mean the same in InnoDB.
    (!matches!(rule.as_str(), "" | "NO ACTION" | "RESTRICT")).then_some(rule)
}

/// Drops defaults from a `SHOW CREATE` header so identical objects created
/// by different users compare equal: `DEFINER=...` always,
/// `ALGORITHM=UNDEFINED` and `SQL SECURITY DEFINER` (view defaults).
fn strip_create_noise(def: &str) -> String {
    if !def.get(..7).is_some_and(|p| p.eq_ignore_ascii_case("CREATE ")) {
        return def.to_owned();
    }
    let mut rest = def[7..].trim_start();
    let mut kept: Vec<&str> = Vec::new();
    loop {
        let upper = rest.get(..14).unwrap_or(rest).to_ascii_uppercase();
        if upper.starts_with("ALGORITHM=") {
            let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
            let tok = &rest[..end];
            if !tok.eq_ignore_ascii_case("ALGORITHM=UNDEFINED") {
                kept.push(tok);
            }
            rest = rest[end..].trim_start();
        } else if upper.starts_with("DEFINER=") {
            let mut i = "DEFINER=".len();
            i += account_part_len(&rest[i..]);
            if rest[i..].starts_with('@') {
                i += 1 + account_part_len(&rest[i + 1..]);
            }
            rest = rest[i..].trim_start();
        } else if upper.starts_with("SQL SECURITY ") {
            let after = rest["SQL SECURITY ".len()..].trim_start();
            let end = after.find(char::is_whitespace).unwrap_or(after.len());
            let consumed = rest.len() - after.len() + end;
            if !after[..end].eq_ignore_ascii_case("DEFINER") {
                kept.push(&rest[..consumed]);
            }
            rest = rest[consumed..].trim_start();
        } else {
            break;
        }
    }
    let mut out = String::from("CREATE ");
    for k in kept {
        out.push_str(k);
        out.push(' ');
    }
    out.push_str(rest);
    out
}

/// Length of one `user` / `host` part of an account: quoted with backticks
/// or quotes (doubled to escape), or bare up to `@` / whitespace.
fn account_part_len(s: &str) -> usize {
    let Some(q) = s.chars().next().filter(|c| matches!(c, '`' | '\'' | '"')) else {
        return s.find(|c: char| c == '@' || c.is_whitespace()).unwrap_or(s.len());
    };
    let b = s.as_bytes();
    let mut i = 1;
    while i < b.len() {
        if b[i] == q as u8 {
            if b.get(i + 1) == Some(&(q as u8)) {
                i += 2;
                continue;
            }
            return i + 1;
        }
        i += 1;
    }
    s.len()
}

/// How a column's text-protocol cells are converted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CellKind {
    Int,
    UInt,
    Bit,
    Decimal,
    Float,
    Text,
    Bytes,
    Date,
    Time,
    DateTime,
    Json,
    Geometry,
    Other,
}

impl CellKind {
    fn of(data_type: &str, column_type: &str) -> CellKind {
        match data_type {
            "tinyint" | "smallint" | "mediumint" | "int" | "integer" | "bigint" => {
                if column_type.contains("unsigned") { CellKind::UInt } else { CellKind::Int }
            }
            "year" => CellKind::Int,
            "bit" => CellKind::Bit,
            "decimal" | "numeric" => CellKind::Decimal,
            "float" | "double" | "real" => CellKind::Float,
            "char" | "varchar" | "tinytext" | "text" | "mediumtext" | "longtext" | "enum" | "set" => CellKind::Text,
            "binary" | "varbinary" | "tinyblob" | "blob" | "mediumblob" | "longblob" => CellKind::Bytes,
            "date" => CellKind::Date,
            "time" => CellKind::Time,
            "datetime" | "timestamp" => CellKind::DateTime,
            "json" => CellKind::Json,
            "geometry" | "point" | "linestring" | "polygon" | "multipoint" | "multilinestring" | "multipolygon"
            | "geometrycollection" | "geomcollection" => CellKind::Geometry,
            _ => CellKind::Other,
        }
    }

    fn convert(self, b: &[u8]) -> std::result::Result<Value, String> {
        let s = || String::from_utf8_lossy(b).into_owned();
        let num = || std::str::from_utf8(b).map_err(|e| e.to_string());
        Ok(match self {
            CellKind::Int => Value::Int(num()?.parse().map_err(|e| format!("{e}: {}", s()))?),
            CellKind::UInt => unsigned(num()?.parse().map_err(|e| format!("{e}: {}", s()))?),
            // BIT(n) arrives as its raw big-endian bytes.
            CellKind::Bit => unsigned(b.iter().fold(0u64, |acc, x| (acc << 8) | u64::from(*x))),
            CellKind::Decimal => Value::Decimal(Value::canonical_decimal(num()?)),
            CellKind::Float => match num()?.parse::<f64>() {
                Ok(f) => Value::Float(f),
                Err(_) => Value::Text(s()),
            },
            CellKind::Text | CellKind::Geometry => Value::Text(s()),
            CellKind::Bytes => Value::Bytes(b.to_vec()),
            CellKind::Json => Value::Document(s()),
            CellKind::Date if zero_date(b) => Value::Text(s()),
            CellKind::Date => Value::Date(s()),
            CellKind::DateTime if zero_date(b) => Value::Text(trim_fraction(&s())),
            CellKind::DateTime => Value::DateTime(trim_fraction(&s())),
            CellKind::Time => Value::Time(trim_fraction(&s())),
            CellKind::Other => match std::str::from_utf8(b) {
                Ok(t) => Value::Text(t.to_owned()),
                Err(_) => Value::Bytes(b.to_vec()),
            },
        })
    }
}

fn unsigned(u: u64) -> Value {
    i64::try_from(u).map_or(Value::UInt(u), Value::Int)
}

/// `0000-00-00`, `2024-00-10`, ... — not a real date.
fn zero_date(b: &[u8]) -> bool {
    b.len() >= 10 && (&b[5..7] == b"00" || &b[8..10] == b"00")
}

/// `10:00:00.500000` → `10:00:00.5`; `10:00:00.000000` → `10:00:00`.
fn trim_fraction(s: &str) -> String {
    match s.split_once('.') {
        Some((head, frac)) => {
            let frac = frac.trim_end_matches('0');
            if frac.is_empty() { head.to_owned() } else { format!("{head}.{frac}") }
        }
        None => s.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert_eq!(parse_version("8.0.36"), (false, (8, 0, 36), "MySQL 8.0.36".into()));
        assert_eq!(parse_version("9.5.0"), (false, (9, 5, 0), "MySQL 9.5.0".into()));
        assert_eq!(
            parse_version("10.11.6-MariaDB-1:10.11.6+maria~ubu2204"),
            (true, (10, 11, 6), "MariaDB 10.11.6".into())
        );
        assert_eq!(parse_version("5.5.5-10.6.12-MariaDB"), (true, (10, 6, 12), "MariaDB 10.6.12".into()));
    }

    #[test]
    fn types() {
        assert_eq!(normalize_type("int(11)"), "int");
        assert_eq!(normalize_type("int(10) unsigned"), "int unsigned");
        assert_eq!(normalize_type("tinyint(1)"), "tinyint(1)");
        assert_eq!(normalize_type("tinyint(4)"), "tinyint");
        assert_eq!(normalize_type("int(5) unsigned zerofill"), "int(5) unsigned zerofill");
        assert_eq!(normalize_type("year(4)"), "year");
        assert_eq!(normalize_type("VARCHAR(50)"), "varchar(50)");
        assert_eq!(normalize_type("decimal(10,2)"), "decimal(10,2)");
        assert_eq!(normalize_type("enum('Active','b')"), "enum('Active','b')");
    }

    #[test]
    fn defaults_mysql8() {
        let d = |raw: Option<&str>, extra: &str, ty: &str| normalize_default(raw, extra, ty, false);
        assert_eq!(d(None, "", "int"), None);
        assert_eq!(d(Some("0"), "", "tinyint(1)"), Some("0".into()));
        assert_eq!(d(Some("1.50"), "", "decimal(10,2)"), Some("1.50".into()));
        assert_eq!(d(Some("-5"), "", "bigint"), Some("-5".into()));
        assert_eq!(d(Some("5"), "", "varchar(5)"), Some("'5'".into()));
        assert_eq!(d(Some(r"it's \ x"), "", "varchar(80)"), Some(r"'it''s \\ x'".into()));
        assert_eq!(d(Some("active"), "", "enum('active','b')"), Some("'active'".into()));
        assert_eq!(d(Some("b'101'"), "", "bit(3)"), Some("b'101'".into()));
        assert_eq!(d(Some("CURRENT_TIMESTAMP"), "default_generated", "timestamp"), Some("CURRENT_TIMESTAMP".into()));
        assert_eq!(
            d(Some("CURRENT_TIMESTAMP(3)"), "default_generated on update current_timestamp(3)", "timestamp(3)"),
            Some("CURRENT_TIMESTAMP(3) ON UPDATE CURRENT_TIMESTAMP(3)".into())
        );
        assert_eq!(d(None, "on update current_timestamp", "timestamp"), Some("ON UPDATE CURRENT_TIMESTAMP".into()));
        assert_eq!(d(Some("curdate()"), "default_generated", "date"), Some("(curdate())".into()));
        assert_eq!(d(Some("(1 + 2)"), "default_generated", "int"), Some("(1 + 2)".into()));
        assert_eq!(d(Some("CURRENT_TIMESTAMP"), "", "datetime"), Some("CURRENT_TIMESTAMP".into()));
    }

    #[test]
    fn defaults_mariadb() {
        let d = |raw: Option<&str>, extra: &str, ty: &str| normalize_default(raw, extra, ty, true);
        assert_eq!(d(Some("NULL"), "", "int"), None);
        assert_eq!(d(Some("'abc'"), "", "varchar(5)"), Some("'abc'".into()));
        assert_eq!(d(Some("'it''s \\\\ x'"), "", "varchar(5)"), Some(r"'it''s \\ x'".into()));
        assert_eq!(d(Some("0"), "", "tinyint(1)"), Some("0".into()));
        assert_eq!(d(Some("1.50"), "", "decimal(10,2)"), Some("1.50".into()));
        assert_eq!(
            d(Some("current_timestamp(3)"), "on update current_timestamp(3)", "timestamp(3)"),
            Some("CURRENT_TIMESTAMP(3) ON UPDATE CURRENT_TIMESTAMP(3)".into())
        );
        assert_eq!(d(Some("current_timestamp()"), "", "timestamp"), Some("CURRENT_TIMESTAMP".into()));
        assert_eq!(d(Some("curdate()"), "", "date"), Some("(curdate())".into()));
    }

    #[test]
    fn expression_helpers() {
        assert!(is_wrapped("(a + b)"));
        assert!(is_wrapped("((1 + 2))"));
        assert!(!is_wrapped("(a) + (b)"));
        assert!(is_wrapped("(concat(`a`,')'))"));
        assert!(!is_wrapped("curdate()"));
        assert_eq!(unescape(r"_utf8mb4\'it\\\'s\'"), r"_utf8mb4'it\'s'");
        assert!(is_number("-1.5e3") && is_number("0") && is_number(".5"));
        assert!(!is_number("1a") && !is_number("") && !is_number("-") && !is_number("1e"));
        assert_eq!(fk_action("NO ACTION"), None);
        assert_eq!(fk_action("RESTRICT"), None);
        assert_eq!(fk_action("SET NULL"), Some("SET NULL".into()));
    }

    #[test]
    fn create_noise() {
        assert_eq!(
            strip_create_noise(
                "CREATE ALGORITHM=UNDEFINED DEFINER=`root`@`localhost` SQL SECURITY DEFINER VIEW `v` AS select 1"
            ),
            "CREATE VIEW `v` AS select 1"
        );
        assert_eq!(
            strip_create_noise("CREATE ALGORITHM=MERGE DEFINER=`a``b`@`%` SQL SECURITY INVOKER VIEW `v` AS select 1"),
            "CREATE ALGORITHM=MERGE SQL SECURITY INVOKER VIEW `v` AS select 1"
        );
        assert_eq!(
            strip_create_noise("CREATE DEFINER=`root`@`localhost` PROCEDURE `p`()\nBEGIN SELECT 1; END"),
            "CREATE PROCEDURE `p`()\nBEGIN SELECT 1; END"
        );
        assert_eq!(strip_create_noise("CREATE DEFINER=`role` TRIGGER `t` x"), "CREATE TRIGGER `t` x");
        assert_eq!(strip_create_noise("CREATE DEFINER=root@localhost FUNCTION f() x"), "CREATE FUNCTION f() x");
    }

    #[test]
    fn cell_conversion() {
        let c = |k: CellKind, s: &[u8]| k.convert(s).unwrap();
        assert_eq!(c(CellKind::Int, b"-5"), Value::Int(-5));
        assert_eq!(c(CellKind::UInt, b"18446744073709551615"), Value::UInt(u64::MAX));
        assert_eq!(c(CellKind::UInt, b"7"), Value::Int(7));
        assert_eq!(c(CellKind::Bit, &[0x01, 0x01]), Value::Int(257));
        assert_eq!(c(CellKind::Decimal, b"12.50"), Value::Decimal("12.5".into()));
        assert_eq!(c(CellKind::Float, b"1.1"), Value::Float(1.1));
        assert_eq!(c(CellKind::Date, b"0000-00-00"), Value::Text("0000-00-00".into()));
        assert_eq!(c(CellKind::DateTime, b"0000-00-00 00:00:00.000"), Value::Text("0000-00-00 00:00:00".into()));
        assert_eq!(c(CellKind::DateTime, b"2024-01-02 03:04:05.500000"), Value::DateTime("2024-01-02 03:04:05.5".into()));
        assert_eq!(c(CellKind::Time, b"-838:59:59.000000"), Value::Time("-838:59:59".into()));
        assert_eq!(c(CellKind::Bytes, &[0, 255]), Value::Bytes(vec![0, 255]));
        assert_eq!(CellKind::of("int", "int unsigned"), CellKind::UInt);
        assert_eq!(CellKind::of("point", "point"), CellKind::Geometry);
    }
}
