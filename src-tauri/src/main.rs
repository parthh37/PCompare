// Hide the console window on Windows release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod merge;
mod meta;
mod picture;
mod sync;
mod table;

use pcompare_core::folder::{self, FolderCompareOptions, FolderCompareResult};
use pcompare_core::hex::{self, HexChunk, HexSummary};
use pcompare_core::session::{SavedSession, SessionInput, SessionStore};
use pcompare_core::text::{self, TextDiffOptions, TextDiffResult};
use pcompare_core::textfile::{self, TextFileInfo};
use pcompare_core::PathKind;
use pcompare_remote::profile::{KeyringSecrets, Profile, ProfileInput};
use pcompare_remote::provider::{self, ProviderKind, ProviderSpec};
use pcompare_remote::RemoteManager;
use serde::Serialize;
use std::path::PathBuf;
use tauri::{AppHandle, Manager};
use tauri_plugin_opener::OpenerExt;

struct AppState {
    sessions: SessionStore,
    remote: RemoteManager,
}

fn state(app: &AppHandle) -> tauri::State<'_, AppState> {
    app.state::<AppState>()
}

type CmdResult<T> = Result<T, String>;

/// Runs blocking engine work off the IPC thread so the UI stays responsive.
/// Remote file systems block on network I/O too, so they must run here.
async fn blocking<T, F>(f: F) -> CmdResult<T>
where
    T: Send + 'static,
    F: FnOnce() -> pcompare_core::Result<T> + Send + 'static,
{
    tauri::async_runtime::spawn_blocking(f)
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TextFileCompare {
    left: TextFileInfo,
    right: TextFileInfo,
    /// `None` when either side looks binary; the UI offers the hex view instead.
    diff: Option<TextDiffResult>,
}

#[tauri::command]
fn path_kind(path: String) -> PathKind {
    if pcompare_remote::is_remote(&path) {
        // Remote sessions are opened explicitly; treat them as files here.
        return PathKind::File;
    }
    pcompare_core::path_kind(path)
}

/// Paths passed on the command line, e.g. `pcompare.exe left right`.
#[tauri::command]
fn launch_args() -> Vec<String> {
    std::env::args()
        .skip(1)
        .filter(|a| !a.starts_with('-'))
        .collect()
}

/// Runs `f` and, if it fails, drops cached connections behind both paths so
/// a retry reconnects instead of reusing a dead session.
fn with_remote<T>(
    app: &AppHandle,
    paths: [&str; 2],
    f: impl FnOnce(&RemoteManager) -> pcompare_core::Result<T>,
) -> pcompare_core::Result<T> {
    let remote = &state(app).remote;
    let result = f(remote);
    if result.is_err() {
        paths.iter().for_each(|p| remote.forget(p));
    }
    result
}

#[tauri::command]
async fn compare_text_files(
    app: AppHandle,
    left: String,
    right: String,
    options: TextDiffOptions,
) -> CmdResult<TextFileCompare> {
    blocking(move || {
        let (lp, rp) = with_remote(&app, [&left, &right], |r| {
            Ok((r.local_copy(&left, true)?, r.local_copy(&right, true)?))
        })?;
        let binary = textfile::file_is_binary(&lp)? || textfile::file_is_binary(&rp)?;
        let mut l = textfile::load_text(&lp)?;
        let mut r = textfile::load_text(&rp)?;
        // Show the user's path, not the download cache location.
        l.info.path = left;
        r.info.path = right;
        let diff = (!binary).then(|| text::diff_text(&l.text, &r.text, &options));
        Ok(TextFileCompare {
            left: l.info,
            right: r.info,
            diff,
        })
    })
    .await
}

#[tauri::command]
async fn compare_folders(
    app: AppHandle,
    left: String,
    right: String,
    options: FolderCompareOptions,
) -> CmdResult<FolderCompareResult> {
    blocking(move || {
        with_remote(&app, [&left, &right], |r| {
            let (lfs, lpath) = r.target(&left)?;
            let (rfs, rpath) = r.target(&right)?;
            let mut res = folder::compare_folders_with(&*lfs, &lpath, &*rfs, &rpath, &options)?;
            res.left_root = left.clone();
            res.right_root = right.clone();
            Ok(res)
        })
    })
    .await
}

/// Local paths for a hex session; remote files are downloaded on `refresh`
/// (opening the session) and reused for every subsequent page.
fn hex_paths(app: &AppHandle, left: &str, right: &str, refresh: bool) -> pcompare_core::Result<(PathBuf, PathBuf)> {
    with_remote(app, [left, right], |r| {
        Ok((r.local_copy(left, refresh)?, r.local_copy(right, refresh)?))
    })
}

#[tauri::command]
async fn hex_summary(app: AppHandle, left: String, right: String) -> CmdResult<HexSummary> {
    blocking(move || {
        let (l, r) = hex_paths(&app, &left, &right, true)?;
        hex::summarize(&l, &r)
    })
    .await
}

#[tauri::command]
async fn hex_chunk(app: AppHandle, left: String, right: String, offset: u64, len: usize) -> CmdResult<HexChunk> {
    let len = len.min(1 << 20); // never ship more than 1 MiB per IPC call
    blocking(move || {
        let (l, r) = hex_paths(&app, &left, &right, false)?;
        hex::read_chunk(&l, &r, offset, len)
    })
    .await
}

#[tauri::command]
async fn hex_next_difference(app: AppHandle, left: String, right: String, from: u64) -> CmdResult<Option<u64>> {
    blocking(move || {
        let (l, r) = hex_paths(&app, &left, &right, false)?;
        hex::next_difference(&l, &r, from)
    })
    .await
}

// ---------- saved sessions ----------

#[tauri::command]
async fn list_sessions(app: AppHandle) -> CmdResult<Vec<SavedSession>> {
    blocking(move || state(&app).sessions.list()).await
}

#[tauri::command]
async fn save_session(app: AppHandle, input: SessionInput) -> CmdResult<SavedSession> {
    blocking(move || state(&app).sessions.save(input)).await
}

#[tauri::command]
async fn rename_session(app: AppHandle, id: String, name: String) -> CmdResult<SavedSession> {
    blocking(move || state(&app).sessions.rename(&id, &name)).await
}

#[tauri::command]
async fn delete_session(app: AppHandle, id: String) -> CmdResult<()> {
    blocking(move || state(&app).sessions.delete(&id)).await
}

// ---------- connection profiles ----------

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RemoteEntry {
    name: String,
    is_dir: bool,
    size: u64,
    modified_ms: Option<i64>,
}

#[tauri::command]
fn list_providers() -> Vec<ProviderSpec> {
    provider::catalog()
}

#[tauri::command]
fn oauth_redirect_uri() -> &'static str {
    pcompare_remote::oauth::REDIRECT_URI
}

#[tauri::command]
async fn list_profiles(app: AppHandle) -> CmdResult<Vec<Profile>> {
    blocking(move || state(&app).remote.profiles.list()).await
}

#[tauri::command]
async fn save_profile(app: AppHandle, input: ProfileInput) -> CmdResult<Profile> {
    blocking(move || {
        let remote = &state(&app).remote;
        let saved = remote.profiles.save(input)?;
        remote.invalidate(&saved.id);
        Ok(saved)
    })
    .await
}

#[tauri::command]
async fn delete_profile(app: AppHandle, id: String) -> CmdResult<()> {
    blocking(move || {
        let remote = &state(&app).remote;
        remote.invalidate(&id);
        remote.profiles.delete(&id)
    })
    .await
}

#[tauri::command]
async fn test_profile(app: AppHandle, input: ProfileInput) -> CmdResult<String> {
    blocking(move || state(&app).remote.test(&input)).await
}

#[tauri::command]
async fn sign_in_profile(app: AppHandle, id: String) -> CmdResult<Profile> {
    blocking(move || {
        let opener = app.clone();
        state(&app).remote.sign_in(&id, move |url| {
            opener
                .opener()
                .open_url(url, None::<&str>)
                .map_err(|e| pcompare_core::CoreError::Other(format!("cannot open browser: {e}")))
        })
    })
    .await
}

/// Opens the vendor's developer console (where users register an OAuth app).
/// Takes a provider kind, not a URL, so the webview can't open arbitrary links.
#[tauri::command]
fn open_provider_console(app: AppHandle, kind: ProviderKind) -> CmdResult<()> {
    let url = provider::spec(kind).oauth.map(|o| o.console_url).ok_or("no console for this provider")?;
    app.opener().open_url(url, None::<&str>).map_err(|e| e.to_string())
}

#[tauri::command]
async fn remote_list(app: AppHandle, path: String) -> CmdResult<Vec<RemoteEntry>> {
    blocking(move || {
        with_remote(&app, [&path, ""], |r| r.list(&path)).map(|entries| {
            entries
                .into_iter()
                .map(|e| RemoteEntry {
                    name: e.name,
                    is_dir: e.meta.is_dir,
                    size: e.meta.size,
                    modified_ms: e.meta.modified_ms,
                })
                .collect()
        })
    })
    .await
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            let config_dir = app.path().app_config_dir()?;
            let cache_dir = app.path().app_cache_dir()?.join("remote");
            let remote = RemoteManager::new(&config_dir, cache_dir, Box::new(KeyringSecrets));
            remote.clear_cache();
            app.manage(AppState {
                sessions: SessionStore::new(config_dir.join("sessions.json")),
                remote,
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            path_kind,
            launch_args,
            compare_text_files,
            compare_folders,
            hex_summary,
            hex_chunk,
            hex_next_difference,
            list_sessions,
            save_session,
            rename_session,
            delete_session,
            list_providers,
            oauth_redirect_uri,
            list_profiles,
            save_profile,
            delete_profile,
            test_profile,
            sign_in_profile,
            open_provider_console,
            remote_list,
            // --- text compare editing + text merge (merge.rs) ---
            // --- table compare (table.rs) ---
            // --- picture compare (picture.rs) ---
            // --- folder sync + folder merge (sync.rs) ---
            // --- registry / version / mp3 (meta.rs) ---
        ])
        .run(tauri::generate_context!())
        .expect("error while running PCompare");
}
