//! Postgres SQL rendering. (Placeholder: only overrides the engine; the driver
//! agent for Postgres fills in quoting, literals and DDL differences.)

use super::Dialect;
use crate::model::Engine;

pub struct PostgresDialect;

impl Dialect for PostgresDialect {
    fn engine(&self) -> Engine {
        Engine::Postgres
    }
}
