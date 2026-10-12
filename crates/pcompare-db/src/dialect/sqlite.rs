//! Sqlite SQL rendering. (Placeholder: only overrides the engine; the driver
//! agent for Sqlite fills in quoting, literals and DDL differences.)

use super::Dialect;
use crate::model::Engine;

pub struct SqliteDialect;

impl Dialect for SqliteDialect {
    fn engine(&self) -> Engine {
        Engine::Sqlite
    }
}
