//! Sync automation for PCompare: saved folder-sync tasks that run on a
//! schedule, on file-system changes, at startup or on demand, unattended,
//! with safety guards and failure notifications.
//!
//! - [`task`]: the saved task model and its validation;
//! - [`trigger`]: interval / daily / cron / real-time / startup triggers and
//!   their next-fire arithmetic;
//! - [`store`]: `automation.json` (tasks + settings);
//! - [`scheduler`]: the runtime that fires triggers, runs tasks through the
//!   app's [`scheduler::TaskRunner`], retries, records history in the scan
//!   database (`pcompare-index`) and notifies;
//! - [`watcher`]: real-time file-system watching for real-time triggers;
//! - [`policy`]: whether and what to notify.
//!
//! The sync itself is the same journaled, crash-resumable pipeline the
//! Folder Sync view uses, so an automated run can be inspected, resumed or
//! rolled back from Sync History like any other.

pub mod policy;
pub mod scheduler;
pub mod store;
pub mod task;
pub mod trigger;
pub mod watcher;

pub use scheduler::{
    RunContext, RunOutcome, RunPhase, RunProgress, Scheduler, SchedulerHost, TaskRunState,
    TaskRunner, TaskStatus,
};
pub use store::{AutomationFile, AutomationSettings, TaskStore};
pub use task::{NotifyRules, RunRetry, SafetyOptions, SyncTask, TaskIssue};
pub use trigger::Trigger;

#[derive(Debug, thiserror::Error)]
pub enum AutomationError {
    #[error("{0}")]
    Core(#[from] pcompare_core::CoreError),
    #[error("{0}")]
    Index(#[from] pcompare_index::IndexError),
    #[error("{0}")]
    Notify(#[from] pcompare_notify::NotifyError),
    #[error("{path}: {message}")]
    File { path: String, message: String },
    /// Validation errors; nothing was saved.
    #[error("{}", .0.iter().filter(|i| i.error).map(|i| i.message.as_str()).collect::<Vec<_>>().join("; "))]
    Invalid(Vec<TaskIssue>),
    #[error("automation task {0} not found")]
    TaskNotFound(String),
}

pub type Result<T> = std::result::Result<T, AutomationError>;
