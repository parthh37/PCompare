// Hide the console window on Windows release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod merge;
mod meta;
mod picture;
mod sync;
mod table;
mod watch;

use pcompare_core::folder::{self, ContentMethod, ContentVerdict, FolderCompareOptions, FolderCompareResult};
use pcompare_core::hex::{self, HexChunk, HexSummary};
use pcompare_core::session::{SavedSession, SessionInput, SessionStore};
use pcompare_core::text::{self, TextDiffOptions, TextDiffResult};
use pcompare_core::textfile::{self, TextFileInfo};
use pcompare_core::vfs::{EmptyFs, Vfs};
use pcompare_core::PathKind;
use pcompare_remote::profile::{KeyringSecrets, Profile, ProfileInput};
use pcompare_remote::provider::{self, ProviderKind, ProviderSpec};
use pcompare_remote::RemoteManager;
use serde::Serialize;
use std::path::PathBuf;
use std::sync::Arc;
use tauri::{AppHandle, Manager};
use tauri_plugin_opener::OpenerExt;

struct AppState {
    sessions: SessionStore,
    remote: RemoteManager,
    watchers: watch::Watchers,
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

/// Where "Select Left Side for Compare" remembers its path between the
/// Explorer menu, the in-app menu and the next "Compare to…".
fn left_selection_file() -> PathBuf {
    std::env::temp_dir().join("pcompare-left-selection.txt")
}

fn read_left_selection() -> Option<String> {
    let s = std::fs::read_to_string(left_selection_file()).ok()?;
    let s = s.trim();
    (!s.is_empty()).then(|| s.to_owned())
}

/// Turns the command line into the paths to open. Plain paths pass through;
/// `--compare-to <path>` pairs `path` with the remembered left side (or opens
/// it alone when nothing was selected) and consumes the selection.
fn resolve_launch_args(args: &[String], left_selection: Option<String>) -> Vec<String> {
    if let Some(i) = args.iter().position(|a| a == "--compare-to") {
        let Some(target) = args.get(i + 1) else { return Vec::new() };
        return match left_selection {
            Some(left) if &left != target => vec![left, target.clone()],
            _ => vec![target.clone()],
        };
    }
    args.iter().filter(|a| !a.starts_with('-')).cloned().collect()
}

/// The paths this process was launched with, resolved once in `main` before
/// any window exists (so the webview never sees a half-consumed selection).
static LAUNCH_PATHS: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();

fn resolve_launch_paths(args: &[String]) -> Vec<String> {
    if !args.iter().any(|a| a == "--compare-to") {
        return resolve_launch_args(args, None);
    }
    let paths = resolve_launch_args(args, read_left_selection());
    let _ = std::fs::remove_file(left_selection_file());
    paths
}

/// Paths passed on the command line, e.g. `pcompare.exe left right`.
#[tauri::command]
fn launch_args() -> Vec<String> {
    LAUNCH_PATHS.get().cloned().unwrap_or_default()
}

/// The path picked with "Select Left Side for Compare", if any.
#[tauri::command]
fn get_left_selection() -> Option<String> {
    read_left_selection()
}

/// Remembers (or with `None`, forgets) the left side for a later "Compare to…".
#[tauri::command]
fn set_left_selection(path: Option<String>) -> CmdResult<()> {
    let file = left_selection_file();
    match path {
        Some(p) => std::fs::write(file, p).map_err(|e| e.to_string()),
        None => match std::fs::remove_file(file) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.to_string()),
            _ => Ok(()),
        },
    }
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
        let diff = if binary {
            None
        } else {
            Some(text::diff_text(&l.text, &r.text, &options)?)
        };
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
            // A side with no folder chosen yet lists as empty, so the other
            // side's contents show as soon as one folder is picked.
            let side = |p: &str| -> pcompare_core::Result<(Arc<dyn Vfs>, String)> {
                if p.is_empty() {
                    Ok((Arc::new(EmptyFs), String::new()))
                } else {
                    r.target(p)
                }
            };
            let (lfs, lpath) = side(&left)?;
            let (rfs, rpath) = side(&right)?;
            let mut res = folder::compare_folders_with(&*lfs, &lpath, &*rfs, &rpath, &options)?;
            res.left_root = left.clone();
            res.right_root = right.clone();
            Ok(res)
        })
    })
    .await
}

/// Re-checks one file pair with a specific content method — the folder
/// view's "Compare contents" action on a selected row.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
async fn compare_file_contents(
    app: AppHandle,
    left: String,
    left_size: u64,
    right: String,
    right_size: u64,
    method: ContentMethod,
    rules: TextDiffOptions,
) -> CmdResult<ContentVerdict> {
    blocking(move || {
        with_remote(&app, [&left, &right], |r| {
            let (lfs, lpath) = r.target(&left)?;
            let (rfs, rpath) = r.target(&right)?;
            folder::compare_contents(&*lfs, &lpath, left_size, &*rfs, &rpath, right_size, method, &rules)
        })
    })
    .await
}

// ---------- auto-refresh ----------

/// Watches local paths for a session; changes arrive as `fs-changed` events.
#[tauri::command]
fn watch_paths(app: AppHandle, id: String, paths: Vec<String>, exclude: Vec<String>) -> CmdResult<usize> {
    state(&app).watchers.watch(app.clone(), id, paths, exclude)
}

#[tauri::command]
fn unwatch_paths(app: AppHandle, id: String) {
    state(&app).watchers.unwatch(&id);
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
    // Explorer's "Select Left Side for Compare" just records the path and
    // exits without opening a window, like Beyond Compare's shell extension.
    let args: Vec<String> = std::env::args().collect();
    if let Some(i) = args.iter().position(|a| a == "--select-left") {
        if let Some(path) = args.get(i + 1) {
            let _ = std::fs::write(left_selection_file(), path);
        }
        return;
    }
    let _ = LAUNCH_PATHS.set(resolve_launch_paths(args.get(1..).unwrap_or_default()));

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .setup(|app| {
            let config_dir = app.path().app_config_dir()?;
            let cache_dir = app.path().app_cache_dir()?.join("remote");
            let remote = RemoteManager::new(&config_dir, cache_dir, Box::new(KeyringSecrets));
            remote.clear_cache();
            app.manage(AppState {
                sessions: SessionStore::new(config_dir.join("sessions.json")),
                remote,
                watchers: watch::Watchers::default(),
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            path_kind,
            launch_args,
            get_left_selection,
            set_left_selection,
            compare_text_files,
            compare_folders,
            compare_file_contents,
            watch_paths,
            unwatch_paths,
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
            merge::load_text_pair,
            merge::diff_texts,
            merge::save_text_file,
            merge::load_merge,
            // --- table compare (table.rs) ---
            table::compare_tables,
            // --- picture compare (picture.rs) ---
            picture::picture_load,
            picture::picture_diff,
            // --- folder sync + folder merge (sync.rs) ---
            sync::sync_plan,
            sync::sync_execute,
            sync::sync_cancel,
            sync::folder_merge_compare,
            sync::folder_merge_execute,
            // --- registry / version / mp3 (meta.rs) ---
            meta::compare_registry,
            meta::compare_versions,
            meta::compare_audio,
        ])
        .run(tauri::generate_context!())
        .expect("error while running PCompare");
}

#[cfg(test)]
mod launch_tests {
    use super::resolve_launch_args;

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn plain_paths_pass_through() {
        assert_eq!(resolve_launch_args(&args(&["a", "-v", "b"]), None), args(&["a", "b"]));
    }

    #[test]
    fn compare_to_pairs_with_the_selected_left() {
        let got = resolve_launch_args(&args(&["--compare-to", "b"]), Some("a".into()));
        assert_eq!(got, args(&["a", "b"]));
    }

    #[test]
    fn compare_to_without_selection_opens_one_side() {
        assert_eq!(resolve_launch_args(&args(&["--compare-to", "b"]), None), args(&["b"]));
        assert_eq!(resolve_launch_args(&args(&["--compare-to", "b"]), Some("b".into())), args(&["b"]));
        assert!(resolve_launch_args(&args(&["--compare-to"]), None).is_empty());
    }
}
