//! "Import sessions…": saved sessions from Beyond Compare, WinMerge,
//! FreeFileSync and Meld (parsed by `pcompare-import`) saved as PCompare's,
//! and "Import connections…": database connections from SQL Server
//! Management Studio and other database tools (saved via `db_import_connections`).

use crate::{blocking, state, CmdResult};
use pcompare_core::session::{SavedSession, SessionInput};
use pcompare_core::CoreError;
use pcompare_import::connections::{ConnectionSource, ImportedConnection};
use pcompare_import::{Candidate, ImportSource};
use std::path::PathBuf;

/// Session files of other tools that exist in their usual places.
#[tauri::command]
pub async fn import_discover() -> CmdResult<Vec<Candidate>> {
    blocking(|| Ok(pcompare_import::discover())).await
}

#[tauri::command]
pub async fn import_read(path: String) -> CmdResult<ImportSource> {
    blocking(move || pcompare_import::read(&PathBuf::from(path)).map_err(|e| CoreError::Other(e.to_string()))).await
}

/// Saves each session as new; clashing names get a ` (2)` suffix.
#[tauri::command]
pub async fn import_commit(app: tauri::AppHandle, sessions: Vec<SessionInput>) -> CmdResult<Vec<SavedSession>> {
    blocking(move || {
        let store = &state(&app).sessions;
        sessions
            .into_iter()
            .map(|mut s| {
                s.id = None;
                store.save(s)
            })
            .collect()
    })
    .await
}

// ---------- database connections ----------

/// Connection lists of database tools found in their usual places.
#[tauri::command]
pub async fn import_discover_connections() -> CmdResult<Vec<Candidate>> {
    blocking(|| Ok(pcompare_import::connections::discover())).await
}

#[tauri::command]
pub async fn import_read_connections(path: String) -> CmdResult<ConnectionSource> {
    blocking(move || {
        pcompare_import::connections::read(&PathBuf::from(path)).map_err(|e| CoreError::Other(e.to_string()))
    })
    .await
}

/// A pasted connection string, as a connection plus the password it carried
/// (the user typed it into this dialog, so it goes back to the form).
#[derive(serde::Serialize)]
pub struct PastedConnection {
    connection: ImportedConnection,
    password: Option<String>,
}

#[tauri::command]
pub fn import_parse_connection_string(text: String) -> CmdResult<PastedConnection> {
    pcompare_import::connections::from_connection_string(&text)
        .map(|(connection, password)| PastedConnection { connection, password })
        .map_err(|e| e.to_string())
}
