//! What a notification says, independent of where it goes. Each transport
//! renders this into its own format (Adaptive Card, Block Kit, HTML+text
//! email, raw JSON).

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum Severity {
    Info,
    Success,
    Warning,
    Error,
}

/// Why this notification was sent.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum NotifyReason {
    /// The run failed (after its retries) or was aborted by a safety check.
    Failure,
    /// Completed, but some operations failed or conflicts were skipped.
    Warning,
    Success,
    /// The first successful run after one or more failures.
    Recovered,
    /// "Send test" from the channel settings.
    Test,
}

/// A label/value line ("Mode" → "Mirror left → right").
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Fact {
    pub label: String,
    pub value: String,
}

/// One failed item.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct FailureLine {
    pub path: String,
    pub error: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Notification {
    pub reason: NotifyReason,
    pub severity: Severity,
    /// One line, e.g. "Sync failed: Nightly backup".
    pub title: String,
    /// A sentence or two of plain text explaining what happened.
    pub summary: String,
    pub task_id: String,
    pub task_name: String,
    pub left: String,
    pub right: String,
    /// Mode, trigger, counts, duration, bytes, host... in display order.
    pub facts: Vec<Fact>,
    /// The first few failures (the sender caps how many it lists).
    pub failures: Vec<FailureLine>,
    /// All failures, including those not in `failures`.
    pub total_failures: usize,
    pub started_ms: i64,
    pub finished_ms: i64,
    /// The machine that ran the sync.
    pub host: String,
    pub app_version: String,
}
