//! Deciding whether a finished run is worth telling anyone about, and
//! what the notification says.
//!
//! Repeat suppression is a trade-off between alert fatigue and missing a
//! persisting outage. A task that runs every five minutes against a share
//! that's down would otherwise post the same failure to Teams 288 times a
//! day, and people learn to ignore the channel. So the *same* problem (see
//! [`fingerprint`]) is re-sent only after `NotifyRules::repeat_after_minutes`
//! — a reminder that it's still broken, not a stream. A *different* problem
//! always goes out at once, and so does the recovery, so nobody is left
//! believing an outage is still on. Setting the minutes to 0 sends every
//! failure. Successes are never suppressed: they're opt-in and the user
//! asked for every one.

use pcompare_index::{AutomationRun, RunStatus, RunTrigger, TaskState};
use pcompare_notify::{Fact, Notification, NotifyReason, Severity};
use pcompare_sync::sync::SyncMode;

use crate::scheduler::RunOutcome;
use crate::task::{NotifyRules, SyncTask};

/// Identifies "the same problem" for repeat suppression: the status plus
/// the message with volatile parts (numbers, timestamps) removed, so
/// "3 files locked" and "4 files locked" count as one recurring failure.
///
/// Every run of letters and digits that contains a digit becomes `#` (so
/// ids, counts, dates and times all fold away), whitespace is collapsed
/// and case ignored.
pub fn fingerprint(status: RunStatus, message: Option<&str>) -> String {
    let mut out = format!("{}|", status.as_str());
    let message = message.unwrap_or("").trim();
    let mut word = String::new();
    let mut pending_space = false;
    let flush = |word: &mut String, out: &mut String| {
        if word.chars().any(|c| c.is_ascii_digit()) {
            out.push('#');
        } else {
            out.extend(word.chars().flat_map(char::to_lowercase));
        }
        word.clear();
    };
    for c in message.chars() {
        if c.is_alphanumeric() {
            if pending_space {
                out.push(' ');
                pending_space = false;
            }
            word.push(c);
            continue;
        }
        flush(&mut word, &mut out);
        if c.is_whitespace() {
            pending_space = !out.ends_with('|');
        } else {
            if pending_space {
                out.push(' ');
                pending_space = false;
            }
            out.push(c);
        }
    }
    flush(&mut word, &mut out);
    out
}

/// Whether to notify about a finished run (the final attempt, never an
/// intermediate one that will be retried) and why.
///
/// `prev` is the task's state *before* this run was recorded, so
/// `prev.consecutive_failures > 0` means the previous run failed.
///
/// - failed, aborted, interrupted → `Failure` (if `on_failure`);
/// - completed with errors → `Warning` (if `on_warning`);
/// - succeeded → `Recovered` after a failure (if `on_recovery`), otherwise
///   `Success` (if `on_success`);
/// - nothing to do → `Recovered` after a failure (if `on_recovery`),
///   otherwise nothing: "still in sync" isn't news;
/// - cancelled → nothing (the user did it).
///
/// Failures and warnings with the same fingerprint as the last notification
/// are suppressed for `repeat_after_minutes` (see the module docs).
pub fn decide(
    rules: &NotifyRules,
    status: RunStatus,
    prev: &TaskState,
    fingerprint: &str,
    now_ms: i64,
) -> Option<NotifyReason> {
    let after_failure = prev.consecutive_failures > 0;
    let reason = match status {
        RunStatus::Failed | RunStatus::Aborted | RunStatus::Interrupted => {
            rules.on_failure.then_some(NotifyReason::Failure)?
        }
        RunStatus::CompletedWithErrors => rules.on_warning.then_some(NotifyReason::Warning)?,
        RunStatus::Succeeded if after_failure && rules.on_recovery => NotifyReason::Recovered,
        RunStatus::Succeeded => rules.on_success.then_some(NotifyReason::Success)?,
        RunStatus::NoChanges if after_failure && rules.on_recovery => NotifyReason::Recovered,
        RunStatus::NoChanges | RunStatus::Cancelled | RunStatus::Running => return None,
    };
    if matches!(reason, NotifyReason::Failure | NotifyReason::Warning)
        && rules.repeat_after_minutes > 0
    {
        let window_ms = i64::from(rules.repeat_after_minutes) * 60_000;
        let same = prev.last_notified_fingerprint.as_deref() == Some(fingerprint);
        if same
            && prev
                .last_notified_ms
                .is_some_and(|at| now_ms - at < window_ms)
        {
            return None;
        }
    }
    Some(reason)
}

/// Builds the notification for a run: title, summary sentence, facts
/// (mode, trigger, attempt, counts, data copied, duration, job id) and the
/// first `max_failures` failures.
pub fn build_notification(
    task: &SyncTask,
    run: &AutomationRun,
    outcome: &RunOutcome,
    reason: NotifyReason,
    host: &str,
    app_version: &str,
    max_failures: usize,
) -> Notification {
    let name = &task.name;
    let (severity, title) = match reason {
        NotifyReason::Failure => (
            Severity::Error,
            match outcome.status {
                RunStatus::Aborted => format!("Sync stopped by a safety check: {name}"),
                RunStatus::Interrupted => format!("Sync interrupted: {name}"),
                _ => format!("Sync failed: {name}"),
            },
        ),
        NotifyReason::Warning => (
            Severity::Warning,
            format!("Sync finished with problems: {name}"),
        ),
        NotifyReason::Success => (Severity::Success, format!("Sync completed: {name}")),
        NotifyReason::Recovered => (Severity::Success, format!("Sync working again: {name}")),
        NotifyReason::Test => (Severity::Info, format!("Test notification: {name}")),
    };
    let finished_ms = run
        .finished_ms
        .unwrap_or(run.started_ms)
        .max(run.started_ms);
    let total_failures = outcome.ops_failed.max(outcome.failures.len());
    let message = outcome
        .message
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty());
    let summary = summary(task, run, outcome, reason, message, total_failures);

    let mut facts = vec![
        fact("Mode", mode_label(task.mode)),
        fact("Trigger", trigger_label(run.trigger)),
    ];
    if run.attempt > 1 || run.trigger == RunTrigger::Retry {
        fact_push(
            &mut facts,
            "Attempt",
            format!("{} of {}", run.attempt, task.retry.attempts + 1),
        );
    }
    if outcome.ops_total > 0 {
        let mut ops = format!("{} of {} done", outcome.ops_done, outcome.ops_total);
        if outcome.ops_failed > 0 {
            ops.push_str(&format!(", {} failed", outcome.ops_failed));
        }
        fact_push(&mut facts, "Operations", ops);
    }
    if outcome.conflicts > 0 {
        fact_push(
            &mut facts,
            "Conflicts skipped",
            outcome.conflicts.to_string(),
        );
    }
    if outcome.bytes_copied > 0 {
        fact_push(&mut facts, "Data copied", human_bytes(outcome.bytes_copied));
    }
    if run.finished_ms.is_some() {
        fact_push(
            &mut facts,
            "Duration",
            human_duration(finished_ms - run.started_ms),
        );
    }
    if let Some(job) = &outcome.job_id {
        fact_push(&mut facts, "Sync job", job.clone());
    }

    Notification {
        reason,
        severity,
        title,
        summary,
        task_id: task.id.clone(),
        task_name: task.name.clone(),
        left: task.left.clone(),
        right: task.right.clone(),
        facts,
        failures: outcome
            .failures
            .iter()
            .take(max_failures)
            .cloned()
            .collect(),
        total_failures,
        started_ms: run.started_ms,
        finished_ms,
        host: host.to_owned(),
        app_version: app_version.to_owned(),
    }
}

fn summary(
    task: &SyncTask,
    run: &AutomationRun,
    outcome: &RunOutcome,
    reason: NotifyReason,
    message: Option<&str>,
    failures: usize,
) -> String {
    let pair = format!("{} and {}", task.left, task.right);
    let attempts = if run.attempt > 1 {
        format!(" after {} attempts", run.attempt)
    } else {
        String::new()
    };
    let because = message
        .map(|m| format!(": {}", m.trim_end_matches('.')))
        .unwrap_or_default();
    let mut s = match (reason, outcome.status) {
        (NotifyReason::Failure, RunStatus::Aborted) => {
            format!("The sync between {pair} was stopped before changing anything{because}.")
        }
        (NotifyReason::Failure, RunStatus::Interrupted) => {
            format!(
                "PCompare exited while syncing {pair} (crash, forced quit or power loss){because}."
            )
        }
        (NotifyReason::Failure, _) => format!("The sync between {pair} failed{attempts}{because}."),
        (NotifyReason::Warning, _) => {
            let mut parts = Vec::new();
            if failures > 0 {
                parts.push(plural(failures, "item", "items") + " couldn't be synced");
            }
            if outcome.conflicts > 0 {
                parts
                    .push(plural(outcome.conflicts, "conflict was", "conflicts were") + " skipped");
            }
            let detail = if parts.is_empty() {
                message.unwrap_or("some operations failed").to_owned()
            } else {
                parts.join(" and ")
            };
            format!("The sync between {pair} finished, but {detail}.")
        }
        (NotifyReason::Recovered, _) => {
            format!("The sync between {pair} is working again: this run finished without errors.")
        }
        _ if outcome.ops_done > 0 => format!(
            "Synced {pair}: {} done.",
            plural(outcome.ops_done, "operation", "operations")
        ),
        _ => format!("Synced {pair}."),
    };
    if reason == NotifyReason::Failure && outcome.status == RunStatus::Interrupted {
        s.push_str(" The next run picks up where it left off.");
    }
    s
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

fn fact(label: &str, value: impl Into<String>) -> Fact {
    Fact {
        label: label.to_owned(),
        value: value.into(),
    }
}

fn fact_push(facts: &mut Vec<Fact>, label: &str, value: String) {
    facts.push(fact(label, value));
}

pub(crate) fn mode_label(mode: SyncMode) -> &'static str {
    match mode {
        SyncMode::UpdateLeftToRight => "Update left → right",
        SyncMode::UpdateRightToLeft => "Update right → left",
        SyncMode::UpdateBoth => "Update both",
        SyncMode::MirrorLeftToRight => "Mirror left → right",
        SyncMode::MirrorRightToLeft => "Mirror right → left",
        SyncMode::TwoWay => "Two-way",
    }
}

pub(crate) fn trigger_label(trigger: RunTrigger) -> &'static str {
    match trigger {
        RunTrigger::Manual => "Run now",
        RunTrigger::Interval => "Interval",
        RunTrigger::Daily => "Daily schedule",
        RunTrigger::Cron => "Cron schedule",
        RunTrigger::Realtime => "File changes",
        RunTrigger::Rescan => "Periodic rescan",
        RunTrigger::Startup => "App startup",
        RunTrigger::CatchUp => "Missed schedule",
        RunTrigger::Retry => "Retry",
    }
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["KB", "MB", "GB", "TB", "PB"];
    if bytes < 1024 {
        return format!("{bytes} bytes");
    }
    let mut v = bytes as f64 / 1024.0;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    if v >= 100.0 {
        format!("{v:.0} {}", UNITS[unit])
    } else {
        format!("{v:.1} {}", UNITS[unit])
    }
}

fn human_duration(ms: i64) -> String {
    let secs = ms.max(0) / 1000;
    match secs {
        0 => "under a second".into(),
        s if s < 60 => format!("{s} s"),
        s if s < 3600 => format!("{} min {} s", s / 60, s % 60),
        s => format!("{} h {} min", s / 3600, (s % 3600) / 60),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pcompare_notify::FailureLine;

    fn rules() -> NotifyRules {
        NotifyRules::default()
    }

    fn state(failures: u32, notified: Option<(i64, &str)>) -> TaskState {
        TaskState {
            task_id: "t".into(),
            consecutive_failures: failures,
            last_notified_ms: notified.map(|n| n.0),
            last_notified_fingerprint: notified.map(|n| n.1.to_owned()),
            ..TaskState::default()
        }
    }

    #[test]
    fn fingerprints_ignore_volatile_parts() {
        let a = fingerprint(RunStatus::Failed, Some("3 files locked"));
        let b = fingerprint(RunStatus::Failed, Some("  4   Files LOCKED "));
        assert_eq!(a, b);
        assert_eq!(a, "failed|# files locked");
        assert_eq!(
            fingerprint(
                RunStatus::Failed,
                Some("Job 18f3a2b0c timed out at 2026-10-13 02:00:05")
            ),
            fingerprint(
                RunStatus::Failed,
                Some("Job 19aa00 timed out at 2026-10-14 03:10:00")
            )
        );
        assert_ne!(
            a,
            fingerprint(RunStatus::Aborted, Some("3 files locked")),
            "status matters"
        );
        assert_ne!(a, fingerprint(RunStatus::Failed, Some("disk full")));
        assert_eq!(fingerprint(RunStatus::Interrupted, None), "interrupted|");
        assert_eq!(
            fingerprint(
                RunStatus::Failed,
                Some("The right folder \\\\nas\\backup isn't reachable")
            ),
            "failed|the right folder \\\\nas\\backup isn't reachable"
        );
    }

    #[test]
    fn status_to_reason() {
        let fresh = state(0, None);
        let failing = state(2, None);
        let r = rules();
        let d = |status, prev: &TaskState| decide(&r, status, prev, "fp", 1_000);
        assert_eq!(d(RunStatus::Failed, &fresh), Some(NotifyReason::Failure));
        assert_eq!(d(RunStatus::Aborted, &fresh), Some(NotifyReason::Failure));
        assert_eq!(
            d(RunStatus::Interrupted, &fresh),
            Some(NotifyReason::Failure)
        );
        assert_eq!(
            d(RunStatus::CompletedWithErrors, &fresh),
            Some(NotifyReason::Warning)
        );
        assert_eq!(d(RunStatus::Succeeded, &fresh), None, "success is opt-in");
        assert_eq!(
            d(RunStatus::Succeeded, &failing),
            Some(NotifyReason::Recovered)
        );
        assert_eq!(
            d(RunStatus::NoChanges, &failing),
            Some(NotifyReason::Recovered)
        );
        assert_eq!(d(RunStatus::NoChanges, &fresh), None);
        assert_eq!(d(RunStatus::Cancelled, &failing), None);

        let all = NotifyRules {
            on_success: true,
            on_recovery: false,
            ..rules()
        };
        assert_eq!(
            decide(&all, RunStatus::Succeeded, &failing, "fp", 0),
            Some(NotifyReason::Success)
        );
        assert_eq!(decide(&all, RunStatus::NoChanges, &failing, "fp", 0), None);
        let none = NotifyRules {
            on_failure: false,
            on_warning: false,
            ..rules()
        };
        assert_eq!(decide(&none, RunStatus::Failed, &fresh, "fp", 0), None);
        assert_eq!(
            decide(&none, RunStatus::CompletedWithErrors, &fresh, "fp", 0),
            None
        );
    }

    #[test]
    fn same_failure_is_suppressed_within_the_window_only() {
        let r = rules(); // 360 minutes
        let hour = 3_600_000;
        let prev = state(3, Some((0, "failed|# files locked")));
        assert_eq!(
            decide(&r, RunStatus::Failed, &prev, "failed|# files locked", hour),
            None
        );
        assert_eq!(
            decide(
                &r,
                RunStatus::Failed,
                &prev,
                "failed|# files locked",
                6 * hour
            ),
            Some(NotifyReason::Failure),
            "reminder"
        );
        assert_eq!(
            decide(&r, RunStatus::Failed, &prev, "failed|disk full", hour),
            Some(NotifyReason::Failure),
            "new problem"
        );
        let warn = state(0, Some((0, "completedWithErrors|x")));
        assert_eq!(
            decide(
                &r,
                RunStatus::CompletedWithErrors,
                &warn,
                "completedWithErrors|x",
                hour
            ),
            None
        );
        // Recovery is never suppressed.
        let recovered = state(3, Some((0, "succeeded|")));
        assert_eq!(
            decide(&r, RunStatus::Succeeded, &recovered, "succeeded|", 1),
            Some(NotifyReason::Recovered)
        );
        // 0 = every time.
        let every = NotifyRules {
            repeat_after_minutes: 0,
            ..rules()
        };
        assert_eq!(
            decide(&every, RunStatus::Failed, &prev, "failed|# files locked", 1),
            Some(NotifyReason::Failure)
        );
    }

    fn task() -> SyncTask {
        serde_json::from_value(serde_json::json!({
            "id": "t1", "name": "Nightly backup", "enabled": true,
            "left": "C:\\Data", "right": "\\\\nas\\backup", "mode": "mirrorLeftToRight",
        }))
        .unwrap()
    }

    fn run(attempt: u32, status: RunStatus) -> AutomationRun {
        AutomationRun {
            id: "r1".into(),
            task_id: "t1".into(),
            task_name: "Nightly backup".into(),
            trigger: if attempt > 1 {
                RunTrigger::Retry
            } else {
                RunTrigger::Daily
            },
            attempt,
            status,
            started_ms: 1_000,
            finished_ms: Some(126_000),
            job_id: Some("job9".into()),
            ops_total: 10,
            ops_done: 7,
            ops_failed: 3,
            conflicts: 1,
            bytes_copied: 5 * 1024 * 1024,
            message: Some("3 files locked".into()),
            notifications: vec![],
        }
    }

    #[test]
    fn failure_notification_content() {
        let mut outcome =
            RunOutcome::failed("The right folder \\\\nas\\backup isn't reachable.", true);
        outcome.failures = (0..5)
            .map(|i| FailureLine {
                path: format!("f{i}"),
                error: "locked".into(),
            })
            .collect();
        outcome.ops_failed = 9;
        outcome.ops_total = 10;
        outcome.job_id = Some("job9".into());
        let n = build_notification(
            &task(),
            &run(3, RunStatus::Failed),
            &outcome,
            NotifyReason::Failure,
            "PC-01",
            "0.2.0",
            2,
        );
        assert_eq!(n.title, "Sync failed: Nightly backup");
        assert_eq!(n.severity, Severity::Error);
        assert_eq!(
            n.summary,
            "The sync between C:\\Data and \\\\nas\\backup failed after 3 attempts: The right folder \\\\nas\\backup isn't reachable."
        );
        assert_eq!(n.failures.len(), 2);
        assert_eq!(n.total_failures, 9);
        assert_eq!((n.started_ms, n.finished_ms), (1_000, 126_000));
        assert_eq!(
            (n.host.as_str(), n.app_version.as_str()),
            ("PC-01", "0.2.0")
        );
        let facts: Vec<(&str, &str)> = n
            .facts
            .iter()
            .map(|f| (f.label.as_str(), f.value.as_str()))
            .collect();
        assert!(facts.contains(&("Mode", "Mirror left → right")));
        assert!(facts.contains(&("Trigger", "Retry")));
        assert!(facts.contains(&("Attempt", "3 of 3")));
        assert!(facts.contains(&("Operations", "0 of 10 done, 9 failed")));
        assert!(facts.contains(&("Duration", "2 min 5 s")));
        assert!(facts.contains(&("Sync job", "job9")));
    }

    #[test]
    fn other_notification_kinds() {
        let mut outcome = RunOutcome::failed("", false);
        outcome.status = RunStatus::CompletedWithErrors;
        outcome.ops_total = 10;
        outcome.ops_done = 7;
        outcome.ops_failed = 3;
        outcome.conflicts = 1;
        outcome.bytes_copied = 5 * 1024 * 1024;
        let n = build_notification(
            &task(),
            &run(1, RunStatus::CompletedWithErrors),
            &outcome,
            NotifyReason::Warning,
            "h",
            "v",
            20,
        );
        assert_eq!(n.title, "Sync finished with problems: Nightly backup");
        assert!(
            n.summary
                .ends_with("but 3 items couldn't be synced and 1 conflict was skipped."),
            "{}",
            n.summary
        );
        assert!(n
            .facts
            .iter()
            .any(|f| f.label == "Data copied" && f.value == "5.0 MB"));
        assert!(n.facts.iter().any(|f| f.label == "Conflicts skipped"));
        assert!(!n.facts.iter().any(|f| f.label == "Attempt"));

        outcome.status = RunStatus::Succeeded;
        let n = build_notification(
            &task(),
            &run(1, RunStatus::Succeeded),
            &outcome,
            NotifyReason::Recovered,
            "h",
            "v",
            20,
        );
        assert_eq!(n.title, "Sync working again: Nightly backup");
        assert_eq!(n.severity, Severity::Success);

        let mut aborted = RunOutcome::failed(
            "Mirror would delete 900 of 1000 files (90%), over the 50% limit.",
            false,
        );
        aborted.status = RunStatus::Aborted;
        let n = build_notification(
            &task(),
            &run(1, RunStatus::Aborted),
            &aborted,
            NotifyReason::Failure,
            "h",
            "v",
            20,
        );
        assert_eq!(n.title, "Sync stopped by a safety check: Nightly backup");
        assert!(n
            .summary
            .contains("stopped before changing anything: Mirror would delete"));

        let mut interrupted = RunOutcome::failed("", false);
        interrupted.status = RunStatus::Interrupted;
        let mut r = run(1, RunStatus::Interrupted);
        r.finished_ms = None;
        let n = build_notification(
            &task(),
            &r,
            &interrupted,
            NotifyReason::Failure,
            "h",
            "v",
            20,
        );
        assert_eq!(n.title, "Sync interrupted: Nightly backup");
        assert!(n
            .summary
            .ends_with("The next run picks up where it left off."));
        assert_eq!(n.finished_ms, n.started_ms);
        assert!(!n.facts.iter().any(|f| f.label == "Duration"));
    }

    #[test]
    fn formatting_helpers() {
        assert_eq!(human_bytes(512), "512 bytes");
        assert_eq!(human_bytes(1536), "1.5 KB");
        assert_eq!(human_bytes(300 * 1024 * 1024), "300 MB");
        assert_eq!(human_duration(400), "under a second");
        assert_eq!(human_duration(42_000), "42 s");
        assert_eq!(human_duration(3_723_000), "1 h 2 min");
    }
}
