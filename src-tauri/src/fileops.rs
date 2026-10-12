//! Tauri commands for Folder Compare's file operations (bc5:file-ops):
//! copy / move to the other side or a folder, delete, rename, new folder,
//! attributes and touch, on local and `remote://` sides.
//!
//! Every operation runs in the background under a client-chosen id: it
//! streams [`FileOpProgress`] over a channel, waits in the [`OpQueue`] while
//! an earlier operation works on overlapping paths, and can be paused,
//! resumed, cancelled and asked about name clashes through the id.

use std::collections::HashMap;
use std::path::Path;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use pcompare_core::vfs_write::VfsWrite;
use pcompare_sync::fileops::{
    self, ConflictAnswer, ConflictQuestion, Control, FileOpProgress, FileOpReport, FileOpRequest,
    FilePhase, OpQueue,
};
use pcompare_sync::fsops::TrashDeleter;
use pcompare_sync::remote::Root;
use tauri::ipc::Channel;
use tauri::{AppHandle, Manager};

use crate::{blocking, with_remote, CmdResult};

/// Minimum spacing between progress messages while bytes flow.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(80);

/// A running (or queued) operation's handles.
struct Run {
    ctl: Arc<Control>,
    answers: Sender<ConflictAnswer>,
}

#[derive(Default)]
struct FileOps {
    queue: OpQueue,
    runs: Mutex<HashMap<String, Run>>,
}

impl FileOps {
    fn runs(&self) -> MutexGuard<'_, HashMap<String, Run>> {
        // A panic while holding the lock leaves the map itself intact.
        self.runs.lock().unwrap_or_else(|e| e.into_inner())
    }
}

fn ops(app: &AppHandle) -> tauri::State<'_, FileOps> {
    // Registered on first use so this module needs no setup hook in main.rs.
    if app.try_state::<FileOps>().is_none() {
        app.manage(FileOps::default());
    }
    app.state::<FileOps>()
}

/// Registers an operation's handles, removing them when dropped.
struct RunGuard {
    app: AppHandle,
    id: String,
}

impl Drop for RunGuard {
    fn drop(&mut self) {
        ops(&self.app).runs().remove(&self.id);
    }
}

type SideFs = Option<(Arc<dyn VfsWrite>, String)>;

fn root<'a>(path: &'a str, fs: &'a SideFs) -> Root<'a> {
    match fs {
        Some((fs, rel)) => Root::Remote {
            fs: &**fs,
            path: rel,
            label: path,
        },
        None => Root::Local(Path::new(path)),
    }
}

/// Forwards progress to the webview: phase changes and questions at once,
/// byte counts throttled.
fn progress_sender(channel: Channel<FileOpProgress>) -> impl FnMut(&FileOpProgress) {
    let mut last: Option<(Instant, FilePhase)> = None;
    move |p: &FileOpProgress| {
        let important = p.question.is_some() || last.is_none_or(|(_, phase)| phase != p.phase);
        if important || last.is_none_or(|(t, _)| t.elapsed() >= PROGRESS_INTERVAL) {
            last = Some((Instant::now(), p.phase));
            // The window may have closed; that must not abort the operation.
            let _ = channel.send(p.clone());
        }
    }
}

/// Waits for the user's answer to a name clash; a cancelled operation (or a
/// closed window) answers "cancel".
fn wait_for_answer(rx: &Receiver<ConflictAnswer>, ctl: &Control) -> ConflictAnswer {
    // Answers that arrived for an earlier question are stale.
    while rx.try_recv().is_ok() {}
    loop {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(answer) => return answer,
            Err(RecvTimeoutError::Timeout) if !ctl.is_cancelled() => {}
            Err(_) => return ConflictAnswer::Cancel,
        }
    }
}

/// Runs a file operation from `src_root` (to `dst_root` for copy / move, or
/// the other side for touch "to other side"). Resolves with the report when
/// it is done; progress streams over `on_progress` meanwhile.
#[tauri::command]
pub async fn fileop_start(
    app: AppHandle,
    op_id: String,
    src_root: String,
    dst_root: Option<String>,
    request: FileOpRequest,
    on_progress: Channel<FileOpProgress>,
) -> CmdResult<FileOpReport> {
    let ctl = Arc::new(Control::default());
    let (tx, rx) = mpsc::channel();
    ops(&app).runs().insert(
        op_id.clone(),
        Run {
            ctl: ctl.clone(),
            answers: tx,
        },
    );
    let guard = RunGuard {
        app: app.clone(),
        id: op_id,
    };
    blocking(move || {
        let _guard = guard;
        let mut send = progress_sender(on_progress);
        send(&FileOpProgress::default()); // queued
        let state = ops(&app);
        let paths = fileops::lock_paths(&request, &src_root, dst_root.as_deref());
        let Some(_slot) = state.queue.acquire(&paths, &ctl.cancel) else {
            let report = FileOpReport {
                cancelled: true,
                ..Default::default()
            };
            send(&FileOpProgress {
                phase: FilePhase::Finished,
                ..Default::default()
            });
            return Ok(report);
        };
        let dst = dst_root.clone().unwrap_or_default();
        with_remote(&app, [&src_root, &dst], |r| {
            let sfs = r.target_write(&src_root)?;
            let dfs = if dst.is_empty() {
                None
            } else {
                r.target_write(&dst)?
            };
            let src = root(&src_root, &sfs);
            let to = (!dst.is_empty()).then(|| root(&dst, &dfs));
            let mut ask = |_: &ConflictQuestion| wait_for_answer(&rx, &ctl);
            Ok(fileops::run(
                &request,
                src,
                to,
                &TrashDeleter::new(),
                &ctl,
                &mut send,
                &mut ask,
            ))
        })
    })
    .await
}

/// Holds (`paused`) or resumes an operation.
#[tauri::command]
pub fn fileop_pause(app: AppHandle, op_id: String, paused: bool) {
    if let Some(run) = ops(&app).runs().get(&op_id) {
        run.ctl.set_paused(paused);
    }
}

/// Stops an operation at the next safe point (or takes it out of the queue).
#[tauri::command]
pub fn fileop_cancel(app: AppHandle, op_id: String) {
    if let Some(run) = ops(&app).runs().get(&op_id) {
        run.ctl.cancel();
    }
}

/// Answers the name clash an operation is asking about.
#[tauri::command]
pub fn fileop_answer(app: AppHandle, op_id: String, answer: ConflictAnswer) {
    if let Some(run) = ops(&app).runs().get(&op_id) {
        let _ = run.answers.send(answer);
    }
}
