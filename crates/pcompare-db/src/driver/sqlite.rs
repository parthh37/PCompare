//! SQLite driver. A database is one file and every object has schema `""`.
//!
//! Introspection reads `sqlite_master` and the table-valued pragma functions.
//! SQLite keeps no catalog for CHECK constraints, generated-column
//! expressions, column collations, AUTOINCREMENT or index expressions, so
//! those are parsed from the stored `CREATE` text with a small quote-aware
//! tokenizer ([`tokenize`]).
//!
//! SQLite doesn't record constraint names for foreign keys or UNIQUE
//! constraints, so both are synthesised deterministically (from the
//! definition, never from declaration order) so two files that declare the
//! same constraints get the same names.

use super::{db_err, statement_err};
use crate::connection::ConnectionConfig;
use crate::dialect::{Dialect, SqliteDialect};
use crate::model::*;
use crate::value::Value;
use pcompare_core::{CoreError, Result};
use sqlx::sqlite::{SqliteConnectOptions, SqliteConnection, SqliteRow};
use sqlx::{AssertSqlSafe, Connection, Row, TypeInfo, ValueRef};
use std::collections::{HashMap, HashSet};
use std::path::Path;

pub struct Conn {
    conn: SqliteConnection,
    database: String,
}

fn err(e: sqlx::Error) -> CoreError {
    db_err("SQLite", e)
}

impl Conn {
    pub async fn connect(cfg: &ConnectionConfig) -> Result<Conn> {
        let file = cfg.file.trim();
        if file.is_empty() {
            return Err(CoreError::Other("no SQLite database file selected".into()));
        }
        let path = Path::new(file);
        // Never create a file: comparing against a new empty database would
        // silently report every object as missing.
        if !path.is_file() {
            return Err(CoreError::Other(format!("SQLite database `{file}` does not exist")));
        }
        let opts = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(false)
            .foreign_keys(true);
        let conn = SqliteConnection::connect_with(&opts).await.map_err(|e| db_err(file, e))?;
        let database = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        Ok(Conn { conn, database })
    }

    pub async fn server_version(&mut self) -> Result<String> {
        let v: String = sqlx::query_scalar("SELECT sqlite_version()")
            .fetch_one(&mut self.conn)
            .await
            .map_err(err)?;
        Ok(format!("SQLite {v}"))
    }

    pub async fn list_databases(&mut self) -> Result<Vec<String>> {
        Ok(Vec::new())
    }

    pub async fn load_schema(&mut self) -> Result<Schema> {
        let server_version = self.server_version().await?;
        let master: Vec<(String, String, String, Option<String>)> = sqlx::query_as(
            "SELECT type, name, tbl_name, sql FROM sqlite_master \
             WHERE name NOT LIKE 'sqlite\\_%' ESCAPE '\\' ORDER BY name",
        )
        .fetch_all(&mut self.conn)
        .await
        .map_err(err)?;
        // Virtual tables (FTS, R*Tree) and their shadow tables can't be
        // recreated from columns; leave them out like internal tables.
        let skipped: HashSet<String> =
            sqlx::query_scalar("SELECT name FROM pragma_table_list WHERE schema = 'main' AND type IN ('virtual', 'shadow')")
                .fetch_all(&mut self.conn)
                .await
                .map_err(err)?
                .into_iter()
                .collect();
        let index_sql: HashMap<&str, &str> = master
            .iter()
            .filter(|m| m.0 == "index")
            .filter_map(|m| Some((m.1.as_str(), m.3.as_deref()?)))
            .collect();

        let mut schema = Schema {
            engine: Some(Engine::Sqlite),
            database: self.database.clone(),
            server_version,
            ..Default::default()
        };
        for (kind, name, tbl_name, sql) in &master {
            let sql = sql.clone().unwrap_or_default();
            match kind.as_str() {
                "table" if !skipped.contains(name) => {
                    let table = self.load_table(name, &sql, &index_sql).await?;
                    schema.tables.push(table);
                }
                "view" | "trigger" => schema.objects.push(CodeObject {
                    kind: if kind == "view" { ObjectKind::View } else { ObjectKind::Trigger },
                    name: ObjectName::new("", name.clone()),
                    definition: sql.trim().trim_end_matches(';').trim_end().to_owned(),
                    table: (kind == "trigger").then(|| ObjectName::new("", tbl_name.clone())),
                }),
                _ => {}
            }
        }
        schema.tables.sort_by(|a, b| a.name.cmp(&b.name));
        schema.objects.sort_by(|a, b| a.name.cmp(&b.name).then(a.kind.cmp(&b.kind)));
        Ok(schema)
    }

    async fn load_table(&mut self, name: &str, sql: &str, index_sql: &HashMap<&str, &str>) -> Result<Table> {
        let parsed = parse_create_table(name, sql);
        let cols: Vec<(String, String, i64, Option<String>, i64, i64)> = sqlx::query_as(
            "SELECT name, type, \"notnull\", dflt_value, pk, hidden FROM pragma_table_xinfo(?1) ORDER BY cid",
        )
        .bind(name)
        .fetch_all(&mut self.conn)
        .await
        .map_err(err)?;

        let mut pk: Vec<(i64, String)> = cols.iter().filter(|c| c.4 > 0).map(|c| (c.4, c.0.clone())).collect();
        pk.sort();
        let primary_key = (!pk.is_empty()).then(|| KeyConstraint {
            name: None,
            columns: pk.iter().map(|p| p.1.clone()).collect(),
            clustered: None,
        });
        // `INTEGER PRIMARY KEY` aliases the rowid: never NULL even though
        // the pragma reports it nullable.
        let rowid_alias = match pk.as_slice() {
            [(_, col)] => cols.iter().find(|c| &c.0 == col).filter(|c| c.1.eq_ignore_ascii_case("integer")).map(|c| c.0.clone()),
            _ => None,
        };

        let columns = cols
            .iter()
            .map(|(cname, ty, notnull, dflt, _, hidden)| {
                let extras = parsed.columns.get(&cname.to_lowercase()).cloned().unwrap_or_default();
                let is_alias = rowid_alias.as_deref() == Some(cname.as_str());
                let generated = matches!(hidden, 2 | 3);
                Column {
                    name: cname.clone(),
                    data_type: ty.trim().to_lowercase(),
                    nullable: *notnull == 0 && !is_alias,
                    default: if generated { None } else { dflt.clone() },
                    identity: (is_alias && extras.autoincrement).then(Identity::default),
                    computed: if generated { extras.computed.clone() } else { None },
                    collation: extras.collation.clone(),
                }
            })
            .collect();

        let mut indexes = Vec::new();
        let list: Vec<(String, i64, String)> =
            sqlx::query_as("SELECT name, \"unique\", origin FROM pragma_index_list(?1)")
                .bind(name)
                .fetch_all(&mut self.conn)
                .await
                .map_err(err)?;
        for (iname, unique, origin) in list {
            if origin == "pk" {
                continue;
            }
            let info: Vec<(i64, Option<String>, i64)> = sqlx::query_as(
                "SELECT cid, name, \"desc\" FROM pragma_index_xinfo(?1) WHERE key = 1 ORDER BY seqno",
            )
            .bind(&iname)
            .fetch_all(&mut self.conn)
            .await
            .map_err(err)?;
            let (terms, filter) = index_sql.get(iname.as_str()).map(|s| parse_create_index(s)).unwrap_or_default();
            let columns: Vec<IndexColumn> = info
                .iter()
                .enumerate()
                .map(|(i, (cid, cname, desc))| IndexColumn {
                    name: match cid {
                        -2 => terms.get(i).cloned().unwrap_or_else(|| "<expression>".into()),
                        -1 => "rowid".into(),
                        _ => cname.clone().unwrap_or_default(),
                    },
                    descending: *desc != 0,
                })
                .collect();
            let constraint = origin == "u";
            let index_name = if constraint {
                // Autoindex names (`sqlite_autoindex_t_1`) follow declaration
                // order; name the constraint by its columns instead.
                let cols: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
                format!("{name}_{}_key", cols.join("_"))
            } else {
                iname
            };
            indexes.push(Index {
                name: index_name,
                columns,
                unique: unique != 0,
                constraint,
                method: None,
                filter: if constraint { None } else { filter },
                include: Vec::new(),
            });
        }
        indexes.sort_by(|a, b| a.name.cmp(&b.name));

        let foreign_keys = self.load_foreign_keys(name).await?;
        Ok(Table {
            name: ObjectName::new("", name),
            columns,
            primary_key,
            indexes,
            foreign_keys,
            checks: parsed.checks,
        })
    }

    async fn load_foreign_keys(&mut self, table: &str) -> Result<Vec<ForeignKey>> {
        type FkRow = (i64, String, String, Option<String>, String, String);
        let rows: Vec<FkRow> = sqlx::query_as(
            "SELECT id, \"table\", \"from\", \"to\", on_update, on_delete \
             FROM pragma_foreign_key_list(?1) ORDER BY id, seq",
        )
        .bind(table)
        .fetch_all(&mut self.conn)
        .await
        .map_err(err)?;
        let mut fks: Vec<(i64, ForeignKey)> = Vec::new();
        for (id, ref_table, from, to, on_update, on_delete) in rows {
            if fks.last().is_none_or(|f| f.0 != id) {
                fks.push((
                    id,
                    ForeignKey {
                        ref_table: ObjectName::new("", ref_table),
                        on_delete: action(&on_delete),
                        on_update: action(&on_update),
                        ..Default::default()
                    },
                ));
            }
            let fk = &mut fks.last_mut().expect("pushed above").1;
            fk.columns.push(from);
            // NULL `to` = the referenced table's primary key, resolved below.
            fk.ref_columns.extend(to);
        }
        let mut fks: Vec<ForeignKey> = fks.into_iter().map(|f| f.1).collect();
        for fk in &mut fks {
            if fk.ref_columns.is_empty() {
                fk.ref_columns = sqlx::query_scalar("SELECT name FROM pragma_table_info(?1) WHERE pk > 0 ORDER BY pk")
                    .bind(&fk.ref_table.name)
                    .fetch_all(&mut self.conn)
                    .await
                    .map_err(err)?;
            }
        }
        let key = |f: &ForeignKey| (f.ref_table.name.to_lowercase(), f.columns.clone(), f.ref_columns.clone());
        fks.sort_by_key(key);
        for (n, fk) in fks.iter_mut().enumerate() {
            fk.name = format!("fk_{table}_{}", n + 1);
        }
        Ok(fks)
    }

    pub async fn fetch_rows(&mut self, table: &ObjectName, columns: &[String], order_by: &[String]) -> Result<Vec<Vec<Value>>> {
        let declared: HashMap<String, String> =
            sqlx::query_as::<_, (String, String)>("SELECT name, type FROM pragma_table_xinfo(?1)")
                .bind(&table.name)
                .fetch_all(&mut self.conn)
                .await
                .map_err(err)?
                .into_iter()
                .map(|(n, t)| (n.to_lowercase(), t.to_lowercase()))
                .collect();
        let d = SqliteDialect;
        let mut sql = format!("SELECT {} FROM {}", d.ident_list(columns), d.qualified(table));
        if !order_by.is_empty() {
            sql.push_str(&format!(" ORDER BY {}", d.ident_list(order_by)));
        }
        let rows = sqlx::query(AssertSqlSafe(sql)).fetch_all(&mut self.conn).await.map_err(err)?;
        let types: Vec<&str> = columns
            .iter()
            .map(|c| declared.get(&c.to_lowercase()).map_or("", String::as_str))
            .collect();
        rows.iter()
            .map(|row| types.iter().enumerate().map(|(i, ty)| cell(row, i, ty)).collect())
            .collect()
    }

    pub async fn execute(&mut self, statements: &[String], transactional: bool) -> Result<()> {
        if !transactional {
            for (i, s) in statements.iter().enumerate() {
                sqlx::raw_sql(AssertSqlSafe(s.as_str()))
                    .execute(&mut self.conn)
                    .await
                    .map_err(|e| statement_err(i, s, e))?;
            }
            return Ok(());
        }
        let mut tx = self.conn.begin().await.map_err(err)?;
        for (i, s) in statements.iter().enumerate() {
            if let Err(e) = sqlx::raw_sql(AssertSqlSafe(s.as_str())).execute(&mut *tx).await {
                // Report the statement even if the rollback itself fails.
                let _ = tx.rollback().await;
                return Err(statement_err(i, s, e));
            }
        }
        tx.commit().await.map_err(err)
    }
}

fn action(a: &str) -> Option<String> {
    let a = a.trim().to_uppercase();
    (!a.is_empty() && a != "NO ACTION").then_some(a)
}

/// One cell, typed by its storage class and refined by the declared type
/// (SQLite stores dates, booleans and UUIDs as plain text/integers).
fn cell(row: &SqliteRow, i: usize, declared: &str) -> Result<Value> {
    let raw = row.try_get_raw(i).map_err(err)?;
    if raw.is_null() {
        return Ok(Value::Null);
    }
    let storage = raw.type_info().name().to_owned();
    Ok(match storage.as_str() {
        "INTEGER" => {
            let v: i64 = row.try_get_unchecked(i).map_err(err)?;
            if declared.contains("bool") && (v == 0 || v == 1) { Value::Bool(v == 1) } else { Value::Int(v) }
        }
        "REAL" => Value::Float(row.try_get_unchecked(i).map_err(err)?),
        "BLOB" => Value::Bytes(row.try_get_unchecked(i).map_err(err)?),
        _ => text_value(declared, row.try_get_unchecked(i).map_err(err)?),
    })
}

fn text_value(declared: &str, s: String) -> Value {
    if declared.contains("json") {
        return Value::Document(s);
    }
    if declared.contains("uuid") || declared.contains("guid") {
        return match uuid::Uuid::parse_str(&s) {
            Ok(u) => Value::Uuid(u.hyphenated().to_string()),
            Err(_) => Value::Text(s),
        };
    }
    if declared.contains("timestamp") || declared.contains("datetime") {
        return datetime_value(&s).unwrap_or(Value::Text(s));
    }
    if declared == "date" && is_date(&s) {
        return Value::Date(s);
    }
    if declared.starts_with("time") && is_time(&s) {
        return Value::Time(trim_fraction(&s));
    }
    Value::Text(s)
}

fn digits_at(s: &str, pattern: &str) -> bool {
    s.len() >= pattern.len()
        && s.bytes().zip(pattern.bytes()).all(|(c, p)| if p == b'9' { c.is_ascii_digit() } else { c == p })
}

fn is_date(s: &str) -> bool {
    s.len() == 10 && digits_at(s, "9999-99-99")
}

fn is_time(s: &str) -> bool {
    digits_at(s, "99:99:99") && s[8..].strip_prefix('.').is_none_or(|f| !f.is_empty() && f.bytes().all(|c| c.is_ascii_digit()))
}

/// `2024-01-02T10:00:00.500Z` → `DateTimeTz("2024-01-02 10:00:00.5+00:00")`.
fn datetime_value(s: &str) -> Option<Value> {
    if !digits_at(s, "9999-99-99") || s.len() < 16 || !matches!(s.as_bytes()[10], b' ' | b'T') {
        return None;
    }
    let (date, rest) = (&s[..10], &s[11..]);
    let zone_at = rest.find(['Z', 'z', '+']).or_else(|| rest.rfind('-'));
    let (time, zone) = match zone_at {
        Some(i) => (&rest[..i], Some(&rest[i..])),
        None => (rest, None),
    };
    let time = if time.len() == 5 { format!("{time}:00") } else { time.to_owned() };
    if !is_time(&time) {
        return None;
    }
    let base = format!("{date} {}", trim_fraction(&time));
    match zone {
        None => Some(Value::DateTime(base)),
        Some("Z" | "z") => Some(Value::DateTimeTz(format!("{base}+00:00"))),
        Some(z) if digits_at(&z[1..], "99:99") && z.len() == 6 => Some(Value::DateTimeTz(format!("{base}{z}"))),
        Some(z) if digits_at(&z[1..], "9999") && z.len() == 5 => {
            Some(Value::DateTimeTz(format!("{base}{}:{}", &z[..3], &z[3..])))
        }
        Some(_) => None,
    }
}

/// `10:00:00.500` → `10:00:00.5`, `10:00:00.000` → `10:00:00`.
fn trim_fraction(t: &str) -> String {
    match t.split_once('.') {
        Some((hms, frac)) => {
            let frac = frac.trim_end_matches('0');
            if frac.is_empty() { hms.to_owned() } else { format!("{hms}.{frac}") }
        }
        None => t.to_owned(),
    }
}

// ----- CREATE statement parsing -----

/// A top-level token of SQL text. Parenthesised groups are one token, so
/// commas and keywords inside them are never seen at the top level.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Tok {
    /// Bare identifier, keyword or number.
    Word,
    /// `"x"`, `` `x` `` or `[x]`.
    Quoted,
    /// `'x'`.
    Str,
    /// `( ... )`.
    Group,
    Punct,
}

#[derive(Debug, Clone, Copy)]
struct Token<'a> {
    kind: Tok,
    text: &'a str,
    start: usize,
    end: usize,
}

impl Token<'_> {
    fn is_kw(&self, kw: &str) -> bool {
        self.kind == Tok::Word && self.text.eq_ignore_ascii_case(kw)
    }

    /// Group contents without the parentheses.
    fn inner(&self) -> &str {
        self.text[1..self.text.len() - 1].trim()
    }
}

/// Index just past a quoted run starting at `i`; doubled closers are escapes
/// (except for `[...]`).
fn skip_quoted(b: &[u8], i: usize) -> usize {
    let close = if b[i] == b'[' { b']' } else { b[i] };
    let mut j = i + 1;
    while j < b.len() {
        if b[j] == close {
            if close != b']' && b.get(j + 1) == Some(&close) {
                j += 2;
                continue;
            }
            return j + 1;
        }
        j += 1;
    }
    b.len()
}

/// Index just past a comment starting at `i`, or `None` if none starts there.
fn skip_comment(b: &[u8], i: usize) -> Option<usize> {
    match (b[i], b.get(i + 1)) {
        (b'-', Some(b'-')) => Some(b[i..].iter().position(|&c| c == b'\n').map_or(b.len(), |p| i + p + 1)),
        (b'/', Some(b'*')) => Some(b[i + 2..].windows(2).position(|w| w == b"*/").map_or(b.len(), |p| i + 2 + p + 2)),
        _ => None,
    }
}

fn tokenize(s: &str) -> Vec<Token<'_>> {
    let b = s.as_bytes();
    let is_word = |c: u8| c.is_ascii_alphanumeric() || c == b'_' || c == b'$' || c >= 0x80;
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if let Some(j) = skip_comment(b, i) {
            i = j;
            continue;
        }
        let (kind, end) = match c {
            b'\'' => (Tok::Str, skip_quoted(b, i)),
            b'"' | b'`' | b'[' => (Tok::Quoted, skip_quoted(b, i)),
            b'(' => {
                let mut depth = 0;
                let mut j = i;
                while j < b.len() {
                    match b[j] {
                        b'\'' | b'"' | b'`' | b'[' => {
                            j = skip_quoted(b, j);
                            continue;
                        }
                        b'(' => depth += 1,
                        b')' => {
                            depth -= 1;
                            if depth == 0 {
                                j += 1;
                                break;
                            }
                        }
                        _ => {
                            if let Some(k) = skip_comment(b, j) {
                                j = k;
                                continue;
                            }
                        }
                    }
                    j += 1;
                }
                (Tok::Group, j)
            }
            c if is_word(c) => (Tok::Word, i + b[i..].iter().position(|&c| !is_word(c)).unwrap_or(b.len() - i)),
            _ => (Tok::Punct, i + 1),
        };
        out.push(Token { kind, text: &s[i..end], start: i, end });
        i = end;
    }
    out
}

/// Splits at top-level commas.
fn split_top_level(s: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut from = 0;
    for t in tokenize(s) {
        if t.kind == Tok::Punct && t.text == "," {
            parts.push(s[from..t.start].trim());
            from = t.end;
        }
    }
    parts.push(s[from..].trim());
    parts.retain(|p| !p.is_empty());
    parts
}

fn unquote(raw: &str) -> String {
    let inner = || &raw[1..raw.len() - 1];
    match raw.as_bytes().first() {
        Some(b'"') if raw.len() >= 2 => inner().replace("\"\"", "\""),
        Some(b'`') if raw.len() >= 2 => inner().replace("``", "`"),
        Some(b'\'') if raw.len() >= 2 => inner().replace("''", "'"),
        Some(b'[') if raw.len() >= 2 => inner().to_owned(),
        _ => raw.to_owned(),
    }
}

#[derive(Debug, Clone, Default)]
struct ColumnExtras {
    computed: Option<String>,
    collation: Option<String>,
    autoincrement: bool,
}

#[derive(Debug, Default)]
struct CreateTable {
    /// Keyed by lower-cased column name.
    columns: HashMap<String, ColumnExtras>,
    checks: Vec<CheckConstraint>,
}

/// Pulls what the pragmas don't report out of `CREATE TABLE` text. Column-
/// level CHECKs are returned as table checks (equivalent in SQLite); unnamed
/// ones are named `{table}_check{n}` in declaration order.
fn parse_create_table(table: &str, sql: &str) -> CreateTable {
    let mut out = CreateTable::default();
    let Some(body) = tokenize(sql).into_iter().find(|t| t.kind == Tok::Group) else {
        return out;
    };
    let mut unnamed = 0;
    for part in split_top_level(body.inner()) {
        let toks = tokenize(part);
        let Some(first) = toks.first() else { continue };
        let is_table_constraint = ["CONSTRAINT", "PRIMARY", "UNIQUE", "CHECK", "FOREIGN"].iter().any(|k| first.is_kw(k));
        let start = if is_table_constraint { 0 } else { 1 };
        let mut extras = ColumnExtras::default();
        let mut pending: Option<String> = None;
        let mut k = start;
        while k < toks.len() {
            let t = toks[k];
            let next = toks.get(k + 1);
            let next_group = next.filter(|n| n.kind == Tok::Group);
            if t.is_kw("CONSTRAINT") {
                pending = next.map(|n| unquote(n.text));
                k += 2;
                continue;
            }
            if t.is_kw("CHECK") {
                if let Some(g) = next_group {
                    let name = pending.take().unwrap_or_else(|| {
                        unnamed += 1;
                        format!("{table}_check{unnamed}")
                    });
                    out.checks.push(CheckConstraint { name, expression: g.inner().to_owned() });
                    k += 2;
                    continue;
                }
            } else if t.is_kw("COLLATE") {
                extras.collation = next.map(|n| unquote(n.text).to_uppercase()).filter(|c| c != "BINARY");
                k += 2;
                continue;
            } else if t.is_kw("AS") {
                if let Some(g) = next_group {
                    extras.computed = Some(g.inner().to_owned());
                    k += 2;
                    continue;
                }
            } else if t.is_kw("AUTOINCREMENT") {
                extras.autoincrement = true;
            } else if t.is_kw("DEFAULT") {
                k += 2;
                continue;
            }
            // A constraint name only labels the constraint right after it.
            if t.kind == Tok::Word
                && ["PRIMARY", "UNIQUE", "NOT", "NULL", "REFERENCES", "FOREIGN", "GENERATED"].iter().any(|kw| t.is_kw(kw))
            {
                pending = None;
            }
            k += 1;
        }
        if !is_table_constraint {
            out.columns.insert(unquote(first.text).to_lowercase(), extras);
        }
    }
    out
}

/// Key terms (column names unquoted, expressions verbatim) and the WHERE
/// predicate of a `CREATE INDEX` statement.
fn parse_create_index(sql: &str) -> (Vec<String>, Option<String>) {
    let toks = tokenize(sql);
    let Some(on) = toks.iter().position(|t| t.is_kw("ON")) else {
        return Default::default();
    };
    let Some(g) = toks[on..].iter().position(|t| t.kind == Tok::Group).map(|i| on + i) else {
        return Default::default();
    };
    let terms = split_top_level(toks[g].inner())
        .into_iter()
        .map(|term| {
            let mut t = tokenize(term);
            if t.last().is_some_and(|l| l.is_kw("ASC") || l.is_kw("DESC")) {
                t.pop();
            }
            if t.len() >= 2 && t[t.len() - 2].is_kw("COLLATE") {
                t.truncate(t.len() - 2);
            }
            match t.as_slice() {
                [one] if matches!(one.kind, Tok::Word | Tok::Quoted) => unquote(one.text),
                [first, .., last] => term[first.start..last.end].to_owned(),
                [one] => one.text.to_owned(),
                [] => term.to_owned(),
            }
        })
        .collect();
    let filter = toks[g + 1..]
        .iter()
        .find(|t| t.is_kw("WHERE"))
        .map(|w| sql[w.end..].trim().trim_end_matches(';').trim_end().to_owned())
        .filter(|f| !f.is_empty());
    (terms, filter)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_create_table_extras() {
        let sql = r#"CREATE TABLE "Order Items" (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            "name" TEXT COLLATE nocase NOT NULL CHECK (length(name) > 0), -- a, b
            total REAL GENERATED ALWAYS AS (price * qty) VIRTUAL,
            price REAL DEFAULT (1.5) CONSTRAINT price_pos CHECK (price >= 0),
            qty INT,
            note TEXT DEFAULT 'as (x)',
            CONSTRAINT qty_ok CHECK (qty BETWEEN 1 AND 100),
            CHECK (price < 1e6 /* , */),
            UNIQUE (name, qty)
        )"#;
        let p = parse_create_table("Order Items", sql);
        assert!(p.columns["id"].autoincrement);
        assert_eq!(p.columns["name"].collation.as_deref(), Some("NOCASE"));
        assert_eq!(p.columns["total"].computed.as_deref(), Some("price * qty"));
        assert_eq!(p.columns["note"].computed, None);
        let checks: Vec<(&str, &str)> = p.checks.iter().map(|c| (c.name.as_str(), c.expression.as_str())).collect();
        assert_eq!(
            checks,
            [
                ("Order Items_check1", "length(name) > 0"),
                ("price_pos", "price >= 0"),
                ("qty_ok", "qty BETWEEN 1 AND 100"),
                ("Order Items_check2", "price < 1e6 /* , */"),
            ]
        );
    }

    #[test]
    fn parses_create_index() {
        let (terms, filter) =
            parse_create_index("CREATE UNIQUE INDEX ix ON \"t\" (lower(email) DESC, \"Last Name\" COLLATE nocase, a) WHERE deleted = 0");
        assert_eq!(terms, ["lower(email)", "Last Name", "a"]);
        assert_eq!(filter.as_deref(), Some("deleted = 0"));
        assert_eq!(parse_create_index("CREATE INDEX ix ON t(a)"), (vec!["a".to_owned()], None));
    }

    #[test]
    fn normalises_text_temporals() {
        assert_eq!(datetime_value("2024-01-02T10:00:00.500Z"), Some(Value::DateTimeTz("2024-01-02 10:00:00.5+00:00".into())));
        assert_eq!(datetime_value("2024-01-02 10:00"), Some(Value::DateTime("2024-01-02 10:00:00".into())));
        assert_eq!(datetime_value("2024-01-02 10:00:00-0530"), Some(Value::DateTimeTz("2024-01-02 10:00:00-05:30".into())));
        assert_eq!(datetime_value("yesterday"), None);
        assert_eq!(text_value("time", "10:00:00.000".into()), Value::Time("10:00:00".into()));
        assert_eq!(text_value("date", "2024-1-2".into()), Value::Text("2024-1-2".into()));
    }
}
