//! Groups "Compare with PCompare" on a multi-selection into one comparison.
//!
//! A static Explorer verb runs its command once per selected item, so two
//! selected folders start two `pcompare --compare-selection <path>`
//! processes at (nearly) the same moment. Each one drops its path into a
//! spool directory; the first to create the claim file waits until no more
//! arrive, takes every spooled path and opens them together. The others just
//! exit: their path is already in the spool.

use std::fs::{self, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::thread::sleep;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const CLAIM: &str = "claim.lock";
const EXT: &str = "path";
/// Leftovers from an interrupted run older than this are ignored.
const STALE: Duration = Duration::from_secs(10);

pub struct Spool {
    dir: PathBuf,
}

/// Held by the process that collects the selection; releases the claim on drop.
pub struct Claim<'a> {
    spool: &'a Spool,
}

impl Spool {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// `%TEMP%\pcompare-selection`, shared by every process of this user.
    pub fn default_location() -> Self {
        Self::new(std::env::temp_dir().join("pcompare-selection"))
    }

    /// Adds one selected item. Names sort by arrival (nanoseconds, then pid).
    pub fn add(&self, path: &str) -> io::Result<()> {
        fs::create_dir_all(&self.dir)?;
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let name = format!("{nanos:024}-{:010}.{EXT}", std::process::id());
        // Write then rename, so a collector never reads a half-written path.
        let tmp = self.dir.join(format!("{name}.tmp"));
        fs::write(&tmp, path)?;
        fs::rename(tmp, self.dir.join(name))
    }

    /// Becomes the collector, or `None` when another process already is.
    pub fn try_claim(&self) -> io::Result<Option<Claim<'_>>> {
        fs::create_dir_all(&self.dir)?;
        let claim = self.dir.join(CLAIM);
        // A collector that crashed mid-way must not block the menu forever.
        if is_older_than(&claim, STALE) {
            let _ = fs::remove_file(&claim);
        }
        match OpenOptions::new().write(true).create_new(true).open(&claim) {
            Ok(_) => Ok(Some(Claim { spool: self })),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn pending(&self) -> Vec<PathBuf> {
        let mut files: Vec<PathBuf> = fs::read_dir(&self.dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == EXT))
            .collect();
        files.sort();
        files
    }
}

fn is_older_than(path: &Path, age: Duration) -> bool {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|e| e > age)
}

impl Claim<'_> {
    /// Waits until no item has arrived for `quiet` (at most `max` in total),
    /// then takes every spooled path in arrival order, without duplicates.
    pub fn collect(self, quiet: Duration, max: Duration) -> Vec<String> {
        let start = Instant::now();
        let mut seen = self.spool.pending().len();
        let mut last_change = Instant::now();
        while last_change.elapsed() < quiet && start.elapsed() < max {
            sleep(Duration::from_millis(25));
            let now = self.spool.pending().len();
            if now != seen {
                seen = now;
                last_change = Instant::now();
            }
        }
        let mut paths: Vec<String> = Vec::new();
        for file in self.spool.pending() {
            let fresh = !is_older_than(&file, STALE);
            if let (true, Ok(p)) = (fresh, fs::read_to_string(&file)) {
                let p = p.trim().to_owned();
                if !p.is_empty() && !paths.contains(&p) {
                    paths.push(p);
                }
            }
            let _ = fs::remove_file(file);
        }
        paths
    }
}

impl Drop for Claim<'_> {
    fn drop(&mut self) {
        let _ = fs::remove_file(self.spool.dir.join(CLAIM));
    }
}

/// Puts the selected items into launch order: `[left, right]` or, for three,
/// `[left, right, base]` (how `pcompare a b c` reads its paths). `paths` come
/// in arrival order, which Explorer doesn't guarantee to match the order the
/// user clicked them in.
///
/// TODO(you): this default sorts by path, case-insensitively, so `v1` lands
/// left of `v2` and `old` left of `new`... alphabetically, which is mostly
/// but not always what people expect. See the note in the PR for the options.
pub fn arrange(mut paths: Vec<String>) -> Vec<String> {
    paths.sort_by_key(|p| p.to_lowercase());
    paths.truncate(3);
    paths
}

#[cfg(test)]
mod tests {
    use super::*;

    const QUICK: Duration = Duration::from_millis(60);

    #[test]
    fn one_collector_gathers_every_selected_item() {
        let dir = tempfile::tempdir().unwrap();
        let spool = Spool::new(dir.path());
        spool.add(r"C:\a").unwrap();
        let claim = spool.try_claim().unwrap().expect("first process claims");
        spool.add(r"C:\b").unwrap();
        // A second process sees the claim and leaves its path for the collector.
        assert!(spool.try_claim().unwrap().is_none());
        spool.add(r"C:\a").unwrap(); // duplicates collapse
        assert_eq!(
            claim.collect(QUICK, Duration::from_secs(2)),
            vec![r"C:\a", r"C:\b"]
        );
        // The claim is released and the spool is empty for the next selection.
        let next = spool.try_claim().unwrap().expect("claim released");
        assert!(next.collect(QUICK, Duration::from_secs(1)).is_empty());
    }

    #[test]
    fn late_arrivals_extend_the_wait() {
        let dir = tempfile::tempdir().unwrap();
        let spool = Spool::new(dir.path());
        spool.add("first").unwrap();
        let claim = spool.try_claim().unwrap().unwrap();
        let path = dir.path().to_owned();
        let late = std::thread::spawn(move || {
            sleep(Duration::from_millis(40));
            Spool::new(path).add("second").unwrap();
        });
        let got = claim.collect(Duration::from_millis(120), Duration::from_secs(2));
        late.join().unwrap();
        assert_eq!(got, vec!["first", "second"]);
    }

    #[test]
    fn arrange_orders_and_caps_the_selection() {
        let v = |s: &[&str]| s.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(arrange(v(&["v2", "V1"])), v(&["V1", "v2"]));
        assert_eq!(arrange(v(&["d", "c", "b", "a"])).len(), 3);
    }
}
