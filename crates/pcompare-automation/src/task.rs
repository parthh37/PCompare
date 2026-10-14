//! A saved, automated sync between two folders.

use pcompare_core::folder::FolderCompareOptions;
use pcompare_sync::exec::{Disposal, ExecOptions};
use pcompare_sync::sync::{ConflictPolicy, PlanRules, SyncMatrix, SyncMode};
use serde::{Deserialize, Serialize};

use crate::trigger::Trigger;

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct SyncTask {
    /// Empty for a task that hasn't been saved yet.
    #[serde(default)]
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// Disabled tasks keep their settings but never run automatically
    /// (Run now still works).
    pub enabled: bool,
    /// Local folders, UNC shares or `remote://` profile paths.
    pub left: String,
    pub right: String,
    pub mode: SyncMode,
    /// One action per category (bc5:sync-matrix). Planning uses it only in
    /// [`SyncMode::Custom`]; for the fixed modes it is kept equal to their
    /// preset ([`normalize_task`]) so the file says what the task does.
    /// Format 1 files have none; [`migrate_task`] fills it in.
    #[serde(default)]
    pub matrix: SyncMatrix,
    /// Don't create orphan folders that hold no files.
    #[serde(default)]
    pub skip_empty_folders: bool,
    #[serde(default)]
    pub conflicts: ConflictPolicy,
    /// How the two sides are compared (filters, content method...). The
    /// scan database's checksum cache makes content compares cheap on
    /// repeat runs: unchanged files (same size + mtime) aren't re-read.
    #[serde(default)]
    pub compare: FolderCompareOptions,
    /// Per-operation retries, locked-file policy, verification, where
    /// deleted/replaced files go.
    #[serde(default)]
    pub exec: ExecOptions,
    /// Any number of triggers; none = manual only.
    #[serde(default)]
    pub triggers: Vec<Trigger>,
    /// Run once at startup (or on wake) if a scheduled time was missed
    /// while the app wasn't running or the machine was asleep.
    #[serde(default = "yes")]
    pub catch_up_missed: bool,
    #[serde(default)]
    pub safety: SafetyOptions,
    #[serde(default)]
    pub retry: RunRetry,
    #[serde(default)]
    pub notify: NotifyRules,
    #[serde(default)]
    pub created_ms: i64,
    #[serde(default)]
    pub updated_ms: i64,
}

fn yes() -> bool {
    true
}

impl SyncTask {
    /// The matrix the task plans with.
    pub fn effective_matrix(&self) -> SyncMatrix {
        SyncMatrix::for_mode(self.mode, &self.matrix)
    }

    /// What planning needs besides the folders, compare options and conflict policy.
    pub fn plan_rules(&self) -> PlanRules {
        PlanRules {
            matrix: self.matrix,
            skip_empty_folders: self.skip_empty_folders,
            ..PlanRules::default()
        }
    }
}

/// Brings a task read from an older automation file up to date. Format 1
/// had no matrix: a fixed mode's preset stands in for it.
pub fn migrate_task(task: &mut SyncTask, from_version: u32) {
    if from_version < 2 {
        task.matrix = SyncMatrix::preset(task.mode);
    }
}

/// Keeps a fixed mode's stored matrix equal to its preset, so switching a
/// task from Custom to a preset doesn't leave a stale table behind.
pub fn normalize_task(task: &mut SyncTask) {
    if task.mode != SyncMode::Custom {
        task.matrix = SyncMatrix::preset(task.mode);
    }
}

/// Guards that stop an unattended run from doing something drastic. A run
/// they stop is `Aborted` and notified as a failure.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct SafetyOptions {
    /// Abort if the plan deletes more than this many items. `None`: no limit.
    pub max_deletions: Option<u32>,
    /// Abort if the plan deletes more than this percentage of the files on
    /// the side being deleted from. `None`: no limit.
    pub max_deletion_percent: Option<u8>,
    /// Abort when one side is empty and the other isn't — almost always an
    /// unplugged drive or an unmounted share, not a real change. (The
    /// planner's `emptySide` warning.)
    pub abort_on_empty_side: bool,
    /// Abort on the planner's `massDeletion` warning.
    pub abort_on_mass_deletion: bool,
    /// Cancel the run if it takes longer than this. `None`: no limit.
    pub timeout_minutes: Option<u32>,
}

impl Default for SafetyOptions {
    fn default() -> Self {
        SafetyOptions {
            max_deletions: None,
            max_deletion_percent: Some(50),
            abort_on_empty_side: true,
            abort_on_mass_deletion: true,
            timeout_minutes: Some(240),
        }
    }
}

/// Whole-run retries, for run-wide failures such as an unreachable network
/// share (per-file retries are `ExecOptions::retry_attempts`).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct RunRetry {
    /// Extra attempts after the first.
    pub attempts: u32,
    /// Wait before the first retry; doubles each time, capped at 1 hour.
    pub delay_secs: u64,
}

impl Default for RunRetry {
    fn default() -> Self {
        RunRetry {
            attempts: 2,
            delay_secs: 60,
        }
    }
}

/// Who hears about runs, and when.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct NotifyRules {
    /// Notification channel ids (`pcompare_notify::ChannelConfig::id`).
    pub channels: Vec<String>,
    /// Also show an OS notification on this machine.
    pub desktop: bool,
    /// Failed, aborted or interrupted runs (after the last retry).
    pub on_failure: bool,
    /// Completed with failed operations or skipped conflicts.
    pub on_warning: bool,
    /// Every successful run that changed something.
    pub on_success: bool,
    /// The first success after a failure.
    pub on_recovery: bool,
    /// The same failure (same status and message) isn't re-sent more often
    /// than this. 0 = every time.
    pub repeat_after_minutes: u32,
}

impl Default for NotifyRules {
    fn default() -> Self {
        NotifyRules {
            channels: Vec::new(),
            desktop: true,
            on_failure: true,
            on_warning: true,
            on_success: false,
            on_recovery: true,
            repeat_after_minutes: 360,
        }
    }
}

/// Something wrong with a task's settings.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TaskIssue {
    /// Errors prevent saving; warnings are shown but allowed.
    pub error: bool,
    /// The settings area it belongs to: "general", "triggers", "options",
    /// "safety" or "notify" (the editor's tab ids).
    pub area: String,
    pub message: String,
}

/// Checks a task before saving. `other_names` are the names of the other
/// saved tasks (names must be unique, case-insensitively).
///
/// Errors: empty name or folders; both sides the same folder, or one
/// inside the other (a sync would recurse into its own output); an invalid
/// trigger; a real-time trigger on a side that can't be watched is only a
/// warning (it falls back to its periodic rescan). Warnings: mirror or
/// two-way with permanent deletion; no notification channel while failure
/// notifications are on; no triggers (manual only); safety checks all off.
pub fn validate_task(task: &SyncTask, other_names: &[String]) -> Vec<TaskIssue> {
    let mut issues = Vec::new();
    let mut error = |area: &str, message: String| {
        issues.push(TaskIssue {
            error: true,
            area: area.into(),
            message,
        })
    };
    let name = task.name.trim();
    if name.is_empty() {
        error("general", "Give the task a name.".into());
    } else if other_names
        .iter()
        .any(|n| n.trim().to_lowercase() == name.to_lowercase())
    {
        error(
            "general",
            format!("Another task is already called \"{name}\"; pick a different name."),
        );
    }
    let (left, right) = (task.left.trim(), task.right.trim());
    if left.is_empty() {
        error("general", "Choose the left folder.".into());
    }
    if right.is_empty() {
        error("general", "Choose the right folder.".into());
    }
    if !left.is_empty() && !right.is_empty() {
        match nesting(left, right) {
            Some(Nesting::Same) => error("general", "The left and right folders are the same folder.".into()),
            Some(Nesting::LeftInRight) => error(
                "general",
                format!("The left folder is inside the right folder ({right}); a sync would copy its own output into itself."),
            ),
            Some(Nesting::RightInLeft) => error(
                "general",
                format!("The right folder is inside the left folder ({left}); a sync would copy its own output into itself."),
            ),
            None => {}
        }
    }
    for (i, trigger) in task.triggers.iter().enumerate() {
        if let Err(message) = crate::trigger::validate(trigger) {
            let what = crate::trigger::describe(trigger);
            error("triggers", format!("Trigger {} ({what}): {message}", i + 1));
        }
    }
    if let Err(message) = check_globs(&task.compare.exclude) {
        error("options", message);
    }
    if task.retry.attempts > MAX_RUN_RETRIES {
        error(
            "options",
            format!("At most {MAX_RUN_RETRIES} whole-run retries are allowed."),
        );
    }
    if task.retry.attempts > 0 && task.retry.delay_secs == 0 {
        error(
            "options",
            "The wait before a retry must be at least 1 second.".into(),
        );
    }
    if task
        .safety
        .max_deletion_percent
        .is_some_and(|p| p == 0 || p > 100)
    {
        error(
            "safety",
            "The deletion percentage limit must be between 1 and 100.".into(),
        );
    }
    if task.safety.timeout_minutes == Some(0) {
        error(
            "safety",
            "The time limit must be at least 1 minute (or turned off).".into(),
        );
    }

    let mut warn = |area: &str, message: String| {
        issues.push(TaskIssue {
            error: false,
            area: area.into(),
            message,
        })
    };
    for (i, trigger) in task.triggers.iter().enumerate() {
        let Trigger::Realtime { rescan_minutes, .. } = trigger else {
            continue;
        };
        for (side, path) in watched_sides(task) {
            if is_remote(path) {
                let fallback = if *rescan_minutes > 0 {
                    format!("changes there are picked up by the rescan every {rescan_minutes} min")
                } else {
                    "with no periodic rescan, changes there never start a run".to_owned()
                };
                warn(
                    "triggers",
                    format!("Trigger {}: the {side} folder is a remote location and can't be watched for changes; {fallback}.", i + 1),
                );
            }
        }
    }
    let matrix = task.effective_matrix();
    let deletes = matrix.deletes();
    if task.mode == SyncMode::Custom {
        if !matrix.copies_to(true) && !matrix.copies_to(false) && !deletes {
            warn(
                "options",
                "The custom sync leaves every kind of item alone, so runs never change anything.".into(),
            );
        }
        if matrix.prompt_cells() > 0 {
            warn(
                "options",
                "Some categories are set to Prompt: unattended runs leave those items alone and report them as conflicts.".into(),
            );
        }
        if !matrix.invalid_cells().is_empty() {
            warn(
                "options",
                "Some categories name an action that can't apply to them (such as copying an item from the side that doesn't have it); those items are left alone.".into(),
            );
        }
    }
    if deletes && task.exec.disposal == Disposal::Permanent {
        warn(
            "options",
            "Files this sync deletes or replaces are removed permanently, with no way to undo an unattended run; consider the Recycle Bin or a versioning folder.".into(),
        );
    }
    if let Disposal::Versioning { folder } = &task.exec.disposal {
        let folder = folder.trim();
        for (side, path) in [("left", left), ("right", right)] {
            if !folder.is_empty()
                && !path.is_empty()
                && matches!(
                    nesting(folder, path),
                    Some(Nesting::Same | Nesting::LeftInRight)
                )
            {
                warn(
                    "options",
                    format!("The versioning folder is inside the {side} folder; exclude it from the comparison so old versions aren't synced too."),
                );
            }
        }
    }
    if task.triggers.is_empty() {
        warn(
            "triggers",
            "No triggers: this task only runs when you click Run now.".into(),
        );
    }
    let s = &task.safety;
    if s.max_deletions.is_none()
        && s.max_deletion_percent.is_none()
        && !s.abort_on_empty_side
        && !s.abort_on_mass_deletion
    {
        warn(
            "safety",
            "All safety checks are off: an unplugged drive or emptied folder could make an unattended sync delete everything on the other side.".into(),
        );
    }
    let n = &task.notify;
    if n.channels.is_empty() && n.on_failure {
        let message = if n.desktop {
            "No notification channel is selected, so failures only show on this computer."
        } else {
            "No notification channel is selected and desktop notifications are off, so nobody hears about failures."
        };
        warn("notify", message.into());
    }
    issues
}

/// Whole-run retries allowed per run.
pub const MAX_RUN_RETRIES: u32 = 10;

/// The sides a real-time trigger watches, per the sync direction: the
/// side(s) changes are copied *from*.
pub(crate) fn watched_sides(task: &SyncTask) -> Vec<(&'static str, &str)> {
    let (left, right) = (("left", task.left.trim()), ("right", task.right.trim()));
    match task.mode {
        SyncMode::UpdateLeftToRight | SyncMode::MirrorLeftToRight => vec![left],
        SyncMode::UpdateRightToLeft | SyncMode::MirrorRightToLeft => vec![right],
        SyncMode::UpdateBoth | SyncMode::TwoWay => vec![left, right],
        SyncMode::Custom => {
            // A side matters when changes there lead to copies or (via the
            // baseline) to deletions on the other side.
            use pcompare_sync::sync::SyncAction::{DeleteLeft, DeleteRight};
            let m = &task.matrix;
            let from_left = m.copies_to(true) || m.left_deleted == DeleteRight;
            let from_right = m.copies_to(false) || m.right_deleted == DeleteLeft;
            match (from_left, from_right) {
                (true, false) => vec![left],
                (false, true) => vec![right],
                _ => vec![left, right],
            }
        }
    }
}

/// `remote://<profile>/...` paths (see `pcompare_remote::parse_remote`).
pub(crate) fn is_remote(path: &str) -> bool {
    const SCHEME: &str = "remote://";
    path.get(..SCHEME.len())
        .is_some_and(|p| p.eq_ignore_ascii_case(SCHEME))
        && path[SCHEME.len()..]
            .split('/')
            .next()
            .is_some_and(|profile| !profile.is_empty())
}

#[derive(Debug, PartialEq, Eq)]
enum Nesting {
    Same,
    LeftInRight,
    RightInLeft,
}

/// How two folder paths relate, comparing whole path components (`/a/b`
/// is inside `/a`, `/a/bc` isn't), case-insensitively on Windows. Remote
/// paths are only compared for equality: their layout belongs to the
/// remote side.
fn nesting(left: &str, right: &str) -> Option<Nesting> {
    let (l, r) = (path_components(left), path_components(right));
    if l == r {
        return Some(Nesting::Same);
    }
    if is_remote(left) || is_remote(right) {
        return None;
    }
    if r.len() > l.len() && r.starts_with(&l) {
        Some(Nesting::RightInLeft)
    } else if l.len() > r.len() && l.starts_with(&r) {
        Some(Nesting::LeftInRight)
    } else {
        None
    }
}

/// The non-empty components of a path, case-folded on Windows. A leading
/// separator (root or UNC prefix) is kept as a marker so `/a` and `a`
/// differ.
fn path_components(path: &str) -> Vec<String> {
    let path = path.trim();
    let windows = cfg!(windows);
    let is_sep = |c: char| c == '/' || (windows && c == '\\');
    let mut out = Vec::new();
    let lead: String = path.chars().take_while(|&c| is_sep(c)).collect();
    if !lead.is_empty() {
        out.push(if lead.chars().count() >= 2 {
            "//".into()
        } else {
            "/".into()
        });
    }
    out.extend(
        path.split(is_sep)
            .filter(|c| !c.is_empty() && *c != ".")
            .map(|c| {
                if windows {
                    c.to_lowercase()
                } else {
                    c.to_owned()
                }
            }),
    );
    out
}

fn check_globs(patterns: &[String]) -> Result<(), String> {
    for p in patterns.iter().map(|p| p.trim()).filter(|p| !p.is_empty()) {
        globset::Glob::new(p)
            .map_err(|e| format!("The exclude pattern \"{p}\" isn't valid: {e}."))?;
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn sample() -> SyncTask {
        serde_json::from_value(serde_json::json!({
            "name": "Backup", "enabled": true, "left": "/data/docs", "right": "/backup/docs",
            "mode": "mirrorLeftToRight",
            "triggers": [{"kind": "daily", "times": ["02:00"], "weekdays": []}],
            "notify": {"channels": ["c1"]},
        }))
        .unwrap()
    }

    fn errors(task: &SyncTask) -> Vec<String> {
        validate_task(task, &[])
            .into_iter()
            .filter(|i| i.error)
            .map(|i| i.message)
            .collect()
    }

    fn warnings(task: &SyncTask) -> Vec<(String, String)> {
        validate_task(task, &[])
            .into_iter()
            .filter(|i| !i.error)
            .map(|i| (i.area, i.message))
            .collect()
    }

    #[test]
    fn defaults_match_the_spec() {
        let t = sample();
        assert!(t.catch_up_missed);
        assert_eq!(
            t.retry,
            RunRetry {
                attempts: 2,
                delay_secs: 60
            }
        );
        let n = &t.notify;
        assert!(n.on_failure && n.on_warning && !n.on_success && n.on_recovery && n.desktop);
        assert_eq!(n.repeat_after_minutes, 360);
        assert!(t.safety.abort_on_empty_side && t.safety.abort_on_mass_deletion);
    }

    #[test]
    fn a_good_task_has_no_issues() {
        assert_eq!(validate_task(&sample(), &["Other".into()]), vec![]);
    }

    #[test]
    fn names_and_folders_are_required() {
        let mut t = sample();
        t.name = "  ".into();
        t.left = "".into();
        let e = errors(&t);
        assert_eq!(e.len(), 2, "{e:?}");
        let issues = validate_task(&sample(), &["backup".into()]);
        assert!(issues[0].error && issues[0].message.contains("already called"));
        assert_eq!(issues[0].area, "general");
    }

    #[test]
    fn nesting_is_path_aware() {
        assert_eq!(nesting("/a/b", "/a/b/"), Some(Nesting::Same));
        assert_eq!(nesting("/a/b", "/a/b/c"), Some(Nesting::RightInLeft));
        assert_eq!(nesting("/a/b/c", "/a/b"), Some(Nesting::LeftInRight));
        assert_eq!(nesting("/a/b", "/a/bc"), None);
        assert_eq!(nesting("/a/b", "a/b"), None, "absolute vs relative");
        assert_eq!(
            nesting("remote://srv/a", "remote://srv/a/"),
            Some(Nesting::Same)
        );
        assert_eq!(
            nesting("remote://srv/a", "remote://srv/a/b"),
            None,
            "remote layouts aren't ours"
        );
        assert_eq!(nesting("/a", "remote://srv/a"), None);
        if cfg!(windows) {
            assert_eq!(
                nesting("C:\\Data", "c:/data/sub"),
                Some(Nesting::RightInLeft)
            );
            assert_eq!(
                nesting("\\\\nas\\share", "\\\\NAS\\Share\\"),
                Some(Nesting::Same)
            );
        } else {
            assert_eq!(
                nesting("/Data", "/data/sub"),
                None,
                "case-sensitive off Windows"
            );
        }

        let mut t = sample();
        t.right = "/data/docs/backup".into();
        assert!(errors(&t)[0].contains("right folder is inside the left folder"));
        t.right = "/data/docs".into();
        assert!(errors(&t)[0].contains("same folder"));
        t.right = "/data/docs2".into();
        assert!(errors(&t).is_empty());
    }

    #[test]
    fn invalid_settings_are_errors() {
        let mut t = sample();
        t.triggers.push(Trigger::Interval { every_minutes: 0 });
        t.compare.exclude = vec!["*.tmp".into(), "a[".into()];
        t.retry.attempts = 11;
        t.safety.max_deletion_percent = Some(0);
        t.safety.timeout_minutes = Some(0);
        let issues = validate_task(&t, &[]);
        let areas: Vec<&str> = issues
            .iter()
            .filter(|i| i.error)
            .map(|i| i.area.as_str())
            .collect();
        assert_eq!(
            areas,
            ["triggers", "options", "options", "safety", "safety"]
        );
        assert!(
            issues[0]
                .message
                .starts_with("Trigger 2 (Every 0 minutes): "),
            "{}",
            issues[0].message
        );
        assert!(issues[1].message.contains("\"a[\""));
    }

    #[test]
    fn warnings_dont_block() {
        let mut t = sample();
        t.triggers.clear();
        t.exec.disposal = Disposal::Permanent;
        t.safety = SafetyOptions {
            max_deletions: None,
            max_deletion_percent: None,
            abort_on_empty_side: false,
            abort_on_mass_deletion: false,
            timeout_minutes: None,
        };
        t.notify.channels.clear();
        let w = warnings(&t);
        let areas: Vec<&str> = w.iter().map(|(a, _)| a.as_str()).collect();
        assert_eq!(areas, ["options", "triggers", "safety", "notify"], "{w:?}");
        assert!(errors(&t).is_empty());
        assert!(w[3].1.contains("only show on this computer"));
        t.notify.desktop = false;
        assert!(warnings(&t)[3].1.contains("nobody hears"));
        // Permanent deletion in an update mode deletes nothing.
        t.mode = SyncMode::UpdateLeftToRight;
        assert!(!warnings(&t).iter().any(|(_, m)| m.contains("permanently")));
    }

    #[test]
    fn realtime_on_a_remote_source_is_a_warning() {
        let mut t = sample();
        t.left = "remote://nas-profile/docs".into();
        t.triggers = vec![Trigger::Realtime {
            debounce_secs: 5,
            max_delay_secs: 60,
            rescan_minutes: 30,
        }];
        let w = warnings(&t);
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(
            w[0].1.contains("left folder is a remote location") && w[0].1.contains("every 30 min")
        );
        // The right side isn't watched in a left → right mode.
        t.left = "/data".into();
        t.right = "remote://nas-profile/docs".into();
        assert!(warnings(&t).is_empty());
        t.mode = SyncMode::TwoWay;
        assert_eq!(warnings(&t).len(), 1);
    }

    #[test]
    fn versioning_folder_inside_a_side_is_a_warning() {
        let mut t = sample();
        t.exec.disposal = Disposal::Versioning {
            folder: "/backup/docs/.versions".into(),
        };
        let w = warnings(&t);
        assert_eq!(w.len(), 1);
        assert!(w[0].1.contains("inside the right folder"));
        t.exec.disposal = Disposal::Versioning {
            folder: "/versions".into(),
        };
        assert!(warnings(&t).is_empty());
    }

    #[test]
    fn remote_detection() {
        assert!(is_remote("remote://p/x"));
        assert!(is_remote("REMOTE://p"));
        assert!(!is_remote("remote:///x"));
        assert!(!is_remote("remote://"));
        assert!(!is_remote("/remote://p"));
    }

    #[test]
    fn watched_sides_follow_the_mode() {
        let mut t = sample();
        let sides = |t: &SyncTask| {
            watched_sides(t)
                .into_iter()
                .map(|(s, _)| s)
                .collect::<Vec<_>>()
        };
        assert_eq!(sides(&t), ["left"]);
        t.mode = SyncMode::UpdateRightToLeft;
        assert_eq!(sides(&t), ["right"]);
        t.mode = SyncMode::UpdateBoth;
        assert_eq!(sides(&t), ["left", "right"]);
    }
}
