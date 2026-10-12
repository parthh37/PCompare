//! Tauri commands for Database Compare sessions (Schema + Data compare and
//! sync scripts, SQL Delta style).
//!
//! A session side is either a saved connection, `db://<id>[/<database>]`, or
//! a local SQLite file path (dropping two `.sqlite` files opens an ad-hoc
//! comparison without saving a connection first).
//!
//! The engine functions are async and run on Tauri's tokio runtime directly;
//! only the connection store (JSON file + OS keychain) blocks, so it goes
//! through [`crate::blocking`].

use std::io::Read;
use std::sync::atomic::Ordering;

use pcompare_db::connection::{is_db_path, ConnectionConfig, DbConnection, DbConnectionInput};
use pcompare_db::data_diff::{DataCompareOptions, TableDataDiff};
use pcompare_db::model::Engine;
use pcompare_db::schema_diff::SchemaCompareOptions;
use pcompare_db::script::{ScriptOptions, SyncScript};
use pcompare_db::{DataCompareResult, Progress, SchemaCompareResult};
use tauri::{AppHandle, Emitter};

use crate::{blocking, state, CmdResult};

/// Event carrying [`Progress`] while [`db_compare_data`] runs.
const PROGRESS_EVENT: &str = "db-progress";
const SQLITE_MAGIC: &[u8; 16] = b"SQLite format 3\0";

fn err(e: pcompare_core::CoreError) -> String {
    e.to_string()
}

/// A local SQLite file used without a saved connection.
fn sqlite_file(path: &str) -> CmdResult<ConnectionConfig> {
    let file = path.trim();
    if file.is_empty() {
        return Err("Choose a database connection for both sides.".into());
    }
    if pcompare_remote::is_remote(file) {
        return Err(format!("{file}: SQLite databases must be local files"));
    }
    Ok(ConnectionConfig {
        engine: Some(Engine::Sqlite),
        file: file.to_owned(),
        ..Default::default()
    })
}

/// Turns a session path into a connectable config (password filled in).
async fn resolve(app: &AppHandle, path: &str) -> CmdResult<ConnectionConfig> {
    if !is_db_path(path) {
        return sqlite_file(path);
    }
    let (app, path) = (app.clone(), path.to_owned());
    blocking(move || state(&app).db.resolve_path(&path)).await
}

async fn resolve_pair(app: &AppHandle, left: &str, right: &str) -> CmdResult<(ConnectionConfig, ConnectionConfig)> {
    Ok((resolve(app, left).await?, resolve(app, right).await?))
}

/// The config a dialog's unsaved edits would produce (stored password kept).
async fn preview(app: &AppHandle, input: DbConnectionInput) -> CmdResult<ConnectionConfig> {
    let app = app.clone();
    blocking(move || state(&app).db.preview(&input)).await
}

// ---------- saved connections ----------

#[tauri::command]
pub async fn db_list_connections(app: AppHandle) -> CmdResult<Vec<DbConnection>> {
    blocking(move || state(&app).db.list()).await
}

#[tauri::command]
pub async fn db_save_connection(app: AppHandle, input: DbConnectionInput) -> CmdResult<DbConnection> {
    blocking(move || state(&app).db.save(input)).await
}

#[tauri::command]
pub async fn db_delete_connection(app: AppHandle, id: String) -> CmdResult<()> {
    blocking(move || state(&app).db.delete(&id)).await
}

/// "Test connection" in the connection dialog; works on unsaved edits.
#[tauri::command]
pub async fn db_test_connection(app: AppHandle, input: DbConnectionInput) -> CmdResult<String> {
    let cfg = preview(&app, input).await?;
    pcompare_db::test_connection(&cfg).await.map_err(err)
}

/// Databases on the server, for the dialog's and header's database pickers.
#[tauri::command]
pub async fn db_list_databases(app: AppHandle, input: DbConnectionInput) -> CmdResult<Vec<String>> {
    let cfg = preview(&app, input).await?;
    pcompare_db::list_databases(&cfg).await.map_err(err)
}

/// Whether every path is a SQLite database file (checked by its header, so
/// `.db` files that aren't SQLite — e.g. `Thumbs.db` — keep opening in Hex).
#[tauri::command]
pub async fn db_is_sqlite(paths: Vec<String>) -> CmdResult<bool> {
    blocking(move || {
        Ok(paths.iter().all(|p| {
            let mut head = [0u8; 16];
            std::fs::File::open(p)
                .and_then(|mut f| f.read_exact(&mut head))
                .is_ok_and(|_| &head == SQLITE_MAGIC)
        }))
    })
    .await
}

// ---------- schema compare ----------

#[tauri::command]
pub async fn db_compare_schema(
    app: AppHandle,
    left: String,
    right: String,
    options: SchemaCompareOptions,
) -> CmdResult<SchemaCompareResult> {
    let (l, r) = resolve_pair(&app, &left, &right).await?;
    pcompare_db::compare_schemas(&l, &r, &options).await.map_err(err)
}

#[tauri::command]
pub async fn db_schema_script(
    app: AppHandle,
    left: String,
    right: String,
    compare: SchemaCompareOptions,
    options: ScriptOptions,
) -> CmdResult<SyncScript> {
    let (l, r) = resolve_pair(&app, &left, &right).await?;
    pcompare_db::schema_script(&l, &r, &compare, &options).await.map_err(err)
}

// ---------- data compare ----------

/// Compares the rows of every matching table, emitting `db-progress` events.
/// [`db_cancel`] stops it between tables.
#[tauri::command]
pub async fn db_compare_data(
    app: AppHandle,
    left: String,
    right: String,
    options: DataCompareOptions,
) -> CmdResult<DataCompareResult> {
    let (l, r) = resolve_pair(&app, &left, &right).await?;
    let st = state(&app);
    st.db_cancel.store(false, Ordering::Relaxed);
    let emitter = app.clone();
    let progress = move |p: Progress| {
        // The window may be gone; progress is best-effort.
        let _ = emitter.emit(PROGRESS_EVENT, p);
    };
    pcompare_db::compare_data(&l, &r, &options, &progress, &st.db_cancel)
        .await
        .map_err(err)
}

/// Row-level differences of one table, for the data grid.
#[tauri::command]
pub async fn db_table_data(
    app: AppHandle,
    left: String,
    right: String,
    table_id: String,
    options: DataCompareOptions,
) -> CmdResult<TableDataDiff> {
    let (l, r) = resolve_pair(&app, &left, &right).await?;
    pcompare_db::table_data(&l, &r, &table_id, &options).await.map_err(err)
}

#[tauri::command]
pub async fn db_data_script(
    app: AppHandle,
    left: String,
    right: String,
    compare: DataCompareOptions,
    options: ScriptOptions,
) -> CmdResult<SyncScript> {
    let (l, r) = resolve_pair(&app, &left, &right).await?;
    let st = state(&app);
    st.db_cancel.store(false, Ordering::Relaxed);
    pcompare_db::data_script(&l, &r, &compare, &options, &st.db_cancel)
        .await
        .map_err(err)
}

/// Asks a running data compare or data script to stop after the current table.
#[tauri::command]
pub fn db_cancel(app: AppHandle) {
    state(&app).db_cancel.store(true, Ordering::Relaxed);
}

// ---------- sync ----------

/// Runs a generated script's statements against one side of the session.
#[tauri::command]
pub async fn db_execute_script(
    app: AppHandle,
    target: String,
    statements: Vec<String>,
    transactional: bool,
) -> CmdResult<()> {
    if statements.is_empty() {
        return Err("The script has no statements to run.".into());
    }
    let cfg = resolve(&app, &target).await?;
    pcompare_db::execute_script(&cfg, &statements, transactional)
        .await
        .map_err(err)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_paths_are_ad_hoc_sqlite() {
        let cfg = sqlite_file("/tmp/a.sqlite").unwrap();
        assert_eq!(cfg.engine, Some(Engine::Sqlite));
        assert_eq!(cfg.file, "/tmp/a.sqlite");
        assert!(sqlite_file("  ").is_err());
        assert!(sqlite_file("remote://s3/x.db").is_err());
    }
}
