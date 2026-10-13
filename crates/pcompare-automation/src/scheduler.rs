//! The scheduler: one background thread that fires triggers, a small pool
//! of run workers, real-time watchers, whole-run retries, history and
//! notifications.
//!
//! Guarantees:
//! - a task never runs twice at once; triggers that arrive while it runs
//!   are coalesced into a single follow-up run;
//! - at most `AutomationSettings::max_parallel` tasks run at once; the rest
//!   queue in trigger order;
//! - while paused (or for a disabled task) automatic triggers are dropped,
//!   but Run now works;
//! - a run that fails with `retryable` is retried `RunRetry::attempts`
//!   times with doubling delays; only the final attempt is notified;
//! - a jump in the wall clock larger than a few minutes between ticks
//!   (sleep/hibernate) is treated like a restart for catch-up purposes;
//! - every run is recorded in the index (when available) before it starts
//!   and closed when it ends, so a crash leaves an `Interrupted` record.
//!
//! The scheduler does no sync work itself: the app supplies a
//! [`TaskRunner`] (plan → safety checks → journaled execution) and a
//! [`SchedulerHost`] (index, channels, UI events, desktop notifications).
//! Tests drive it with a fake runner and a manual [`Clock`].

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex, MutexGuard, Weak};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use chrono::{DateTime, Local, TimeZone};
use pcompare_core::store::new_id;
use pcompare_index::{AutomationRun, Index, NewRun, RunFinish, RunStatus, RunTrigger, TaskState};
use pcompare_notify::{ChannelStore, FailureLine, Notification, NotifyReason};
use serde::Serialize;

use crate::policy;
use crate::store::{AutomationFile, AutomationSettings, TaskStore};
use crate::task::SyncTask;
use crate::trigger::{self, Trigger};
use crate::watcher::{WatchStatus, Watcher};
use crate::{AutomationError, Result};

/// The host's side of a run: does the actual sync.
pub trait TaskRunner: Send + Sync + 'static {
    /// Plans and executes one run of `task`, blocking until it's done. Must
    /// check `ctx.cancel` regularly (the executor already does between
    /// operations) and report progress through `ctx.progress`.
    fn run(&self, task: &SyncTask, ctx: &RunContext) -> RunOutcome;
}

pub struct RunContext<'a> {
    pub run_id: &'a str,
    pub trigger: RunTrigger,
    pub attempt: u32,
    /// Set by Cancel, by the run timeout, and at shutdown.
    pub cancel: &'a AtomicBool,
    pub progress: &'a (dyn Fn(RunProgress) + Send + Sync),
}

#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum RunPhase {
    /// Scanning and comparing the folders.
    Planning,
    Executing,
}

#[derive(Serialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RunProgress {
    pub phase: RunPhase,
    pub done_ops: usize,
    pub total_ops: usize,
    pub bytes_done: u64,
    pub bytes_total: u64,
    /// The item being worked on.
    pub current: Option<String>,
}

/// How a run went, as the runner reports it.
#[derive(Clone, Debug)]
pub struct RunOutcome {
    pub status: RunStatus,
    pub job_id: Option<String>,
    pub ops_total: usize,
    pub ops_done: usize,
    pub ops_failed: usize,
    pub conflicts: usize,
    pub bytes_copied: u64,
    /// Why it failed / was aborted, or a one-line summary.
    pub message: Option<String>,
    /// The first failures (path + error), at most ~20.
    pub failures: Vec<FailureLine>,
    /// A transient, run-wide failure worth a whole-run retry (side
    /// unreachable, network down, run stopped by a full disk...). Never
    /// set for safety aborts, invalid settings or cancellation.
    pub retryable: bool,
}

impl RunOutcome {
    /// A failed outcome with just a message.
    pub fn failed(message: impl Into<String>, retryable: bool) -> Self {
        RunOutcome {
            status: RunStatus::Failed,
            job_id: None,
            ops_total: 0,
            ops_done: 0,
            ops_failed: 0,
            conflicts: 0,
            bytes_copied: 0,
            message: Some(message.into()),
            failures: Vec::new(),
            retryable,
        }
    }
}

/// What the scheduler needs from the app.
pub trait SchedulerHost: Send + Sync + 'static {
    /// The scan database, if it opened.
    fn index(&self) -> Option<&Index>;
    fn channels(&self) -> &ChannelStore;
    /// A task's status changed (queued, started, progress, finished, next
    /// run moved). Called often during runs; the host throttles if needed.
    fn status_changed(&self, status: &TaskStatus);
    /// A run finished and was recorded (for refreshing history views).
    fn run_finished(&self, task_id: &str, run_id: &str);
    /// Shows an OS notification on this machine.
    fn desktop_notify(&self, n: &Notification);
    /// For notification footers.
    fn host_name(&self) -> String;
    fn app_version(&self) -> String;
}

/// Wall-clock source; tests substitute a manual clock.
pub trait Clock: Send + Sync + 'static {
    fn now(&self) -> DateTime<Local>;
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Local> {
        Local::now()
    }
}

#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum TaskRunState {
    Idle,
    /// Triggered, waiting for a free worker.
    Queued,
    Running,
    /// Failed; the next attempt is scheduled (`retry_at_ms`).
    WaitingToRetry,
    Disabled,
    /// Automations are paused globally.
    Paused,
}

/// Live status of one task, for the UI.
#[derive(Serialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TaskStatus {
    pub task_id: String,
    pub state: TaskRunState,
    pub current_run_id: Option<String>,
    pub current_trigger: Option<RunTrigger>,
    pub attempt: u32,
    pub progress: Option<RunProgress>,
    pub next_run_ms: Option<i64>,
    pub next_trigger: Option<RunTrigger>,
    pub retry_at_ms: Option<i64>,
    /// A follow-up run is queued behind the current one.
    pub rerun_pending: bool,
    pub last_run_ms: Option<i64>,
    pub last_status: Option<RunStatus>,
    pub last_success_ms: Option<i64>,
    pub consecutive_failures: u32,
    /// Real-time watching is active on at least one side.
    pub watching: bool,
    /// Why watching isn't possible (remote side, folder missing...).
    pub watch_error: Option<String>,
}

/// The scheduler never sleeps longer than this, so a wall-clock jump
/// (sleep, hibernate, manual change) is noticed within about this long.
const TICK: Duration = Duration::from_secs(30);
/// A wall-clock move this much beyond the elapsed (monotonic) time counts
/// as a jump: the machine slept or the clock was changed.
const CLOCK_JUMP: Duration = Duration::from_secs(180);
/// How often watches with errors (folder missing, drive unplugged) are set
/// up again.
const WATCH_RETRY_EVERY: Duration = Duration::from_secs(300);
/// At most this many progress updates per second per task reach the host.
const PROGRESS_EVERY: Duration = Duration::from_millis(250);
/// Whole-run retry delays double up to this.
const MAX_RETRY_DELAY_SECS: u64 = 3600;
/// Failures listed in a notification.
const NOTIFY_MAX_FAILURES: usize = 10;

pub struct Scheduler {
    inner: Arc<Inner>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

struct Inner {
    store: Arc<TaskStore>,
    runner: Arc<dyn TaskRunner>,
    host: Arc<dyn SchedulerHost>,
    clock: Arc<dyn Clock>,
    state: Mutex<State>,
    /// Wakes the scheduler thread. Separate from `state` so OS watcher
    /// threads can wake it without touching the state lock.
    signal: Mutex<bool>,
    cv: Condvar,
    /// Keeps status events in the order their snapshots were taken.
    emit_lock: Mutex<()>,
    watcher: Watcher,
    notifier: Mutex<Option<mpsc::Sender<Delivery>>>,
    /// Run workers and the notifier, for a bounded join at shutdown.
    threads: Mutex<Vec<JoinHandle<()>>>,
    /// A periodic watch retry is in progress.
    watch_retry_busy: AtomicBool,
    /// Serializes watch refreshes, so an older task snapshot never undoes
    /// a newer one.
    watch_lock: Mutex<()>,
    /// Scheduler loop iterations, so tests can tell it isn't spinning.
    loops: AtomicUsize,
}

struct State {
    settings: AutomationSettings,
    tasks: BTreeMap<String, Rt>,
    /// Task ids waiting for a worker, in trigger order.
    queue: VecDeque<String>,
    running: usize,
    shutting_down: bool,
}

/// One task's runtime state.
struct Rt {
    task: SyncTask,
    /// Deleted while running: dropped when the run ends.
    deleted: bool,
    /// Run-derived state, kept in memory (and mirrored in the index).
    history: TaskState,
    /// The earliest clock trigger.
    next: Option<(DateTime<Local>, RunTrigger)>,
    /// A one-off startup (or startup catch-up) run.
    startup: Option<(DateTime<Local>, RunTrigger)>,
    queued: Option<Pending>,
    active: Option<Active>,
    /// One coalesced run after the current one.
    follow_up: Option<RunTrigger>,
    retry: Option<Retry>,
}

#[derive(Clone, Copy)]
struct Pending {
    trigger: RunTrigger,
    attempt: u32,
}

struct Active {
    run_id: Option<String>,
    trigger: RunTrigger,
    attempt: u32,
    cancel: Arc<AtomicBool>,
    started: DateTime<Local>,
    progress: Option<RunProgress>,
    last_emit: Option<Instant>,
    timed_out: bool,
    cancelled: bool,
}

#[derive(Clone, Copy)]
struct Retry {
    at: DateTime<Local>,
    /// The attempt number of the retry run.
    attempt: u32,
}

/// A worker to start.
struct Launch {
    task: SyncTask,
    pending: Pending,
    cancel: Arc<AtomicBool>,
}

/// A notification to deliver, on the notifier thread.
struct Delivery {
    task: SyncTask,
    run: AutomationRun,
    outcome: RunOutcome,
    reason: NotifyReason,
    fingerprint: String,
    at_ms: i64,
    /// The in-memory "last notified" before this one, restored if nothing
    /// could be delivered.
    previous: (Option<i64>, Option<String>),
}

impl Rt {
    fn new(task: SyncTask, history: TaskState) -> Rt {
        Rt {
            task,
            deleted: false,
            history,
            next: None,
            startup: None,
            queued: None,
            active: None,
            follow_up: None,
            retry: None,
        }
    }

    fn last_run(&self) -> Option<DateTime<Local>> {
        self.history.last_run_ms.and_then(from_ms)
    }

    /// Recomputes the next clock trigger (none while the task can't run
    /// automatically).
    fn reschedule(&mut self, automatic: bool, now: DateTime<Local>) {
        self.next = None;
        if !automatic {
            return;
        }
        let last = self.last_run();
        self.next = self
            .task
            .triggers
            .iter()
            .filter_map(|t| trigger::next_fire(t, last, now).map(|at| (at, label(t))))
            .min_by_key(|(at, _)| *at);
    }
}

/// The history label of a clock trigger's runs.
fn label(t: &Trigger) -> RunTrigger {
    match t {
        Trigger::Realtime { .. } => RunTrigger::Rescan,
        t => t.run_trigger(),
    }
}

fn from_ms(ms: i64) -> Option<DateTime<Local>> {
    Local.timestamp_millis_opt(ms).single()
}

impl State {
    /// Whether automatic triggers may fire for this task.
    fn automatic(&self, rt: &Rt) -> bool {
        rt.task.enabled && !rt.deleted && !self.settings.paused && !self.shutting_down
    }

    /// Handles a trigger: queues a run, or coalesces it into the current
    /// one. Returns whether anything changed.
    fn fire(&mut self, id: &str, trigger: RunTrigger) -> bool {
        let manual = trigger == RunTrigger::Manual;
        let Some(rt) = self.tasks.get_mut(id) else {
            return false;
        };
        if rt.active.is_some() {
            // Anchored triggers count from the end of this run instead.
            if matches!(trigger, RunTrigger::Interval | RunTrigger::Rescan) {
                return false;
            }
            if rt.follow_up != Some(RunTrigger::Manual) {
                rt.follow_up = Some(trigger);
            }
            return true;
        }
        if let Some(q) = rt.queued.as_mut() {
            if manual && q.trigger != RunTrigger::Manual {
                *q = Pending {
                    trigger,
                    attempt: 1,
                };
                return true;
            }
            return false;
        }
        if rt.retry.is_some() {
            if !manual {
                return false; // the retry is coming anyway
            }
            rt.retry = None;
        }
        rt.queued = Some(Pending {
            trigger,
            attempt: 1,
        });
        self.queue.push_back(id.to_owned());
        true
    }

    fn enqueue_retry(&mut self, id: &str, attempt: u32) {
        if let Some(rt) = self.tasks.get_mut(id) {
            rt.retry = None;
            rt.queued = Some(Pending {
                trigger: RunTrigger::Retry,
                attempt,
            });
            self.queue.push_back(id.to_owned());
        }
    }

    /// Removes queued runs that may no longer start automatically.
    fn drop_automatic_queued(&mut self) {
        let paused = self.settings.paused;
        let mut dropped = Vec::new();
        for (id, rt) in self.tasks.iter_mut() {
            let blocked = paused || !rt.task.enabled || rt.deleted;
            if blocked && rt.queued.is_some_and(|q| q.trigger != RunTrigger::Manual) {
                rt.queued = None;
                dropped.push(id.clone());
            }
            if blocked && rt.follow_up.is_some_and(|t| t != RunTrigger::Manual) {
                rt.follow_up = None;
            }
            if !rt.task.enabled || rt.deleted {
                rt.retry = None;
            }
        }
        self.queue.retain(|id| !dropped.contains(id));
    }

    /// Starts as many queued runs as `max_parallel` allows.
    fn dispatch(&mut self, now: DateTime<Local>) -> Vec<Launch> {
        let mut launches = Vec::new();
        let max = self.settings.max_parallel.max(1) as usize;
        while self.running < max && !self.shutting_down {
            let Some(id) = self.queue.pop_front() else {
                break;
            };
            let Some(rt) = self.tasks.get_mut(&id) else {
                continue;
            };
            let Some(pending) = rt.queued.take() else {
                continue;
            };
            if rt.active.is_some() {
                // Can't happen (queued runs never coexist with an active
                // one), but never overlap a task with itself.
                rt.follow_up.get_or_insert(pending.trigger);
                continue;
            }
            let cancel = Arc::new(AtomicBool::new(false));
            rt.active = Some(Active {
                run_id: None,
                trigger: pending.trigger,
                attempt: pending.attempt,
                cancel: cancel.clone(),
                started: now,
                progress: None,
                last_emit: None,
                timed_out: false,
                cancelled: false,
            });
            self.running += 1;
            launches.push(Launch {
                task: rt.task.clone(),
                pending,
                cancel,
            });
        }
        launches
    }
}

impl Scheduler {
    /// Starts the scheduler thread and watchers for the saved tasks: fires
    /// `OnStartup` triggers (after their delay) and catch-up runs, marks
    /// interrupted runs from a previous process (and notifies about them
    /// per each task's rules).
    pub fn start(
        store: Arc<TaskStore>,
        runner: Arc<dyn TaskRunner>,
        host: Arc<dyn SchedulerHost>,
    ) -> Arc<Scheduler> {
        Self::start_with_clock(store, runner, host, Arc::new(SystemClock))
    }

    pub fn start_with_clock(
        store: Arc<TaskStore>,
        runner: Arc<dyn TaskRunner>,
        host: Arc<dyn SchedulerHost>,
        clock: Arc<dyn Clock>,
    ) -> Arc<Scheduler> {
        let inner = Arc::new_cyclic(|weak: &Weak<Inner>| {
            let weak = weak.clone();
            Inner {
                store,
                runner,
                host,
                clock,
                state: Mutex::new(State {
                    settings: AutomationSettings::default(),
                    tasks: BTreeMap::new(),
                    queue: VecDeque::new(),
                    running: 0,
                    shutting_down: false,
                }),
                signal: Mutex::new(false),
                cv: Condvar::new(),
                emit_lock: Mutex::new(()),
                watcher: Watcher::new(move || {
                    if let Some(inner) = weak.upgrade() {
                        inner.wake();
                    }
                }),
                notifier: Mutex::new(None),
                threads: Mutex::new(Vec::new()),
                watch_retry_busy: AtomicBool::new(false),
                watch_lock: Mutex::new(()),
                loops: AtomicUsize::new(0),
            }
        });

        let (tx, rx) = mpsc::channel::<Delivery>();
        *lock(&inner.notifier) = Some(tx);
        let notifier = {
            let inner = inner.clone();
            thread::Builder::new()
                .name("automation-notify".into())
                .spawn(move || {
                    for d in rx {
                        inner.deliver(d);
                    }
                })
                .expect("spawn automation notifier")
        };
        lock(&inner.threads).push(notifier);

        inner.startup();

        let thread = {
            let inner = inner.clone();
            thread::Builder::new()
                .name("automation-scheduler".into())
                .spawn(move || inner.run_loop())
                .expect("spawn automation scheduler")
        };
        Arc::new(Scheduler {
            inner,
            thread: Mutex::new(Some(thread)),
        })
    }

    /// Re-reads tasks and settings after a save/delete/settings change:
    /// recomputes next runs, starts/stops watchers, applies pause and
    /// `max_parallel`. Running runs are not interrupted (a deleted task's
    /// run finishes; an edited task's next run uses the new settings).
    pub fn reload(&self) {
        self.inner.reload();
    }

    /// Queues a manual run (works for disabled tasks and while paused). If
    /// the task is running, one follow-up run is queued instead.
    pub fn run_now(&self, task_id: &str) -> Result<()> {
        let known = |inner: &Inner| {
            lock(&inner.state)
                .tasks
                .get(task_id)
                .is_some_and(|rt| !rt.deleted)
        };
        if !known(&self.inner) {
            // Saved but not reloaded yet?
            self.inner.reload();
            if !known(&self.inner) {
                return Err(AutomationError::TaskNotFound(task_id.to_owned()));
            }
        }
        let changed = {
            let mut st = lock(&self.inner.state);
            !st.shutting_down && st.fire(task_id, RunTrigger::Manual)
        };
        if changed {
            self.inner.emit(&[task_id.to_owned()]);
            self.inner.wake();
        }
        Ok(())
    }

    /// Cancels the task's current run (if any), a pending retry and a
    /// queued follow-up.
    pub fn cancel(&self, task_id: &str) {
        {
            let mut st = lock(&self.inner.state);
            let st = &mut *st;
            let Some(rt) = st.tasks.get_mut(task_id) else {
                return;
            };
            if let Some(a) = rt.active.as_mut() {
                a.cancelled = true;
                a.cancel.store(true, Ordering::SeqCst);
            }
            rt.retry = None;
            rt.follow_up = None;
            if rt.queued.take().is_some() {
                st.queue.retain(|id| id != task_id);
            }
        }
        self.inner.emit(&[task_id.to_owned()]);
    }

    /// Every task's status, sorted by task name.
    pub fn statuses(&self) -> Vec<TaskStatus> {
        let st = lock(&self.inner.state);
        let mut rts: Vec<&Rt> = st.tasks.values().filter(|rt| !rt.deleted).collect();
        rts.sort_by_key(|rt| rt.task.name.to_lowercase());
        rts.into_iter()
            .map(|rt| self.inner.status_of(&st, rt))
            .collect()
    }

    pub fn status(&self, task_id: &str) -> Option<TaskStatus> {
        let st = lock(&self.inner.state);
        st.tasks
            .get(task_id)
            .filter(|rt| !rt.deleted)
            .map(|rt| self.inner.status_of(&st, rt))
    }

    /// Whether any run is in progress (the app asks before quitting).
    pub fn is_busy(&self) -> bool {
        lock(&self.inner.state).running > 0
    }

    /// Cancels running runs, stops watchers and the thread, waiting at most
    /// `wait` for runs to reach a safe point. Safe to call more than once.
    pub fn shutdown(&self, wait: Duration) {
        let deadline = Instant::now() + wait;
        {
            let mut st = lock(&self.inner.state);
            st.shutting_down = true;
            st.queue.clear();
            for rt in st.tasks.values_mut() {
                rt.queued = None;
                rt.follow_up = None;
                rt.retry = None;
                rt.startup = None;
                rt.next = None;
                if let Some(a) = &rt.active {
                    a.cancel.store(true, Ordering::SeqCst);
                }
            }
        }
        // No new notifications; the notifier drains what's queued and exits.
        drop(lock(&self.inner.notifier).take());
        self.inner.watcher.retain(&HashSet::new());
        self.inner.wake();
        let mut handles: Vec<JoinHandle<()>> = lock(&self.thread).take().into_iter().collect();
        handles.append(&mut lock(&self.inner.threads));
        for h in handles {
            while !h.is_finished() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(10));
            }
            if h.is_finished() {
                let _ = h.join();
            }
            // Otherwise it's left to finish on its own; a run still going
            // when the process exits is marked interrupted next time.
        }
    }
}

impl Drop for Scheduler {
    fn drop(&mut self) {
        self.shutdown(Duration::ZERO);
    }
}

impl Inner {
    fn wake(&self) {
        *lock(&self.signal) = true;
        self.cv.notify_all();
    }

    fn index(&self) -> Option<&Index> {
        self.host.index()
    }

    /// Loads everything at start: interrupted runs, pruning, tasks,
    /// startup and catch-up runs.
    fn startup(self: &Arc<Self>) {
        let file = self.store.load().unwrap_or_else(|e| {
            eprintln!("pcompare-automation: {e}");
            AutomationFile::default()
        });
        let mut prev_states: HashMap<String, TaskState> = HashMap::new();
        let mut interrupted = Vec::new();
        if let Some(index) = self.index() {
            match index.task_states() {
                Ok(states) => {
                    prev_states = states.into_iter().map(|s| (s.task_id.clone(), s)).collect()
                }
                Err(e) => eprintln!("pcompare-automation: can't read task states: {e}"),
            }
            match index.mark_runs_interrupted() {
                Ok(runs) => interrupted = runs,
                Err(e) => eprintln!("pcompare-automation: can't check for interrupted runs: {e}"),
            }
            if let Err(e) = index.prune_runs(file.settings.history_days.max(1)) {
                eprintln!("pcompare-automation: can't prune run history: {e}");
            }
        }

        let now = self.clock.now();
        let mut notify = Vec::new();
        {
            let mut st = lock(&self.state);
            st.settings = file.settings.clone();
            for task in file.tasks {
                let history = self.load_history(&task.id);
                st.tasks.insert(task.id.clone(), Rt::new(task, history));
            }
            let ids: Vec<String> = st.tasks.keys().cloned().collect();
            for id in &ids {
                let automatic = st.automatic(&st.tasks[id]);
                let rt = st.tasks.get_mut(id).expect("listed");
                rt.reschedule(automatic, now);
                if !automatic {
                    continue;
                }
                let last = rt.last_run();
                let missed = rt.task.catch_up_missed
                    && last.is_some_and(|l| {
                        rt.task.triggers.iter().any(|t| trigger::missed(t, l, now))
                    });
                let delay = rt
                    .task
                    .triggers
                    .iter()
                    .filter_map(|t| match t {
                        Trigger::OnStartup { delay_secs } => Some(*delay_secs),
                        _ => None,
                    })
                    .min();
                // A task with both runs once: the catch-up waits for the
                // startup delay (time for the network to come up).
                match (missed, delay) {
                    (true, Some(d)) => {
                        rt.startup = Some((
                            now + chrono::Duration::seconds(i64::from(d)),
                            RunTrigger::CatchUp,
                        ))
                    }
                    (true, None) => {
                        st.fire(id, RunTrigger::CatchUp);
                    }
                    (false, Some(d)) => {
                        rt.startup = Some((
                            now + chrono::Duration::seconds(i64::from(d)),
                            RunTrigger::Startup,
                        ))
                    }
                    (false, None) => {}
                }
            }

            // Notify about the newest interrupted run of each task.
            let mut latest: HashMap<&str, &AutomationRun> = HashMap::new();
            for run in &interrupted {
                latest.insert(run.task_id.as_str(), run);
            }
            for (task_id, run) in latest {
                let Some(rt) = st.tasks.get_mut(task_id) else {
                    continue;
                };
                let prev = prev_states.get(task_id).cloned().unwrap_or_default();
                let outcome = RunOutcome {
                    status: RunStatus::Interrupted,
                    job_id: run.job_id.clone(),
                    ops_total: run.ops_total,
                    ops_done: run.ops_done,
                    ops_failed: run.ops_failed,
                    conflicts: run.conflicts,
                    bytes_copied: run.bytes_copied,
                    message: None,
                    failures: Vec::new(),
                    retryable: false,
                };
                if let Some(d) = plan_delivery(rt, run.clone(), outcome, &prev, now) {
                    notify.push(d);
                }
            }
        }
        for d in notify {
            self.send_delivery(d);
        }
        let ids: Vec<String> = lock(&self.state).tasks.keys().cloned().collect();
        self.emit(&ids);
    }

    fn load_history(&self, task_id: &str) -> TaskState {
        let fresh = || TaskState {
            task_id: task_id.to_owned(),
            ..TaskState::default()
        };
        match self.index().map(|i| i.task_state(task_id)) {
            Some(Ok(s)) => s,
            Some(Err(e)) => {
                eprintln!("pcompare-automation: can't read the state of task {task_id}: {e}");
                fresh()
            }
            None => fresh(),
        }
    }

    fn reload(self: &Arc<Self>) {
        let file = match self.store.load() {
            Ok(file) => file,
            Err(e) => {
                eprintln!("pcompare-automation: {e}");
                return;
            }
        };
        let now = self.clock.now();
        let mut forget = Vec::new();
        let ids = {
            let mut st = lock(&self.state);
            if st.shutting_down {
                return;
            }
            let pause_changed = st.settings.paused != file.settings.paused;
            st.settings = file.settings;
            let mut seen = HashSet::new();
            for task in file.tasks {
                seen.insert(task.id.clone());
                let id = task.id.clone();
                let recompute = match st.tasks.get_mut(&id) {
                    Some(rt) => {
                        let changed = rt.deleted
                            || rt.task.enabled != task.enabled
                            || rt.task.triggers != task.triggers;
                        rt.deleted = false;
                        rt.task = task;
                        changed || pause_changed
                    }
                    None => {
                        let history = self.load_history(&id);
                        st.tasks.insert(id.clone(), Rt::new(task, history));
                        true
                    }
                };
                if recompute {
                    let automatic = st.automatic(&st.tasks[&id]);
                    st.tasks
                        .get_mut(&id)
                        .expect("present")
                        .reschedule(automatic, now);
                }
            }
            let gone: Vec<String> = st
                .tasks
                .keys()
                .filter(|id| !seen.contains(*id))
                .cloned()
                .collect();
            for id in gone {
                let rt = st.tasks.get_mut(&id).expect("listed");
                rt.next = None;
                rt.startup = None;
                if rt.active.is_some() {
                    rt.deleted = true;
                } else {
                    st.tasks.remove(&id);
                    forget.push(id);
                }
            }
            st.queue.retain(|id| seen.contains(id));
            st.drop_automatic_queued();
            st.tasks.keys().cloned().collect::<Vec<_>>()
        };
        if let Some(index) = self.index() {
            for id in &forget {
                if let Err(e) = index.forget_task(id) {
                    eprintln!("pcompare-automation: can't forget task {id}: {e}");
                }
            }
        }
        self.refresh_watches(true);
        self.emit(&ids);
        self.wake();
    }

    /// Brings the watches in line with the tasks (outside the state lock;
    /// see the watcher docs). `retry` also rebuilds broken watches.
    fn refresh_watches(&self, retry: bool) {
        let _serial = lock(&self.watch_lock);
        let tasks: Vec<SyncTask> = {
            let st = lock(&self.state);
            if st.shutting_down {
                return;
            }
            st.tasks
                .values()
                .filter(|rt| {
                    rt.task.enabled && !rt.deleted && Watcher::realtime(&rt.task).is_some()
                })
                .map(|rt| rt.task.clone())
                .collect()
        };
        let before: HashMap<String, Option<WatchStatus>> = tasks
            .iter()
            .map(|t| (t.id.clone(), self.watcher.status(&t.id)))
            .collect();
        self.watcher
            .retain(&tasks.iter().map(|t| t.id.clone()).collect());
        for task in &tasks {
            self.watcher.ensure(task, retry);
        }
        if lock(&self.state).shutting_down {
            // Shut down meanwhile: don't leave watches behind.
            self.watcher.retain(&HashSet::new());
            return;
        }
        let changed: Vec<String> = tasks
            .iter()
            .filter(|t| before.get(&t.id).cloned().flatten() != self.watcher.status(&t.id))
            .map(|t| t.id.clone())
            .collect();
        self.emit(&changed);
    }

    fn run_loop(self: Arc<Self>) {
        // Watches are set up on their own thread rather than in `start`:
        // an unreachable share can take a while to answer.
        self.spawn_watch_retry();
        let mut last_wall = self.clock.now();
        let mut last_mono = Instant::now();
        let mut last_watch_retry = Instant::now();
        loop {
            self.loops.fetch_add(1, Ordering::Relaxed);
            let now = self.clock.now();
            let mono = Instant::now();
            let wall = (now - last_wall).to_std().ok();
            let elapsed = mono - last_mono;
            // Forward: the wall clock moved further than we could have slept
            // (this loop never waits longer than TICK). Backward: it went back.
            let jumped = match wall {
                Some(w) => w > elapsed.min(TICK + Duration::from_secs(5)) + CLOCK_JUMP,
                None => (last_wall - now)
                    .to_std()
                    .is_ok_and(|back| back > CLOCK_JUMP),
            };
            last_wall = now;
            last_mono = mono;

            let (launches, timeout, mut changed) = {
                let mut st = lock(&self.state);
                if st.shutting_down {
                    return;
                }
                let mut changed = Vec::new();
                if jumped {
                    changed.extend(self.after_clock_jump(&mut st, now));
                }
                changed.extend(self.fire_due(&mut st, now, mono));
                let launches = st.dispatch(now);
                changed.extend(launches.iter().map(|l| l.task.id.clone()));
                (launches, self.sleep_for(&st, now, mono), changed)
            };
            for l in launches {
                self.launch(l);
            }
            changed.sort();
            changed.dedup();
            self.emit(&changed);

            if jumped || last_watch_retry.elapsed() >= WATCH_RETRY_EVERY {
                last_watch_retry = Instant::now();
                self.spawn_watch_retry();
            }

            let mut signal = lock(&self.signal);
            if !*signal {
                signal = self
                    .cv
                    .wait_timeout(signal, timeout)
                    .map(|r| r.0)
                    .unwrap_or_else(|e| e.into_inner().0);
            }
            *signal = false;
        }
    }

    /// The machine slept or the clock was changed: catch up on what was
    /// missed (once per task, as `CatchUp`) and recompute every next run.
    fn after_clock_jump(&self, st: &mut State, now: DateTime<Local>) -> Vec<String> {
        let ids: Vec<String> = st.tasks.keys().cloned().collect();
        let mut changed = Vec::new();
        for id in ids {
            let rt = &st.tasks[&id];
            let automatic = st.automatic(rt);
            let due = rt.next.is_some_and(|(at, _)| at <= now);
            let missed = due
                || rt
                    .last_run()
                    .is_some_and(|l| rt.task.triggers.iter().any(|t| trigger::missed(t, l, now)));
            let catch_up = automatic && missed && rt.task.catch_up_missed;
            if catch_up && st.fire(&id, RunTrigger::CatchUp) {
                changed.push(id.clone());
            }
            st.tasks
                .get_mut(&id)
                .expect("listed")
                .reschedule(automatic, now);
            changed.push(id);
        }
        changed
    }

    /// Fires clock, startup, retry and real-time triggers that are due, and
    /// enforces run time limits.
    fn fire_due(&self, st: &mut State, now: DateTime<Local>, mono: Instant) -> Vec<String> {
        let mut changed = Vec::new();
        let ids: Vec<String> = st.tasks.keys().cloned().collect();
        let paused = st.settings.paused;
        for id in &ids {
            let automatic = st.automatic(&st.tasks[id]);
            let rt = st.tasks.get_mut(id).expect("listed");
            if let Some(a) = rt.active.as_mut() {
                let limit = rt
                    .task
                    .safety
                    .timeout_minutes
                    .map(|m| chrono::Duration::minutes(i64::from(m)));
                if limit.is_some_and(|l| now - a.started >= l) && !a.timed_out {
                    a.timed_out = true;
                    a.cancel.store(true, Ordering::SeqCst);
                }
            }
            // A retry continues a run that already started, so it also
            // happens for a disabled task's Run now (not while paused).
            if let Some(r) = rt.retry.filter(|r| r.at <= now && !paused && !rt.deleted) {
                st.enqueue_retry(id, r.attempt);
                changed.push(id.clone());
            }
            if !automatic {
                continue;
            }
            let rt = st.tasks.get_mut(id).expect("listed");
            let mut fired = Vec::new();
            if let Some((_, trigger)) = rt.next.filter(|(at, _)| *at <= now) {
                fired.push(trigger);
                rt.reschedule(true, now);
                if rt.next.is_some_and(|(at, _)| at <= now) {
                    // A never-run task's interval is due "now" until it has
                    // run; the end of that run sets the next time.
                    rt.next = None;
                }
                changed.push(id.clone());
            }
            if let Some((_, trigger)) = rt.startup.filter(|(at, _)| *at <= now) {
                rt.startup = None;
                fired.push(trigger);
                changed.push(id.clone());
            }
            for trigger in fired {
                st.fire(id, trigger);
            }
        }
        for id in self.watcher.take_due(mono) {
            let ok = st.tasks.get(&id).is_some_and(|rt| st.automatic(rt));
            if ok && st.fire(&id, RunTrigger::Realtime) {
                changed.push(id);
            }
        }
        changed
    }

    /// How long the loop may sleep: until the next due time, at most TICK.
    fn sleep_for(&self, st: &State, now: DateTime<Local>, mono: Instant) -> Duration {
        let mut wall: Vec<DateTime<Local>> = Vec::new();
        for rt in st.tasks.values() {
            if st.automatic(rt) {
                wall.extend(rt.next.map(|n| n.0));
                wall.extend(rt.startup.map(|n| n.0));
            }
            if !st.settings.paused {
                wall.extend(rt.retry.map(|r| r.at));
            }
            if let (Some(a), Some(m)) = (&rt.active, rt.task.safety.timeout_minutes) {
                if !a.timed_out {
                    wall.push(a.started + chrono::Duration::minutes(i64::from(m)));
                }
            }
        }
        let mut sleep = TICK;
        if let Some(at) = wall.into_iter().min() {
            sleep = sleep.min((at - now).to_std().unwrap_or(Duration::ZERO));
        }
        if let Some(at) = self.watcher.next_due() {
            sleep = sleep.min(at.saturating_duration_since(mono));
        }
        // Wall-clock times are in milliseconds; don't spin just short of one.
        sleep.max(Duration::from_millis(5))
    }

    fn spawn_watch_retry(self: &Arc<Self>) {
        if self.watch_retry_busy.swap(true, Ordering::SeqCst) {
            return;
        }
        let inner = self.clone();
        let spawned = thread::Builder::new()
            .name("automation-watch".into())
            .spawn(move || {
                inner.refresh_watches(true);
                inner.watch_retry_busy.store(false, Ordering::SeqCst);
            });
        match spawned {
            Ok(h) => {
                let mut threads = lock(&self.threads);
                threads.retain(|h| !h.is_finished());
                threads.push(h);
            }
            Err(_) => self.watch_retry_busy.store(false, Ordering::SeqCst),
        }
    }

    fn launch(self: &Arc<Self>, l: Launch) {
        let inner = self.clone();
        let id = l.task.id.clone();
        let spawned = thread::Builder::new()
            .name(format!("automation-run-{id}"))
            .spawn(move || inner.run_one(l));
        let mut threads = lock(&self.threads);
        threads.retain(|h| !h.is_finished());
        match spawned {
            Ok(h) => threads.push(h),
            Err(e) => {
                drop(threads);
                eprintln!("pcompare-automation: can't start a run of task {id}: {e}");
                // Release the slot; the run is recorded as failed.
                let mut st = lock(&self.state);
                st.running = st.running.saturating_sub(1);
                if let Some(rt) = st.tasks.get_mut(&id) {
                    rt.active = None;
                }
            }
        }
    }

    /// One run, on its worker thread.
    fn run_one(self: Arc<Self>, l: Launch) {
        let Launch {
            task,
            pending,
            cancel,
        } = l;
        let run_id = match self.index().map(|i| {
            i.start_run(&NewRun {
                task_id: &task.id,
                task_name: &task.name,
                trigger: pending.trigger,
                attempt: pending.attempt,
            })
        }) {
            Some(Ok(id)) => Some(id),
            Some(Err(e)) => {
                eprintln!(
                    "pcompare-automation: can't record the start of a run of {}: {e}",
                    task.name
                );
                None
            }
            None => None,
        };
        let ctx_run_id = run_id.clone().unwrap_or_else(new_id);
        if let Some(a) = lock(&self.state)
            .tasks
            .get_mut(&task.id)
            .and_then(|rt| rt.active.as_mut())
        {
            a.run_id = Some(ctx_run_id.clone());
        }
        self.emit(std::slice::from_ref(&task.id));
        self.watcher.set_running(&task.id, true);

        let progress = |p: RunProgress| self.on_progress(&task.id, p);
        let ctx = RunContext {
            run_id: &ctx_run_id,
            trigger: pending.trigger,
            attempt: pending.attempt,
            cancel: &cancel,
            progress: &progress,
        };
        let runner = self.runner.clone();
        let outcome = panic::catch_unwind(AssertUnwindSafe(|| runner.run(&task, &ctx)))
            .unwrap_or_else(|_| {
                RunOutcome::failed(
                    "The sync stopped because of an internal error in PCompare.",
                    false,
                )
            });
        self.watcher.set_running(&task.id, false);
        self.finish(&task, pending, run_id, ctx_run_id, outcome);
    }

    fn on_progress(&self, task_id: &str, p: RunProgress) {
        let emit = {
            let mut st = lock(&self.state);
            let Some(a) = st.tasks.get_mut(task_id).and_then(|rt| rt.active.as_mut()) else {
                return;
            };
            a.progress = Some(p);
            let due = a.last_emit.is_none_or(|t| t.elapsed() >= PROGRESS_EVERY);
            if due {
                a.last_emit = Some(Instant::now());
            }
            due
        };
        if emit {
            self.emit(&[task_id.to_owned()]);
        }
    }

    /// Records a finished run, schedules a retry or follow-up, and hands
    /// the final attempt to the notifier.
    fn finish(
        self: &Arc<Self>,
        task: &SyncTask,
        pending: Pending,
        run_id: Option<String>,
        ctx_run_id: String,
        mut outcome: RunOutcome,
    ) {
        let now = self.clock.now();
        let (final_attempt, prev, deleted) = {
            let mut st = lock(&self.state);
            let st = &mut *st;
            st.running = st.running.saturating_sub(1);
            let shutting_down = st.shutting_down;
            let paused = st.settings.paused;
            let Some(rt) = st.tasks.get_mut(&task.id) else {
                return;
            };
            let active = rt.active.take();
            let (timed_out, cancelled) = active
                .as_ref()
                .map_or((false, false), |a| (a.timed_out, a.cancelled));
            if outcome.status == RunStatus::Running {
                outcome.status = RunStatus::Failed;
            }
            // A runner that noticed the limit itself may already have said
            // so (status Failed); only a plain cancellation is reworded.
            if timed_out && !cancelled && outcome.status == RunStatus::Cancelled {
                let after = match task.safety.timeout_minutes.unwrap_or(0) {
                    1 => "1 minute".to_owned(),
                    m => format!("{m} minutes"),
                };
                outcome.status = RunStatus::Failed;
                outcome.message = Some(format!(
                    "Timed out after {after}; the sync was stopped at a safe point and the next run picks up the rest."
                ));
            }
            if timed_out || cancelled || shutting_down {
                outcome.retryable = false;
            }
            let prev = rt.history.clone();
            rt.history.record(outcome.status, now.timestamp_millis());
            let can_retry = outcome.retryable
                && outcome.status.is_failure()
                && pending.attempt <= task.retry.attempts
                && !rt.deleted;
            let final_attempt = !can_retry;
            if can_retry {
                let shift = (pending.attempt.saturating_sub(1)).min(20);
                let delay = task
                    .retry
                    .delay_secs
                    .max(1)
                    .saturating_mul(1u64 << shift)
                    .min(MAX_RETRY_DELAY_SECS);
                rt.retry = Some(Retry {
                    at: now + chrono::Duration::seconds(delay as i64),
                    attempt: pending.attempt + 1,
                });
                // The retry is a full run; it covers any follow-up.
                rt.follow_up = None;
            }
            let automatic = rt.task.enabled && !rt.deleted && !paused && !shutting_down;
            // A schedule that came due just now and hasn't been seen by the
            // scheduler thread yet still counts (anchored ones restart from
            // this run's end instead).
            let due = rt.next.filter(|(at, t)| {
                automatic && *at <= now && !matches!(t, RunTrigger::Interval | RunTrigger::Rescan)
            });
            if let (Some((_, t)), None) = (due, rt.retry) {
                rt.follow_up.get_or_insert(t);
            }
            let follow_up = rt.follow_up.take();
            rt.reschedule(automatic, now);
            let deleted = rt.deleted;
            if deleted {
                st.tasks.remove(&task.id);
            } else if let Some(trigger) =
                follow_up.filter(|t| automatic || *t == RunTrigger::Manual)
            {
                st.fire(&task.id, trigger);
            }
            (final_attempt && !shutting_down, prev, deleted)
        };

        let finish = RunFinish {
            status: Some(outcome.status),
            job_id: outcome.job_id.clone(),
            ops_total: outcome.ops_total,
            ops_done: outcome.ops_done,
            ops_failed: outcome.ops_failed,
            conflicts: outcome.conflicts,
            bytes_copied: outcome.bytes_copied,
            message: outcome.message.clone(),
        };
        let recorded = match (self.index(), &run_id) {
            (Some(index), Some(id)) => match index.finish_run(id, &finish) {
                Ok(run) => Some(run),
                Err(e) => {
                    eprintln!("pcompare-automation: can't record the end of run {id}: {e}");
                    None
                }
            },
            _ => None,
        };
        if deleted {
            if let Some(index) = self.index() {
                let _ = index.forget_task(&task.id);
            }
        }
        let run = recorded.unwrap_or_else(|| AutomationRun {
            id: ctx_run_id.clone(),
            task_id: task.id.clone(),
            task_name: task.name.clone(),
            trigger: pending.trigger,
            attempt: pending.attempt,
            status: outcome.status,
            started_ms: now.timestamp_millis(),
            finished_ms: Some(now.timestamp_millis()),
            job_id: outcome.job_id.clone(),
            ops_total: outcome.ops_total,
            ops_done: outcome.ops_done,
            ops_failed: outcome.ops_failed,
            conflicts: outcome.conflicts,
            bytes_copied: outcome.bytes_copied,
            message: outcome.message.clone(),
            notifications: Vec::new(),
        });
        self.host.run_finished(&task.id, &run.id);

        if final_attempt && !deleted {
            let delivery = {
                let mut st = lock(&self.state);
                st.tasks
                    .get_mut(&task.id)
                    .and_then(|rt| plan_delivery(rt, run, outcome, &prev, now))
            };
            if let Some(d) = delivery {
                self.send_delivery(d);
            }
        }
        self.emit(std::slice::from_ref(&task.id));
        self.wake();
    }

    fn send_delivery(&self, d: Delivery) {
        if let Some(tx) = lock(&self.notifier).as_ref() {
            let _ = tx.send(d);
        }
    }

    /// Delivers one notification to the desktop and every channel the task
    /// lists, and records the results. Runs on the notifier thread.
    fn deliver(&self, d: Delivery) {
        let host = &self.host;
        let rules = &d.task.notify;
        let n = policy::build_notification(
            &d.task,
            &d.run,
            &d.outcome,
            d.reason,
            &host.host_name(),
            &host.app_version(),
            NOTIFY_MAX_FAILURES,
        );
        let mut delivered = false;
        let mut results = Vec::new();
        if rules.desktop {
            delivered |= panic::catch_unwind(AssertUnwindSafe(|| host.desktop_notify(&n))).is_ok();
        }
        for channel_id in &rules.channels {
            let line = match host.channels().get(channel_id) {
                Ok(Some(ch)) if !ch.enabled => format!("{}: skipped (disabled)", ch.name),
                Ok(Some(ch)) => {
                    let sent = panic::catch_unwind(AssertUnwindSafe(|| {
                        pcompare_notify::send(&ch, host.channels().secrets(), &n)
                    }));
                    match sent {
                        Ok(Ok(())) => {
                            delivered = true;
                            format!("{}: sent", ch.name)
                        }
                        Ok(Err(e)) => format!("{}: {e}", ch.name),
                        Err(_) => format!("{}: internal error while sending", ch.name),
                    }
                }
                Ok(None) => format!("{channel_id}: skipped (channel deleted)"),
                Err(e) => format!("{channel_id}: {e}"),
            };
            results.push(line);
        }
        let index = self.index();
        if let Some(index) = index.filter(|_| !results.is_empty()) {
            if let Err(e) = index.record_notifications(&d.run.id, &results) {
                eprintln!("pcompare-automation: can't record notification results: {e}");
            }
        }
        if delivered {
            if let Some(index) = index {
                if let Err(e) = index.mark_notified(&d.task.id, d.at_ms, &d.fingerprint) {
                    eprintln!("pcompare-automation: can't record a notification: {e}");
                }
            }
        } else {
            // Nothing got through: don't let repeat suppression hide the
            // next attempt.
            let mut st = lock(&self.state);
            if let Some(rt) = st.tasks.get_mut(&d.task.id) {
                let h = &mut rt.history;
                if h.last_notified_ms == Some(d.at_ms)
                    && h.last_notified_fingerprint.as_deref() == Some(d.fingerprint.as_str())
                {
                    (h.last_notified_ms, h.last_notified_fingerprint) = d.previous;
                }
            }
        }
        if !results.is_empty() {
            host.run_finished(&d.task.id, &d.run.id);
        }
    }

    fn status_of(&self, st: &State, rt: &Rt) -> TaskStatus {
        let automatic = st.automatic(rt);
        let state = if rt.active.is_some() {
            TaskRunState::Running
        } else if rt.queued.is_some() {
            TaskRunState::Queued
        } else if rt.retry.is_some() {
            TaskRunState::WaitingToRetry
        } else if !rt.task.enabled {
            TaskRunState::Disabled
        } else if st.settings.paused {
            TaskRunState::Paused
        } else {
            TaskRunState::Idle
        };
        let (current_run_id, current_trigger, attempt, progress) =
            match (&rt.active, &rt.queued, &rt.retry) {
                (Some(a), _, _) => (
                    a.run_id.clone(),
                    Some(a.trigger),
                    a.attempt,
                    a.progress.clone(),
                ),
                (None, Some(q), _) => (None, Some(q.trigger), q.attempt, None),
                (None, None, Some(r)) => (None, None, r.attempt, None),
                _ => (None, None, 0, None),
            };
        let next = if automatic {
            [rt.next, rt.startup]
                .into_iter()
                .flatten()
                .min_by_key(|n| n.0)
        } else {
            None
        };
        let watch = self.watcher.status(&rt.task.id).unwrap_or_default();
        TaskStatus {
            task_id: rt.task.id.clone(),
            state,
            current_run_id,
            current_trigger,
            attempt,
            progress,
            next_run_ms: next.map(|n| n.0.timestamp_millis()),
            next_trigger: next.map(|n| n.1),
            retry_at_ms: rt.retry.map(|r| r.at.timestamp_millis()),
            rerun_pending: rt.follow_up.is_some(),
            last_run_ms: rt.history.last_run_ms,
            last_status: rt.history.last_status,
            last_success_ms: rt.history.last_success_ms,
            consecutive_failures: rt.history.consecutive_failures,
            watching: watch.watching,
            watch_error: watch.error,
        }
    }

    /// Sends the current status of `ids` to the host, outside the state
    /// lock (the host may call back in).
    fn emit(&self, ids: &[String]) {
        if ids.is_empty() {
            return;
        }
        let _order = lock(&self.emit_lock);
        let statuses: Vec<TaskStatus> = {
            let st = lock(&self.state);
            ids.iter()
                .filter_map(|id| st.tasks.get(id))
                .filter(|rt| !rt.deleted)
                .map(|rt| self.status_of(&st, rt))
                .collect()
        };
        for s in &statuses {
            self.host.status_changed(s);
        }
    }
}

/// Decides whether to notify about a final run and, if so, notes it as
/// notified (optimistically; undone if nothing gets through) and returns
/// the delivery.
fn plan_delivery(
    rt: &mut Rt,
    run: AutomationRun,
    outcome: RunOutcome,
    prev: &TaskState,
    now: DateTime<Local>,
) -> Option<Delivery> {
    let rules = &rt.task.notify;
    if rules.channels.is_empty() && !rules.desktop {
        return None;
    }
    let fp = policy::fingerprint(outcome.status, outcome.message.as_deref());
    let now_ms = now.timestamp_millis();
    // Suppression looks at the latest notification, which may be newer
    // than `prev` (an earlier run's notification still being delivered).
    let mut seen = prev.clone();
    seen.last_notified_ms = rt.history.last_notified_ms;
    seen.last_notified_fingerprint = rt.history.last_notified_fingerprint.clone();
    let reason = policy::decide(rules, outcome.status, &seen, &fp, now_ms)?;
    let previous = (
        rt.history.last_notified_ms,
        rt.history.last_notified_fingerprint.clone(),
    );
    rt.history.last_notified_ms = Some(now_ms);
    rt.history.last_notified_fingerprint = Some(fp.clone());
    Some(Delivery {
        task: rt.task.clone(),
        run,
        outcome,
        reason,
        fingerprint: fp,
        at_ms: now_ms,
        previous,
    })
}

/// Locks, ignoring poisoning: every critical section leaves the state
/// consistent before anything that could panic.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::RunRetry;
    use pcompare_core::secrets::MemorySecrets;

    struct ManualClock(Mutex<DateTime<Local>>);

    impl Clock for ManualClock {
        fn now(&self) -> DateTime<Local> {
            *lock(&self.0)
        }
    }

    impl ManualClock {
        fn advance(&self, secs: i64) {
            *lock(&self.0) += chrono::Duration::seconds(secs);
        }
    }

    #[derive(Clone, Debug, PartialEq)]
    struct Call {
        task: String,
        trigger: RunTrigger,
        attempt: u32,
    }

    /// Returns scripted outcomes (default: succeeded); while `hold` is set,
    /// runs block until released or cancelled.
    #[derive(Default)]
    struct FakeRunner {
        calls: Mutex<Vec<Call>>,
        script: Mutex<HashMap<String, VecDeque<RunOutcome>>>,
        hold: Mutex<bool>,
        active: AtomicUsize,
        max_active: AtomicUsize,
        /// Behave like a runner stuck somewhere that doesn't check `cancel`.
        ignore_cancel: AtomicBool,
    }

    impl FakeRunner {
        fn push(&self, task: &str, outcome: RunOutcome) {
            lock(&self.script)
                .entry(task.to_owned())
                .or_default()
                .push_back(outcome);
        }
        fn hold(&self, on: bool) {
            *lock(&self.hold) = on;
        }
        fn calls(&self) -> Vec<Call> {
            lock(&self.calls).clone()
        }
        fn triggers(&self, task: &str) -> Vec<RunTrigger> {
            self.calls()
                .into_iter()
                .filter(|c| c.task == task)
                .map(|c| c.trigger)
                .collect()
        }
    }

    impl TaskRunner for FakeRunner {
        fn run(&self, task: &SyncTask, ctx: &RunContext) -> RunOutcome {
            lock(&self.calls).push(Call {
                task: task.id.clone(),
                trigger: ctx.trigger,
                attempt: ctx.attempt,
            });
            let n = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_active.fetch_max(n, Ordering::SeqCst);
            (ctx.progress)(RunProgress {
                phase: RunPhase::Planning,
                done_ops: 0,
                total_ops: 0,
                bytes_done: 0,
                bytes_total: 0,
                current: None,
            });
            let mut cancelled = false;
            while *lock(&self.hold) {
                if ctx.cancel.load(Ordering::SeqCst) && !self.ignore_cancel.load(Ordering::SeqCst) {
                    cancelled = true;
                    break;
                }
                thread::sleep(Duration::from_millis(5));
            }
            self.active.fetch_sub(1, Ordering::SeqCst);
            if cancelled {
                return RunOutcome {
                    status: RunStatus::Cancelled,
                    message: None,
                    ..RunOutcome::failed("", false)
                };
            }
            lock(&self.script)
                .get_mut(&task.id)
                .and_then(|q| q.pop_front())
                .unwrap_or_else(ok)
        }
    }

    fn ok() -> RunOutcome {
        RunOutcome {
            status: RunStatus::Succeeded,
            message: None,
            ops_total: 1,
            ops_done: 1,
            ..RunOutcome::failed("", false)
        }
    }

    struct FakeHost {
        index: Option<Index>,
        channels: ChannelStore,
        statuses: Mutex<Vec<TaskStatus>>,
        desktop: Mutex<Vec<Notification>>,
        finished: Mutex<Vec<String>>,
    }

    impl SchedulerHost for FakeHost {
        fn index(&self) -> Option<&Index> {
            self.index.as_ref()
        }
        fn channels(&self) -> &ChannelStore {
            &self.channels
        }
        fn status_changed(&self, status: &TaskStatus) {
            lock(&self.statuses).push(status.clone());
        }
        fn run_finished(&self, _task_id: &str, run_id: &str) {
            lock(&self.finished).push(run_id.to_owned());
        }
        fn desktop_notify(&self, n: &Notification) {
            lock(&self.desktop).push(n.clone());
        }
        fn host_name(&self) -> String {
            "TEST-PC".into()
        }
        fn app_version(&self) -> String {
            "0.0.0".into()
        }
    }

    impl FakeHost {
        fn titles(&self) -> Vec<String> {
            lock(&self.desktop)
                .iter()
                .map(|n| n.title.clone())
                .collect()
        }
    }

    struct Rig {
        _dir: tempfile::TempDir,
        store: Arc<TaskStore>,
        runner: Arc<FakeRunner>,
        host: Arc<FakeHost>,
        clock: Arc<ManualClock>,
        sched: Option<Arc<Scheduler>>,
    }

    fn start_at() -> DateTime<Local> {
        Local
            .with_ymd_and_hms(2026, 5, 4, 10, 0, 0)
            .earliest()
            .unwrap()
    }

    impl Rig {
        fn new() -> Rig {
            Self::with_index(Index::open_in_memory().unwrap())
        }

        fn with_index(index: Index) -> Rig {
            let dir = tempfile::tempdir().unwrap();
            let store = Arc::new(TaskStore::new(dir.path().join("automation.json")));
            let channels = ChannelStore::new(
                dir.path().join("notify-channels.json"),
                Box::new(MemorySecrets::default()),
            );
            Rig {
                store,
                runner: Arc::new(FakeRunner::default()),
                host: Arc::new(FakeHost {
                    index: Some(index),
                    channels,
                    statuses: Mutex::new(Vec::new()),
                    desktop: Mutex::new(Vec::new()),
                    finished: Mutex::new(Vec::new()),
                }),
                clock: Arc::new(ManualClock(Mutex::new(start_at()))),
                _dir: dir,
                sched: None,
            }
        }

        /// Saves a task (name must be unique) and returns its id.
        fn add(&self, name: &str, edit: impl FnOnce(&mut SyncTask)) -> String {
            let mut t: SyncTask = serde_json::from_value(serde_json::json!({
                "name": name, "enabled": true,
                "left": format!("/data/{name}"), "right": format!("/backup/{name}"),
                "mode": "mirrorLeftToRight",
                "retry": {"attempts": 0, "delaySecs": 60},
                "notify": {"desktop": true},
            }))
            .unwrap();
            edit(&mut t);
            self.store.save_task(t).unwrap().0.id
        }

        fn settings(&self, edit: impl FnOnce(&mut AutomationSettings)) {
            let mut s = self.store.settings().unwrap();
            edit(&mut s);
            self.store.save_settings(&s).unwrap();
        }

        fn start(&mut self) -> Arc<Scheduler> {
            let s = Scheduler::start_with_clock(
                self.store.clone(),
                self.runner.clone(),
                self.host.clone(),
                self.clock.clone(),
            );
            self.sched = Some(s.clone());
            s
        }

        fn sched(&self) -> &Scheduler {
            self.sched.as_ref().unwrap()
        }

        fn advance(&self, secs: i64) {
            self.clock.advance(secs);
            self.sched().inner.wake();
        }

        fn status(&self, id: &str) -> TaskStatus {
            self.sched().status(id).unwrap()
        }

        fn wait(&self, what: &str, cond: impl Fn(&Rig) -> bool) {
            let deadline = Instant::now() + Duration::from_secs(10);
            while !cond(self) {
                assert!(Instant::now() < deadline, "timed out waiting for {what}");
                thread::sleep(Duration::from_millis(5));
            }
        }

        fn wait_idle(&self, id: &str, runs: usize) {
            self.wait(&format!("{runs} runs of {id}"), |r| {
                r.runner.triggers(id).len() >= runs
                    && r.status(id).state == TaskRunState::Idle
                    && !r.sched().is_busy()
            });
            // Let the notifier catch up.
            thread::sleep(Duration::from_millis(30));
        }

        fn runs(&self, id: &str) -> Vec<AutomationRun> {
            let mut runs = self
                .host
                .index
                .as_ref()
                .unwrap()
                .list_runs(Some(id), 100)
                .unwrap();
            runs.reverse();
            runs
        }
    }

    impl Drop for Rig {
        fn drop(&mut self) {
            if let Some(s) = &self.sched {
                s.shutdown(Duration::from_secs(5));
            }
        }
    }

    #[test]
    fn manual_run_is_recorded() {
        let mut rig = Rig::new();
        let id = rig.add("a", |_| {});
        let s = rig.start();
        assert_eq!(s.status(&id).unwrap().state, TaskRunState::Idle);
        assert_eq!(s.status(&id).unwrap().next_run_ms, None, "no triggers");
        s.run_now(&id).unwrap();
        rig.wait_idle(&id, 1);
        let runs = rig.runs(&id);
        assert_eq!(runs.len(), 1);
        assert_eq!(
            (runs[0].trigger, runs[0].status, runs[0].attempt),
            (RunTrigger::Manual, RunStatus::Succeeded, 1)
        );
        let st = rig.status(&id);
        assert_eq!(st.last_status, Some(RunStatus::Succeeded));
        assert_eq!(st.last_run_ms, Some(start_at().timestamp_millis()));
        assert!(lock(&rig.host.finished).contains(&runs[0].id));
        assert!(
            rig.host.titles().is_empty(),
            "success isn't notified by default"
        );
        assert!(lock(&rig.host.statuses)
            .iter()
            .any(|s| s.state == TaskRunState::Running && s.current_run_id.is_some()));
        assert!(matches!(
            s.run_now("nope"),
            Err(AutomationError::TaskNotFound(_))
        ));
    }

    #[test]
    fn no_overlap_and_triggers_coalesce_into_one_follow_up() {
        let mut rig = Rig::new();
        let id = rig.add("a", |_| {});
        let s = rig.start();
        rig.runner.hold(true);
        s.run_now(&id).unwrap();
        rig.wait("running", |r| r.status(&id).state == TaskRunState::Running);
        for _ in 0..3 {
            s.run_now(&id).unwrap();
        }
        let st = rig.status(&id);
        assert!(st.rerun_pending);
        assert_eq!(rig.runner.calls().len(), 1);
        rig.runner.hold(false);
        rig.wait_idle(&id, 2);
        thread::sleep(Duration::from_millis(50));
        assert_eq!(
            rig.runner.triggers(&id),
            [RunTrigger::Manual, RunTrigger::Manual],
            "exactly one follow-up"
        );
        assert_eq!(rig.runner.max_active.load(Ordering::SeqCst), 1);
        assert!(!rig.status(&id).rerun_pending);
    }

    #[test]
    fn max_parallel_is_respected() {
        let mut rig = Rig::new();
        rig.settings(|s| s.max_parallel = 2);
        let ids: Vec<String> = ["a", "b", "c"].iter().map(|n| rig.add(n, |_| {})).collect();
        let s = rig.start();
        rig.runner.hold(true);
        for id in &ids {
            s.run_now(id).unwrap();
        }
        rig.wait("two running", |r| {
            r.runner.active.load(Ordering::SeqCst) == 2
        });
        thread::sleep(Duration::from_millis(50));
        let states: Vec<TaskRunState> = ids.iter().map(|id| rig.status(id).state).collect();
        assert_eq!(
            states,
            [
                TaskRunState::Running,
                TaskRunState::Running,
                TaskRunState::Queued
            ]
        );
        assert!(s.is_busy());
        rig.runner.hold(false);
        for id in &ids {
            rig.wait_idle(id, 1);
        }
        assert_eq!(rig.runner.max_active.load(Ordering::SeqCst), 2);
        assert_eq!(
            rig.runner.calls()[2].task,
            ids[2],
            "the queued one runs last"
        );
    }

    #[test]
    fn interval_fires_and_counts_from_the_end_of_the_run() {
        let mut rig = Rig::new();
        let id = rig.add("a", |t| {
            t.triggers = vec![Trigger::Interval { every_minutes: 1 }]
        });
        rig.start();
        // Never ran: fires right away.
        rig.wait_idle(&id, 1);
        let st = rig.status(&id);
        assert_eq!(
            st.next_run_ms,
            Some((start_at() + chrono::Duration::minutes(1)).timestamp_millis())
        );
        assert_eq!(st.next_trigger, Some(RunTrigger::Interval));
        rig.advance(30);
        thread::sleep(Duration::from_millis(50));
        assert_eq!(rig.runner.calls().len(), 1, "not due yet");
        rig.advance(30);
        rig.wait_idle(&id, 2);
        assert_eq!(
            rig.runner.triggers(&id),
            [RunTrigger::Interval, RunTrigger::Interval]
        );
    }

    #[test]
    fn the_loop_sleeps_while_runs_are_due_or_overdue() {
        let mut rig = Rig::new();
        let id = rig.add("a", |t| {
            t.triggers = vec![Trigger::Interval { every_minutes: 1 }];
            t.safety.timeout_minutes = Some(1);
        });
        rig.runner.hold(true);
        rig.runner.ignore_cancel.store(true, Ordering::SeqCst);
        let s = rig.start();
        rig.wait("running", |r| r.status(&id).state == TaskRunState::Running);
        let spins = |rig: &Rig| {
            thread::sleep(Duration::from_millis(50));
            let before = s.inner.loops.load(Ordering::Relaxed);
            thread::sleep(Duration::from_millis(300));
            let _ = rig;
            s.inner.loops.load(Ordering::Relaxed) - before
        };
        // A never-run interval is "due now" until its first run ends.
        let n = spins(&rig);
        assert!(n <= 3, "{n} loop iterations in 300 ms");
        // A run past its time limit stays active until the runner returns.
        rig.advance(61);
        let n = spins(&rig);
        assert!(n <= 3, "{n} loop iterations in 300 ms");
        rig.runner.hold(false);
        rig.wait_idle(&id, 1);
    }

    #[test]
    fn pause_drops_automatic_triggers_but_run_now_works() {
        let mut rig = Rig::new();
        rig.settings(|s| s.paused = true);
        let id = rig.add("a", |t| {
            t.triggers = vec![
                Trigger::Interval { every_minutes: 1 },
                Trigger::OnStartup { delay_secs: 0 },
            ]
        });
        let s = rig.start();
        assert_eq!(rig.status(&id).state, TaskRunState::Paused);
        assert_eq!(rig.status(&id).next_run_ms, None);
        rig.advance(120);
        thread::sleep(Duration::from_millis(100));
        assert!(rig.runner.calls().is_empty());
        s.run_now(&id).unwrap();
        rig.wait("manual run", |r| {
            r.runner.calls().len() == 1 && !r.sched().is_busy()
        });
        rig.wait("paused again", |r| {
            r.status(&id).state == TaskRunState::Paused
        });

        // Unpause: the schedule resumes.
        rig.settings(|s| s.paused = false);
        s.reload();
        assert_eq!(rig.status(&id).state, TaskRunState::Idle);
        assert!(rig.status(&id).next_run_ms.is_some());
        rig.advance(61);
        rig.wait_idle(&id, 2);
        assert_eq!(
            rig.runner.triggers(&id),
            [RunTrigger::Manual, RunTrigger::Interval]
        );
    }

    #[test]
    fn disabled_task_runs_only_on_demand() {
        let mut rig = Rig::new();
        let id = rig.add("a", |t| {
            t.enabled = false;
            t.triggers = vec![Trigger::Interval { every_minutes: 1 }];
        });
        let s = rig.start();
        assert_eq!(rig.status(&id).state, TaskRunState::Disabled);
        rig.advance(90);
        thread::sleep(Duration::from_millis(100));
        assert!(rig.runner.calls().is_empty());
        s.run_now(&id).unwrap();
        rig.wait("manual run", |r| {
            r.runner.calls().len() == 1 && !r.sched().is_busy()
        });
        rig.wait("disabled again", |r| {
            r.status(&id).state == TaskRunState::Disabled
        });
    }

    #[test]
    fn retries_back_off_and_only_the_final_failure_notifies() {
        let mut rig = Rig::new();
        let id = rig.add("a", |t| {
            t.retry = RunRetry {
                attempts: 2,
                delay_secs: 60,
            }
        });
        for _ in 0..3 {
            rig.runner.push(
                &id,
                RunOutcome::failed("The left folder /data/a isn't reachable.", true),
            );
        }
        let s = rig.start();
        s.run_now(&id).unwrap();
        rig.wait("first retry scheduled", |r| {
            r.status(&id).state == TaskRunState::WaitingToRetry
        });
        let st = rig.status(&id);
        assert_eq!(
            st.retry_at_ms,
            Some((start_at() + chrono::Duration::seconds(60)).timestamp_millis())
        );
        assert_eq!(st.attempt, 2);
        assert!(rig.host.titles().is_empty(), "not the final attempt");

        rig.advance(59);
        thread::sleep(Duration::from_millis(50));
        assert_eq!(rig.runner.calls().len(), 1);
        rig.advance(1);
        rig.wait("second retry scheduled", |r| {
            r.runner.calls().len() == 2 && r.status(&id).state == TaskRunState::WaitingToRetry
        });
        assert_eq!(
            rig.status(&id).retry_at_ms,
            Some((start_at() + chrono::Duration::seconds(60 + 120)).timestamp_millis()),
            "the delay doubles"
        );
        assert!(rig.host.titles().is_empty());
        rig.advance(120);
        rig.wait_idle(&id, 3);

        let calls = rig.runner.calls();
        assert_eq!(
            calls
                .iter()
                .map(|c| (c.trigger, c.attempt))
                .collect::<Vec<_>>(),
            [
                (RunTrigger::Manual, 1),
                (RunTrigger::Retry, 2),
                (RunTrigger::Retry, 3)
            ]
        );
        assert_eq!(rig.host.titles(), ["Sync failed: a"], "exactly once");
        let n = &lock(&rig.host.desktop)[0];
        assert!(n.summary.contains("after 3 attempts"), "{}", n.summary);
        assert_eq!(n.host, "TEST-PC");
        let runs = rig.runs(&id);
        assert_eq!(
            runs.iter().map(|r| r.attempt).collect::<Vec<_>>(),
            [1, 2, 3]
        );
        assert!(runs.iter().all(|r| r.status == RunStatus::Failed));
        assert_eq!(rig.status(&id).consecutive_failures, 3);
        let state = rig.host.index.as_ref().unwrap().task_state(&id).unwrap();
        assert!(state.last_notified_ms.is_some(), "recorded in the index");
    }

    #[test]
    fn the_retry_delay_is_capped_at_an_hour() {
        let mut rig = Rig::new();
        let id = rig.add("a", |t| {
            t.retry = RunRetry {
                attempts: 1,
                delay_secs: 3 * 3600,
            }
        });
        rig.runner.push(&id, RunOutcome::failed("down", true));
        let s = rig.start();
        s.run_now(&id).unwrap();
        rig.wait("retry scheduled", |r| {
            r.status(&id).state == TaskRunState::WaitingToRetry
        });
        assert_eq!(
            rig.status(&id).retry_at_ms,
            Some((start_at() + chrono::Duration::hours(1)).timestamp_millis())
        );
        // Run now replaces the pending retry.
        s.run_now(&id).unwrap();
        rig.wait_idle(&id, 2);
        assert_eq!(
            rig.runner.triggers(&id),
            [RunTrigger::Manual, RunTrigger::Manual]
        );
        assert_eq!(rig.status(&id).retry_at_ms, None);
    }

    #[test]
    fn non_retryable_failures_and_safety_aborts_are_not_retried() {
        let mut rig = Rig::new();
        let id = rig.add("a", |t| {
            t.retry = RunRetry {
                attempts: 3,
                delay_secs: 1,
            }
        });
        let mut aborted = RunOutcome::failed("Would delete 90% of the files on the right.", false);
        aborted.status = RunStatus::Aborted;
        rig.runner.push(&id, aborted);
        let s = rig.start();
        s.run_now(&id).unwrap();
        rig.wait_idle(&id, 1);
        assert_eq!(rig.runner.calls().len(), 1);
        assert_eq!(rig.host.titles(), ["Sync stopped by a safety check: a"]);
    }

    #[test]
    fn repeat_failures_are_suppressed_and_recovery_is_announced() {
        let mut rig = Rig::new();
        let id = rig.add("a", |_| {});
        rig.runner
            .push(&id, RunOutcome::failed("3 files locked", false));
        rig.runner
            .push(&id, RunOutcome::failed("4 files locked", false));
        rig.runner.push(&id, RunOutcome::failed("Disk full", false));
        let s = rig.start();
        for n in 1..=3 {
            s.run_now(&id).unwrap();
            rig.wait_idle(&id, n);
        }
        assert_eq!(
            rig.host.titles(),
            ["Sync failed: a", "Sync failed: a"],
            "the repeat was suppressed, the new problem wasn't"
        );
        assert!(lock(&rig.host.desktop)[1].summary.contains("Disk full"));
        // Six hours later the same problem is re-sent as a reminder.
        rig.runner.push(&id, RunOutcome::failed("Disk full", false));
        rig.clock.advance(6 * 3600);
        s.run_now(&id).unwrap();
        rig.wait_idle(&id, 4);
        assert_eq!(rig.host.titles().len(), 3);
        // Recovery.
        s.run_now(&id).unwrap();
        rig.wait_idle(&id, 5);
        assert_eq!(rig.host.titles().last().unwrap(), "Sync working again: a");
        assert_eq!(rig.status(&id).consecutive_failures, 0);
        // And a further success isn't news.
        s.run_now(&id).unwrap();
        rig.wait_idle(&id, 6);
        assert_eq!(rig.host.titles().len(), 4);
    }

    #[test]
    fn missed_schedule_is_caught_up_once_after_a_clock_jump() {
        let mut rig = Rig::new();
        let id = rig.add("a", |t| {
            t.triggers = vec![Trigger::Daily {
                times: vec!["12:00".into()],
                weekdays: vec![],
            }]
        });
        let off = rig.add("b", |t| {
            t.triggers = vec![Trigger::Daily {
                times: vec!["12:00".into()],
                weekdays: vec![],
            }];
            t.catch_up_missed = false;
        });
        let s = rig.start();
        s.run_now(&id).unwrap();
        s.run_now(&off).unwrap();
        rig.wait_idle(&id, 1);
        rig.wait_idle(&off, 1);
        let noon = |day: u32| {
            Local
                .with_ymd_and_hms(2026, 5, day, 12, 0, 0)
                .earliest()
                .unwrap()
                .timestamp_millis()
        };
        assert_eq!(rig.status(&id).next_run_ms, Some(noon(4)));

        // The machine sleeps for two days.
        rig.advance(2 * 86_400);
        rig.wait_idle(&id, 2);
        thread::sleep(Duration::from_millis(100));
        assert_eq!(
            rig.runner.triggers(&id),
            [RunTrigger::Manual, RunTrigger::CatchUp],
            "once, not twice"
        );
        assert_eq!(
            rig.runner.triggers(&off),
            [RunTrigger::Manual],
            "catch-up off: skipped"
        );
        assert_eq!(rig.status(&id).next_run_ms, Some(noon(6)));
        assert_eq!(rig.status(&off).next_run_ms, Some(noon(6)));
        assert_eq!(rig.status(&id).next_trigger, Some(RunTrigger::Daily));
    }

    #[test]
    fn missed_schedule_is_caught_up_at_startup() {
        let index = Index::open_in_memory().unwrap();
        let mut rig = Rig::with_index(index);
        let id = rig.add("a", |t| {
            t.triggers = vec![Trigger::Daily {
                times: vec!["12:00".into()],
                weekdays: vec![],
            }]
        });
        let delayed = rig.add("b", |t| {
            t.triggers = vec![
                Trigger::Cron {
                    expr: "0 * * * *".into(),
                },
                Trigger::OnStartup { delay_secs: 30 },
            ];
        });
        let index = rig.host.index.as_ref().unwrap();
        for task in [&id, &delayed] {
            let r = index
                .start_run(&NewRun {
                    task_id: task,
                    task_name: "x",
                    trigger: RunTrigger::Manual,
                    attempt: 1,
                })
                .unwrap();
            index
                .finish_run(
                    &r,
                    &RunFinish {
                        status: Some(RunStatus::Succeeded),
                        ..RunFinish::default()
                    },
                )
                .unwrap();
        }
        // The app starts two days after those runs.
        *lock(&rig.clock.0) = Local::now() + chrono::Duration::days(2);
        rig.start();
        rig.wait_idle(&id, 1);
        assert_eq!(rig.runner.triggers(&id), [RunTrigger::CatchUp]);
        // With a startup trigger, the catch-up waits for its delay and replaces it.
        assert!(rig.runner.triggers(&delayed).is_empty());
        assert_eq!(rig.status(&delayed).next_trigger, Some(RunTrigger::CatchUp));
        rig.advance(30);
        rig.wait_idle(&delayed, 1);
        thread::sleep(Duration::from_millis(50));
        assert_eq!(rig.runner.triggers(&delayed), [RunTrigger::CatchUp]);
    }

    #[test]
    fn startup_trigger_fires_after_its_delay() {
        let mut rig = Rig::new();
        let id = rig.add("a", |t| {
            t.triggers = vec![Trigger::OnStartup { delay_secs: 20 }]
        });
        rig.start();
        assert_eq!(rig.status(&id).next_trigger, Some(RunTrigger::Startup));
        rig.advance(19);
        thread::sleep(Duration::from_millis(50));
        assert!(rig.runner.calls().is_empty());
        rig.advance(1);
        rig.wait_idle(&id, 1);
        assert_eq!(rig.runner.triggers(&id), [RunTrigger::Startup]);
        assert_eq!(rig.status(&id).next_run_ms, None, "once");
    }

    #[test]
    fn reload_picks_up_edits_and_deletions() {
        let mut rig = Rig::new();
        let id = rig.add("a", |_| {});
        let s = rig.start();
        assert_eq!(rig.status(&id).next_run_ms, None);

        let mut task = rig.store.task(&id).unwrap().unwrap();
        task.triggers = vec![Trigger::Daily {
            times: vec!["10:01".into()],
            weekdays: vec![],
        }];
        rig.store.save_task(task).unwrap();
        s.reload();
        assert_eq!(
            rig.status(&id).next_run_ms,
            Some((start_at() + chrono::Duration::minutes(1)).timestamp_millis())
        );
        rig.advance(60);
        rig.wait_idle(&id, 1);
        assert_eq!(rig.runner.triggers(&id), [RunTrigger::Daily]);

        // A new task is picked up by Run now even before a reload.
        let b = rig.add("b", |_| {});
        s.run_now(&b).unwrap();
        rig.wait_idle(&b, 1);

        // Deleting a running task lets the run finish, then forgets it.
        rig.runner.hold(true);
        s.run_now(&id).unwrap();
        rig.wait("running", |r| r.status(&id).state == TaskRunState::Running);
        rig.store.delete_task(&id).unwrap();
        s.reload();
        assert!(s.status(&id).is_none());
        assert_eq!(s.statuses().len(), 1);
        rig.runner.hold(false);
        rig.wait("finished", |r| !r.sched().is_busy());
        thread::sleep(Duration::from_millis(50));
        assert!(s.status(&id).is_none());
        assert_eq!(rig.runs(&id).len(), 2, "history kept");
    }

    #[test]
    fn cancel_stops_the_run_and_drops_follow_ups() {
        let mut rig = Rig::new();
        let id = rig.add("a", |_| {});
        let s = rig.start();
        rig.runner.hold(true);
        s.run_now(&id).unwrap();
        rig.wait("running", |r| r.status(&id).state == TaskRunState::Running);
        s.run_now(&id).unwrap();
        assert!(rig.status(&id).rerun_pending);
        s.cancel(&id);
        rig.wait_idle(&id, 1);
        thread::sleep(Duration::from_millis(50));
        rig.runner.hold(false);
        assert_eq!(rig.runner.calls().len(), 1, "follow-up dropped");
        let st = rig.status(&id);
        assert_eq!(st.last_status, Some(RunStatus::Cancelled));
        assert!(!st.rerun_pending);
        assert!(rig.host.titles().is_empty(), "cancelling isn't notified");
        assert_eq!(rig.runs(&id)[0].status, RunStatus::Cancelled);
    }

    #[test]
    fn a_run_over_its_time_limit_is_stopped_and_failed() {
        let mut rig = Rig::new();
        let id = rig.add("a", |t| {
            t.safety.timeout_minutes = Some(1);
            t.retry = RunRetry {
                attempts: 2,
                delay_secs: 1,
            };
        });
        let s = rig.start();
        rig.runner.hold(true);
        s.run_now(&id).unwrap();
        rig.wait("running", |r| r.status(&id).state == TaskRunState::Running);
        rig.advance(61);
        rig.wait_idle(&id, 1);
        rig.runner.hold(false);
        let runs = rig.runs(&id);
        assert_eq!(runs.len(), 1, "not retried");
        assert_eq!(runs[0].status, RunStatus::Failed);
        assert!(runs[0]
            .message
            .as_deref()
            .unwrap()
            .starts_with("Timed out after 1 minute;"));
        assert_eq!(rig.host.titles(), ["Sync failed: a"]);
    }

    #[test]
    fn shutdown_cancels_runs_and_is_idempotent() {
        let mut rig = Rig::new();
        let id = rig.add("a", |t| {
            t.retry = RunRetry {
                attempts: 2,
                delay_secs: 1,
            }
        });
        let s = rig.start();
        rig.runner.hold(true);
        s.run_now(&id).unwrap();
        rig.wait("running", |r| r.status(&id).state == TaskRunState::Running);
        let t = Instant::now();
        s.shutdown(Duration::from_secs(5));
        assert!(t.elapsed() < Duration::from_secs(2), "bounded");
        assert!(!s.is_busy());
        s.shutdown(Duration::from_secs(1));
        s.run_now(&id).unwrap();
        s.reload();
        thread::sleep(Duration::from_millis(50));
        assert_eq!(rig.runner.calls().len(), 1, "nothing runs after shutdown");
        assert_eq!(rig.runs(&id)[0].status, RunStatus::Cancelled);
        assert!(rig.host.titles().is_empty());

        // A zero wait returns at once, whatever the runs are doing.
        let mut rig = Rig::new();
        let id = rig.add("a", |_| {});
        let s = rig.start();
        rig.runner.hold(true);
        s.run_now(&id).unwrap();
        rig.wait("running", |r| r.status(&id).state == TaskRunState::Running);
        let t = Instant::now();
        s.shutdown(Duration::ZERO);
        assert!(t.elapsed() < Duration::from_millis(500));
        rig.runner.hold(false);
    }

    #[test]
    fn interrupted_runs_from_a_previous_process_are_notified() {
        let index = Index::open_in_memory().unwrap();
        let mut rig = Rig::with_index(index);
        let id = rig.add("a", |_| {});
        let quiet = rig.add("b", |t| t.notify.on_failure = false);
        let index = rig.host.index.as_ref().unwrap();
        for task in [&id, &quiet] {
            index
                .start_run(&NewRun {
                    task_id: task,
                    task_name: "x",
                    trigger: RunTrigger::Daily,
                    attempt: 1,
                })
                .unwrap();
        }
        rig.start();
        rig.wait("notified", |r| !r.host.titles().is_empty());
        thread::sleep(Duration::from_millis(50));
        assert_eq!(rig.host.titles(), ["Sync interrupted: a"]);
        assert_eq!(rig.status(&id).consecutive_failures, 1);
        assert_eq!(rig.status(&id).last_status, Some(RunStatus::Interrupted));
        assert_eq!(rig.runs(&id)[0].status, RunStatus::Interrupted);
    }

    #[test]
    fn works_without_an_index() {
        let mut rig = Rig::new();
        Arc::get_mut(&mut rig.host).unwrap().index = None;
        let id = rig.add("a", |_| {});
        rig.runner.push(&id, RunOutcome::failed("boom", false));
        let s = rig.start();
        s.run_now(&id).unwrap();
        rig.wait("notified", |r| !r.host.titles().is_empty());
        assert_eq!(rig.status(&id).consecutive_failures, 1);
        s.run_now(&id).unwrap();
        rig.wait("recovered", |r| r.host.titles().len() == 2);
        assert_eq!(rig.host.titles()[1], "Sync working again: a");
    }

    #[test]
    fn progress_updates_are_throttled() {
        struct Chatty;
        impl TaskRunner for Chatty {
            fn run(&self, _task: &SyncTask, ctx: &RunContext) -> RunOutcome {
                for i in 0..200 {
                    (ctx.progress)(RunProgress {
                        phase: RunPhase::Executing,
                        done_ops: i,
                        total_ops: 200,
                        bytes_done: 0,
                        bytes_total: 0,
                        current: Some(format!("f{i}")),
                    });
                    thread::sleep(Duration::from_millis(2));
                }
                RunOutcome {
                    status: RunStatus::Succeeded,
                    message: None,
                    ..RunOutcome::failed("", false)
                }
            }
        }
        let rig = Rig::new();
        let id = rig.add("a", |_| {});
        let s = Scheduler::start_with_clock(
            rig.store.clone(),
            Arc::new(Chatty),
            rig.host.clone(),
            rig.clock.clone(),
        );
        let t = Instant::now();
        s.run_now(&id).unwrap();
        while s.is_busy() || s.status(&id).unwrap().last_status.is_none() {
            assert!(t.elapsed() < Duration::from_secs(10));
            thread::sleep(Duration::from_millis(5));
        }
        let elapsed = t.elapsed();
        s.shutdown(Duration::from_secs(5));
        let with_progress = lock(&rig.host.statuses)
            .iter()
            .filter(|s| s.progress.is_some())
            .count();
        let allowed = (elapsed.as_millis() / PROGRESS_EVERY.as_millis()) as usize + 2;
        assert!(
            with_progress >= 1 && with_progress <= allowed,
            "{with_progress} updates in {elapsed:?}"
        );
    }

    #[test]
    fn realtime_changes_start_a_run() {
        let mut rig = Rig::new();
        let src = rig._dir.path().join("src");
        fs_create(&src);
        let src_str = src.to_string_lossy().into_owned();
        let id = rig.add("live", |t| {
            t.left = src_str.clone();
            t.triggers = vec![Trigger::Realtime {
                debounce_secs: 1,
                max_delay_secs: 5,
                rescan_minutes: 0,
            }];
        });
        rig.start();
        rig.wait("watching", |r| r.status(&id).watching);
        thread::sleep(Duration::from_millis(300));
        std::fs::write(src.join("new.txt"), b"hello").unwrap();
        rig.wait_idle(&id, 1);
        assert_eq!(rig.runner.triggers(&id), [RunTrigger::Realtime]);
        assert_eq!(rig.status(&id).watch_error, None);
    }

    #[test]
    fn unwatchable_realtime_side_reports_and_falls_back_to_rescans() {
        let mut rig = Rig::new();
        let id = rig.add("remote", |t| {
            t.left = "remote://nas/docs".into();
            t.triggers = vec![Trigger::Realtime {
                debounce_secs: 1,
                max_delay_secs: 5,
                rescan_minutes: 1,
            }];
        });
        rig.start();
        rig.wait("watch error", |r| r.status(&id).watch_error.is_some());
        assert!(!rig.status(&id).watching);
        // Never ran: the first rescan is right away.
        rig.wait_idle(&id, 1);
        assert_eq!(rig.runner.triggers(&id), [RunTrigger::Rescan]);
        assert_eq!(rig.status(&id).next_trigger, Some(RunTrigger::Rescan));
    }

    fn fs_create(p: &std::path::Path) {
        std::fs::create_dir_all(p).unwrap();
    }

    /// Answers every request with 200 and counts the bodies it got.
    fn local_webhook() -> (String, Arc<Mutex<Vec<String>>>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!(
            "http://127.0.0.1:{}/hook",
            listener.local_addr().unwrap().port()
        );
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let seen = bodies.clone();
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                // Read headers, then the body by Content-Length.
                while let Ok(n) = stream.read(&mut chunk) {
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    let text = String::from_utf8_lossy(&buf).to_string();
                    if let Some(end) = text.find("\r\n\r\n") {
                        let len = text[..end]
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                            })
                            .unwrap_or(0);
                        if buf.len() >= end + 4 + len {
                            lock(&seen).push(text[end + 4..].to_string());
                            break;
                        }
                    }
                }
                let _ = stream.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                );
            }
        });
        (url, bodies)
    }

    #[test]
    fn channel_results_are_recorded() {
        use pcompare_notify::{ChannelSecrets, ChannelSettings};
        let mut rig = Rig::new();
        let (url, bodies) = local_webhook();
        let secrets = |url: &str| ChannelSecrets {
            url: Some(url.to_owned()),
            ..ChannelSecrets::default()
        };
        let channels = &rig.host.channels;
        let hook = channels
            .save(
                "",
                "Ops hook",
                true,
                ChannelSettings::Webhook { headers: vec![] },
                &secrets(&url),
            )
            .unwrap();
        let off = channels
            .save(
                "",
                "Teams (off)",
                false,
                ChannelSettings::Teams {},
                &secrets("https://example.com/teams"),
            )
            .unwrap();
        let id = rig.add("a", |t| {
            t.notify.channels = vec![hook.id.clone(), off.id.clone(), "gone".into()];
            t.notify.desktop = false;
        });
        rig.runner.push(
            &id,
            RunOutcome::failed("The right folder isn't reachable.", false),
        );
        let s = rig.start();
        s.run_now(&id).unwrap();
        rig.wait("results recorded", |r| {
            r.runs(&id)
                .first()
                .is_some_and(|r| !r.notifications.is_empty())
        });
        let run = &rig.runs(&id)[0];
        assert_eq!(
            run.notifications,
            [
                "Ops hook: sent",
                "Teams (off): skipped (disabled)",
                "gone: skipped (channel deleted)"
            ]
        );
        let bodies = lock(&bodies).clone();
        assert_eq!(bodies.len(), 1);
        let n: Notification = serde_json::from_str(&bodies[0]).unwrap();
        assert_eq!(n.title, "Sync failed: a");
        assert!(rig.host.titles().is_empty(), "desktop off");
        let state = rig.host.index.as_ref().unwrap().task_state(&id).unwrap();
        assert!(state.last_notified_ms.is_some());

        // Nothing delivered (only a disabled channel): the next failure isn't suppressed.
        let only_off = rig.add("b", |t| {
            t.notify.channels = vec![off.id.clone()];
            t.notify.desktop = false;
        });
        rig.runner.push(&only_off, RunOutcome::failed("x", false));
        s.run_now(&only_off).unwrap();
        rig.wait("results recorded", |r| {
            r.runs(&only_off)
                .first()
                .is_some_and(|r| !r.notifications.is_empty())
        });
        thread::sleep(Duration::from_millis(50));
        assert_eq!(
            rig.host
                .index
                .as_ref()
                .unwrap()
                .task_state(&only_off)
                .unwrap()
                .last_notified_ms,
            None
        );
        assert_eq!(
            lock(&s.inner.state).tasks[&only_off]
                .history
                .last_notified_ms,
            None
        );
    }
}
