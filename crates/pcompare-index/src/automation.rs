//! Automation run history and per-task scheduler state.
//!
//! Task *definitions* are not stored here: the index is a cache and a
//! safety net that may be quarantined and rebuilt (see the crate docs), so
//! tasks live in `automation.json` next to the other settings. What lives
//! here is what the scheduler learns by running them:
//!
//! - `automation_runs`: one row per run attempt (trigger, outcome, counts,
//!   the sync job it produced, notification delivery), pruned with the
//!   job history;
//! - `automation_state`: one row per task — when it last ran and last
//!   succeeded, how many runs in a row failed, and the fingerprint/time of
//!   the last notification so the same failure isn't re-sent every minute.
//!   Losing this only means a missed run may not be caught up and one
//!   repeat notification may go out.

use pcompare_core::store::{new_id, now_ms};
use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSqlOutput, Type, ValueRef};
use rusqlite::{params, OptionalExtension, Row, ToSql, Transaction};
use serde::{Deserialize, Serialize};

use crate::{Index, IndexError, Result};

const DAY_MS: i64 = 86_400_000;
/// No-op runs are kept at most this many days, whatever `prune_runs` is
/// given: a real-time task can produce dozens a day and they say nothing.
const NO_CHANGES_KEEP_DAYS: u32 = 7;

/// What started a run.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "camelCase")]
pub enum RunTrigger {
    Manual,
    Interval,
    Daily,
    Cron,
    /// A file-system change under a watched folder.
    Realtime,
    /// The periodic full rescan of a real-time task (catches missed events).
    Rescan,
    Startup,
    /// A scheduled time passed while the app wasn't running or the machine slept.
    CatchUp,
    /// A whole-run retry after a failed attempt.
    Retry,
}

impl RunTrigger {
    /// The stored form, identical to the serde name.
    pub fn as_str(self) -> &'static str {
        match self {
            RunTrigger::Manual => "manual",
            RunTrigger::Interval => "interval",
            RunTrigger::Daily => "daily",
            RunTrigger::Cron => "cron",
            RunTrigger::Realtime => "realtime",
            RunTrigger::Rescan => "rescan",
            RunTrigger::Startup => "startup",
            RunTrigger::CatchUp => "catchUp",
            RunTrigger::Retry => "retry",
        }
    }

    const ALL: [RunTrigger; 9] = [
        RunTrigger::Manual,
        RunTrigger::Interval,
        RunTrigger::Daily,
        RunTrigger::Cron,
        RunTrigger::Realtime,
        RunTrigger::Rescan,
        RunTrigger::Startup,
        RunTrigger::CatchUp,
        RunTrigger::Retry,
    ];
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "camelCase")]
pub enum RunStatus {
    /// Still running (or the app died during it; see `mark_runs_interrupted`).
    Running,
    /// Everything planned was done.
    Succeeded,
    /// Nothing to do: the folders were already in sync.
    NoChanges,
    /// Ran to the end but some operations failed or conflicts were skipped.
    CompletedWithErrors,
    /// Couldn't run or stopped early: a side unreachable, planning failed,
    /// a blocking preflight issue, the run stopped itself, or it timed out.
    Failed,
    /// A safety check refused the plan (mass deletion, empty side).
    Aborted,
    Cancelled,
    /// Was `Running` when the app last exited.
    Interrupted,
}

impl RunStatus {
    /// Failed, aborted and interrupted runs count towards
    /// `consecutive_failures`; a success or no-op resets it.
    pub fn is_failure(self) -> bool {
        matches!(
            self,
            RunStatus::Failed | RunStatus::Aborted | RunStatus::Interrupted
        )
    }

    /// The stored form, identical to the serde name.
    pub fn as_str(self) -> &'static str {
        match self {
            RunStatus::Running => "running",
            RunStatus::Succeeded => "succeeded",
            RunStatus::NoChanges => "noChanges",
            RunStatus::CompletedWithErrors => "completedWithErrors",
            RunStatus::Failed => "failed",
            RunStatus::Aborted => "aborted",
            RunStatus::Cancelled => "cancelled",
            RunStatus::Interrupted => "interrupted",
        }
    }

    const ALL: [RunStatus; 8] = [
        RunStatus::Running,
        RunStatus::Succeeded,
        RunStatus::NoChanges,
        RunStatus::CompletedWithErrors,
        RunStatus::Failed,
        RunStatus::Aborted,
        RunStatus::Cancelled,
        RunStatus::Interrupted,
    ];
}

// The same mapping as `jobs.rs`: enums are stored as their serde names.
macro_rules! sql_text_enum {
    ($t:ty) => {
        impl ToSql for $t {
            fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
                Ok(self.as_str().into())
            }
        }

        impl FromSql for $t {
            fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
                let s = value.as_str()?;
                <$t>::ALL
                    .into_iter()
                    .find(|v| v.as_str() == s)
                    .ok_or_else(|| {
                        FromSqlError::Other(format!("unknown {} `{s}`", stringify!($t)).into())
                    })
            }
        }
    };
}

sql_text_enum!(RunTrigger);
sql_text_enum!(RunStatus);

/// One run attempt as stored.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AutomationRun {
    pub id: String,
    pub task_id: String,
    /// The task's name when it ran (tasks can be renamed or deleted later).
    pub task_name: String,
    pub trigger: RunTrigger,
    /// 1 for the first attempt; retries count up.
    pub attempt: u32,
    pub status: RunStatus,
    pub started_ms: i64,
    pub finished_ms: Option<i64>,
    /// The journaled sync job, when the run got as far as executing.
    pub job_id: Option<String>,
    pub ops_total: usize,
    pub ops_done: usize,
    pub ops_failed: usize,
    pub conflicts: usize,
    pub bytes_copied: u64,
    /// Why it failed / was aborted, or a one-line summary.
    pub message: Option<String>,
    /// Per-channel delivery results, e.g. `["Ops (Teams): sent", "Mail: SMTP 535 ..."]`.
    pub notifications: Vec<String>,
}

/// A run about to start.
pub struct NewRun<'a> {
    pub task_id: &'a str,
    pub task_name: &'a str,
    pub trigger: RunTrigger,
    pub attempt: u32,
}

/// How a run ended; everything `finish_run` records.
#[derive(Clone, Debug, Default)]
pub struct RunFinish {
    pub status: Option<RunStatus>,
    pub job_id: Option<String>,
    pub ops_total: usize,
    pub ops_done: usize,
    pub ops_failed: usize,
    pub conflicts: usize,
    pub bytes_copied: u64,
    pub message: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TaskState {
    pub task_id: String,
    pub last_run_ms: Option<i64>,
    pub last_status: Option<RunStatus>,
    pub last_success_ms: Option<i64>,
    pub consecutive_failures: u32,
    /// When a notification for this task last went out, and a fingerprint
    /// of what it was about (status + message), for repeat suppression.
    pub last_notified_ms: Option<i64>,
    pub last_notified_fingerprint: Option<String>,
}

impl TaskState {
    /// Folds a finished run into the state, the same way [`Index::finish_run`]
    /// does (the scheduler keeps an in-memory copy when there's no index):
    ///
    /// - `last_run_ms` is when the run *ended* (interval triggers count from
    ///   it), whatever the outcome;
    /// - failures ([`RunStatus::is_failure`]) add one to
    ///   `consecutive_failures`;
    /// - a success or no-op resets it and sets `last_success_ms`; a run that
    ///   completed with errors also resets it (it got through, so the outage
    ///   is over) but isn't a success;
    /// - a cancelled run changes neither.
    pub fn record(&mut self, status: RunStatus, finished_ms: i64) {
        self.last_run_ms = Some(self.last_run_ms.map_or(finished_ms, |t| t.max(finished_ms)));
        self.last_status = Some(status);
        match status {
            s if s.is_failure() => {
                self.consecutive_failures = self.consecutive_failures.saturating_add(1)
            }
            RunStatus::Succeeded | RunStatus::NoChanges => {
                self.consecutive_failures = 0;
                self.last_success_ms = Some(finished_ms);
            }
            RunStatus::CompletedWithErrors => self.consecutive_failures = 0,
            _ => {}
        }
    }
}

const RUN_COLUMNS: &str =
    "id, task_id, task_name, run_trigger, attempt, status, started_ms, finished_ms, job_id,
     ops_total, ops_done, ops_failed, conflicts, bytes_copied, message, notifications";

impl Index {
    /// Records a run as `Running` and returns its id.
    pub fn start_run(&self, run: &NewRun) -> Result<String> {
        let id = new_id();
        self.conn()
            .prepare_cached(
                "INSERT INTO automation_runs (id, task_id, task_name, run_trigger, attempt, status, started_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?
            .execute(params![id, run.task_id, run.task_name, run.trigger, run.attempt, RunStatus::Running, now_ms()])?;
        Ok(id)
    }

    /// Closes a run and updates the task's [`TaskState`] in the same
    /// transaction (last run / success, consecutive failures; see
    /// [`TaskState::record`]). `status` `None` (or `Running`) is treated as
    /// `Failed`.
    pub fn finish_run(&self, run_id: &str, finish: &RunFinish) -> Result<AutomationRun> {
        let status = match finish.status {
            None | Some(RunStatus::Running) => RunStatus::Failed,
            Some(s) => s,
        };
        let now = now_ms();
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let n = tx
            .prepare_cached(
                "UPDATE automation_runs SET status = ?2, finished_ms = ?3, job_id = ?4, ops_total = ?5,
                     ops_done = ?6, ops_failed = ?7, conflicts = ?8, bytes_copied = ?9, message = ?10
                 WHERE id = ?1",
            )?
            .execute(params![
                run_id,
                status,
                now,
                finish.job_id,
                finish.ops_total as i64,
                finish.ops_done as i64,
                finish.ops_failed as i64,
                finish.conflicts as i64,
                i64::try_from(finish.bytes_copied).unwrap_or(i64::MAX),
                finish.message,
            ])?;
        if n == 0 {
            return Err(run_not_found(run_id));
        }
        let run = load_run(&tx, run_id)?.ok_or_else(|| run_not_found(run_id))?;
        let mut state = load_state(&tx, &run.task_id)?;
        state.record(status, now);
        store_state(&tx, &state)?;
        tx.commit()?;
        Ok(run)
    }

    /// Appends notification delivery results to a run.
    pub fn record_notifications(&self, run_id: &str, results: &[String]) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let stored: Option<String> = tx
            .prepare_cached("SELECT notifications FROM automation_runs WHERE id = ?1")?
            .query_row([run_id], |r| r.get(0))
            .optional()?;
        let mut all: Vec<String> = match stored {
            Some(json) => serde_json::from_str(&json)?,
            None => return Err(run_not_found(run_id)),
        };
        all.extend(results.iter().cloned());
        tx.prepare_cached("UPDATE automation_runs SET notifications = ?2 WHERE id = ?1")?
            .execute(params![run_id, serde_json::to_string(&all)?])?;
        tx.commit()?;
        Ok(())
    }

    /// Stores that a notification went out for `task_id` now.
    pub fn mark_notified(&self, task_id: &str, at_ms: i64, fingerprint: &str) -> Result<()> {
        let conn = self.conn();
        ensure_state(&conn, task_id)?;
        conn.prepare_cached(
            "UPDATE automation_state SET last_notified_ms = ?2, last_notified_fingerprint = ?3 WHERE task_id = ?1",
        )?
        .execute(params![task_id, at_ms, fingerprint])?;
        Ok(())
    }

    /// Runs, newest first; for one task when `task_id` is given.
    pub fn list_runs(&self, task_id: Option<&str>, limit: usize) -> Result<Vec<AutomationRun>> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let conn = self.conn();
        let rows = match task_id {
            None => {
                let sql = format!("SELECT {RUN_COLUMNS} FROM automation_runs ORDER BY started_ms DESC, rowid DESC LIMIT ?1");
                let mut stmt = conn.prepare_cached(&sql)?;
                let rows = stmt.query_map(params![limit], run_row)?;
                rows.collect::<rusqlite::Result<Vec<_>>>()?
            }
            Some(task_id) => {
                let sql = format!(
                    "SELECT {RUN_COLUMNS} FROM automation_runs WHERE task_id = ?2
                     ORDER BY started_ms DESC, rowid DESC LIMIT ?1"
                );
                let mut stmt = conn.prepare_cached(&sql)?;
                let rows = stmt.query_map(params![limit, task_id], run_row)?;
                rows.collect::<rusqlite::Result<Vec<_>>>()?
            }
        };
        Ok(rows)
    }

    pub fn task_state(&self, task_id: &str) -> Result<TaskState> {
        let conn = self.conn();
        load_state(&conn, task_id)
    }

    pub fn task_states(&self) -> Result<Vec<TaskState>> {
        let conn = self.conn();
        let mut stmt = conn.prepare_cached(&format!(
            "SELECT {STATE_COLUMNS} FROM automation_state ORDER BY task_id"
        ))?;
        let rows = stmt.query_map([], state_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Drops a deleted task's state (its run history is kept until pruned).
    pub fn forget_task(&self, task_id: &str) -> Result<()> {
        self.conn()
            .prepare_cached("DELETE FROM automation_state WHERE task_id = ?1")?
            .execute([task_id])?;
        Ok(())
    }

    /// At startup: runs still `Running` belong to a previous process that
    /// died; mark them `Interrupted` (counts as a failure in task state).
    /// Returns them so the app can notify about them.
    ///
    /// Their `finished_ms` stays empty: when exactly the process died isn't
    /// known. In the task state they count from when they started.
    pub fn mark_runs_interrupted(&self) -> Result<Vec<AutomationRun>> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let ids: Vec<String> = tx
            .prepare_cached(
                "SELECT id FROM automation_runs WHERE status = ?1 ORDER BY started_ms, rowid",
            )?
            .query_map([RunStatus::Running], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        let mut runs = Vec::with_capacity(ids.len());
        for id in &ids {
            tx.prepare_cached("UPDATE automation_runs SET status = ?2 WHERE id = ?1")?
                .execute(params![id, RunStatus::Interrupted])?;
            let Some(run) = load_run(&tx, id)? else {
                continue;
            };
            let mut state = load_state(&tx, &run.task_id)?;
            state.record(RunStatus::Interrupted, run.started_ms);
            store_state(&tx, &state)?;
            runs.push(run);
        }
        tx.commit()?;
        Ok(runs)
    }

    /// Deletes runs that finished more than `days` ago, keeping no-op
    /// (`NoChanges`) runs for at most 7 days so real-time tasks don't bloat
    /// the history. Returns how many were deleted.
    pub fn prune_runs(&self, days: u32) -> Result<usize> {
        let now = now_ms();
        let cutoff = now - i64::from(days) * DAY_MS;
        let no_op_cutoff = now - i64::from(days.min(NO_CHANGES_KEEP_DAYS)) * DAY_MS;
        Ok(self
            .conn()
            .prepare_cached(
                "DELETE FROM automation_runs
                 WHERE status != ?3
                   AND (COALESCE(finished_ms, started_ms) < ?1
                        OR (status = ?4 AND COALESCE(finished_ms, started_ms) < ?2))",
            )?
            .execute(params![
                cutoff,
                no_op_cutoff,
                RunStatus::Running,
                RunStatus::NoChanges
            ])?)
    }
}

fn run_not_found(run_id: &str) -> IndexError {
    IndexError::Invalid(format!("automation run {run_id} not found"))
}

fn run_row(r: &Row) -> rusqlite::Result<AutomationRun> {
    let notifications: String = r.get(15)?;
    Ok(AutomationRun {
        id: r.get(0)?,
        task_id: r.get(1)?,
        task_name: r.get(2)?,
        trigger: r.get(3)?,
        attempt: r.get(4)?,
        status: r.get(5)?,
        started_ms: r.get(6)?,
        finished_ms: r.get(7)?,
        job_id: r.get(8)?,
        ops_total: r.get::<_, i64>(9)? as usize,
        ops_done: r.get::<_, i64>(10)? as usize,
        ops_failed: r.get::<_, i64>(11)? as usize,
        conflicts: r.get::<_, i64>(12)? as usize,
        bytes_copied: r.get::<_, i64>(13)? as u64,
        message: r.get(14)?,
        notifications: serde_json::from_str(&notifications)
            .map_err(|e| rusqlite::Error::FromSqlConversionFailure(15, Type::Text, Box::new(e)))?,
    })
}

fn load_run(tx: &Transaction, run_id: &str) -> Result<Option<AutomationRun>> {
    Ok(tx
        .prepare_cached(&format!(
            "SELECT {RUN_COLUMNS} FROM automation_runs WHERE id = ?1"
        ))?
        .query_row([run_id], run_row)
        .optional()?)
}

const STATE_COLUMNS: &str =
    "task_id, last_run_ms, last_status, last_success_ms, consecutive_failures,
     last_notified_ms, last_notified_fingerprint";

fn state_row(r: &Row) -> rusqlite::Result<TaskState> {
    Ok(TaskState {
        task_id: r.get(0)?,
        last_run_ms: r.get(1)?,
        last_status: r.get(2)?,
        last_success_ms: r.get(3)?,
        consecutive_failures: r.get(4)?,
        last_notified_ms: r.get(5)?,
        last_notified_fingerprint: r.get(6)?,
    })
}

/// The stored state, or a fresh one for a task that never ran.
fn load_state(conn: &rusqlite::Connection, task_id: &str) -> Result<TaskState> {
    let state = conn
        .prepare_cached(&format!(
            "SELECT {STATE_COLUMNS} FROM automation_state WHERE task_id = ?1"
        ))?
        .query_row([task_id], state_row)
        .optional()?;
    Ok(state.unwrap_or_else(|| TaskState {
        task_id: task_id.to_owned(),
        ..TaskState::default()
    }))
}

fn ensure_state(conn: &rusqlite::Connection, task_id: &str) -> Result<()> {
    conn.prepare_cached(
        "INSERT INTO automation_state (task_id) VALUES (?1) ON CONFLICT (task_id) DO NOTHING",
    )?
    .execute([task_id])?;
    Ok(())
}

/// Writes the run-derived columns (the notification columns are left alone).
fn store_state(conn: &rusqlite::Connection, s: &TaskState) -> Result<()> {
    ensure_state(conn, &s.task_id)?;
    conn.prepare_cached(
        "UPDATE automation_state SET last_run_ms = ?2, last_status = ?3, last_success_ms = ?4,
             consecutive_failures = ?5
         WHERE task_id = ?1",
    )?
    .execute(params![
        s.task_id,
        s.last_run_ms,
        s.last_status,
        s.last_success_ms,
        s.consecutive_failures
    ])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MIGRATIONS, SCHEMA_VERSION};
    use serde_json::json;

    fn start(index: &Index, task: &str, trigger: RunTrigger) -> String {
        index
            .start_run(&NewRun {
                task_id: task,
                task_name: "Nightly",
                trigger,
                attempt: 1,
            })
            .unwrap()
    }

    fn finish(index: &Index, run: &str, status: RunStatus) -> AutomationRun {
        index
            .finish_run(
                run,
                &RunFinish {
                    status: Some(status),
                    ops_total: 4,
                    ops_done: 3,
                    ops_failed: 1,
                    bytes_copied: 1234,
                    message: Some("1 file locked".into()),
                    job_id: Some("job1".into()),
                    ..RunFinish::default()
                },
            )
            .unwrap()
    }

    #[test]
    fn stored_names_match_serde() {
        for s in RunStatus::ALL {
            assert_eq!(json!(s), json!(s.as_str()));
        }
        for t in RunTrigger::ALL {
            assert_eq!(json!(t), json!(t.as_str()));
        }
    }

    #[test]
    fn run_lifecycle_updates_task_state() {
        let index = Index::open_in_memory().unwrap();
        assert_eq!(
            index.task_state("t1").unwrap().task_id,
            "t1",
            "unknown task: fresh state"
        );

        let r1 = start(&index, "t1", RunTrigger::Daily);
        let running = index.list_runs(Some("t1"), 10).unwrap();
        assert_eq!(running.len(), 1);
        assert_eq!(running[0].status, RunStatus::Running);
        assert_eq!(running[0].finished_ms, None);

        let run = finish(&index, &r1, RunStatus::Failed);
        assert_eq!(run.status, RunStatus::Failed);
        assert_eq!(run.trigger, RunTrigger::Daily);
        assert_eq!(
            (
                run.ops_total,
                run.ops_done,
                run.ops_failed,
                run.bytes_copied
            ),
            (4, 3, 1, 1234)
        );
        assert_eq!(run.job_id.as_deref(), Some("job1"));
        assert!(run.finished_ms.is_some());
        let s = index.task_state("t1").unwrap();
        assert_eq!(s.consecutive_failures, 1);
        assert_eq!(s.last_status, Some(RunStatus::Failed));
        assert_eq!(s.last_run_ms, run.finished_ms);
        assert_eq!(s.last_success_ms, None);

        let r2 = start(&index, "t1", RunTrigger::Retry);
        finish(&index, &r2, RunStatus::Aborted);
        assert_eq!(index.task_state("t1").unwrap().consecutive_failures, 2);

        let r3 = start(&index, "t1", RunTrigger::Manual);
        finish(&index, &r3, RunStatus::Cancelled);
        assert_eq!(
            index.task_state("t1").unwrap().consecutive_failures,
            2,
            "cancel changes nothing"
        );

        let r4 = start(&index, "t1", RunTrigger::Manual);
        let ok = finish(&index, &r4, RunStatus::NoChanges);
        let s = index.task_state("t1").unwrap();
        assert_eq!(s.consecutive_failures, 0);
        assert_eq!(s.last_success_ms, ok.finished_ms);

        // None is a failure.
        let r5 = start(&index, "t1", RunTrigger::Manual);
        let run = index.finish_run(&r5, &RunFinish::default()).unwrap();
        assert_eq!(run.status, RunStatus::Failed);

        let all = index.list_runs(Some("t1"), 10).unwrap();
        assert_eq!(
            all.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            [&r5, &r4, &r3, &r2, &r1]
        );
        assert_eq!(index.list_runs(None, 2).unwrap().len(), 2);
        assert!(index.list_runs(Some("other"), 10).unwrap().is_empty());
        assert!(matches!(
            index.finish_run("nope", &RunFinish::default()),
            Err(IndexError::Invalid(_))
        ));
    }

    #[test]
    fn completed_with_errors_resets_failures_but_is_not_a_success() {
        let mut s = TaskState::default();
        s.record(RunStatus::Failed, 10);
        s.record(RunStatus::Interrupted, 20);
        assert_eq!(s.consecutive_failures, 2);
        s.record(RunStatus::CompletedWithErrors, 30);
        assert_eq!(
            (s.consecutive_failures, s.last_success_ms, s.last_run_ms),
            (0, None, Some(30))
        );
        s.record(RunStatus::Succeeded, 40);
        assert_eq!(s.last_success_ms, Some(40));
        s.record(RunStatus::Failed, 5);
        assert_eq!(s.last_run_ms, Some(40), "never moves backwards");
    }

    #[test]
    fn notifications_and_notified_state() {
        let index = Index::open_in_memory().unwrap();
        let r = start(&index, "t1", RunTrigger::Cron);
        finish(&index, &r, RunStatus::Failed);
        index
            .record_notifications(&r, &["Ops (Teams): sent".into()])
            .unwrap();
        index
            .record_notifications(&r, &["Mail: SMTP 535".into()])
            .unwrap();
        let run = &index.list_runs(None, 1).unwrap()[0];
        assert_eq!(run.notifications, ["Ops (Teams): sent", "Mail: SMTP 535"]);
        assert!(index.record_notifications("nope", &[]).is_err());

        index
            .mark_notified("t1", 99, "failed|# files locked")
            .unwrap();
        index.mark_notified("t2", 5, "x").unwrap();
        let s = index.task_state("t1").unwrap();
        assert_eq!(s.last_notified_ms, Some(99));
        assert_eq!(
            s.last_notified_fingerprint.as_deref(),
            Some("failed|# files locked")
        );
        assert_eq!(s.consecutive_failures, 1, "run state kept");

        // A later run keeps the notification columns.
        let r2 = start(&index, "t1", RunTrigger::Cron);
        finish(&index, &r2, RunStatus::Succeeded);
        let s = index.task_state("t1").unwrap();
        assert_eq!((s.last_notified_ms, s.consecutive_failures), (Some(99), 0));

        assert_eq!(index.task_states().unwrap().len(), 2);
        index.forget_task("t1").unwrap();
        let states = index.task_states().unwrap();
        assert_eq!(states.len(), 1);
        assert_eq!(states[0].task_id, "t2");
        assert_eq!(
            index.list_runs(Some("t1"), 10).unwrap().len(),
            2,
            "history kept"
        );
    }

    #[test]
    fn running_runs_become_interrupted_failures() {
        let index = Index::open_in_memory().unwrap();
        let done = start(&index, "t1", RunTrigger::Manual);
        finish(&index, &done, RunStatus::Succeeded);
        let a = start(&index, "t1", RunTrigger::Realtime);
        let b = start(&index, "t2", RunTrigger::Startup);

        let runs = index.mark_runs_interrupted().unwrap();
        assert_eq!(
            runs.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            [&a, &b]
        );
        assert!(runs
            .iter()
            .all(|r| r.status == RunStatus::Interrupted && r.finished_ms.is_none()));
        let s1 = index.task_state("t1").unwrap();
        assert_eq!(
            (s1.consecutive_failures, s1.last_status),
            (1, Some(RunStatus::Interrupted))
        );
        assert_eq!(index.task_state("t2").unwrap().consecutive_failures, 1);
        assert!(
            index.mark_runs_interrupted().unwrap().is_empty(),
            "only once"
        );
    }

    #[test]
    fn prune_keeps_recent_running_and_short_lived_no_ops() {
        let index = Index::open_in_memory().unwrap();
        let mk = |status: RunStatus, age_days: i64| {
            let id = start(&index, "t", RunTrigger::Realtime);
            if status != RunStatus::Running {
                finish(&index, &id, status);
            }
            let at = now_ms() - age_days * DAY_MS;
            index
                .conn()
                .execute(
                    "UPDATE automation_runs SET started_ms = ?2, finished_ms = CASE WHEN finished_ms IS NULL THEN NULL ELSE ?2 END WHERE id = ?1",
                    params![id, at],
                )
                .unwrap();
            id
        };
        let old_fail = mk(RunStatus::Failed, 100);
        let recent_fail = mk(RunStatus::Failed, 10);
        let old_noop = mk(RunStatus::NoChanges, 8);
        let new_noop = mk(RunStatus::NoChanges, 2);
        let old_running = mk(RunStatus::Running, 200);

        assert_eq!(index.prune_runs(90).unwrap(), 2);
        let left: Vec<String> = index
            .list_runs(None, 10)
            .unwrap()
            .into_iter()
            .map(|r| r.id)
            .collect();
        assert!(!left.contains(&old_fail) && !left.contains(&old_noop));
        assert!(
            left.contains(&recent_fail) && left.contains(&new_noop) && left.contains(&old_running)
        );
        // A shorter history limit applies to no-ops too.
        assert_eq!(index.prune_runs(1).unwrap(), 2);
        assert_eq!(index.list_runs(None, 10).unwrap().len(), 1);
    }

    #[test]
    fn v1_database_upgrades_to_v2() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(MIGRATIONS[0]).unwrap();
            conn.pragma_update(None, "user_version", 1).unwrap();
            conn.execute("INSERT INTO checksums VALUES ('/x', 1, 2, 3, 4)", [])
                .unwrap();
            conn.execute(
                "INSERT INTO jobs (id, left, right, mode, options, status, started_ms, total_ops)
                 VALUES ('j1', '/l', '/r', 'twoWay', '{}', 'completed', 1, 0)",
                [],
            )
            .unwrap();
        }
        let index = Index::open(&path).unwrap();
        let v: i64 = index
            .conn()
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(v, SCHEMA_VERSION);
        const { assert!(SCHEMA_VERSION >= 2) };
        let stats = index.stats().unwrap();
        assert_eq!((stats.checksums, stats.jobs), (1, 1), "v1 data kept");
        let r = start(&index, "t1", RunTrigger::Manual);
        finish(&index, &r, RunStatus::Succeeded);
        drop(index);
        let index = Index::open(&path).unwrap();
        assert_eq!(index.list_runs(None, 10).unwrap().len(), 1);
        assert_eq!(
            index.task_state("t1").unwrap().last_status,
            Some(RunStatus::Succeeded)
        );
    }
}
