//! "Import sessions…": saved sessions from Beyond Compare, WinMerge,
//! FreeFileSync and Meld (parsed by `pcompare-import`) saved as PCompare's.

use crate::{blocking, state, CmdResult};
use pcompare_core::session::{SavedSession, SessionInput};
use pcompare_core::CoreError;
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
