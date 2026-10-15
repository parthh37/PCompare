// Hide the console window on Windows release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod archive; // bc5:archives
mod automation; // automation:shell
mod close_guard;
mod convert; // bc5:converters
mod db;
mod fileops; // bc5:file-ops
mod folder; // bc5:folder-engine
mod hex; // bc5:hex
mod merge;
mod meta;
mod patch; // bc5:editor-patch
mod picture;
mod remote; // bc5:remote-write
mod sync;
mod table;
mod text_compare; // bc5:text-engine
mod watch;
mod workspace; // bc5:app-shell
mod cli; // bc5:reports-script
mod report; // bc5:reports-script
mod review; // araxis:text-review
mod vcs; // araxis:vcs
mod explorer;
mod import;

use pcompare_core::session::{SavedSession, SessionInput, SessionStore};
use pcompare_core::text::{self, TextDiffOptions, TextDiffResult};
use pcompare_core::textfile::{self, TextFileInfo};
use pcompare_core::PathKind;
use pcompare_remote::profile::{KeyringSecrets, Profile, ProfileInput};
use pcompare_remote::provider::{self, ProviderKind, ProviderSpec};
use pcompare_remote::RemoteManager;
use serde::Serialize;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use tauri::{AppHandle, Manager};
use tauri_plugin_opener::OpenerExt;

struct AppState {
    sessions: SessionStore,
    remote: RemoteManager,
    watchers: watch::Watchers,
    /// Saved database connections (passwords in the OS keychain).
    db: pcompare_db::connection::ConnectionStore,
    /// Set by `db_cancel` to stop a running data compare between tables.
    db_cancel: AtomicBool,
    /// The scan database (checksum cache, sync baselines, job journal).
    /// `None` if it couldn't be opened; everything still works, just without
    /// caching, two-way history or crash recovery.
    index: Option<pcompare_index::Index>,
}

/// Opens the scan database, quarantining a corrupt file and marking syncs
/// that were running when the app last died as interrupted.
fn open_index(data_dir: &std::path::Path) -> Option<pcompare_index::Index> {
    let index = match pcompare_index::Index::open(&data_dir.join("pcompare.db")) {
        Ok(ix) => ix,
        Err(e) => {
            eprintln!("scan database unavailable: {e}");
            return None;
        }
    };
    if let Err(e) = index.mark_interrupted() {
        eprintln!("scan database: {e}");
    }
    // Keep a quarter of history; interrupted jobs are never pruned.
    let _ = index.prune_jobs(90);
    Some(index)
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
async fn path_kind(app: AppHandle, path: String) -> PathKind {
    // bc5:archives — inside archives and snapshots, and svn: repositories
    if pcompare_remote::is_container_path(&path) {
        return archive::path_kind(app, path).await;
    }
    if pcompare_remote::is_remote(&path) {
        // Remote sessions are opened explicitly; treat them as files here.
        return PathKind::File;
    }
    pcompare_core::path_kind(path)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DiskFree {
    /// Bytes available to this user.
    free: u64,
    /// The volume as people name it: `C:\` or `\\server\share` on Windows,
    /// empty elsewhere (a mount point says little there).
    volume: String,
}

/// Free space on the volume holding a local folder, for the folder views'
/// per-side status. `None` for remote, archive and VCS paths.
#[tauri::command]
async fn disk_free(path: String) -> Option<DiskFree> {
    if path.is_empty() || pcompare_remote::is_remote(&path) || pcompare_remote::is_container_path(&path) {
        return None;
    }
    tauri::async_runtime::spawn_blocking(move || {
        let free = fs4::available_space(&path).ok()?;
        Some(DiskFree { free, volume: volume_label(std::path::Path::new(&path)) })
    })
    .await
    .ok()
    .flatten()
}

/// The drive or share a Windows path is on (`C:\`), or "" without one.
fn volume_label(path: &std::path::Path) -> String {
    match path.components().next() {
        Some(std::path::Component::Prefix(p)) => format!("{}{}", p.as_os_str().to_string_lossy(), std::path::MAIN_SEPARATOR),
        _ => String::new(),
    }
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

/// Turns the command line into what to open. Plain paths pass through;
/// `--compare-to <path>` pairs `path` with the remembered left side (or opens
/// it alone when nothing was selected) and consumes the selection.
/// bc5:reports-script — parsed by cli.rs, so BC switches (`/fv=`, `/title1=`,
/// `/readonly`, `/mergeoutput=`…) come along and are never taken for paths.
fn resolve_launch_args(args: &[String], left_selection: Option<String>) -> cli::LaunchOptions {
    cli::resolve(cli::gui_options(args), left_selection)
}

/// What this process was launched with, resolved once in `main` before any
/// window exists (so the webview never sees a half-consumed selection).
static LAUNCH_OPTIONS: std::sync::OnceLock<cli::LaunchOptions> = std::sync::OnceLock::new();

fn resolve_launch_paths(args: &[String]) -> cli::LaunchOptions {
    if !args.iter().any(|a| a == "--compare-to") {
        return resolve_launch_args(args, None);
    }
    let launch = resolve_launch_args(args, read_left_selection());
    let _ = std::fs::remove_file(left_selection_file());
    pcompare_shell::menu::set_left(None);
    launch
}

/// The command line, e.g. `pcompare.exe left right` plus any viewer, title,
/// read-only and merge-output switches.
#[tauri::command]
fn launch_args() -> cli::LaunchOptions {
    LAUNCH_OPTIONS.get().cloned().unwrap_or_default()
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
    pcompare_shell::menu::set_left(path.as_deref());
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

// Folder Compare commands live in folder.rs. // bc5:folder-engine

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

// bc5:hex — hex_summary / hex_chunk / hex_next_difference moved to hex.rs

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

/// Folders in `path` for the Root folder picker, using unsaved form values.
#[tauri::command]
async fn browse_profile(app: AppHandle, input: ProfileInput, path: String) -> CmdResult<Vec<RemoteEntry>> {
    blocking(move || {
        state(&app).remote.browse(&input, &path).map(|entries| {
            entries
                .into_iter()
                .map(|e| RemoteEntry {
                    name: e.name,
                    is_dir: true,
                    size: 0,
                    modified_ms: e.meta.modified_ms,
                })
                .collect()
        })
    })
    .await
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

/// Stops a sign-in that is waiting for the browser; `sign_in_profile` then
/// fails with "sign-in cancelled".
#[tauri::command]
fn cancel_sign_in(app: AppHandle) {
    state(&app).remote.cancel_sign_in();
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
    // Explorer menu verbs ("Select Left Side", multi-select "Compare with
    // PCompare") and installer hooks run without a window.
    let args: Vec<String> = std::env::args().collect();
    if let Some(code) = explorer::run_early(args.get(1..).unwrap_or_default()) {
        std::process::exit(code);
    }
    // bc5:reports-script — `@script`, `/qc` and `--help` run without a window.
    if let Some(code) = cli::run_headless(args.get(1..).unwrap_or_default()) {
        std::process::exit(code);
    }
    // araxis:vcs — `--wait` (git difftool / mergetool) blocks here until the tab closes.
    if let Some(code) = vcs::run_wait_mode(args.get(1..).unwrap_or_default()) {
        std::process::exit(code);
    }
    // araxis:vcs end
    let _ = LAUNCH_OPTIONS.set(resolve_launch_paths(args.get(1..).unwrap_or_default()));
    // araxis:vcs
    if let Some(token) = LAUNCH_OPTIONS.get().and_then(|o| o.wait_token.as_deref()) {
        vcs::register(token);
    }
    // araxis:vcs end

    tauri::Builder::default()
        // bc5:app-shell — must be the first plugin: a second launch hands its
        // paths to this window and exits before creating its own.
        .plugin(tauri_plugin_single_instance::init(|app, argv, cwd| {
            workspace::on_second_instance(app, argv, cwd)
        }))
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        // bc5:text-ui — clipboard text as a Text Compare side.
        .plugin(tauri_plugin_clipboard_manager::init())
        // automation:shell — desktop notifications and start at login (hidden, in the tray).
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec![automation::BACKGROUND_ARG]),
        ))
        .on_window_event(automation::on_window_event)
        // automation:shell end
        .setup(|app| {
            let config_dir = app.path().app_config_dir()?;
            let data_dir = app.path().app_data_dir()?;
            let cache_dir = app.path().app_cache_dir()?.join("remote");
            let remote = RemoteManager::new(&config_dir, cache_dir, Box::new(KeyringSecrets));
            remote.clear_cache();
            app.manage(AppState {
                sessions: SessionStore::new(config_dir.join("sessions.json")),
                remote,
                watchers: watch::Watchers::default(),
                db: pcompare_db::connection::ConnectionStore::new(
                    config_dir.join("db-connections.json"),
                    Box::new(KeyringSecrets),
                ),
                db_cancel: AtomicBool::new(false),
                index: open_index(&data_dir),
            });
            app.manage(workspace::ShellState::new(&config_dir)); // bc5:app-shell
            close_guard::manage(app.handle());
            #[cfg(target_os = "macos")]
            if let Err(e) = close_guard::install_app_menu(app.handle()) {
                eprintln!("app menu: {e}");
            }
            remote::install_prompter(app.handle()); // bc5:remote-write
            explorer::ensure_on_startup();
            // automation:shell — after AppState: the scheduler uses its scan database.
            // Also shows the main window (created hidden for `--background`).
            automation::setup(app)?;
            // automation:shell end
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            path_kind,
            disk_free,
            launch_args,
            get_left_selection,
            set_left_selection,
            explorer::shell_menu_status,
            explorer::shell_menu_set,
            import::import_discover,
            import::import_read,
            import::import_commit,
            compare_text_files,
            folder::compare_folders, // bc5:folder-engine (moved to folder.rs)
            folder::compare_file_contents,
            // bc5:archives — archive / snapshot / svn: sides, snapshots (archive.rs)
            archive::archive_side_info,
            archive::snapshot_save,
            // araxis:text-review — comparison archives (review.rs)
            review::review_save,
            review::review_open,
            review::review_save_bookmarks,
            review::review_read_file,
            // araxis:text-review end
            folder::compare_folders_cancel,
            folder::restage_folders,
            folder::forget_folder_scan,
            folder::compare_folder_entries,
            watch_paths,
            unwatch_paths,
            // bc5:hex — hex compare (hex.rs)
            hex::hex_summary,
            hex::hex_chunk,
            hex::hex_next_difference,
            hex::hex_open,
            hex::hex_close,
            hex::hex_state,
            hex::hex_rows,
            hex::hex_edit,
            hex::hex_undo,
            hex::hex_redo,
            hex::hex_realign,
            hex::hex_navigate,
            hex::hex_locate,
            hex::hex_find,
            hex::hex_read,
            hex::hex_replace_all,
            hex::hex_save,
            hex::hex_disk_changed,
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
            browse_profile,
            sign_in_profile,
            cancel_sign_in,
            open_provider_console,
            remote_list,
            // bc5:remote-write — remote browser writes + SSH sign-in prompts
            remote::remote_mkdir,
            remote::remote_delete,
            remote::remote_rename,
            remote::remote_can_rename,
            remote::remote_auth_answer,
            // --- text compare editing + text merge (merge.rs) ---
            merge::load_text_pair,
            merge::diff_texts,
            merge::save_text_file,
            merge::load_merge,
            // --- table compare (table.rs) ---
            table::compare_tables,
            table::compare_table_sheets, // bc5:table
            // --- picture compare (picture.rs) ---
            picture::picture_load,
            picture::picture_diff,
            picture::picture_probe, // bc5:picture-registry
            // --- folder sync + folder merge (sync.rs) ---
            sync::sync_plan,
            sync::sync_execute,
            sync::sync_cancel,
            sync::sync_pause, // bc5:file-ops
            sync::sync_preflight,
            sync::sync_interrupted,
            sync::sync_resume,
            sync::sync_discard,
            sync::sync_rollback,
            sync::sync_jobs,
            sync::sync_job_detail,
            sync::sync_forget_history,
            sync::sync_default_versioning_folder,
            sync::index_stats,
            sync::index_clear_checksums,
            sync::folder_merge_compare,
            sync::folder_merge_execute,
            // --- registry / version / mp3 (meta.rs) ---
            meta::compare_registry,
            meta::registry_save, // bc5:picture-registry
            meta::registry_live_plan, // bc5:picture-registry
            meta::registry_apply_live, // bc5:picture-registry
            meta::compare_versions,
            meta::compare_audio,
            // --- database compare (db.rs) ---
            db::db_list_connections,
            db::db_save_connection,
            db::db_delete_connection,
            db::db_test_connection,
            db::db_list_databases,
            db::db_is_sqlite,
            db::db_compare_schema,
            db::db_schema_script,
            db::db_compare_data,
            db::db_table_data,
            db::db_data_script,
            db::db_cancel,
            db::db_execute_script,
            // --- bc5:app-shell — prefs, workspaces, recent sessions (workspace.rs) ---
            workspace::prefs_load,
            workspace::prefs_save,
            workspace::workspace_list,
            workspace::workspace_save,
            workspace::workspace_rename,
            workspace::workspace_delete,
            workspace::workspace_last,
            workspace::workspace_set_last,
            workspace::recent_list,
            workspace::recent_add,
            workspace::recent_remove,
            workspace::recent_clear,
            workspace::session_autosave,
            // --- bc5:text-engine — Text Compare encodings, grammars, backups (text_compare.rs) ---
            text_compare::text_load_pair,
            text_compare::text_load,
            text_compare::text_diff,
            text_compare::text_save,
            text_compare::text_encodings,
            text_compare::text_grammars,
            // --- bc5:editor-patch — Patch Viewer (patch.rs) ---
            patch::patch_load,
            patch::patch_check,
            patch::patch_apply_file,
            // araxis:merge
            patch::patch_write_selected,
            // araxis:merge end
            // --- bc5:converters — file format rules, external tools (convert.rs) ---
            convert::convert_config_load,
            convert::convert_config_save,
            convert::convert_config_defaults,
            convert::convert_tools_for,
            convert::convert_launch_tool,
            convert::convert_source_text,
            // --- bc5:file-ops — Folder Compare file operations (fileops.rs) ---
            fileops::fileop_start,
            fileops::fileop_pause,
            fileops::fileop_cancel,
            fileops::fileop_answer,
            // --- bc5:reports-script — text / folder / table reports (report.rs) ---
            report::report_text,
            report::report_folder,
            report::report_folder_save,
            report::report_table,
            report::report_write,
            report::report_open_in_browser,
            // araxis:vcs — wait mode, git setup, tool snippets, revision picker (vcs.rs)
            vcs::vcs_wait_done,
            vcs::vcs_git_setup_preview,
            vcs::vcs_git_setup_apply,
            vcs::vcs_snippets,
            vcs::vcs_detect,
            vcs::vcs_revisions,
            vcs::vcs_blame,
            // araxis:vcs end
            // automation:shell — sync automation + notification channels (automation.rs)
            automation::automation_tasks,
            automation::automation_new_task,
            automation::automation_validate,
            automation::automation_save,
            automation::automation_delete,
            automation::automation_set_enabled,
            automation::automation_duplicate,
            automation::automation_run_now,
            automation::automation_cancel,
            automation::automation_statuses,
            automation::automation_runs,
            automation::automation_upcoming,
            automation::automation_describe_trigger,
            automation::automation_settings,
            automation::automation_save_settings,
            automation::automation_export,
            automation::automation_import,
            automation::notify_channels,
            automation::notify_save_channel,
            automation::notify_delete_channel,
            automation::notify_test_channel,
            // automation:shell end
            close_guard::close_guard_set_unsaved,
            close_guard::close_guard_proceed,
        ])
        // automation:shell — with an event handler, so quitting stops running syncs at a safe point.
        .build(tauri::generate_context!())
        .expect("error while running PCompare")
        .run(|app, event| {
            close_guard::on_run_event(app, &event);
            automation::on_run_event(app, event);
        });
        // automation:shell end
}

#[cfg(test)]
mod launch_tests {
    use super::resolve_launch_args;

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn plain_paths_pass_through() {
        assert_eq!(resolve_launch_args(&args(&["a", "-v", "b"]), None).paths, args(&["a", "b"]));
    }

    #[test]
    fn compare_to_pairs_with_the_selected_left() {
        let got = resolve_launch_args(&args(&["--compare-to", "b"]), Some("a".into())).paths;
        assert_eq!(got, args(&["a", "b"]));
    }

    #[test]
    fn compare_to_without_selection_opens_one_side() {
        assert_eq!(resolve_launch_args(&args(&["--compare-to", "b"]), None).paths, args(&["b"]));
        assert_eq!(resolve_launch_args(&args(&["--compare-to", "b"]), Some("b".into())).paths, args(&["b"]));
        assert!(resolve_launch_args(&args(&["--compare-to"]), None).paths.is_empty());
    }
}
