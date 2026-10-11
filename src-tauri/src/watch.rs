//! File-system watching behind the "auto-refresh" option.
//!
//! Each open session registers its two paths under an id; when something
//! changes underneath them (debounced), the frontend receives an
//! `fs-changed` event carrying that id and re-runs its comparison.
//! Remote paths can't be watched — the frontend polls those instead.

use globset::{Glob, GlobSet, GlobSetBuilder};
use notify::RecursiveMode;
use notify_debouncer_mini::{new_debouncer, DebounceEventResult, Debouncer};
use serde::Serialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;
use tauri::{AppHandle, Emitter};

/// Long enough to coalesce a build or `git checkout` touching many files.
const DEBOUNCE: Duration = Duration::from_millis(700);

pub const EVENT: &str = "fs-changed";

#[derive(Clone, Serialize)]
pub struct ChangedEvent {
    pub id: String,
}

type FsDebouncer = Debouncer<notify::RecommendedWatcher>;

#[derive(Default)]
pub struct Watchers {
    active: Mutex<HashMap<String, FsDebouncer>>,
}

/// What to watch for one path. Files are watched through their parent
/// directory because editors save by writing a temp file and renaming it over
/// the original, which would orphan a watch on the file itself.
#[derive(Clone)]
struct Target {
    watch: PathBuf,
    mode: RecursiveMode,
    /// For file targets, only events on this exact path count.
    only: Option<PathBuf>,
}

fn target(path: &str) -> Option<Target> {
    let p = PathBuf::from(path);
    let meta = std::fs::metadata(&p).ok()?;
    Some(if meta.is_dir() {
        Target { watch: p, mode: RecursiveMode::Recursive, only: None }
    } else {
        Target {
            watch: p.parent()?.to_path_buf(),
            mode: RecursiveMode::NonRecursive,
            only: Some(p),
        }
    })
}

fn build_ignore(patterns: &[String]) -> GlobSet {
    let mut b = GlobSetBuilder::new();
    for p in patterns.iter().map(|p| p.trim()).filter(|p| !p.is_empty()) {
        // Bad patterns were already reported by the compare itself.
        if let Ok(g) = Glob::new(p) {
            b.add(g);
        }
    }
    b.build().unwrap_or_else(|_| GlobSet::empty())
}

/// True if any component of `path` below a watched root matches `ignore` —
/// so churn inside `.git` or `node_modules` doesn't trigger refreshes.
fn ignored(path: &Path, roots: &[Target], ignore: &GlobSet) -> bool {
    let rel = roots
        .iter()
        .find_map(|t| path.strip_prefix(&t.watch).ok())
        .unwrap_or(path);
    rel.components().any(|c| ignore.is_match(c.as_os_str()))
}

impl Watchers {
    /// Starts (or replaces) the watch for `id`. Remote and missing paths are
    /// skipped; returns how many paths are actually being watched.
    pub fn watch(&self, app: AppHandle, id: String, paths: Vec<String>, exclude: Vec<String>) -> Result<usize, String> {
        self.unwatch(&id);
        let targets: Vec<Target> = paths
            .iter()
            .filter(|p| !pcompare_remote::is_remote(p))
            .filter_map(|p| target(p))
            .collect();
        if targets.is_empty() {
            return Ok(0);
        }

        let ignore = build_ignore(&exclude);
        let files: Vec<PathBuf> = targets.iter().filter_map(|t| t.only.clone()).collect();
        let has_dirs = targets.iter().any(|t| t.only.is_none());
        let roots = targets.clone();
        let event_id = id.clone();

        let mut debouncer = new_debouncer(DEBOUNCE, move |res: DebounceEventResult| {
            let Ok(events) = res else { return };
            let relevant = events.iter().any(|e| {
                // A file target's parent also sees its siblings; filter those.
                let in_scope = (has_dirs && roots.iter().any(|t| t.only.is_none() && e.path.starts_with(&t.watch)))
                    || files.iter().any(|f| f == &e.path);
                in_scope && !ignored(&e.path, &roots, &ignore)
            });
            if relevant {
                let _ = app.emit(EVENT, ChangedEvent { id: event_id.clone() });
            }
        })
        .map_err(|e| e.to_string())?;

        let mut watched = 0;
        for t in &targets {
            // Watching the same parent twice (two files in one folder) is harmless.
            if debouncer.watcher().watch(&t.watch, t.mode).is_ok() {
                watched += 1;
            }
        }
        self.active.lock().unwrap().insert(id, debouncer);
        Ok(watched)
    }

    pub fn unwatch(&self, id: &str) {
        // Dropping the debouncer stops its thread and the OS watch.
        self.active.lock().unwrap().remove(id);
    }
}
