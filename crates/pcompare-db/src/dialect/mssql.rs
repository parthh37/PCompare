//! Mssql SQL rendering. (Placeholder: only overrides the engine; the driver
//! agent for Mssql fills in quoting, literals and DDL differences.)

use super::Dialect;
use crate::model::Engine;

pub struct MssqlDialect;

impl Dialect for MssqlDialect {
    fn engine(&self) -> Engine {
        Engine::Mssql
    }
}
