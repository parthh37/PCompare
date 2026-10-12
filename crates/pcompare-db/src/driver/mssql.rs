//! SQL Server driver (tiberius over tokio).
//!
//! Introspection reads the catalog views of the connected database
//! (`sys.tables`, `sys.columns`, `sys.indexes`, `sys.sql_modules`, ...).
//! Model conventions (the dialect relies on them, see `dialect/mssql.rs`):
//! - `Column::default` / `CheckConstraint::expression`: the stored definition
//!   minus redundant outer parentheses (`((0))` → `0`); default constraint
//!   names are not kept.
//! - `Column::computed`: the expression, same paren rule, with ` PERSISTED`
//!   appended for persisted columns.
//! - `Index::method`: `None` = nonclustered rowstore, `clustered`,
//!   `columnstore`, `clustered columnstore`. XML, spatial and hash indexes
//!   are not modelled.
//! - Code objects: `sys.sql_modules` text with a leading `CREATE OR ALTER`
//!   normalised to `CREATE`; encrypted modules (no readable text), synonyms
//!   and user-defined types are skipped.

use crate::connection::{AuthMethod, ConnectionConfig, TlsMode};
use crate::dialect::{quote_string, Dialect, MssqlDialect};
use crate::driver::{db_err, statement_err};
use crate::model::*;
use crate::value::Value;
use chrono::{Datelike, NaiveDate};
use pcompare_core::{CoreError, Result};
use std::collections::HashMap;
use std::time::Duration;
use tiberius::{ColumnData, Config, EncryptionLevel, Row, SqlBrowser};
use tokio::net::TcpStream;
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt};

type TdsClient = tiberius::Client<Compat<TcpStream>>;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// Azure SQL gateways redirect once; more hops means something is wrong.
const MAX_REDIRECTS: usize = 3;

pub struct Conn {
    /// Boxed: the TDS client is large and `Client` is an enum of drivers.
    client: Box<TdsClient>,
    /// ` AND s.name IN (...)` from `cfg.schemas`, or empty.
    schema_filter: String,
}

impl Conn {
    pub async fn connect(cfg: &ConnectionConfig) -> Result<Conn> {
        let (mut config, mut named) = build_config(cfg)?;
        for _ in 0..=MAX_REDIRECTS {
            match open(&config, named).await {
                Ok(client) => return Ok(Conn { client: Box::new(client), schema_filter: schema_filter(&cfg.schemas) }),
                Err(tiberius::error::Error::Routing { host, port }) => {
                    // Azure SQL: the gateway hands us the node to log in to.
                    config.host(&host);
                    config.port(port);
                    named = false;
                }
                Err(e) => return Err(connect_err(e)),
            }
        }
        Err(CoreError::Other("SQL Server connect: too many redirects".into()))
    }

    pub async fn server_version(&mut self) -> Result<String> {
        let rows = self
            .query(
                "SELECT CAST(SERVERPROPERTY('ProductVersion') AS nvarchar(128)), \
                 CAST(SERVERPROPERTY('Edition') AS nvarchar(128)), @@VERSION",
            )
            .await?;
        let row = rows.first().ok_or_else(|| CoreError::Other("SQL Server returned no version".into()))?;
        Ok(format_version(
            &text(row, 2).unwrap_or_default(),
            &text(row, 0).unwrap_or_default(),
            &text(row, 1).unwrap_or_default(),
        ))
    }

    pub async fn list_databases(&mut self) -> Result<Vec<String>> {
        let rows = self
            .query(
                "SELECT name FROM sys.databases \
                 WHERE state = 0 AND database_id > 4 AND HAS_DBACCESS(name) = 1 ORDER BY name",
            )
            .await?;
        Ok(rows.iter().filter_map(|r| text(r, 0)).collect())
    }

    pub async fn load_schema(&mut self) -> Result<Schema> {
        let server_version = self.server_version().await?;
        let head = self
            .query("SELECT DB_NAME(), CAST(DATABASEPROPERTYEX(DB_NAME(), 'Collation') AS nvarchar(128))")
            .await?;
        let database = head.first().and_then(|r| text(r, 0)).unwrap_or_default();
        let db_collation = head.first().and_then(|r| text(r, 1)).unwrap_or_default();
        let filter = self.schema_filter.clone();

        // Tables keyed by object_id.
        let mut tables: HashMap<i64, Table> = HashMap::new();
        for r in self.query(&format!("SELECT t.object_id, s.name, t.name {} ", from_tables(&filter))).await? {
            tables.insert(int(&r, 0), Table { name: ObjectName::new(str_at(&r, 1), str_at(&r, 2)), ..Default::default() });
        }

        let columns_sql = format!(
            "SELECT c.object_id, c.name, ty.name, SCHEMA_NAME(ty.schema_id), ty.is_user_defined, \
             c.max_length, c.precision, c.scale, c.is_nullable, c.collation_name, \
             CAST(ic.seed_value AS bigint), CAST(ic.increment_value AS bigint), c.is_identity, \
             cc.definition, cc.is_persisted, dc.definition \
             FROM sys.columns c \
             JOIN sys.types ty ON ty.user_type_id = c.user_type_id \
             LEFT JOIN sys.identity_columns ic ON ic.object_id = c.object_id AND ic.column_id = c.column_id \
             LEFT JOIN sys.computed_columns cc ON cc.object_id = c.object_id AND cc.column_id = c.column_id \
             LEFT JOIN sys.default_constraints dc ON dc.object_id = c.default_object_id \
             WHERE c.object_id IN (SELECT t.object_id {}) \
             ORDER BY c.object_id, c.column_id",
            from_tables(&filter)
        );
        for r in self.query(&columns_sql).await? {
            let Some(t) = tables.get_mut(&int(&r, 0)) else { continue };
            let type_name = str_at(&r, 2);
            let data_type = if flag(&r, 4) {
                // Alias / CLR user types keep their (schema-qualified) name.
                match text(&r, 3).as_deref() {
                    Some("dbo") | Some("sys") | None => type_name,
                    Some(s) => format!("{s}.{type_name}"),
                }
            } else {
                render_type(&type_name, int(&r, 5), int(&r, 6), int(&r, 7))
            };
            let computed = text(&r, 13).map(|d| {
                let e = strip_outer_parens(&d).to_owned();
                if flag(&r, 14) { format!("{e} PERSISTED") } else { e }
            });
            t.columns.push(Column {
                name: str_at(&r, 1),
                data_type,
                nullable: flag(&r, 8),
                default: text(&r, 15).map(|d| strip_outer_parens(&d).to_owned()),
                identity: flag(&r, 12).then(|| Identity {
                    seed: opt_int(&r, 10).unwrap_or(1),
                    increment: opt_int(&r, 11).unwrap_or(1),
                }),
                computed,
                collation: text(&r, 9).filter(|c| !c.eq_ignore_ascii_case(&db_collation)),
            });
        }

        // Index key / INCLUDE columns, keyed by (object_id, index_id).
        let mut index_cols: HashMap<(i64, i64), (Vec<IndexColumn>, Vec<String>)> = HashMap::new();
        let ic_sql = format!(
            "SELECT ic.object_id, ic.index_id, c.name, ic.is_descending_key, ic.is_included_column, ic.key_ordinal, i.type \
             FROM sys.index_columns ic \
             JOIN sys.indexes i ON i.object_id = ic.object_id AND i.index_id = ic.index_id \
             JOIN sys.columns c ON c.object_id = ic.object_id AND c.column_id = ic.column_id \
             WHERE ic.object_id IN (SELECT t.object_id {}) \
             ORDER BY ic.object_id, ic.index_id, ic.key_ordinal, ic.index_column_id",
            from_tables(&filter)
        );
        for r in self.query(&ic_sql).await? {
            let entry = index_cols.entry((int(&r, 0), int(&r, 1))).or_default();
            let name = str_at(&r, 2);
            let columnstore = matches!(int(&r, 6), 5 | 6);
            if columnstore {
                // Columnstore columns have no key order; keep them all as keys.
                entry.0.push(IndexColumn { name, descending: false });
            } else if flag(&r, 4) {
                entry.1.push(name);
            } else if int(&r, 5) > 0 {
                entry.0.push(IndexColumn { name, descending: flag(&r, 3) });
            }
            // Anything else is a partitioning column, not part of the definition.
        }

        let ix_sql = format!(
            "SELECT i.object_id, i.index_id, i.name, i.type, i.is_unique, i.is_primary_key, \
             i.is_unique_constraint, i.filter_definition \
             FROM sys.indexes i \
             WHERE i.type IN (1, 2, 5, 6) AND i.is_hypothetical = 0 AND i.name IS NOT NULL \
             AND i.object_id IN (SELECT t.object_id {})",
            from_tables(&filter)
        );
        for r in self.query(&ix_sql).await? {
            let Some(t) = tables.get_mut(&int(&r, 0)) else { continue };
            let (keys, include) = index_cols.remove(&(int(&r, 0), int(&r, 1))).unwrap_or_default();
            let kind = int(&r, 3);
            let name = str_at(&r, 2);
            if flag(&r, 5) {
                t.primary_key = Some(KeyConstraint {
                    name: Some(name),
                    columns: keys.into_iter().map(|c| c.name).collect(),
                    clustered: Some(kind == 1),
                });
                continue;
            }
            let method = match kind {
                1 => Some("clustered"),
                5 => Some("clustered columnstore"),
                6 => Some("columnstore"),
                _ => None,
            };
            t.indexes.push(Index {
                name,
                // A clustered columnstore index covers the whole table; DDL lists no columns.
                columns: if kind == 5 { Vec::new() } else { keys },
                unique: flag(&r, 4),
                constraint: flag(&r, 6),
                method: method.map(str::to_owned),
                filter: text(&r, 7),
                include,
            });
        }

        // Foreign keys.
        let mut fk_cols: HashMap<i64, (Vec<String>, Vec<String>)> = HashMap::new();
        for r in self
            .query(
                "SELECT fkc.constraint_object_id, pc.name, rc.name \
                 FROM sys.foreign_key_columns fkc \
                 JOIN sys.columns pc ON pc.object_id = fkc.parent_object_id AND pc.column_id = fkc.parent_column_id \
                 JOIN sys.columns rc ON rc.object_id = fkc.referenced_object_id AND rc.column_id = fkc.referenced_column_id \
                 ORDER BY fkc.constraint_object_id, fkc.constraint_column_id",
            )
            .await?
        {
            let e = fk_cols.entry(int(&r, 0)).or_default();
            e.0.push(str_at(&r, 1));
            e.1.push(str_at(&r, 2));
        }
        for r in self
            .query(
                "SELECT fk.object_id, fk.parent_object_id, fk.name, SCHEMA_NAME(rt.schema_id), rt.name, \
                 fk.delete_referential_action_desc, fk.update_referential_action_desc \
                 FROM sys.foreign_keys fk JOIN sys.tables rt ON rt.object_id = fk.referenced_object_id",
            )
            .await?
        {
            let Some(t) = tables.get_mut(&int(&r, 1)) else { continue };
            let (columns, ref_columns) = fk_cols.remove(&int(&r, 0)).unwrap_or_default();
            t.foreign_keys.push(ForeignKey {
                name: str_at(&r, 2),
                columns,
                ref_table: ObjectName::new(str_at(&r, 3), str_at(&r, 4)),
                ref_columns,
                on_delete: referential_action(&str_at(&r, 5)),
                on_update: referential_action(&str_at(&r, 6)),
            });
        }

        for r in self.query("SELECT parent_object_id, name, definition FROM sys.check_constraints").await? {
            let Some(t) = tables.get_mut(&int(&r, 0)) else { continue };
            t.checks.push(CheckConstraint {
                name: str_at(&r, 1),
                expression: strip_outer_parens(&str_at(&r, 2)).to_owned(),
            });
        }

        let mut tables: Vec<Table> = tables.into_values().collect();
        for t in &mut tables {
            t.indexes.sort_by(|a, b| a.name.cmp(&b.name));
            t.foreign_keys.sort_by(|a, b| a.name.cmp(&b.name));
            t.checks.sort_by(|a, b| a.name.cmp(&b.name));
        }
        tables.sort_by(|a, b| a.name.cmp(&b.name));

        let mut objects = self.load_modules(&filter).await?;
        objects.extend(self.load_sequences(&filter).await?);
        objects.sort_by(|a, b| (&a.name, a.kind).cmp(&(&b.name, b.kind)));

        Ok(Schema { engine: Some(Engine::Mssql), database, server_version, tables, objects })
    }

    async fn load_modules(&mut self, filter: &str) -> Result<Vec<CodeObject>> {
        let sql = format!(
            "SELECT RTRIM(o.type), s.name, o.name, m.definition, \
             OBJECT_SCHEMA_NAME(o.parent_object_id), OBJECT_NAME(o.parent_object_id) \
             FROM sys.objects o \
             JOIN sys.schemas s ON s.schema_id = o.schema_id \
             JOIN sys.sql_modules m ON m.object_id = o.object_id \
             WHERE o.is_ms_shipped = 0 AND o.type IN ('V', 'P', 'FN', 'IF', 'TF', 'TR') \
             AND m.definition IS NOT NULL AND {}",
            user_object_predicate("o", filter)
        );
        let mut out = Vec::new();
        for r in self.query(&sql).await? {
            let kind = match str_at(&r, 0).as_str() {
                "V" => ObjectKind::View,
                "P" => ObjectKind::Procedure,
                "TR" => ObjectKind::Trigger,
                _ => ObjectKind::Function,
            };
            let table = match (kind, text(&r, 4), text(&r, 5)) {
                (ObjectKind::Trigger, Some(s), Some(n)) => Some(ObjectName::new(s, n)),
                _ => None,
            };
            out.push(CodeObject {
                kind,
                name: ObjectName::new(str_at(&r, 1), str_at(&r, 2)),
                definition: MssqlDialect::normalize_definition(&str_at(&r, 3)),
                table,
            });
        }
        Ok(out)
    }

    async fn load_sequences(&mut self, filter: &str) -> Result<Vec<CodeObject>> {
        let sql = format!(
            "SELECT s.name, o.name, TYPE_NAME(o.system_type_id), o.precision, o.scale, \
             CAST(o.start_value AS nvarchar(64)), CAST(o.increment AS nvarchar(64)), \
             CAST(o.minimum_value AS nvarchar(64)), CAST(o.maximum_value AS nvarchar(64)), \
             o.is_cycling, o.is_cached, o.cache_size \
             FROM sys.sequences o JOIN sys.schemas s ON s.schema_id = o.schema_id \
             WHERE o.is_ms_shipped = 0 AND {}",
            user_object_predicate("o", filter)
        );
        let mut out = Vec::new();
        for r in self.query(&sql).await? {
            let name = ObjectName::new(str_at(&r, 0), str_at(&r, 1));
            let seq = Sequence {
                data_type: render_type(&str_at(&r, 2), 0, int(&r, 3), int(&r, 4)),
                start: str_at(&r, 5),
                increment: str_at(&r, 6),
                min: str_at(&r, 7),
                max: str_at(&r, 8),
                cycle: flag(&r, 9),
                cached: flag(&r, 10),
                cache_size: opt_int(&r, 11),
            };
            out.push(CodeObject {
                kind: ObjectKind::Sequence,
                definition: seq.definition(&MssqlDialect.qualified(&name)),
                name,
                table: None,
            });
        }
        Ok(out)
    }

    pub async fn fetch_rows(&mut self, table: &ObjectName, columns: &[String], order_by: &[String]) -> Result<Vec<Vec<Value>>> {
        let d = MssqlDialect;
        let qualified = d.qualified(table);
        // Base types decide which columns need a cast tiberius can decode exactly.
        let types_sql = format!(
            "SELECT c.name, TYPE_NAME(c.system_type_id), ty.is_assembly_type \
             FROM sys.columns c JOIN sys.types ty ON ty.user_type_id = c.user_type_id \
             WHERE c.object_id = OBJECT_ID({})",
            nstring(&qualified)
        );
        let types: HashMap<String, (String, bool)> = self
            .query(&types_sql)
            .await?
            .iter()
            .map(|r| (str_at(r, 0).to_lowercase(), (str_at(r, 1), flag(r, 2))))
            .collect();
        let select = columns
            .iter()
            .map(|c| {
                let (base, clr) = types.get(&c.to_lowercase()).map_or(("", false), |(b, a)| (b.as_str(), *a));
                select_expr(&d.quote_ident(c), base, clr)
            })
            .collect::<Vec<_>>()
            .join(", ");
        let mut sql = format!("SELECT {select} FROM {qualified}");
        if !order_by.is_empty() {
            sql.push_str(&format!(" ORDER BY {}", d.ident_list(order_by)));
        }
        let rows = self.query(&sql).await.map_err(|e| CoreError::Other(format!("{table}: {e}")))?;
        rows.into_iter()
            .map(|row| row.into_iter().map(to_value).collect::<Result<Vec<_>>>())
            .collect()
    }

    pub async fn execute(&mut self, statements: &[String], transactional: bool) -> Result<()> {
        if transactional {
            self.batch("SET XACT_ABORT ON; BEGIN TRANSACTION")
                .await
                .map_err(|e| db_err("BEGIN TRANSACTION", e))?;
        }
        for (i, sql) in statements.iter().enumerate() {
            if let Err(e) = self.batch(sql).await {
                if transactional {
                    // XACT_ABORT may already have rolled back.
                    let _ = self.batch("IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION").await;
                }
                return Err(statement_err(i, sql, e));
            }
        }
        if transactional {
            self.batch("COMMIT TRANSACTION").await.map_err(|e| db_err("COMMIT TRANSACTION", e))?;
        }
        Ok(())
    }

    // ----- plumbing -----

    /// Runs one batch (never through sp_executesql, so session settings such
    /// as `SET IDENTITY_INSERT` persist) and drains every result.
    async fn batch(&mut self, sql: &str) -> tiberius::Result<()> {
        self.client.simple_query(sql).await?.into_results().await.map(drop)
    }

    async fn query(&mut self, sql: &str) -> Result<Vec<Row>> {
        let stream = self.client.simple_query(sql).await.map_err(|e| db_err("SQL Server query", e))?;
        stream.into_first_result().await.map_err(|e| db_err("SQL Server query", e))
    }
}

/// Splits `HOST\INSTANCE` / `HOST,PORT` typed into the host field, maps
/// `.`/`(local)` to localhost. Returns (host, port, instance).
fn parse_server(cfg: &ConnectionConfig) -> (String, Option<u16>, Option<String>) {
    let mut host = cfg.host.trim().to_owned();
    let mut port = cfg.port;
    let mut instance = Some(cfg.instance.trim().to_owned()).filter(|i| !i.is_empty());
    if let Some(rest) = host.strip_prefix("tcp:") {
        host = rest.to_owned();
    }
    if let Some((h, p)) = host.split_once(',') {
        if let Ok(p) = p.trim().parse() {
            port = Some(p);
        }
        host = h.trim().to_owned();
    }
    if let Some((h, i)) = host.split_once('\\') {
        if instance.is_none() && !i.trim().is_empty() {
            instance = Some(i.trim().to_owned());
        }
        host = h.trim().to_owned();
    }
    if host.is_empty() || host == "." || host.eq_ignore_ascii_case("(local)") {
        host = "localhost".into();
    }
    // An explicit port wins over SQL Browser resolution.
    if port.is_some() {
        instance = None;
    }
    (host, port, instance)
}

/// The tiberius config, plus whether to resolve a named instance first.
fn build_config(cfg: &ConnectionConfig) -> Result<(Config, bool)> {
    let (host, port, instance) = parse_server(cfg);
    let mut config = Config::new();
    config.host(&host);
    match &instance {
        Some(i) => config.instance_name(i),
        None => config.port(port.unwrap_or(1433)),
    }
    config.database(if cfg.database.trim().is_empty() { "master" } else { cfg.database.trim() });
    config.application_name("PCompare");
    // Schema scripts can run far longer than ADO.NET's 30 s default.
    config.command_timeout(None);
    config.encryption(match cfg.tls {
        TlsMode::Disable => EncryptionLevel::NotSupported,
        TlsMode::Prefer => EncryptionLevel::On,
        TlsMode::Require => EncryptionLevel::Required,
    });
    if cfg.trust_server_certificate {
        config.trust_cert();
    }
    config.authentication(match cfg.auth {
        AuthMethod::Integrated => integrated_auth()?,
        AuthMethod::Password => password_auth(&cfg.user, &cfg.password),
    });
    Ok((config, instance.is_some()))
}

#[cfg(windows)]
fn integrated_auth() -> Result<tiberius::AuthMethod> {
    Ok(tiberius::AuthMethod::Integrated)
}

#[cfg(not(windows))]
fn integrated_auth() -> Result<tiberius::AuthMethod> {
    Err(CoreError::Other(
        "Windows authentication is only available on Windows; use SQL Server authentication".into(),
    ))
}

/// `DOMAIN\user` logins (never valid SQL logins) use NTLM on Windows.
fn password_auth(user: &str, password: &str) -> tiberius::AuthMethod {
    #[cfg(windows)]
    if user.contains('\\') {
        return tiberius::AuthMethod::windows(user, password);
    }
    tiberius::AuthMethod::sql_server(user, password)
}

async fn open(config: &Config, named: bool) -> tiberius::Result<TdsClient> {
    let tcp = async {
        if named {
            TcpStream::connect_named(config).await
        } else {
            TcpStream::connect(config.get_addr()).await.map_err(tiberius::error::Error::from)
        }
    };
    let tcp = tokio::time::timeout(CONNECT_TIMEOUT, tcp).await.map_err(|_| {
        tiberius::error::Error::from(std::io::Error::new(std::io::ErrorKind::TimedOut, "timed out connecting to the server"))
    })??;
    tcp.set_nodelay(true)?;
    tiberius::Client::connect(config.clone(), tcp.compat_write()).await
}

fn connect_err(e: tiberius::error::Error) -> CoreError {
    let msg = e.to_string();
    let hint = if msg.to_lowercase().contains("certificate") {
        " (enable \"Trust server certificate\" for self-signed certificates)"
    } else {
        ""
    };
    CoreError::Other(format!("SQL Server connect: {msg}{hint}"))
}

/// `"Microsoft SQL Server 2022 (16.0.4135.4) Developer Edition (64-bit)"`.
fn format_version(at_version: &str, product_version: &str, edition: &str) -> String {
    let first = at_version.lines().next().unwrap_or("").trim();
    let product = first.split(" (").next().unwrap_or(first).split(" - ").next().unwrap_or(first).trim();
    let product = if product.is_empty() { "Microsoft SQL Server" } else { product };
    let mut s = product.to_owned();
    if !product_version.is_empty() {
        s.push_str(&format!(" ({product_version})"));
    }
    if !edition.is_empty() {
        s.push(' ');
        s.push_str(edition);
    }
    s
}

fn nstring(s: &str) -> String {
    format!("N{}", quote_string(s))
}

/// `AND s.name IN (...)` for `cfg.schemas`, or empty.
fn schema_filter(schemas: &[String]) -> String {
    let names: Vec<String> = schemas.iter().map(|s| s.trim()).filter(|s| !s.is_empty()).map(nstring).collect();
    if names.is_empty() { String::new() } else { format!(" AND s.name IN ({})", names.join(", ")) }
}

/// Shared predicate for user objects (alias `o`, schema alias `s`): not a
/// system schema and not an SSMS diagram-support object.
fn user_object_predicate(o: &str, filter: &str) -> String {
    format!(
        "s.name NOT IN (N'sys', N'INFORMATION_SCHEMA'){filter} \
         AND NOT EXISTS (SELECT 1 FROM sys.extended_properties ep WHERE ep.class = 1 \
         AND ep.major_id = {o}.object_id AND ep.minor_id = 0 AND ep.name = N'microsoft_database_tools_support')"
    )
}

/// `FROM ... WHERE ...` selecting the user tables to compare (alias `t`).
fn from_tables(filter: &str) -> String {
    format!(
        "FROM sys.tables t JOIN sys.schemas s ON s.schema_id = t.schema_id WHERE t.is_ms_shipped = 0 AND {}",
        user_object_predicate("t", filter)
    )
}

/// DDL spelling of a system type from `sys.columns` metadata.
fn render_type(name: &str, max_length: i64, precision: i64, scale: i64) -> String {
    let name = name.to_lowercase();
    let len = |chars: i64| if max_length == -1 { "max".to_owned() } else { chars.to_string() };
    match name.as_str() {
        "varchar" | "char" | "varbinary" | "binary" => format!("{name}({})", len(max_length)),
        // Lengths are stored in bytes; n-types use two per character.
        "nvarchar" | "nchar" => format!("{name}({})", len(max_length / 2)),
        "decimal" | "numeric" => format!("{name}({precision},{scale})"),
        "datetime2" | "time" | "datetimeoffset" => format!("{name}({scale})"),
        "float" if precision != 53 && precision > 0 => format!("float({precision})"),
        _ => name,
    }
}

/// Strips parentheses that wrap the whole expression: `((0))` → `0`,
/// `(getdate())` → `getdate()`, but `(1)+(2)` stays. Respects string
/// literals and `[bracketed]` names.
fn strip_outer_parens(expr: &str) -> &str {
    let mut s = expr.trim();
    while s.starts_with('(') && s.ends_with(')') && closing_paren(s) == Some(s.len() - 1) {
        s = s[1..s.len() - 1].trim();
    }
    s
}

/// Index of the `)` matching the `(` at index 0.
fn closing_paren(s: &str) -> Option<usize> {
    let mut depth = 0usize;
    let mut quote: Option<u8> = None;
    for (i, &c) in s.as_bytes().iter().enumerate() {
        match quote {
            // A doubled quote/bracket re-enters immediately, which is equivalent.
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None => match c {
                b'\'' => quote = Some(b'\''),
                b'"' => quote = Some(b'"'),
                b'[' => quote = Some(b']'),
                b'(' => depth += 1,
                b')' => {
                    depth = depth.checked_sub(1)?;
                    if depth == 0 {
                        return Some(i);
                    }
                }
                _ => {}
            },
        }
    }
    None
}

/// `NO_ACTION` → `None`, `SET_NULL` → `Some("SET NULL")`.
fn referential_action(desc: &str) -> Option<String> {
    match desc {
        "" | "NO_ACTION" => None,
        d => Some(d.replace('_', " ")),
    }
}

/// Sequence metadata, rendered as an executable CREATE SEQUENCE.
struct Sequence {
    data_type: String,
    start: String,
    increment: String,
    min: String,
    max: String,
    cycle: bool,
    cached: bool,
    cache_size: Option<i64>,
}

impl Sequence {
    fn definition(&self, qualified: &str) -> String {
        let mut s = format!(
            "CREATE SEQUENCE {qualified} AS {} START WITH {} INCREMENT BY {}",
            self.data_type, self.start, self.increment
        );
        // Bounds equal to the type's range are the defaults; leave them out.
        let (lo, hi) = match self.data_type.as_str() {
            "tinyint" => ("0", "255"),
            "smallint" => ("-32768", "32767"),
            "int" => ("-2147483648", "2147483647"),
            "bigint" => ("-9223372036854775808", "9223372036854775807"),
            _ => ("", ""),
        };
        if self.min != lo {
            s.push_str(&format!(" MINVALUE {}", self.min));
        }
        if self.max != hi {
            s.push_str(&format!(" MAXVALUE {}", self.max));
        }
        if self.cycle {
            s.push_str(" CYCLE");
        }
        if !self.cached {
            s.push_str(" NO CACHE");
        } else if let Some(n) = self.cache_size {
            s.push_str(&format!(" CACHE {n}"));
        }
        s
    }
}

/// Select-list expression for one column. money is decoded by tiberius as
/// a lossy f64, and sql_variant / CLR types have no canonical form, so they
/// are converted server-side.
fn select_expr(col: &str, base_type: &str, clr: bool) -> String {
    if clr {
        return format!("{col}.ToString()");
    }
    match base_type {
        "money" => format!("CAST({col} AS decimal(19,4))"),
        "smallmoney" => format!("CAST({col} AS decimal(10,4))"),
        "sql_variant" => format!("CAST({col} AS nvarchar(max))"),
        _ => col.to_owned(),
    }
}

// ----- value conversion -----

fn to_value(cell: ColumnData<'static>) -> Result<Value> {
    Ok(match cell {
        ColumnData::U8(v) => v.map_or(Value::Null, |v| Value::Int(v.into())),
        ColumnData::I16(v) => v.map_or(Value::Null, |v| Value::Int(v.into())),
        ColumnData::I32(v) => v.map_or(Value::Null, |v| Value::Int(v.into())),
        ColumnData::I64(v) => v.map_or(Value::Null, Value::Int),
        // Widen `real` through its shortest text so 0.1f32 stays 0.1.
        ColumnData::F32(v) => v.map_or(Value::Null, |v| Value::Float(v.to_string().parse().unwrap_or(v.into()))),
        ColumnData::F64(v) => v.map_or(Value::Null, Value::Float),
        ColumnData::Bit(v) => v.map_or(Value::Null, Value::Bool),
        ColumnData::String(v) => v.map_or(Value::Null, |s| Value::Text(s.into_owned())),
        ColumnData::Guid(v) => v.map_or(Value::Null, |g| Value::Uuid(g.hyphenated().to_string().to_lowercase())),
        ColumnData::Binary(v) => v.map_or(Value::Null, |b| Value::Bytes(b.into_owned())),
        ColumnData::Numeric(v) => v.map_or(Value::Null, |n| Value::Decimal(numeric_text(n.value(), n.scale()))),
        ColumnData::Xml(v) => v.map_or(Value::Null, |x| Value::Document(x.into_owned().into_string())),
        ColumnData::DateTime(v) => match v {
            Some(dt) => Value::DateTime(legacy_datetime(dt.days(), dt.seconds_fragments())?),
            None => Value::Null,
        },
        ColumnData::SmallDateTime(v) => match v {
            Some(dt) => {
                let date = date_from_days(DAYS_1900 + i64::from(dt.days()))?;
                let mins = u64::from(dt.seconds_fragments());
                Value::DateTime(format!("{date} {}", clock(mins * 60, 0, 0)))
            }
            None => Value::Null,
        },
        ColumnData::Time(v) => v.map_or(Value::Null, |t| {
            let (secs, frac) = split_increments(t.increments(), t.scale());
            Value::Time(clock(secs, frac, t.scale()))
        }),
        ColumnData::Date(v) => match v {
            Some(d) => Value::Date(date_from_days(i64::from(d.days()))?),
            None => Value::Null,
        },
        ColumnData::DateTime2(v) => match v {
            Some(dt) => {
                let t = dt.time();
                let (secs, frac) = split_increments(t.increments(), t.scale());
                Value::DateTime(datetime_text(i64::from(dt.date().days()), secs as i64, frac, t.scale())?)
            }
            None => Value::Null,
        },
        ColumnData::DateTimeOffset(v) => match v {
            Some(dto) => {
                // The wire value is UTC; show the stored local time and offset.
                let dt = dto.datetime2();
                let t = dt.time();
                let (secs, frac) = split_increments(t.increments(), t.scale());
                let offset = i64::from(dto.offset());
                let local = datetime_text(i64::from(dt.date().days()), secs as i64 + offset * 60, frac, t.scale())?;
                let sign = if offset < 0 { '-' } else { '+' };
                Value::DateTimeTz(format!("{local}{sign}{:02}:{:02}", offset.abs() / 60, offset.abs() % 60))
            }
            None => Value::Null,
        },
    })
}

/// Days from 0001-01-01 to 1900-01-01 (the `datetime` epoch).
const DAYS_1900: i64 = 693_595;

/// `YYYY-MM-DD` for a day count from 0001-01-01.
fn date_from_days(days: i64) -> Result<String> {
    i32::try_from(days + 1)
        .ok()
        .and_then(NaiveDate::from_num_days_from_ce_opt)
        .map(|d| format!("{:04}-{:02}-{:02}", d.year(), d.month(), d.day()))
        .ok_or_else(|| CoreError::Other(format!("date out of range ({days} days)")))
}

/// Date-time text for a day count plus seconds that may spill into the
/// neighbouring days (datetimeoffset local shifts).
fn datetime_text(days: i64, secs: i64, frac: u64, scale: u8) -> Result<String> {
    let total = days * 86_400 + secs;
    let date = date_from_days(total.div_euclid(86_400))?;
    Ok(format!("{date} {}", clock(total.rem_euclid(86_400) as u64, frac, scale)))
}

/// `datetime`: 1/300 s ticks, shown rounded to milliseconds as SQL Server does.
fn legacy_datetime(days: i32, ticks: u32) -> Result<String> {
    let ms = (u64::from(ticks) * 10 + 1) / 3;
    datetime_text(DAYS_1900 + i64::from(days), (ms / 1000) as i64, ms % 1000, 3)
}

/// (whole seconds, fractional increments) of a `10^-scale` s counter.
fn split_increments(increments: u64, scale: u8) -> (u64, u64) {
    let unit = 10u64.pow(u32::from(scale));
    (increments / unit, increments % unit)
}

/// `HH:MM:SS[.fraction]` with trailing fractional zeros removed.
fn clock(secs: u64, frac: u64, scale: u8) -> String {
    let mut s = format!("{:02}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60);
    if scale > 0 && frac > 0 {
        let digits = format!("{frac:0width$}", width = scale as usize);
        s.push('.');
        s.push_str(digits.trim_end_matches('0'));
    }
    s
}

/// Canonical decimal text of `value * 10^-scale`.
fn numeric_text(value: i128, scale: u8) -> String {
    let digits = value.unsigned_abs().to_string();
    let scale = scale as usize;
    let s = if scale == 0 {
        digits
    } else {
        let padded = format!("{digits:0>width$}", width = scale + 1);
        let (int, frac) = padded.split_at(padded.len() - scale);
        format!("{int}.{frac}")
    };
    let s = if value < 0 { format!("-{s}") } else { s };
    Value::canonical_decimal(&s)
}

// ----- catalog row helpers (tolerant of the exact integer width) -----

fn cell(r: &Row, i: usize) -> Option<&ColumnData<'static>> {
    r.cells().nth(i).map(|(_, d)| d)
}

fn opt_int(r: &Row, i: usize) -> Option<i64> {
    match cell(r, i)? {
        ColumnData::U8(v) => v.map(i64::from),
        ColumnData::I16(v) => v.map(i64::from),
        ColumnData::I32(v) => v.map(i64::from),
        ColumnData::I64(v) => *v,
        ColumnData::Bit(v) => v.map(i64::from),
        ColumnData::Numeric(v) => v.and_then(|n| i64::try_from(n.int_part()).ok()),
        ColumnData::String(v) => v.as_ref().and_then(|s| s.trim().parse().ok()),
        _ => None,
    }
}

fn int(r: &Row, i: usize) -> i64 {
    opt_int(r, i).unwrap_or(0)
}

fn flag(r: &Row, i: usize) -> bool {
    int(r, i) != 0
}

fn text(r: &Row, i: usize) -> Option<String> {
    match cell(r, i)? {
        ColumnData::String(v) => v.as_ref().map(|s| s.to_string()),
        _ => None,
    }
}

fn str_at(r: &Row, i: usize) -> String {
    text(r, i).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn types_render_like_ddl() {
        assert_eq!(render_type("nvarchar", 100, 0, 0), "nvarchar(50)");
        assert_eq!(render_type("nvarchar", -1, 0, 0), "nvarchar(max)");
        assert_eq!(render_type("NCHAR", 6, 0, 0), "nchar(3)");
        assert_eq!(render_type("varchar", 20, 0, 0), "varchar(20)");
        assert_eq!(render_type("varbinary", -1, 0, 0), "varbinary(max)");
        assert_eq!(render_type("binary", 16, 0, 0), "binary(16)");
        assert_eq!(render_type("decimal", 9, 10, 2), "decimal(10,2)");
        assert_eq!(render_type("numeric", 5, 5, 0), "numeric(5,0)");
        assert_eq!(render_type("datetime2", 8, 27, 7), "datetime2(7)");
        assert_eq!(render_type("time", 5, 16, 3), "time(3)");
        assert_eq!(render_type("datetimeoffset", 10, 34, 7), "datetimeoffset(7)");
        assert_eq!(render_type("float", 8, 53, 0), "float");
        assert_eq!(render_type("int", 4, 10, 0), "int");
        assert_eq!(render_type("datetime", 8, 23, 3), "datetime");
        assert_eq!(render_type("timestamp", 8, 0, 0), "timestamp");
        assert_eq!(render_type("sysname", 256, 0, 0), "sysname");
        assert_eq!(render_type("xml", -1, 0, 0), "xml");
    }

    #[test]
    fn outer_parens_are_stripped() {
        assert_eq!(strip_outer_parens("((0))"), "0");
        assert_eq!(strip_outer_parens("(getdate())"), "getdate()");
        assert_eq!(strip_outer_parens("((-1))"), "-1");
        assert_eq!(strip_outer_parens("(N'a(b')"), "N'a(b'");
        assert_eq!(strip_outer_parens("('x)')"), "'x)'");
        assert_eq!(strip_outer_parens("([Qty]>(0))"), "[Qty]>(0)");
        assert_eq!(strip_outer_parens("((1)+(2))"), "(1)+(2)");
        assert_eq!(strip_outer_parens("(1)+(2)"), "(1)+(2)");
        assert_eq!(strip_outer_parens("([a)]*(2))"), "[a)]*(2)");
        assert_eq!(strip_outer_parens("('it''s')"), "'it''s'");
        assert_eq!(strip_outer_parens("([Qty]*[Price])"), "[Qty]*[Price]");
        assert_eq!(strip_outer_parens("newid()"), "newid()");
        assert_eq!(strip_outer_parens("(("), "((");
    }

    #[test]
    fn referential_actions() {
        assert_eq!(referential_action("NO_ACTION"), None);
        assert_eq!(referential_action("CASCADE").as_deref(), Some("CASCADE"));
        assert_eq!(referential_action("SET_NULL").as_deref(), Some("SET NULL"));
        assert_eq!(referential_action("SET_DEFAULT").as_deref(), Some("SET DEFAULT"));
    }

    #[test]
    fn version_text() {
        let v = "Microsoft SQL Server 2022 (RTM-CU12) (KB5033663) - 16.0.4135.4 (X64) \n\tNov 2023\n\tDeveloper Edition";
        assert_eq!(
            format_version(v, "16.0.4135.4", "Developer Edition (64-bit)"),
            "Microsoft SQL Server 2022 (16.0.4135.4) Developer Edition (64-bit)"
        );
        assert_eq!(
            format_version("Microsoft SQL Azure (RTM) - 12.0.2000.8 \n\tCopyright", "12.0.2000.8", "SQL Azure"),
            "Microsoft SQL Azure (12.0.2000.8) SQL Azure"
        );
        assert_eq!(format_version("", "", ""), "Microsoft SQL Server");
    }

    fn cfg(host: &str, port: Option<u16>, instance: &str) -> ConnectionConfig {
        ConnectionConfig { host: host.into(), port, instance: instance.into(), ..Default::default() }
    }

    #[test]
    fn server_names() {
        assert_eq!(parse_server(&cfg("db1", Some(1500), "")), ("db1".into(), Some(1500), None));
        assert_eq!(parse_server(&cfg("db1", None, "SQLEXPRESS")), ("db1".into(), None, Some("SQLEXPRESS".into())));
        assert_eq!(parse_server(&cfg("db1\\SQLEXPRESS", None, "")), ("db1".into(), None, Some("SQLEXPRESS".into())));
        assert_eq!(parse_server(&cfg("tcp:db1,1501", None, "")), ("db1".into(), Some(1501), None));
        assert_eq!(parse_server(&cfg(".", None, "")), ("localhost".into(), None, None));
        assert_eq!(parse_server(&cfg("(local)\\X", None, "")), ("localhost".into(), None, Some("X".into())));
        assert_eq!(parse_server(&cfg("", None, "")), ("localhost".into(), None, None));
        // An explicit port bypasses SQL Browser.
        assert_eq!(parse_server(&cfg("db1", Some(1433), "X")), ("db1".into(), Some(1433), None));
    }

    #[test]
    fn config_defaults() {
        let (config, named) = build_config(&cfg("db1", None, "")).unwrap();
        assert!(!named);
        assert_eq!(config.get_addr(), "db1:1433");
        let (config, named) = build_config(&cfg("db1\\SQLEXPRESS", None, "")).unwrap();
        assert!(named);
        assert_eq!(config.get_addr(), "db1:1434", "SQL Browser port");
        #[cfg(not(windows))]
        {
            let c = ConnectionConfig { auth: AuthMethod::Integrated, ..cfg("db1", None, "") };
            let err = build_config(&c).map(|_| ()).expect_err("integrated auth needs Windows").to_string();
            assert!(err.contains("only available on Windows"), "{err}");
        }
    }

    #[test]
    fn schema_filters() {
        assert_eq!(schema_filter(&[]), "");
        assert_eq!(schema_filter(&["dbo".into(), " ".into(), "o'k".into()]), " AND s.name IN (N'dbo', N'o''k')");
    }

    #[test]
    fn select_expressions() {
        assert_eq!(select_expr("[m]", "money", false), "CAST([m] AS decimal(19,4))");
        assert_eq!(select_expr("[m]", "smallmoney", false), "CAST([m] AS decimal(10,4))");
        assert_eq!(select_expr("[v]", "sql_variant", false), "CAST([v] AS nvarchar(max))");
        assert_eq!(select_expr("[g]", "", true), "[g].ToString()");
        assert_eq!(select_expr("[i]", "int", false), "[i]");
    }

    #[test]
    fn sequences_render() {
        let seq = Sequence {
            data_type: "bigint".into(),
            start: "1000".into(),
            increment: "5".into(),
            min: "-9223372036854775808".into(),
            max: "9223372036854775807".into(),
            cycle: false,
            cached: true,
            cache_size: None,
        };
        assert_eq!(seq.definition("[dbo].[s]"), "CREATE SEQUENCE [dbo].[s] AS bigint START WITH 1000 INCREMENT BY 5");
        let seq = Sequence { data_type: "int".into(), min: "1".into(), max: "100".into(), cycle: true, cached: false, ..seq };
        assert_eq!(
            seq.definition("[dbo].[s]"),
            "CREATE SEQUENCE [dbo].[s] AS int START WITH 1000 INCREMENT BY 5 MINVALUE 1 MAXVALUE 100 CYCLE NO CACHE"
        );
        let seq = Sequence { data_type: "decimal(18,0)".into(), cached: true, cache_size: Some(50), cycle: false, ..seq };
        assert!(seq.definition("[s]").ends_with("MINVALUE 1 MAXVALUE 100 CACHE 50"));
    }

    #[test]
    fn numerics_are_canonical() {
        assert_eq!(numeric_text(125000, 4), "12.5");
        assert_eq!(numeric_text(-1234, 2), "-12.34");
        assert_eq!(numeric_text(5, 3), "0.005");
        assert_eq!(numeric_text(-5, 3), "-0.005");
        assert_eq!(numeric_text(30000, 4), "3");
        assert_eq!(numeric_text(12345, 0), "12345");
        assert_eq!(numeric_text(0, 2), "0");
        assert_eq!(numeric_text(i128::MAX, 38).len(), "1.70141183460469231731687303715884105727".len());
    }

    #[test]
    fn dates_and_times() {
        assert_eq!(date_from_days(0).unwrap(), "0001-01-01");
        assert_eq!(date_from_days(DAYS_1900).unwrap(), "1900-01-01");
        assert_eq!(date_from_days(738_886).unwrap(), "2024-01-02");
        assert_eq!(clock(36_000, 0, 7), "10:00:00");
        assert_eq!(clock(36_672, 1_234_567, 7), "10:11:12.1234567");
        assert_eq!(clock(36_672, 1_234_560, 7), "10:11:12.123456");
        assert_eq!(clock(1, 5, 2), "00:00:01.05");
        assert_eq!(split_increments(366_721_234_567, 7), (36_672, 1_234_567));
        assert_eq!(datetime_text(738_886, 36_000, 5, 1).unwrap(), "2024-01-02 10:00:00.5");
        // Offsets shift across midnight in both directions.
        assert_eq!(datetime_text(738_886, 3_600 - 7_200, 0, 0).unwrap(), "2024-01-01 23:00:00");
        assert_eq!(datetime_text(738_886, 86_000 + 1_800, 0, 0).unwrap(), "2024-01-03 00:23:20");
    }

    #[test]
    fn legacy_datetime_rounds_to_milliseconds() {
        // 2024-01-02 is 45291 days after 1900-01-01.
        let ten = 10 * 3600 * 300;
        assert_eq!(legacy_datetime(45_291, ten).unwrap(), "2024-01-02 10:00:00");
        assert_eq!(legacy_datetime(45_291, ten + 37).unwrap(), "2024-01-02 10:00:00.123");
        assert_eq!(legacy_datetime(45_291, ten + 1).unwrap(), "2024-01-02 10:00:00.003");
        assert_eq!(legacy_datetime(45_291, ten + 2).unwrap(), "2024-01-02 10:00:00.007");
        assert_eq!(legacy_datetime(45_291, 25_919_999).unwrap(), "2024-01-02 23:59:59.997");
        assert_eq!(legacy_datetime(-53_690, 0).unwrap(), "1753-01-01 00:00:00");
    }

    #[test]
    fn cells_convert_to_canonical_values() {
        use tiberius::numeric::Numeric;
        use tiberius::time::{Date, DateTime2, DateTimeOffset, Time};
        assert_eq!(to_value(ColumnData::U8(Some(255))).unwrap(), Value::Int(255));
        assert_eq!(to_value(ColumnData::I64(None)).unwrap(), Value::Null);
        assert_eq!(to_value(ColumnData::F32(Some(0.1))).unwrap(), Value::Float(0.1));
        assert_eq!(to_value(ColumnData::Bit(Some(true))).unwrap(), Value::Bool(true));
        assert_eq!(
            to_value(ColumnData::Numeric(Some(Numeric::new_with_scale(125000, 4)))).unwrap(),
            Value::Decimal("12.5".into())
        );
        let g = tiberius::Uuid::parse_str("6F9619FF-8B86-D011-B42D-00C04FC964FF").unwrap();
        assert_eq!(to_value(ColumnData::Guid(Some(g))).unwrap(), Value::Uuid("6f9619ff-8b86-d011-b42d-00c04fc964ff".into()));
        let date = Date::new(738_886);
        assert_eq!(to_value(ColumnData::Date(Some(date))).unwrap(), Value::Date("2024-01-02".into()));
        let t = Time::new(360_001_234_560, 7);
        assert_eq!(to_value(ColumnData::Time(Some(t))).unwrap(), Value::Time("10:00:00.123456".into()));
        assert_eq!(
            to_value(ColumnData::DateTime2(Some(DateTime2::new(date, t)))).unwrap(),
            Value::DateTime("2024-01-02 10:00:00.123456".into())
        );
        // Stored as 04:30 UTC with a +05:30 offset → local 10:00+05:30.
        let utc = DateTime2::new(date, Time::new((4 * 3600 + 1800) * 10 + 5, 1));
        assert_eq!(
            to_value(ColumnData::DateTimeOffset(Some(DateTimeOffset::new(utc, 330)))).unwrap(),
            Value::DateTimeTz("2024-01-02 10:00:00.5+05:30".into())
        );
        let utc = DateTime2::new(date, Time::new(3600, 0));
        assert_eq!(
            to_value(ColumnData::DateTimeOffset(Some(DateTimeOffset::new(utc, -480)))).unwrap(),
            Value::DateTimeTz("2024-01-01 17:00:00-08:00".into())
        );
        let sdt = tiberius::time::SmallDateTime::new(45_291, 630);
        assert_eq!(to_value(ColumnData::SmallDateTime(Some(sdt))).unwrap(), Value::DateTime("2024-01-02 10:30:00".into()));
    }
}
