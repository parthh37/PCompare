//! Executes a sync plan as safely as the file system allows.
//!
//! Guarantees, per operation:
//! - **No torn files.** Data goes to a temp sibling, is flushed to disk and
//!   (optionally) read back and checksummed, then renamed into place.
//! - **No silent loss.** A file that changed since the preview is never
//!   overwritten or deleted; replaced and deleted items go to the Recycle
//!   Bin / Trash or a versioning folder, and the journal knows where.
//! - **No half-copied sources.** A source that changes mid-copy is retried.
//! - **Locked files are handled, not fatal.** Transient failures retry with
//!   backoff; a file still locked afterwards is skipped, deferred to the end
//!   of the run, or stops the run, as configured.
//!
//! Every attempt and outcome is reported to a [`Journal`] so an interrupted
//! run can be resumed or rolled back.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::baseline::Snap;
use crate::failure::{FailureKind, LockHolder, OpError};
use crate::fsops::{self, Deleter};
use crate::journal::Journal;
use crate::remote::{Partial, Root}; // bc5:remote-write
use crate::sync::{Direction, OpKind, SyncOp};

/// What to do with a file that is still locked after all retries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum LockedPolicy {
    /// Record it as failed and carry on.
    Skip,
    /// Move it to the end of the run and try again once everything else is
    /// done — by then the other program has often let go.
    #[default]
    Defer,
    /// Stop the whole run.
    Stop,
}

/// What to do when an operation fails for any other reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ErrorPolicy {
    #[default]
    Continue,
    Stop,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ExecOptions {
    /// Extra attempts after the first for transient failures.
    pub retry_attempts: u32,
    /// Wait before the first retry; doubles each time up to `retry_max_delay_ms`.
    pub retry_delay_ms: u64,
    pub retry_max_delay_ms: u64,
    pub on_locked: LockedPolicy,
    pub on_error: ErrorPolicy,
    /// Read every copy back and compare checksums before it replaces anything.
    pub verify: bool,
    /// Refuse to overwrite or delete targets that changed since the preview.
    pub check_drift: bool,
    /// Where deleted and replaced items go. The executor itself uses the
    /// [`Deleter`] it is given; callers build it with [`Disposal::deleter`].
    /// Stored with the job so a rollback knows where to look.
    pub disposal: Disposal,
    /// Copy NTFS owners and access control lists along with local files and
    /// folders (Windows; best effort, see [`fsops::copy_security`]) (bc5:file-ops).
    pub copy_acls: bool,
    /// Carry DOS 8.3 short names over (Windows; needs administrator rights).
    pub copy_short_names: bool,
}

/// What happens to deleted items and to the old versions of replaced files.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum Disposal {
    /// Recycle Bin / Trash: recoverable by hand, not by rollback.
    #[default]
    Trash,
    /// Moved into `<folder>/<run stamp>/<side>/…`, so a rollback can put
    /// everything back exactly.
    Versioning { folder: String },
    /// Gone for good.
    Permanent,
}

impl Disposal {
    /// Builds the deleter for one run; `stamp` names the run's versioning
    /// subfolder (e.g. a timestamp or the job id).
    pub fn deleter(&self, stamp: &str) -> Box<dyn Deleter> {
        match self {
            Disposal::Trash => Box::new(fsops::TrashDeleter::new()),
            Disposal::Versioning { folder } => Box::new(fsops::VersioningDeleter::new(folder, stamp)),
            Disposal::Permanent => Box::new(fsops::PermanentDeleter),
        }
    }
}

impl Default for ExecOptions {
    fn default() -> Self {
        ExecOptions {
            retry_attempts: 3,
            retry_delay_ms: 500,
            retry_max_delay_ms: 8_000,
            on_locked: LockedPolicy::Defer,
            on_error: ErrorPolicy::Continue,
            verify: false,
            check_drift: true,
            disposal: Disposal::Trash,
            copy_acls: false,
            copy_short_names: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Phase {
    #[default]
    Running,
    /// Waiting before another attempt at a failed operation.
    Retrying,
    /// Working through operations deferred because their files were locked.
    Deferred,
    /// Held by the user; continues where it stopped (bc5:file-ops).
    Paused,
    Finished,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecProgress {
    pub done: usize,
    pub total: usize,
    pub bytes_done: u64,
    pub bytes_total: u64,
    /// Relative path of the operation in progress.
    pub current: Option<String>,
    pub phase: Phase,
    /// Attempt number of the current operation (1 = first try).
    pub attempt: u32,
    /// Why the current operation is being retried.
    pub waiting_on: Option<FailureKind>,
    pub failed: usize,
    pub deferred: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpResult {
    pub id: u32,
    pub kind: OpKind,
    pub direction: Direction,
    pub rel_path: String,
    pub ok: bool,
    pub error: Option<String>,
    pub failure: Option<FailureKind>,
    pub attempts: u32,
    /// Bytes written by a successful copy.
    #[serde(default)]
    pub bytes: u64,
    /// Processes holding the file, when it was locked (Windows only).
    #[serde(default)]
    pub locked_by: Vec<LockHolder>,
    /// Where the replaced or deleted item can be restored from (versioning
    /// folder), if it was kept somewhere restorable.
    #[serde(default)]
    pub backup: Option<String>,
    /// State of the destination right after a successful copy, so a rollback
    /// can tell whether anyone has touched it since.
    #[serde(default)]
    pub written: Option<Snap>,
    /// The op was postponed because its file was locked, then retried.
    #[serde(default)]
    pub deferred: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecReport {
    pub results: Vec<OpResult>,
    pub succeeded: usize,
    pub failed: usize,
    pub bytes_copied: u64,
    /// The user cancelled; operations after the last result never ran.
    pub cancelled: bool,
    /// The run stopped itself (stop-on-error policy, or a run-wide failure
    /// such as a full disk); operations after the last result never ran.
    pub stopped: Option<String>,
    /// Ids of operations that never ran (cancelled or stopped).
    pub not_run: Vec<u32>,
    pub elapsed_ms: u128,
}

impl ExecReport {
    /// Failures grouped by kind, most common first (for the summary line).
    pub fn failure_counts(&self) -> Vec<(FailureKind, usize)> {
        let mut counts: Vec<(FailureKind, usize)> = Vec::new();
        for r in self.results.iter().filter(|r| !r.ok) {
            let kind = r.failure.unwrap_or(FailureKind::Other);
            match counts.iter_mut().find(|(k, _)| *k == kind) {
                Some((_, n)) => *n += 1,
                None => counts.push((kind, 1)),
            }
        }
        counts.sort_by_key(|&(_, n)| std::cmp::Reverse(n));
        counts
    }
}

/// Whether a failure is worth retrying after a pause. Every kind listed
/// here costs a full retry cycle (~3.5 s with the default backoff) per file
/// when the failure turns out to be permanent.
///
/// Access denied is retried only on Windows, where it is usually fleeting —
/// an antivirus scan, the search indexer, or a delete still pending — while
/// elsewhere it means real permissions that no amount of waiting will fix.
pub fn is_transient(kind: FailureKind) -> bool {
    matches!(
        kind,
        FailureKind::Locked | FailureKind::Network | FailureKind::SourceChanged | FailureKind::VerifyFailed
    ) || (kind == FailureKind::AccessDenied && cfg!(windows))
}

/// Delay before retry number `retry` (1-based): exponential, capped.
fn backoff(opts: &ExecOptions, retry: u32) -> Duration {
    let factor = 1u64 << (retry.saturating_sub(1)).min(16);
    Duration::from_millis(opts.retry_delay_ms.saturating_mul(factor).min(opts.retry_max_delay_ms))
}

/// Sleeps for `d`, waking early (returning false) if the run is cancelled.
fn pause(d: Duration, cancel: &AtomicBool) -> bool {
    let until = Instant::now() + d;
    while Instant::now() < until {
        if cancel.load(Ordering::Relaxed) {
            return false;
        }
        std::thread::sleep(Duration::from_millis(25).min(until - Instant::now()));
    }
    !cancel.load(Ordering::Relaxed)
}

/// Blocks while `pause` is set (bc5:file-ops). `on_change(true)` is called
/// when the hold starts and `on_change(false)` when it ends. Returns false
/// if the run was cancelled meanwhile.
pub(crate) fn hold(pause: &AtomicBool, cancel: &AtomicBool, mut on_change: impl FnMut(bool)) -> bool {
    if !pause.load(Ordering::Relaxed) {
        return !cancel.load(Ordering::Relaxed);
    }
    on_change(true);
    while pause.load(Ordering::Relaxed) && !cancel.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(50));
    }
    on_change(false);
    !cancel.load(Ordering::Relaxed)
}

pub(crate) fn side_label(dir: Direction) -> &'static str {
    match dir {
        Direction::ToRight => "right",
        Direction::ToLeft => "left",
    }
}

/// What a successful operation produced.
#[derive(Default)]
pub(crate) struct Done {
    pub(crate) bytes: u64,
    pub(crate) backup: Option<PathBuf>,
    pub(crate) written: Option<Snap>,
    /// Succeeded, but with a caveat worth showing (e.g. the old version
    /// couldn't go to the Trash and was kept next to the file instead).
    pub(crate) note: Option<String>,
}

pub(crate) struct Runner<'a> {
    pub(crate) left: Root<'a>,
    pub(crate) right: Root<'a>,
    pub(crate) opts: &'a ExecOptions,
    pub(crate) deleter: &'a dyn Deleter,
    pub(crate) journal: &'a mut dyn Journal,
    pub(crate) cancel: &'a AtomicBool,
    /// Set while the user holds the run (bc5:file-ops).
    pub(crate) pause: &'a AtomicBool,
    pub(crate) progress: &'a mut dyn FnMut(&ExecProgress),
    pub(crate) state: ExecProgress,
    pub(crate) last_tick: Option<Instant>,
    /// Remote transfers cut short by a network failure, kept so the retry
    /// continues instead of starting over (bc5:remote-write).
    pub(crate) partials: HashMap<u32, Partial<'a>>,
}

/// Minimum spacing between progress callbacks while bytes are flowing.
const TICK: Duration = Duration::from_millis(100);

impl<'a> Runner<'a> {
    fn emit(&mut self) {
        self.last_tick = Some(Instant::now());
        (self.progress)(&self.state);
    }

    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    /// Waits out a pause between operations; false if cancelled meanwhile.
    fn hold(&mut self) -> bool {
        let (pause, cancel) = (self.pause, self.cancel);
        let mut phase = self.state.phase;
        hold(pause, cancel, |paused| {
            if paused {
                phase = self.state.phase;
                self.state.phase = Phase::Paused;
            } else {
                self.state.phase = phase;
            }
            self.emit();
        })
    }

    /// Runs one op with retries. `Err` carries the last failure.
    fn run_with_retries(&mut self, op: &SyncOp) -> (Result<Done, OpError>, u32) {
        let mut attempt = 0;
        loop {
            attempt += 1;
            self.state.attempt = attempt;
            self.journal.op_started(op.id, attempt);
            let result = self.run_once(op);
            match result {
                Err(e)
                    if is_transient(e.kind)
                        && attempt <= self.opts.retry_attempts
                        && !self.cancelled() =>
                {
                    let phase = self.state.phase;
                    self.state.phase = Phase::Retrying;
                    self.state.waiting_on = Some(e.kind);
                    self.emit();
                    let go_on = pause(backoff(self.opts, attempt), self.cancel);
                    self.state.phase = phase;
                    self.state.waiting_on = None;
                    if !go_on {
                        return (Err(OpError::new(FailureKind::Cancelled, "cancelled")), attempt);
                    }
                }
                other => {
                    // Out of retries (or done): a kept partial transfer is no use now.
                    self.discard_partial(op.id);
                    return (other, attempt);
                }
            }
        }
    }

    pub(crate) fn roots(&self, dir: Direction) -> (Root<'a>, Root<'a>) {
        match dir {
            Direction::ToRight => (self.left, self.right),
            Direction::ToLeft => (self.right, self.left),
        }
    }

    /// Calls `f` with a byte counter that drives progress and cancellation.
    /// Counted bytes are divided by `share` (a transfer that moves the data
    /// twice reports each half), starting from `already` bytes.
    pub(crate) fn with_bytes<T>(
        &mut self,
        already: u64,
        share: u64,
        f: impl FnOnce(&mut dyn FnMut(u64) -> io::Result<()>) -> T,
    ) -> T {
        let base = self.state.bytes_done;
        let mut written = already;
        let cancel = self.cancel;
        let pause = self.pause;
        let result = {
            let state = &mut self.state;
            let progress = &mut *self.progress;
            let last_tick = &mut self.last_tick;
            let mut on_bytes = |n: u64| {
                // A pause holds a large copy mid-file (bc5:file-ops).
                let mut before = state.phase;
                let held = hold(pause, cancel, |paused| {
                    if paused {
                        before = state.phase;
                        state.phase = Phase::Paused;
                    } else {
                        state.phase = before;
                    }
                    progress(state);
                });
                if !held {
                    return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
                }
                written += n / share.max(1);
                if last_tick.is_none_or(|t| t.elapsed() >= TICK) {
                    *last_tick = Some(Instant::now());
                    state.bytes_done = base + written;
                    progress(state);
                }
                Ok(())
            };
            f(&mut on_bytes)
        };
        self.state.bytes_done = base;
        result
    }

    fn run_once(&mut self, op: &SyncOp) -> Result<Done, OpError> {
        let (src_root, dst_root) = self.roots(op.direction);
        let (Root::Local(src_root), Root::Local(dst_root)) = (src_root, dst_root) else {
            return self.run_once_remote(op, src_root, dst_root);
        };
        let (src_root, dst_root) = (src_root.to_path_buf(), dst_root.to_path_buf());
        // bc5:file-ops — aligned pairs have a different name on the left.
        let dst = fsops::resolve(&dst_root, op.dst_rel())?;
        match op.kind {
            OpKind::Mkdir => {
                if dst.is_file() {
                    return Err(OpError::new(FailureKind::TypeClash, "a file with this name exists at the destination"));
                }
                fs::create_dir_all(&dst).map_err(|e| OpError::io(&dst, e))?;
                let src = fsops::resolve(&src_root, op.src_rel())?;
                Ok(self.with_extras(&src, &dst, Done::default()))
            }
            OpKind::Delete => self.delete(op, &dst),
            OpKind::Copy => {
                let src = fsops::resolve(&src_root, op.src_rel())?;
                let done = self.copy(op, &src, &dst)?;
                Ok(self.with_extras(&src, &dst, done))
            }
        }
    }

    /// Copies ACLs / short names onto a local `dst` as configured, noting
    /// what couldn't be copied (bc5:file-ops).
    fn with_extras(&self, src: &Path, dst: &Path, mut done: Done) -> Done {
        let notes = fsops::copy_extras(src, dst, self.opts.copy_acls, self.opts.copy_short_names);
        if !notes.is_empty() {
            let mut all: Vec<String> = done.note.take().into_iter().collect();
            all.extend(notes);
            done.note = Some(all.join("; "));
        }
        done
    }

    /// Refuses to touch a target that no longer matches the preview.
    pub(crate) fn check_drift(&self, op: &SyncOp, dst: &Path, now: Option<Snap>) -> Result<(), OpError> {
        if !self.opts.check_drift || op.is_dir {
            return Ok(());
        }
        let expected = match (op.kind, op.overwrite) {
            (OpKind::Copy, false) => None,
            // Ops without a recorded target state can't be checked.
            _ => match op.dst_before {
                Some(s) => Some(s),
                None => return Ok(()),
            },
        };
        if now == expected {
            return Ok(());
        }
        let msg = match (expected, now) {
            (None, Some(_)) => "a file appeared at the destination after the preview",
            (Some(_), None) => "the destination disappeared after the preview",
            _ => "the destination was modified after the preview; preview again before syncing it",
        };
        Err(OpError {
            path: Some(dst.to_path_buf()),
            ..OpError::new(FailureKind::ChangedSincePreview, msg)
        })
    }

    pub(crate) fn delete(&mut self, op: &SyncOp, dst: &Path) -> Result<Done, OpError> {
        let now = fsops::snap(dst).map_err(|e| OpError::io(dst, e))?;
        let Some(now) = now else {
            return Ok(Done::default()); // already gone
        };
        self.check_drift(op, dst, Some(now))?;
        let backup = self
            .deleter
            .retire(dst, side_label(op.direction), op.dst_rel())
            .map_err(|e| OpError::io(dst, e))?;
        Ok(Done {
            backup,
            ..Done::default()
        })
    }

    fn copy(&mut self, op: &SyncOp, src: &Path, dst: &Path) -> Result<Done, OpError> {
        let now = fsops::snap(dst).map_err(|e| OpError::io(dst, e))?;
        if fs::symlink_metadata(dst).is_ok_and(|m| m.is_dir()) {
            return Err(OpError::new(FailureKind::TypeClash, "a folder with this name exists at the destination"));
        }
        self.check_drift(op, dst, now)?;
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent).map_err(|e| OpError::io(parent, e))?;
        }

        // Stream the data into a temp sibling, reporting bytes as they flow.
        let verify = self.opts.verify;
        let temp = self.with_bytes(0, 1, |on_bytes| fsops::copy_to_temp(src, dst, verify, on_bytes))?;
        self.install(op, dst, now, temp)
    }

    /// Puts a finished temp copy in place of `dst` (whose state before the
    /// copy was `now`), disposing of the version it replaces.
    pub(crate) fn install(&mut self, op: &SyncOp, dst: &Path, now: Option<Snap>, temp: fsops::TempCopy) -> Result<Done, OpError> {
        // Nothing there: one atomic rename and we're done.
        if now.is_none() {
            if let Err(e) = fs::rename(&temp.tmp, dst) {
                temp.discard();
                return Err(OpError::io(dst, e));
            }
            return Ok(self.copied(dst, temp.bytes, None, None));
        }

        // Replace: move the old version aside (journaled), swap the new one
        // in, then dispose of the old one. A crash at any point leaves either
        // the old file in place or the journal pointing at it.
        let staged = match fsops::stage_aside(dst) {
            Ok(p) => p,
            Err(e) => {
                temp.discard();
                return Err(OpError::io(dst, e));
            }
        };
        self.journal.backup_staged(op.id, &staged);
        if let Err(e) = fs::rename(&temp.tmp, dst) {
            temp.discard();
            return Err(match fs::rename(&staged, dst) {
                Ok(()) => OpError::io(dst, e),
                Err(e2) => OpError::new(
                    FailureKind::Other,
                    format!("{e}; restoring the old version also failed ({e2}); it is at {}", staged.display()),
                ),
            });
        }
        let (backup, note) = match self.deleter.retire(&staged, side_label(op.direction), op.dst_rel()) {
            Ok(b) => (b, None),
            Err(e) => (
                Some(staged.clone()),
                Some(format!("copied, but the old version couldn't be disposed of ({e}); it was kept as {}", staged.display())),
            ),
        };
        Ok(self.copied(dst, temp.bytes, backup, note))
    }

    fn copied(&self, dst: &Path, bytes: u64, backup: Option<PathBuf>, note: Option<String>) -> Done {
        Done {
            bytes,
            backup,
            written: fsops::snap(dst).ok().flatten(),
            note,
        }
    }

    fn record(&mut self, report: &mut ExecReport, op: &SyncOp, outcome: Result<Done, OpError>, attempts: u32, deferred: bool) {
        let result = match outcome {
            Ok(done) => {
                report.succeeded += 1;
                report.bytes_copied += done.bytes;
                self.state.bytes_done += done.bytes;
                OpResult {
                    id: op.id,
                    kind: op.kind,
                    direction: op.direction,
                    rel_path: op.rel_path.clone(),
                    ok: true,
                    error: done.note,
                    failure: None,
                    attempts,
                    bytes: done.bytes,
                    locked_by: Vec::new(),
                    backup: done.backup.map(|p| p.to_string_lossy().into_owned()),
                    written: done.written,
                    deferred,
                }
            }
            Err(e) => {
                report.failed += 1;
                self.state.failed += 1;
                let locked_by = match (&e.kind, &e.path) {
                    (FailureKind::Locked | FailureKind::AccessDenied, Some(p)) => crate::failure::lock_holders(p),
                    _ => Vec::new(),
                };
                OpResult {
                    id: op.id,
                    kind: op.kind,
                    direction: op.direction,
                    rel_path: op.rel_path.clone(),
                    ok: false,
                    error: Some(e.message),
                    failure: Some(e.kind),
                    attempts,
                    bytes: 0,
                    locked_by,
                    backup: None,
                    written: None,
                    deferred,
                }
            }
        };
        self.journal.op_finished(&result);
        report.results.push(result);
        self.state.done += 1;
    }
}

/// Performs `ops` (in order) against two local folders.
///
/// `progress` is called before each operation, while large files copy, when
/// waiting to retry, and once at the end (`phase == Finished`). Setting
/// `cancel` stops the run at the next safe point; a copy in flight is
/// abandoned and its temp file removed, leaving the destination untouched.
#[allow(clippy::too_many_arguments)]
pub fn execute(
    left: &Path,
    right: &Path,
    ops: &[SyncOp],
    opts: &ExecOptions,
    deleter: &dyn Deleter,
    journal: &mut dyn Journal,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(&ExecProgress),
) -> ExecReport {
    execute_on(Root::Local(left), Root::Local(right), ops, opts, deleter, journal, cancel, progress)
}

/// [`execute`] where either side may be a remote folder (see
/// [`crate::remote`]). `deleter` disposes of items on local sides only;
/// remote deletions are permanent.
#[allow(clippy::too_many_arguments)]
pub fn execute_on(
    left: Root,
    right: Root,
    ops: &[SyncOp],
    opts: &ExecOptions,
    deleter: &dyn Deleter,
    journal: &mut dyn Journal,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(&ExecProgress),
) -> ExecReport {
    let never = AtomicBool::new(false);
    execute_ctl(left, right, ops, opts, deleter, journal, cancel, &never, progress)
}

/// [`execute_on`] that can also be paused: while `pause` is set the run
/// holds between operations and inside large copies, reporting
/// [`Phase::Paused`], and continues where it stopped once it is cleared
/// (bc5:file-ops).
#[allow(clippy::too_many_arguments)]
pub fn execute_ctl(
    left: Root,
    right: Root,
    ops: &[SyncOp],
    opts: &ExecOptions,
    deleter: &dyn Deleter,
    journal: &mut dyn Journal,
    cancel: &AtomicBool,
    pause: &AtomicBool,
    progress: &mut dyn FnMut(&ExecProgress),
) -> ExecReport {
    let started = Instant::now();
    let mut report = ExecReport::default();
    let mut r = Runner {
        left,
        right,
        opts,
        deleter,
        journal,
        cancel,
        pause,
        progress,
        state: ExecProgress {
            total: ops.len(),
            bytes_total: ops.iter().filter(|o| o.kind == OpKind::Copy).map(|o| o.size).sum(),
            ..Default::default()
        },
        last_tick: None,
        partials: HashMap::new(),
    };

    let mut deferred: Vec<(&SyncOp, OpError, u32)> = Vec::new();
    let mut next = 0;
    while next < ops.len() {
        let op = &ops[next];
        r.state.current = Some(op.rel_path.clone());
        r.emit();
        if r.cancelled() || !r.hold() {
            break;
        }
        next += 1;
        let (outcome, attempts) = r.run_with_retries(op);
        match outcome {
            Err(e) if e.kind == FailureKind::Locked && opts.on_locked == LockedPolicy::Defer => {
                r.state.deferred += 1;
                deferred.push((op, e, attempts));
                continue;
            }
            Err(e) => {
                let stop = match e.kind {
                    FailureKind::Cancelled => None,
                    k if k.is_fatal_for_run() => Some(format!("{}: {}", k.label(), e.message)),
                    FailureKind::Locked if opts.on_locked == LockedPolicy::Stop => {
                        Some(format!("{} is in use by another program", op.rel_path))
                    }
                    _ if opts.on_error == ErrorPolicy::Stop => Some(format!("{} failed: {}", op.rel_path, e.message)),
                    _ => None,
                };
                r.record(&mut report, op, Err(e), attempts, false);
                if stop.is_some() {
                    report.stopped = stop;
                    break;
                }
            }
            ok => r.record(&mut report, op, ok, attempts, false),
        }
    }

    // Second chance for locked files: by now the other program may be done.
    r.state.phase = Phase::Deferred;
    for (op, first_error, first_attempts) in deferred {
        r.state.deferred -= 1;
        if r.cancelled() || report.stopped.is_some() {
            r.record(&mut report, op, Err(first_error), first_attempts, true);
            continue;
        }
        r.state.current = Some(op.rel_path.clone());
        r.emit();
        if !r.hold() {
            r.record(&mut report, op, Err(first_error), first_attempts, true);
            continue;
        }
        let (outcome, attempts) = r.run_with_retries(op);
        r.record(&mut report, op, outcome, first_attempts + attempts, true);
    }

    report.not_run = ops[next..].iter().map(|o| o.id).collect();
    report.cancelled = r.cancelled() && (!report.not_run.is_empty() || report.results.iter().any(|x| x.failure == Some(FailureKind::Cancelled)));
    r.journal.flush();
    r.state.current = None;
    r.state.phase = Phase::Finished;
    r.state.attempt = 0;
    r.emit();
    report.elapsed_ms = started.elapsed().as_millis();
    report
}

// ---------- pre-flight ----------

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PreflightIssue {
    /// The sync can't run until this is fixed.
    pub blocking: bool,
    pub message: String,
}

/// Free space beyond the bytes to copy that a side should keep (temp files,
/// file-system metadata, the old versions staged during replacement).
const SPACE_MARGIN: u64 = 64 << 20;

/// Checks what can be known before touching anything: both folders exist,
/// neither contains the other, every side that will be written to accepts
/// writes, and each has room for what is about to be copied there.
pub fn preflight(left: &Path, right: &Path, ops: &[SyncOp]) -> Vec<PreflightIssue> {
    let mut issues = Vec::new();
    let block = |issues: &mut Vec<PreflightIssue>, message: String| {
        issues.push(PreflightIssue { blocking: true, message })
    };
    for (name, root) in [("left", left), ("right", right)] {
        if !root.is_dir() {
            block(&mut issues, format!("The {name} folder {} doesn't exist or isn't reachable.", root.display()));
        }
    }
    if !issues.is_empty() {
        return issues;
    }
    if let (Ok(l), Ok(r)) = (fsops::canonical_lenient(left), fsops::canonical_lenient(right)) {
        if l == r {
            block(&mut issues, "Both sides are the same folder.".into());
        } else if l.starts_with(&r) || r.starts_with(&l) {
            block(&mut issues, "One folder is inside the other; syncing them would copy the folder into itself.".into());
        }
    }

    for (name, root, dir) in [("left", left, Direction::ToLeft), ("right", right, Direction::ToRight)] {
        let incoming: Vec<&SyncOp> = ops.iter().filter(|o| o.direction == dir).collect();
        local_side_issues(name, root, &incoming, &mut issues);
    }
    issues
}

/// Write access and free space of a local side about to receive `incoming`.
pub(crate) fn local_side_issues(name: &str, root: &Path, incoming: &[&SyncOp], issues: &mut Vec<PreflightIssue>) {
    if incoming.is_empty() {
        return;
    }
    let probe = root.join(format!(".pcompare-probe-{}", std::process::id()));
    match fs::File::create(&probe) {
        Ok(_) => {
            let _ = fs::remove_file(&probe);
        }
        Err(e) => issues.push(PreflightIssue {
            blocking: true,
            message: format!("Can't write to the {name} folder: {e}."),
        }),
    }
    let need: u64 = incoming.iter().filter(|o| o.kind == OpKind::Copy).map(|o| o.size).sum();
    if need == 0 {
        return;
    }
    if let Ok(free) = fs4::available_space(root) {
        if need.saturating_add(SPACE_MARGIN) > free {
            issues.push(PreflightIssue {
                blocking: false,
                message: format!(
                    "The {name} side may run out of space: {} to copy, {} free. \
                     The sync stops safely if the disk fills up.",
                    human_bytes(need),
                    human_bytes(free)
                ),
            });
        }
    }
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

// ---------- recovery and rollback ----------

/// What [`recover_op`] found for an operation that was in flight when the
/// app died.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase", tag = "outcome")]
pub enum Recovery {
    /// Nothing was staged; at most a temp file was cleaned up.
    Clean,
    /// The new file never made it into place; the old version was put back.
    Restored,
    /// The new file is in place; the old version was disposed of.
    Completed { backup: Option<String> },
    /// The old version couldn't be handled automatically.
    Manual { message: String },
}

/// Repairs the aftermath of a crash during `op`: removes leftover temp
/// files and, if the old destination had been staged aside, either puts it
/// back (the swap never happened) or disposes of it (the swap completed).
pub fn recover_op(left: &Path, right: &Path, op: &SyncOp, staged: Option<&Path>, deleter: &dyn Deleter) -> Recovery {
    let root = match op.direction {
        Direction::ToRight => right,
        Direction::ToLeft => left,
    };
    let Ok(dst) = fsops::resolve(root, op.dst_rel()) else {
        return Recovery::Clean;
    };
    fsops::remove_stale_temps(&dst);
    let Some(staged) = staged.filter(|p| fs::symlink_metadata(p).is_ok()) else {
        return Recovery::Clean;
    };
    if fs::symlink_metadata(&dst).is_err() {
        return match fs::rename(staged, &dst) {
            Ok(()) => Recovery::Restored,
            Err(e) => Recovery::Manual {
                message: format!("{}: the previous version is at {} ({e})", op.rel_path, staged.display()),
            },
        };
    }
    match deleter.retire(staged, side_label(op.direction), op.dst_rel()) {
        Ok(backup) => Recovery::Completed {
            backup: backup.map(|p| p.to_string_lossy().into_owned()),
        },
        Err(e) => Recovery::Manual {
            message: format!("{}: the previous version is at {} ({e})", op.rel_path, staged.display()),
        },
    }
}

/// A successful operation to undo, with what its run recorded.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RollbackItem {
    pub op: SyncOp,
    pub result: OpResult,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RollbackResult {
    pub id: u32,
    pub rel_path: String,
    pub ok: bool,
    pub message: Option<String>,
}

/// Undoes successful operations, newest first: deleted items come back
/// from the versioning folder, replaced files get their old version back,
/// newly copied files and folders are removed. Anything modified since the
/// sync is left alone. Items whose old version went to the Recycle Bin /
/// Trash (or was deleted permanently) can't be restored automatically.
pub fn rollback(left: &Path, right: &Path, items: &[RollbackItem], deleter: &dyn Deleter) -> Vec<RollbackResult> {
    items
        .iter()
        .rev()
        .filter(|it| it.result.ok)
        .map(|it| {
            let outcome = undo_one(left, right, it, deleter);
            RollbackResult {
                id: it.op.id,
                rel_path: it.op.rel_path.clone(),
                ok: outcome.is_ok(),
                message: outcome.err(),
            }
        })
        .collect()
}

pub(crate) fn undo_one(left: &Path, right: &Path, it: &RollbackItem, deleter: &dyn Deleter) -> Result<(), String> {
    let op = &it.op;
    let root = match op.direction {
        Direction::ToRight => right,
        Direction::ToLeft => left,
    };
    let dst = fsops::resolve(root, op.dst_rel()).map_err(|e| e.to_string())?;
    let backup = it.result.backup.as_deref().map(Path::new).filter(|p| fs::symlink_metadata(p).is_ok());
    let unrecoverable = || "the previous version went to the Recycle Bin / Trash; restore it from there".to_string();
    match op.kind {
        OpKind::Mkdir => match fs::remove_dir(&dst) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(_) => Err("kept: the folder isn't empty".into()),
        },
        OpKind::Delete => {
            let Some(backup) = backup else {
                return Err(unrecoverable());
            };
            if fs::symlink_metadata(&dst).is_ok() {
                return Err("something new exists at the original location".into());
            }
            fsops::move_path(backup, &dst).map_err(|e| e.to_string())
        }
        OpKind::Copy => {
            let now = fsops::snap(&dst).map_err(|e| e.to_string())?;
            if now.is_some() && now != it.result.written {
                return Err("kept: the file was modified after the sync".into());
            }
            match (op.overwrite, backup) {
                (true, Some(backup)) => {
                    // Swap the old version back in, keeping ours until it's there.
                    let ours = match now {
                        Some(_) => Some(fsops::stage_aside(&dst).map_err(|e| e.to_string())?),
                        None => None,
                    };
                    if let Err(e) = fsops::move_path(backup, &dst) {
                        if let Some(ours) = &ours {
                            let _ = fs::rename(ours, &dst);
                        }
                        return Err(e.to_string());
                    }
                    if let Some(ours) = ours {
                        fsops::PermanentDeleter.delete(&ours).map_err(|e| e.to_string())?;
                    }
                    Ok(())
                }
                (true, None) => Err(unrecoverable()),
                (false, _) => match now {
                    // The copy still exists on the source side, so this loses nothing.
                    Some(_) => deleter.delete(&dst).map_err(|e| e.to_string()),
                    None => Ok(()),
                },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::baseline::{self, Baseline};
    use crate::fsops::{PermanentDeleter, VersioningDeleter};
    use crate::journal::{MemoryJournal, NullJournal};
    use crate::sync::{plan_sync_local, ConflictPolicy, PlanOptions, SyncMode, SyncPlan, WarningKind};
    use filetime::FileTime;
    use pcompare_core::folder::{self, FolderCompareOptions};
    use std::sync::atomic::AtomicU32;

    const OLD: i64 = 1_600_000_000;
    const NEW: i64 = 1_700_000_000;

    fn write(root: &Path, rel: &str, body: &str, secs: i64) {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, body).unwrap();
        filetime::set_file_mtime(&p, FileTime::from_unix_time(secs, 0)).unwrap();
    }

    fn read(root: &Path, rel: &str) -> String {
        fs::read_to_string(root.join(rel)).unwrap()
    }

    struct Pair {
        _dir: tempfile::TempDir,
        left: PathBuf,
        right: PathBuf,
        base: Baseline,
    }

    impl Pair {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let (left, right) = (dir.path().join("L"), dir.path().join("R"));
            fs::create_dir_all(&left).unwrap();
            fs::create_dir_all(&right).unwrap();
            Pair { _dir: dir, left, right, base: Baseline::default() }
        }

        fn opts() -> FolderCompareOptions {
            FolderCompareOptions { ignore_name_case: false, ..Default::default() }
        }

        fn plan(&self, mode: SyncMode, conflicts: ConflictPolicy) -> SyncPlan {
            let po = PlanOptions { baseline: Some(&self.base), conflicts };
            plan_sync_local(&self.left, &self.right, mode, &Self::opts(), &po).unwrap()
        }

        fn run(&self, ops: &[SyncOp], opts: &ExecOptions, deleter: &dyn Deleter) -> ExecReport {
            execute(&self.left, &self.right, ops, opts, deleter, &mut NullJournal, &AtomicBool::new(false), &mut |_| {})
        }

        /// Records the current state as the last good sync.
        fn snapshot(&mut self) {
            let cmp = folder::compare_folders(&self.left, &self.right, &Self::opts()).unwrap();
            self.base = baseline::update_baseline(&self.base, &cmp.entries);
        }

        fn describe(p: &SyncPlan) -> Vec<String> {
            p.ops
                .iter()
                .map(|o| {
                    let d = if o.direction == Direction::ToRight { ">" } else { "<" };
                    format!("{:?}{d}{}", o.kind, o.rel_path)
                })
                .collect()
        }
    }

    fn fast() -> ExecOptions {
        ExecOptions { retry_delay_ms: 1, retry_max_delay_ms: 2, ..Default::default() }
    }

    /// Both sides hold a.txt, b.txt and docs/c.txt, already in sync.
    fn synced() -> Pair {
        let mut p = Pair::new();
        for root in [&p.left, &p.right] {
            write(root, "a.txt", "a1", OLD);
            write(root, "b.txt", "b1", OLD);
            write(root, "docs/c.txt", "c1", OLD);
        }
        p.snapshot();
        p
    }

    #[test]
    fn two_way_propagates_edits_and_deletions() {
        let p = synced();
        write(&p.left, "a.txt", "a2-left", NEW); // edited on the left
        fs::remove_file(p.right.join("b.txt")).unwrap(); // deleted on the right
        fs::remove_dir_all(p.right.join("docs")).unwrap(); // folder deleted on the right
        write(&p.right, "new.txt", "n", OLD); // created on the right

        let plan = p.plan(SyncMode::TwoWay, ConflictPolicy::Skip);
        assert!(plan.has_baseline);
        assert_eq!(
            Pair::describe(&plan),
            ["Copy>a.txt", "Copy<new.txt", "Delete<docs", "Delete<b.txt"]
        );
        let r = p.run(&plan.ops, &fast(), &PermanentDeleter);
        assert_eq!(r.failed, 0, "{:?}", r.results);
        assert_eq!(read(&p.right, "a.txt"), "a2-left");
        assert!(!p.left.join("b.txt").exists() && !p.left.join("docs").exists());
        assert_eq!(read(&p.left, "new.txt"), "n");
    }

    #[test]
    fn two_way_without_baseline_never_deletes() {
        let mut p = synced();
        p.base = Baseline::default();
        fs::remove_file(p.right.join("b.txt")).unwrap();
        let plan = p.plan(SyncMode::TwoWay, ConflictPolicy::Skip);
        assert!(!plan.has_baseline);
        assert_eq!(Pair::describe(&plan), ["Copy>b.txt"], "restored, not deleted");
    }

    #[test]
    fn two_way_conflicts_follow_the_policy() {
        let p = synced();
        write(&p.left, "a.txt", "left edit", NEW);
        write(&p.right, "a.txt", "right edit!", NEW + 60);
        // b.txt: edited on the left, deleted on the right.
        write(&p.left, "b.txt", "b2", NEW);
        fs::remove_file(p.right.join("b.txt")).unwrap();

        let skip = p.plan(SyncMode::TwoWay, ConflictPolicy::Skip);
        assert!(skip.ops.is_empty(), "{:?}", Pair::describe(&skip));
        let conflicts: Vec<_> = skip.skipped.iter().filter(|s| s.conflict).map(|s| s.rel_path.as_str()).collect();
        assert_eq!(conflicts, ["a.txt", "b.txt"]);

        let newer = p.plan(SyncMode::TwoWay, ConflictPolicy::NewerWins);
        assert_eq!(Pair::describe(&newer), ["Copy<a.txt", "Copy>b.txt"], "newer edit; edit beats deletion");

        let right = p.plan(SyncMode::TwoWay, ConflictPolicy::RightWins);
        assert_eq!(Pair::describe(&right), ["Copy<a.txt", "Delete<b.txt"]);
    }

    #[test]
    fn deleted_folder_with_new_file_inside_is_a_conflict() {
        let p = synced();
        fs::remove_dir_all(p.right.join("docs")).unwrap();
        write(&p.left, "docs/fresh.txt", "new", OLD);
        let plan = p.plan(SyncMode::TwoWay, ConflictPolicy::Skip);
        assert!(plan.ops.is_empty(), "{:?}", Pair::describe(&plan));
        assert!(plan.skipped.iter().any(|s| s.rel_path == "docs" && s.conflict));
    }

    #[test]
    fn empty_source_is_flagged_before_a_mirror_wipes_the_target() {
        let p = Pair::new();
        for i in 0..5 {
            write(&p.right, &format!("f{i}.txt"), "x", OLD);
        }
        let plan = p.plan(SyncMode::MirrorLeftToRight, ConflictPolicy::Skip);
        assert_eq!(plan.warnings.len(), 1);
        assert_eq!(plan.warnings[0].kind, WarningKind::EmptySide);

        // Two-way after the left drive "vanished": every file looks deleted.
        let mut p = synced();
        for i in 0..30 {
            write(&p.left, &format!("bulk/{i}.txt"), "x", OLD);
            write(&p.right, &format!("bulk/{i}.txt"), "x", OLD);
        }
        p.snapshot();
        fs::remove_dir_all(&p.left).unwrap();
        fs::create_dir_all(&p.left).unwrap();
        let plan = p.plan(SyncMode::TwoWay, ConflictPolicy::Skip);
        assert!(plan.warnings.iter().any(|w| w.kind == WarningKind::EmptySide), "{:?}", plan.warnings);
    }

    #[test]
    fn target_changed_since_preview_is_left_alone() {
        let p = synced();
        write(&p.left, "a.txt", "left v2", NEW);
        fs::remove_file(p.right.join("b.txt")).unwrap();
        let plan = p.plan(SyncMode::TwoWay, ConflictPolicy::Skip);
        // Meanwhile someone saves new work into both targets.
        write(&p.right, "a.txt", "precious right edit", NEW + 5);
        write(&p.left, "b.txt", "precious left edit", NEW + 5);

        let r = p.run(&plan.ops, &fast(), &PermanentDeleter);
        assert_eq!(r.failed, 2);
        assert!(r.results.iter().all(|x| x.failure == Some(FailureKind::ChangedSincePreview)));
        assert_eq!(read(&p.right, "a.txt"), "precious right edit");
        assert_eq!(read(&p.left, "b.txt"), "precious left edit");
    }

    #[test]
    fn versioning_keeps_old_versions_and_rollback_restores_them() {
        let p = synced();
        let vault = p._dir.path().join("versions");
        let deleter = VersioningDeleter::new(&vault, "run1");
        write(&p.left, "a.txt", "a2", NEW);
        write(&p.left, "fresh/new.txt", "n", OLD);
        fs::remove_file(p.left.join("b.txt")).unwrap();
        let plan = p.plan(SyncMode::MirrorLeftToRight, ConflictPolicy::Skip);
        let mut journal = MemoryJournal::default();
        let r = execute(&p.left, &p.right, &plan.ops, &fast(), &deleter, &mut journal, &AtomicBool::new(false), &mut |_| {});
        assert_eq!(r.failed, 0, "{:?}", r.results);
        assert_eq!(read(&vault, "run1/right/a.txt"), "a1", "replaced version kept");
        assert_eq!(read(&vault, "run1/right/b.txt"), "b1", "deleted file kept");
        assert_eq!(journal.staged.len(), 1, "the overwrite was journaled before the swap");
        assert_eq!(journal.finished.len(), plan.ops.len());

        let items: Vec<_> = plan
            .ops
            .iter()
            .zip(&r.results)
            .map(|(op, res)| RollbackItem { op: op.clone(), result: res.clone() })
            .collect();
        let undo = rollback(&p.left, &p.right, &items, &PermanentDeleter);
        assert!(undo.iter().all(|u| u.ok), "{undo:?}");
        assert_eq!(read(&p.right, "a.txt"), "a1");
        assert_eq!(read(&p.right, "b.txt"), "b1");
        assert!(!p.right.join("fresh").exists(), "created folder removed again");
    }

    #[test]
    fn rollback_keeps_files_edited_after_the_sync() {
        let p = synced();
        write(&p.left, "z.txt", "z", OLD);
        let plan = p.plan(SyncMode::UpdateLeftToRight, ConflictPolicy::Skip);
        let r = p.run(&plan.ops, &fast(), &PermanentDeleter);
        write(&p.right, "z.txt", "edited later", NEW);
        let items = vec![RollbackItem { op: plan.ops[0].clone(), result: r.results[0].clone() }];
        let undo = rollback(&p.left, &p.right, &items, &PermanentDeleter);
        assert!(!undo[0].ok);
        assert_eq!(read(&p.right, "z.txt"), "edited later");
    }

    /// Fails with "resource busy" (a locked file) a given number of times.
    struct Flaky {
        busy_for: AtomicU32,
        kind: io::ErrorKind,
    }

    impl Deleter for Flaky {
        fn delete(&self, path: &Path) -> io::Result<()> {
            let left = self.busy_for.load(Ordering::SeqCst);
            if left > 0 {
                self.busy_for.store(left - 1, Ordering::SeqCst);
                return Err(io::Error::from(self.kind));
            }
            PermanentDeleter.delete(path)
        }
    }

    fn mirror_with_two_deletes() -> (Pair, SyncPlan) {
        let p = synced();
        fs::remove_file(p.left.join("a.txt")).unwrap();
        fs::remove_file(p.left.join("b.txt")).unwrap();
        let plan = p.plan(SyncMode::MirrorLeftToRight, ConflictPolicy::Skip);
        assert_eq!(plan.ops.len(), 2);
        (p, plan)
    }

    #[test]
    fn locked_file_is_retried_then_succeeds() {
        let (p, plan) = mirror_with_two_deletes();
        let flaky = Flaky { busy_for: AtomicU32::new(2), kind: io::ErrorKind::ResourceBusy };
        let r = p.run(&plan.ops, &fast(), &flaky);
        assert_eq!(r.failed, 0);
        assert_eq!(r.results[0].attempts, 3);
    }

    #[test]
    fn locked_file_is_deferred_to_the_end() {
        let (p, plan) = mirror_with_two_deletes();
        // Locked through the first op's 4 attempts, free by the time it comes back.
        let flaky = Flaky { busy_for: AtomicU32::new(4), kind: io::ErrorKind::ResourceBusy };
        let r = p.run(&plan.ops, &fast(), &flaky);
        assert_eq!(r.failed, 0, "{:?}", r.results);
        let order: Vec<_> = r.results.iter().map(|x| (x.rel_path.as_str(), x.deferred)).collect();
        assert_eq!(order, [("b.txt", false), ("a.txt", true)]);
    }

    #[test]
    fn locked_file_skip_and_stop_policies() {
        let (p, plan) = mirror_with_two_deletes();
        let flaky = Flaky { busy_for: AtomicU32::new(4), kind: io::ErrorKind::ResourceBusy };
        let skip = ExecOptions { on_locked: LockedPolicy::Skip, ..fast() };
        let r = p.run(&plan.ops, &skip, &flaky);
        assert_eq!((r.succeeded, r.failed), (1, 1));
        assert_eq!(r.results[0].failure, Some(FailureKind::Locked));

        let (p, plan) = mirror_with_two_deletes();
        let flaky = Flaky { busy_for: AtomicU32::new(4), kind: io::ErrorKind::ResourceBusy };
        let stop = ExecOptions { on_locked: LockedPolicy::Stop, ..fast() };
        let r = p.run(&plan.ops, &stop, &flaky);
        assert!(r.stopped.is_some());
        assert_eq!(r.not_run, [plan.ops[1].id]);
    }

    #[test]
    fn full_disk_stops_the_run() {
        let (p, plan) = mirror_with_two_deletes();
        let full = Flaky { busy_for: AtomicU32::new(1), kind: io::ErrorKind::StorageFull };
        let r = p.run(&plan.ops, &fast(), &full);
        assert_eq!(r.results.len(), 1);
        assert_eq!(r.results[0].failure, Some(FailureKind::DiskFull));
        assert!(r.stopped.as_deref().unwrap().starts_with("Disk full"));
        assert_eq!(r.not_run.len(), 1);
    }

    #[test]
    fn source_changing_mid_copy_is_rejected() {
        let p = Pair::new();
        write(&p.left, "grow.log", "start", OLD);
        let src = p.left.join("grow.log");
        let dst = p.right.join("grow.log");
        let mut appended = false;
        let err = fsops::copy_to_temp(&src, &dst, true, &mut |_| {
            if !appended {
                appended = true;
                let mut f = fs::OpenOptions::new().append(true).open(&src)?;
                io::Write::write_all(&mut f, b" more")?;
            }
            Ok(())
        })
        .unwrap_err();
        assert_eq!(err.kind, FailureKind::SourceChanged);
        assert_eq!(fs::read_dir(&p.right).unwrap().count(), 0, "temp file removed");
    }

    #[test]
    fn recovery_after_a_crash_mid_swap() {
        let p = synced();
        let op = SyncOp {
            id: 0,
            kind: OpKind::Copy,
            direction: Direction::ToRight,
            rel_path: "a.txt".into(),
            is_dir: false,
            size: 2,
            overwrite: true,
            reason: String::new(),
            dst_before: None,
            left_rel_path: None,
        };
        // Crash after staging, before the new file was renamed in.
        let dst = p.right.join("a.txt");
        let staged = fsops::stage_aside(&dst).unwrap();
        fs::write(p.right.join(".a.txt.pcompare-1-1.tmp"), "half").unwrap();
        assert_eq!(recover_op(&p.left, &p.right, &op, Some(&staged), &PermanentDeleter), Recovery::Restored);
        assert_eq!(read(&p.right, "a.txt"), "a1");
        assert_eq!(fs::read_dir(&p.right).unwrap().count(), 3, "temp cleaned: a.txt, b.txt, docs");

        // Crash after the swap, before the old version was disposed of.
        let staged = fsops::stage_aside(&dst).unwrap();
        fs::write(&dst, "new").unwrap();
        let rec = recover_op(&p.left, &p.right, &op, Some(&staged), &PermanentDeleter);
        assert_eq!(rec, Recovery::Completed { backup: None });
        assert!(!staged.exists());
        assert_eq!(read(&p.right, "a.txt"), "new");
    }

    #[test]
    fn preflight_blocks_nested_and_missing_folders() {
        let p = synced();
        let copy = SyncOp {
            id: 0,
            kind: OpKind::Copy,
            direction: Direction::ToRight,
            rel_path: "a.txt".into(),
            is_dir: false,
            size: 1,
            overwrite: false,
            reason: String::new(),
            dst_before: None,
            left_rel_path: None,
        };
        assert!(preflight(&p.left, &p.right, std::slice::from_ref(&copy)).is_empty());
        let nested = p.left.join("docs");
        assert!(preflight(&p.left, &nested, &[]).iter().any(|i| i.blocking));
        let missing = p.left.join("nope");
        assert!(preflight(&missing, &p.right, &[]).iter().any(|i| i.blocking));
        let huge = SyncOp { size: u64::MAX / 2, ..copy };
        let issues = preflight(&p.left, &p.right, &[huge]);
        assert!(issues.iter().any(|i| !i.blocking && i.message.contains("space")), "{issues:?}");
    }

    #[test]
    fn pause_holds_the_run_until_resumed() {
        // bc5:file-ops — Pause / Resume in the sync progress panel.
        let (p, plan) = mirror_with_two_deletes();
        let pause = std::sync::Arc::new(AtomicBool::new(true));
        let release = {
            let pause = pause.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(150));
                pause.store(false, Ordering::Relaxed);
            })
        };
        let started = Instant::now();
        let mut phases = Vec::new();
        let r = execute_ctl(
            Root::Local(&p.left),
            Root::Local(&p.right),
            &plan.ops,
            &fast(),
            &PermanentDeleter,
            &mut NullJournal,
            &AtomicBool::new(false),
            &pause,
            &mut |s| phases.push(s.phase),
        );
        release.join().unwrap();
        assert!(started.elapsed() >= Duration::from_millis(150));
        assert_eq!((r.succeeded, r.failed), (2, 0));
        assert!(phases.contains(&Phase::Paused), "{phases:?}");

        // Cancelling a paused run stops it without running anything.
        let (p, plan) = mirror_with_two_deletes();
        let r = execute_ctl(
            Root::Local(&p.left),
            Root::Local(&p.right),
            &plan.ops,
            &fast(),
            &PermanentDeleter,
            &mut NullJournal,
            &AtomicBool::new(true),
            &AtomicBool::new(true),
            &mut |_| {},
        );
        assert!(r.cancelled);
        assert_eq!(r.not_run.len(), 2);
    }

    #[test]
    fn access_denied_is_retried_only_on_windows() {
        assert!(is_transient(FailureKind::Locked));
        assert_eq!(is_transient(FailureKind::AccessDenied), cfg!(windows));
        assert!(!is_transient(FailureKind::NotFound));
        assert!(!is_transient(FailureKind::ChangedSincePreview));
    }

    #[test]
    fn backoff_doubles_and_caps() {
        let o = ExecOptions::default();
        let ms: Vec<_> = (1..=6).map(|n| backoff(&o, n).as_millis()).collect();
        assert_eq!(ms, [500, 1000, 2000, 4000, 8000, 8000]);
    }
}
