//! SQLite driver. (Placeholder until the SQLite driver lands.)

use crate::connection::ConnectionConfig;
use crate::model::{ObjectName, Schema};
use crate::value::Value;
use pcompare_core::{CoreError, Result};

pub struct Conn;

fn unsupported<T>() -> Result<T> {
    Err(CoreError::Other("SQLite support is not built yet".into()))
}

impl Conn {
    pub async fn connect(_cfg: &ConnectionConfig) -> Result<Conn> {
        unsupported()
    }
    pub async fn server_version(&mut self) -> Result<String> {
        unsupported()
    }
    pub async fn list_databases(&mut self) -> Result<Vec<String>> {
        unsupported()
    }
    pub async fn load_schema(&mut self) -> Result<Schema> {
        unsupported()
    }
    pub async fn fetch_rows(&mut self, _table: &ObjectName, _columns: &[String], _order_by: &[String]) -> Result<Vec<Vec<Value>>> {
        unsupported()
    }
    pub async fn execute(&mut self, _statements: &[String], _transactional: bool) -> Result<()> {
        unsupported()
    }
}
