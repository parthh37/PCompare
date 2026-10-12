//! Tauri commands for Folder Sync and 3-way Folder Merge.
//!
//! Planning and comparison work on remote stores too (through the shared
//! `RemoteManager`); execution writes to disk, so it is local-only for now.
//!
//! Every sync run is a job in the scan database (`pcompare-index`): its
//! operations are journaled as they run, so a sync cut short by a crash can
//! be resumed, discarded or rolled back from the UI. The database also holds
//! the checksum cache that speeds up rescans and the per-pair baseline that
//! two-way sync needs.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use pcompare_core::folder::{ChecksumCache, FolderCompareOptions};
use pcompare_core::store::now_ms;
use pcompare_core::CoreError;
use pcompare_index::{CachedChecksums, Index, IndexStats, JobDetail, JobStatus, JobSummary, NewJob, OpState};
use pcompare_sync::baseline;
use pcompare_sync::exec::{
    self, Disposal, ExecOptions, ExecProgress, ExecReport, PreflightIssue, Recovery, RollbackItem, RollbackResult,
};
use pcompare_sync::fsops::{self, TrashDeleter};
use pcompare_sync::journal::{Journal, NullJournal};
use pcompare_sync::merge::{self, FolderMergeResult, MergeDecision, MergeReport};
use pcompare_sync::sync::{self as engine, ConflictPolicy, PlanOptions, SyncMode, SyncOp, SyncPlan};
use serde::Serialize;
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

// ---------- scan database helpers ----------

fn db_err(e: pcompare_index::IndexError) -> CoreError {
    CoreError::Other(format!("scan database: {e}"))
}

fn index(app: &AppHandle) -> Option<&Index> {
    state(app).inner().index.as_ref()
}

fn require_index(app: &AppHandle) -> pcompare_core::Result<&Index> {
    index(app).ok_or_else(|| CoreError::Other("The scan database isn't available.".into()))
}

/// The key a folder is known by in the database: its canonical path, so
/// `C:\Data\` and `c:/data` share history. Remote paths are used verbatim.
fn pair_key(path: &str) -> String {
    if pcompare_remote::is_remote(path) {
        return path.to_owned();
    }
    fsops::canonical_lenient(Path::new(path))
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| path.to_owned())
}

/// A checksum cache for the local sides of a comparison (remote listings
/// carry no trustworthy file identity, so they are always read). An empty
/// path is a side with no folder chosen yet, which has nothing to cache.
pub(crate) fn checksum_cache<'a>(app: &'a AppHandle, paths: [&str; 2]) -> Option<CachedChecksums<'a>> {
    let roots = paths.map(|p| (!p.is_empty() && !pcompare_remote::is_remote(p)).then_some(p));
    index(app)?.checksum_cache(roots).ok()
}

fn as_cache<'a>(c: &'a Option<CachedChecksums<'_>>) -> Option<&'a dyn ChecksumCache> {
    c.as_ref().map(|c| c as &dyn ChecksumCache)
}

/// Plans a sync, using the pair's baseline (two-way) and the checksum cache.
fn plan(
    app: &AppHandle,
    left: &str,
    right: &str,
    mode: SyncMode,
    options: &FolderCompareOptions,
    conflicts: ConflictPolicy,
) -> pcompare_core::Result<SyncPlan> {
    with_remote(app, [left, right], |r| {
        let (lfs, lpath) = r.target(left)?;
        let (rfs, rpath) = r.target(right)?;
        let baseline = match (mode, index(app)) {
            (SyncMode::TwoWay, Some(ix)) => Some(ix.load_baseline(&pair_key(left), &pair_key(right)).map_err(db_err)?),
            _ => None,
        };
        let cache = checksum_cache(app, [left, right]);
        let plan_opts = PlanOptions { baseline: baseline.as_ref(), conflicts };
        let mut plan = engine::plan_sync_with(&*lfs, &lpath, &*rfs, &rpath, mode, options, &plan_opts, as_cache(&cache))?;
        if let Some(cache) = cache {
            let _ = cache.flush();
        }
        plan.left_root = left.to_owned();
        plan.right_root = right.to_owned();
        Ok(plan)
    })
}

/// Records what is in sync now as the pair's new baseline.
fn update_baseline(app: &AppHandle, left: &str, right: &str, plan: &SyncPlan) {
    let (Some(ix), Some(cmp)) = (index(app), plan.comparison.as_ref()) else {
        return;
    };
    let (lk, rk) = (pair_key(left), pair_key(right));
    let old = ix.load_baseline(&lk, &rk).unwrap_or_default();
    let next = baseline::update_baseline(&old, &cmp.entries);
    if let Err(e) = ix.save_baseline(&lk, &rk, &next) {
        eprintln!("scan database: {e}");
    }
}

/// The versioning subfolder of a job: its start time, so a resumed run
/// keeps adding to the same folder.
fn job_stamp(started_ms: i64) -> String {
    fsops::run_stamp(started_ms)
}

fn job_status(report: &ExecReport) -> JobStatus {
    if report.cancelled {
        JobStatus::Cancelled
    } else if report.stopped.is_some() {
        JobStatus::Stopped
    } else if report.failed > 0 {
        JobStatus::CompletedWithErrors
    } else {
        JobStatus::Completed
    }
}

/// Registers a cancellation flag for the run, removing it when dropped.
struct RunGuard {
    app: AppHandle,
    key: String,
    flag: Arc<AtomicBool>,
}

impl RunGuard {
    fn new(app: &AppHandle, key: String) -> Self {
        let flag = Arc::new(AtomicBool::new(false));
        jobs(app).map().insert(key.clone(), flag.clone());
        RunGuard { app: app.clone(), key, flag }
    }
}

impl Drop for RunGuard {
    fn drop(&mut self) {
        jobs(&self.app).map().remove(&self.key);
    }
}

/// Forwards progress to the webview, throttled.
fn progress_sender(channel: Channel<ExecProgress>) -> impl FnMut(&ExecProgress) {
    let mut last_sent: Option<Instant> = None;
    move |p: &ExecProgress| {
        let important = p.current.is_none() || p.waiting_on.is_some();
        if important || last_sent.is_none_or(|t| t.elapsed() >= PROGRESS_INTERVAL) {
            last_sent = Some(Instant::now());
            // The window may have closed; that must not abort the sync.
            let _ = channel.send(p.clone());
        }
    }
}

/// A finished run: its journal id, the report, and what remains to do.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncRun {
    job_id: Option<String>,
    report: ExecReport,
    /// A fresh plan with the same settings; `None` if re-planning failed.
    plan: Option<SyncPlan>,
}

struct RunSpec<'a> {
    left: &'a str,
    right: &'a str,
    mode: SyncMode,
    compare: &'a FolderCompareOptions,
    conflicts: ConflictPolicy,
}

/// Executes `ops` under `job_id`'s journal, closes the job, re-plans and
/// refreshes the baseline.
fn run_job(
    app: &AppHandle,
    spec: &RunSpec,
    job: Option<(&str, i64)>,
    ops: &[SyncOp],
    exec_opts: &ExecOptions,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(&ExecProgress),
) -> SyncRun {
    let ix = index(app);
    let started = job.map_or_else(now_ms, |(_, t)| t);
    let deleter = exec_opts.disposal.deleter(&job_stamp(started));
    let mut null = NullJournal;
    let mut db_journal = job.zip(ix).map(|((id, _), ix)| ix.journal(id));
    let journal: &mut dyn Journal = match db_journal.as_mut() {
        Some(j) => j,
        None => &mut null,
    };
    let report = exec::execute(
        Path::new(spec.left),
        Path::new(spec.right),
        ops,
        exec_opts,
        &*deleter,
        journal,
        cancel,
        progress,
    );
    drop(db_journal);
    if let (Some((id, _)), Some(ix)) = (job, ix) {
        if let Err(e) = ix.finish_job(id, &report, job_status(&report)) {
            eprintln!("scan database: {e}");
        }
    }
    let fresh = plan(app, spec.left, spec.right, spec.mode, spec.compare, spec.conflicts).ok();
    if let Some(p) = &fresh {
        update_baseline(app, spec.left, spec.right, p);
    }
    SyncRun {
        job_id: job.map(|(id, _)| id.to_owned()),
        report,
        plan: fresh,
    }
}

/// Puts half-finished operations of an interrupted job right: staged old
/// versions go back (or are disposed of if the new file made it), temp
/// files are removed.
fn recover_job(detail: &JobDetail) -> Vec<Recovery> {
    let (left, right) = (Path::new(&detail.summary.left), Path::new(&detail.summary.right));
    let deleter = detail.options.disposal.deleter(&job_stamp(detail.summary.started_ms));
    detail
        .ops
        .iter()
        .filter(|o| o.state == OpState::Running)
        .map(|o| exec::recover_op(left, right, &o.op, o.staged.as_deref().map(Path::new), &*deleter))
        .collect()
}

// ---------- folder sync ----------

#[tauri::command]
pub async fn sync_plan(
    app: AppHandle,
    left: String,
    right: String,
    mode: SyncMode,
    options: FolderCompareOptions,
    conflicts: Option<ConflictPolicy>,
) -> CmdResult<SyncPlan> {
    blocking(move || plan(&app, &left, &right, mode, &options, conflicts.unwrap_or_default())).await
}

/// Checks folders, write access and free space before a run — and, when
/// old versions are to be kept, that the versioning folder can take them.
#[tauri::command]
pub async fn sync_preflight(
    left: String,
    right: String,
    ops: Vec<SyncOp>,
    exec: Option<ExecOptions>,
) -> CmdResult<Vec<PreflightIssue>> {
    blocking(move || {
        require_local_dirs(&[("left", &left), ("right", &right)])?;
        let mut issues = exec::preflight(Path::new(&left), Path::new(&right), &ops);
        if let Some(Disposal::Versioning { folder }) = exec.map(|e| e.disposal) {
            let folder = Path::new(&folder);
            if !folder.as_os_str().is_empty() {
                if let Err(e) = std::fs::create_dir_all(folder) {
                    issues.push(PreflightIssue {
                        blocking: true,
                        message: format!("The versioning folder {} can't be created: {e}.", folder.display()),
                    });
                }
            }
        }
        Ok(issues)
    })
    .await
}

/// Runs the given operations as a journaled job, streaming progress over
/// `on_progress`. The run can be stopped with [`sync_cancel`] using `run_key`.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub async fn sync_execute(
    app: AppHandle,
    left: String,
    right: String,
    mode: SyncMode,
    ops: Vec<SyncOp>,
    exec: ExecOptions,
    compare: FolderCompareOptions,
    conflicts: ConflictPolicy,
    run_key: String,
    on_progress: Channel<ExecProgress>,
) -> CmdResult<SyncRun> {
    require_local_dirs(&[("left", &left), ("right", &right)])
        .and_then(|_| require_existing_dir("left", &left))
        .and_then(|_| require_existing_dir("right", &right))
        .map_err(|e| e.to_string())?;

    let mut exec = exec;
    if let Disposal::Versioning { folder } = &mut exec.disposal {
        // An empty folder would resolve against the working directory.
        if folder.trim().is_empty() {
            *folder = sync_default_versioning_folder(app.clone())?;
        }
    }
    let guard = RunGuard::new(&app, run_key);
    blocking(move || {
        let started = now_ms();
        let job_id = match index(&app) {
            Some(ix) => {
                let job = NewJob {
                    left: &pair_key(&left),
                    right: &pair_key(&right),
                    mode,
                    options: &exec,
                    ops: &ops,
                };
                // A journal is part of the safety net: refuse to run without
                // one rather than silently losing crash recovery.
                Some(ix.create_job(&job).map_err(db_err)?)
            }
            None => None,
        };
        let spec = RunSpec { left: &left, right: &right, mode, compare: &compare, conflicts };
        let mut send = progress_sender(on_progress);
        Ok(run_job(
            &app,
            &spec,
            job_id.as_deref().map(|id| (id, started)),
            &ops,
            &exec,
            &guard.flag,
            &mut send,
        ))
    })
    .await
}

/// Asks a running sync to stop at the next safe point.
#[tauri::command]
pub fn sync_cancel(app: AppHandle, run_key: String) {
    if let Some(flag) = jobs(&app).map().get(&run_key) {
        flag.store(true, Ordering::Relaxed);
    }
}

/// The latest interrupted job for a folder pair, if any.
#[tauri::command]
pub async fn sync_interrupted(app: AppHandle, left: String, right: String) -> CmdResult<Option<JobSummary>> {
    blocking(move || match index(&app) {
        Some(ix) => ix.interrupted_job(&pair_key(&left), &pair_key(&right)).map_err(db_err),
        None => Ok(None),
    })
    .await
}

/// Repairs an interrupted job's half-finished operations, then runs every
/// operation that never completed.
#[tauri::command]
pub async fn sync_resume(
    app: AppHandle,
    job_id: String,
    compare: FolderCompareOptions,
    conflicts: ConflictPolicy,
    run_key: String,
    on_progress: Channel<ExecProgress>,
) -> CmdResult<SyncRun> {
    let guard = RunGuard::new(&app, run_key);
    blocking(move || {
        let ix = require_index(&app)?;
        let detail = ix.job_detail(&job_id).map_err(db_err)?;
        let s = &detail.summary;
        require_local_dirs(&[("left", &s.left), ("right", &s.right)])?;
        require_existing_dir("left", &s.left)?;
        require_existing_dir("right", &s.right)?;
        recover_job(&detail);
        ix.set_job_status(&job_id, JobStatus::Running).map_err(db_err)?;
        let remaining: Vec<SyncOp> = detail
            .ops
            .iter()
            .filter(|o| matches!(o.state, OpState::Pending | OpState::Running))
            .map(|o| o.op.clone())
            .collect();
        let spec = RunSpec { left: &s.left, right: &s.right, mode: s.mode, compare: &compare, conflicts };
        let mut send = progress_sender(on_progress);
        Ok(run_job(
            &app,
            &spec,
            Some((&job_id, s.started_ms)),
            &remaining,
            &detail.options,
            &guard.flag,
            &mut send,
        ))
    })
    .await
}

/// Repairs an interrupted job's half-finished operations and closes it
/// without running the rest.
#[tauri::command]
pub async fn sync_discard(app: AppHandle, job_id: String) -> CmdResult<Vec<Recovery>> {
    blocking(move || {
        let ix = require_index(&app)?;
        let detail = ix.job_detail(&job_id).map_err(db_err)?;
        let recovered = recover_job(&detail);
        ix.set_job_status(&job_id, JobStatus::Cancelled).map_err(db_err)?;
        Ok(recovered)
    })
    .await
}

/// Undoes a job's completed operations, newest first.
#[tauri::command]
pub async fn sync_rollback(app: AppHandle, job_id: String) -> CmdResult<Vec<RollbackResult>> {
    blocking(move || {
        let ix = require_index(&app)?;
        let detail = ix.job_detail(&job_id).map_err(db_err)?;
        let s = &detail.summary;
        require_local_dirs(&[("left", &s.left), ("right", &s.right)])?;
        // Finish any half-done operation first so the rollback sees a clean state.
        recover_job(&detail);
        let items: Vec<RollbackItem> = detail
            .ops
            .iter()
            .filter(|o| o.state == OpState::Done)
            .filter_map(|o| Some(RollbackItem { op: o.op.clone(), result: o.result.clone()? }))
            .collect();
        // Files the sync created still exist at their source, but go to the
        // Trash anyway in case the source has changed since.
        let results = exec::rollback(Path::new(&s.left), Path::new(&s.right), &items, &TrashDeleter::new());
        let undone: Vec<u32> = results.iter().filter(|r| r.ok).map(|r| r.id).collect();
        ix.mark_ops_rolled_back(&job_id, &undone).map_err(db_err)?;
        ix.set_job_status(&job_id, JobStatus::RolledBack).map_err(db_err)?;
        // The baseline describes the state the rollback just undid; keeping it
        // would make the next two-way sync push the restored old versions back
        // over the newer ones. Without it, two-way falls back to "newer wins".
        ix.forget_baseline(&s.left, &s.right).map_err(db_err)?;
        Ok(results)
    })
    .await
}

/// Sync history, newest first; for one pair when both paths are given.
#[tauri::command]
pub async fn sync_jobs(
    app: AppHandle,
    left: Option<String>,
    right: Option<String>,
    limit: usize,
) -> CmdResult<Vec<JobSummary>> {
    blocking(move || {
        let Some(ix) = index(&app) else {
            return Ok(Vec::new());
        };
        let keys = left.zip(right).map(|(l, r)| (pair_key(&l), pair_key(&r)));
        ix.list_jobs(keys.as_ref().map(|(l, r)| (l.as_str(), r.as_str())), limit)
            .map_err(db_err)
    })
    .await
}

#[tauri::command]
pub async fn sync_job_detail(app: AppHandle, job_id: String) -> CmdResult<JobDetail> {
    blocking(move || require_index(&app)?.job_detail(&job_id).map_err(db_err)).await
}

/// Drops a pair's two-way baseline; the next two-way sync starts fresh.
#[tauri::command]
pub async fn sync_forget_history(app: AppHandle, left: String, right: String) -> CmdResult<()> {
    blocking(move || {
        require_index(&app)?
            .forget_baseline(&pair_key(&left), &pair_key(&right))
            .map_err(db_err)
    })
    .await
}

/// Where versioned files go unless the user picks a folder.
#[tauri::command]
pub fn sync_default_versioning_folder(app: AppHandle) -> CmdResult<String> {
    let dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    Ok(dir.join("Versions").to_string_lossy().into_owned())
}

#[tauri::command]
pub async fn index_stats(app: AppHandle) -> CmdResult<IndexStats> {
    blocking(move || require_index(&app)?.stats().map_err(db_err)).await
}

#[tauri::command]
pub async fn index_clear_checksums(app: AppHandle) -> CmdResult<usize> {
    blocking(move || require_index(&app)?.clear_checksums().map_err(db_err)).await
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
