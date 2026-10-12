//! Tauri commands for Folder Sync and 3-way Folder Merge.
//!
//! Planning and comparison work on remote stores too (through the shared
//! `RemoteManager`); execution writes to disk, so it is local-only for now.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use pcompare_core::folder::FolderCompareOptions;
use pcompare_core::CoreError;
use pcompare_sync::fsops::TrashDeleter;
use pcompare_sync::merge::{self, FolderMergeResult, MergeDecision, MergeReport};
use pcompare_sync::sync::{self as engine, SyncMode, SyncOp, SyncPlan, SyncProgress, SyncReport};
use tauri::ipc::Channel;
use tauri::{AppHandle, Manager};

use crate::{blocking, state, with_remote, CmdResult};

/// Minimum spacing between progress messages, so a sync of many small files
/// doesn't flood the IPC channel.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(50);

/// Cancellation flags of running sync jobs, keyed by a client-chosen id.
#[derive(Default)]
struct SyncJobs(Mutex<HashMap<String, Arc<AtomicBool>>>);

impl SyncJobs {
    fn map(&self) -> MutexGuard<'_, HashMap<String, Arc<AtomicBool>>> {
        // A panic while holding the lock leaves the map itself intact.
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

fn jobs(app: &AppHandle) -> tauri::State<'_, SyncJobs> {
    // Registered on first use so this module needs no setup hook in main.rs.
    if app.try_state::<SyncJobs>().is_none() {
        app.manage(SyncJobs::default());
    }
    app.state::<SyncJobs>()
}

/// Execution writes and deletes files, which remote stores don't support yet.
fn require_local_dirs(paths: &[(&str, &str)]) -> pcompare_core::Result<()> {
    for (label, path) in paths {
        if pcompare_remote::is_remote(path) {
            return Err(CoreError::Other(format!(
                "Remote write-back isn't supported yet: the {label} folder is on a remote profile."
            )));
        }
    }
    Ok(())
}

fn require_existing_dir(label: &str, path: &str) -> pcompare_core::Result<()> {
    if Path::new(path).is_dir() {
        Ok(())
    } else {
        Err(CoreError::Other(format!("The {label} folder {path} doesn't exist.")))
    }
}

// ---------- folder sync ----------

#[tauri::command]
pub async fn sync_plan(
    app: AppHandle,
    left: String,
    right: String,
    mode: SyncMode,
    options: FolderCompareOptions,
) -> CmdResult<SyncPlan> {
    blocking(move || {
        with_remote(&app, [&left, &right], |r| {
            let (lfs, lpath) = r.target(&left)?;
            let (rfs, rpath) = r.target(&right)?;
            let mut plan = engine::plan_sync_with(&*lfs, &lpath, &*rfs, &rpath, mode, &options)?;
            plan.left_root = left.clone();
            plan.right_root = right.clone();
            Ok(plan)
        })
    })
    .await
}

/// Runs the given operations, streaming progress over `on_progress`. The
/// job can be stopped with [`sync_cancel`] using the same `job_id`.
#[tauri::command]
pub async fn sync_execute(
    app: AppHandle,
    left: String,
    right: String,
    ops: Vec<SyncOp>,
    job_id: String,
    on_progress: Channel<SyncProgress>,
) -> CmdResult<SyncReport> {
    require_local_dirs(&[("left", &left), ("right", &right)])
        .and_then(|_| require_existing_dir("left", &left))
        .and_then(|_| require_existing_dir("right", &right))
        .map_err(|e| e.to_string())?;

    let cancel = Arc::new(AtomicBool::new(false));
    jobs(&app).map().insert(job_id.clone(), cancel.clone());
    let result = blocking(move || {
        let mut last_sent: Option<Instant> = None;
        let mut emit = |p: &SyncProgress| {
            let finished = p.current.is_none();
            if finished || last_sent.is_none_or(|t| t.elapsed() >= PROGRESS_INTERVAL) {
                last_sent = Some(Instant::now());
                // The window may have closed; that must not abort the sync.
                let _ = on_progress.send(p.clone());
            }
        };
        let deleter = TrashDeleter::new();
        Ok(engine::execute_sync(
            Path::new(&left),
            Path::new(&right),
            &ops,
            &deleter,
            &cancel,
            &mut emit,
        ))
    })
    .await;
    jobs(&app).map().remove(&job_id);
    result
}

/// Asks a running [`sync_execute`] to stop after the current operation.
#[tauri::command]
pub fn sync_cancel(app: AppHandle, job_id: String) {
    if let Some(flag) = jobs(&app).map().get(&job_id) {
        flag.store(true, Ordering::Relaxed);
    }
}

// ---------- 3-way folder merge ----------

#[tauri::command]
pub async fn folder_merge_compare(
    app: AppHandle,
    base: String,
    left: String,
    right: String,
    options: FolderCompareOptions,
) -> CmdResult<FolderMergeResult> {
    blocking(move || {
        let result = with_remote(&app, [&left, &right], |r| {
            let (bfs, bpath) = r.target(&base)?;
            let (lfs, lpath) = r.target(&left)?;
            let (rfs, rpath) = r.target(&right)?;
            let mut res = merge::compare_merge_with((&*bfs, &bpath), (&*lfs, &lpath), (&*rfs, &rpath), &options)?;
            res.base_root = base.clone();
            res.left_root = left.clone();
            res.right_root = right.clone();
            Ok(res)
        });
        if result.is_err() {
            state(&app).remote.forget(&base);
        }
        result
    })
    .await
}

#[tauri::command]
pub async fn folder_merge_execute(
    base: String,
    left: String,
    right: String,
    output: String,
    decisions: Vec<MergeDecision>,
) -> CmdResult<MergeReport> {
    blocking(move || {
        require_local_dirs(&[("base", &base), ("left", &left), ("right", &right), ("output", &output)])?;
        for (label, path) in [("base", &base), ("left", &left), ("right", &right)] {
            require_existing_dir(label, path)?;
        }
        merge::execute_merge(
            Path::new(&base),
            Path::new(&left),
            Path::new(&right),
            Path::new(&output),
            &decisions,
            &TrashDeleter::new(),
        )
    })
    .await
}
