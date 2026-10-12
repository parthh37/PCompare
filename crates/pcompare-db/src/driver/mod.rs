//! Live connections. Each engine module exposes the same inherent API on its
//! `Conn` type; [`Client`] dispatches to it so callers stay engine-agnostic.
//!
//! Driver contract (every engine must honour it):
//! - `load_schema` returns user objects only (no system schemas/tables),
//!   sorted by name, in the canonical forms documented on `model.rs`.
//! - `fetch_rows` returns each row's values in `columns` order, converted to
//!   canonical [`Value`]s (see `value.rs`), ordered by the given key columns.
//! - `execute` runs statements in order; with `transactional` it wraps them
//!   in one transaction and rolls back on the first error, reporting the
//!   failing statement's index and text.

pub mod mssql;
pub mod mysql;
pub mod postgres;
pub mod sqlite;

use crate::connection::ConnectionConfig;
use crate::model::{Engine, ObjectName, Schema};
use crate::value::Value;
use pcompare_core::{CoreError, Result};

pub enum Client {
    Mssql(mssql::Conn),
    Mysql(mysql::Conn),
    Postgres(postgres::Conn),
    Sqlite(sqlite::Conn),
}

macro_rules! dispatch {
    ($self:ident, $c:ident => $e:expr) => {
        match $self {
            Client::Mssql($c) => $e,
            Client::Mysql($c) => $e,
            Client::Postgres($c) => $e,
            Client::Sqlite($c) => $e,
        }
    };
}

impl Client {
    pub async fn connect(cfg: &ConnectionConfig) -> Result<Client> {
        Ok(match cfg.engine()? {
            Engine::Mssql => Client::Mssql(mssql::Conn::connect(cfg).await?),
            Engine::Mysql => Client::Mysql(mysql::Conn::connect(cfg).await?),
            Engine::Postgres => Client::Postgres(postgres::Conn::connect(cfg).await?),
            Engine::Sqlite => Client::Sqlite(sqlite::Conn::connect(cfg).await?),
        })
    }

    pub fn engine(&self) -> Engine {
        match self {
            Client::Mssql(_) => Engine::Mssql,
            Client::Mysql(_) => Engine::Mysql,
            Client::Postgres(_) => Engine::Postgres,
            Client::Sqlite(_) => Engine::Sqlite,
        }
    }

    /// e.g. `"PostgreSQL 16.2"`, `"Microsoft SQL Server 2022 (16.0.4135.4)"`.
    pub async fn server_version(&mut self) -> Result<String> {
        dispatch!(self, c => c.server_version().await)
    }

    /// Databases the login can see (empty for SQLite).
    pub async fn list_databases(&mut self) -> Result<Vec<String>> {
        dispatch!(self, c => c.list_databases().await)
    }

    pub async fn load_schema(&mut self) -> Result<Schema> {
        dispatch!(self, c => c.load_schema().await)
    }

    pub async fn fetch_rows(&mut self, table: &ObjectName, columns: &[String], order_by: &[String]) -> Result<Vec<Vec<Value>>> {
        dispatch!(self, c => c.fetch_rows(table, columns, order_by).await)
    }

    pub async fn execute(&mut self, statements: &[String], transactional: bool) -> Result<()> {
        dispatch!(self, c => c.execute(statements, transactional).await)
    }
}

/// Uniform error for driver failures.
pub(crate) fn db_err(context: &str, e: impl std::fmt::Display) -> CoreError {
    CoreError::Other(format!("{context}: {e}"))
}

/// Error for `execute`: which statement failed and why.
pub(crate) fn statement_err(index: usize, sql: &str, e: impl std::fmt::Display) -> CoreError {
    let preview: String = sql.chars().take(300).collect();
    CoreError::Other(format!("statement {} failed: {e}\n{preview}", index + 1))
}
