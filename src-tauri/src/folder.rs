//! Tauri commands for Folder Compare.
//!
//! A comparison streams progress (entries listed so far) over a channel and
//! can be stopped with [`compare_folders_cancel`] using the client's run key,
//! so huge trees neither freeze the overlay nor have to run to the end. With
//! `stream` set, the rows themselves travel over the channel too, folder by
//! folder as they're listed, so the view fills in live and a tree of
//! hundreds of thousands of rows never has to cross IPC as one huge reply.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use pcompare_core::folder::{
    self, ChecksumCache, ContentMethod, ContentVerdict, FolderCompareOptions, FolderCompareResult,
    FolderEntry, FolderLevel, FolderScan, ScanControl, ScanProgress,
};
use pcompare_remote::RemoteManager;
use serde::Serialize;
use pcompare_core::text::TextDiffOptions;
use tauri::ipc::Channel;
use tauri::{AppHandle, Manager};

use crate::{archive, blocking, sync, with_remote, CmdResult}; // bc5:archives

/// Minimum spacing between progress messages.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);
/// Streamed rows waiting past this many go out without waiting for the
/// interval, so no single message grows huge.
const BATCH_ROWS: usize = 20_000;

/// One message on a comparison's channel.
#[derive(Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanUpdate {
    progress: Option<ScanProgress>,
    /// Folder rows, in the order the engine reported them (streaming only).
    levels: Vec<FolderLevel>,
    /// The last message of the run: everything streamed has been sent.
    end: bool,
}

/// What hasn't been sent yet.
#[derive(Default)]
struct Outbox {
    update: ScanUpdate,
    rows: usize,
    last_sent: Option<Instant>,
}

impl Outbox {
    fn flush(&mut self, channel: &Channel<ScanUpdate>, force: bool) {
        let due = self.last_sent.is_none_or(|t| t.elapsed() >= PROGRESS_INTERVAL);
        if !(force || due || self.rows >= BATCH_ROWS) {
            return;
        }
        if self.update.progress.is_none() && self.update.levels.is_empty() && !self.update.end {
            return;
        }
        self.last_sent = Some(Instant::now());
        self.rows = 0;
        // The window may have closed; that must not abort the compare.
        let _ = channel.send(std::mem::take(&mut self.update));
    }
}

/// Cancellation flags of running comparisons, keyed by a client-chosen id.
#[derive(Default)]
struct FolderRuns(Mutex<HashMap<String, Arc<AtomicBool>>>);

impl FolderRuns {
    fn map(&self) -> MutexGuard<'_, HashMap<String, Arc<AtomicBool>>> {
        // A panic while holding the lock leaves the map itself intact.
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

fn runs(app: &AppHandle) -> tauri::State<'_, FolderRuns> {
    // Registered on first use so this module needs no setup hook in main.rs.
    if app.try_state::<FolderRuns>().is_none() {
        app.manage(FolderRuns::default());
    }
    app.state::<FolderRuns>()
}

/// Registers a cancellation flag for a run, removing it when dropped.
struct RunGuard {
    app: AppHandle,
    key: String,
    flag: Arc<AtomicBool>,
}

impl RunGuard {
    fn new(app: &AppHandle, key: String) -> Self {
        let flag = Arc::new(AtomicBool::new(false));
        runs(app).map().insert(key.clone(), flag.clone());
        RunGuard {
            app: app.clone(),
            key,
            flag,
        }
    }
}

impl Drop for RunGuard {
    fn drop(&mut self) {
        let runs = runs(&self.app);
        let mut map = runs.map();
        // A newer run may have reused the key; only remove our own flag.
        if map
            .get(&self.key)
            .is_some_and(|f| Arc::ptr_eq(f, &self.flag))
        {
            map.remove(&self.key);
        }
    }
}

/// Listed trees kept for restaging; the oldest go beyond this many (each
/// holds its folders' listings in memory).
const MAX_SCANS: usize = 8;

/// A comparison's resolved sides and listed tree.
struct ScanSession {
    left: String,
    right: String,
    expand: bool,
    sides: (archive::FolderSide, archive::FolderSide),
    scan: FolderScan,
}

/// Kept scans by the client's scan id, least recently used first.
#[derive(Default)]
struct FolderScans(Mutex<Vec<(String, Arc<ScanSession>)>>);

impl FolderScans {
    fn list(&self) -> MutexGuard<'_, Vec<(String, Arc<ScanSession>)>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn take(&self, id: &str) -> Option<Arc<ScanSession>> {
        let mut list = self.list();
        let at = list.iter().position(|(key, _)| key == id)?;
        let entry = list.remove(at);
        let session = entry.1.clone();
        list.push(entry);
        Some(session)
    }

    fn put(&self, id: String, session: Arc<ScanSession>) {
        let mut list = self.list();
        list.retain(|(key, _)| *key != id);
        list.push((id, session));
        let excess = list.len().saturating_sub(MAX_SCANS);
        list.drain(..excess);
    }

    fn forget(&self, id: &str) {
        self.list().retain(|(key, _)| key != id);
    }
}

fn scans(app: &AppHandle) -> tauri::State<'_, FolderScans> {
    if app.try_state::<FolderScans>().is_none() {
        app.manage(FolderScans::default());
    }
    app.state::<FolderScans>()
}

/// The tree `id` kept, if it's of these folders.
fn kept_session(app: &AppHandle, id: &str, left: &str, right: &str, expand: bool) -> Option<Arc<ScanSession>> {
    scans(app)
        .take(id)
        .filter(|s| s.left == left && s.right == right && s.expand == expand)
}

/// Resolves both sides afresh, with nothing listed yet.
fn new_session(r: &RemoteManager, left: &str, right: &str, expand: bool) -> pcompare_core::Result<Arc<ScanSession>> {
    // bc5:archives — sides resolve in archive.rs: folders, archive /
    // snapshot / svn: sides, archives in the tree optionally expanded,
    // and a side with no folder chosen yet as empty.
    let sides = (archive::folder_side(r, left, expand)?, archive::folder_side(r, right, expand)?);
    Ok(Arc::new(ScanSession {
        left: left.to_owned(),
        right: right.to_owned(),
        expand,
        sides,
        scan: FolderScan::new(),
    }))
}

/// What a comparison command was asked for.
struct Request {
    left: String,
    right: String,
    options: FolderCompareOptions,
    run_key: Option<String>,
    expand: bool,
    stream: bool,
    /// Keep the listed tree under this id (and report filtered rows).
    scan_id: Option<String>,
    /// Walk folders the filters leave out, so their contents show.
    show_filtered: bool,
}

/// Compares two folders (either may be a `remote://` path or empty),
/// streaming [`ScanUpdate`]s over `on_progress`. Pass a `run_key` to make
/// the run cancellable with [`compare_folders_cancel`]. With `stream`, rows
/// arrive as [`FolderLevel`]s on the channel and the returned result has no
/// `entries` (its stats are still complete); the channel's `end` message
/// follows the last level. With a `scan_id`, the folders are listed afresh
/// and kept for [`restage_folders`], and rows the filters leave out are
/// reported too, marked (their folders walked with `show_filtered`).
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub async fn compare_folders(
    app: AppHandle,
    left: String,
    right: String,
    options: FolderCompareOptions,
    run_key: Option<String>,
    on_progress: Channel<ScanUpdate>,
    expand_archives: Option<bool>, // bc5:archives
    stream: Option<bool>,
    scan_id: Option<String>,
    show_filtered: Option<bool>,
) -> CmdResult<FolderCompareResult> {
    let request = Request {
        left,
        right,
        options,
        run_key,
        expand: expand_archives.unwrap_or(false),
        stream: stream.unwrap_or(false),
        scan_id,
        show_filtered: show_filtered.unwrap_or(false),
    };
    run(app, request, on_progress, false).await
}

/// [`compare_folders`] on the tree a scan id kept: only the stages the
/// options changed since its last run are redone (see
/// [`folder::CompareNeeds`], reported in the result's `needs`), so filters,
/// alignment and comparison criteria apply without reading any folder
/// again. Falls back to a fresh listing when the scan is gone or was of
/// other folders.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub async fn restage_folders(
    app: AppHandle,
    left: String,
    right: String,
    options: FolderCompareOptions,
    scan_id: String,
    run_key: Option<String>,
    on_progress: Channel<ScanUpdate>,
    expand_archives: Option<bool>,
    stream: Option<bool>,
    show_filtered: Option<bool>,
) -> CmdResult<FolderCompareResult> {
    let request = Request {
        left,
        right,
        options,
        run_key,
        expand: expand_archives.unwrap_or(false),
        stream: stream.unwrap_or(false),
        scan_id: Some(scan_id),
        show_filtered: show_filtered.unwrap_or(false),
    };
    run(app, request, on_progress, true).await
}

/// Drops a kept tree (its view closed or moved to other folders).
#[tauri::command]
pub fn forget_folder_scan(app: AppHandle, scan_id: String) {
    scans(&app).forget(&scan_id);
}

async fn run(
    app: AppHandle,
    req: Request,
    on_progress: Channel<ScanUpdate>,
    restage: bool,
) -> CmdResult<FolderCompareResult> {
    let guard = req.run_key.clone().map(|key| RunGuard::new(&app, key));
    blocking(move || {
        let (left, right, expand, stream) = (&req.left, &req.right, req.expand, req.stream);
        // Sending under the lock keeps messages in report order: a folder's
        // "listed" level must not be overtaken by its "complete" one.
        let outbox = Mutex::new(Outbox::default());
        let outbox_lock = || outbox.lock().unwrap_or_else(|e| e.into_inner());
        let res = with_remote(&app, [left, right], |r| {
            let kept = match &req.scan_id {
                Some(id) if restage => kept_session(&app, id, left, right, expand),
                _ => None,
            };
            let session = match kept {
                Some(session) => session,
                None => {
                    let session = new_session(r, left, right, expand)?;
                    if let Some(id) = &req.scan_id {
                        scans(&app).put(id.clone(), session.clone());
                    }
                    session
                }
            };
            let (ls, rs) = &session.sides;
            let options = archive::options_for_sides(&req.options, ls, rs);
            let (lfs, lpath, rfs, rpath) = (&ls.fs, &ls.path, &rs.fs, &rs.path);
            let cache = sync::checksum_cache(&app, [archive::cache_root(left), archive::cache_root(right)]);
            let stored = archive::stored_checksums(ls, rs, cache.as_ref().map(|c| c as &dyn ChecksumCache));
            let report = |p: &ScanProgress| {
                let mut out = outbox_lock();
                out.update.progress = Some(p.clone());
                out.flush(&on_progress, false);
            };
            let levels = |level: FolderLevel| {
                let mut out = outbox_lock();
                out.rows += level.entries.len();
                out.update.levels.push(level);
                out.flush(&on_progress, false);
            };
            let control = ScanControl {
                cache: match &stored {
                    Some(s) => Some(s as &dyn ChecksumCache), // bc5:archives
                    None => cache.as_ref().map(|c| c as &dyn ChecksumCache),
                },
                cancel: guard.as_ref().map(|g| &*g.flag),
                progress: Some(&report),
                levels: stream.then_some(&levels as &(dyn Fn(FolderLevel) + Sync)),
                // Without a kept scan, nothing would outlive this run.
                scan: req.scan_id.is_some().then_some(&session.scan),
                keep_filtered: req.scan_id.is_some(),
                expand_filtered: req.show_filtered,
            };
            let mut res = folder::compare_folders_controlled(
                &**lfs, lpath, &**rfs, rpath, &options, control,
            )
            .inspect_err(|e| {
                // A tree that failed to compare may be what's broken (a
                // dropped connection): list afresh next time. A cancelled
                // one is fine; what it listed stays good.
                if let Some(id) = req.scan_id.as_ref().filter(|_| e.to_string() != folder::CANCELLED) {
                    scans(&app).forget(id);
                }
            })?;
            if let Some(cache) = cache {
                let _ = cache.flush();
            }
            res.left_root = left.clone();
            res.right_root = right.clone();
            if stream {
                // Already on the client, level by level.
                res.entries = Vec::new();
            }
            Ok(res)
        });
        let mut out = outbox_lock();
        out.update.end = true;
        out.flush(&on_progress, true);
        res
    })
    .await
}

/// Compares just the rows at `rels` (tree paths, right-hand names) the way
/// [`compare_folders`] would report them, each `None` where it would have
/// no such row — the folder view's refresh after a file operation (see
/// [`folder::compare_entries`]). With the `scan_id` of the comparison on
/// screen, folders on the way down come from its kept tree, and filtered
/// rows are reported as that comparison reports them.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub async fn compare_folder_entries(
    app: AppHandle,
    left: String,
    right: String,
    options: FolderCompareOptions,
    rels: Vec<String>,
    scan_id: Option<String>,
    expand_archives: Option<bool>,
    show_filtered: Option<bool>,
) -> CmdResult<Vec<Option<FolderEntry>>> {
    let expand = expand_archives.unwrap_or(false);
    blocking(move || {
        with_remote(&app, [&left, &right], |r| {
            let kept = scan_id.as_deref().and_then(|id| kept_session(&app, id, &left, &right, expand));
            let session = match kept {
                Some(session) => session,
                None => new_session(r, &left, &right, expand)?,
            };
            let (ls, rs) = &session.sides;
            let options = archive::options_for_sides(&options, ls, rs);
            let cache = sync::checksum_cache(&app, [archive::cache_root(&left), archive::cache_root(&right)]);
            let stored = archive::stored_checksums(ls, rs, cache.as_ref().map(|c| c as &dyn ChecksumCache));
            let control = ScanControl {
                cache: match &stored {
                    Some(s) => Some(s as &dyn ChecksumCache), // bc5:archives
                    None => cache.as_ref().map(|c| c as &dyn ChecksumCache),
                },
                scan: Some(&session.scan),
                keep_filtered: scan_id.is_some(),
                expand_filtered: show_filtered.unwrap_or(false),
                ..Default::default()
            };
            let rows = folder::compare_entries(&*ls.fs, &ls.path, &*rs.fs, &rs.path, &rels, &options, control)?;
            if let Some(cache) = cache {
                let _ = cache.flush();
            }
            Ok(rows)
        })
    })
    .await
}

/// Asks a running comparison to stop; it then fails with
/// [`folder::CANCELLED`].
#[tauri::command]
pub fn compare_folders_cancel(app: AppHandle, run_key: String) {
    if let Some(flag) = runs(&app).map().get(&run_key) {
        flag.store(true, Ordering::Relaxed);
    }
}

/// Re-checks one file pair with a specific content method — the folder
/// view's "Compare contents" action on a selected row.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub async fn compare_file_contents(
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
            // bc5:archives — checksums stored by archives and snapshots
            if let Some(v) = archive::stored_verdict(r, &left, left_size, &right, right_size, method)? {
                return Ok(v);
            }
            let (lfs, lpath) = r.target(&left)?;
            let (rfs, rpath) = r.target(&right)?;
            folder::compare_contents(
                &*lfs, &lpath, left_size, &*rfs, &rpath, right_size, method, &rules,
            )
        })
    })
    .await
}
