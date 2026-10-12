//! App-shell commands: preferences, workspaces, recent (auto-saved) sessions,
//! auto-saving linked sessions on close, and routing a second launch into the
//! running window (single instance).

use crate::{blocking, state, CmdResult};
use pcompare_core::session::{SavedSession, SessionInput};
use pcompare_core::workspace::{
    PrefsStore, RecentInput, RecentSession, RecentStore, Workspace, WorkspaceInput, WorkspaceStore, WorkspaceTab,
};
use std::path::Path;
use tauri::{AppHandle, Emitter, Manager};

/// Event carrying the paths of a second `pcompare <left> <right>` launch.
pub const OPEN_PATHS_EVENT: &str = "open-launch-paths";

pub struct ShellState {
    workspaces: WorkspaceStore,
    recent: RecentStore,
    prefs: PrefsStore,
}

impl ShellState {
    pub fn new(config_dir: &Path) -> Self {
        Self {
            workspaces: WorkspaceStore::new(config_dir.join("workspaces.json")),
            recent: RecentStore::new(config_dir.join("recent-sessions.json")),
            prefs: PrefsStore::new(config_dir.join("prefs.json")),
        }
    }
}

fn shell(app: &AppHandle) -> tauri::State<'_, ShellState> {
    app.state::<ShellState>()
}

// ---------- preferences ----------

#[tauri::command]
pub async fn prefs_load(app: AppHandle) -> CmdResult<serde_json::Value> {
    blocking(move || shell(&app).prefs.load()).await
}

#[tauri::command]
pub async fn prefs_save(app: AppHandle, prefs: serde_json::Value) -> CmdResult<()> {
    blocking(move || shell(&app).prefs.save(prefs)).await
}

// ---------- workspaces ----------

#[tauri::command]
pub async fn workspace_list(app: AppHandle) -> CmdResult<Vec<Workspace>> {
    blocking(move || shell(&app).workspaces.list()).await
}

#[tauri::command]
pub async fn workspace_save(app: AppHandle, input: WorkspaceInput) -> CmdResult<Workspace> {
    blocking(move || shell(&app).workspaces.save(input)).await
}

#[tauri::command]
pub async fn workspace_rename(app: AppHandle, id: String, name: String) -> CmdResult<Workspace> {
    blocking(move || shell(&app).workspaces.rename(&id, &name)).await
}

#[tauri::command]
pub async fn workspace_delete(app: AppHandle, id: String) -> CmdResult<()> {
    blocking(move || shell(&app).workspaces.delete(&id)).await
}

/// The tabs that were open when the app last ran.
#[tauri::command]
pub async fn workspace_last(app: AppHandle) -> CmdResult<Option<Workspace>> {
    blocking(move || shell(&app).workspaces.last()).await
}

#[tauri::command]
pub async fn workspace_set_last(app: AppHandle, tabs: Vec<WorkspaceTab>, active: usize) -> CmdResult<()> {
    blocking(move || shell(&app).workspaces.set_last(tabs, active).map(|_| ())).await
}

// ---------- recent sessions ----------

#[tauri::command]
pub async fn recent_list(app: AppHandle) -> CmdResult<Vec<RecentSession>> {
    blocking(move || shell(&app).recent.list()).await
}

#[tauri::command]
pub async fn recent_add(app: AppHandle, input: RecentInput) -> CmdResult<RecentSession> {
    blocking(move || shell(&app).recent.add(input)).await
}

#[tauri::command]
pub async fn recent_remove(app: AppHandle, id: String) -> CmdResult<()> {
    blocking(move || shell(&app).recent.remove(&id)).await
}

#[tauri::command]
pub async fn recent_clear(app: AppHandle) -> CmdResult<()> {
    blocking(move || shell(&app).recent.clear()).await
}

/// Writes a closing tab back into its saved session; `None` if it was deleted.
#[tauri::command]
pub async fn session_autosave(app: AppHandle, id: String, input: SessionInput) -> CmdResult<Option<SavedSession>> {
    blocking(move || state(&app).sessions.autosave(&id, input)).await
}

// ---------- single instance ----------

/// Anchors relative command-line paths at the second instance's working
/// directory; the running process has its own, unrelated one.
fn absolutize(paths: Vec<String>, cwd: &str) -> Vec<String> {
    paths
        .into_iter()
        .map(|p| {
            // araxis:vcs — `git:repo@HEAD~1` anchors the working-tree path inside.
            if pcompare_archive::vcs::is_vcs(&p) {
                let anchored = (!cwd.is_empty())
                    .then(|| pcompare_archive::vcs::with_base_dir(&p, Path::new(cwd)))
                    .flatten();
                return anchored.unwrap_or(p);
            }
            // araxis:vcs end
            // `remote://…`, `db://…` and other URLs are never file-relative.
            if p.contains("://") || Path::new(&p).is_absolute() || cwd.is_empty() {
                p
            } else {
                Path::new(cwd).join(&p).to_string_lossy().into_owned()
            }
        })
        .collect()
}

/// Called in the running instance when PCompare is launched again: brings the
/// window forward and hands the new launch's paths to the frontend, which
/// opens them in a new tab exactly like startup arguments.
pub fn on_second_instance(app: &AppHandle, argv: Vec<String>, cwd: String) {
    // bc5:reports-script — the full launch options (viewer, titles, read-only), not just paths.
    let mut launch = crate::resolve_launch_paths(argv.get(1..).unwrap_or_default());
    launch.paths = absolutize(launch.paths, &cwd);
    launch.merge_output = launch.merge_output.and_then(|o| absolutize(vec![o], &cwd).pop());
    if let Some(window) = app.webview_windows().values().next() {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
    }
    // araxis:vcs — this window answers for a `--wait` launch.
    if let Some(token) = launch.wait_token.as_deref() {
        crate::vcs::register(token);
    }
    // araxis:vcs end
    if !launch.paths.is_empty() {
        let _ = app.emit(OPEN_PATHS_EVENT, launch); // bc5:reports-script
    }
}

#[cfg(test)]
mod tests {
    use super::absolutize;

    #[test]
    fn relative_paths_are_anchored_at_the_launch_directory() {
        let cwd = std::env::temp_dir();
        let abs = cwd.join("x").to_string_lossy().into_owned();
        let got = absolutize(
            vec![
                "a.txt".into(),
                abs.clone(),
                "remote://s3/bucket/k".into(),
                "db://conn/db".into(),
            ],
            &cwd.to_string_lossy(),
        );
        assert_eq!(got[0], cwd.join("a.txt").to_string_lossy());
        assert_eq!(got[1], abs);
        assert_eq!(got[2], "remote://s3/bucket/k");
        assert_eq!(got[3], "db://conn/db");
        assert_eq!(absolutize(vec!["a".into()], ""), vec!["a".to_string()]);
    }
}
