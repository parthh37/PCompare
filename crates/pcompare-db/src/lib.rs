//! Database comparison for PCompare — a SQL Delta style Schema Compare and
//! Data Compare across SQL Server, MySQL/MariaDB, PostgreSQL and SQLite.
//!
//! Layers:
//! - [`driver`]: live connections that introspect a server into the
//!   engine-neutral [`model`] and read rows as canonical [`value::Value`]s.
//! - [`schema_diff`] / [`data_diff`]: pure comparisons of those models.
//! - [`dialect`] + [`script`]: render sync scripts in the target's SQL.
//!
//! The async functions below are the API the Tauri shell calls.

pub mod connection;
pub mod data_diff;
pub mod dialect;
pub mod driver;
pub mod model;
pub mod schema_diff;
pub mod script;
pub mod value;

use connection::ConnectionConfig;
use data_diff::{DataCompareOptions, TableDataDiff, TableDataSummary};
use driver::Client;
use model::{Engine, Schema};
use pcompare_core::{CoreError, Result};
use schema_diff::{DiffStats, ObjectDiff, SchemaCompareOptions};
use script::{ScriptOptions, SyncDirection, SyncScript};
use serde::Serialize;
use std::sync::atomic::{AtomicBool, Ordering};

/// Header info for one side of a comparison.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SideInfo {
    pub engine: Engine,
    /// `host/database` or the SQLite file (never credentials).
    pub label: String,
    pub database: String,
    pub server_version: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SchemaCompareResult {
    pub left: SideInfo,
    pub right: SideInfo,
    pub objects: Vec<ObjectDiff>,
    pub stats: DiffStats,
    /// Comparing different engines: types and bodies won't match literally.
    pub cross_engine: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DataCompareResult {
    pub left: SideInfo,
    pub right: SideInfo,
    pub tables: Vec<TableDataSummary>,
    pub identical: bool,
}

/// Progress of a long data comparison.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Progress {
    pub done: usize,
    pub total: usize,
    /// Table being compared.
    pub current: String,
}

fn side(cfg: &ConnectionConfig, schema: &Schema) -> SideInfo {
    SideInfo {
        engine: schema.engine.or(cfg.engine).unwrap_or(Engine::Sqlite),
        label: cfg.describe(),
        database: schema.database.clone(),
        server_version: schema.server_version.clone(),
    }
}

/// Connects and reports the server version — the "Test connection" button.
pub async fn test_connection(cfg: &ConnectionConfig) -> Result<String> {
    let mut c = Client::connect(cfg).await?;
    let version = c.server_version().await?;
    Ok(format!("Connected — {version}"))
}

pub async fn list_databases(cfg: &ConnectionConfig) -> Result<Vec<String>> {
    Client::connect(cfg).await?.list_databases().await
}

async fn load_both(left: &ConnectionConfig, right: &ConnectionConfig) -> Result<(Client, Schema, Client, Schema)> {
    let load = |cfg: &ConnectionConfig| {
        let cfg = cfg.clone();
        async move {
            let mut c = Client::connect(&cfg).await.map_err(|e| prefix(&cfg, e))?;
            let s = c.load_schema().await.map_err(|e| prefix(&cfg, e))?;
            Ok::<_, CoreError>((c, s))
        }
    };
    let ((lc, ls), (rc, rs)) = tokio::try_join!(load(left), load(right))?;
    Ok((lc, ls, rc, rs))
}

fn prefix(cfg: &ConnectionConfig, e: CoreError) -> CoreError {
    CoreError::Other(format!("{}: {e}", cfg.describe()))
}

pub async fn compare_schemas(
    left: &ConnectionConfig,
    right: &ConnectionConfig,
    opts: &SchemaCompareOptions,
) -> Result<SchemaCompareResult> {
    let (_, ls, _, rs) = load_both(left, right).await?;
    let (objects, stats) = schema_diff::compare_schemas(&ls, &rs, opts);
    Ok(SchemaCompareResult {
        left: side(left, &ls),
        right: side(right, &rs),
        objects,
        stats,
        cross_engine: ls.engine != rs.engine,
    })
}

pub async fn schema_script(
    left: &ConnectionConfig,
    right: &ConnectionConfig,
    cmp: &SchemaCompareOptions,
    opts: &ScriptOptions,
) -> Result<SyncScript> {
    let (_, ls, _, rs) = load_both(left, right).await?;
    Ok(script::schema_script(&ls, &rs, cmp, opts))
}

/// Reads and diffs one planned table.
async fn diff_table(
    lc: &mut Client,
    rc: &mut Client,
    plan: &data_diff::TablePlan,
    opts: &DataCompareOptions,
) -> Result<TableDataDiff> {
    let right_key: Vec<String> = plan.right_columns[..plan.key.len()].to_vec();
    let (l, r) = tokio::try_join!(
        lc.fetch_rows(&plan.left, &plan.columns, &plan.key),
        rc.fetch_rows(&plan.right, &plan.right_columns, &right_key),
    )?;
    Ok(data_diff::diff_rows(plan, l, r, &opts.rules))
}

/// Compares every selected table's rows. Returns summaries only; open a
/// table with [`table_data`]. `cancel` stops between tables.
pub async fn compare_data(
    left: &ConnectionConfig,
    right: &ConnectionConfig,
    opts: &DataCompareOptions,
    progress: &(dyn Fn(Progress) + Send + Sync),
    cancel: &AtomicBool,
) -> Result<DataCompareResult> {
    let (mut lc, ls, mut rc, rs) = load_both(left, right).await?;
    let (plans, mut tables) = data_diff::plan_tables(&ls, &rs, opts);
    let total = plans.len();
    for (done, plan) in plans.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            return Err(CoreError::Other("cancelled".into()));
        }
        progress(Progress { done, total, current: plan.left.to_string() });
        let summary = if plan.error.is_some() {
            data_diff::diff_rows(plan, Vec::new(), Vec::new(), &opts.rules).summary
        } else {
            match diff_table(&mut lc, &mut rc, plan, opts).await {
                Ok(d) => d.summary,
                Err(e) => {
                    let mut s = data_diff::diff_rows(plan, Vec::new(), Vec::new(), &opts.rules).summary;
                    s.error = Some(e.to_string());
                    s
                }
            }
        };
        tables.push(summary);
    }
    progress(Progress { done: total, total, current: String::new() });
    tables.sort_by(|a, b| a.id.cmp(&b.id));
    let identical = tables.iter().all(|t| {
        t.error.is_none()
            && t.status == Some(schema_diff::DiffStatus::Same)
    });
    Ok(DataCompareResult { left: side(left, &ls), right: side(right, &rs), tables, identical })
}

/// Row-level differences of one table (capped at `opts.max_rows`).
pub async fn table_data(
    left: &ConnectionConfig,
    right: &ConnectionConfig,
    table_id: &str,
    opts: &DataCompareOptions,
) -> Result<TableDataDiff> {
    let (mut lc, ls, mut rc, rs) = load_both(left, right).await?;
    let (plans, _) = data_diff::plan_tables(&ls, &rs, opts);
    let plan = plans
        .iter()
        .find(|p| p.id == table_id)
        .ok_or_else(|| CoreError::Other(format!("table `{table_id}` is not on both sides")))?;
    if let Some(e) = &plan.error {
        return Err(CoreError::Other(e.clone()));
    }
    let mut diff = diff_table(&mut lc, &mut rc, plan, opts).await?;
    if diff.rows.len() > opts.max_rows {
        diff.rows.truncate(opts.max_rows);
        diff.truncated = true;
    }
    Ok(diff)
}

pub async fn data_script(
    left: &ConnectionConfig,
    right: &ConnectionConfig,
    cmp: &DataCompareOptions,
    opts: &ScriptOptions,
    cancel: &AtomicBool,
) -> Result<SyncScript> {
    let (mut lc, ls, mut rc, rs) = load_both(left, right).await?;
    let (plans, _) = data_diff::plan_tables(&ls, &rs, cmp);
    let mut diffs = Vec::new();
    for plan in plans.iter().filter(|p| p.error.is_none() && (opts.ids.is_empty() || opts.ids.contains(&p.id))) {
        if cancel.load(Ordering::Relaxed) {
            return Err(CoreError::Other("cancelled".into()));
        }
        diffs.push(diff_table(&mut lc, &mut rc, plan, cmp).await?);
    }
    let target = match opts.direction {
        SyncDirection::LeftToRight => &rs,
        SyncDirection::RightToLeft => &ls,
    };
    Ok(script::data_script(&diffs, target, opts))
}

/// Runs a generated script's statements against `target`.
pub async fn execute_script(target: &ConnectionConfig, statements: &[String], transactional: bool) -> Result<()> {
    Client::connect(target).await?.execute(statements, transactional).await
}
