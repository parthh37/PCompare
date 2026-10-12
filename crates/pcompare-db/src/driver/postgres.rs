//! PostgreSQL driver: one `PgConnection`, introspected through `pg_catalog`.
//!
//! Representation choices the diff/script engine relies on:
//! - `serial` columns are reported like identity columns (`identity` set,
//!   `default` None) and their owned sequence is not listed separately.
//! - Functions and procedures are named with their identity signature,
//!   `name(integer, text)`, so overloads are distinct objects.
//! - Trigger names are only unique per table, so triggers are named
//!   `table.trigger` (with `CodeObject::table` set).
//!
//! The session runs with `DateStyle = ISO`, `TimeZone = UTC` so deparsed
//! defaults and fetched temporals look the same whatever the server's
//! configuration.

use super::{db_err, statement_err};
use crate::connection::{ConnectionConfig, TlsMode};
use crate::dialect::{Dialect, PostgresDialect};
use crate::model::*;
use crate::value::Value;
use pcompare_core::{CoreError, Result};
use sqlx::postgres::{PgConnectOptions, PgConnection, PgRow, PgSslMode};
use sqlx::{AssertSqlSafe, Connection, Decode, Postgres, Row, Type, ValueRef};
use std::collections::{HashMap, HashSet};

pub struct Conn {
    conn: PgConnection,
    /// `cfg.schemas`: only these schemas are introspected (empty = all).
    schemas: Vec<String>,
}

fn err(e: sqlx::Error) -> CoreError {
    db_err("PostgreSQL", e)
}

fn get<'r, T: Decode<'r, Postgres> + Type<Postgres>>(row: &'r PgRow, col: &str) -> Result<T> {
    row.try_get(col).map_err(err)
}

/// User schemas only, narrowed to `cfg.schemas` (bound as `$1`) when set.
const NS_FILTER: &str = "n.nspname NOT IN ('pg_catalog', 'information_schema') \
     AND n.nspname NOT LIKE 'pg\\_toast%' AND n.nspname NOT LIKE 'pg\\_temp\\_%' \
     AND (cardinality($1::text[]) = 0 OR n.nspname = ANY($1::text[]))";

/// Excludes objects that belong to an extension (installed by `CREATE EXTENSION`).
fn not_extension(catalog: &str, oid: &str) -> String {
    format!(
        "NOT EXISTS (SELECT 1 FROM pg_depend e WHERE e.classid = '{catalog}'::regclass \
         AND e.objid = {oid} AND e.deptype = 'e')"
    )
}

/// Ordinary and partitioned tables; partitions are compared through their parent.
const TABLE_FILTER: &str = "c.relkind IN ('r', 'p') AND NOT c.relispartition";

impl Conn {
    pub async fn connect(cfg: &ConnectionConfig) -> Result<Conn> {
        let host = if cfg.host.trim().is_empty() { "localhost" } else { cfg.host.trim() };
        let database = if cfg.database.is_empty() { "postgres" } else { cfg.database.as_str() };
        let mut opts = PgConnectOptions::new()
            .host(host)
            .port(cfg.port_or_default())
            .database(database)
            .application_name("PCompare")
            .ssl_mode(match cfg.tls {
                TlsMode::Disable => PgSslMode::Disable,
                TlsMode::Prefer => PgSslMode::Prefer,
                TlsMode::Require => PgSslMode::Require,
            });
        if !cfg.user.is_empty() {
            opts = opts.username(&cfg.user);
        }
        // An empty password leaves room for ~/.pgpass and trust auth.
        if !cfg.password.is_empty() {
            opts = opts.password(&cfg.password);
        }
        let mut conn = PgConnection::connect_with(&opts).await.map_err(err)?;
        sqlx::raw_sql("SET DateStyle = 'ISO, YMD'; SET TimeZone = 'UTC'; SET IntervalStyle = 'postgres'")
            .execute(&mut conn)
            .await
            .map_err(err)?;
        Ok(Conn { conn, schemas: cfg.schemas.clone() })
    }

    pub async fn server_version(&mut self) -> Result<String> {
        sqlx::query_scalar("SELECT 'PostgreSQL ' || current_setting('server_version')")
            .fetch_one(&mut self.conn)
            .await
            .map_err(err)
    }

    pub async fn list_databases(&mut self) -> Result<Vec<String>> {
        sqlx::query_scalar(
            "SELECT datname::text FROM pg_database WHERE NOT datistemplate AND datallowconn ORDER BY datname",
        )
        .fetch_all(&mut self.conn)
        .await
        .map_err(err)
    }

    /// Runs a catalog query whose `$1` is the schema filter.
    async fn catalog(&mut self, sql: String, schemas: &[String]) -> Result<Vec<PgRow>> {
        sqlx::query(AssertSqlSafe(sql)).bind(schemas).fetch_all(&mut self.conn).await.map_err(err)
    }

    pub async fn load_schema(&mut self) -> Result<Schema> {
        let schemas = &self.schemas.clone();
        let server_version = self.server_version().await?;
        let database: String =
            sqlx::query_scalar("SELECT current_database()::text").fetch_one(&mut self.conn).await.map_err(err)?;

        let rows = self
            .catalog(
                format!(
                    "SELECT c.oid::int8 AS oid, n.nspname::text AS schema, c.relname::text AS name \
                     FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
                     WHERE {TABLE_FILTER} AND {NS_FILTER} AND {}",
                    not_extension("pg_class", "c.oid")
                ),
                schemas,
            )
            .await?;
        let mut tables: HashMap<i64, Table> = HashMap::new();
        for r in &rows {
            let name = ObjectName::new(get::<String>(r, "schema")?, get::<String>(r, "name")?);
            tables.insert(get(r, "oid")?, Table { name, ..Default::default() });
        }

        let owned_sequences = self.load_columns(&mut tables, schemas).await?;
        self.load_constraints(&mut tables, schemas).await?;
        self.load_indexes(&mut tables, schemas).await?;

        let mut objects = self.load_code_objects(schemas).await?;
        objects.extend(self.load_sequences(schemas, &owned_sequences).await?);
        let mut tables: Vec<Table> = tables.into_values().collect();
        for t in &mut tables {
            t.indexes.sort_by(|a, b| a.name.cmp(&b.name));
            t.foreign_keys.sort_by(|a, b| a.name.cmp(&b.name));
            t.checks.sort_by(|a, b| a.name.cmp(&b.name));
        }
        tables.sort_by(|a, b| a.name.cmp(&b.name));
        objects.sort_by(|a, b| a.name.cmp(&b.name).then(a.kind.cmp(&b.kind)));
        Ok(Schema { engine: Some(Engine::Postgres), database, server_version, tables, objects })
    }

    /// Fills columns; returns the sequences owned by identity/serial columns.
    async fn load_columns(&mut self, tables: &mut HashMap<i64, Table>, schemas: &[String]) -> Result<HashSet<i64>> {
        let rows = self
            .catalog(
                format!(
                    "SELECT a.attrelid::int8 AS oid, a.attname::text AS name, \
                       lower(format_type(a.atttypid, a.atttypmod)) AS data_type, a.attnotnull AS notnull, \
                       pg_get_expr(d.adbin, d.adrelid, true) AS default_expr, a.attidentity::text AS identity, \
                       a.attgenerated::text AS generated, \
                       CASE WHEN a.attcollation <> 0 AND a.attcollation <> t.typcollation THEN co.collname::text END AS collation, \
                       sq.seqrelid::int8 AS seq_oid, sq.seqstart AS seq_start, sq.seqincrement AS seq_increment \
                     FROM pg_attribute a \
                     JOIN pg_class c ON c.oid = a.attrelid \
                     JOIN pg_namespace n ON n.oid = c.relnamespace \
                     JOIN pg_type t ON t.oid = a.atttypid \
                     LEFT JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum \
                     LEFT JOIN pg_collation co ON co.oid = a.attcollation \
                     LEFT JOIN LATERAL ( \
                       SELECT s.seqrelid, s.seqstart, s.seqincrement FROM pg_depend dep \
                       JOIN pg_sequence s ON s.seqrelid = dep.objid \
                       WHERE dep.classid = 'pg_class'::regclass AND dep.refclassid = 'pg_class'::regclass \
                         AND dep.refobjid = a.attrelid AND dep.refobjsubid = a.attnum AND dep.deptype IN ('a', 'i') \
                       LIMIT 1) sq ON true \
                     WHERE a.attnum > 0 AND NOT a.attisdropped AND {TABLE_FILTER} AND {NS_FILTER} \
                     ORDER BY a.attrelid, a.attnum"
                ),
                schemas,
            )
            .await?;
        let mut owned = HashSet::new();
        for r in &rows {
            let Some(table) = tables.get_mut(&get::<i64>(r, "oid")?) else { continue };
            let mut default: Option<String> = get(r, "default_expr")?;
            let attidentity: String = get(r, "identity")?;
            let generated = get::<String>(r, "generated")? == "s";
            let seq_oid: Option<i64> = get(r, "seq_oid")?;
            let seq = || -> Result<Identity> {
                Ok(Identity {
                    seed: get::<Option<i64>>(r, "seq_start")?.unwrap_or(1),
                    increment: get::<Option<i64>>(r, "seq_increment")?.unwrap_or(1),
                })
            };
            let is_serial = attidentity.is_empty()
                && seq_oid.is_some()
                && default.as_deref().is_some_and(|d| d.starts_with("nextval("));
            let identity = if !attidentity.is_empty() || is_serial {
                owned.extend(seq_oid);
                if is_serial {
                    default = None;
                }
                Some(seq()?)
            } else {
                None
            };
            table.columns.push(Column {
                name: get(r, "name")?,
                data_type: get(r, "data_type")?,
                nullable: !get::<bool>(r, "notnull")?,
                computed: if generated { default.take() } else { None },
                default,
                identity,
                collation: get(r, "collation")?,
            });
        }
        Ok(owned)
    }

    /// Primary keys, foreign keys and checks (UNIQUE constraints come with indexes).
    async fn load_constraints(&mut self, tables: &mut HashMap<i64, Table>, schemas: &[String]) -> Result<()> {
        let rows = self
            .catalog(
                format!(
                    "SELECT con.conrelid::int8 AS oid, con.conname::text AS name, con.contype::text AS kind, \
                       pg_get_constraintdef(con.oid, true) AS def, \
                       ARRAY(SELECT a.attname::text FROM unnest(con.conkey) WITH ORDINALITY k(attnum, ord) \
                             JOIN pg_attribute a ON a.attrelid = con.conrelid AND a.attnum = k.attnum ORDER BY k.ord) AS cols, \
                       rn.nspname::text AS ref_schema, rc.relname::text AS ref_table, \
                       ARRAY(SELECT a.attname::text FROM unnest(con.confkey) WITH ORDINALITY k(attnum, ord) \
                             JOIN pg_attribute a ON a.attrelid = con.confrelid AND a.attnum = k.attnum ORDER BY k.ord) AS ref_cols, \
                       con.confdeltype::text AS on_delete, con.confupdtype::text AS on_update \
                     FROM pg_constraint con \
                     JOIN pg_class c ON c.oid = con.conrelid \
                     JOIN pg_namespace n ON n.oid = c.relnamespace \
                     LEFT JOIN pg_class rc ON rc.oid = con.confrelid \
                     LEFT JOIN pg_namespace rn ON rn.oid = rc.relnamespace \
                     WHERE con.contype IN ('p', 'c', 'f') AND {TABLE_FILTER} AND {NS_FILTER}"
                ),
                schemas,
            )
            .await?;
        for r in &rows {
            let Some(table) = tables.get_mut(&get::<i64>(r, "oid")?) else { continue };
            let name: String = get(r, "name")?;
            let columns: Vec<String> = get(r, "cols")?;
            match get::<String>(r, "kind")?.as_str() {
                "p" => table.primary_key = Some(KeyConstraint { name: Some(name), columns, clustered: None }),
                "c" => table.checks.push(CheckConstraint { name, expression: check_expression(&get::<String>(r, "def")?) }),
                _ => table.foreign_keys.push(ForeignKey {
                    name,
                    columns,
                    ref_table: ObjectName::new(
                        get::<Option<String>>(r, "ref_schema")?.unwrap_or_default(),
                        get::<Option<String>>(r, "ref_table")?.unwrap_or_default(),
                    ),
                    ref_columns: get(r, "ref_cols")?,
                    on_delete: fk_action(&get::<String>(r, "on_delete")?),
                    on_update: fk_action(&get::<String>(r, "on_update")?),
                }),
            }
        }
        Ok(())
    }

    /// Secondary indexes plus UNIQUE constraints (as `constraint: true`);
    /// indexes backing primary keys and exclusion constraints are skipped.
    async fn load_indexes(&mut self, tables: &mut HashMap<i64, Table>, schemas: &[String]) -> Result<()> {
        let rows = self
            .catalog(
                format!(
                    "SELECT i.indrelid::int8 AS oid, ic.relname::text AS name, i.indisunique AS is_unique, \
                       am.amname::text AS method, pg_get_expr(i.indpred, i.indrelid, true) AS predicate, \
                       i.indnkeyatts::int4 AS nkey, \
                       ARRAY(SELECT CASE WHEN k.attnum = 0 THEN pg_get_indexdef(i.indexrelid, k.ord::int, true) \
                                    ELSE a.attname::text END \
                             FROM unnest(i.indkey::int2[]) WITH ORDINALITY k(attnum, ord) \
                             LEFT JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = k.attnum \
                             ORDER BY k.ord) AS cols, \
                       ARRAY(SELECT unnest(i.indoption::int2[])) AS options, con.contype::text AS contype, con.conname::text AS conname \
                     FROM pg_index i \
                     JOIN pg_class ic ON ic.oid = i.indexrelid \
                     JOIN pg_class c ON c.oid = i.indrelid \
                     JOIN pg_namespace n ON n.oid = c.relnamespace \
                     JOIN pg_am am ON am.oid = ic.relam \
                     LEFT JOIN pg_constraint con ON con.conindid = i.indexrelid AND con.conrelid = i.indrelid \
                       AND con.contype IN ('p', 'u', 'x') \
                     WHERE {TABLE_FILTER} AND {NS_FILTER}"
                ),
                schemas,
            )
            .await?;
        for r in &rows {
            let Some(table) = tables.get_mut(&get::<i64>(r, "oid")?) else { continue };
            let contype: Option<String> = get(r, "contype")?;
            if matches!(contype.as_deref(), Some("p" | "x")) {
                continue;
            }
            let constraint = contype.is_some();
            let cols: Vec<String> = get(r, "cols")?;
            let options: Vec<i16> = get(r, "options")?;
            let nkey = (get::<i32>(r, "nkey")?.max(0) as usize).min(cols.len());
            let method: String = get(r, "method")?;
            table.indexes.push(Index {
                name: if constraint { get::<Option<String>>(r, "conname")?.unwrap_or_default() } else { get(r, "name")? },
                columns: cols[..nkey]
                    .iter()
                    .enumerate()
                    // indoption bit 0 = DESC.
                    .map(|(k, c)| IndexColumn { name: c.clone(), descending: options.get(k).is_some_and(|o| o & 1 != 0) })
                    .collect(),
                unique: get(r, "is_unique")?,
                constraint,
                method: (method != "btree").then_some(method),
                filter: get(r, "predicate")?,
                include: cols[nkey..].to_vec(),
            });
        }
        Ok(())
    }

    /// Views, materialized views, functions, procedures and triggers.
    async fn load_code_objects(&mut self, schemas: &[String]) -> Result<Vec<CodeObject>> {
        let mut out = Vec::new();
        let views = self
            .catalog(
                format!(
                    "SELECT n.nspname::text AS schema, c.relname::text AS name, c.relkind::text AS kind, \
                       quote_ident(n.nspname) || '.' || quote_ident(c.relname) AS qname, \
                       pg_get_viewdef(c.oid, true) AS body \
                     FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
                     WHERE c.relkind IN ('v', 'm') AND {NS_FILTER} AND {}",
                    not_extension("pg_class", "c.oid")
                ),
                schemas,
            )
            .await?;
        for r in &views {
            let create = if get::<String>(r, "kind")? == "m" { "CREATE MATERIALIZED VIEW" } else { "CREATE VIEW" };
            let body: String = get(r, "body")?;
            out.push(CodeObject {
                kind: ObjectKind::View,
                name: ObjectName::new(get::<String>(r, "schema")?, get::<String>(r, "name")?),
                definition: format!("{create} {} AS\n{}", get::<String>(r, "qname")?, trim_statement(&body)),
                table: None,
            });
        }

        let routines = self
            .catalog(
                format!(
                    "SELECT n.nspname::text AS schema, p.proname::text AS name, p.prokind::text AS kind, \
                       oidvectortypes(p.proargtypes) AS args, pg_get_functiondef(p.oid) AS def \
                     FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace \
                     WHERE p.prokind IN ('f', 'p') AND {NS_FILTER} AND {}",
                    not_extension("pg_proc", "p.oid")
                ),
                schemas,
            )
            .await?;
        for r in &routines {
            let def: String = get(r, "def")?;
            out.push(CodeObject {
                kind: if get::<String>(r, "kind")? == "p" { ObjectKind::Procedure } else { ObjectKind::Function },
                name: ObjectName::new(
                    get::<String>(r, "schema")?,
                    format!("{}({})", get::<String>(r, "name")?, get::<String>(r, "args")?),
                ),
                definition: trim_statement(&def).to_owned(),
                table: None,
            });
        }

        let triggers = self
            .catalog(
                format!(
                    "SELECT n.nspname::text AS schema, c.relname::text AS table_name, t.tgname::text AS name, \
                       pg_get_triggerdef(t.oid, true) AS def \
                     FROM pg_trigger t JOIN pg_class c ON c.oid = t.tgrelid \
                     JOIN pg_namespace n ON n.oid = c.relnamespace \
                     WHERE NOT t.tgisinternal AND NOT c.relispartition AND {NS_FILTER} AND {}",
                    not_extension("pg_class", "c.oid")
                ),
                schemas,
            )
            .await?;
        for r in &triggers {
            let schema: String = get(r, "schema")?;
            let table: String = get(r, "table_name")?;
            out.push(CodeObject {
                kind: ObjectKind::Trigger,
                name: ObjectName::new(schema.clone(), format!("{table}.{}", get::<String>(r, "name")?)),
                definition: get(r, "def")?,
                table: Some(ObjectName::new(schema, table)),
            });
        }
        Ok(out)
    }

    /// Free-standing sequences (not those owned by identity/serial columns).
    async fn load_sequences(&mut self, schemas: &[String], owned: &HashSet<i64>) -> Result<Vec<CodeObject>> {
        let rows = self
            .catalog(
                format!(
                    "SELECT c.oid::int8 AS oid, n.nspname::text AS schema, c.relname::text AS name, \
                       quote_ident(n.nspname) || '.' || quote_ident(c.relname) AS qname, \
                       format_type(s.seqtypid, NULL) AS type_name, s.seqstart, s.seqincrement, \
                       s.seqmin, s.seqmax, s.seqcache, s.seqcycle \
                     FROM pg_sequence s JOIN pg_class c ON c.oid = s.seqrelid \
                     JOIN pg_namespace n ON n.oid = c.relnamespace \
                     WHERE {NS_FILTER} AND {}",
                    not_extension("pg_class", "c.oid")
                ),
                schemas,
            )
            .await?;
        let mut out = Vec::new();
        for r in &rows {
            if owned.contains(&get::<i64>(r, "oid")?) {
                continue;
            }
            let mut def = format!(
                "CREATE SEQUENCE {} AS {} START WITH {} INCREMENT BY {} MINVALUE {} MAXVALUE {}",
                get::<String>(r, "qname")?,
                get::<String>(r, "type_name")?,
                get::<i64>(r, "seqstart")?,
                get::<i64>(r, "seqincrement")?,
                get::<i64>(r, "seqmin")?,
                get::<i64>(r, "seqmax")?,
            );
            let cache: i64 = get(r, "seqcache")?;
            if cache != 1 {
                def.push_str(&format!(" CACHE {cache}"));
            }
            if get::<bool>(r, "seqcycle")? {
                def.push_str(" CYCLE");
            }
            out.push(CodeObject {
                kind: ObjectKind::Sequence,
                name: ObjectName::new(get::<String>(r, "schema")?, get::<String>(r, "name")?),
                definition: def,
                table: None,
            });
        }
        Ok(out)
    }

    pub async fn fetch_rows(&mut self, table: &ObjectName, columns: &[String], order_by: &[String]) -> Result<Vec<Vec<Value>>> {
        let d = PostgresDialect;
        let qualified = d.qualified(table);
        // Domains are classified by their base type.
        let types: HashMap<String, String> = sqlx::query_as::<_, (String, String)>(
            "SELECT a.attname::text, coalesce(bt.typname, t.typname)::text \
             FROM pg_attribute a JOIN pg_type t ON t.oid = a.atttypid \
             LEFT JOIN pg_type bt ON t.typtype = 'd' AND bt.oid = t.typbasetype \
             WHERE a.attrelid = $1::text::regclass AND a.attnum > 0 AND NOT a.attisdropped",
        )
        .bind(&qualified)
        .fetch_all(&mut self.conn)
        .await
        .map_err(err)?
        .into_iter()
        .collect();
        let kinds: Vec<PgKind> =
            columns.iter().map(|c| types.get(c).map_or(PgKind::Other, |t| PgKind::of(t))).collect();
        let select = columns
            .iter()
            .zip(&kinds)
            .map(|(c, k)| if k.native() { d.quote_ident(c) } else { format!("{}::text", d.quote_ident(c)) })
            .collect::<Vec<_>>()
            .join(", ");
        let mut sql = format!("SELECT {select} FROM {qualified}");
        if !order_by.is_empty() {
            sql.push_str(&format!(" ORDER BY {}", d.ident_list(order_by)));
        }
        let rows = sqlx::query(AssertSqlSafe(sql)).fetch_all(&mut self.conn).await.map_err(err)?;
        rows.iter()
            .map(|row| kinds.iter().enumerate().map(|(i, k)| cell(row, i, *k)).collect())
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

/// `CHECK ((price > (0)::numeric))` → `(price > (0)::numeric)`.
fn check_expression(def: &str) -> String {
    let def = def.trim().trim_end_matches(" NOT VALID");
    def.strip_prefix("CHECK ")
        .and_then(|d| d.strip_prefix('('))
        .and_then(|d| d.strip_suffix(')'))
        .unwrap_or(def)
        .trim()
        .to_owned()
}

fn fk_action(code: &str) -> Option<String> {
    match code {
        "c" => Some("CASCADE".into()),
        "n" => Some("SET NULL".into()),
        "d" => Some("SET DEFAULT".into()),
        "r" => Some("RESTRICT".into()),
        _ => None,
    }
}

fn trim_statement(s: &str) -> &str {
    s.trim().trim_end_matches(';').trim_end()
}

/// How a column is read: decoded natively, or cast to text in the SELECT and
/// normalised here (everything not listed is compared as text).
#[derive(Debug, Clone, Copy, PartialEq)]
enum PgKind {
    I16,
    I32,
    I64,
    F32,
    F64,
    Bool,
    Bytes,
    Str,
    Numeric,
    Date,
    Time,
    TimeTz,
    Timestamp,
    TimestampTz,
    Uuid,
    Document,
    Other,
}

impl PgKind {
    fn of(typname: &str) -> PgKind {
        match typname {
            "int2" => PgKind::I16,
            "int4" => PgKind::I32,
            "int8" => PgKind::I64,
            "float4" => PgKind::F32,
            "float8" => PgKind::F64,
            "bool" => PgKind::Bool,
            "bytea" => PgKind::Bytes,
            // Read natively: `bpchar::text` would strip the padding.
            "text" | "varchar" | "bpchar" | "name" => PgKind::Str,
            "numeric" => PgKind::Numeric,
            "date" => PgKind::Date,
            "time" => PgKind::Time,
            "timetz" => PgKind::TimeTz,
            "timestamp" => PgKind::Timestamp,
            "timestamptz" => PgKind::TimestampTz,
            "uuid" => PgKind::Uuid,
            "json" | "jsonb" | "xml" => PgKind::Document,
            _ => PgKind::Other,
        }
    }

    fn native(self) -> bool {
        matches!(
            self,
            PgKind::I16 | PgKind::I32 | PgKind::I64 | PgKind::F32 | PgKind::F64 | PgKind::Bool | PgKind::Bytes | PgKind::Str
        )
    }
}

fn cell(row: &PgRow, i: usize, kind: PgKind) -> Result<Value> {
    if row.try_get_raw(i).map_err(err)?.is_null() {
        return Ok(Value::Null);
    }
    // Unchecked: domains report their own type oid but decode as the base type.
    macro_rules! val {
        ($t:ty) => {
            row.try_get_unchecked::<$t, _>(i).map_err(err)?
        };
    }
    Ok(match kind {
        PgKind::I16 => Value::Int(val!(i16).into()),
        PgKind::I32 => Value::Int(val!(i32).into()),
        PgKind::I64 => Value::Int(val!(i64)),
        // Via the shortest decimal form so 1.1::real reads 1.1, not 1.100000023841858.
        PgKind::F32 => {
            let f = val!(f32);
            Value::Float(f.to_string().parse().unwrap_or(f.into()))
        }
        PgKind::F64 => Value::Float(val!(f64)),
        PgKind::Bool => Value::Bool(val!(bool)),
        PgKind::Bytes => Value::Bytes(val!(Vec<u8>)),
        PgKind::Str => Value::Text(val!(String)),
        kind => {
            let s = val!(String);
            match kind {
                // NaN / Infinity stay text so they're quoted in scripts.
                PgKind::Numeric if s.bytes().all(|c| c.is_ascii_digit() || c == b'-' || c == b'.') => {
                    Value::Decimal(Value::canonical_decimal(&s))
                }
                PgKind::Date => Value::Date(s),
                PgKind::Time => Value::Time(s),
                PgKind::TimeTz => Value::Time(normalize_offset(&s, 8)),
                PgKind::Timestamp => Value::DateTime(s),
                PgKind::TimestampTz => Value::DateTimeTz(normalize_offset(&s, 19)),
                PgKind::Uuid => Value::Uuid(s),
                PgKind::Document => Value::Document(s),
                _ => Value::Text(s),
            }
        }
    })
}

/// PostgreSQL prints zone offsets as `+00` / `+05:30`; canonical is `+HH:MM`.
/// `from` skips the date part so its `-` separators aren't mistaken for a sign.
fn normalize_offset(s: &str, from: usize) -> String {
    let Some(at) = s.get(from..).and_then(|t| t.rfind(['+', '-'])).map(|i| from + i) else {
        return s.to_owned();
    };
    let (base, zone) = s.split_at(at);
    if zone.len() == 3 { format!("{base}{zone}:00") } else { s.to_owned() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helpers() {
        assert_eq!(check_expression("CHECK ((price > (0)::numeric))"), "(price > (0)::numeric)");
        assert_eq!(check_expression("CHECK (qty > 0) NOT VALID"), "qty > 0");
        assert_eq!(normalize_offset("2024-01-02 10:00:00.5+00", 19), "2024-01-02 10:00:00.5+00:00");
        assert_eq!(normalize_offset("2024-01-02 10:00:00", 19), "2024-01-02 10:00:00");
        assert_eq!(normalize_offset("10:00:00-03:30", 8), "10:00:00-03:30");
        assert_eq!(normalize_offset("10:00:00+02", 8), "10:00:00+02:00");
        assert_eq!(fk_action("a"), None);
    }
}
