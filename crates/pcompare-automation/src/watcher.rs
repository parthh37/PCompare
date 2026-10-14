//! Real-time triggers: watches the source side(s) of real-time tasks and
//! reports when changes have settled. Owned and driven by the scheduler.
//!
//! Each watched task gets its own recursive OS watch (`notify`) on the
//! side(s) its mode copies from. Events are collapsed into "task X changed":
//! a task is due once `debounce_secs` pass without a new event, or
//! `max_delay_secs` after the first one if changes never stop. Events under
//! excluded globs or inside the versioning folder don't count, and neither
//! do events while the task itself runs (plus [`OWN_WRITES_GRACE`] after):
//! those are mostly its own writes, and anything real is caught by the next
//! change, the periodic rescan, or the follow-up run finding nothing to do.
//!
//! A side that can't be watched (remote, missing, unplugged) is reported as
//! a watch error rather than failing the task; the scheduler retries the
//! setup on reload and every few minutes.
//!
//! Locking: OS watchers are created and dropped without holding the state
//! lock, because their event threads call back into it, and the wake
//! callback is invoked after the state lock is released.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use notify::{EventKind, RecursiveMode, Watcher as _};
use pcompare_sync::exec::Disposal;

use crate::task::{is_remote, watched_sides, SyncTask};
use crate::trigger::Trigger;

/// Events this long after a run ends are still treated as its own writes
/// (file systems report them late, especially network shares).
pub const OWN_WRITES_GRACE: Duration = Duration::from_secs(2);

/// Whether a task's watch is up, and why not.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct WatchStatus {
    /// At least one side is being watched.
    pub watching: bool,
    /// Why a side isn't watched (remote, missing, OS error...).
    pub error: Option<String>,
}

/// The real-time watches of all tasks. Thread-safe; dropping it stops them.
pub struct Watcher {
    shared: Arc<Shared>,
}

struct Shared {
    entries: Mutex<HashMap<String, Entry>>,
    /// Tells the scheduler something may be due.
    wake: Box<dyn Fn() + Send + Sync>,
}

struct Entry {
    /// What the watch was built from; a change rebuilds it.
    key: String,
    /// Bumped on every rebuild so a replaced OS watcher's late events are ignored.
    generation: u64,
    debounce: Duration,
    max_delay: Duration,
    first: Option<Instant>,
    last: Option<Instant>,
    running: bool,
    quiet_until: Option<Instant>,
    status: WatchStatus,
    /// Every watched root still exists; checked by `ensure(.., retry)`.
    roots: Vec<PathBuf>,
    backend: Option<notify::RecommendedWatcher>,
}

/// Decides which event paths count, per task.
struct Filter {
    /// Watched roots, as given and canonicalized (macOS reports `/private/var`
    /// for `/var`, for example).
    roots: Vec<PathBuf>,
    exclude: GlobSet,
    /// The versioning folder (both forms).
    ignore: Vec<PathBuf>,
}

impl Watcher {
    /// `wake` is called (from OS watcher threads) when a task may have
    /// become due or a watch broke; it must be cheap and never block on the
    /// scheduler's state.
    pub fn new(wake: impl Fn() + Send + Sync + 'static) -> Watcher {
        Watcher {
            shared: Arc::new(Shared {
                entries: Mutex::new(HashMap::new()),
                wake: Box::new(wake),
            }),
        }
    }

    /// The real-time trigger of a task, if it has one.
    pub fn realtime(task: &SyncTask) -> Option<(u32, u32)> {
        task.triggers.iter().find_map(|t| match t {
            Trigger::Realtime {
                debounce_secs,
                max_delay_secs,
                ..
            } => Some((*debounce_secs, *max_delay_secs)),
            _ => None,
        })
    }

    /// Starts or updates the watch for `task` (which must have a real-time
    /// trigger). An unchanged, healthy watch is left alone; with `retry`, a
    /// watch with an error or a vanished root is set up again.
    pub fn ensure(&self, task: &SyncTask, retry: bool) {
        let Some((debounce, max_delay)) = Self::realtime(task) else {
            self.remove(&task.id);
            return;
        };
        let key = spec_key(task, debounce, max_delay);
        let current = self
            .shared
            .lock()
            .get(&task.id)
            .map(|e| (e.key.clone(), e.status.error.is_none(), e.roots.clone()));
        if let Some((current_key, no_error, roots)) = current {
            // Checked outside the lock: an unreachable share can stall.
            if current_key == key && (!retry || (no_error && roots.iter().all(|r| r.is_dir()))) {
                return;
            }
        }
        let generation = next_generation();
        // Build outside the lock (see the module docs).
        let built = build(&self.shared, &task.id, generation, task);
        let mut entries = self.shared.lock();
        let entry = entries.entry(task.id.clone()).or_insert_with(|| Entry {
            key: String::new(),
            generation: 0,
            debounce: Duration::ZERO,
            max_delay: Duration::ZERO,
            first: None,
            last: None,
            running: false,
            quiet_until: None,
            status: WatchStatus::default(),
            roots: Vec::new(),
            backend: None,
        });
        if entry.generation > generation {
            // A concurrent `ensure` already installed a newer watch.
            drop(entries);
            drop(built.backend);
            return;
        }
        entry.key = key;
        entry.generation = generation;
        entry.debounce = Duration::from_secs(u64::from(debounce.max(1)));
        entry.max_delay = Duration::from_secs(u64::from(max_delay.max(debounce).max(1)));
        entry.status = built.status;
        entry.roots = built.roots;
        let old = std::mem::replace(&mut entry.backend, built.backend);
        drop(entries);
        drop(old);
    }

    /// Stops watching a task.
    pub fn remove(&self, task_id: &str) {
        let old = self.shared.lock().remove(task_id);
        drop(old);
    }

    /// Stops watching every task not in `keep`.
    pub fn retain(&self, keep: &HashSet<String>) {
        let removed: Vec<Entry> = {
            let mut entries = self.shared.lock();
            let gone: Vec<String> = entries
                .keys()
                .filter(|id| !keep.contains(*id))
                .cloned()
                .collect();
            gone.iter().filter_map(|id| entries.remove(id)).collect()
        };
        drop(removed);
    }

    /// Ids of the watched tasks.
    pub fn task_ids(&self) -> Vec<String> {
        self.shared.lock().keys().cloned().collect()
    }

    /// Marks the task's run as started or ended. Changes seen before the
    /// start are part of the run; events during it and for
    /// [`OWN_WRITES_GRACE`] after are ignored.
    pub fn set_running(&self, task_id: &str, running: bool) {
        self.set_running_at(task_id, running, Instant::now());
    }

    fn set_running_at(&self, task_id: &str, running: bool, now: Instant) {
        if let Some(e) = self.shared.lock().get_mut(task_id) {
            e.running = running;
            if running {
                e.first = None;
                e.last = None;
            } else {
                e.quiet_until = Some(now + OWN_WRITES_GRACE);
            }
        }
    }

    /// Tasks whose changes have settled (or waited long enough), clearing
    /// their pending state.
    pub fn take_due(&self, now: Instant) -> Vec<String> {
        let mut entries = self.shared.lock();
        let mut due = Vec::new();
        for (id, e) in entries.iter_mut() {
            if e.due_at().is_some_and(|at| at <= now) {
                e.first = None;
                e.last = None;
                due.push(id.clone());
            }
        }
        due.sort();
        due
    }

    /// When the next task becomes due if nothing else changes.
    pub fn next_due(&self) -> Option<Instant> {
        self.shared.lock().values().filter_map(Entry::due_at).min()
    }

    pub fn status(&self, task_id: &str) -> Option<WatchStatus> {
        self.shared.lock().get(task_id).map(|e| e.status.clone())
    }

    /// Records a change as if the OS had reported it (tests).
    #[cfg(test)]
    fn inject(&self, task_id: &str, now: Instant) {
        if let Some(e) = self.shared.lock().get_mut(task_id) {
            e.record(now);
        }
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        let all: Vec<Entry> = self.shared.lock().drain().map(|(_, e)| e).collect();
        drop(all);
    }
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, HashMap<String, Entry>> {
        self.entries.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn on_event(
        &self,
        task_id: &str,
        generation: u64,
        filter: &Filter,
        res: notify::Result<notify::Event>,
    ) {
        let now = Instant::now();
        {
            let mut entries = self.lock();
            let Some(e) = entries
                .get_mut(task_id)
                .filter(|e| e.generation == generation)
            else {
                return;
            };
            match res {
                Ok(event) => {
                    if matches!(event.kind, EventKind::Access(_)) {
                        return;
                    }
                    let relevant = event.need_rescan()
                        || event.paths.is_empty()
                        || event.paths.iter().any(|p| filter.relevant(p));
                    if !relevant || !e.record(now) {
                        return;
                    }
                }
                Err(err) => {
                    // The watch may be dead (folder removed, too many
                    // watches...); the next retry rebuilds it.
                    e.status.error = Some(format!("Watching for changes stopped: {err}. It will be set up again automatically."));
                }
            }
        }
        (self.wake)();
    }
}

impl Entry {
    /// Notes a change; false when it's ignored as the task's own writes.
    fn record(&mut self, now: Instant) -> bool {
        if self.running || self.quiet_until.is_some_and(|q| now < q) {
            return false;
        }
        self.first.get_or_insert(now);
        self.last = Some(now);
        true
    }

    fn due_at(&self) -> Option<Instant> {
        let (first, last) = (self.first?, self.last?);
        Some((last + self.debounce).min(first + self.max_delay))
    }
}

impl Filter {
    fn relevant(&self, path: &Path) -> bool {
        if self.ignore.iter().any(|dir| path.starts_with(dir)) {
            return false;
        }
        let Some(rel) = self.roots.iter().find_map(|r| path.strip_prefix(r).ok()) else {
            // Outside every root as we know it (a path form we didn't
            // predict): count it rather than miss a change.
            return true;
        };
        let mut prefix = String::new();
        for part in rel.components() {
            let name = part.as_os_str().to_string_lossy();
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(&name);
            if self.exclude.is_match(name.as_ref()) || self.exclude.is_match(&prefix) {
                return false;
            }
        }
        true
    }
}

/// Unique across tasks too, which does no harm.
fn next_generation() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

fn spec_key(task: &SyncTask, debounce: u32, max_delay: u32) -> String {
    let versioning = match &task.exec.disposal {
        Disposal::Versioning { folder } => folder.as_str(),
        _ => "",
    };
    format!(
        "{:?}|{:?}|{}|{versioning}|{debounce}|{max_delay}",
        watched_sides(task),
        task.compare.exclude,
        task.compare.ignore_name_case
    )
}

struct Built {
    backend: Option<notify::RecommendedWatcher>,
    status: WatchStatus,
    roots: Vec<PathBuf>,
}

/// Sets up the OS watch for a task's source side(s).
fn build(shared: &Arc<Shared>, task_id: &str, generation: u64, task: &SyncTask) -> Built {
    let mut errors = Vec::new();
    let mut roots = Vec::new();
    for (side, path) in watched_sides(task) {
        if is_remote(path) {
            errors.push(format!(
                "The {side} folder is a remote location and can't be watched; changes there are picked up by the periodic rescan."
            ));
            continue;
        }
        match fs::metadata(path) {
            Ok(m) if m.is_dir() => roots.push((side, PathBuf::from(path))),
            Ok(_) => errors.push(format!("The {side} folder {path} isn't a folder.")),
            Err(e) => errors.push(format!(
                "The {side} folder {path} isn't reachable ({e}); watching resumes when it's back."
            )),
        }
    }
    let error = |errors: Vec<String>| (!errors.is_empty()).then(|| errors.join(" "));
    if roots.is_empty() {
        return Built {
            backend: None,
            status: WatchStatus {
                watching: false,
                error: error(errors),
            },
            roots: Vec::new(),
        };
    }

    let filter = Filter {
        roots: with_canonical(roots.iter().map(|(_, r)| r.clone())),
        exclude: exclude_set(&task.compare.exclude, task.compare.ignore_name_case),
        ignore: match &task.exec.disposal {
            Disposal::Versioning { folder } if !folder.trim().is_empty() => {
                with_canonical([PathBuf::from(folder.trim())])
            }
            _ => Vec::new(),
        },
    };
    let weak: Weak<Shared> = Arc::downgrade(shared);
    let id = task_id.to_owned();
    let handler = move |res: notify::Result<notify::Event>| {
        if let Some(shared) = weak.upgrade() {
            shared.on_event(&id, generation, &filter, res);
        }
    };
    let mut backend = match notify::recommended_watcher(handler) {
        Ok(b) => b,
        Err(e) => {
            errors.push(format!("Can't watch for changes: {e}."));
            return Built {
                backend: None,
                status: WatchStatus {
                    watching: false,
                    error: error(errors),
                },
                roots: Vec::new(),
            };
        }
    };
    let mut watched = Vec::new();
    for (side, root) in roots {
        match backend.watch(&root, RecursiveMode::Recursive) {
            Ok(()) => watched.push(root),
            Err(e) => errors.push(format!(
                "Can't watch the {side} folder {}: {e}.",
                root.display()
            )),
        }
    }
    Built {
        backend: (!watched.is_empty()).then_some(backend),
        status: WatchStatus {
            watching: !watched.is_empty(),
            error: error(errors),
        },
        roots: watched,
    }
}

fn with_canonical(paths: impl IntoIterator<Item = PathBuf>) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for p in paths {
        if let Ok(c) = fs::canonicalize(&p) {
            if c != p {
                out.push(c);
            }
        }
        out.push(p);
    }
    out
}

/// The task's exclude globs, matched like the folder compare matches them
/// (against each name and each relative path). Bad patterns are skipped:
/// saving the task already reported them.
fn exclude_set(patterns: &[String], case_insensitive: bool) -> GlobSet {
    let mut b = GlobSetBuilder::new();
    for p in patterns.iter().map(|p| p.trim()).filter(|p| !p.is_empty()) {
        if let Ok(g) = GlobBuilder::new(p)
            .case_insensitive(case_insensitive)
            .build()
        {
            b.add(g);
        }
    }
    b.build().unwrap_or_else(|_| GlobSet::empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::thread;

    fn task(left: &Path, right: &Path) -> SyncTask {
        serde_json::from_value(serde_json::json!({
            "id": "t1", "name": "Live", "enabled": true,
            "left": left.to_str().unwrap(), "right": right.to_str().unwrap(),
            "mode": "updateLeftToRight",
            "compare": {"exclude": ["*.tmp", "build"]},
            "triggers": [{"kind": "realtime", "debounceSecs": 1, "maxDelaySecs": 3, "rescanMinutes": 0}],
        }))
        .unwrap()
    }

    fn counter() -> (Arc<AtomicUsize>, impl Fn() + Send + Sync + 'static) {
        let n = Arc::new(AtomicUsize::new(0));
        let m = n.clone();
        (n, move || {
            m.fetch_add(1, Ordering::SeqCst);
        })
    }

    #[test]
    fn debounce_and_max_delay() {
        let dir = tempfile::tempdir().unwrap();
        let (l, r) = (dir.path().join("l"), dir.path().join("r"));
        fs::create_dir_all(&l).unwrap();
        fs::create_dir_all(&r).unwrap();
        let (_, wake) = counter();
        let w = Watcher::new(wake);
        w.ensure(&task(&l, &r), false);
        assert_eq!(
            w.status("t1"),
            Some(WatchStatus {
                watching: true,
                error: None
            })
        );

        let t0 = Instant::now() + Duration::from_secs(10);
        let s = |n: u64| t0 + Duration::from_millis(n);
        assert_eq!(w.next_due(), None);
        w.inject("t1", s(0));
        w.inject("t1", s(500));
        assert_eq!(w.next_due(), Some(s(1500)), "1 s after the last change");
        assert!(w.take_due(s(1400)).is_empty());
        assert_eq!(w.take_due(s(1500)), ["t1"]);
        assert_eq!(w.next_due(), None, "cleared");

        // Constant churn: fires at the maximum delay.
        for i in 0..10 {
            w.inject("t1", s(2000 + i * 500));
        }
        assert_eq!(w.next_due(), Some(s(5000)), "3 s after the first change");
        assert_eq!(w.take_due(s(5000)), ["t1"]);
    }

    #[test]
    fn own_writes_are_suppressed() {
        let dir = tempfile::tempdir().unwrap();
        let (l, r) = (dir.path().join("l"), dir.path().join("r"));
        fs::create_dir_all(&l).unwrap();
        let (_, wake) = counter();
        let w = Watcher::new(wake);
        w.ensure(&task(&l, &r), false);
        let t0 = Instant::now() + Duration::from_secs(10);
        w.inject("t1", t0);
        w.set_running_at("t1", true, t0);
        assert_eq!(w.next_due(), None, "pending changes are part of the run");
        w.inject("t1", t0 + Duration::from_secs(1));
        assert_eq!(w.next_due(), None, "ignored while running");
        w.set_running_at("t1", false, t0 + Duration::from_secs(5));
        w.inject("t1", t0 + Duration::from_secs(6));
        assert_eq!(w.next_due(), None, "ignored during the grace period");
        w.inject("t1", t0 + Duration::from_secs(8));
        assert!(w.next_due().is_some());
    }

    #[test]
    fn unwatchable_sides_are_reported() {
        let dir = tempfile::tempdir().unwrap();
        let (_, wake) = counter();
        let w = Watcher::new(wake);
        let mut t = task(&dir.path().join("missing"), dir.path());
        w.ensure(&t, false);
        let s = w.status("t1").unwrap();
        assert!(!s.watching);
        assert!(s.error.unwrap().contains("left folder"), "names the side");

        t.left = "remote://nas/docs".into();
        w.ensure(&t, false);
        assert!(w
            .status("t1")
            .unwrap()
            .error
            .unwrap()
            .contains("remote location"));

        // Two-way: one side fine, the other remote.
        t.mode = pcompare_sync::sync::SyncMode::TwoWay;
        w.ensure(&t, false);
        let s = w.status("t1").unwrap();
        assert!(s.watching && s.error.is_some());

        // The folder appears: a retry picks it up.
        t.mode = pcompare_sync::sync::SyncMode::UpdateLeftToRight;
        t.left = dir.path().join("later").to_string_lossy().into_owned();
        w.ensure(&t, false);
        assert!(!w.status("t1").unwrap().watching);
        fs::create_dir_all(&t.left).unwrap();
        w.ensure(&t, false);
        assert!(
            !w.status("t1").unwrap().watching,
            "unchanged spec without retry"
        );
        w.ensure(&t, true);
        assert_eq!(
            w.status("t1").unwrap(),
            WatchStatus {
                watching: true,
                error: None
            }
        );

        // Without a real-time trigger the watch goes away.
        t.triggers.clear();
        w.ensure(&t, false);
        assert_eq!(w.status("t1"), None);
    }

    #[test]
    fn filter_skips_excluded_and_versioning_paths() {
        let f = Filter {
            roots: vec![PathBuf::from("/data")],
            exclude: exclude_set(
                &["*.tmp".into(), "build".into(), "docs/private".into()],
                false,
            ),
            ignore: vec![PathBuf::from("/data/.versions")],
        };
        assert!(f.relevant(Path::new("/data/a.txt")));
        assert!(f.relevant(Path::new("/data/src/main.rs")));
        assert!(!f.relevant(Path::new("/data/a.tmp")));
        assert!(!f.relevant(Path::new("/data/build/out.o")));
        assert!(
            !f.relevant(Path::new("/data/src/build/out.o")),
            "excluded folder at any depth"
        );
        assert!(
            !f.relevant(Path::new("/data/docs/private/x")),
            "relative-path pattern"
        );
        assert!(f.relevant(Path::new("/data/docs/public/x")));
        assert!(!f.relevant(Path::new("/data/.versions/2026/x")));
        assert!(
            f.relevant(Path::new("/data")),
            "the root itself (deleted, renamed)"
        );
        assert!(
            f.relevant(Path::new("/elsewhere/x")),
            "unknown form: count it"
        );
    }

    #[test]
    fn real_file_changes_are_reported() {
        let dir = tempfile::tempdir().unwrap();
        let (l, r) = (dir.path().join("l"), dir.path().join("r"));
        fs::create_dir_all(&l).unwrap();
        let (wakes, wake) = counter();
        let w = Watcher::new(wake);
        w.ensure(&task(&l, &r), false);
        // Some backends need a moment before the first events arrive.
        thread::sleep(Duration::from_millis(300));
        fs::write(l.join("ignored.tmp"), b"x").unwrap();
        thread::sleep(Duration::from_millis(700));
        let before = wakes.load(Ordering::SeqCst);
        assert!(w.next_due().is_none(), "excluded file: nothing pending");

        let deadline = Instant::now() + Duration::from_secs(10);
        fs::write(l.join("real.txt"), b"x").unwrap();
        while w.next_due().is_none() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(50));
        }
        assert!(w.next_due().is_some(), "a change is pending");
        assert!(
            wakes.load(Ordering::SeqCst) > before,
            "the scheduler was woken"
        );
        assert!(w.take_due(Instant::now()).is_empty(), "not settled yet");
        assert_eq!(w.take_due(Instant::now() + Duration::from_secs(2)), ["t1"]);
        drop(w); // stops cleanly
    }

    #[test]
    fn retain_and_concurrent_use() {
        let dir = tempfile::tempdir().unwrap();
        let (_, wake) = counter();
        let w = Arc::new(Watcher::new(wake));
        let mut handles = Vec::new();
        for i in 0..4 {
            let w = w.clone();
            let dir = dir.path().to_path_buf();
            handles.push(thread::spawn(move || {
                let side = dir.join(format!("s{i}"));
                fs::create_dir_all(&side).unwrap();
                let mut t = task(&side, &dir.join("out"));
                t.id = format!("t{i}");
                for _ in 0..5 {
                    w.ensure(&t, true);
                    w.set_running(&t.id, true);
                    w.set_running(&t.id, false);
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        let mut ids = w.task_ids();
        ids.sort();
        assert_eq!(ids, ["t0", "t1", "t2", "t3"]);
        w.retain(&["t1".to_owned()].into_iter().collect());
        assert_eq!(w.task_ids(), ["t1"]);
    }
}
