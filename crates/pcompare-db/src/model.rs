//! Engine-neutral description of a database schema.
//!
//! Every driver introspects its server into these types; the diff, data and
//! script modules never talk to a server. Types (`Column::data_type`) and
//! expressions (defaults, checks, view bodies) are kept as the *source
//! engine's* SQL text, so a same-engine sync script can reuse them verbatim.

use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Engine {
    Mssql,
    Mysql,
    Postgres,
    Sqlite,
}

impl Engine {
    pub fn label(self) -> &'static str {
        match self {
            Engine::Mssql => "SQL Server",
            Engine::Mysql => "MySQL / MariaDB",
            Engine::Postgres => "PostgreSQL",
            Engine::Sqlite => "SQLite",
        }
    }

    pub fn default_port(self) -> Option<u16> {
        match self {
            Engine::Mssql => Some(1433),
            Engine::Mysql => Some(3306),
            Engine::Postgres => Some(5432),
            Engine::Sqlite => None,
        }
    }
}

/// A schema-qualified name. `schema` is `""` on engines without schemas
/// inside a database (MySQL, SQLite).
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ObjectName {
    pub schema: String,
    pub name: String,
}

impl ObjectName {
    pub fn new(schema: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            schema: schema.into(),
            name: name.into(),
        }
    }
}

impl fmt::Display for ObjectName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.schema.is_empty() {
            f.write_str(&self.name)
        } else {
            write!(f, "{}.{}", self.schema, self.name)
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Schema {
    pub engine: Option<Engine>,
    pub database: String,
    pub server_version: String,
    pub tables: Vec<Table>,
    /// Views, procedures, functions, triggers and sequences.
    pub objects: Vec<CodeObject>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Table {
    pub name: ObjectName,
    /// In ordinal order.
    pub columns: Vec<Column>,
    pub primary_key: Option<KeyConstraint>,
    /// Secondary indexes and UNIQUE constraints (not the primary key).
    pub indexes: Vec<Index>,
    pub foreign_keys: Vec<ForeignKey>,
    pub checks: Vec<CheckConstraint>,
}

impl Table {
    pub fn column(&self, name: &str) -> Option<&Column> {
        self.columns.iter().find(|c| c.name.eq_ignore_ascii_case(name))
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Column {
    pub name: String,
    /// Full type as the engine writes it in DDL, lower-cased:
    /// `nvarchar(50)`, `int unsigned`, `numeric(10,2)`, `varchar(max)`.
    pub data_type: String,
    pub nullable: bool,
    /// Default expression in source-engine SQL (`getdate()`, `'x'`, `0`).
    pub default: Option<String>,
    /// IDENTITY / AUTO_INCREMENT / GENERATED ... AS IDENTITY / serial.
    pub identity: Option<Identity>,
    /// Computed / generated column expression.
    pub computed: Option<String>,
    /// Only when it differs from the database default.
    pub collation: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Identity {
    pub seed: i64,
    pub increment: i64,
}

impl Default for Identity {
    fn default() -> Self {
        Self { seed: 1, increment: 1 }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyConstraint {
    /// `None` when the engine doesn't name it (SQLite) or it is implicit.
    pub name: Option<String>,
    pub columns: Vec<String>,
    /// SQL Server: CLUSTERED vs NONCLUSTERED.
    pub clustered: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Index {
    pub name: String,
    pub columns: Vec<IndexColumn>,
    pub unique: bool,
    /// Declared as a UNIQUE *constraint* rather than a plain index; scripts
    /// recreate it with `ALTER TABLE ... ADD CONSTRAINT`.
    pub constraint: bool,
    /// Engine-specific access method / kind: `btree`, `hash`, `gin`,
    /// `clustered`, `fulltext`, ... `None` = the engine default.
    pub method: Option<String>,
    /// Partial/filtered index predicate.
    pub filter: Option<String>,
    /// SQL Server / PostgreSQL INCLUDE columns.
    pub include: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexColumn {
    /// Column name, or an expression for expression indexes.
    pub name: String,
    pub descending: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ForeignKey {
    pub name: String,
    pub columns: Vec<String>,
    pub ref_table: ObjectName,
    pub ref_columns: Vec<String>,
    /// Upper-case referential action (`CASCADE`, `SET NULL`, ...); `None` = NO ACTION.
    pub on_delete: Option<String>,
    pub on_update: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckConstraint {
    pub name: String,
    pub expression: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ObjectKind {
    Table,
    View,
    Procedure,
    Function,
    Trigger,
    Sequence,
}

impl ObjectKind {
    pub fn label(self) -> &'static str {
        match self {
            ObjectKind::Table => "Table",
            ObjectKind::View => "View",
            ObjectKind::Procedure => "Procedure",
            ObjectKind::Function => "Function",
            ObjectKind::Trigger => "Trigger",
            ObjectKind::Sequence => "Sequence",
        }
    }
}

/// An object compared by its definition text.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodeObject {
    pub kind: ObjectKind,
    pub name: ObjectName,
    /// The complete, executable `CREATE ...` statement in source-engine SQL
    /// (no trailing `;` or `GO`). Drivers that only store the body
    /// (PostgreSQL functions, sequences) synthesise the full statement.
    pub definition: String,
    /// Triggers: the table they fire on.
    pub table: Option<ObjectName>,
}
