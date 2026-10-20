//! Backups of the files PCompare overwrites or removes, so any change made by
//! copying left → right (or right → left), saving an edited side, or running
//! a sync or merge can be put back later.
//!
//! One [`BackupStore`] folder holds every backup. Each operation (one copy,
//! one save, one sync run) that replaced something gets its own folder:
//!
//! ```text
//! <root>/2026-10-20/143012-1a2b/
//!     backup.json     when, which tool, what it did, the compared sides
//!     items.jsonl     one line per kept file: where it lived, size, time
//!     files/0/report.txt
//!     files/1/notes.md
//! ```
//!
//! `items.jsonl` is appended as files are kept, so a crash mid-operation
//! still leaves a usable record of everything kept up to that point. Day and
//! time in folder names are UTC; the UI shows local times from `backup.json`.

use crate::store::{now_ms, utc_civil, write_json};
use crate::{CoreError, Result};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Component, Path, PathBuf};
use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, UNIX_EPOCH};

const HEADER: &str = "backup.json";
const ITEMS: &str = "items.jsonl";
const FILES: &str = "files";

// ---------- settings ----------

/// The user's backup preferences: `backups` in the app's `prefs.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct BackupSettings {
    /// Keep a backup of every file PCompare overwrites or removes.
    pub enabled: bool,
    /// The one-time "Keep backups?" question has been answered.
    pub asked: bool,
    /// Where backups go; empty = the default folder in the app's data folder.
    pub folder: String,
    /// Days a backup is kept; 0 = until removed by hand.
    pub keep_days: u32,
    /// Size limit for the whole backup folder in MB; 0 = no limit.
    pub max_size_mb: u64,
}

impl Default for BackupSettings {
    fn default() -> Self {
        BackupSettings {
            enabled: false,
            asked: false,
            folder: String::new(),
            keep_days: 30,
            max_size_mb: 0,
        }
    }
}

impl BackupSettings {
    /// Reads the `backups` entry of the app preferences (defaults when it is
    /// missing or malformed).
    pub fn from_prefs(prefs: &serde_json::Value) -> Self {
        prefs
            .get("backups")
            .and_then(|b| serde_json::from_value(b.clone()).ok())
            .unwrap_or_default()
    }

    /// The backup folder: the chosen one, or `default` when none is set.
    pub fn root(&self, default: &Path) -> PathBuf {
        match self.folder.trim() {
            "" => default.to_path_buf(),
            f => PathBuf::from(f),
        }
    }
}

// ---------- records ----------

/// Who made a change and what it was, as shown on the Backups page.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct BackupOrigin {
    /// The tool, e.g. "Folder Compare", "Text Compare", "Folder Sync".
    pub tool: String,
    /// What it did, e.g. "Copy left → right", "Save right side".
    pub action: String,
    /// The compared sides, for context ("" when there are none).
    pub left: String,
    pub right: String,
}

/// Why a file was kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum BackupReason {
    /// Something new was written in its place.
    Replaced,
    /// It was deleted (by a sync, a merge, or before a restore put a folder back).
    Removed,
}

/// One kept file or folder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupItem {
    /// Where it lived (absolute), which is where a restore puts it back.
    pub original: String,
    /// Where the copy is, relative to the backup's folder (`/`-separated).
    /// Unique within a backup, so it doubles as the item's key.
    pub stored: String,
    pub is_dir: bool,
    /// Bytes (a folder: everything inside it).
    pub size: u64,
    /// Last modified, in ms since the Unix epoch (restores set it back).
    pub modified_ms: Option<i64>,
    pub reason: BackupReason,
}

/// What `backup.json` holds.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct Header {
    time_ms: i64,
    #[serde(flatten)]
    origin: BackupOrigin,
}

/// One backup (an operation's kept files), as listed on the Backups page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Backup {
    /// `<day>/<folder>`, relative to the store; names it to every call.
    pub id: String,
    /// The backup's folder (absolute), for "Show in folder".
    pub folder: String,
    pub time_ms: i64,
    #[serde(flatten)]
    pub origin: BackupOrigin,
    pub items: Vec<BackupItem>,
    /// Bytes kept, all items together.
    pub size: u64,
}

/// How one item of a restore went.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RestoreOutcome {
    pub original: String,
    pub stored: String,
    pub error: Option<String>,
}

/// What [`BackupStore::prune`] removed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PruneReport {
    pub removed: usize,
    pub freed: u64,
}

// ---------- the store ----------

/// The backup folder.
#[derive(Debug, Clone)]
pub struct BackupStore {
    root: PathBuf,
}

impl BackupStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        BackupStore { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Starts recording one operation. Nothing touches the disk until the
    /// first file is kept, so operations that replace nothing leave no trace.
    pub fn begin(&self, origin: BackupOrigin) -> BackupBatch {
        BackupBatch {
            root: self.root.clone(),
            origin,
            state: Mutex::new(BatchState::default()),
        }
    }

    /// Every backup, newest first. Folders that aren't backups are skipped.
    pub fn list(&self) -> Result<Vec<Backup>> {
        let mut out = Vec::new();
        for day in read_dir_sorted(&self.root)? {
            if !day.is_dir() {
                continue;
            }
            for dir in read_dir_sorted(&day)? {
                if let Some(b) = read_backup(&self.root, &dir)? {
                    out.push(b);
                }
            }
        }
        out.sort_by(|a, b| b.time_ms.cmp(&a.time_ms).then_with(|| b.id.cmp(&a.id)));
        Ok(out)
    }

    /// One backup by id.
    pub fn get(&self, id: &str) -> Result<Backup> {
        let dir = self.dir_of(id)?;
        read_backup(&self.root, &dir)?.ok_or_else(|| CoreError::Other(format!("backup {id} no longer exists")))
    }

    /// Puts kept items back where they lived: those named by `stored` (see
    /// [`BackupItem::stored`]), or all of them when `stored` is empty. What
    /// is there now is kept in `before` first, so a restore can be undone too.
    pub fn restore(&self, id: &str, stored: &[String], before: &BackupBatch) -> Result<Vec<RestoreOutcome>> {
        let backup = self.get(id)?;
        let dir = self.dir_of(id)?;
        let picked: Vec<&BackupItem> = backup
            .items
            .iter()
            .filter(|i| stored.is_empty() || stored.contains(&i.stored))
            .collect();
        if picked.is_empty() {
            return Err(CoreError::Other("nothing to restore".into()));
        }
        Ok(picked
            .into_iter()
            .map(|item| RestoreOutcome {
                original: item.original.clone(),
                stored: item.stored.clone(),
                error: restore_item(&dir, item, before).err().map(|e| e.to_string()),
            })
            .collect())
    }

    /// Removes a backup for good.
    pub fn delete(&self, id: &str) -> Result<()> {
        let dir = self.dir_of(id)?;
        fs::remove_dir_all(&dir).map_err(|e| CoreError::io(&dir, e))?;
        if let Some(day) = dir.parent() {
            let _ = fs::remove_dir(day); // only when it was the day's last backup
        }
        Ok(())
    }

    /// Removes the backups `settings` no longer keeps (see [`expired`]).
    /// Backups this process is still writing are left alone.
    pub fn prune(&self, settings: &BackupSettings, now_ms: i64) -> Result<PruneReport> {
        let mut all = self.list()?;
        let writing = in_progress().lock().unwrap_or_else(|e| e.into_inner()).clone();
        all.retain(|b| !writing.contains(Path::new(&b.folder)));
        let mut report = PruneReport::default();
        for i in expired(&all, settings, now_ms) {
            if self.delete(&all[i].id).is_ok() {
                report.removed += 1;
                report.freed += all[i].size;
            }
        }
        Ok(report)
    }

    /// The folder of backup `id`, refusing ids that would leave the store.
    fn dir_of(&self, id: &str) -> Result<PathBuf> {
        let rel = Path::new(id);
        let ok = rel.components().count() == 2 && rel.components().all(|c| matches!(c, Component::Normal(_)));
        if !ok {
            return Err(CoreError::Other(format!("not a backup id: {id}")));
        }
        Ok(self.root.join(rel))
    }
}

/// Which backups to remove under `settings`, as indexes into `all` (newest
/// first, as [`BackupStore::list`] returns them).
///
/// Backups older than `keep_days` go; then, while the rest is over
/// `max_size_mb`, more go. The newest backup always stays, so one larger
/// than the limit isn't removed the moment it is made.
pub fn expired(all: &[Backup], settings: &BackupSettings, now_ms: i64) -> Vec<usize> {
    let day_ms = 86_400_000i64;
    let mut out: Vec<usize> = Vec::new();
    if settings.keep_days > 0 {
        let cutoff = now_ms - i64::from(settings.keep_days) * day_ms;
        out.extend(all.iter().enumerate().filter(|(_, b)| b.time_ms < cutoff).map(|(i, _)| i));
    }
    if settings.max_size_mb > 0 {
        let cap = settings.max_size_mb.saturating_mul(1024 * 1024);
        let mut total: u64 = all.iter().enumerate().filter(|(i, _)| !out.contains(i)).map(|(_, b)| b.size).sum();
        // TODO(you): which backups give way when the folder is over its size
        // limit. This removes the oldest first, until it fits — even the only
        // copy of a file's earlier version. Alternatives: largest first (frees
        // space with the fewest removals), or always spare the newest backup
        // of each original path so every file keeps one version to go back to.
        for i in (1..all.len()).rev() {
            if total <= cap {
                break;
            }
            if !out.contains(&i) {
                out.push(i);
                total = total.saturating_sub(all[i].size);
            }
        }
    }
    out
}

// ---------- recording one operation ----------

/// The folders of backups this process is still writing; pruning skips them.
fn in_progress() -> &'static Mutex<HashSet<PathBuf>> {
    static SET: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
    SET.get_or_init(Default::default)
}

/// Keeps the files one operation replaces or removes. Shared by the threads
/// of a parallel copy.
pub struct BackupBatch {
    root: PathBuf,
    origin: BackupOrigin,
    state: Mutex<BatchState>,
}

#[derive(Default)]
struct BatchState {
    /// The backup's folder and `items.jsonl`, once the first item is kept.
    open: Option<(String, PathBuf, File)>,
    next: usize,
    kept: usize,
}

impl BackupBatch {
    /// The backup's id, once something was kept.
    pub fn id(&self) -> Option<String> {
        self.lock().open.as_ref().map(|(id, _, _)| id.clone())
    }

    /// How many items were kept.
    pub fn kept(&self) -> usize {
        self.lock().kept
    }

    /// Copies `original` (a file or folder) into the backup before it is
    /// changed in place, e.g. before a save. Returns where the copy went, or
    /// `None` when there is nothing at `original` yet.
    pub fn keep_copy(&self, original: &Path) -> io::Result<Option<PathBuf>> {
        let meta = match fs::symlink_metadata(original) {
            Ok(m) => m,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        let (stored, target) = self.reserve(original)?;
        if let Err(e) = copy_tree(original, &target) {
            let _ = remove_any(&target);
            return Err(e);
        }
        self.record(original, &meta, stored, &target, BackupReason::Replaced)?;
        Ok(Some(target))
    }

    /// Moves `current` — what lived at `original` — into the backup. For an
    /// item already moved aside (`current != original`, e.g. a sync's staged
    /// old version) or about to be replaced or deleted (`current ==
    /// original`). Falls back to copy + delete across volumes.
    pub fn keep_move(&self, current: &Path, original: &Path, reason: BackupReason) -> io::Result<PathBuf> {
        let meta = fs::symlink_metadata(current)?;
        let size = tree_size(current);
        let (stored, target) = self.reserve(original)?;
        if fs::rename(current, &target).is_err() {
            // Across volumes (or a rename refused): copy, then delete. If the
            // original can't go, the copy goes instead and the caller sees why.
            if let Err(e) = copy_tree(current, &target).and_then(|()| remove_any(current)) {
                let _ = remove_any(&target);
                return Err(e);
            }
        }
        self.append(item(original, &meta, size, stored, reason))?;
        Ok(target)
    }

    /// A fresh `files/<n>/<name>` slot for `original`, creating the backup's
    /// folder on first use.
    fn reserve(&self, original: &Path) -> io::Result<(String, PathBuf)> {
        let name = original
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "item".into());
        let mut st = self.lock();
        if st.open.is_none() {
            st.open = Some(self.create()?);
        }
        let n = st.next;
        st.next += 1;
        let (_, dir, _) = st.open.as_ref().expect("opened above");
        let slot = dir.join(FILES).join(n.to_string());
        drop(st);
        fs::create_dir_all(&slot)?;
        Ok((format!("{FILES}/{n}/{name}"), slot.join(&name)))
    }

    /// Makes the backup's folder and header.
    fn create(&self) -> io::Result<(String, PathBuf, File)> {
        let time_ms = now_ms();
        let [y, mo, d, h, mi, s] = utc_civil(time_ms.div_euclid(1000));
        let day = format!("{y:04}-{mo:02}-{d:02}");
        let base = format!("{h:02}{mi:02}{s:02}");
        let day_dir = self.root.join(&day);
        fs::create_dir_all(&day_dir)?;
        // Two operations in the same second get "-2", "-3", …
        let (name, dir) = (1..)
            .map(|n| if n == 1 { base.clone() } else { format!("{base}-{n}") })
            .map(|name| (name.clone(), day_dir.join(name)))
            .find(|(_, dir)| fs::create_dir(dir).is_ok())
            .expect("some suffix is free");
        let header = Header {
            time_ms,
            origin: self.origin.clone(),
        };
        in_progress().lock().unwrap_or_else(|e| e.into_inner()).insert(dir.clone());
        write_json(&dir.join(HEADER), &header).map_err(|e| io::Error::other(e.to_string()))?;
        let log = OpenOptions::new().create(true).append(true).open(dir.join(ITEMS))?;
        Ok((format!("{day}/{name}"), dir, log))
    }

    fn record(&self, original: &Path, meta: &fs::Metadata, stored: String, copy: &Path, reason: BackupReason) -> io::Result<()> {
        self.append(item(original, meta, tree_size(copy), stored, reason))
    }

    fn append(&self, item: BackupItem) -> io::Result<()> {
        let mut line = serde_json::to_vec(&item).map_err(io::Error::other)?;
        line.push(b'\n');
        let mut st = self.lock();
        let (_, _, log) = st.open.as_mut().expect("an item was reserved");
        log.write_all(&line)?;
        log.flush()?;
        st.kept += 1;
        Ok(())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BatchState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl Drop for BackupBatch {
    fn drop(&mut self) {
        if let Some((_, dir, _)) = &self.lock().open {
            in_progress().lock().unwrap_or_else(|e| e.into_inner()).remove(dir);
        }
    }
}

fn item(original: &Path, meta: &fs::Metadata, size: u64, stored: String, reason: BackupReason) -> BackupItem {
    BackupItem {
        original: original.to_string_lossy().into_owned(),
        stored,
        is_dir: meta.is_dir(),
        size,
        modified_ms: meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as i64),
        reason,
    }
}

// ---------- reading and restoring ----------

fn read_backup(root: &Path, dir: &Path) -> Result<Option<Backup>> {
    let header_file = dir.join(HEADER);
    let header: Header = match fs::read_to_string(&header_file) {
        Ok(t) => match serde_json::from_str(&t) {
            Ok(h) => h,
            Err(_) => return Ok(None),
        },
        Err(e) if matches!(e.kind(), io::ErrorKind::NotFound | io::ErrorKind::NotADirectory) => return Ok(None),
        Err(e) => return Err(CoreError::io(&header_file, e)),
    };
    let mut items = Vec::new();
    if let Ok(f) = File::open(dir.join(ITEMS)) {
        // A line cut short by a crash is skipped; the rest still count.
        items.extend(
            BufReader::new(f)
                .lines()
                .map_while(|l| l.ok())
                .filter_map(|l| serde_json::from_str::<BackupItem>(&l).ok()),
        );
    }
    let rel = dir.strip_prefix(root).unwrap_or(dir);
    let id = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/");
    Ok(Some(Backup {
        id,
        folder: dir.to_string_lossy().into_owned(),
        time_ms: header.time_ms,
        size: items.iter().map(|i| i.size).sum(),
        origin: header.origin,
        items,
    }))
}

fn restore_item(dir: &Path, item: &BackupItem, before: &BackupBatch) -> io::Result<()> {
    let rel = Path::new(&item.stored);
    if !rel.components().all(|c| matches!(c, Component::Normal(_))) {
        return Err(io::Error::other("the backup's record is damaged"));
    }
    let src = dir.join(rel);
    if fs::symlink_metadata(&src).is_err() {
        return Err(io::Error::other("the backed-up copy is missing"));
    }
    let original = PathBuf::from(&item.original);
    if let Some(parent) = original.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = sibling_temp(&original);
    if let Err(e) = copy_tree(&src, &tmp) {
        let _ = remove_any(&tmp);
        return Err(e);
    }
    // What is there now goes into `before`: a file is copied and then
    // replaced in one rename; a folder (or a file where a folder was) is
    // moved there, since a rename can't replace it.
    let current = fs::symlink_metadata(&original).ok();
    let kept = match &current {
        Some(m) if m.is_file() && !item.is_dir => before.keep_copy(&original).map(|_| ()),
        Some(_) => before.keep_move(&original, &original, BackupReason::Removed).map(|_| ()),
        None => Ok(()),
    };
    if let Err(e) = kept {
        let _ = remove_any(&tmp);
        return Err(io::Error::other(format!("couldn't back up the current version first: {e}")));
    }
    if let Err(e) = fs::rename(&tmp, &original) {
        let _ = remove_any(&tmp);
        return Err(e);
    }
    if let (Some(ms), false) = (item.modified_ms, item.is_dir) {
        let _ = set_modified(&original, ms);
    }
    Ok(())
}

/// A free temp name beside `path`.
fn sibling_temp(path: &Path) -> PathBuf {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    (0..)
        .map(|n| path.with_file_name(format!(".{name}.pcompare-restore-{n}")))
        .find(|p| fs::symlink_metadata(p).is_err())
        .expect("some suffix is free")
}

// ---------- file helpers ----------

/// Copies a file (keeping its modified time), a symlink (as a link where
/// the OS allows, else its target's contents) or a whole folder.
fn copy_tree(src: &Path, dst: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(src)?;
    if meta.is_dir() {
        fs::create_dir_all(dst)?;
        for entry in fs::read_dir(src)? {
            let entry = entry?;
            copy_tree(&entry.path(), &dst.join(entry.file_name()))?;
        }
        return Ok(());
    }
    #[cfg(unix)]
    if meta.file_type().is_symlink() {
        return std::os::unix::fs::symlink(fs::read_link(src)?, dst);
    }
    fs::copy(src, dst)?;
    if let Ok(t) = meta.modified() {
        let _ = File::options().write(true).open(dst).and_then(|f| f.set_modified(t));
    }
    Ok(())
}

fn remove_any(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() => fs::remove_dir_all(path),
        Ok(m) => {
            if m.permissions().readonly() {
                let mut p = m.permissions();
                #[allow(clippy::permissions_set_readonly_false)]
                p.set_readonly(false);
                let _ = fs::set_permissions(path, p);
            }
            fs::remove_file(path)
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Bytes in a file, or in everything inside a folder.
fn tree_size(path: &Path) -> u64 {
    match fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() => fs::read_dir(path)
            .map(|rd| rd.flatten().map(|e| tree_size(&e.path())).sum())
            .unwrap_or(0),
        Ok(m) => m.len(),
        Err(_) => 0,
    }
}

fn set_modified(path: &Path, ms: i64) -> io::Result<()> {
    let t = if ms >= 0 {
        UNIX_EPOCH + Duration::from_millis(ms as u64)
    } else {
        UNIX_EPOCH - Duration::from_millis(ms.unsigned_abs())
    };
    File::options().write(true).open(path)?.set_modified(t)
}

/// A folder's entries, sorted by name (missing folder = none).
fn read_dir_sorted(dir: &Path) -> Result<Vec<PathBuf>> {
    let rd = match fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(CoreError::io(dir, e)),
    };
    let mut out: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
    out.sort();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn origin(action: &str) -> BackupOrigin {
        BackupOrigin {
            tool: "Folder Compare".into(),
            action: action.into(),
            left: "/l".into(),
            right: "/r".into(),
        }
    }

    #[test]
    fn nothing_kept_leaves_no_folder() {
        let dir = tempfile::tempdir().unwrap();
        let store = BackupStore::new(dir.path().join("b"));
        let batch = store.begin(origin("Copy left → right"));
        assert_eq!(batch.id(), None);
        assert!(!dir.path().join("b").exists());
        assert!(store.list().unwrap().is_empty());
    }

    #[test]
    fn keeps_copies_and_restores_them() {
        let dir = tempfile::tempdir().unwrap();
        let work = dir.path().join("work");
        fs::create_dir_all(&work).unwrap();
        let a = work.join("a.txt");
        fs::write(&a, "old a").unwrap();
        let store = BackupStore::new(dir.path().join("b"));

        let batch = store.begin(origin("Save right side"));
        let copy = batch.keep_copy(&a).unwrap().unwrap();
        assert_eq!(fs::read_to_string(&copy).unwrap(), "old a");
        assert_eq!(batch.keep_copy(&work.join("missing.txt")).unwrap(), None);
        fs::write(&a, "new a").unwrap();

        let all = store.list().unwrap();
        assert_eq!(all.len(), 1);
        let b = &all[0];
        assert_eq!(Some(b.id.clone()), batch.id());
        assert_eq!(b.origin.action, "Save right side");
        assert_eq!(b.items.len(), 1);
        assert_eq!(b.items[0].original, a.to_string_lossy());
        assert_eq!(b.items[0].reason, BackupReason::Replaced);
        assert_eq!(b.size, 5);

        let before = store.begin(origin("Restore"));
        let out = store.restore(&b.id, &[], &before).unwrap();
        assert_eq!(out[0].error, None);
        assert_eq!(fs::read_to_string(&a).unwrap(), "old a");
        // The version the restore replaced was kept too, so it can be undone.
        let undo = store.get(&before.id().unwrap()).unwrap();
        assert_eq!(fs::read_to_string(Path::new(&undo.folder).join(&undo.items[0].stored)).unwrap(), "new a");
    }

    #[test]
    fn moves_staged_versions_and_folders() {
        let dir = tempfile::tempdir().unwrap();
        let right = dir.path().join("right");
        fs::create_dir_all(right.join("sub/deep")).unwrap();
        fs::write(right.join("sub/deep/x.txt"), "xx").unwrap();
        let staged = right.join(".f.txt.staged");
        fs::write(&staged, "old f").unwrap();
        let store = BackupStore::new(dir.path().join("b"));
        let batch = store.begin(origin("Sync"));

        batch.keep_move(&staged, &right.join("f.txt"), BackupReason::Replaced).unwrap();
        batch.keep_move(&right.join("sub"), &right.join("sub"), BackupReason::Removed).unwrap();
        assert!(!staged.exists() && !right.join("sub").exists());

        let b = store.get(&batch.id().unwrap()).unwrap();
        assert_eq!(b.items.len(), 2);
        assert!(b.items[1].is_dir);
        assert_eq!(b.size, 7);

        let before = store.begin(origin("Restore"));
        let picked = vec![b.items[1].stored.clone()];
        let out = store.restore(&b.id, &picked, &before).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(fs::read_to_string(right.join("sub/deep/x.txt")).unwrap(), "xx");
        assert!(!right.join("f.txt").exists(), "only the picked item comes back");
        assert_eq!(before.id(), None, "nothing was in the way");
    }

    #[test]
    fn restore_puts_back_the_modified_time() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("f.txt");
        fs::write(&f, "v1").unwrap();
        set_modified(&f, 1_000_000_000_000).unwrap();
        let store = BackupStore::new(dir.path().join("b"));
        let batch = store.begin(origin("Save"));
        batch.keep_copy(&f).unwrap();
        fs::write(&f, "v2").unwrap();
        store.restore(&batch.id().unwrap(), &[], &store.begin(origin("Restore"))).unwrap();
        let t = fs::metadata(&f).unwrap().modified().unwrap();
        assert_eq!(t.duration_since(UNIX_EPOCH).unwrap().as_millis(), 1_000_000_000_000);
    }

    #[test]
    fn same_second_operations_get_their_own_folders() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("f.txt");
        fs::write(&f, "x").unwrap();
        let store = BackupStore::new(dir.path().join("b"));
        let ids: Vec<String> = (0..3)
            .map(|_| {
                let b = store.begin(origin("Save"));
                b.keep_copy(&f).unwrap();
                b.id().unwrap()
            })
            .collect();
        assert_eq!(store.list().unwrap().len(), 3);
        assert!(ids[0] != ids[1] && ids[1] != ids[2]);
    }

    #[test]
    fn rejects_ids_outside_the_store() {
        let dir = tempfile::tempdir().unwrap();
        let store = BackupStore::new(dir.path());
        for bad in ["..", "../x", "a/../../b", "/etc/passwd", "one", "a/b/c"] {
            assert!(store.delete(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn delete_removes_the_backup_and_its_empty_day() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("f.txt");
        fs::write(&f, "x").unwrap();
        let store = BackupStore::new(dir.path().join("b"));
        let batch = store.begin(origin("Save"));
        batch.keep_copy(&f).unwrap();
        store.delete(&batch.id().unwrap()).unwrap();
        assert!(store.list().unwrap().is_empty());
        assert_eq!(fs::read_dir(store.root()).unwrap().count(), 0);
    }

    fn fake(time_ms: i64, size: u64) -> Backup {
        Backup {
            id: format!("d/{time_ms}"),
            folder: String::new(),
            time_ms,
            origin: BackupOrigin::default(),
            items: Vec::new(),
            size,
        }
    }

    #[test]
    fn expiry_by_age_and_size() {
        let day = 86_400_000;
        let now = 100 * day;
        // Newest first, as list() returns them.
        let all = vec![fake(now - day, 600), fake(now - 10 * day, 600), fake(now - 40 * day, 600)];
        let age = BackupSettings {
            keep_days: 30,
            ..Default::default()
        };
        assert_eq!(expired(&all, &age, now), vec![2]);
        let forever = BackupSettings {
            keep_days: 0,
            ..Default::default()
        };
        assert!(expired(&all, &forever, now).is_empty());

        let mb = 1024 * 1024;
        let big: Vec<Backup> = all.iter().map(|b| fake(b.time_ms, b.size * mb)).collect();
        let capped = BackupSettings {
            keep_days: 0,
            max_size_mb: 1000,
            ..Default::default()
        };
        // 1800 MB over a 1000 MB cap: the oldest goes, then the next oldest.
        assert_eq!(expired(&big, &capped, now), vec![2, 1]);
        let tiny = BackupSettings {
            keep_days: 0,
            max_size_mb: 100,
            ..Default::default()
        };
        assert_eq!(expired(&big, &tiny, now), vec![2, 1], "the newest stays, even over the limit");
    }

    #[test]
    fn prune_removes_what_expired() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("f.txt");
        fs::write(&f, "12345").unwrap();
        let store = BackupStore::new(dir.path().join("b"));
        store.begin(origin("Save")).keep_copy(&f).unwrap();
        let settings = BackupSettings {
            keep_days: 1,
            ..Default::default()
        };
        assert_eq!(store.prune(&settings, now_ms()).unwrap().removed, 0);
        let later = now_ms() + 2 * 86_400_000;
        let writing = store.begin(origin("Sync"));
        writing.keep_copy(&f).unwrap();
        assert_eq!(store.prune(&settings, later).unwrap(), PruneReport { removed: 1, freed: 5 });
        assert_eq!(store.list().unwrap().len(), 1, "the backup still being written stays");
        drop(writing);
        assert_eq!(store.prune(&settings, later).unwrap().removed, 1);
        assert!(store.list().unwrap().is_empty());
    }

    #[test]
    fn settings_come_from_prefs() {
        let prefs = serde_json::json!({ "theme": "dark", "backups": { "enabled": true, "keepDays": 7 } });
        let s = BackupSettings::from_prefs(&prefs);
        assert!(s.enabled);
        assert_eq!(s.keep_days, 7);
        assert_eq!(s.root(Path::new("/d")), PathBuf::from("/d"));
        assert_eq!(BackupSettings::from_prefs(&serde_json::json!({})), BackupSettings::default());
    }
}
