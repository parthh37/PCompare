//! The sync job journal: every run, every operation, and how far each got.
//!
//! [`Index::create_job`] records the whole plan up front (all operations
//! `Pending`), then the executor reports progress through a [`JobJournal`],
//! the database-backed [`Journal`]. If the app dies mid-run the job is still
//! `Running` on the next start; [`Index::mark_interrupted`] flips it to
//! `Interrupted` and [`Index::job_detail`] tells the resume / rollback code
//! exactly which operations finished, which never ran, and where any
//! displaced destination file was staged.
//!
//! Durability is graded by what a lost record costs. Operations are
//! idempotent, so losing "started" or "finished" only means redoing one, and
//! those records are batched (one transaction per [`COMMIT_INTERVAL`] or
//! [`COMMIT_EVERY_RECORDS`]) to keep a 100 000-file sync from doing 200 000
//! fsyncs. "Backup staged" is the only record of where an overwritten file
//! went, so it is committed, together with everything before it, before
//! [`Journal::backup_staged`] returns.
//!
//! Operations and results are stored as JSON so the executor's types can
//! grow fields without schema migrations; only what queries filter or count
//! on (state, attempts, bytes) has its own column.

use std::path::Path;
use std::time::{Duration, Instant};

use pcompare_core::store::{new_id, now_ms};
use pcompare_sync::exec::{ExecOptions, ExecReport, OpResult};
use pcompare_sync::journal::Journal;
use pcompare_sync::sync::{SyncMode, SyncOp};
use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSqlOutput, Type, ValueRef};
use rusqlite::{params, OptionalExtension, Row, ToSql, Transaction, TransactionBehavior};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::{normalize_key, Index, IndexError, Result};

/// Buffered journal records are committed at least this often...
pub const COMMIT_INTERVAL: Duration = Duration::from_millis(250);
/// ...or as soon as this many are waiting.
pub const COMMIT_EVERY_RECORDS: usize = 256;
/// After a failed commit, buffered records wait this long before the next try.
const RETRY_AFTER: Duration = Duration::from_secs(1);
const DAY_MS: i64 = 86_400_000;

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "camelCase")]
pub enum JobStatus {
    Running,
    Completed,
    CompletedWithErrors,
    Cancelled,
    Stopped,
    /// Was `Running` when the app last exited: crashed, killed or lost power.
    Interrupted,
    RolledBack,
}

impl JobStatus {
    /// The stored form, identical to the serde name.
    pub fn as_str(self) -> &'static str {
        match self {
            JobStatus::Running => "running",
            JobStatus::Completed => "completed",
            JobStatus::CompletedWithErrors => "completedWithErrors",
            JobStatus::Cancelled => "cancelled",
            JobStatus::Stopped => "stopped",
            JobStatus::Interrupted => "interrupted",
            JobStatus::RolledBack => "rolledBack",
        }
    }

    const ALL: [JobStatus; 7] = [
        JobStatus::Running,
        JobStatus::Completed,
        JobStatus::CompletedWithErrors,
        JobStatus::Cancelled,
        JobStatus::Stopped,
        JobStatus::Interrupted,
        JobStatus::RolledBack,
    ];
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "camelCase")]
pub enum OpState {
    Pending,
    /// An attempt started; no outcome recorded yet.
    Running,
    Done,
    Failed,
    /// Undone by a rollback.
    RolledBack,
}

impl OpState {
    /// The stored form, identical to the serde name.
    pub fn as_str(self) -> &'static str {
        match self {
            OpState::Pending => "pending",
            OpState::Running => "running",
            OpState::Done => "done",
            OpState::Failed => "failed",
            OpState::RolledBack => "rolledBack",
        }
    }

    const ALL: [OpState; 5] = [
        OpState::Pending,
        OpState::Running,
        OpState::Done,
        OpState::Failed,
        OpState::RolledBack,
    ];
}

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

sql_text_enum!(JobStatus);
sql_text_enum!(OpState);

/// A sync run about to start.
pub struct NewJob<'a> {
    pub left: &'a str,
    pub right: &'a str,
    pub mode: SyncMode,
    pub options: &'a ExecOptions,
    pub ops: &'a [SyncOp],
}

/// One row of the job history.
#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct JobSummary {
    pub id: String,
    pub left: String,
    pub right: String,
    pub mode: SyncMode,
    pub status: JobStatus,
    pub started_ms: i64,
    pub finished_ms: Option<i64>,
    pub total_ops: usize,
    pub done_ops: usize,
    pub failed_ops: usize,
    /// Operations that never finished: `Pending` or `Running`.
    pub pending_ops: usize,
    /// From the final report once finished; the running total before that.
    pub bytes_copied: u64,
}

/// One planned operation and how far it got.
#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct JobOp {
    pub op: SyncOp,
    pub state: OpState,
    pub attempts: u32,
    /// Where the destination was moved before being replaced
    /// ([`Journal::backup_staged`]); what a crash recovery restores from.
    pub staged: Option<String>,
    pub result: Option<OpResult>,
    pub updated_ms: i64,
}

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct JobDetail {
    pub summary: JobSummary,
    pub options: ExecOptions,
    /// Ordered by op id.
    pub ops: Vec<JobOp>,
}

/// Summary columns for jobs matching `{filter}` (parameters from `?2`),
/// newest first, at most `?1`. Jobs are limited before ops are aggregated
/// so a long history doesn't make every listing scan every op.
const SUMMARY_SQL: &str = "
    SELECT j.id, j.left, j.right, j.mode, j.status, j.started_ms, j.finished_ms, j.total_ops,
           COALESCE(SUM(o.state = 'done'), 0),
           COALESCE(SUM(o.state = 'failed'), 0),
           COALESCE(SUM(o.state IN ('pending', 'running')), 0),
           COALESCE(j.bytes_copied, SUM(o.bytes), 0)
    FROM (SELECT rowid AS seq, * FROM jobs WHERE {filter}
          ORDER BY started_ms DESC, rowid DESC LIMIT ?1) j
    LEFT JOIN job_ops o ON o.job_id = j.id
    GROUP BY j.id
    ORDER BY j.started_ms DESC, j.seq DESC";

impl Index {
    /// Records a new run (status `Running`) and all its operations (state
    /// `Pending`) in one transaction; returns the job id.
    pub fn create_job(&self, job: &NewJob) -> Result<String> {
        let id = new_id();
        let now = now_ms();
        let mode = enum_text(&job.mode)?;
        let options = serde_json::to_string(job.options)?;
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        {
            tx.prepare_cached(
                "INSERT INTO jobs (id, left, right, mode, options, status, started_ms, total_ops)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            )?
            .execute(params![
                id,
                normalize_key(job.left),
                normalize_key(job.right),
                mode,
                options,
                JobStatus::Running,
                now,
                job.ops.len() as i64,
            ])?;
            let mut insert = tx.prepare_cached(
                "INSERT INTO job_ops (job_id, op_id, op, state, updated_ms) VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            for op in job.ops {
                insert.execute(params![
                    id,
                    op.id,
                    serde_json::to_string(op)?,
                    OpState::Pending,
                    now
                ])?;
            }
        }
        tx.commit()?;
        Ok(id)
    }

    /// The [`Journal`] to hand the executor for `job_id`.
    pub fn journal(&self, job_id: &str) -> JobJournal<'_> {
        JobJournal {
            index: self,
            job_id: job_id.to_owned(),
            buf: Vec::new(),
            last_commit: Instant::now(),
            retry_at: None,
        }
    }

    /// Records the end of a run. Operations still `Pending` or `Running`
    /// stay so: they never ran (cancelled, stopped) or never finished.
    pub fn finish_job(&self, job_id: &str, report: &ExecReport, status: JobStatus) -> Result<()> {
        let n = self
            .conn()
            .prepare_cached(
                "UPDATE jobs SET status = ?2, finished_ms = ?3, succeeded = ?4, failed = ?5,
                     bytes_copied = ?6, cancelled = ?7, stopped = ?8, elapsed_ms = ?9
                 WHERE id = ?1",
            )?
            .execute(params![
                job_id,
                status,
                now_ms(),
                report.succeeded as i64,
                report.failed as i64,
                report.bytes_copied as i64,
                report.cancelled,
                report.stopped,
                i64::try_from(report.elapsed_ms).unwrap_or(i64::MAX),
            ])?;
        found(n, job_id)
    }

    /// Changes a job's status, e.g. `Interrupted` → `Running` when resuming
    /// or → `RolledBack` after a rollback. Setting `Running` clears the
    /// finish time; other terminal statuses set it if it isn't set yet.
    pub fn set_job_status(&self, job_id: &str, status: JobStatus) -> Result<()> {
        let finish = match status {
            JobStatus::Running => 0,
            JobStatus::Interrupted => 1,
            _ => 2,
        };
        let n = self
            .conn()
            .prepare_cached(
                "UPDATE jobs SET status = ?2,
                     finished_ms = CASE ?3 WHEN 0 THEN NULL WHEN 1 THEN finished_ms
                                   ELSE COALESCE(finished_ms, ?4) END
                 WHERE id = ?1",
            )?
            .execute(params![job_id, status, finish, now_ms()])?;
        found(n, job_id)
    }

    /// Marks operations as undone by a rollback, immediately. (A rollback
    /// loop that wants batching can use [`JobJournal::mark_rolled_back`].)
    pub fn mark_ops_rolled_back(&self, job_id: &str, op_ids: &[u32]) -> Result<usize> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let mut n = 0;
        {
            let mut stmt = tx.prepare_cached(
                "UPDATE job_ops SET state = ?3, updated_ms = ?4 WHERE job_id = ?1 AND op_id = ?2",
            )?;
            let now = now_ms();
            for &op in op_ids {
                n += stmt.execute(params![job_id, op, OpState::RolledBack, now])?;
            }
        }
        tx.commit()?;
        Ok(n)
    }

    /// Call once at app start: any job still `Running` belongs to a previous
    /// process that died, so it becomes `Interrupted`. Returns how many.
    pub fn mark_interrupted(&self) -> Result<usize> {
        Ok(self
            .conn()
            .prepare_cached("UPDATE jobs SET status = ?1 WHERE status = ?2")?
            .execute(params![JobStatus::Interrupted, JobStatus::Running])?)
    }

    /// Job history, newest first, optionally for one folder pair.
    pub fn list_jobs(&self, pair: Option<(&str, &str)>, limit: usize) -> Result<Vec<JobSummary>> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let conn = self.conn();
        let rows = match pair {
            None => {
                let mut stmt = conn.prepare_cached(&SUMMARY_SQL.replace("{filter}", "1"))?;
                let rows = stmt.query_map(params![limit], summary_row)?;
                rows.collect::<rusqlite::Result<Vec<_>>>()?
            }
            Some((left, right)) => {
                let sql = SUMMARY_SQL.replace("{filter}", "left = ?2 AND right = ?3");
                let mut stmt = conn.prepare_cached(&sql)?;
                let rows = stmt.query_map(
                    params![limit, normalize_key(left), normalize_key(right)],
                    summary_row,
                )?;
                rows.collect::<rusqlite::Result<Vec<_>>>()?
            }
        };
        Ok(rows)
    }

    /// Everything about one job, for the history view and for resume or
    /// rollback after an interruption.
    pub fn job_detail(&self, job_id: &str) -> Result<JobDetail> {
        let conn = self.conn();
        let summary = conn
            .prepare_cached(&SUMMARY_SQL.replace("{filter}", "id = ?2"))?
            .query_row(params![1, job_id], summary_row)
            .optional()?
            .ok_or_else(|| IndexError::JobNotFound(job_id.to_owned()))?;
        let options: String = conn
            .prepare_cached("SELECT options FROM jobs WHERE id = ?1")?
            .query_row([job_id], |r| r.get(0))?;
        let mut stmt = conn.prepare_cached(
            "SELECT op, state, attempts, staged, result, updated_ms
             FROM job_ops WHERE job_id = ?1 ORDER BY op_id",
        )?;
        let ops = stmt
            .query_map([job_id], |r| {
                Ok(JobOp {
                    op: json_col(r, 0)?,
                    state: r.get(1)?,
                    attempts: r.get(2)?,
                    staged: r.get(3)?,
                    result: r
                        .get::<_, Option<String>>(4)?
                        .map(|s| parse_json(4, &s))
                        .transpose()?,
                    updated_ms: r.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(JobDetail {
            summary,
            options: serde_json::from_str(&options)?,
            ops,
        })
    }

    /// The most recent `Interrupted` job for this pair, if any, so the UI can
    /// offer to resume or roll it back.
    pub fn interrupted_job(&self, left: &str, right: &str) -> Result<Option<JobSummary>> {
        let sql = SUMMARY_SQL.replace("{filter}", "left = ?2 AND right = ?3 AND status = ?4");
        Ok(self
            .conn()
            .prepare_cached(&sql)?
            .query_row(
                params![
                    1,
                    normalize_key(left),
                    normalize_key(right),
                    JobStatus::Interrupted
                ],
                summary_row,
            )
            .optional()?)
    }

    /// Deletes finished jobs (and their operations) that ended more than
    /// `older_than_days` ago. `Running` and `Interrupted` jobs are kept
    /// whatever their age: they may still need recovering.
    pub fn prune_jobs(&self, older_than_days: u32) -> Result<usize> {
        let cutoff = now_ms() - i64::from(older_than_days) * DAY_MS;
        Ok(self
            .conn()
            .prepare_cached(
                "DELETE FROM jobs WHERE status NOT IN (?2, ?3) AND COALESCE(finished_ms, started_ms) < ?1",
            )?
            .execute(params![cutoff, JobStatus::Running, JobStatus::Interrupted])?)
    }
}

fn found(rows: usize, job_id: &str) -> Result<()> {
    if rows == 0 {
        return Err(IndexError::JobNotFound(job_id.to_owned()));
    }
    Ok(())
}

fn summary_row(r: &Row) -> rusqlite::Result<JobSummary> {
    let mode: String = r.get(3)?;
    Ok(JobSummary {
        id: r.get(0)?,
        left: r.get(1)?,
        right: r.get(2)?,
        mode: serde_json::from_value(serde_json::Value::String(mode))
            .map_err(|e| rusqlite::Error::FromSqlConversionFailure(3, Type::Text, Box::new(e)))?,
        status: r.get(4)?,
        started_ms: r.get(5)?,
        finished_ms: r.get(6)?,
        total_ops: r.get::<_, i64>(7)? as usize,
        done_ops: r.get::<_, i64>(8)? as usize,
        failed_ops: r.get::<_, i64>(9)? as usize,
        pending_ops: r.get::<_, i64>(10)? as usize,
        bytes_copied: r.get::<_, i64>(11)? as u64,
    })
}

fn json_col<T: DeserializeOwned>(r: &Row, idx: usize) -> rusqlite::Result<T> {
    parse_json(idx, &r.get::<_, String>(idx)?)
}

fn parse_json<T: DeserializeOwned>(idx: usize, s: &str) -> rusqlite::Result<T> {
    serde_json::from_str(s)
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(idx, Type::Text, Box::new(e)))
}

/// A unit enum's serde name (`"twoWay"`), stored without JSON quotes.
fn enum_text<T: Serialize>(v: &T) -> Result<String> {
    match serde_json::to_value(v)? {
        serde_json::Value::String(s) => Ok(s),
        other => Err(IndexError::Invalid(format!(
            "expected a unit enum, got {other}"
        ))),
    }
}

/// One buffered journal record.
enum Rec {
    Started {
        op: u32,
        attempt: u32,
        ms: i64,
    },
    Staged {
        op: u32,
        path: String,
        ms: i64,
    },
    Finished {
        op: u32,
        state: OpState,
        attempts: u32,
        bytes: u64,
        json: String,
        ms: i64,
    },
    RolledBack {
        op: u32,
        ms: i64,
    },
}

/// The database-backed [`Journal`] for one job. Records are buffered and
/// committed in batches (see the module docs); the connection is locked
/// only while committing. Whatever is still buffered is committed on drop.
///
/// The [`Journal`] methods cannot return errors, so failures are logged to
/// stderr and the records kept for the next attempt; call
/// [`JobJournal::commit`] to see errors.
pub struct JobJournal<'a> {
    index: &'a Index,
    job_id: String,
    buf: Vec<Rec>,
    last_commit: Instant,
    /// Set after a failed commit: don't retry on every record.
    retry_at: Option<Instant>,
}

impl JobJournal<'_> {
    pub fn job_id(&self) -> &str {
        &self.job_id
    }

    /// Buffers "operation undone by a rollback".
    pub fn mark_rolled_back(&mut self, op_id: u32) {
        self.push(Rec::RolledBack {
            op: op_id,
            ms: now_ms(),
        });
    }

    /// Commits everything buffered now.
    pub fn commit(&mut self) -> Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let result = (|| {
            let mut conn = self.index.conn();
            let tx = conn.transaction()?;
            write_records(&tx, &self.job_id, &self.buf)?;
            tx.commit()
        })();
        match result {
            Ok(()) => {
                self.buf.clear();
                self.last_commit = Instant::now();
                self.retry_at = None;
                Ok(())
            }
            Err(e) => {
                self.retry_at = Some(Instant::now() + RETRY_AFTER);
                Err(e.into())
            }
        }
    }

    fn push(&mut self, rec: Rec) {
        self.buf.push(rec);
        let now = Instant::now();
        let due = self.buf.len() >= COMMIT_EVERY_RECORDS
            || now.duration_since(self.last_commit) >= COMMIT_INTERVAL;
        if due && self.retry_at.is_none_or(|t| now >= t) {
            self.commit_logged();
        }
    }

    fn commit_logged(&mut self) {
        if let Err(e) = self.commit() {
            eprintln!(
                "pcompare-index: cannot record progress of sync job {}: {e}",
                self.job_id
            );
        }
    }
}

fn write_records(tx: &Transaction, job_id: &str, recs: &[Rec]) -> rusqlite::Result<()> {
    let mut started = tx.prepare_cached(
        "UPDATE job_ops SET state = ?3, attempts = ?4, updated_ms = ?5 WHERE job_id = ?1 AND op_id = ?2",
    )?;
    let mut staged = tx.prepare_cached(
        "UPDATE job_ops SET staged = ?3, updated_ms = ?4 WHERE job_id = ?1 AND op_id = ?2",
    )?;
    let mut finished = tx.prepare_cached(
        "UPDATE job_ops SET state = ?3, attempts = ?4, bytes = ?5, result = ?6, updated_ms = ?7
         WHERE job_id = ?1 AND op_id = ?2",
    )?;
    let mut rolled_back = tx.prepare_cached(
        "UPDATE job_ops SET state = ?3, updated_ms = ?4 WHERE job_id = ?1 AND op_id = ?2",
    )?;
    for rec in recs {
        match rec {
            Rec::Started { op, attempt, ms } => {
                started.execute(params![job_id, op, OpState::Running, attempt, ms])?;
            }
            Rec::Staged { op, path, ms } => {
                staged.execute(params![job_id, op, path, ms])?;
            }
            Rec::Finished {
                op,
                state,
                attempts,
                bytes,
                json,
                ms,
            } => {
                finished.execute(params![
                    job_id,
                    op,
                    state,
                    attempts,
                    *bytes as i64,
                    json,
                    ms
                ])?;
            }
            Rec::RolledBack { op, ms } => {
                rolled_back.execute(params![job_id, op, OpState::RolledBack, ms])?;
            }
        }
    }
    Ok(())
}

impl Journal for JobJournal<'_> {
    fn op_started(&mut self, op_id: u32, attempt: u32) {
        self.push(Rec::Started {
            op: op_id,
            attempt,
            ms: now_ms(),
        });
    }

    fn backup_staged(&mut self, op_id: u32, staged: &Path) {
        self.buf.push(Rec::Staged {
            op: op_id,
            path: staged.to_string_lossy().into_owned(),
            ms: now_ms(),
        });
        self.commit_logged();
    }

    fn op_finished(&mut self, result: &OpResult) {
        match serde_json::to_string(result) {
            Ok(json) => self.push(Rec::Finished {
                op: result.id,
                state: if result.ok {
                    OpState::Done
                } else {
                    OpState::Failed
                },
                attempts: result.attempts,
                bytes: result.bytes,
                json,
                ms: now_ms(),
            }),
            Err(e) => eprintln!(
                "pcompare-index: cannot encode result of op {}: {e}",
                result.id
            ),
        }
    }

    fn flush(&mut self) {
        self.commit_logged();
    }
}

impl Drop for JobJournal<'_> {
    fn drop(&mut self) {
        self.commit_logged();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn op(id: u32, rel: &str) -> SyncOp {
        serde_json::from_value(json!({
            "id": id, "kind": "copy", "direction": "toRight", "relPath": rel,
            "isDir": false, "size": 10, "reason": "left newer",
        }))
        .unwrap()
    }

    fn result(id: u32, rel: &str, ok: bool, attempts: u32) -> OpResult {
        serde_json::from_value(json!({
            "id": id, "kind": "copy", "direction": "toRight", "relPath": rel, "ok": ok,
            "error": if ok { None } else { Some("locked") }, "attempts": attempts,
            "bytes": if ok { 10 } else { 0 },
        }))
        .unwrap()
    }

    fn new_job(index: &Index, left: &str, right: &str, n: u32) -> String {
        let ops: Vec<SyncOp> = (1..=n).map(|i| op(i, &format!("f{i}.txt"))).collect();
        index
            .create_job(&NewJob {
                left,
                right,
                mode: SyncMode::TwoWay,
                options: &ExecOptions::default(),
                ops: &ops,
            })
            .unwrap()
    }

    #[test]
    fn stored_names_match_serde() {
        for s in JobStatus::ALL {
            assert_eq!(json!(s), json!(s.as_str()));
        }
        for s in OpState::ALL {
            assert_eq!(json!(s), json!(s.as_str()));
        }
    }

    #[test]
    fn job_lifecycle() {
        let index = Index::open_in_memory().unwrap();
        let id = new_job(&index, "/l/", "/r", 3);

        let s = &index.list_jobs(None, 10).unwrap()[0];
        assert_eq!(
            (s.id.as_str(), s.left.as_str(), s.status),
            (id.as_str(), "/l", JobStatus::Running)
        );
        assert_eq!((s.total_ops, s.pending_ops, s.done_ops), (3, 3, 0));
        assert_eq!(s.mode, SyncMode::TwoWay);

        {
            let mut j = index.journal(&id);
            j.op_started(1, 1);
            j.op_finished(&result(1, "f1.txt", true, 1));
            j.op_started(2, 1);
            j.op_started(2, 2);
            j.op_finished(&result(2, "f2.txt", false, 2));
            j.flush();
        }
        let report = ExecReport {
            succeeded: 1,
            failed: 1,
            bytes_copied: 10,
            cancelled: true,
            not_run: vec![3],
            ..Default::default()
        };
        index
            .finish_job(&id, &report, JobStatus::Cancelled)
            .unwrap();

        let d = index.job_detail(&id).unwrap();
        assert_eq!(d.summary.status, JobStatus::Cancelled);
        assert!(d.summary.finished_ms.is_some());
        assert_eq!(
            (
                d.summary.done_ops,
                d.summary.failed_ops,
                d.summary.pending_ops
            ),
            (1, 1, 1)
        );
        assert_eq!(d.summary.bytes_copied, 10);
        assert_eq!(
            d.options.retry_attempts,
            ExecOptions::default().retry_attempts
        );
        let ids: Vec<u32> = d.ops.iter().map(|o| o.op.id).collect();
        assert_eq!(ids, [1, 2, 3]);
        assert_eq!((d.ops[0].state, d.ops[0].attempts), (OpState::Done, 1));
        assert_eq!((d.ops[1].state, d.ops[1].attempts), (OpState::Failed, 2));
        assert_eq!(
            d.ops[1].result.as_ref().unwrap().error.as_deref(),
            Some("locked")
        );
        assert_eq!(
            (d.ops[2].state, d.ops[2].result.is_none()),
            (OpState::Pending, true),
            "never ran"
        );

        assert!(index.interrupted_job("/l", "/r").unwrap().is_none());
        assert!(matches!(
            index.job_detail("nope"),
            Err(IndexError::JobNotFound(_))
        ));
        assert!(matches!(
            index.finish_job("nope", &report, JobStatus::Completed),
            Err(IndexError::JobNotFound(_))
        ));

        assert_eq!(index.mark_ops_rolled_back(&id, &[1]).unwrap(), 1);
        index.set_job_status(&id, JobStatus::RolledBack).unwrap();
        let d = index.job_detail(&id).unwrap();
        assert_eq!(
            (d.summary.status, d.ops[0].state),
            (JobStatus::RolledBack, OpState::RolledBack)
        );
    }

    #[test]
    fn interrupted_jobs_and_history_order() {
        let index = Index::open_in_memory().unwrap();
        let a = new_job(&index, "/l", "/r", 1);
        let b = new_job(&index, "/l", "/r", 2);
        let other = new_job(&index, "/x", "/y", 1);
        index
            .finish_job(&a, &ExecReport::default(), JobStatus::Completed)
            .unwrap();

        assert_eq!(
            index.mark_interrupted().unwrap(),
            2,
            "b and other were running"
        );
        let hit = index.interrupted_job("/l/", "/r").unwrap().unwrap();
        assert_eq!(
            (hit.id.as_str(), hit.status, hit.pending_ops),
            (b.as_str(), JobStatus::Interrupted, 2)
        );

        let all: Vec<String> = index
            .list_jobs(None, 10)
            .unwrap()
            .into_iter()
            .map(|s| s.id)
            .collect();
        assert_eq!(all, [other.clone(), b.clone(), a.clone()], "newest first");
        let pair: Vec<String> = index
            .list_jobs(Some(("/l", "/r")), 10)
            .unwrap()
            .into_iter()
            .map(|s| s.id)
            .collect();
        assert_eq!(pair, [b.clone(), a]);
        assert_eq!(index.list_jobs(None, 1).unwrap().len(), 1);

        // Resuming puts it back to Running and clears the finish time.
        index.set_job_status(&b, JobStatus::Running).unwrap();
        assert!(index.interrupted_job("/l", "/r").unwrap().is_none());
        assert!(index.job_detail(&b).unwrap().summary.finished_ms.is_none());
    }

    #[test]
    fn records_are_batched_and_flushed_on_drop() {
        let index = Index::open_in_memory().unwrap();
        let id = new_job(&index, "/l", "/r", 600);
        {
            let mut j = index.journal(&id);
            for i in 1..=600 {
                j.op_finished(&result(i, "f", true, 1));
            }
            // At least two full batches of 256 are committed by now.
            assert!(index.job_detail(&id).unwrap().summary.done_ops >= 512);
        }
        let s = index.job_detail(&id).unwrap().summary;
        assert_eq!(
            (s.done_ops, s.pending_ops, s.bytes_copied),
            (600, 0, 6000),
            "running byte total"
        );
    }

    #[test]
    fn backup_staged_is_durable_immediately() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        let writer = Index::open(&path).unwrap();
        let id = new_job(&writer, "/l", "/r", 2);
        let mut j = writer.journal(&id);
        j.op_started(1, 1);
        j.backup_staged(1, Path::new("/r/.f1.txt.pcompare-old"));

        // Another connection, as after a crash, sees it without a flush.
        let reader = Index::open(&path).unwrap();
        let d = reader.job_detail(&id).unwrap();
        assert_eq!(d.ops[0].staged.as_deref(), Some("/r/.f1.txt.pcompare-old"));
        assert_eq!(
            (d.ops[0].state, d.ops[0].attempts),
            (OpState::Running, 1),
            "earlier records too"
        );
        drop(j);
    }

    #[test]
    fn prune_keeps_recent_and_unfinished_jobs() {
        let index = Index::open_in_memory().unwrap();
        let old = new_job(&index, "/l", "/r", 3);
        let recent = new_job(&index, "/l", "/r", 1);
        let stuck = new_job(&index, "/l", "/r", 1);
        index
            .finish_job(&old, &ExecReport::default(), JobStatus::Completed)
            .unwrap();
        index
            .finish_job(&recent, &ExecReport::default(), JobStatus::Completed)
            .unwrap();
        let long_ago = now_ms() - 40 * DAY_MS;
        index
            .conn()
            .execute(
                "UPDATE jobs SET started_ms = ?1, finished_ms = ?1 WHERE id IN (?2, ?3)",
                params![long_ago, old, stuck],
            )
            .unwrap();
        index.mark_interrupted().unwrap();

        assert_eq!(index.prune_jobs(30).unwrap(), 1);
        let left: Vec<String> = index
            .list_jobs(None, 10)
            .unwrap()
            .into_iter()
            .map(|s| s.id)
            .collect();
        assert!(left.contains(&recent) && left.contains(&stuck) && !left.contains(&old));
        let ops: i64 = index
            .conn()
            .query_row("SELECT COUNT(*) FROM job_ops", [], |r| r.get(0))
            .unwrap();
        assert_eq!(ops, 2, "ops of the pruned job cascade");
        assert_eq!(index.stats().unwrap().jobs, 2);
    }
}
