//! Mysql SQL rendering. (Placeholder: only overrides the engine; the driver
//! agent for Mysql fills in quoting, literals and DDL differences.)

use super::Dialect;
use crate::model::Engine;

pub struct MysqlDialect;

impl Dialect for MysqlDialect {
    fn engine(&self) -> Engine {
        Engine::Mysql
    }
}
