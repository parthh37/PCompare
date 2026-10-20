//! File operations on a Folder Compare selection: copy or
//! move to the other side or to any folder, delete (Recycle Bin / Trash or
//! permanently), rename, new folder, change attributes and touch — on local
//! folders and remote ones alike, through [`Root`].
//!
//! Operations are built to run in the background:
//! - each reports [`FileOpProgress`] (files, bytes, the current file),
//! - can be paused between files and inside large copies, and cancelled,
//!   through a shared [`Control`],
//! - settles name clashes by a [`ConflictMode`], asking the caller when it
//!   is [`ConflictMode::Ask`],
//! - copies and moves treat a file in use by another program like Folder
//!   Sync does: retried with backoff, then tried once more after everything
//!   else, and if it is still held the failure names the program (Windows),
////! - and the [`OpQueue`] serializes operations whose paths overlap, so two
//!   background jobs never write (or read what the other writes) at once.
//!
//! Local copies keep every guarantee of the sync executor: data goes to a
//! temp sibling, is flushed and renamed into place, and replaced files go to
//! the Recycle Bin / Trash unless the caller opts out. Remote folders have
//! no Recycle Bin, so deletions and overwrites there are permanent.

use std::fs::{self, File};
use std::io::{self, Read, Seek};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, UNIX_EPOCH};

use filetime::FileTime;
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use pcompare_core::folder::{attr, pair_passes, EntryMeta, FolderFilters};
use pcompare_core::vfs_write::{pump, VfsWrite};
use serde::{Deserialize, Serialize};

use crate::exec::{self, backoff_delay, hold, is_transient, ExecOptions};
use crate::failure::{classify, lock_holders, FailureKind, LockHolder, OpError};
use crate::fsops::{self, AttrChange, Deleter, PermanentDeleter};
use crate::remote::Root;

// ---------- requests ----------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FileAction {
    /// Copy items from the source root to the destination root.
    Copy,
    /// Copy, then remove the originals (a rename when both are on one volume).
    Move,
    Delete,
    /// Rename the single item to [`FileOpOptions::new_name`].
    Rename,
    /// Create the folder named by the single item.
    NewFolder,
    /// Apply [`FileOpOptions::attributes`] (local folders only).
    Attributes,
    /// Set modification times per [`FileOpOptions::touch`].
    Touch,
}

/// What to do when a copy finds a file already at its destination.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ConflictMode {
    /// Ask about each clash (the caller's `ask` hook answers).
    #[default]
    Ask,
    Overwrite,
    Skip,
    /// Overwrite only files older than the one being copied.
    OverwriteOlder,
}

/// The answer to a [`ConflictQuestion`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ConflictAnswer {
    Overwrite,
    Skip,
    OverwriteAll,
    SkipAll,
    OverwriteOlderAll,
    /// Stop the whole operation.
    Cancel,
}

/// The time a touch sets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum TouchTime {
    /// The current time, taken once when the operation starts.
    #[default]
    Now,
    /// A chosen time, in milliseconds since the Unix epoch.
    At { ms: i64 },
    /// The modification time of the same item on the other side.
    OtherSide,
}

/// One selected item; `/`-separated paths relative to the operation's roots.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct FileItem {
    /// Under the source root: the item copied, moved, deleted, renamed,
    /// created, changed or touched.
    pub src: String,
    /// Under the destination root: where a copy or move goes, or (touch "to
    /// other side") the counterpart whose time is copied.
    pub dst: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct FileOpOptions {
    pub conflicts: ConflictMode,
    /// Delete for good instead of moving to the Recycle Bin / Trash.
    pub permanent: bool,
    /// Copies and moves send the local files they replace to the Recycle Bin
    /// / Trash (otherwise they are overwritten for good).
    pub trash_replaced: bool,
    /// Windows: copy NTFS owners and ACLs along (best effort).
    pub copy_acls: bool,
    /// Windows: carry DOS 8.3 short names over (needs administrator rights).
    pub copy_short_names: bool,
    /// Rename: the new name (a single path component).
    pub new_name: String,
    /// Attributes: bits to set and clear.
    pub attributes: AttrChange,
    /// Attributes and touch: apply to everything inside selected folders too.
    pub recursive: bool,
    pub touch: TouchTime,
    /// Copy and move: leave out what's inside selected folders that these
    /// filters hide in the comparison (the default, with
    /// "Include filtered items" off). `None`: everything is copied.
    pub filter: Option<ItemFilter>,
    /// Copy and move: files transferred at once. 0 picks a number for the
    /// two sides ([`copy_threads`]).
    pub threads: usize,
    /// Copy and move: extra attempts after the first when a file is in use
    /// by another program or the connection drops ([`is_transient`]). The
    /// wait starts at `retry_delay_ms` and doubles up to
    /// `retry_max_delay_ms`; a file still in use after that is tried once
    /// more when everything else is done.
    pub retry_attempts: u32,
    pub retry_delay_ms: u64,
    pub retry_max_delay_ms: u64,
}

/// The folder compare's name, attribute, size and date filters, as far as
/// file operations apply them (see [`FileOpOptions::filter`]). Mirrors what
/// the folder engine's listing leaves out; the "containing text" filters
/// aren't applied (they would read every file).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ItemFilter {
    /// Globs excluded everywhere (files and folders), matched against the
    /// name and the path relative to the operation's root.
    pub exclude: Vec<String>,
    /// When non-empty, only files matching one of these are kept.
    pub include_files: Vec<String>,
    pub exclude_files: Vec<String>,
    pub exclude_folders: Vec<String>,
    pub include_hidden: bool,
    pub ignore_name_case: bool,
    pub min_size: Option<u64>,
    pub max_size: Option<u64>,
    pub modified_after_ms: Option<i64>,
    pub modified_before_ms: Option<i64>,
    /// [`attr`] bits that exclude an entry (hidden and system apply to
    /// folders too).
    pub exclude_attributes: u32,
    pub exclude_executable: bool,
}

/// An [`ItemFilter`] with its globs compiled.
struct CompiledFilter {
    exclude: GlobSet,
    include_files: GlobSet,
    exclude_files: GlobSet,
    exclude_folders: GlobSet,
    include_hidden: bool,
    /// The size and date limits, in the folder engine's form.
    limits: FolderFilters,
}

fn globs(patterns: &[String], case_insensitive: bool) -> Result<GlobSet, String> {
    let mut builder = GlobSetBuilder::new();
    for p in patterns.iter().map(|p| p.trim()).filter(|p| !p.is_empty()) {
        let glob = GlobBuilder::new(p)
            .case_insensitive(case_insensitive)
            .build()
            .map_err(|e| format!("bad filter pattern `{p}`: {e}"))?;
        builder.add(glob);
    }
    builder.build().map_err(|e| e.to_string())
}

impl CompiledFilter {
    fn new(f: &ItemFilter) -> Result<Self, String> {
        let ci = f.ignore_name_case;
        Ok(CompiledFilter {
            exclude: globs(&f.exclude, ci)?,
            include_files: globs(&f.include_files, ci)?,
            exclude_files: globs(&f.exclude_files, ci)?,
            exclude_folders: globs(&f.exclude_folders, ci)?,
            include_hidden: f.include_hidden,
            limits: FolderFilters {
                min_size: f.min_size,
                max_size: f.max_size,
                modified_after_ms: f.modified_after_ms,
                modified_before_ms: f.modified_before_ms,
                exclude_attributes: f.exclude_attributes,
                exclude_executable: f.exclude_executable,
                ..Default::default()
            },
        })
    }

    /// Whether the comparison would leave out `name` (at `rel`, relative to
    /// the operation's root).
    fn hides(&self, name: &str, rel: &str, meta: &EntryMeta) -> bool {
        let attrs = meta.attributes.unwrap_or(0);
        if !self.include_hidden && (name.starts_with('.') || attrs & attr::HIDDEN != 0) {
            return true;
        }
        let hit = |set: &GlobSet| set.is_match(name) || set.is_match(rel);
        if hit(&self.exclude) {
            return true;
        }
        let f = &self.limits;
        if meta.is_dir {
            return hit(&self.exclude_folders) || attrs & f.exclude_attributes & (attr::HIDDEN | attr::SYSTEM) != 0;
        }
        hit(&self.exclude_files)
            || (!self.include_files.is_empty() && !hit(&self.include_files))
            || attrs & f.exclude_attributes != 0
            || (f.exclude_executable && meta.mode.is_some_and(|m| m & 0o111 != 0))
            || !pair_passes(Some(meta), None, f)
    }
}

impl Default for FileOpOptions {
    fn default() -> Self {
        // The same retries as Folder Sync.
        let sync = ExecOptions::default();
        FileOpOptions {
            conflicts: ConflictMode::Ask,
            permanent: false,
            trash_replaced: true,
            copy_acls: false,
            copy_short_names: false,
            new_name: String::new(),
            attributes: AttrChange::default(),
            recursive: false,
            touch: TouchTime::Now,
            filter: None,
            threads: 0,
            retry_attempts: sync.retry_attempts,
            retry_delay_ms: sync.retry_delay_ms,
            retry_max_delay_ms: sync.retry_max_delay_ms,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileOpRequest {
    pub action: FileAction,
    pub items: Vec<FileItem>,
    #[serde(default)]
    pub options: FileOpOptions,
}

// ---------- progress, control and results ----------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum FilePhase {
    /// Waiting for an earlier operation on overlapping paths.
    #[default]
    Queued,
    /// Listing folders to count what there is to do.
    Scanning,
    Running,
    /// Held by the user.
    Paused,
    /// Waiting for an answer to [`FileOpProgress::question`].
    Asking,
    /// Copy and move: trying again the files that were still in use.
    Deferred,
    Finished,
}

/// A name clash the user is asked about.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConflictQuestion {
    /// The destination, as shown to the user.
    pub path: String,
    pub source: EntryMeta,
    pub target: EntryMeta,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileOpProgress {
    pub phase: FilePhase,
    pub files_done: usize,
    pub files_total: usize,
    pub bytes_done: u64,
    pub bytes_total: u64,
    /// The item being worked on (relative path).
    pub current: Option<String>,
    pub failed: usize,
    pub skipped: usize,
    /// Copy and move: files waiting to try again, because another program
    /// holds them or the connection dropped.
    pub retrying: usize,
    /// Copy and move: files left for the end because they were still in use.
    pub deferred: usize,
    pub question: Option<ConflictQuestion>,
    /// Copy and move: items that arrived at the destination since the last
    /// message, relative to the destination root, so a view can show them
    /// while the rest is still on its way.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub landed: Vec<String>,
    /// Items gone from the source root since the last message (moved away
    /// or deleted), relative to the source root.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub removed: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileOpFailure {
    pub path: String,
    pub kind: FailureKind,
    pub message: String,
    /// Processes holding the file, when it was in use (Windows only).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub locked_by: Vec<LockHolder>,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileOpReport {
    /// Files (or, for delete / rename / new folder, items) done.
    pub succeeded: usize,
    pub failed: usize,
    pub skipped: usize,
    pub bytes: u64,
    pub cancelled: bool,
    /// Items inside selected folders left out because the comparison's
    /// filters hide them (see [`FileOpOptions::filter`]).
    pub filtered: usize,
    pub failures: Vec<FileOpFailure>,
    /// Caveats on items that succeeded (e.g. an ACL that couldn't be copied).
    pub notes: Vec<String>,
    pub elapsed_ms: u128,
    /// The backup holding the files this operation replaced (set by the
    /// app when backups are on and something was replaced).
    pub backup: Option<String>,
    /// How many replaced files that backup holds.
    pub backed_up: usize,
}

/// Pause and cancel switches shared between a running operation and the UI.
#[derive(Debug, Default)]
pub struct Control {
    pub cancel: AtomicBool,
    pub pause: AtomicBool,
}

impl Control {
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    pub fn set_paused(&self, paused: bool) {
        self.pause.store(paused, Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
}

// ---------- the queue ----------

/// Serializes operations on overlapping paths: an operation runs once no
/// running operation, and none queued before it, touches a path that is the
/// same as, inside, or above one of its own. Unrelated operations run side
/// by side.
#[derive(Default)]
pub struct OpQueue {
    state: Mutex<QueueState>,
    changed: Condvar,
}

#[derive(Default)]
struct QueueState {
    next: u64,
    waiting: Vec<(u64, Vec<String>)>,
    running: Vec<(u64, Vec<String>)>,
}

/// A place in the running set; dropping it lets queued operations go.
pub struct Slot<'a> {
    queue: &'a OpQueue,
    ticket: u64,
}

impl Drop for Slot<'_> {
    fn drop(&mut self) {
        let mut st = self.queue.lock();
        st.running.retain(|(t, _)| *t != self.ticket);
        drop(st);
        self.queue.changed.notify_all();
    }
}

/// Comparable form of a path: `/` separators, no trailing slash, lower case
/// (over-serializing on a case-sensitive disk is harmless).
fn path_key(path: &str) -> String {
    let p = path.replace('\\', "/");
    p.trim_end_matches('/').to_lowercase()
}

fn overlaps(a: &[String], b: &[String]) -> bool {
    let within =
        |x: &str, y: &str| x.len() > y.len() && x.starts_with(y) && x.as_bytes()[y.len()] == b'/';
    a.iter()
        .any(|x| b.iter().any(|y| x == y || within(x, y) || within(y, x)))
}

impl OpQueue {
    fn lock(&self) -> std::sync::MutexGuard<'_, QueueState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Waits for a turn to work on `paths`. Returns `None` if `cancel` is
    /// set while waiting.
    pub fn acquire(&self, paths: &[String], cancel: &AtomicBool) -> Option<Slot<'_>> {
        let keys: Vec<String> = paths.iter().map(|p| path_key(p)).collect();
        let mut st = self.lock();
        let ticket = st.next;
        st.next += 1;
        st.waiting.push((ticket, keys.clone()));
        loop {
            if cancel.load(Ordering::Relaxed) {
                st.waiting.retain(|(t, _)| *t != ticket);
                drop(st);
                self.changed.notify_all();
                return None;
            }
            let blocked = st.running.iter().any(|(_, p)| overlaps(p, &keys))
                || st
                    .waiting
                    .iter()
                    .take_while(|(t, _)| *t != ticket)
                    .any(|(_, p)| overlaps(p, &keys));
            if !blocked {
                st.waiting.retain(|(t, _)| *t != ticket);
                st.running.push((ticket, keys));
                return Some(Slot {
                    queue: self,
                    ticket,
                });
            }
            st = self
                .changed
                .wait_timeout(st, Duration::from_millis(100))
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }
}

/// Every path a request reads or writes, for [`OpQueue::acquire`]. `src`
/// and `dst` are the roots as the user sees them (local paths or
/// `remote://` URLs).
pub fn lock_paths(req: &FileOpRequest, src: &str, dst: Option<&str>) -> Vec<String> {
    let join = |root: &str, rel: &str| {
        format!(
            "{}/{}",
            root.trim_end_matches(['/', '\\']),
            rel.trim_matches('/')
        )
    };
    let mut out = Vec::new();
    for item in &req.items {
        out.push(join(src, &item.src));
        if let (Some(dst), false) = (dst, item.dst.is_empty()) {
            out.push(join(dst, &item.dst));
        }
        if req.action == FileAction::Rename {
            let parent = item
                .src
                .trim_matches('/')
                .rsplit_once('/')
                .map_or("", |(p, _)| p);
            out.push(join(src, &format!("{parent}/{}", req.options.new_name)));
        }
    }
    out
}

// ---------- places ----------

/// A file or folder on one side: a local path, or a path on a remote.
#[derive(Clone)]
enum Place<'a> {
    Local(PathBuf),
    Remote {
        fs: &'a dyn VfsWrite,
        /// The parent folder and the name, in the remote's own syntax.
        dir: String,
        name: String,
    },
}

fn invalid(msg: impl Into<String>) -> OpError {
    OpError::new(FailureKind::Invalid, msg)
}

/// `rel` under `root`, refusing anything that could escape it. Operation
/// lists round-trip through the webview, so they are never trusted blindly.
fn place<'a>(root: Root<'a>, rel: &str) -> Result<Place<'a>, OpError> {
    match root {
        Root::Local(p) => Ok(Place::Local(fsops::resolve(p, rel)?)),
        Root::Remote { fs, path, .. } => {
            let parts: Vec<&str> = rel.split('/').filter(|p| !p.is_empty()).collect();
            if parts
                .iter()
                .any(|p| *p == "." || *p == ".." || p.contains('\\'))
            {
                return Err(invalid(format!("unsafe relative path `{rel}`")));
            }
            let Some((name, parents)) = parts.split_last() else {
                return Err(invalid("empty relative path"));
            };
            let dir = parents.iter().fold(path.to_owned(), |d, p| fs.join(&d, p));
            Ok(Place::Remote {
                fs,
                dir,
                name: (*name).to_owned(),
            })
        }
    }
}

fn meta_of(m: &fs::Metadata) -> EntryMeta {
    let s = fsops::snap_of(m);
    EntryMeta {
        size: s.size,
        modified_ms: s.modified_ms,
        is_dir: m.is_dir(),
        ..Default::default()
    }
}

fn core_err(e: pcompare_core::CoreError) -> io::Error {
    io::Error::other(e.to_string())
}

impl<'a> Place<'a> {
    fn child(&self, name: &str) -> Place<'a> {
        match self {
            Place::Local(p) => Place::Local(p.join(name)),
            Place::Remote { fs, .. } => Place::Remote {
                fs: *fs,
                dir: self.path(),
                name: name.to_owned(),
            },
        }
    }

    /// The path on this computer, for a local place.
    fn local(&self) -> Option<&Path> {
        match self {
            Place::Local(p) => Some(p),
            Place::Remote { .. } => None,
        }
    }

    /// The full path in the place's own syntax.
    fn path(&self) -> String {
        match self {
            Place::Local(p) => p.to_string_lossy().into_owned(),
            Place::Remote { fs, dir, name } => fs.join(dir, name),
        }
    }

    fn name(&self) -> String {
        match self {
            Place::Local(p) => p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            Place::Remote { name, .. } => name.clone(),
        }
    }

    /// What is there. Local links describe their target (a dangling link
    /// itself), with `link_target` set.
    fn stat(&self) -> io::Result<Option<EntryMeta>> {
        match self {
            Place::Local(p) => {
                let own = match fs::symlink_metadata(p) {
                    Ok(m) => m,
                    Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
                    Err(e) => return Err(e),
                };
                if !own.file_type().is_symlink() {
                    return Ok(Some(meta_of(&own)));
                }
                let mut m = fs::metadata(p).map_or_else(|_| meta_of(&own), |t| meta_of(&t));
                m.link_target = Some(
                    fs::read_link(p)
                        .map(|t| t.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                );
                Ok(Some(m))
            }
            Place::Remote { fs, .. } => fs.stat(&self.path()),
        }
    }

    fn list(&self) -> io::Result<Vec<(String, EntryMeta)>> {
        match self {
            Place::Local(p) => {
                let mut out = Vec::new();
                for entry in fs::read_dir(p)? {
                    let name = entry?.file_name().to_string_lossy().into_owned();
                    if let Some(m) = self.child(&name).stat()? {
                        out.push((name, m));
                    }
                }
                Ok(out)
            }
            Place::Remote { fs, .. } => Ok(fs
                .read_dir(&self.path())
                .map_err(core_err)?
                .into_iter()
                .map(|e| (e.name, e.meta))
                .collect()),
        }
    }

    fn create_dir_all(&self) -> io::Result<()> {
        match self {
            Place::Local(p) => fs::create_dir_all(p),
            Place::Remote { fs, .. } => fs.create_dir_all(&self.path()),
        }
    }

    /// Makes sure the folder holding this place exists.
    fn create_parent(&self) -> io::Result<()> {
        match self {
            Place::Local(p) => p.parent().map_or(Ok(()), fs::create_dir_all),
            Place::Remote { fs, dir, .. } if fs.caps().folders => fs.create_dir_all(dir),
            Place::Remote { .. } => Ok(()),
        }
    }

    /// Whether `self` is `other` or inside it.
    fn within(&self, other: &Place) -> bool {
        match (self, other) {
            (Place::Local(a), Place::Local(b)) => {
                let canon =
                    |p: &Path| fsops::canonical_lenient(p).unwrap_or_else(|_| p.to_path_buf());
                canon(a).starts_with(canon(b))
            }
            (Place::Remote { fs: fa, .. }, Place::Remote { fs: fb, .. })
                if std::ptr::addr_eq(*fa, *fb) =>
            {
                let (a, b) = (path_key(&self.path()), path_key(&other.path()));
                a == b || a.starts_with(&format!("{b}/"))
            }
            _ => false,
        }
    }

    /// Removes a file or link for good (moves, temp files).
    fn remove_file(&self) -> io::Result<()> {
        match self {
            Place::Local(p) => PermanentDeleter.delete(p),
            Place::Remote { fs, .. } => fs.remove_file(&self.path()),
        }
    }

    /// Removes a folder a move has emptied, if it really is empty.
    fn remove_empty_dir(&self) -> io::Result<()> {
        match self {
            Place::Local(p) => fs::remove_dir(p),
            Place::Remote { fs, .. } => {
                let path = self.path();
                if !fs.read_dir(&path).map_err(core_err)?.is_empty() {
                    return Err(io::Error::other("the folder isn't empty"));
                }
                fs.remove_dir_all(&path)
            }
        }
    }

    fn set_modified(&self, ms: i64) -> io::Result<()> {
        match self {
            Place::Local(p) => {
                let t = FileTime::from_unix_time(
                    ms.div_euclid(1000),
                    (ms.rem_euclid(1000) * 1_000_000) as u32,
                );
                filetime::set_file_mtime(p, t)
            }
            Place::Remote { fs, .. } if fs.caps().set_modified => fs.set_modified(&self.path(), ms),
            Place::Remote { .. } => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "this server can't set modification times",
            )),
        }
    }
}

/// A scanned file or folder; folders carry their contents.
struct Node {
    name: String,
    meta: EntryMeta,
    children: Vec<Node>,
    /// Some of the folder's contents (at any depth) were left out by the
    /// filter, so a move must leave the folder itself behind.
    holds_filtered: bool,
}

impl Node {
    fn is_dir(&self) -> bool {
        self.meta.is_dir && self.meta.link_target.is_none()
    }

    /// Files and bytes in the tree.
    fn totals(&self) -> (usize, u64) {
        if self.is_dir() {
            self.children.iter().fold((0, 0), |(f, b), c| {
                let (cf, cb) = c.totals();
                (f + cf, b + cb)
            })
        } else {
            (1, self.meta.size)
        }
    }
}

/// Drops items inside other selected items (a folder and a file in it).
fn outermost(items: &[FileItem]) -> Vec<FileItem> {
    let key = |s: &str| s.trim_matches('/').to_owned();
    let mut out: Vec<FileItem> = Vec::new();
    for item in items {
        let k = key(&item.src);
        let inside = |other: &FileItem| {
            let o = key(&other.src);
            k == o || k.starts_with(&format!("{o}/"))
        };
        if out.iter().any(inside) {
            continue;
        }
        out.retain(|o| {
            let ok = key(&o.src);
            !ok.starts_with(&format!("{k}/"))
        });
        out.push(item.clone());
    }
    out
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

/// Minimum spacing between progress callbacks while bytes are flowing.
const TICK: Duration = Duration::from_millis(100);

enum Decision {
    Overwrite,
    Skip,
    Cancel,
}

struct Runner<'r> {
    opts: &'r FileOpOptions,
    ctl: &'r Control,
    deleter: &'r dyn Deleter,
    progress: &'r mut dyn FnMut(&FileOpProgress),
    ask: &'r mut dyn FnMut(&ConflictQuestion) -> ConflictAnswer,
    conflicts: ConflictMode,
    state: FileOpProgress,
    report: FileOpReport,
    last_tick: Option<Instant>,
    /// Leaves filtered items out of scanned folders (copy and move).
    filter: Option<CompiledFilter>,
    /// What the copy workers are up to right now.
    flow: Arc<Flow>,
    /// Copy and move: the second round, for the files that were in use.
    second_round: bool,
}

/// Counters the copy workers keep up to date for the progress messages.
#[derive(Default)]
struct Flow {
    /// Bytes of the files being moved right now.
    inflight: AtomicU64,
    /// Files waiting to try again.
    retrying: AtomicUsize,
}

impl Runner<'_> {
    fn emit(&mut self) {
        self.last_tick = Some(Instant::now());
        self.state.retrying = self.flow.retrying.load(Ordering::Relaxed);
        let flowing = self.flow.inflight.load(Ordering::Relaxed);
        self.state.bytes_done += flowing;
        (self.progress)(&self.state);
        self.state.bytes_done -= flowing;
        self.state.landed.clear();
        self.state.removed.clear();
    }

    fn emit_throttled(&mut self) {
        if self.last_tick.is_none_or(|t| t.elapsed() >= TICK) {
            self.emit();
        }
    }

    fn cancelled(&self) -> bool {
        self.ctl.is_cancelled()
    }

    /// Waits out a pause; false if the operation was cancelled.
    fn checkpoint(&mut self) -> bool {
        let ctl = self.ctl;
        let mut before = self.state.phase;
        hold(&ctl.pause, &ctl.cancel, |paused| {
            if paused {
                before = self.state.phase;
                self.state.phase = FilePhase::Paused;
            } else {
                self.state.phase = before;
            }
            self.emit();
        })
    }

    fn start(&mut self, current: &str) -> bool {
        if !self.checkpoint() {
            return false;
        }
        self.state.current = Some(current.to_owned());
        self.emit_throttled();
        true
    }

    fn succeed(&mut self, files: usize, bytes: u64) {
        self.report.succeeded += files;
        self.report.bytes += bytes;
        self.state.files_done += files;
        self.state.bytes_done += bytes;
    }

    fn note(&mut self, path: &str, notes: Vec<String>) {
        self.report
            .notes
            .extend(notes.into_iter().map(|n| format!("{path}: {n}")));
    }

    /// Records a failure; a cancellation is not one.
    fn fail(&mut self, path: &str, e: OpError) {
        self.fail_held(path, e, Vec::new());
    }

    /// [`Self::fail`] for a file in use by `locked_by`.
    fn fail_held(&mut self, path: &str, e: OpError, locked_by: Vec<LockHolder>) {
        if e.kind == FailureKind::Cancelled || self.cancelled() {
            return;
        }
        self.report.failed += 1;
        self.state.failed += 1;
        self.report.failures.push(FileOpFailure {
            path: path.to_owned(),
            kind: e.kind,
            message: e.message,
            locked_by,
        });
        self.emit();
    }

    fn skip(&mut self, files: usize) {
        self.report.skipped += files;
        self.state.skipped += files;
        self.state.files_done += files;
    }

    /// Settles a clash between `source` and the existing `target`.
    fn decide(&mut self, path: &str, source: &EntryMeta, target: &EntryMeta) -> Decision {
        loop {
            match self.conflicts {
                ConflictMode::Overwrite => return Decision::Overwrite,
                ConflictMode::Skip => return Decision::Skip,
                ConflictMode::OverwriteOlder => {
                    return match (target.modified_ms, source.modified_ms) {
                        (Some(t), Some(s)) if t < s => Decision::Overwrite,
                        _ => Decision::Skip,
                    }
                }
                ConflictMode::Ask => {}
            }
            let question = ConflictQuestion {
                path: path.to_owned(),
                source: source.clone(),
                target: target.clone(),
            };
            let before = self.state.phase;
            self.state.phase = FilePhase::Asking;
            self.state.question = Some(question.clone());
            self.emit();
            let answer = (self.ask)(&question);
            self.state.phase = before;
            self.state.question = None;
            self.emit();
            match answer {
                ConflictAnswer::Overwrite => return Decision::Overwrite,
                ConflictAnswer::Skip => return Decision::Skip,
                ConflictAnswer::OverwriteAll => self.conflicts = ConflictMode::Overwrite,
                ConflictAnswer::SkipAll => self.conflicts = ConflictMode::Skip,
                ConflictAnswer::OverwriteOlderAll => self.conflicts = ConflictMode::OverwriteOlder,
                ConflictAnswer::Cancel => {
                    self.ctl.cancel();
                    return Decision::Cancel;
                }
            }
        }
    }

    // ----- scanning -----

    fn scan(&mut self, at: &Place, name: String, meta: EntryMeta, shown: &str) -> io::Result<Node> {
        let mut node = Node {
            name,
            meta,
            children: Vec::new(),
            holds_filtered: false,
        };
        if node.is_dir() {
            self.state.current = Some(shown.to_owned());
            self.emit_throttled();
            for (name, meta) in at.list()? {
                if self.cancelled() {
                    break;
                }
                let shown = format!("{shown}/{name}");
                if self.filter.as_ref().is_some_and(|f| f.hides(&name, &shown, &meta)) {
                    self.report.filtered += 1;
                    node.holds_filtered = true;
                    continue;
                }
                let child = at.child(&name);
                let child = self.scan(&child, name, meta, &shown)?;
                node.holds_filtered |= child.holds_filtered;
                node.children.push(child);
            }
        }
        Ok(node)
    }

    // ----- copy and move -----
    //
    // The walk runs here, on the calling thread: it creates folders, settles
    // name clashes (asking the user one at a time) and renames what a move
    // can rename. Every file whose bytes must be copied becomes a [`Job`]
    // for the [`Pool`]'s workers, which run several at once. Folders are
    // finished (times, security, a moved folder's removal) by
    // [`Self::finish`] once all of their files are done.

    /// Plans copying (or moving) `node` from `src` to `dst`; `dst_shown` is
    /// its path under the destination root.
    #[allow(clippy::too_many_arguments)]
    fn plan_node<'a>(
        &mut self,
        pool: &mut Pool<'a>,
        src: &Place<'a>,
        node: &Node,
        dst: &Place<'a>,
        shown: &str,
        dst_shown: &str,
        moving: bool,
    ) -> Pending<'a> {
        if !self.start(shown) {
            return Pending::Settled(false);
        }
        self.collect(pool, false);
        let existing = match dst.stat() {
            Ok(m) => m,
            Err(e) => {
                self.fail(shown, OpError::from(e));
                self.skip_rest(node);
                return Pending::Settled(false);
            }
        };
        if node.is_dir() {
            return self.plan_dir(pool, src, node, dst, shown, dst_shown, moving, existing);
        }
        if let Some(target) = &existing {
            if target.is_dir {
                self.fail(
                    shown,
                    OpError::new(
                        FailureKind::TypeClash,
                        "a folder with this name exists at the destination",
                    ),
                );
                return Pending::Settled(false);
            }
            match self.decide(shown, &node.meta, target) {
                Decision::Overwrite => {}
                Decision::Skip => {
                    self.skip(1);
                    return Pending::Settled(false);
                }
                Decision::Cancel => return Pending::Settled(false),
            }
        }
        if moving && existing.is_none() && self.rename_within(src, dst) {
            self.succeed(1, node.meta.size);
            self.landed(shown, dst_shown, moving);
            return Pending::Settled(true);
        }
        Pending::File(pool.submit(
            Job {
                id: 0,
                src: src.clone(),
                dst: dst.clone(),
                meta: node.meta.clone(),
                replace: existing.is_some(),
                moving,
                copied: None,
                deferred: false,
            },
            JobInfo {
                shown: shown.to_owned(),
                dst_shown: dst_shown.to_owned(),
                size: node.meta.size,
                moving,
            },
        ))
    }

    #[allow(clippy::too_many_arguments)]
    fn plan_dir<'a>(
        &mut self,
        pool: &mut Pool<'a>,
        src: &Place<'a>,
        node: &Node,
        dst: &Place<'a>,
        shown: &str,
        dst_shown: &str,
        moving: bool,
        existing: Option<EntryMeta>,
    ) -> Pending<'a> {
        if existing.as_ref().is_some_and(|m| !m.is_dir) {
            self.fail(
                shown,
                OpError::new(
                    FailureKind::TypeClash,
                    "a file with this name exists at the destination",
                ),
            );
            self.skip_rest(node);
            return Pending::Settled(false);
        }
        // A move into a free spot is a single rename when the volume allows
        // (not when filtered items must stay behind).
        if moving && !node.holds_filtered && existing.is_none() && self.rename_within(src, dst) {
            let (files, bytes) = node.totals();
            self.succeed(files, bytes);
            self.landed(shown, dst_shown, moving);
            return Pending::Settled(true);
        }
        if let Err(e) = dst.create_dir_all() {
            self.fail(shown, OpError::from(e));
            self.skip_rest(node);
            return Pending::Settled(false);
        }
        if existing.is_none() {
            // An empty folder shows up at once; files inside follow.
            self.state.landed.push(dst_shown.to_owned());
        }
        let mut children = Vec::with_capacity(node.children.len());
        for child in &node.children {
            if self.cancelled() {
                break;
            }
            children.push(self.plan_node(
                pool,
                &src.child(&child.name),
                child,
                &dst.child(&child.name),
                &format!("{shown}/{}", child.name),
                &format!("{dst_shown}/{}", child.name),
                moving,
            ));
        }
        Pending::Dir(Box::new(PendingDir {
            src: src.clone(),
            dst: dst.clone(),
            shown: shown.to_owned(),
            modified_ms: node.meta.modified_ms,
            moving,
            holds_filtered: node.holds_filtered,
            children,
        }))
    }

    /// Settles what the workers finished: counts, failures, and the items
    /// that landed. With `wait`, blocks (showing progress) until every
    /// submitted job is done.
    fn collect(&mut self, pool: &mut Pool, wait: bool) {
        loop {
            let next = if wait {
                if pool.outstanding == 0 {
                    break;
                }
                // Show a pause while only the workers are busy.
                let paused = self.ctl.pause.load(Ordering::Relaxed) && !self.cancelled();
                let phase = if paused {
                    FilePhase::Paused
                } else if self.second_round {
                    FilePhase::Deferred
                } else {
                    FilePhase::Running
                };
                if self.state.phase != phase {
                    self.state.phase = phase;
                    self.emit();
                }
                pool.done.recv_timeout(TICK)
            } else {
                pool.done.try_recv().map_err(|e| match e {
                    mpsc::TryRecvError::Empty => RecvTimeoutError::Timeout,
                    mpsc::TryRecvError::Disconnected => RecvTimeoutError::Disconnected,
                })
            };
            match next {
                Ok(Finished {
                    job,
                    outcome,
                    locked_by,
                }) => {
                    pool.outstanding -= 1;
                    let id = job.id;
                    if job.deferred {
                        self.state.deferred -= 1;
                    }
                    match outcome {
                        Err(e) if !self.cancelled() && defers(&job, &e) => {
                            // Kept (with its info) for the second round.
                            self.state.deferred += 1;
                            pool.deferred.push(job);
                        }
                        Ok(notes) => {
                            let info = std::mem::take(&mut pool.info[id]);
                            self.note(&info.shown, notes);
                            self.succeed(1, info.size);
                            self.landed(&info.shown, &info.dst_shown, info.moving);
                            self.state.current = Some(info.shown);
                            pool.results[id] = Some(true);
                        }
                        Err(e) => {
                            let info = std::mem::take(&mut pool.info[id]);
                            if job.copied.is_some() {
                                // A move whose original stays: the copy is there all the same.
                                self.state.landed.push(info.dst_shown);
                            }
                            self.fail_held(&info.shown, e, locked_by);
                            pool.results[id] = Some(false);
                        }
                    }
                }
                Err(RecvTimeoutError::Timeout) if wait => {}
                Err(RecvTimeoutError::Timeout) => break,
                // Every worker is gone: nothing more will arrive.
                Err(RecvTimeoutError::Disconnected) => {
                    pool.outstanding = 0;
                    break;
                }
            }
            self.emit_throttled();
        }
        self.emit_throttled();
    }

    /// An item arrived at `dst_shown` (and, moving, left `shown`).
    fn landed(&mut self, shown: &str, dst_shown: &str, moving: bool) {
        self.state.landed.push(dst_shown.to_owned());
        if moving {
            self.state.removed.push(shown.to_owned());
        }
    }

    /// Finishes a planned item once its jobs are done. Returns true when all
    /// of it arrived — for a move, that the source may be removed.
    fn finish(&mut self, pending: Pending, results: &[Option<bool>]) -> bool {
        let d = match pending {
            Pending::Settled(ok) => return ok,
            Pending::File(id) => return results[id] == Some(true),
            Pending::Dir(d) => *d,
        };
        let mut all = true;
        for child in d.children {
            all &= self.finish(child, results);
        }
        if self.cancelled() {
            return false;
        }
        // Folders keep their own times and (Windows) security, like files.
        if let (Place::Local(s), Place::Local(t)) = (&d.src, &d.dst) {
            let notes = fsops::copy_extras(s, t, self.opts.copy_acls, self.opts.copy_short_names);
            self.note(&d.shown, notes);
        }
        if let Some(ms) = d.modified_ms {
            let _ = d.dst.set_modified(ms);
        }
        if d.moving && d.holds_filtered {
            // The filtered items stay, and so does their folder.
            return false;
        }
        if d.moving && all {
            if let Err(e) = d.src.remove_empty_dir() {
                self.fail(
                    &d.shown,
                    OpError::new(
                        classify(&e),
                        format!("moved its contents, but the folder couldn't be removed: {e}"),
                    ),
                );
                return false;
            }
            self.state.removed.push(d.shown);
        }
        all
    }

    /// Copies (or moves) the scanned items with `threads` workers. With
    /// `after_walk`, they start only once the walk has made every folder at
    /// the destination.
    #[allow(clippy::too_many_arguments)]
    fn copy_all<'a>(
        &mut self,
        mover: Mover,
        threads: usize,
        after_walk: bool,
        items: &[(String, Place<'a>, Option<Place<'a>>, Node)],
        dst_rels: &[String],
        moving: bool,
    ) {
        let (job_tx, job_rx) = mpsc::channel::<Job<'a>>();
        let (done_tx, done_rx) = mpsc::channel::<Finished<'a>>();
        let job_rx = Mutex::new(job_rx);
        let flow = self.flow.clone();
        std::thread::scope(|scope| {
            for _ in 0..threads.max(1) {
                let (jobs, done, flow) = (&job_rx, done_tx.clone(), &*flow);
                scope.spawn(move || mover.serve(jobs, &done, flow));
            }
            drop(done_tx);
            let mut pool = Pool {
                jobs: Some(job_tx),
                held: after_walk.then(Vec::new),
                done: done_rx,
                info: Vec::new(),
                results: Vec::new(),
                outstanding: 0,
                deferred: Vec::new(),
            };
            let mut planned = Vec::new();
            for ((shown, at, to, node), dst_shown) in items.iter().zip(dst_rels) {
                if self.cancelled() {
                    break;
                }
                let to = to.as_ref().expect("copies have a destination");
                if to.within(at) {
                    let why = if at.within(to) {
                        "the source and the destination are the same"
                    } else {
                        "can't copy a folder into itself"
                    };
                    self.fail(shown, invalid(why));
                    continue;
                }
                planned.push(self.plan_node(&mut pool, at, node, to, shown, dst_shown, moving));
            }
            pool.release();
            self.collect(&mut pool, true);
            // A second round for the files that were in use: by now the
            // program holding them has often let go.
            let deferred = std::mem::take(&mut pool.deferred);
            if !deferred.is_empty() && !self.cancelled() {
                self.second_round = true;
                for mut job in deferred {
                    job.deferred = true;
                    pool.resend(job);
                }
                self.collect(&mut pool, true);
                self.second_round = false;
            }
            self.state.deferred = 0;
            // No more jobs: the workers stop once the queue runs dry.
            pool.jobs = None;
            let results = std::mem::take(&mut pool.results);
            for p in planned {
                self.finish(p, &results);
            }
        });
    }

    /// Counts everything in `node` as skipped (its folder couldn't be handled).
    fn skip_rest(&mut self, node: &Node) {
        if node.is_dir() {
            let (files, _) = node.totals();
            self.skip(files);
        }
    }

    /// Moves by renaming when source and destination are on the same file
    /// system. False when that isn't possible (another volume, no rename
    /// support), so the caller copies instead.
    fn rename_within(&self, src: &Place, dst: &Place) -> bool {
        if dst.create_parent().is_err() {
            return false;
        }
        match (src, dst) {
            (Place::Local(a), Place::Local(b)) => {
                fs::symlink_metadata(b).is_err() && fs::rename(a, b).is_ok()
            }
            (Place::Remote { fs: fa, .. }, Place::Remote { fs: fb, .. }) => {
                std::ptr::addr_eq(*fa, *fb)
                    && fa.caps().rename
                    && fa.rename(&src.path(), &dst.path()).is_ok()
            }
            _ => false,
        }
    }

}

/// Byte counter a transfer calls as data flows; an error stops it.
type OnBytes<'b> = &'b mut dyn FnMut(u64) -> io::Result<()>;

/// A file whose bytes a copy worker moves.
struct Job<'a> {
    id: usize,
    src: Place<'a>,
    dst: Place<'a>,
    meta: EntryMeta,
    /// Something is at `dst` already (the clash was settled: overwrite).
    replace: bool,
    /// Remove `src` once the copy is in place.
    moving: bool,
    /// The copy is in place (with these caveats); a move whose original
    /// couldn't be removed yet only has that left to do.
    copied: Option<Vec<String>>,
    /// Back for the second round, after being in use the first time.
    deferred: bool,
}

/// Whether a failed job gets a second round once everything else is done.
/// Files in use are usually held only for a moment (an antivirus scan, a
/// cloud-sync client, a program saving), and by the end it has often let go.
fn defers(job: &Job, e: &OpError) -> bool {
    // TODO(you): Windows often reports a destination open in Excel or Word as
    // AccessDenied (the rename into place), not Locked. Decide whether that
    // also earns a second round; for now this matches Folder Sync.
    !job.deferred && e.kind == FailureKind::Locked
}

/// The programs holding the file a job failed on. When the error doesn't
/// say which of its files that was (or names a temp file), both are asked.
fn holders(e: &OpError, job: &Job) -> Vec<LockHolder> {
    let (src, dst) = (job.src.local(), job.dst.local());
    let asked: Vec<&Path> = match e.path.as_deref() {
        Some(p) if Some(p) == src || Some(p) == dst => vec![p],
        _ => src.into_iter().chain(dst).collect(),
    };
    let mut out: Vec<LockHolder> = Vec::new();
    for h in asked.into_iter().flat_map(lock_holders) {
        if !out.contains(&h) {
            out.push(h);
        }
    }
    out
}

/// What the coordinator keeps about a submitted [`Job`].
#[derive(Default)]
struct JobInfo {
    shown: String,
    dst_shown: String,
    size: u64,
    moving: bool,
}

/// A job back from a worker, done or failed.
struct Finished<'a> {
    job: Job<'a>,
    /// The copy's caveats, or why it failed.
    outcome: Result<Vec<String>, OpError>,
    /// Who holds the file, when it failed for good because it is in use.
    locked_by: Vec<LockHolder>,
}

/// The copy workers' queue, as the coordinating thread sees it.
struct Pool<'a> {
    /// `None` once everything is submitted, which lets the workers finish.
    jobs: Option<mpsc::Sender<Job<'a>>>,
    /// Jobs kept from the workers until the walk is done, when it must not
    /// overlap them (see [`read_out_first`]).
    held: Option<Vec<Job<'a>>>,
    done: mpsc::Receiver<Finished<'a>>,
    info: Vec<JobInfo>,
    /// Per job: `None` while it runs, then whether it succeeded.
    results: Vec<Option<bool>>,
    outstanding: usize,
    /// Jobs whose files were in use, waiting for the second round.
    deferred: Vec<Job<'a>>,
}

impl<'a> Pool<'a> {
    fn submit(&mut self, mut job: Job<'a>, info: JobInfo) -> usize {
        let id = self.info.len();
        job.id = id;
        self.info.push(info);
        self.results.push(None);
        self.outstanding += 1;
        match &mut self.held {
            Some(held) => held.push(job),
            None => self.send(job),
        }
        id
    }

    /// Hands `job` to the workers; it fails at once if they are gone.
    fn send(&mut self, job: Job<'a>) {
        let id = job.id;
        if !self.jobs.as_ref().is_some_and(|tx| tx.send(job).is_ok()) {
            self.results[id] = Some(false);
            self.outstanding -= 1;
        }
    }

    /// Hands a deferred job (already submitted once) to the workers again.
    fn resend(&mut self, job: Job<'a>) {
        self.outstanding += 1;
        self.send(job);
    }

    /// Lets the workers start on the jobs held during the walk.
    fn release(&mut self) {
        for job in self.held.take().unwrap_or_default() {
            self.send(job);
        }
    }
}

/// A copy or move as the walk planned it, finished once its jobs are done.
enum Pending<'a> {
    /// Decided during the walk (skipped, failed, or renamed in one go).
    Settled(bool),
    /// A [`Job`]'s id.
    File(usize),
    Dir(Box<PendingDir<'a>>),
}

struct PendingDir<'a> {
    src: Place<'a>,
    dst: Place<'a>,
    shown: String,
    modified_ms: Option<i64>,
    moving: bool,
    holds_filtered: bool,
    children: Vec<Pending<'a>>,
}

/// The most files a copy transfers at once.
pub const MAX_COPY_THREADS: usize = 32;

/// How many files a copy between these sides transfers at once when
/// [`FileOpOptions::threads`] is 0 (automatic).
pub fn copy_threads(src_remote: bool, dst_remote: bool) -> usize {
    if src_remote || dst_remote {
        // Latency-bound: a few requests in flight hide the round trips, and
        // FTP / SFTP servers often cap connections per user.
        return 4;
    }
    // Local disks: enough to overlap the per-file overhead (open, create,
    // flush, rename, metadata) that dominates small files, without
    // thrashing a spinning disk on big ones.
    std::thread::available_parallelism().map_or(4, |n| n.get().clamp(2, 8))
}

/// How many workers a copy (or move) from `src` to `dst` uses: what
/// `threads` asks for (0: [`copy_threads`]), but one alone when a side it
/// changes takes one change at a time — the destination, or the source of
/// a move, which removes each file once copied. That's a zip archive (see
/// [`pcompare_core::vfs_write::WriteCaps::serial_writes`]).
fn copy_workers(threads: usize, src: &Root, dst: Option<&Root>, moving: bool) -> usize {
    let serial = |root: &Root| matches!(root, Root::Remote { fs, .. } if fs.caps().serial_writes);
    if dst.is_some_and(serial) || (moving && serial(src)) {
        return 1;
    }
    match threads {
        0 => copy_threads(src.is_remote(), dst.is_some_and(Root::is_remote)),
        n => n.min(MAX_COPY_THREADS),
    }
}

/// Whether a copy from `src` into `dst` must never read the source while
/// the destination changes. Every change to an archive replaces its file,
/// which Windows refuses while a reader has it open, and a source in an
/// archive may be that very file: the same instance, or the same file
/// opened under another spelling of its path, which no pointer comparison
/// would catch. So each file is read out to a local temp file before it is
/// written, and the copying starts only once the walk has made the folders.
fn read_out_first(src: &dyn VfsWrite, dst: &dyn VfsWrite) -> bool {
    dst.caps().serial_writes && src.caps().serial_writes
}

/// The byte-moving half of a copy: everything a worker thread needs, and
/// nothing the threads could race on.
#[derive(Clone, Copy)]
struct Mover<'r> {
    opts: &'r FileOpOptions,
    ctl: &'r Control,
    deleter: &'r dyn Deleter,
}

impl<'a> Mover<'_> {
    /// A worker: takes jobs until the queue closes.
    fn serve(&self, jobs: &Mutex<mpsc::Receiver<Job<'a>>>, done: &mpsc::Sender<Finished<'a>>, flow: &Flow) {
        loop {
            let job = {
                let rx = jobs.lock().unwrap_or_else(|e| e.into_inner());
                rx.recv()
            };
            let Ok(mut job) = job else { return };
            let outcome = if self.ctl.is_cancelled() {
                Err(OpError::new(FailureKind::Cancelled, "cancelled"))
            } else {
                // A panic must not leave the coordinator waiting for this job.
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.work(&mut job, flow)))
                    .unwrap_or_else(|_| Err(OpError::new(FailureKind::Other, "the copy crashed")))
            };
            // Asked here, off the coordinator's thread, and only once the
            // failure is final: the lookup takes a moment per file.
            let locked_by = match &outcome {
                Err(e) if matches!(e.kind, FailureKind::Locked | FailureKind::AccessDenied) && !defers(&job, e) => {
                    holders(e, &job)
                }
                _ => Vec::new(),
            };
            let finished = Finished {
                job,
                outcome,
                locked_by,
            };
            if done.send(finished).is_err() {
                return;
            }
        }
    }

    /// Copies one file, then removes the original for a move, each step
    /// retried on its own: a retry never copies again what is in place.
    fn work(&self, job: &mut Job, flow: &Flow) -> Result<Vec<String>, OpError> {
        if job.copied.is_none() {
            let notes = self.retrying(flow, |again| self.copy_once(job, again, flow))?;
            job.copied = Some(notes);
        }
        if job.moving {
            let src = &job.src;
            self.retrying(flow, |_| {
                src.remove_file().map_err(|e| OpError {
                    path: src.local().map(Path::to_path_buf),
                    ..OpError::new(
                        classify(&e),
                        format!("copied, but the original couldn't be removed: {e}"),
                    )
                })
            })?;
        }
        Ok(job.copied.take().unwrap_or_default())
    }

    /// Runs `step` until it succeeds, fails for good, or runs out of
    /// retries; `step(true)` is a retry. Waits back off like Folder Sync's.
    fn retrying<T>(&self, flow: &Flow, mut step: impl FnMut(bool) -> Result<T, OpError>) -> Result<T, OpError> {
        let mut retry = 0;
        loop {
            match step(retry > 0) {
                Err(e) if is_transient(e.kind) && retry < self.opts.retry_attempts && !self.ctl.is_cancelled() => {
                    retry += 1;
                    let wait = backoff_delay(self.opts.retry_delay_ms, self.opts.retry_max_delay_ms, retry);
                    flow.retrying.fetch_add(1, Ordering::Relaxed);
                    let go_on = exec::pause(wait, &self.ctl.cancel);
                    flow.retrying.fetch_sub(1, Ordering::Relaxed);
                    if !go_on {
                        return Err(OpError::new(FailureKind::Cancelled, "cancelled"));
                    }
                }
                done => return done,
            }
        }
    }

    /// One attempt at copying a job's file, counting the bytes in flight in
    /// `flow` while it runs.
    fn copy_once(&self, job: &Job, again: bool, flow: &Flow) -> Result<Vec<String>, OpError> {
        let ctl = self.ctl;
        if !hold(&ctl.pause, &ctl.cancel, |_| {}) {
            return Err(OpError::new(FailureKind::Cancelled, "cancelled"));
        }
        // An earlier attempt may have sent the old version to the Trash
        // before it failed; there is nothing left to replace then.
        let replace = job.replace && (!again || job.dst.stat().map_or(true, |m| m.is_some()));
        let mut sent = 0u64;
        let result = {
            let mut on_bytes = |n: u64| {
                if !hold(&ctl.pause, &ctl.cancel, |_| {}) {
                    return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
                }
                sent += n;
                flow.inflight.fetch_add(n, Ordering::Relaxed);
                Ok(())
            };
            self.transfer(&job.src, &job.meta, &job.dst, replace, &mut on_bytes)
        };
        flow.inflight.fetch_sub(sent, Ordering::Relaxed);
        result
    }

    /// Copies one file's data (and times) from `src` to `dst`, replacing
    /// what is there if `replace`. Returns caveats worth showing.
    fn transfer(
        &self,
        src: &Place,
        meta: &EntryMeta,
        dst: &Place,
        replace: bool,
        on_bytes: OnBytes,
    ) -> Result<Vec<String>, OpError> {
        dst.create_parent().map_err(OpError::from)?;
        match (src, dst) {
            (Place::Local(s), Place::Local(d)) => {
                if let Some(target) = &meta.link_target {
                    return self.copy_link(s, d, target, replace);
                }
                let temp = fsops::copy_to_temp(s, d, false, on_bytes)?;
                self.install_local(d, temp.tmp, replace)?;
                Ok(fsops::copy_extras(
                    s,
                    d,
                    self.opts.copy_acls,
                    self.opts.copy_short_names,
                ))
            }
            (Place::Remote { fs, .. }, Place::Local(d)) => {
                let tmp = fsops::temp_sibling(d);
                let fetched = (|| {
                    let mut input = fs.open_at(&src.path(), 0)?;
                    let mut out = File::create_new(&tmp).map_err(|e| OpError::io(&tmp, e))?;
                    let n = pump(&mut input, &mut out, on_bytes)?;
                    out.sync_all().map_err(|e| OpError::io(&tmp, e))?;
                    drop(out);
                    check_size(n, meta.size)?;
                    if let Some(ms) = meta.modified_ms {
                        Place::Local(tmp.clone())
                            .set_modified(ms)
                            .map_err(|e| OpError::io(&tmp, e))?;
                    }
                    Ok::<(), OpError>(())
                })();
                if let Err(e) = fetched {
                    let _ = fs::remove_file(&tmp);
                    return Err(e);
                }
                self.install_local(d, tmp, replace)?;
                Ok(Vec::new())
            }
            (_, Place::Remote { fs, dir, name }) => {
                let mut input: Box<dyn Read> = match src {
                    Place::Local(s) => Box::new(File::open(s).map_err(|e| OpError::io(s, e))?),
                    Place::Remote { fs: sfs, .. } if read_out_first(*sfs, *fs) => {
                        // Nothing may hold the source archive open while the
                        // write swaps the rewritten one in: it may be the
                        // same file (two folders of one zip), and Windows
                        // refuses to replace an open file.
                        let mut copy = tempfile::tempfile()?;
                        let mut entry = sfs.open_at(&src.path(), 0)?;
                        pump(&mut entry, &mut copy, &mut |_| on_bytes(0))?;
                        drop(entry);
                        copy.rewind()?;
                        Box::new(copy)
                    }
                    Place::Remote { fs: sfs, .. } => sfs.open_at(&src.path(), 0)?,
                };
                self.upload(*fs, dir, name, &mut input, meta, replace, on_bytes)
            }
        }
    }

    /// Recreates a symbolic link (local to local).
    fn copy_link(
        &self,
        src: &Path,
        dst: &Path,
        target: &str,
        replace: bool,
    ) -> Result<Vec<String>, OpError> {
        if replace {
            self.dispose_local(dst)?;
        }
        #[cfg(unix)]
        let made = std::os::unix::fs::symlink(target, dst);
        #[cfg(windows)]
        let made = if fs::metadata(src).is_ok_and(|m| m.is_dir()) {
            std::os::windows::fs::symlink_dir(target, dst)
        } else {
            std::os::windows::fs::symlink_file(target, dst)
        };
        #[cfg(not(any(unix, windows)))]
        let made: io::Result<()> = Err(io::Error::from(io::ErrorKind::Unsupported));
        let _ = src;
        made.map_err(|e| OpError::io(dst, e))?;
        Ok(Vec::new())
    }

    /// Gets a replaced local file out of the way: to the Recycle Bin / Trash
    /// or the backups (the operation's deleter), or for good. Returns where
    /// the old version was kept, when the deleter knows.
    fn dispose_local(&self, path: &Path) -> Result<Option<PathBuf>, OpError> {
        let r = if self.opts.trash_replaced {
            self.deleter.dispose_replaced(path)
        } else {
            PermanentDeleter.delete(path).map(|()| None)
        };
        r.map_err(|e| OpError::io(path, e))
    }

    /// Puts the finished temp copy `tmp` in place of `dst`.
    fn install_local(&self, dst: &Path, tmp: PathBuf, replace: bool) -> Result<(), OpError> {
        if !replace {
            return fs::rename(&tmp, dst).map_err(|e| {
                let _ = fs::remove_file(&tmp);
                OpError::io(dst, e)
            });
        }
        if self.opts.trash_replaced {
            // The old version goes to the Trash (or the backups) under its own name.
            let kept = match self.dispose_local(dst) {
                Ok(kept) => kept,
                Err(e) => {
                    let _ = fs::remove_file(&tmp);
                    return Err(e);
                }
            };
            return fs::rename(&tmp, dst).map_err(|e| {
                let _ = fs::remove_file(&tmp);
                let whereabouts = match &kept {
                    Some(p) => format!("the previous version was kept as {}", p.display()),
                    None => "the previous version is in the Recycle Bin / Trash".to_owned(),
                };
                OpError {
                    path: Some(dst.to_path_buf()),
                    ..OpError::new(classify(&e), format!("{e}; {whereabouts}"))
                }
            });
        }
        // Overwrite for good, but keep the old file one rename away until
        // the new one is in place.
        let staged = match fsops::stage_aside(dst) {
            Ok(p) => p,
            Err(e) => {
                let _ = fs::remove_file(&tmp);
                return Err(OpError::io(dst, e));
            }
        };
        if let Err(e) = fs::rename(&tmp, dst) {
            let _ = fs::remove_file(&tmp);
            let _ = fs::rename(&staged, dst);
            return Err(OpError::io(dst, e));
        }
        let _ = PermanentDeleter.delete(&staged);
        Ok(())
    }

    /// Writes `input` to `name` in `dir` on a remote: through a temp name
    /// swapped into place where the server can rename, directly otherwise.
    #[allow(clippy::too_many_arguments)]
    fn upload(
        &self,
        fs: &dyn VfsWrite,
        dir: &str,
        name: &str,
        input: &mut dyn Read,
        meta: &EntryMeta,
        replace: bool,
        on_bytes: OnBytes,
    ) -> Result<Vec<String>, OpError> {
        let caps = fs.caps();
        let dst = fs.join(dir, name);
        let mut notes = Vec::new();
        let stamp = |path: &str, notes: &mut Vec<String>| {
            if let Some(ms) = meta.modified_ms.filter(|_| caps.set_modified) {
                if fs.set_modified(path, ms).is_err() {
                    notes.push("the server didn't accept the file's modification time".to_owned());
                }
            }
        };
        if !caps.rename {
            let n = fs.write(&dst, 0, input, on_bytes)?;
            check_size(n, meta.size)?;
            stamp(&dst, &mut notes);
            return Ok(notes);
        }
        let n = fsops::TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let temp = |ext: &str| {
            fs.join(
                dir,
                &format!(".{name}{}{}-{n}.{ext}", fsops::MARKER, std::process::id()),
            )
        };
        let tmp = temp("tmp");
        let sent = fs
            .write(&tmp, 0, input, on_bytes)
            .map_err(OpError::from)
            .and_then(|n| check_size(n, meta.size));
        if let Err(e) = sent {
            let _ = fs.remove_file(&tmp);
            return Err(e);
        }
        stamp(&tmp, &mut notes);
        if !replace {
            return match fs.rename(&tmp, &dst) {
                Ok(()) => Ok(notes),
                Err(e) => {
                    let _ = fs.remove_file(&tmp);
                    Err(e.into())
                }
            };
        }
        // Old version aside, new one in, old one gone: no Recycle Bin remotely.
        let old = temp("old");
        if let Err(e) = fs.rename(&dst, &old) {
            let _ = fs.remove_file(&tmp);
            return Err(e.into());
        }
        if let Err(e) = fs.rename(&tmp, &dst) {
            let _ = fs.remove_file(&tmp);
            let _ = fs.rename(&old, &dst);
            return Err(e.into());
        }
        if let Err(e) = fs.remove_file(&old) {
            notes.push(format!(
                "the replaced version couldn't be removed ({e}); it was kept as {old}"
            ));
        }
        Ok(notes)
    }

}

impl Runner<'_> {
    // ----- the other actions -----

    fn delete(&mut self, at: &Place, shown: &str) {
        if !self.start(shown) {
            return;
        }
        let result = match at {
            Place::Local(p) => match fs::symlink_metadata(p) {
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
                _ if self.opts.permanent => PermanentDeleter.delete(p),
                _ => self.deleter.delete(p),
            },
            // No Recycle Bin on a remote: this is final.
            Place::Remote { fs, .. } => match fs.stat(&at.path()) {
                Ok(Some(m)) if m.is_dir => fs.remove_dir_all(&at.path()),
                Ok(Some(_)) => fs.remove_file(&at.path()),
                Ok(None) => Ok(()),
                Err(e) => Err(e),
            },
        };
        match result {
            Ok(()) => {
                self.succeed(1, 0);
                self.state.removed.push(shown.to_owned());
            }
            Err(e) => self.fail(shown, OpError::from(e)),
        }
    }

    fn rename(&mut self, at: &Place, shown: &str) {
        if !self.start(shown) {
            return;
        }
        let new_name = self.opts.new_name.trim();
        let bad = new_name.is_empty()
            || new_name.contains(['/', '\\'])
            || new_name == "."
            || new_name == "..";
        if bad {
            return self.fail(shown, invalid(format!("`{new_name}` isn't a valid name")));
        }
        let result = match at {
            Place::Local(p) => {
                let to = p.with_file_name(new_name);
                let case_only = p
                    .file_name()
                    .is_some_and(|n| n.to_string_lossy().to_lowercase() == new_name.to_lowercase());
                if fs::symlink_metadata(&to).is_ok() && !case_only {
                    Err(OpError::new(
                        FailureKind::Invalid,
                        format!("`{new_name}` already exists"),
                    ))
                } else {
                    fs::rename(p, &to).map_err(|e| OpError::io(p, e))
                }
            }
            Place::Remote { fs, dir, .. } => {
                let to = fs.join(dir, new_name);
                if !fs.caps().rename {
                    Err(OpError::new(
                        FailureKind::Invalid,
                        "this server can't rename files",
                    ))
                } else {
                    match fs.stat(&to) {
                        Ok(Some(_)) => Err(OpError::new(
                            FailureKind::Invalid,
                            format!("`{new_name}` already exists"),
                        )),
                        Ok(None) => fs.rename(&at.path(), &to).map_err(OpError::from),
                        Err(e) => Err(e.into()),
                    }
                }
            }
        };
        match result {
            Ok(()) => self.succeed(1, 0),
            Err(e) => self.fail(shown, e),
        }
    }

    fn new_folder(&mut self, at: &Place, shown: &str) {
        if !self.start(shown) {
            return;
        }
        let result = match at.stat() {
            Ok(Some(_)) => Err(OpError::new(
                FailureKind::Invalid,
                format!("`{}` already exists", at.name()),
            )),
            Ok(None) => at.create_dir_all().map_err(OpError::from),
            Err(e) => Err(e.into()),
        };
        match result {
            Ok(()) => self.succeed(1, 0),
            Err(e) => self.fail(shown, e),
        }
    }

    /// Applies attribute changes to `node` (and its contents if recursive).
    fn attributes(&mut self, at: &Place, node: &Node, shown: &str) {
        if !self.start(shown) {
            return;
        }
        let result = match at {
            Place::Local(p) => {
                fsops::set_attributes(p, &self.opts.attributes).map_err(|e| OpError::io(p, e))
            }
            Place::Remote { .. } => Err(OpError::new(
                FailureKind::Invalid,
                "attributes can't be changed on remote folders",
            )),
        };
        match result {
            Ok(()) => self.succeed(1, 0),
            Err(e) => return self.fail(shown, e),
        }
        for child in &node.children {
            if self.cancelled() {
                return;
            }
            self.attributes(
                &at.child(&child.name),
                child,
                &format!("{shown}/{}", child.name),
            );
        }
    }

    /// Sets `node`'s modification time (and its contents' if recursive).
    /// `other` is the counterpart on the other side, for [`TouchTime::OtherSide`].
    fn touch(&mut self, at: &Place, node: &Node, other: Option<&Place>, now: i64, shown: &str) {
        if !self.start(shown) {
            return;
        }
        let time = match self.opts.touch {
            TouchTime::Now => Ok(Some(now)),
            TouchTime::At { ms } => Ok(Some(ms)),
            TouchTime::OtherSide => match other.map(|o| o.stat()) {
                Some(Ok(Some(m))) => Ok(m.modified_ms),
                Some(Ok(None)) | None => Ok(None),
                Some(Err(e)) => Err(OpError::from(e)),
            },
        };
        match time {
            Ok(Some(ms)) => match at.set_modified(ms) {
                Ok(()) => self.succeed(1, 0),
                Err(e) => self.fail(shown, OpError::from(e)),
            },
            // Nothing on the other side to take a time from.
            Ok(None) => self.skip(1),
            Err(e) => self.fail(shown, e),
        }
        for child in &node.children {
            if self.cancelled() {
                return;
            }
            let o = other.map(|o| o.child(&child.name));
            self.touch(
                &at.child(&child.name),
                child,
                o.as_ref(),
                now,
                &format!("{shown}/{}", child.name),
            );
        }
    }
}

fn check_size(got: u64, want: u64) -> Result<(), OpError> {
    if got == want {
        Ok(())
    } else {
        Err(OpError::new(
            FailureKind::VerifyFailed,
            format!(
                "{got} of {want} bytes arrived; the source may have changed while it was copied"
            ),
        ))
    }
}

// ---------- running a request ----------

/// Runs `req` from the `src` root (to the `dst` root for copy and move, the
/// other side for touch "to other side").
///
/// `deleter` disposes of local items deleted or replaced (the app passes the
/// Recycle Bin / Trash; [`FileOpOptions::permanent`] bypasses it for
/// deletes). `progress` is called as work proceeds and once at the end
/// (`phase == Finished`); `ask` answers name clashes under
/// [`ConflictMode::Ask`] and may block until the user decides. Setting
/// `ctl.pause` holds the operation between files and inside large copies;
/// `ctl.cancel` stops it at the next safe point, abandoning a copy in flight
/// and removing its temp file.
pub fn run(
    req: &FileOpRequest,
    src: Root,
    dst: Option<Root>,
    deleter: &dyn Deleter,
    ctl: &Control,
    progress: &mut dyn FnMut(&FileOpProgress),
    ask: &mut dyn FnMut(&ConflictQuestion) -> ConflictAnswer,
) -> FileOpReport {
    let started = Instant::now();
    let mut r = Runner {
        opts: &req.options,
        ctl,
        deleter,
        progress,
        ask,
        conflicts: req.options.conflicts,
        state: FileOpProgress {
            phase: FilePhase::Scanning,
            ..Default::default()
        },
        report: FileOpReport::default(),
        last_tick: None,
        filter: None,
        flow: Arc::new(Flow::default()),
        second_round: false,
    };
    if matches!(req.action, FileAction::Copy | FileAction::Move) {
        if let Some(f) = &req.options.filter {
            match CompiledFilter::new(f) {
                Ok(f) => r.filter = Some(f),
                Err(e) => {
                    r.fail("", invalid(&e));
                    r.state.phase = FilePhase::Finished;
                    r.emit();
                    return std::mem::take(&mut r.report);
                }
            }
        }
    }
    r.emit();
    let items = match req.action {
        FileAction::Rename | FileAction::NewFolder => req.items.iter().take(1).cloned().collect(),
        _ => outermost(&req.items),
    };

    // Resolve every item first, so a bad path fails alone and up front.
    let mut work: Vec<(String, Place, Option<Place>)> = Vec::new();
    let mut dst_rels: Vec<String> = Vec::new();
    for item in &items {
        let shown = item.src.trim_matches('/').to_owned();
        let at = match place(src, &item.src) {
            Ok(p) => p,
            Err(e) => {
                r.fail(&shown, e);
                continue;
            }
        };
        let needs_dst = matches!(req.action, FileAction::Copy | FileAction::Move)
            || (req.action == FileAction::Touch && req.options.touch == TouchTime::OtherSide);
        let to = match (needs_dst, dst) {
            (false, _) => None,
            (true, Some(root)) if !item.dst.is_empty() => match place(root, &item.dst) {
                Ok(p) => Some(p),
                Err(e) => {
                    r.fail(&shown, e);
                    continue;
                }
            },
            // Touch "to other side" just skips items missing over there.
            (true, _) if req.action == FileAction::Touch => None,
            (true, _) => {
                r.fail(&shown, invalid("no destination"));
                continue;
            }
        };
        dst_rels.push(item.dst.trim_matches('/').to_owned());
        work.push((shown, at, to));
    }

    match req.action {
        FileAction::Delete => {
            r.state.files_total = work.len();
            r.state.phase = FilePhase::Running;
            for (shown, at, _) in &work {
                if r.cancelled() {
                    break;
                }
                r.delete(at, shown);
            }
        }
        FileAction::Rename | FileAction::NewFolder => {
            r.state.files_total = work.len();
            r.state.phase = FilePhase::Running;
            for (shown, at, _) in &work {
                if req.action == FileAction::Rename {
                    r.rename(at, shown);
                } else {
                    r.new_folder(at, shown);
                }
            }
        }
        FileAction::Copy | FileAction::Move | FileAction::Attributes | FileAction::Touch => {
            let deep =
                matches!(req.action, FileAction::Copy | FileAction::Move) || req.options.recursive;
            let mut scanned = Vec::new();
            let mut scanned_dst = Vec::new();
            for ((shown, at, to), dst_rel) in work.into_iter().zip(dst_rels) {
                if r.cancelled() {
                    break;
                }
                let node = match at.stat() {
                    Ok(Some(meta)) if deep => r.scan(&at, at.name(), meta, &shown),
                    Ok(Some(meta)) => Ok(Node {
                        name: at.name(),
                        meta,
                        children: Vec::new(),
                        holds_filtered: false,
                    }),
                    Ok(None) => Err(io::Error::new(
                        io::ErrorKind::NotFound,
                        "it no longer exists",
                    )),
                    Err(e) => Err(e),
                };
                match node {
                    Ok(node) => {
                        scanned.push((shown, at, to, node));
                        scanned_dst.push(dst_rel);
                    }
                    Err(e) => r.fail(&shown, OpError::from(e)),
                }
            }
            let (files, bytes) = match req.action {
                FileAction::Copy | FileAction::Move => {
                    scanned.iter().fold((0, 0), |(f, b), (.., n)| {
                        let (nf, nb) = n.totals();
                        (f + nf, b + nb)
                    })
                }
                _ => (scanned.iter().map(|(.., n)| count_nodes(n)).sum(), 0),
            };
            r.state.files_total = files;
            r.state.bytes_total = bytes;
            r.state.phase = FilePhase::Running;
            r.emit();
            if matches!(req.action, FileAction::Copy | FileAction::Move) {
                let moving = req.action == FileAction::Move;
                let threads = copy_workers(req.options.threads, &src, dst.as_ref(), moving);
                let after_walk = matches!(
                    (&src, &dst),
                    (Root::Remote { fs: s, .. }, Some(Root::Remote { fs: d, .. }))
                        if read_out_first(*s, *d)
                );
                let mover = Mover {
                    opts: &req.options,
                    ctl,
                    deleter,
                };
                r.copy_all(mover, threads, after_walk, &scanned, &scanned_dst, moving);
            } else {
                let now = now_ms();
                for (shown, at, to, node) in &scanned {
                    if r.cancelled() {
                        break;
                    }
                    match req.action {
                        FileAction::Attributes => r.attributes(at, node, shown),
                        _ => r.touch(at, node, to.as_ref(), now, shown),
                    }
                }
            }
        }
    }

    let mut report = std::mem::take(&mut r.report);
    report.cancelled = r.cancelled();
    report.elapsed_ms = started.elapsed().as_millis();
    r.state.phase = FilePhase::Finished;
    r.state.current = None;
    r.emit();
    report
}

/// Items in a tree (the folder itself included).
fn count_nodes(n: &Node) -> usize {
    1 + n.children.iter().map(count_nodes).sum::<usize>()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pcompare_core::vfs::{LocalFs, Vfs, VfsEntry};
    use pcompare_core::vfs_write::WriteCaps;
    use std::sync::atomic::AtomicUsize;
    use std::sync::Arc;

    const OLD: i64 = 1_600_000_000;
    const NEW: i64 = 1_700_000_000;

    fn write(root: &Path, rel: &str, body: &str, secs: i64) {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, body).unwrap();
        filetime::set_file_mtime(&p, FileTime::from_unix_time(secs, 0)).unwrap();
    }

    fn read(root: &Path, rel: &str) -> String {
        fs::read_to_string(root.join(rel)).unwrap()
    }

    fn mtime(root: &Path, rel: &str) -> i64 {
        FileTime::from_last_modification_time(&fs::metadata(root.join(rel)).unwrap()).unix_seconds()
    }

    /// Every file under `root` (temp files would show up here).
    fn files(root: &Path) -> Vec<String> {
        let mut out = Vec::new();
        let mut dirs = vec![root.to_path_buf()];
        while let Some(d) = dirs.pop() {
            for e in fs::read_dir(&d).unwrap().flatten() {
                if e.path().is_dir() {
                    dirs.push(e.path());
                } else {
                    out.push(
                        e.path()
                            .strip_prefix(root)
                            .unwrap()
                            .to_string_lossy()
                            .replace('\\', "/"),
                    );
                }
            }
        }
        out.sort();
        out
    }

    struct Pair {
        _dir: tempfile::TempDir,
        left: PathBuf,
        right: PathBuf,
    }

    /// left: a.txt (new), docs/b.txt, docs/sub/c.txt; right: a.txt (old)
    fn pair() -> Pair {
        let dir = tempfile::tempdir().unwrap();
        let (left, right) = (dir.path().join("L"), dir.path().join("R"));
        write(&left, "a.txt", "left a", NEW);
        write(&left, "docs/b.txt", "bb", OLD);
        write(&left, "docs/sub/c.txt", "ccc", OLD);
        write(&right, "a.txt", "right a (old)", OLD);
        Pair {
            _dir: dir,
            left,
            right,
        }
    }

    fn items(rels: &[&str]) -> Vec<FileItem> {
        rels.iter()
            .map(|r| FileItem {
                src: (*r).into(),
                dst: (*r).into(),
            })
            .collect()
    }

    fn req(action: FileAction, rels: &[&str], options: FileOpOptions) -> FileOpRequest {
        FileOpRequest {
            action,
            items: items(rels),
            options,
        }
    }

    fn opts(conflicts: ConflictMode) -> FileOpOptions {
        FileOpOptions {
            conflicts,
            ..Default::default()
        }
    }

    fn go(req: &FileOpRequest, src: &Path, dst: Option<&Path>) -> FileOpReport {
        run(
            req,
            Root::Local(src),
            dst.map(Root::Local),
            &PermanentDeleter,
            &Control::default(),
            &mut |_| {},
            &mut |_| panic!("not expected to ask"),
        )
    }

    /// left: proj/{main.rs, notes.tmp, .env, build/out.o, src/lib.rs, src/old.tmp}
    fn filtered_tree() -> Pair {
        let dir = tempfile::tempdir().unwrap();
        let (left, right) = (dir.path().join("L"), dir.path().join("R"));
        for rel in ["main.rs", "notes.tmp", ".env", "build/out.o", "src/lib.rs", "src/old.tmp"] {
            write(&left, &format!("proj/{rel}"), rel, OLD);
        }
        fs::create_dir_all(&right).unwrap();
        Pair { _dir: dir, left, right }
    }

    /// The folder compare's filters: no *.tmp, no build folders, no hidden files.
    fn compare_filter() -> FileOpOptions {
        FileOpOptions {
            conflicts: ConflictMode::Overwrite,
            filter: Some(ItemFilter {
                exclude: vec!["*.tmp".into()],
                exclude_folders: vec!["build".into()],
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn copying_a_folder_skips_filtered_items() {
        let p = filtered_tree();
        let r = go(&req(FileAction::Copy, &["proj"], compare_filter()), &p.left, Some(&p.right));
        assert_eq!((r.failed, r.succeeded), (0, 2), "{:?}", r.failures);
        assert_eq!(files(&p.right), vec!["proj/main.rs", "proj/src/lib.rs"]);
        // notes.tmp, .env, build, src/old.tmp
        assert_eq!(r.filtered, 4);
    }

    #[test]
    fn include_filtered_items_copies_everything() {
        let p = filtered_tree();
        let o = FileOpOptions {
            filter: None,
            ..compare_filter()
        };
        let r = go(&req(FileAction::Copy, &["proj"], o), &p.left, Some(&p.right));
        assert_eq!((r.failed, r.succeeded, r.filtered), (0, 6, 0), "{:?}", r.failures);
        assert_eq!(files(&p.right).len(), 6);
    }

    #[test]
    fn explicitly_selected_items_are_copied_even_if_filtered() {
        let p = filtered_tree();
        let r = go(&req(FileAction::Copy, &["proj/notes.tmp"], compare_filter()), &p.left, Some(&p.right));
        assert_eq!(r.succeeded, 1);
        assert_eq!(files(&p.right), vec!["proj/notes.tmp"]);
    }

    #[test]
    fn moving_a_folder_leaves_filtered_items_and_their_folders_behind() {
        let p = filtered_tree();
        let r = go(&req(FileAction::Move, &["proj"], compare_filter()), &p.left, Some(&p.right));
        assert_eq!((r.failed, r.succeeded), (0, 2), "{:?}", r.failures);
        assert_eq!(files(&p.right), vec!["proj/main.rs", "proj/src/lib.rs"]);
        // The hidden items stay where they were, folders included.
        assert_eq!(
            files(&p.left),
            vec!["proj/.env", "proj/build/out.o", "proj/notes.tmp", "proj/src/old.tmp"]
        );
    }

    #[test]
    fn filter_names_sizes_and_case() {
        let meta = |is_dir: bool, size: u64| EntryMeta {
            is_dir,
            size,
            ..Default::default()
        };
        let f = CompiledFilter::new(&ItemFilter {
            include_files: vec!["*.rs".into()],
            exclude_folders: vec!["target".into(), "docs/old".into()],
            max_size: Some(100),
            include_hidden: true,
            ignore_name_case: true,
            ..Default::default()
        })
        .unwrap();
        assert!(!f.hides("lib.RS", "src/lib.RS", &meta(false, 10)));
        assert!(f.hides("lib.c", "src/lib.c", &meta(false, 10)), "not included");
        assert!(f.hides("big.rs", "big.rs", &meta(false, 1000)), "too large");
        // Include masks are for files; folders are only excluded by folder masks.
        assert!(!f.hides("src", "src", &meta(true, 0)));
        assert!(f.hides("Target", "a/Target", &meta(true, 0)));
        assert!(f.hides("old", "docs/old", &meta(true, 0)), "relative path match");
        assert!(!f.hides(".git", ".git", &meta(true, 0)), "hidden items included");
        assert!(CompiledFilter::new(&ItemFilter {
            exclude: vec!["[".into()],
            ..Default::default()
        })
        .is_err());
    }

    #[test]
    fn copy_files_and_folders_to_the_other_side() {
        let p = pair();
        let r = go(
            &req(
                FileAction::Copy,
                &["a.txt", "docs"],
                opts(ConflictMode::Overwrite),
            ),
            &p.left,
            Some(&p.right),
        );
        assert_eq!(
            (r.succeeded, r.failed, r.skipped),
            (3, 0, 0),
            "{:?}",
            r.failures
        );
        assert_eq!(r.bytes, 6 + 2 + 3);
        assert_eq!(read(&p.right, "a.txt"), "left a");
        assert_eq!(read(&p.right, "docs/sub/c.txt"), "ccc");
        assert_eq!(mtime(&p.right, "docs/b.txt"), OLD, "times carried over");
        assert_eq!(
            files(&p.right),
            ["a.txt", "docs/b.txt", "docs/sub/c.txt"],
            "no temp files"
        );
        assert_eq!(
            read(&p.left, "a.txt"),
            "left a",
            "copies leave the source alone"
        );
    }

    #[test]
    fn replaced_files_go_to_the_backups_and_deletes_to_the_bin() {
        use pcompare_core::backups::{BackupOrigin, BackupReason, BackupStore};
        let p = pair();
        let store = BackupStore::new(p._dir.path().join("backups"));
        let batch = Arc::new(store.begin(BackupOrigin {
            tool: "Folder Compare".into(),
            action: "Copy left → right".into(),
            ..Default::default()
        }));
        let deleter = crate::fsops::BackupDeleter::new(batch.clone()).deleting_with(Box::new(PermanentDeleter));
        let mut o = opts(ConflictMode::Overwrite);
        o.trash_replaced = true;
        let ctl = Control::default();
        let r = run(
            &req(FileAction::Copy, &["a.txt", "docs"], o),
            Root::Local(&p.left),
            Some(Root::Local(&p.right)),
            &deleter,
            &ctl,
            &mut |_| {},
            &mut |_| panic!("not expected to ask"),
        );
        assert_eq!((r.succeeded, r.failed), (3, 0), "{:?}", r.failures);
        assert_eq!(read(&p.right, "a.txt"), "left a");
        // Only the replaced file was kept; new files had nothing to replace.
        let kept = store.get(&batch.id().unwrap()).unwrap();
        assert_eq!(kept.items.len(), 1);
        let item = &kept.items[0];
        assert_eq!(Path::new(&item.original), p.right.join("a.txt"));
        assert_eq!(item.reason, BackupReason::Replaced);
        assert_eq!(fs::read_to_string(Path::new(&kept.folder).join(&item.stored)).unwrap(), "right a (old)");

        // A plain delete doesn't fill the backups.
        let r = run(
            &req(FileAction::Delete, &["docs"], FileOpOptions::default()),
            Root::Local(&p.right),
            None,
            &deleter,
            &ctl,
            &mut |_| {},
            &mut |_| panic!("not expected to ask"),
        );
        assert_eq!(r.failed, 0, "{:?}", r.failures);
        assert_eq!(batch.kept(), 1);
    }

    #[test]
    fn conflict_policies() {
        let p = pair();
        let skip = go(
            &req(FileAction::Copy, &["a.txt"], opts(ConflictMode::Skip)),
            &p.left,
            Some(&p.right),
        );
        assert_eq!((skip.succeeded, skip.skipped), (0, 1));
        assert_eq!(read(&p.right, "a.txt"), "right a (old)");

        // Older target: overwritten; newer target: kept.
        let older = go(
            &req(
                FileAction::Copy,
                &["a.txt"],
                opts(ConflictMode::OverwriteOlder),
            ),
            &p.left,
            Some(&p.right),
        );
        assert_eq!(older.succeeded, 1);
        assert_eq!(read(&p.right, "a.txt"), "left a");
        write(&p.right, "a.txt", "right a (newest)", NEW + 10);
        let newer = go(
            &req(
                FileAction::Copy,
                &["a.txt"],
                opts(ConflictMode::OverwriteOlder),
            ),
            &p.left,
            Some(&p.right),
        );
        assert_eq!((newer.succeeded, newer.skipped), (0, 1));
        assert_eq!(read(&p.right, "a.txt"), "right a (newest)");
    }

    #[test]
    fn ask_answers_apply_once_or_to_all() {
        let p = pair();
        for f in ["x.txt", "y.txt", "z.txt"] {
            write(&p.left, f, "L", NEW);
            write(&p.right, f, "R", OLD);
        }
        let asked = AtomicUsize::new(0);
        let mut answers = vec![ConflictAnswer::Skip, ConflictAnswer::OverwriteAll].into_iter();
        let r = run(
            &req(
                FileAction::Copy,
                &["x.txt", "y.txt", "z.txt"],
                opts(ConflictMode::Ask),
            ),
            Root::Local(&p.left),
            Some(Root::Local(&p.right)),
            &PermanentDeleter,
            &Control::default(),
            &mut |_| {},
            &mut |q| {
                asked.fetch_add(1, Ordering::Relaxed);
                assert_eq!((q.source.size, q.target.size), (1, 1));
                answers.next().unwrap()
            },
        );
        assert_eq!(
            asked.load(Ordering::Relaxed),
            2,
            "the second answer covered the rest"
        );
        assert_eq!((r.succeeded, r.skipped), (2, 1));
        assert_eq!(
            [
                read(&p.right, "x.txt"),
                read(&p.right, "y.txt"),
                read(&p.right, "z.txt")
            ],
            ["R", "L", "L"]
        );

        // Cancel from the question stops everything.
        write(&p.right, "x.txt", "R", OLD);
        let r = run(
            &req(
                FileAction::Copy,
                &["x.txt", "docs"],
                opts(ConflictMode::Ask),
            ),
            Root::Local(&p.left),
            Some(Root::Local(&p.right)),
            &PermanentDeleter,
            &Control::default(),
            &mut |_| {},
            &mut |_| ConflictAnswer::Cancel,
        );
        assert!(r.cancelled);
        assert!(!p.right.join("docs").exists());
    }

    #[test]
    fn move_renames_on_one_volume_and_copies_into_existing_folders() {
        let p = pair();
        write(&p.right, "docs/keep.txt", "k", OLD);
        let r = go(
            &req(
                FileAction::Move,
                &["docs", "a.txt"],
                opts(ConflictMode::Overwrite),
            ),
            &p.left,
            Some(&p.right),
        );
        assert_eq!((r.succeeded, r.failed), (3, 0), "{:?}", r.failures);
        assert_eq!(
            files(&p.right),
            ["a.txt", "docs/b.txt", "docs/keep.txt", "docs/sub/c.txt"]
        );
        assert_eq!(read(&p.right, "a.txt"), "left a");
        assert!(
            files(&p.left).is_empty(),
            "sources removed: {:?}",
            files(&p.left)
        );
        assert!(!p.left.join("docs").exists(), "emptied folders removed");

        // A fresh destination folder: one rename.
        write(&p.left, "pack/x.txt", "x", OLD);
        let r = go(
            &FileOpRequest {
                action: FileAction::Move,
                items: vec![FileItem {
                    src: "pack".into(),
                    dst: "moved/pack".into(),
                }],
                options: opts(ConflictMode::Skip),
            },
            &p.left,
            Some(&p.right),
        );
        assert_eq!(r.succeeded, 1);
        assert_eq!(read(&p.right, "moved/pack/x.txt"), "x");
        assert!(!p.left.join("pack").exists());
    }

    #[test]
    fn skipped_conflicts_keep_the_source_of_a_move() {
        let p = pair();
        let r = go(
            &req(FileAction::Move, &["a.txt"], opts(ConflictMode::Skip)),
            &p.left,
            Some(&p.right),
        );
        assert_eq!(r.skipped, 1);
        assert_eq!(read(&p.left, "a.txt"), "left a");
        assert_eq!(read(&p.right, "a.txt"), "right a (old)");
    }

    #[test]
    fn copy_refuses_a_folder_into_itself_and_unsafe_paths() {
        let p = pair();
        let r = go(
            &FileOpRequest {
                action: FileAction::Copy,
                items: vec![
                    FileItem {
                        src: "docs".into(),
                        dst: "docs/sub/again".into(),
                    },
                    FileItem {
                        src: "../escape".into(),
                        dst: "x".into(),
                    },
                ],
                options: opts(ConflictMode::Overwrite),
            },
            &p.left,
            Some(&p.left),
        );
        assert_eq!(r.failed, 2, "{:?}", r.failures);
        assert!(!p.left.join("docs/sub/again").exists());

        // Moving a file onto itself must not lose it.
        let same = go(
            &req(FileAction::Move, &["a.txt"], opts(ConflictMode::Overwrite)),
            &p.left,
            Some(&p.left),
        );
        assert_eq!(same.failed, 1);
        assert!(same.failures[0].message.contains("same"));
        assert_eq!(read(&p.left, "a.txt"), "left a");
    }

    #[test]
    fn delete_rename_and_new_folder() {
        let p = pair();
        let r = go(
            &req(
                FileAction::Delete,
                &["docs", "docs/b.txt", "missing.txt"],
                FileOpOptions {
                    permanent: true,
                    ..Default::default()
                },
            ),
            &p.left,
            None,
        );
        assert_eq!(
            (r.succeeded, r.failed),
            (2, 0),
            "nested item merged into its folder; missing is fine"
        );
        assert_eq!(files(&p.left), ["a.txt"]);

        let rename = FileOpOptions {
            new_name: "renamed.txt".into(),
            ..Default::default()
        };
        assert_eq!(
            go(&req(FileAction::Rename, &["a.txt"], rename), &p.left, None).succeeded,
            1
        );
        assert_eq!(read(&p.left, "renamed.txt"), "left a");
        write(&p.left, "other.txt", "o", OLD);
        let clash = FileOpOptions {
            new_name: "other.txt".into(),
            ..Default::default()
        };
        assert_eq!(
            go(
                &req(FileAction::Rename, &["renamed.txt"], clash),
                &p.left,
                None
            )
            .failed,
            1
        );
        let bad = FileOpOptions {
            new_name: "../up.txt".into(),
            ..Default::default()
        };
        assert_eq!(
            go(
                &req(FileAction::Rename, &["renamed.txt"], bad),
                &p.left,
                None
            )
            .failed,
            1
        );
        // Changing only the case is a rename, not a clash.
        let case = FileOpOptions {
            new_name: "Renamed.txt".into(),
            ..Default::default()
        };
        assert_eq!(
            go(
                &req(FileAction::Rename, &["renamed.txt"], case),
                &p.left,
                None
            )
            .succeeded,
            1
        );
        assert!(files(&p.left).contains(&"Renamed.txt".to_string()));

        assert_eq!(
            go(
                &req(
                    FileAction::NewFolder,
                    &["fresh/inner"],
                    FileOpOptions::default()
                ),
                &p.left,
                None
            )
            .succeeded,
            1
        );
        assert!(p.left.join("fresh/inner").is_dir());
        assert_eq!(
            go(
                &req(FileAction::NewFolder, &["fresh"], FileOpOptions::default()),
                &p.left,
                None
            )
            .failed,
            1
        );
    }

    #[cfg(unix)]
    #[test]
    fn attributes_change_permission_bits_recursively() {
        use std::os::unix::fs::PermissionsExt;
        let p = pair();
        let mode = |rel: &str| fs::metadata(p.left.join(rel)).unwrap().permissions().mode() & 0o777;
        let change = FileOpOptions {
            attributes: AttrChange {
                set: pcompare_core::folder::attr::READONLY,
                mode_clear: 0o044,
                ..Default::default()
            },
            recursive: true,
            ..Default::default()
        };
        let r = go(
            &req(FileAction::Attributes, &["docs"], change),
            &p.left,
            None,
        );
        assert_eq!(
            (r.succeeded, r.failed),
            (4, 0),
            "docs, b.txt, sub, c.txt: {:?}",
            r.failures
        );
        assert_eq!(mode("docs/b.txt") & 0o222, 0, "read-only");
        assert_eq!(mode("docs/b.txt") & 0o044, 0);
        // Writable again, so the temp dir can be cleaned up.
        let undo = FileOpOptions {
            attributes: AttrChange {
                clear: pcompare_core::folder::attr::READONLY,
                mode_set: 0o700,
                ..Default::default()
            },
            recursive: true,
            ..Default::default()
        };
        go(&req(FileAction::Attributes, &["docs"], undo), &p.left, None);
        assert_eq!(mode("docs/sub/c.txt") & 0o200, 0o200);
    }

    #[test]
    fn touch_now_at_and_from_the_other_side() {
        let p = pair();
        let at = FileOpOptions {
            touch: TouchTime::At {
                ms: 1_650_000_000_000,
            },
            recursive: true,
            ..Default::default()
        };
        let r = go(&req(FileAction::Touch, &["docs"], at), &p.left, None);
        assert_eq!(r.succeeded, 4);
        assert_eq!(mtime(&p.left, "docs/sub/c.txt"), 1_650_000_000);

        let other = FileOpOptions {
            touch: TouchTime::OtherSide,
            ..Default::default()
        };
        let r = go(
            &req(FileAction::Touch, &["a.txt", "docs"], other),
            &p.left,
            Some(&p.right),
        );
        assert_eq!((r.succeeded, r.skipped), (1, 1), "docs isn't on the right");
        assert_eq!(mtime(&p.left, "a.txt"), OLD);

        let before = now_ms() / 1000;
        go(
            &req(FileAction::Touch, &["a.txt"], FileOpOptions::default()),
            &p.left,
            None,
        );
        assert!(mtime(&p.left, "a.txt") >= before);
    }

    #[test]
    fn pause_holds_and_resume_finishes_cancel_stops() {
        let p = pair();
        for i in 0..5 {
            write(&p.left, &format!("many/{i}.txt"), "data", OLD);
        }
        let ctl = Arc::new(Control::default());
        ctl.set_paused(true);
        let seen_pause = Arc::new(AtomicBool::new(false));
        let (left, right) = (p.left.clone(), p.right.clone());
        let worker = {
            let (ctl, seen_pause) = (ctl.clone(), seen_pause.clone());
            std::thread::spawn(move || {
                run(
                    &req(FileAction::Copy, &["many"], opts(ConflictMode::Overwrite)),
                    Root::Local(&left),
                    Some(Root::Local(&right)),
                    &PermanentDeleter,
                    &ctl,
                    &mut |s| {
                        if s.phase == FilePhase::Paused {
                            seen_pause.store(true, Ordering::Relaxed);
                        }
                    },
                    &mut |_| ConflictAnswer::Overwrite,
                )
            })
        };
        let wait = Instant::now();
        while !seen_pause.load(Ordering::Relaxed) && wait.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(seen_pause.load(Ordering::Relaxed), "reported as paused");
        std::thread::sleep(Duration::from_millis(100));
        assert!(
            !p.right.join("many").exists() || files(&p.right).len() == 1,
            "nothing copied while paused"
        );
        ctl.set_paused(false);
        let r = worker.join().unwrap();
        assert_eq!((r.succeeded, r.cancelled), (5, false));

        // Cancelled while paused: stops without copying anything more.
        let ctl = Control::default();
        ctl.set_paused(true);
        ctl.cancel();
        let r = run(
            &req(FileAction::Copy, &["docs"], opts(ConflictMode::Overwrite)),
            Root::Local(&p.left),
            Some(Root::Local(&p.right)),
            &PermanentDeleter,
            &ctl,
            &mut |_| {},
            &mut |_| ConflictAnswer::Overwrite,
        );
        assert!(r.cancelled);
        assert_eq!(r.failed, 0, "a cancellation isn't a failure");
        assert!(!p.right.join("docs").exists());
    }

    #[test]
    fn queue_serializes_overlapping_paths_only() {
        let q = Arc::new(OpQueue::default());
        let never = AtomicBool::new(false);
        let a = q.acquire(&["/data/L/docs".into()], &never).unwrap();
        // Unrelated work runs alongside.
        let b = q.acquire(&["/data/L/other".into()], &never).unwrap();
        drop(b);

        let order = Arc::new(Mutex::new(Vec::new()));
        let inside = {
            let (q, order) = (q.clone(), order.clone());
            std::thread::spawn(move || {
                let never = AtomicBool::new(false);
                let _slot = q
                    .acquire(&["/DATA/L/docs/sub/x.txt".into()], &never)
                    .unwrap();
                order.lock().unwrap().push("inside");
            })
        };
        std::thread::sleep(Duration::from_millis(150));
        assert!(
            order.lock().unwrap().is_empty(),
            "waits for the folder above it"
        );
        order.lock().unwrap().push("first done");
        drop(a);
        inside.join().unwrap();
        assert_eq!(*order.lock().unwrap(), ["first done", "inside"]);

        // A queued operation can be cancelled while it waits.
        let held = q.acquire(&["/x".into()], &never).unwrap();
        let cancel = AtomicBool::new(true);
        assert!(q.acquire(&["/x/y".into()], &cancel).is_none());
        drop(held);
        assert!(q.acquire(&["/x/y".into()], &never).is_some());
    }

    #[test]
    fn lock_paths_cover_sources_destinations_and_new_names() {
        let mut r = req(FileAction::Copy, &["docs"], FileOpOptions::default());
        assert_eq!(lock_paths(&r, "/L/", Some("/R")), ["/L/docs", "/R/docs"]);
        r.action = FileAction::Rename;
        r.options.new_name = "new".into();
        r.items[0].src = "docs/old".into();
        assert_eq!(lock_paths(&r, "/L", None), ["/L/docs/old", "/L/docs/new"]);
        assert!(overlaps(&[path_key("C:\\Data\\x")], &[path_key("c:/data")]));
        assert!(!overlaps(&["/a/bc".into()], &["/a/b".into()]));
    }

    /// The local disk posing as a remote, with chosen capabilities.
    struct FakeRemote(WriteCaps);

    impl Vfs for FakeRemote {
        fn read_dir(&self, dir: &str) -> pcompare_core::Result<Vec<VfsEntry>> {
            LocalFs.read_dir(dir)
        }
        fn open(&self, path: &str) -> pcompare_core::Result<Box<dyn Read + Send>> {
            LocalFs.open(path)
        }
        fn join(&self, dir: &str, name: &str) -> String {
            LocalFs.join(dir, name)
        }
    }

    impl VfsWrite for FakeRemote {
        fn caps(&self) -> WriteCaps {
            self.0
        }
        fn stat(&self, path: &str) -> io::Result<Option<EntryMeta>> {
            LocalFs.stat(path)
        }
        fn open_at(&self, path: &str, offset: u64) -> io::Result<Box<dyn Read + Send>> {
            LocalFs.open_at(path, offset)
        }
        fn write(
            &self,
            path: &str,
            offset: u64,
            src: &mut dyn Read,
            on_bytes: &mut dyn FnMut(u64) -> io::Result<()>,
        ) -> io::Result<u64> {
            LocalFs.write(path, offset, src, on_bytes)
        }
        fn create_dir_all(&self, path: &str) -> io::Result<()> {
            LocalFs.create_dir_all(path)
        }
        fn remove_file(&self, path: &str) -> io::Result<()> {
            LocalFs.remove_file(path)
        }
        fn remove_dir_all(&self, path: &str) -> io::Result<()> {
            LocalFs.remove_dir_all(path)
        }
        fn rename(&self, from: &str, to: &str) -> io::Result<()> {
            if !self.0.rename {
                return Err(io::Error::from(io::ErrorKind::Unsupported));
            }
            LocalFs.rename(from, to)
        }
        fn set_modified(&self, path: &str, ms: i64) -> io::Result<()> {
            LocalFs.set_modified(path, ms)
        }
    }

    const SFTP: WriteCaps = WriteCaps {
        rename: true,
        set_modified: true,
        resume_write: true,
        range_read: true,
        folders: true,
        serial_writes: false,
    };
    const S3: WriteCaps = WriteCaps {
        rename: false,
        set_modified: false,
        resume_write: false,
        range_read: true,
        folders: false,
        serial_writes: false,
    };
    /// Like a zip archive: one file, rewritten whole by every change.
    const ZIP: WriteCaps = WriteCaps {
        rename: true,
        set_modified: true,
        resume_write: false,
        range_read: true,
        folders: true,
        serial_writes: true,
    };

    /// A remote whose uploads take a while, counting how many run at once.
    struct SlowRemote {
        inner: FakeRemote,
        active: AtomicUsize,
        peak: AtomicUsize,
    }

    impl Vfs for SlowRemote {
        fn read_dir(&self, dir: &str) -> pcompare_core::Result<Vec<VfsEntry>> {
            self.inner.read_dir(dir)
        }
        fn open(&self, path: &str) -> pcompare_core::Result<Box<dyn Read + Send>> {
            self.inner.open(path)
        }
        fn join(&self, dir: &str, name: &str) -> String {
            self.inner.join(dir, name)
        }
    }

    impl VfsWrite for SlowRemote {
        fn caps(&self) -> WriteCaps {
            self.inner.caps()
        }
        fn stat(&self, path: &str) -> io::Result<Option<EntryMeta>> {
            self.inner.stat(path)
        }
        fn open_at(&self, path: &str, offset: u64) -> io::Result<Box<dyn Read + Send>> {
            self.inner.open_at(path, offset)
        }
        fn write(
            &self,
            path: &str,
            offset: u64,
            src: &mut dyn Read,
            on_bytes: &mut dyn FnMut(u64) -> io::Result<()>,
        ) -> io::Result<u64> {
            let now = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(now, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(40));
            let r = self.inner.write(path, offset, src, on_bytes);
            self.active.fetch_sub(1, Ordering::SeqCst);
            r
        }
        fn create_dir_all(&self, path: &str) -> io::Result<()> {
            self.inner.create_dir_all(path)
        }
        fn remove_file(&self, path: &str) -> io::Result<()> {
            self.inner.remove_file(path)
        }
        fn remove_dir_all(&self, path: &str) -> io::Result<()> {
            self.inner.remove_dir_all(path)
        }
        fn rename(&self, from: &str, to: &str) -> io::Result<()> {
            self.inner.rename(from, to)
        }
        fn set_modified(&self, path: &str, ms: i64) -> io::Result<()> {
            self.inner.set_modified(path, ms)
        }
    }

    #[test]
    fn copies_run_in_parallel_and_report_each_file_as_it_lands() {
        let p = pair();
        for i in 0..12 {
            write(&p.left, &format!("many/{i:02}.txt"), "data", OLD);
        }
        let remote = SlowRemote {
            inner: FakeRemote(SFTP),
            active: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
        };
        let r_path = p.right.to_string_lossy().into_owned();
        let right = Root::Remote {
            fs: &remote,
            path: &r_path,
            label: "remote://box/R",
        };
        let mut landed = Vec::new();
        let mut done_when_landed = Vec::new();
        let options = FileOpOptions {
            threads: 4,
            ..opts(ConflictMode::Overwrite)
        };
        let r = run(
            &req(FileAction::Copy, &["many"], options),
            Root::Local(&p.left),
            Some(right),
            &PermanentDeleter,
            &Control::default(),
            &mut |s| {
                if !s.landed.is_empty() && s.phase != FilePhase::Finished {
                    done_when_landed.push(s.files_done);
                }
                landed.extend(s.landed.iter().cloned());
            },
            &mut |_| panic!("not expected to ask"),
        );
        assert_eq!((r.succeeded, r.failed), (12, 0));
        let peak = remote.peak.load(Ordering::SeqCst);
        assert!(peak >= 2, "uploads overlapped (peak {peak})");
        assert!(peak <= 4, "never more than the threads asked for (peak {peak})");
        // The new folder, then every file exactly once, while the copy ran.
        let mut want: Vec<String> = (0..12).map(|i| format!("many/{i:02}.txt")).collect();
        want.push("many".into());
        want.sort();
        landed.sort();
        assert_eq!(landed, want);
        assert!(done_when_landed.iter().any(|&n| n < 12), "rows arrive before the end");
        assert_eq!(files(&p.right).len(), 13);
    }

    #[test]
    fn copies_into_an_archive_run_one_at_a_time() {
        // Each change rewrites the whole archive; on Windows a rewrite fails
        // ("Access is denied") while another thread has the archive open.
        let p = pair();
        for i in 0..6 {
            write(&p.left, &format!("many/{i}.txt"), "data", OLD);
        }
        let archive = SlowRemote {
            inner: FakeRemote(ZIP),
            active: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
        };
        let r_path = p.right.to_string_lossy().into_owned();
        let right = Root::Remote {
            fs: &archive,
            path: &r_path,
            label: "out/a.zip",
        };
        let options = FileOpOptions {
            threads: 4,
            ..opts(ConflictMode::Overwrite)
        };
        let r = run(
            &req(FileAction::Copy, &["many"], options),
            Root::Local(&p.left),
            Some(right),
            &PermanentDeleter,
            &Control::default(),
            &mut |_| {},
            &mut |_| panic!("not expected to ask"),
        );
        assert_eq!((r.succeeded, r.failed), (6, 0), "{:?}", r.failures);
        assert_eq!(archive.peak.load(Ordering::SeqCst), 1, "one write at a time");
        assert_eq!(files(&p.right).len(), 7);
    }

    #[test]
    fn archives_get_a_single_worker() {
        let (zip, sftp) = (FakeRemote(ZIP), FakeRemote(SFTP));
        let archive = Root::Remote {
            fs: &zip,
            path: "",
            label: "a.zip",
        };
        let server = Root::Remote {
            fs: &sftp,
            path: "",
            label: "remote://box",
        };
        let local = Root::Local(Path::new("."));
        assert_eq!(copy_workers(4, &local, Some(&archive), false), 1);
        assert_eq!(copy_workers(0, &server, Some(&archive), true), 1);
        // Reading from an archive can overlap; a move also removes from it.
        assert_eq!(copy_workers(4, &archive, Some(&local), false), 4);
        assert_eq!(copy_workers(4, &archive, Some(&local), true), 1);
        assert_eq!(copy_workers(4, &local, Some(&server), true), 4);
        assert_eq!(copy_workers(0, &local, Some(&server), false), copy_threads(false, true));
        assert_eq!(copy_workers(99, &local, Some(&local), false), MAX_COPY_THREADS);
    }

    /// A zip archive as Windows sees it: every change swaps a rewritten
    /// file in, which fails while one of its own readers still has the
    /// old one open. Lookups and reads take a while, so a walk that runs
    /// alongside the copying meets the readers.
    struct OneFileArchive {
        inner: FakeRemote,
        readers: Arc<AtomicUsize>,
    }

    /// A reader of [`OneFileArchive`], counted as open until dropped.
    struct OpenReader {
        inner: Box<dyn Read + Send>,
        readers: Arc<AtomicUsize>,
    }

    impl Read for OpenReader {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            std::thread::sleep(Duration::from_millis(50));
            self.inner.read(buf)
        }
    }

    impl Drop for OpenReader {
        fn drop(&mut self) {
            self.readers.fetch_sub(1, Ordering::SeqCst);
        }
    }

    impl OneFileArchive {
        fn swap_in(&self) -> io::Result<()> {
            match self.readers.load(Ordering::SeqCst) {
                0 => Ok(()),
                _ => Err(io::Error::new(io::ErrorKind::PermissionDenied, "Access is denied")),
            }
        }
    }

    impl Vfs for OneFileArchive {
        fn read_dir(&self, dir: &str) -> pcompare_core::Result<Vec<VfsEntry>> {
            self.inner.read_dir(dir)
        }
        fn open(&self, path: &str) -> pcompare_core::Result<Box<dyn Read + Send>> {
            self.inner.open(path)
        }
        fn join(&self, dir: &str, name: &str) -> String {
            self.inner.join(dir, name)
        }
    }

    impl VfsWrite for OneFileArchive {
        fn caps(&self) -> WriteCaps {
            self.inner.caps()
        }
        fn stat(&self, path: &str) -> io::Result<Option<EntryMeta>> {
            std::thread::sleep(Duration::from_millis(10));
            self.inner.stat(path)
        }
        fn open_at(&self, path: &str, offset: u64) -> io::Result<Box<dyn Read + Send>> {
            let inner = self.inner.open_at(path, offset)?;
            self.readers.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(OpenReader {
                inner,
                readers: self.readers.clone(),
            }))
        }
        fn write(
            &self,
            path: &str,
            offset: u64,
            src: &mut dyn Read,
            on_bytes: &mut dyn FnMut(u64) -> io::Result<()>,
        ) -> io::Result<u64> {
            self.swap_in()?;
            self.inner.write(path, offset, src, on_bytes)
        }
        fn create_dir_all(&self, path: &str) -> io::Result<()> {
            self.swap_in()?;
            self.inner.create_dir_all(path)
        }
        fn remove_file(&self, path: &str) -> io::Result<()> {
            self.swap_in()?;
            self.inner.remove_file(path)
        }
        fn remove_dir_all(&self, path: &str) -> io::Result<()> {
            self.swap_in()?;
            self.inner.remove_dir_all(path)
        }
        fn rename(&self, from: &str, to: &str) -> io::Result<()> {
            self.swap_in()?;
            self.inner.rename(from, to)
        }
        fn set_modified(&self, path: &str, ms: i64) -> io::Result<()> {
            self.swap_in()?;
            self.inner.set_modified(path, ms)
        }
    }

    #[test]
    fn copies_between_folders_of_one_archive_read_each_file_out_first() {
        // Folder Compare of a.zip/v1 against a.zip/v2: both sides are the
        // one archive, so no source entry may be open while it changes —
        // neither when a file is written nor when the walk makes docs.
        let p = pair();
        let archive = OneFileArchive {
            inner: FakeRemote(ZIP),
            readers: Arc::default(),
        };
        let l_path = p.left.to_string_lossy().into_owned();
        let r_path = p.right.to_string_lossy().into_owned();
        let left = Root::Remote {
            fs: &archive,
            path: &l_path,
            label: "a.zip/v1",
        };
        let right = Root::Remote {
            fs: &archive,
            path: &r_path,
            label: "a.zip/v2",
        };
        let r = run(
            &req(FileAction::Copy, &["a.txt", "docs"], opts(ConflictMode::Overwrite)),
            left,
            Some(right),
            &PermanentDeleter,
            &Control::default(),
            &mut |_| {},
            &mut |_| panic!("not expected to ask"),
        );
        assert_eq!((r.succeeded, r.failed), (3, 0), "{:?}", r.failures);
        assert_eq!(r.bytes, 6 + 2 + 3);
        assert_eq!(read(&p.right, "a.txt"), "left a");
        assert_eq!(read(&p.right, "docs/sub/c.txt"), "ccc");
        assert_eq!(mtime(&p.right, "docs/b.txt"), OLD);
        assert_eq!(files(&p.right), ["a.txt", "docs/b.txt", "docs/sub/c.txt"]);
        assert_eq!(archive.readers.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn only_copies_from_an_archive_into_one_read_out_first() {
        let (zip, other_zip, sftp) = (FakeRemote(ZIP), FakeRemote(ZIP), FakeRemote(SFTP));
        assert!(read_out_first(&zip, &zip));
        // Maybe the same file under another spelling of its path.
        assert!(read_out_first(&other_zip, &zip));
        assert!(!read_out_first(&sftp, &zip));
        assert!(!read_out_first(&zip, &sftp));
        assert!(!read_out_first(&sftp, &sftp));
    }

    #[test]
    fn a_parallel_move_still_removes_the_emptied_folders() {
        let p = pair();
        let remote = FakeRemote(S3); // no rename: every file is copied, then removed
        let l_path = p.left.to_string_lossy().into_owned();
        let left = Root::Remote {
            fs: &remote,
            path: &l_path,
            label: "remote://box/L",
        };
        let mut removed = Vec::new();
        let options = FileOpOptions {
            threads: 3,
            ..opts(ConflictMode::Overwrite)
        };
        let r = run(
            &req(FileAction::Move, &["docs"], options),
            left,
            Some(Root::Local(&p.right)),
            &PermanentDeleter,
            &Control::default(),
            &mut |s| removed.extend(s.removed.iter().cloned()),
            &mut |_| panic!("not expected to ask"),
        );
        assert_eq!((r.succeeded, r.failed), (2, 0));
        assert!(!p.left.join("docs").exists(), "the moved folder is gone");
        assert_eq!(read(&p.right, "docs/sub/c.txt"), "ccc");
        removed.sort();
        assert_eq!(removed, ["docs", "docs/b.txt", "docs/sub", "docs/sub/c.txt"]);
    }

    #[test]
    fn remote_sides_upload_download_delete_and_move() {
        for caps in [SFTP, S3] {
            let p = pair();
            let remote = FakeRemote(caps);
            let r_path = p.right.to_string_lossy().into_owned();
            let right = Root::Remote {
                fs: &remote,
                path: &r_path,
                label: "remote://box/R",
            };
            let all = |a: FileAction, rels: &[&str], src: Root, dst: Option<Root>| {
                run(
                    &req(a, rels, opts(ConflictMode::Overwrite)),
                    src,
                    dst,
                    &PermanentDeleter,
                    &Control::default(),
                    &mut |_| {},
                    &mut |_| ConflictAnswer::Overwrite,
                )
            };
            let up = all(
                FileAction::Copy,
                &["a.txt", "docs"],
                Root::Local(&p.left),
                Some(right),
            );
            assert_eq!((up.succeeded, up.failed), (3, 0), "{:?}", up.failures);
            assert_eq!(read(&p.right, "a.txt"), "left a");
            assert_eq!(
                files(&p.right),
                ["a.txt", "docs/b.txt", "docs/sub/c.txt"],
                "no temp files"
            );
            if caps.set_modified {
                assert_eq!(mtime(&p.right, "docs/b.txt"), OLD);
            }

            write(&p.right, "from-remote.txt", "remote", OLD);
            let down = all(
                FileAction::Copy,
                &["from-remote.txt"],
                right,
                Some(Root::Local(&p.left)),
            );
            assert_eq!(down.succeeded, 1, "{:?}", down.failures);
            assert_eq!(read(&p.left, "from-remote.txt"), "remote");
            assert_eq!(mtime(&p.left, "from-remote.txt"), OLD);

            let moved = all(
                FileAction::Move,
                &["from-remote.txt"],
                right,
                Some(Root::Local(&p.left)),
            );
            assert_eq!(moved.succeeded, 1, "{:?}", moved.failures);
            assert!(!p.right.join("from-remote.txt").exists());

            let gone = all(FileAction::Delete, &["docs"], right, None);
            assert_eq!(gone.succeeded, 1);
            assert_eq!(files(&p.right), ["a.txt"]);
        }
    }

    // ----- files in use -----

    /// Retries without the waiting.
    fn quick(conflicts: ConflictMode) -> FileOpOptions {
        FileOpOptions {
            conflicts,
            retry_delay_ms: 1,
            retry_max_delay_ms: 2,
            ..Default::default()
        }
    }

    /// Counts one more refusal down; false once there are none left.
    fn refuse(left: &Mutex<u32>) -> bool {
        let mut n = left.lock().unwrap();
        *n = match n.checked_sub(1) {
            Some(rest) => rest,
            None => return false,
        };
        true
    }

    /// A Recycle Bin that refuses the first `n` files it is given, as Windows
    /// does while another program has them open.
    struct Busy(Mutex<u32>);

    impl Busy {
        fn new(n: u32) -> Self {
            Busy(Mutex::new(n))
        }
    }

    impl Deleter for Busy {
        fn delete(&self, path: &Path) -> io::Result<()> {
            if refuse(&self.0) {
                return Err(io::Error::from(io::ErrorKind::ResourceBusy));
            }
            PermanentDeleter.delete(path)
        }
    }

    /// Runs `req` between two local folders, replacing files through `bin`.
    fn go_with(
        req: &FileOpRequest,
        src: Root,
        dst: Root,
        bin: &dyn Deleter,
        progress: &mut dyn FnMut(&FileOpProgress),
    ) -> FileOpReport {
        run(
            req,
            src,
            Some(dst),
            bin,
            &Control::default(),
            progress,
            &mut |_| panic!("not expected to ask"),
        )
    }

    #[test]
    fn a_file_in_use_is_retried_until_it_is_free() {
        let p = pair();
        // The old a.txt can't be put in the Trash for two attempts.
        let busy = Busy::new(2);
        let copy = req(FileAction::Copy, &["a.txt"], quick(ConflictMode::Overwrite));
        let r = go_with(&copy, Root::Local(&p.left), Root::Local(&p.right), &busy, &mut |_| {});
        assert_eq!((r.succeeded, r.failed), (1, 0), "{:?}", r.failures);
        assert_eq!(read(&p.right, "a.txt"), "left a");
        assert_eq!(files(&p.right), ["a.txt"], "no temp files left behind");
    }

    #[test]
    fn a_file_still_in_use_gets_a_second_round_at_the_end() {
        let p = pair();
        // Held through the first round's four attempts, free by the second.
        let busy = Busy::new(4);
        let options = FileOpOptions {
            threads: 1,
            ..quick(ConflictMode::Overwrite)
        };
        let copy = req(FileAction::Copy, &["a.txt", "docs"], options);
        let (mut landed, mut phases) = (Vec::new(), Vec::new());
        let r = go_with(&copy, Root::Local(&p.left), Root::Local(&p.right), &busy, &mut |pr| {
            landed.extend(pr.landed.iter().cloned());
            phases.push(pr.phase);
        });
        assert_eq!((r.succeeded, r.failed), (3, 0), "{:?}", r.failures);
        assert_eq!(read(&p.right, "a.txt"), "left a");
        assert_eq!(landed.last().map(String::as_str), Some("a.txt"), "{landed:?}");
        assert!(phases.contains(&FilePhase::Deferred));
    }

    #[test]
    fn a_file_in_use_to_the_end_fails_and_keeps_the_old_version() {
        let p = pair();
        let busy = Busy::new(u32::MAX);
        let copy = req(FileAction::Copy, &["a.txt", "docs"], quick(ConflictMode::Overwrite));
        let r = go_with(&copy, Root::Local(&p.left), Root::Local(&p.right), &busy, &mut |_| {});
        assert_eq!((r.succeeded, r.failed), (2, 1), "{:?}", r.failures);
        assert_eq!(r.failures[0].path, "a.txt");
        assert_eq!(r.failures[0].kind, FailureKind::Locked);
        assert_eq!(read(&p.right, "a.txt"), "right a (old)");
        assert_eq!(files(&p.right), ["a.txt", "docs/b.txt", "docs/sub/c.txt"]);
        // Holders are listed only where the OS can tell (Windows).
        let json = serde_json::to_value(&r.failures[0]).unwrap();
        assert_eq!(json["kind"], "locked");
        assert_eq!(json.get("lockedBy").is_some(), !r.failures[0].locked_by.is_empty());
    }

    #[test]
    fn cancelling_ends_a_wait_to_retry() {
        let p = pair();
        let busy = Busy::new(u32::MAX);
        let options = FileOpOptions {
            retry_delay_ms: 60_000,
            retry_max_delay_ms: 60_000,
            ..opts(ConflictMode::Overwrite)
        };
        let ctl = Control::default();
        let started = Instant::now();
        let r = run(
            &req(FileAction::Copy, &["a.txt"], options),
            Root::Local(&p.left),
            Some(Root::Local(&p.right)),
            &busy,
            &ctl,
            &mut |pr| {
                if pr.retrying > 0 {
                    ctl.cancel();
                }
            },
            &mut |_| panic!("not expected to ask"),
        );
        assert!(r.cancelled);
        assert!(r.failures.is_empty(), "{:?}", r.failures);
        assert!(started.elapsed() < Duration::from_secs(10));
        assert_eq!(read(&p.right, "a.txt"), "right a (old)");
    }

    /// A remote whose files can't be removed while `busy` lasts (open in
    /// another program), counting how often files are read.
    struct Held {
        inner: FakeRemote,
        busy: Mutex<u32>,
        reads: AtomicUsize,
    }

    impl Vfs for Held {
        fn read_dir(&self, dir: &str) -> pcompare_core::Result<Vec<VfsEntry>> {
            self.inner.read_dir(dir)
        }
        fn open(&self, path: &str) -> pcompare_core::Result<Box<dyn Read + Send>> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            self.inner.open(path)
        }
        fn join(&self, dir: &str, name: &str) -> String {
            self.inner.join(dir, name)
        }
    }

    impl VfsWrite for Held {
        fn caps(&self) -> WriteCaps {
            self.inner.caps()
        }
        fn stat(&self, path: &str) -> io::Result<Option<EntryMeta>> {
            self.inner.stat(path)
        }
        fn open_at(&self, path: &str, offset: u64) -> io::Result<Box<dyn Read + Send>> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            self.inner.open_at(path, offset)
        }
        fn write(
            &self,
            path: &str,
            offset: u64,
            src: &mut dyn Read,
            on_bytes: &mut dyn FnMut(u64) -> io::Result<()>,
        ) -> io::Result<u64> {
            self.inner.write(path, offset, src, on_bytes)
        }
        fn create_dir_all(&self, path: &str) -> io::Result<()> {
            self.inner.create_dir_all(path)
        }
        fn remove_file(&self, path: &str) -> io::Result<()> {
            if refuse(&self.busy) {
                return Err(io::Error::from(io::ErrorKind::ResourceBusy));
            }
            self.inner.remove_file(path)
        }
        fn remove_dir_all(&self, path: &str) -> io::Result<()> {
            self.inner.remove_dir_all(path)
        }
        fn rename(&self, from: &str, to: &str) -> io::Result<()> {
            self.inner.rename(from, to)
        }
        fn set_modified(&self, path: &str, ms: i64) -> io::Result<()> {
            self.inner.set_modified(path, ms)
        }
    }

    #[test]
    fn a_move_whose_original_is_in_use_copies_it_only_once() {
        for (busy, removed) in [(4, true), (u32::MAX, false)] {
            let p = pair();
            let held = Held {
                inner: FakeRemote(SFTP),
                busy: Mutex::new(busy),
                reads: AtomicUsize::new(0),
            };
            let l_path = p.left.to_string_lossy().into_owned();
            let left = Root::Remote {
                fs: &held,
                path: &l_path,
                label: "remote://test",
            };
            let (mut landed, mut gone) = (Vec::new(), Vec::new());
            let r = go_with(
                &req(FileAction::Move, &["docs/b.txt"], quick(ConflictMode::Overwrite)),
                left,
                Root::Local(&p.right),
                &PermanentDeleter,
                &mut |pr| {
                    landed.extend(pr.landed.iter().cloned());
                    gone.extend(pr.removed.iter().cloned());
                },
            );
            assert_eq!(held.reads.load(Ordering::SeqCst), 1, "copied once (busy {busy})");
            assert_eq!(read(&p.right, "docs/b.txt"), "bb");
            assert_eq!(landed, ["docs/b.txt"]);
            assert_eq!(p.left.join("docs/b.txt").exists(), !removed);
            if removed {
                assert_eq!((r.succeeded, r.failed), (1, 0), "{:?}", r.failures);
                assert_eq!(gone, ["docs/b.txt"]);
            } else {
                assert_eq!(r.failed, 1);
                assert_eq!(r.failures[0].kind, FailureKind::Locked);
                assert!(r.failures[0].message.starts_with("copied, but"), "{}", r.failures[0].message);
                assert!(gone.is_empty());
            }
        }
    }
}
