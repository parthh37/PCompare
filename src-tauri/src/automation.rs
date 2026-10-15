//! Sync automation in the app: saved folder-sync tasks that run on
//! schedules, on file-system changes, at startup or on demand (the
//! `pcompare-automation` scheduler), failure notifications
//! (`pcompare-notify`), and what keeps them running unattended — the tray
//! icon, close-to-tray, quitting safely and starting at login.
//!
//! The scheduler does no sync work itself. [`TauriRunner`] runs a task
//! through the same pipeline as the Folder Sync view (sync.rs): plan with
//! the checksum cache and the two-way baseline, preflight, then a journaled
//! job — so an automated run shows up in Sync History and can be resumed or
//! rolled back like any other. Before executing it applies the task's
//! safety checks, which an unattended run needs more than an interactive
//! one: nobody is there to read the plan's warnings.
//!
//! Threading: the scheduler calls [`TauriHost`] from its own threads and
//! may hold its locks while doing so, so nothing here that runs on the UI
//! thread (window close, tray menu, exit) waits for the scheduler, and
//! nothing the host does waits for the UI thread. Tray updates are posted
//! to the UI thread without waiting; scheduler calls from the tray menu run
//! on a spawned thread.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use chrono::Local;
use pcompare_automation::scheduler::{
    RunContext, RunOutcome, RunPhase, RunProgress, Scheduler, SchedulerHost, TaskRunState,
    TaskRunner, TaskStatus,
};
use pcompare_automation::store::AutomationFile;
use pcompare_automation::task::validate_task;
use pcompare_automation::trigger;
use pcompare_automation::{
    AutomationSettings, NotifyRules, RunRetry, SafetyOptions, SyncTask, TaskIssue, TaskStore,
    Trigger,
};
use pcompare_core::folder::{FolderCompareOptions, FolderEntry};
use pcompare_core::CoreError;
use pcompare_index::{AutomationRun, Index, JobStatus, RunStatus, RunTrigger};
use pcompare_notify::{
    ChannelConfig, ChannelSecrets, ChannelSettings, ChannelStore, FailureLine, Notification,
    SecretsPresent,
};
use pcompare_remote::profile::KeyringSecrets;
use pcompare_sync::exec::{ExecOptions, ExecProgress};
use pcompare_sync::sync::{
    ConflictPolicy, Direction, OpKind, SyncMatrix, SyncMode, SyncPlan, SyncSummary, WarningKind,
};
use serde::{Deserialize, Serialize};
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, RunEvent, WindowEvent, Wry};
use tauri_plugin_autostart::ManagerExt as _;
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};
use tauri_plugin_notification::NotificationExt;

use crate::close_guard::CloseAction;
use crate::sync::{self as shell_sync, RunSpec};
use crate::{blocking, CmdResult};

/// Emitted with a [`TaskStatus`] whenever a task's live status changes.
pub const STATUS_EVENT: &str = "automation-status";
/// Emitted with `{ taskId, runId }` when a run has been recorded.
pub const RUN_EVENT: &str = "automation-run";
/// Emitted (no payload) when tasks, channels or settings change.
pub const CHANGED_EVENT: &str = "automation-changed";
/// Emitted (no payload) by the tray menu's "Sync Automation…".
pub const OPEN_EVENT: &str = "automation-open";

/// The command-line switch start-at-login launches with: start hidden in
/// the tray when there is something to run.
pub const BACKGROUND_ARG: &str = "--background";

const MAIN_WINDOW: &str = "main";
const TRAY_ID: &str = "pcompare-tray";
/// How long quitting waits for running syncs to reach a safe point.
const SHUTDOWN_WAIT: Duration = Duration::from_secs(10);
/// Minimum spacing of progress reports passed to the scheduler.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);
/// Failures listed on a run (and so in its notification).
const MAX_FAILURES: usize = 20;
/// The percentage guard ignores deleting fewer files than this: removing
/// 2 of a folder's 3 files is 67 %, but not the catastrophe the guard is for.
const PERCENT_GUARD_MIN_FILES: usize = 5;
/// Windows truncates tray tooltips at 127 characters.
const TOOLTIP_MAX_CHARS: usize = 120;

pub struct AutomationState {
    store: Arc<TaskStore>,
    channels: Arc<ChannelStore>,
    scheduler: Arc<Scheduler>,
    shell: Arc<Shell>,
}

fn automation(app: &AppHandle) -> tauri::State<'_, AutomationState> {
    app.state::<AutomationState>()
}

fn err(e: impl std::fmt::Display) -> CoreError {
    CoreError::Other(e.to_string())
}

// ---------- shell state: tray, close-to-tray, quitting ----------

/// What the tray and the window-close handler show and decide, kept up to
/// date from the task file and the scheduler's status events so the UI
/// thread never has to ask either.
#[derive(Default)]
struct Shell {
    model: Mutex<TrayModel>,
    /// Closing the main window hides it instead (`run_in_background` and at
    /// least one enabled task).
    keep_running: AtomicBool,
    /// The "still running in the tray" notice was shown this session.
    told_about_tray: AtomicBool,
    shut_down: AtomicBool,
    pause_item: Mutex<Option<CheckMenuItem<Wry>>>,
}

#[derive(Default)]
struct TrayModel {
    /// Id, name and enabled flag of every saved task.
    tasks: Vec<(String, String, bool)>,
    statuses: HashMap<String, TaskStatus>,
    paused: bool,
    /// What the tray shows now, to skip redundant updates.
    shown: Option<(String, bool, bool)>,
}

impl Shell {
    fn model(&self) -> MutexGuard<'_, TrayModel> {
        self.model.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl TrayModel {
    fn tooltip(&self) -> String {
        let enabled = self.tasks.iter().filter(|t| t.2).count();
        let running = self
            .statuses
            .values()
            .filter(|s| s.state == TaskRunState::Running)
            .count();
        let mut tip = if self.paused {
            "PCompare — automations paused".to_owned()
        } else {
            format!(
                "PCompare — {enabled} automation{}",
                if enabled == 1 { "" } else { "s" }
            )
        };
        if running > 0 {
            tip.push_str(&format!(" ({running} running)"));
        }
        let failed: Vec<&str> = self
            .tasks
            .iter()
            .filter(|(id, ..)| {
                self.statuses
                    .get(id)
                    .and_then(|s| s.last_status)
                    .is_some_and(RunStatus::is_failure)
            })
            .map(|(_, name, _)| name.as_str())
            .collect();
        if !failed.is_empty() {
            tip.push_str(&format!("\nLast run failed: {}", failed.join(", ")));
        }
        if tip.chars().count() > TOOLTIP_MAX_CHARS {
            tip = tip.chars().take(TOOLTIP_MAX_CHARS - 1).collect::<String>() + "…";
        }
        tip
    }
}

/// Pushes the model to the tray if what it shows changed. Never waits for
/// the UI thread.
fn update_tray(app: &AppHandle, shell: &Arc<Shell>) {
    let (tip, visible, paused) = {
        let mut m = shell.model();
        let next = (m.tooltip(), !m.tasks.is_empty(), m.paused);
        if m.shown.as_ref() == Some(&next) {
            return;
        }
        m.shown = Some(next.clone());
        next
    };
    let (handle, shell) = (app.clone(), shell.clone());
    let _ = app.run_on_main_thread(move || {
        if let Some(tray) = handle.tray_by_id(TRAY_ID) {
            let _ = tray.set_tooltip(Some(&tip));
            let _ = tray.set_visible(visible);
        }
        if let Some(item) = shell
            .pause_item
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            let _ = item.set_checked(paused);
        }
    });
}

/// Re-reads tasks, settings and statuses after a change (or at startup).
fn refresh_shell(app: &AppHandle, store: &TaskStore, scheduler: &Scheduler, shell: &Arc<Shell>) {
    let file = match store.load() {
        Ok(f) => f,
        Err(e) => {
            eprintln!("automation: {e}");
            AutomationFile::default()
        }
    };
    let statuses = scheduler.statuses();
    let any_enabled = file.tasks.iter().any(|t| t.enabled);
    shell.keep_running.store(
        file.settings.run_in_background && any_enabled,
        Ordering::Relaxed,
    );
    {
        let mut m = shell.model();
        m.tasks = file
            .tasks
            .iter()
            .map(|t| (t.id.clone(), t.name.clone(), t.enabled))
            .collect();
        m.statuses = statuses
            .into_iter()
            .map(|s| (s.task_id.clone(), s))
            .collect();
        m.paused = file.settings.paused;
    }
    update_tray(app, shell);
}

/// After tasks or settings changed: the scheduler re-reads them, the tray
/// follows, and the UI hears about it.
fn after_change(app: &AppHandle) {
    let st = automation(app);
    st.scheduler.reload();
    refresh_shell(app, &st.store, &st.scheduler, &st.shell);
    let _ = app.emit(CHANGED_EVENT, ());
}

pub fn show_main_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window(MAIN_WINDOW) {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
    }
}

fn build_tray(app: &AppHandle, shell: &Shell, paused: bool) -> tauri::Result<()> {
    let open = MenuItem::with_id(app, "open", "Open PCompare", true, None::<&str>)?;
    let automations = MenuItem::with_id(app, "automation", "Sync Automation…", true, None::<&str>)?;
    let pause = CheckMenuItem::with_id(
        app,
        "pause",
        "Pause automations",
        true,
        paused,
        None::<&str>,
    )?;
    let quit = MenuItem::with_id(app, "quit", "Quit PCompare", true, None::<&str>)?;
    let menu = Menu::with_items(
        app,
        &[
            &open,
            &automations,
            &PredefinedMenuItem::separator(app)?,
            &pause,
            &PredefinedMenuItem::separator(app)?,
            &quit,
        ],
    )?;
    *shell.pause_item.lock().unwrap_or_else(|e| e.into_inner()) = Some(pause);
    let mut tray = TrayIconBuilder::with_id(TRAY_ID)
        .tooltip("PCompare")
        .menu(&menu)
        // Windows: left click opens the window, right click the menu. On
        // macOS a menu-bar item opens its menu on any click.
        .show_menu_on_left_click(cfg!(target_os = "macos"))
        .on_menu_event(|app, event| match event.id().as_ref() {
            "open" => show_main_window(app),
            "automation" => {
                show_main_window(app);
                let _ = app.emit(OPEN_EVENT, ());
            }
            "pause" => {
                let app = app.clone();
                std::thread::spawn(move || toggle_pause(&app));
            }
            "quit" => quit_from_tray(app.clone()),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if cfg!(target_os = "macos") {
                return;
            }
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                show_main_window(tray.app_handle());
            }
        });
    if let Some(icon) = app.default_window_icon() {
        tray = tray.icon(icon.clone());
    }
    tray.build(app)?;
    Ok(())
}

fn toggle_pause(app: &AppHandle) {
    let st = automation(app);
    let saved = st.store.settings().and_then(|mut s| {
        s.paused = !s.paused;
        st.store.save_settings(&s)
    });
    if let Err(e) = saved {
        eprintln!("automation: {e}");
    }
    // Also puts the check mark back if saving failed (the menu toggles it on click).
    st.shell.model().shown = None;
    after_change(app);
}

/// Quits from the tray menu: asks about unsaved edits, then about a running sync.
fn quit_from_tray(app: AppHandle) {
    crate::close_guard::request_quit(&app);
}

/// Quits, asking first while a sync is running.
pub fn quit(app: AppHandle) {
    // The question blocks, and the scheduler must not be asked from the UI thread.
    std::thread::spawn(move || {
        let busy = app
            .try_state::<AutomationState>()
            .is_some_and(|st| st.scheduler.is_busy());
        if busy {
            let quit = app
                .dialog()
                .message(
                    "A sync is running. Quit anyway?\n\nIt stops at a safe point, and the next run picks up \
                     where it left off.",
                )
                .title("Quit PCompare")
                .kind(MessageDialogKind::Warning)
                .buttons(MessageDialogButtons::OkCancelCustom("Quit".into(), "Keep running".into()))
                .blocking_show();
            if !quit {
                crate::close_guard::rearm(&app);
                return;
            }
        }
        shutdown(&app);
        app.exit(0);
    });
}

/// Stops the scheduler once: cancels running syncs and waits briefly for
/// them to reach a safe point.
fn shutdown(app: &AppHandle) {
    if let Some(st) = app.try_state::<AutomationState>() {
        if !st.shell.shut_down.swap(true, Ordering::SeqCst) {
            st.scheduler.shutdown(SHUTDOWN_WAIT);
        }
    }
}

/// Close-to-tray: with background running on and something to run, closing
/// the main window hides it instead. Otherwise unsaved edits are asked about.
pub fn on_window_event(window: &tauri::Window, event: &WindowEvent) {
    let WindowEvent::CloseRequested { api, .. } = event else {
        return;
    };
    if window.label() != MAIN_WINDOW {
        return;
    }
    let app = window.app_handle();
    let st = app.try_state::<AutomationState>();
    let hide = st.as_ref().is_some_and(|st| {
        st.shell.keep_running.load(Ordering::Relaxed) && !st.shell.shut_down.load(Ordering::Relaxed)
    });
    match crate::close_guard::close_action(hide, crate::close_guard::blocks(app)) {
        CloseAction::Close => return,
        CloseAction::Ask => {
            api.prevent_close();
            crate::close_guard::ask(app, false);
            return;
        }
        CloseAction::HideToTray => {}
    }
    let Some(st) = st else {
        return;
    };
    api.prevent_close();
    let _ = window.hide();
    if !st.shell.told_about_tray.swap(true, Ordering::Relaxed) {
        let place = if cfg!(target_os = "macos") {
            "menu bar"
        } else {
            "tray"
        };
        let _ = app
            .notification()
            .builder()
            .title("PCompare is still running")
            .body(format!(
                "Automations keep running in the {place}. To stop them, choose Quit PCompare from the {place} icon's menu."
            ))
            .show();
    }
}

pub fn on_run_event(app: &AppHandle, event: RunEvent) {
    match event {
        RunEvent::Exit => shutdown(app),
        // The Dock icon was clicked while the window was hidden.
        #[cfg(target_os = "macos")]
        RunEvent::Reopen {
            has_visible_windows: false,
            ..
        } => show_main_window(app),
        _ => {}
    }
}

/// Starts automation: stores, scheduler, tray, start-at-login. Shows the
/// main window (created hidden) unless this is a `--background` launch with
/// enabled tasks. Must run after `AppState` is managed (the scheduler uses
/// its scan database).
pub fn setup(app: &tauri::App) -> tauri::Result<()> {
    let handle = app.handle().clone();
    let result = start(&handle);
    let background = std::env::args().any(|a| a == BACKGROUND_ARG);
    let any_enabled = handle
        .try_state::<AutomationState>()
        .is_some_and(|st| st.shell.model().tasks.iter().any(|t| t.2));
    if !(background && any_enabled) {
        show_main_window(&handle);
    }
    result
}

fn start(app: &AppHandle) -> tauri::Result<()> {
    let config_dir = app.path().app_config_dir()?;
    let store = Arc::new(TaskStore::new(config_dir.join("automation.json")));
    let channels = Arc::new(ChannelStore::new(
        config_dir.join("notify-channels.json"),
        Box::new(KeyringSecrets),
    ));
    let settings = store.settings().unwrap_or_else(|e| {
        eprintln!("automation: {e}");
        AutomationSettings::default()
    });
    let shell = Arc::new(Shell::default());
    if let Err(e) = build_tray(app, &shell, settings.paused) {
        eprintln!("tray icon: {e}");
    }
    let host = Arc::new(TauriHost {
        app: app.clone(),
        channels: channels.clone(),
        shell: shell.clone(),
    });
    let runner = Arc::new(TauriRunner { app: app.clone() });
    let scheduler = Scheduler::start(store.clone(), runner, host);
    refresh_shell(app, &store, &scheduler, &shell);
    app.manage(AutomationState {
        store,
        channels,
        scheduler,
        shell,
    });
    sync_autostart(app, settings.start_at_login);
    Ok(())
}

/// Makes the OS login item match the setting (it may have been removed by
/// hand, or the settings file copied from another machine).
fn sync_autostart(app: &AppHandle, wanted: bool) {
    let autostart = app.autolaunch();
    let Ok(enabled) = autostart.is_enabled() else {
        return;
    };
    if enabled == wanted {
        return;
    }
    let result = if wanted {
        autostart.enable()
    } else {
        autostart.disable()
    };
    if let Err(e) = result {
        eprintln!("start at login: {e}");
    }
}

// ---------- the scheduler's host ----------

struct TauriHost {
    app: AppHandle,
    channels: Arc<ChannelStore>,
    shell: Arc<Shell>,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct RunEventPayload<'a> {
    task_id: &'a str,
    run_id: &'a str,
}

impl SchedulerHost for TauriHost {
    fn index(&self) -> Option<&Index> {
        shell_sync::index(&self.app)
    }

    fn channels(&self) -> &ChannelStore {
        &self.channels
    }

    fn status_changed(&self, status: &TaskStatus) {
        self.shell
            .model()
            .statuses
            .insert(status.task_id.clone(), status.clone());
        let _ = self.app.emit(STATUS_EVENT, status);
        update_tray(&self.app, &self.shell);
    }

    fn run_finished(&self, task_id: &str, run_id: &str) {
        let _ = self
            .app
            .emit(RUN_EVENT, RunEventPayload { task_id, run_id });
    }

    fn desktop_notify(&self, n: &Notification) {
        if let Err(e) = self
            .app
            .notification()
            .builder()
            .title(&n.title)
            .body(&n.summary)
            .show()
        {
            eprintln!("desktop notification: {e}");
        }
    }

    fn host_name(&self) -> String {
        host_name()
    }

    fn app_version(&self) -> String {
        self.app.package_info().version.to_string()
    }
}

/// This machine's name, for notification footers.
fn host_name() -> String {
    gethostname::gethostname().to_string_lossy().into_owned()
}

// ---------- the runner: one automated sync ----------

/// Runs a task through the Folder Sync pipeline (sync.rs).
struct TauriRunner {
    app: AppHandle,
}

impl TaskRunner for TauriRunner {
    /// The scheduler enforces `safety.timeout_minutes` itself (it sets
    /// `ctx.cancel` and reports the run as timed out), so a run here only
    /// has to stop when the flag is set.
    fn run(&self, task: &SyncTask, ctx: &RunContext) -> RunOutcome {
        self.run_task(task, ctx)
    }
}

impl TauriRunner {
    fn run_task(&self, task: &SyncTask, ctx: &RunContext) -> RunOutcome {
        let app = &self.app;
        let (left, right) = (task.left.as_str(), task.right.as_str());
        let progress = |phase, done_ops, total_ops| {
            (ctx.progress)(RunProgress {
                phase,
                done_ops,
                total_ops,
                bytes_done: 0,
                bytes_total: 0,
                current: None,
            })
        };
        progress(RunPhase::Planning, 0, 0);

        let mut exec = task.exec.clone();
        if let Err(e) = shell_sync::resolve_versioning_folder(app, &mut exec) {
            return RunOutcome::failed(
                format!("The default versioning folder can't be determined: {e}"),
                false,
            );
        }
        // An unplugged drive or a share that's down: worth retrying later.
        let reachable = shell_sync::with_roots(app, left, right, |l, r| {
            shell_sync::require_existing_root("left", l)?;
            shell_sync::require_existing_root("right", r)
        });
        if let Err(e) = reachable {
            return RunOutcome::failed(e.to_string(), is_transient(&e));
        }
        repair_interrupted_job(app, left, right);
        if ctx.cancel.load(Ordering::SeqCst) {
            return cancelled_before_start();
        }

        let rules = task.plan_rules();
        let plan =
            match shell_sync::plan(app, left, right, task.mode, &task.compare, task.conflicts, &rules) {
                Ok(p) => p,
                Err(e) => {
                    return RunOutcome::failed(
                        format!("Comparing the folders failed: {e}"),
                        is_transient(&e),
                    )
                }
            };
        if ctx.cancel.load(Ordering::SeqCst) {
            return cancelled_before_start();
        }
        if let Some(why) = safety_violation(&task.safety, &plan) {
            return planned(RunStatus::Aborted, &plan, Some(why));
        }
        let conflicts = plan.summary.conflicts;
        if plan.ops.is_empty() {
            if task.mode.uses_baseline() {
                // Without a baseline the next two-way run can't tell a
                // deletion from a new file, so record the in-sync state now.
                shell_sync::update_baseline(app, left, right, &plan);
            }
            return if conflicts > 0 {
                planned(
                    RunStatus::CompletedWithErrors,
                    &plan,
                    Some(format!(
                        "Nothing else to do, but {} need{} attention: open the task's folders in Folder Sync to \
                         decide {}.",
                        plural(conflicts, "conflict"),
                        if conflicts == 1 { "s" } else { "" },
                        if conflicts == 1 { "it" } else { "them" },
                    )),
                )
            } else {
                planned(
                    RunStatus::NoChanges,
                    &plan,
                    Some("The folders were already in sync.".into()),
                )
            };
        }

        let issues = match shell_sync::preflight(app, left, right, &plan.ops, Some(&exec)) {
            Ok(issues) => issues,
            Err(e) => {
                return RunOutcome::failed(
                    format!("Checking the folders failed: {e}"),
                    is_transient(&e),
                )
            }
        };
        let blocking: Vec<&str> = issues
            .iter()
            .filter(|i| i.blocking)
            .map(|i| i.message.as_str())
            .collect();
        if !blocking.is_empty() {
            // Only reachability is worth retrying; a read-only folder or one
            // folder inside the other won't fix itself.
            let retryable = blocking.iter().any(|m| mentions_unreachable(m));
            let mut outcome = planned(RunStatus::Failed, &plan, Some(blocking.join(" ")));
            outcome.retryable = retryable;
            return outcome;
        }

        let total = plan.ops.len();
        progress(RunPhase::Executing, 0, total);
        let mut last_sent: Option<Instant> = None;
        let mut forward = |p: &ExecProgress| {
            let important = p.current.is_none() || p.done == p.total;
            if important || last_sent.is_none_or(|t| t.elapsed() >= PROGRESS_INTERVAL) {
                last_sent = Some(Instant::now());
                (ctx.progress)(RunProgress {
                    phase: RunPhase::Executing,
                    done_ops: p.done,
                    total_ops: p.total,
                    bytes_done: p.bytes_done,
                    bytes_total: p.bytes_total,
                    current: p.current.clone(),
                });
            }
        };
        // Unattended runs are never paused.
        let pause = AtomicBool::new(false);
        let spec = RunSpec {
            left,
            right,
            mode: task.mode,
            compare: &task.compare,
            conflicts: task.conflicts,
            rules: &rules,
        };
        let run = match shell_sync::execute_new_job(
            app,
            &spec,
            &plan.ops,
            &exec,
            ctx.cancel,
            &pause,
            &mut forward,
        ) {
            Ok(run) => run,
            Err(e) => {
                return RunOutcome::failed(
                    format!("The sync couldn't start: {e}"),
                    is_transient(&e),
                )
            }
        };

        let report = &run.report;
        let failures = report
            .results
            .iter()
            .filter(|r| !r.ok)
            .take(MAX_FAILURES)
            .map(|r| FailureLine {
                path: r.rel_path.clone(),
                error: r
                    .error
                    .clone()
                    .unwrap_or_else(|| "Failed for an unknown reason.".into()),
            })
            .collect();
        let done = report.succeeded;
        let (status, message, retryable) = if report.cancelled {
            let message = format!(
                "Cancelled after {done} of {total} operations. The next run picks up the rest."
            );
            (RunStatus::Cancelled, message, false)
        } else if let Some(reason) = &report.stopped {
            (
                RunStatus::Failed,
                format!("The sync stopped after {done} of {total} operations: {reason}"),
                true,
            )
        } else if report.failed > 0 {
            let mut message = format!("{} of {total} operations failed.", report.failed);
            if conflicts > 0 {
                message.push_str(&format!(
                    " {} also need attention.",
                    plural(conflicts, "conflict")
                ));
            }
            (RunStatus::CompletedWithErrors, message, false)
        } else if conflicts > 0 {
            let message = format!(
                "{} Skipped {}; open the task's folders in Folder Sync to decide.",
                summary_line(&plan.summary, report.bytes_copied),
                plural(conflicts, "conflict"),
            );
            (RunStatus::CompletedWithErrors, message, false)
        } else {
            (
                RunStatus::Succeeded,
                summary_line(&plan.summary, report.bytes_copied),
                false,
            )
        };
        RunOutcome {
            status,
            job_id: run.job_id.clone(),
            ops_total: total,
            ops_done: done,
            ops_failed: report.failed,
            conflicts,
            bytes_copied: report.bytes_copied,
            message: Some(message),
            failures,
            retryable,
        }
    }
}

/// An earlier sync of the same pair that was cut short by a crash: put its
/// half-finished operations right and close it. The fresh plan then picks
/// up whatever it didn't do.
fn repair_interrupted_job(app: &AppHandle, left: &str, right: &str) {
    let Some(ix) = shell_sync::index(app) else {
        return;
    };
    let job = match ix.interrupted_job(&shell_sync::pair_key(left), &shell_sync::pair_key(right)) {
        Ok(Some(job)) => job,
        Ok(None) => return,
        Err(e) => return eprintln!("scan database: {e}"),
    };
    let repaired = ix
        .job_detail(&job.id)
        .map_err(shell_sync::db_err)
        .and_then(|detail| {
            let s = &detail.summary;
            shell_sync::with_roots(app, &s.left, &s.right, |l, r| {
                Ok(shell_sync::recover_job(l, r, &detail))
            })
        });
    match repaired {
        Ok(_) => {
            if let Err(e) = ix.set_job_status(&job.id, JobStatus::Cancelled) {
                eprintln!("scan database: {e}");
            }
        }
        Err(e) => eprintln!("automation: repairing interrupted sync {}: {e}", job.id),
    }
}

fn cancelled_before_start() -> RunOutcome {
    let mut outcome = RunOutcome::failed("Cancelled before anything was changed.", false);
    outcome.status = RunStatus::Cancelled;
    outcome
}

/// An outcome decided from the plan alone, before anything ran.
fn planned(status: RunStatus, plan: &SyncPlan, message: Option<String>) -> RunOutcome {
    RunOutcome {
        status,
        job_id: None,
        ops_total: plan.ops.len(),
        ops_done: 0,
        ops_failed: 0,
        conflicts: plan.summary.conflicts,
        bytes_copied: 0,
        message,
        failures: Vec::new(),
        retryable: false,
    }
}

/// Whether an error is likely to go away by itself: a side that isn't
/// there right now (unplugged drive, share or VPN down) or a network error.
fn is_transient(e: &CoreError) -> bool {
    use std::io::ErrorKind::*;
    match e {
        CoreError::Io { source, .. } => {
            matches!(
                source.kind(),
                NotFound
                    | TimedOut
                    | Interrupted
                    | WouldBlock
                    | ConnectionRefused
                    | ConnectionReset
                    | ConnectionAborted
                    | NotConnected
                    | BrokenPipe
                    | HostUnreachable
                    | NetworkUnreachable
                    | NetworkDown
                    | ResourceBusy
                    | StaleNetworkFileHandle
            ) || matches!(
                // Windows: device not ready, network path / name not found,
                // network name deleted, semaphore timeout, network unreachable.
                source.raw_os_error(),
                Some(21 | 53 | 64 | 67 | 121 | 1231) if cfg!(windows)
            )
        }
        CoreError::Remote { .. } => true,
        CoreError::Other(message) => mentions_unreachable(message),
        CoreError::BadPattern { .. } => false,
    }
}

fn mentions_unreachable(message: &str) -> bool {
    let m = message.to_lowercase();
    [
        "doesn't exist",
        "isn't reachable",
        "not reachable",
        "timed out",
        "connection",
        "network",
    ]
    .iter()
    .any(|k| m.contains(k))
}

/// Files on one side of a comparison.
fn files_on(entries: &[FolderEntry], left: bool) -> usize {
    entries
        .iter()
        .map(|e| {
            let present = if left {
                e.left.is_some()
            } else {
                e.right.is_some()
            };
            match (present, e.is_dir) {
                (false, _) => 0,
                (true, true) => files_on(&e.children, left),
                (true, false) => 1,
            }
        })
        .sum()
}

fn find_entry<'a>(entries: &'a [FolderEntry], rel: &str) -> Option<&'a FolderEntry> {
    entries.iter().find_map(|e| {
        if e.rel_path == rel {
            Some(e)
        } else if rel.starts_with(&e.rel_path)
            && rel.as_bytes().get(e.rel_path.len()) == Some(&b'/')
        {
            find_entry(&e.children, rel)
        } else {
            None
        }
    })
}

/// Files the plan deletes from one side (a folder counts its files).
fn files_deleted(plan: &SyncPlan, entries: &[FolderEntry], left: bool) -> usize {
    let dir = if left {
        Direction::ToLeft
    } else {
        Direction::ToRight
    };
    plan.ops
        .iter()
        .filter(|o| o.kind == OpKind::Delete && o.direction == dir)
        .map(|o| match (o.is_dir, find_entry(entries, &o.rel_path)) {
            (true, Some(e)) => files_on(&e.children, left),
            (true, None) => 0,
            (false, _) => 1,
        })
        .sum()
}

/// Why the task's safety settings refuse this plan, if they do.
fn safety_violation(safety: &SafetyOptions, plan: &SyncPlan) -> Option<String> {
    const RELAX: &str = "If that's intended, relax the task's safety settings and run it again.";
    let sides = plan.comparison.as_ref().map(|cmp| {
        [true, false].map(|left| {
            (
                left,
                files_on(&cmp.entries, left),
                files_deleted(plan, &cmp.entries, left),
            )
        })
    });
    let side_name = |left: bool| if left { "left" } else { "right" };
    let root = |left: bool| {
        if left {
            plan.left_root.as_str()
        } else {
            plan.right_root.as_str()
        }
    };
    let warning = |kind| plan.warnings.iter().find(|w| w.kind == kind);

    if safety.abort_on_empty_side {
        if let Some(w) = warning(WarningKind::EmptySide) {
            let empty = sides.iter().flatten().find(|&&(left, _, deleted)| {
                deleted > 0 && sides.iter().flatten().any(|o| o.0 != left && o.1 == 0)
            });
            return Some(match empty {
                Some(&(left, _, deleted)) => format!(
                    "Stopped for safety: the {} folder {} is empty, so this run would delete all {} on the {}. If it's \
                     on a drive or network share that isn't connected, connect it; the next run will sync normally. \
                     If the folder really is meant to be empty, relax the task's safety settings and run it again.",
                    side_name(!left),
                    root(!left),
                    plural(deleted, "file"),
                    side_name(left),
                ),
                None => format!("Stopped for safety: {} {RELAX}", tidy(&w.message)),
            });
        }
    }
    if safety.abort_on_mass_deletion {
        if let Some(w) = warning(WarningKind::MassDeletion) {
            return Some(format!("Stopped for safety: {} {RELAX}", tidy(&w.message)));
        }
    }
    let sides = sides?;
    if let Some(max) = safety.max_deletions {
        let total: usize = sides.iter().map(|s| s.2).sum();
        if total > max as usize {
            return Some(format!(
                "Stopped for safety: this run would delete {}, more than the task's limit of {max}. {RELAX}",
                plural(total, "file"),
            ));
        }
    }
    if let Some(pct) = safety.max_deletion_percent {
        for (left, present, deleted) in sides {
            if present > 0
                && deleted >= PERCENT_GUARD_MIN_FILES
                && deleted * 100 > pct as usize * present
            {
                return Some(format!(
                    "Stopped for safety: this run would delete {deleted} of the {present} files on the {} ({}%), \
                     more than the task's limit of {pct}%. {RELAX}",
                    side_name(left),
                    deleted * 100 / present,
                ));
            }
        }
    }
    None
}

/// Collapses the runs of spaces some planner messages contain.
fn tidy(message: &str) -> String {
    message.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn plural(n: usize, noun: &str) -> String {
    format!("{n} {noun}{}", if n == 1 { "" } else { "s" })
}

/// "Copied 12 items (3.4 MB), deleted 2, created 1 folder."
fn summary_line(s: &SyncSummary, bytes: u64) -> String {
    let copied = s.copy_to_left + s.copy_to_right;
    let deleted = s.delete_left + s.delete_right;
    let folders = s.mkdir_left + s.mkdir_right;
    let mut parts = Vec::new();
    if copied > 0 {
        parts.push(format!(
            "copied {} ({})",
            plural(copied, "item"),
            human_bytes(bytes)
        ));
    }
    if deleted > 0 {
        parts.push(format!("deleted {deleted}"));
    }
    if folders > 0 {
        parts.push(format!("created {}", plural(folders, "folder")));
    }
    if parts.is_empty() {
        return "Done.".into();
    }
    let mut line = parts.join(", ");
    line[..1].make_ascii_uppercase();
    line + "."
}

fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[unit])
    }
}

// ---------- task commands ----------

const DEFAULT_TASK_NAME: &str = "New sync task";

fn default_task() -> SyncTask {
    SyncTask {
        id: String::new(),
        name: DEFAULT_TASK_NAME.into(),
        description: String::new(),
        enabled: true,
        left: String::new(),
        right: String::new(),
        mode: SyncMode::default(),
        matrix: SyncMatrix::preset(SyncMode::default()),
        skip_empty_folders: false,
        conflicts: ConflictPolicy::default(),
        compare: FolderCompareOptions::default(),
        exec: ExecOptions::default(),
        triggers: vec![Trigger::Interval { every_minutes: 60 }],
        catch_up_missed: true,
        safety: SafetyOptions::default(),
        retry: RunRetry::default(),
        notify: NotifyRules::default(),
        created_ms: 0,
        updated_ms: 0,
    }
}

/// Overlays `patch` on `base`: objects merge key by key (so a partial
/// prefill keeps the defaults it doesn't mention), anything else replaces.
fn merge_json(base: &mut serde_json::Value, patch: serde_json::Value) {
    match (base, patch) {
        (serde_json::Value::Object(b), serde_json::Value::Object(p)) => {
            for (k, v) in p {
                match b.get_mut(&k) {
                    Some(slot) => merge_json(slot, v),
                    None => {
                        b.insert(k, v);
                    }
                }
            }
        }
        (slot, v) => *slot = v,
    }
}

fn same_name(a: &str, b: &str) -> bool {
    a.trim().to_lowercase() == b.trim().to_lowercase()
}

/// `name`, or `name (2)`, `name (3)`… if it's taken (case-insensitively).
fn unique_name(name: &str, taken: &[String]) -> String {
    let free = |n: &str| !taken.iter().any(|t| same_name(t, n));
    if free(name) {
        return name.to_owned();
    }
    (2..)
        .map(|i| format!("{name} ({i})"))
        .find(|n| free(n))
        .unwrap_or_default()
}

fn task_names(store: &TaskStore, except_id: &str) -> pcompare_core::Result<Vec<String>> {
    Ok(store
        .tasks()
        .map_err(err)?
        .into_iter()
        .filter(|t| t.id != except_id || except_id.is_empty())
        .map(|t| t.name)
        .collect())
}

#[tauri::command]
pub async fn automation_tasks(app: AppHandle) -> CmdResult<Vec<SyncTask>> {
    blocking(move || automation(&app).store.tasks().map_err(err)).await
}

/// A new, unsaved task with defaults, overlaid with `prefill` (e.g. the
/// folders and options of a Folder Sync session).
#[tauri::command]
pub async fn automation_new_task(
    app: AppHandle,
    prefill: Option<serde_json::Value>,
) -> CmdResult<SyncTask> {
    blocking(move || {
        let mut value = serde_json::to_value(default_task()).map_err(err)?;
        if let Some(patch) = prefill.filter(|p| !p.is_null()) {
            merge_json(&mut value, patch);
        }
        let mut task: SyncTask = serde_json::from_value(value)
            .map_err(|e| err(format!("The task settings aren't valid: {e}")))?;
        task.id.clear();
        task.created_ms = 0;
        task.updated_ms = 0;
        if task.name.trim().is_empty() {
            task.name = DEFAULT_TASK_NAME.into();
        }
        task.name = unique_name(task.name.trim(), &task_names(&automation(&app).store, "")?);
        Ok(task)
    })
    .await
}

#[tauri::command]
pub async fn automation_validate(app: AppHandle, task: SyncTask) -> CmdResult<Vec<TaskIssue>> {
    blocking(move || {
        let others = task_names(&automation(&app).store, &task.id)?;
        Ok(validate_task(&task, &others))
    })
    .await
}

/// Saves (creating when `task.id` is empty). Fails with the validation
/// errors joined; returns the saved task and any warnings.
#[tauri::command]
pub async fn automation_save(
    app: AppHandle,
    task: SyncTask,
) -> CmdResult<(SyncTask, Vec<TaskIssue>)> {
    blocking(move || {
        let saved = automation(&app).store.save_task(task).map_err(err)?;
        after_change(&app);
        Ok(saved)
    })
    .await
}

#[tauri::command]
pub async fn automation_delete(app: AppHandle, id: String) -> CmdResult<()> {
    blocking(move || {
        automation(&app).store.delete_task(&id).map_err(err)?;
        if let Some(ix) = shell_sync::index(&app) {
            if let Err(e) = ix.forget_task(&id) {
                eprintln!("scan database: {e}");
            }
        }
        after_change(&app);
        Ok(())
    })
    .await
}

#[tauri::command]
pub async fn automation_set_enabled(
    app: AppHandle,
    id: String,
    enabled: bool,
) -> CmdResult<SyncTask> {
    blocking(move || {
        let task = automation(&app)
            .store
            .set_enabled(&id, enabled)
            .map_err(err)?;
        after_change(&app);
        Ok(task)
    })
    .await
}

/// Saves a disabled copy named "<name> (copy)".
#[tauri::command]
pub async fn automation_duplicate(app: AppHandle, id: String) -> CmdResult<SyncTask> {
    blocking(move || {
        let st = automation(&app);
        let mut task = st
            .store
            .task(&id)
            .map_err(err)?
            .ok_or_else(|| err("That automation task no longer exists."))?;
        let taken = task_names(&st.store, "")?;
        let free = |n: &str| !taken.iter().any(|t| same_name(t, n));
        let base = format!("{} (copy)", task.name);
        task.name = if free(&base) {
            base
        } else {
            (2..)
                .map(|i| format!("{} (copy {i})", task.name))
                .find(|n| free(n))
                .unwrap_or_default()
        };
        task.id.clear();
        task.enabled = false;
        task.created_ms = 0;
        task.updated_ms = 0;
        let (saved, _) = st.store.save_task(task).map_err(err)?;
        after_change(&app);
        Ok(saved)
    })
    .await
}

/// Runs a task now (also when disabled or paused).
#[tauri::command]
pub async fn automation_run_now(app: AppHandle, id: String) -> CmdResult<()> {
    blocking(move || automation(&app).scheduler.run_now(&id).map_err(err)).await
}

/// Cancels a task's current run, pending retry and queued follow-up.
#[tauri::command]
pub async fn automation_cancel(app: AppHandle, id: String) -> CmdResult<()> {
    blocking(move || {
        automation(&app).scheduler.cancel(&id);
        Ok(())
    })
    .await
}

#[tauri::command]
pub async fn automation_statuses(app: AppHandle) -> CmdResult<Vec<TaskStatus>> {
    blocking(move || Ok(automation(&app).scheduler.statuses())).await
}

/// Run history, newest first; for one task when `task_id` is given.
#[tauri::command]
pub async fn automation_runs(
    app: AppHandle,
    task_id: Option<String>,
    limit: usize,
) -> CmdResult<Vec<AutomationRun>> {
    blocking(move || match shell_sync::index(&app) {
        Some(ix) => ix
            .list_runs(task_id.as_deref(), limit)
            .map_err(shell_sync::db_err),
        None => Ok(Vec::new()),
    })
    .await
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpcomingRun {
    at_ms: i64,
    trigger: RunTrigger,
}

/// The next `count` scheduled times of `triggers` from now (the editor's
/// preview). Fails with the first invalid trigger's problem (the editor
/// uses this to check a cron expression as it's typed).
#[tauri::command]
pub async fn automation_upcoming(
    triggers: Vec<Trigger>,
    count: usize,
) -> CmdResult<Vec<UpcomingRun>> {
    for t in &triggers {
        trigger::validate(t)?;
    }
    Ok(
        trigger::upcoming(&triggers, None, Local::now(), count.min(100))
            .into_iter()
            .map(|(at, trigger)| UpcomingRun {
                at_ms: at.timestamp_millis(),
                trigger,
            })
            .collect(),
    )
}

#[tauri::command]
pub async fn automation_describe_trigger(trigger: Trigger) -> CmdResult<String> {
    Ok(trigger::describe(&trigger))
}

#[tauri::command]
pub async fn automation_settings(app: AppHandle) -> CmdResult<AutomationSettings> {
    blocking(move || automation(&app).store.settings().map_err(err)).await
}

/// Saves the settings; start-at-login is applied to the OS first, so a
/// failure there leaves everything as it was.
#[tauri::command]
pub async fn automation_save_settings(
    app: AppHandle,
    settings: AutomationSettings,
) -> CmdResult<()> {
    blocking(move || {
        let st = automation(&app);
        let before = st.store.settings().map_err(err)?;
        if before.start_at_login != settings.start_at_login {
            let autostart = app.autolaunch();
            let applied = if settings.start_at_login { autostart.enable() } else { autostart.disable() };
            applied.map_err(|e| {
                err(if settings.start_at_login {
                    format!("PCompare couldn't add itself to the programs that start at login: {e}")
                } else {
                    format!("PCompare couldn't remove itself from the programs that start at login: {e}")
                })
            })?;
        }
        st.store.save_settings(&settings).map_err(err)?;
        after_change(&app);
        Ok(())
    })
    .await
}

/// The export/import file.
#[derive(Serialize, Deserialize)]
struct ExportFile {
    version: u32,
    tasks: Vec<SyncTask>,
}

const EXPORT_VERSION: u32 = 1;

/// Exports tasks (all of them when `ids` is empty) as JSON. Ids, timestamps
/// and notification channels (which are per machine) are left out; nothing
/// secret is in a task.
#[tauri::command]
pub async fn automation_export(app: AppHandle, ids: Vec<String>) -> CmdResult<String> {
    blocking(move || {
        let tasks = automation(&app)
            .store
            .tasks()
            .map_err(err)?
            .into_iter()
            .filter(|t| ids.is_empty() || ids.contains(&t.id))
            .map(|mut t| {
                t.id.clear();
                t.created_ms = 0;
                t.updated_ms = 0;
                t.notify.channels.clear();
                t
            })
            .collect();
        serde_json::to_string_pretty(&ExportFile {
            version: EXPORT_VERSION,
            tasks,
        })
        .map_err(err)
    })
    .await
}

/// Reads an export (or a bare task list, or one task).
fn parse_import(json: &str) -> pcompare_core::Result<Vec<SyncTask>> {
    let bad = |e: serde_json::Error| err(format!("This isn't a PCompare automation export: {e}"));
    let value: serde_json::Value = serde_json::from_str(json).map_err(bad)?;
    if let Some(version) = value.get("version").and_then(|v| v.as_u64()) {
        if version > u64::from(EXPORT_VERSION) {
            return Err(err(format!(
                "This export comes from a newer PCompare (format {version}); update PCompare to import it."
            )));
        }
    }
    if value.get("tasks").is_some() {
        Ok(serde_json::from_value::<ExportFile>(value)
            .map_err(bad)?
            .tasks)
    } else if value.is_array() {
        serde_json::from_value(value).map_err(bad)
    } else {
        Ok(vec![serde_json::from_value(value).map_err(bad)?])
    }
}

/// Imports tasks as new ones; a name that's taken gets " (2)" and so on.
/// Nothing is saved if any task has errors.
#[tauri::command]
pub async fn automation_import(app: AppHandle, json: String) -> CmdResult<Vec<SyncTask>> {
    blocking(move || {
        let st = automation(&app);
        let mut tasks = parse_import(&json)?;
        if tasks.is_empty() {
            return Err(err("The file contains no automation tasks."));
        }
        let known_channels: Vec<String> = st
            .channels
            .list()
            .map_err(err)?
            .into_iter()
            .map(|c| c.id)
            .collect();
        let mut taken = task_names(&st.store, "")?;
        let mut problems = Vec::new();
        for task in &mut tasks {
            task.id.clear();
            task.created_ms = 0;
            task.updated_ms = 0;
            task.notify.channels.retain(|c| known_channels.contains(c));
            task.name = unique_name(task.name.trim(), &taken);
            for issue in validate_task(task, &taken).iter().filter(|i| i.error) {
                problems.push(format!("“{}”: {}", task.name, issue.message));
            }
            taken.push(task.name.clone());
        }
        if !problems.is_empty() {
            return Err(err(format!("Nothing was imported. {}", problems.join(" "))));
        }
        let mut saved = Vec::new();
        let mut failure = None;
        for task in tasks {
            match st.store.save_task(task) {
                Ok((t, _)) => saved.push(t),
                Err(e) => {
                    failure = Some(e);
                    break;
                }
            }
        }
        after_change(&app);
        match failure {
            Some(e) if saved.is_empty() => Err(err(e)),
            Some(e) => Err(err(format!(
                "Imported {} of the tasks, then stopped: {e}",
                saved.len()
            ))),
            None => Ok(saved),
        }
    })
    .await
}

// ---------- notification channels ----------

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChannelWithSecrets {
    channel: ChannelConfig,
    secrets: SecretsPresent,
}

#[tauri::command]
pub async fn notify_channels(app: AppHandle) -> CmdResult<Vec<ChannelWithSecrets>> {
    blocking(move || {
        let channels = &automation(&app).channels;
        Ok(channels
            .list()
            .map_err(err)?
            .into_iter()
            .map(|channel| ChannelWithSecrets {
                secrets: channels.secrets_present(&channel.id).unwrap_or_default(),
                channel,
            })
            .collect())
    })
    .await
}

/// Creates (empty `id`) or updates a channel. Secrets: `None` keeps the
/// stored value, `""` removes it.
#[tauri::command]
pub async fn notify_save_channel(
    app: AppHandle,
    id: String,
    name: String,
    enabled: bool,
    settings: ChannelSettings,
    secrets: ChannelSecrets,
) -> CmdResult<ChannelConfig> {
    blocking(move || {
        let saved = automation(&app)
            .channels
            .save(&id, &name, enabled, settings, &secrets)
            .map_err(err)?;
        let _ = app.emit(CHANGED_EVENT, ());
        Ok(saved)
    })
    .await
}

/// Deletes a channel and its secrets, and removes it from every task.
#[tauri::command]
pub async fn notify_delete_channel(app: AppHandle, id: String) -> CmdResult<()> {
    blocking(move || {
        let st = automation(&app);
        st.channels.delete(&id).map_err(err)?;
        for mut task in st.store.tasks().map_err(err)? {
            if task.notify.channels.contains(&id) {
                task.notify.channels.retain(|c| *c != id);
                let name = task.name.clone();
                // The scheduler skips unknown channel ids, so a task that
                // can't be re-saved still works.
                if let Err(e) = st.store.save_task(task) {
                    eprintln!("automation: removing the channel from “{name}”: {e}");
                }
            }
        }
        after_change(&app);
        Ok(())
    })
    .await
}

/// Sends a sample notification through a channel (enabled or not); the
/// error says why delivery failed.
#[tauri::command]
pub async fn notify_test_channel(app: AppHandle, id: String) -> CmdResult<()> {
    blocking(move || {
        let channels = &automation(&app).channels;
        let channel = channels
            .get(&id)
            .map_err(err)?
            .ok_or_else(|| err("That notification channel no longer exists."))?;
        let mut sample = pcompare_notify::test_notification(&channel.name);
        sample.host = host_name();
        sample.app_version = app.package_info().version.to_string();
        pcompare_notify::send(&channel, channels.secrets(), &sample).map_err(err)
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefill_merges_into_the_defaults() {
        let mut value = serde_json::to_value(default_task()).unwrap();
        merge_json(
            &mut value,
            serde_json::json!({ "left": "/a", "safety": { "maxDeletions": 10 }, "triggers": [] }),
        );
        let task: SyncTask = serde_json::from_value(value).unwrap();
        assert_eq!(task.left, "/a");
        assert_eq!(task.safety.max_deletions, Some(10));
        // Unmentioned fields keep their defaults, nested ones too.
        assert_eq!(
            task.safety.max_deletion_percent,
            SafetyOptions::default().max_deletion_percent
        );
        assert!(task.triggers.is_empty());
        assert!(task.enabled);
    }

    #[test]
    fn names_are_made_unique_case_insensitively() {
        let taken = vec!["Backup".to_owned(), "backup (2)".to_owned()];
        assert_eq!(unique_name("Other", &taken), "Other");
        assert_eq!(unique_name("BACKUP", &taken), "BACKUP (3)");
    }

    #[test]
    fn imports_accept_exports_lists_and_single_tasks() {
        let task = serde_json::to_value(default_task()).unwrap();
        let export = serde_json::json!({ "version": 1, "tasks": [task.clone()] }).to_string();
        assert_eq!(parse_import(&export).unwrap().len(), 1);
        assert_eq!(
            parse_import(&serde_json::json!([task.clone(), task.clone()]).to_string())
                .unwrap()
                .len(),
            2
        );
        assert_eq!(parse_import(&task.to_string()).unwrap().len(), 1);
        assert!(
            parse_import(&serde_json::json!({ "version": 9, "tasks": [] }).to_string()).is_err()
        );
        assert!(parse_import("not json").is_err());
    }

    #[test]
    fn summary_lines_read_naturally() {
        let s = SyncSummary {
            copy_to_right: 2,
            delete_right: 1,
            mkdir_right: 1,
            ..Default::default()
        };
        assert_eq!(
            summary_line(&s, 2048),
            "Copied 2 items (2.0 KB), deleted 1, created 1 folder."
        );
        assert_eq!(summary_line(&SyncSummary::default(), 0), "Done.");
    }

    /// A fresh pair of folders under the temp dir, removed on drop.
    struct Pair(std::path::PathBuf);

    impl Pair {
        fn new(tag: &str, left: &[&str], right: &[&str]) -> Pair {
            let root = std::env::temp_dir()
                .join(format!("pcompare-automation-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            for (side, files) in [("l", left), ("r", right)] {
                std::fs::create_dir_all(root.join(side)).unwrap();
                for f in files {
                    let path = root.join(side).join(f);
                    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                    std::fs::write(path, f).unwrap();
                }
            }
            Pair(root)
        }

        fn plan(&self, mode: SyncMode) -> SyncPlan {
            pcompare_sync::sync::plan_sync(
                self.0.join("l"),
                self.0.join("r"),
                mode,
                &FolderCompareOptions::default(),
            )
            .unwrap()
        }
    }

    impl Drop for Pair {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn an_empty_source_aborts_a_mirror() {
        let pair = Pair::new("empty", &[], &["a", "b", "c"]);
        let why = safety_violation(
            &SafetyOptions::default(),
            &pair.plan(SyncMode::MirrorLeftToRight),
        )
        .unwrap();
        assert!(
            why.contains("the left folder") && why.contains("all 3 files on the right"),
            "{why}"
        );
        let off = SafetyOptions {
            abort_on_empty_side: false,
            max_deletion_percent: None,
            ..Default::default()
        };
        assert_eq!(
            safety_violation(&off, &pair.plan(SyncMode::MirrorLeftToRight)),
            None
        );
    }

    #[test]
    fn deletion_limits_count_the_files_in_deleted_folders() {
        let kept = ["a", "b", "c", "d"];
        let extra = ["x/1", "x/2", "x/3", "x/4", "x/5", "x/6"];
        let right: Vec<&str> = kept.iter().chain(&extra).copied().collect();
        let pair = Pair::new("limits", &kept, &right);
        let plan = pair.plan(SyncMode::MirrorLeftToRight);
        // 6 of the right side's 10 files: over the default 50 %.
        let why = safety_violation(&SafetyOptions::default(), &plan).unwrap();
        assert!(
            why.contains("6 of the 10 files on the right (60%)"),
            "{why}"
        );
        let count = SafetyOptions {
            max_deletions: Some(5),
            max_deletion_percent: None,
            ..Default::default()
        };
        assert!(safety_violation(&count, &plan)
            .unwrap()
            .contains("delete 6 files"));
        let relaxed = SafetyOptions {
            max_deletions: Some(6),
            max_deletion_percent: Some(60),
            ..Default::default()
        };
        assert_eq!(safety_violation(&relaxed, &plan), None);
    }

    #[test]
    fn missing_sides_and_network_errors_are_transient() {
        let missing = CoreError::io("/x", std::io::Error::from(std::io::ErrorKind::NotFound));
        assert!(is_transient(&missing));
        let denied = CoreError::io(
            "/x",
            std::io::Error::from(std::io::ErrorKind::PermissionDenied),
        );
        assert!(!is_transient(&denied));
        assert!(is_transient(&CoreError::Other(
            "The left folder /x doesn't exist.".into()
        )));
        assert!(!is_transient(&CoreError::Other(
            "Both sides are the same folder.".into()
        )));
    }
}
