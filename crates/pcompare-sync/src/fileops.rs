//! File operations on a Folder Compare selection (bc5:file-ops): copy or
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
//! - and the [`OpQueue`] serializes operations whose paths overlap, so two
//!   background jobs never write (or read what the other writes) at once.
//!
//! Local copies keep every guarantee of the sync executor: data goes to a
//! temp sibling, is flushed and renamed into place, and replaced files go to
//! the Recycle Bin / Trash unless the caller opts out. Remote folders have
//! no Recycle Bin, so deletions and overwrites there are permanent.

use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant, UNIX_EPOCH};

use filetime::FileTime;
use pcompare_core::folder::EntryMeta;
use pcompare_core::vfs_write::{pump, VfsWrite};
use serde::{Deserialize, Serialize};

use crate::exec::hold;
use crate::failure::{classify, FailureKind, OpError};
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
}

impl Default for FileOpOptions {
    fn default() -> Self {
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
    pub question: Option<ConflictQuestion>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileOpFailure {
    pub path: String,
    pub kind: FailureKind,
    pub message: String,
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
    pub failures: Vec<FileOpFailure>,
    /// Caveats on items that succeeded (e.g. an ACL that couldn't be copied).
    pub notes: Vec<String>,
    pub elapsed_ms: u128,
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
}

impl Runner<'_> {
    fn emit(&mut self) {
        self.last_tick = Some(Instant::now());
        (self.progress)(&self.state);
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
        if e.kind == FailureKind::Cancelled || self.cancelled() {
            return;
        }
        self.report.failed += 1;
        self.state.failed += 1;
        self.report.failures.push(FileOpFailure {
            path: path.to_owned(),
            kind: e.kind,
            message: e.message,
        });
        self.emit();
    }

    fn skip(&mut self, files: usize) {
        self.report.skipped += files;
        self.state.skipped += files;
        self.state.files_done += files;
    }

    /// Calls `f` with a byte counter that drives progress, pause and cancel.
    fn with_bytes<T>(&mut self, f: impl FnOnce(&mut dyn FnMut(u64) -> io::Result<()>) -> T) -> T {
        let ctl = self.ctl;
        let base = self.state.bytes_done;
        let mut sent = 0u64;
        let result = {
            let state = &mut self.state;
            let progress = &mut *self.progress;
            let last_tick = &mut self.last_tick;
            let mut on_bytes = |n: u64| {
                let mut before = state.phase;
                let go = hold(&ctl.pause, &ctl.cancel, |paused| {
                    if paused {
                        before = state.phase;
                        state.phase = FilePhase::Paused;
                    } else {
                        state.phase = before;
                    }
                    progress(state);
                });
                if !go {
                    return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
                }
                sent += n;
                if last_tick.is_none_or(|t| t.elapsed() >= TICK) {
                    *last_tick = Some(Instant::now());
                    state.bytes_done = base + sent;
                    progress(state);
                }
                Ok(())
            };
            f(&mut on_bytes)
        };
        self.state.bytes_done = base;
        result
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
        };
        if node.is_dir() {
            self.state.current = Some(shown.to_owned());
            self.emit_throttled();
            for (name, meta) in at.list()? {
                if self.cancelled() {
                    break;
                }
                let child = at.child(&name);
                let shown = format!("{shown}/{name}");
                node.children.push(self.scan(&child, name, meta, &shown)?);
            }
        }
        Ok(node)
    }

    // ----- copy and move -----

    /// Copies (or moves) `node` from `src` to `dst`. Returns true when all
    /// of it arrived — for a move, that the source may be removed.
    fn copy_node(
        &mut self,
        src: &Place,
        node: &Node,
        dst: &Place,
        shown: &str,
        moving: bool,
    ) -> bool {
        if !self.start(shown) {
            return false;
        }
        let existing = match dst.stat() {
            Ok(m) => m,
            Err(e) => {
                self.fail(shown, OpError::from(e));
                self.skip_rest(node);
                return false;
            }
        };
        if node.is_dir() {
            return self.copy_dir(src, node, dst, shown, moving, existing);
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
                return false;
            }
            match self.decide(shown, &node.meta, target) {
                Decision::Overwrite => {}
                Decision::Skip => {
                    self.skip(1);
                    return false;
                }
                Decision::Cancel => return false,
            }
        }
        if moving && existing.is_none() && self.rename_within(src, dst) {
            self.succeed(1, node.meta.size);
            return true;
        }
        match self.transfer(src, node, dst, existing.is_some()) {
            Ok(notes) => {
                self.note(shown, notes);
                if moving {
                    if let Err(e) = src.remove_file() {
                        self.fail(
                            shown,
                            OpError::new(
                                classify(&e),
                                format!("copied, but the original couldn't be removed: {e}"),
                            ),
                        );
                        return false;
                    }
                }
                self.succeed(1, node.meta.size);
                true
            }
            Err(e) => {
                self.fail(shown, e);
                false
            }
        }
    }

    fn copy_dir(
        &mut self,
        src: &Place,
        node: &Node,
        dst: &Place,
        shown: &str,
        moving: bool,
        existing: Option<EntryMeta>,
    ) -> bool {
        if existing.as_ref().is_some_and(|m| !m.is_dir) {
            self.fail(
                shown,
                OpError::new(
                    FailureKind::TypeClash,
                    "a file with this name exists at the destination",
                ),
            );
            self.skip_rest(node);
            return false;
        }
        // A move into a free spot is a single rename when the volume allows.
        if moving && existing.is_none() && self.rename_within(src, dst) {
            let (files, bytes) = node.totals();
            self.succeed(files, bytes);
            return true;
        }
        if let Err(e) = dst.create_dir_all() {
            self.fail(shown, OpError::from(e));
            self.skip_rest(node);
            return false;
        }
        let mut all = true;
        for child in &node.children {
            if self.cancelled() {
                return false;
            }
            let ok = self.copy_node(
                &src.child(&child.name),
                child,
                &dst.child(&child.name),
                &format!("{shown}/{}", child.name),
                moving,
            );
            all &= ok;
        }
        if self.cancelled() {
            return false;
        }
        // Folders keep their own times and (Windows) security, like files.
        if let (Place::Local(s), Place::Local(d)) = (src, dst) {
            let notes = fsops::copy_extras(s, d, self.opts.copy_acls, self.opts.copy_short_names);
            self.note(shown, notes);
        }
        if let Some(ms) = node.meta.modified_ms {
            let _ = dst.set_modified(ms);
        }
        if moving && all {
            if let Err(e) = src.remove_empty_dir() {
                self.fail(
                    shown,
                    OpError::new(
                        classify(&e),
                        format!("moved its contents, but the folder couldn't be removed: {e}"),
                    ),
                );
                return false;
            }
        }
        all
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

    /// Copies one file's data (and times) from `src` to `dst`, replacing
    /// what is there if `replace`. Returns caveats worth showing.
    fn transfer(
        &mut self,
        src: &Place,
        node: &Node,
        dst: &Place,
        replace: bool,
    ) -> Result<Vec<String>, OpError> {
        dst.create_parent().map_err(OpError::from)?;
        match (src, dst) {
            (Place::Local(s), Place::Local(d)) => {
                if let Some(target) = &node.meta.link_target {
                    return self.copy_link(s, d, target, replace);
                }
                let temp =
                    self.with_bytes(|on_bytes| fsops::copy_to_temp(s, d, false, on_bytes))?;
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
                    let n = self.with_bytes(|on_bytes| pump(&mut input, &mut out, on_bytes))?;
                    out.sync_all().map_err(|e| OpError::io(&tmp, e))?;
                    drop(out);
                    check_size(n, node.meta.size)?;
                    if let Some(ms) = node.meta.modified_ms {
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
                    Place::Remote { fs: sfs, .. } => sfs.open_at(&src.path(), 0)?,
                };
                self.upload(*fs, dir, name, &mut input, node, replace)
            }
        }
    }

    /// Recreates a symbolic link (local to local).
    fn copy_link(
        &mut self,
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
    /// (the operation's deleter) or for good.
    fn dispose_local(&self, path: &Path) -> Result<(), OpError> {
        let r = if self.opts.trash_replaced {
            self.deleter.delete(path)
        } else {
            PermanentDeleter.delete(path)
        };
        r.map_err(|e| OpError::io(path, e))
    }

    /// Puts the finished temp copy `tmp` in place of `dst`.
    fn install_local(&mut self, dst: &Path, tmp: PathBuf, replace: bool) -> Result<(), OpError> {
        if !replace {
            return fs::rename(&tmp, dst).map_err(|e| {
                let _ = fs::remove_file(&tmp);
                OpError::io(dst, e)
            });
        }
        if self.opts.trash_replaced {
            // The old version goes to the Trash under its own name.
            if let Err(e) = self.dispose_local(dst) {
                let _ = fs::remove_file(&tmp);
                return Err(e);
            }
            return fs::rename(&tmp, dst).map_err(|e| {
                let _ = fs::remove_file(&tmp);
                OpError::new(
                    classify(&e),
                    format!("{e}; the previous version is in the Recycle Bin / Trash"),
                )
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
    fn upload(
        &mut self,
        fs: &dyn VfsWrite,
        dir: &str,
        name: &str,
        input: &mut dyn Read,
        node: &Node,
        replace: bool,
    ) -> Result<Vec<String>, OpError> {
        let caps = fs.caps();
        let dst = fs.join(dir, name);
        let mut notes = Vec::new();
        let stamp = |path: &str, notes: &mut Vec<String>| {
            if let Some(ms) = node.meta.modified_ms.filter(|_| caps.set_modified) {
                if fs.set_modified(path, ms).is_err() {
                    notes.push("the server didn't accept the file's modification time".to_owned());
                }
            }
        };
        if !caps.rename {
            let n = self.with_bytes(|on_bytes| fs.write(&dst, 0, input, on_bytes))?;
            check_size(n, node.meta.size)?;
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
        let sent = self
            .with_bytes(|on_bytes| fs.write(&tmp, 0, input, on_bytes))
            .map_err(OpError::from)
            .and_then(|n| check_size(n, node.meta.size));
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
            Ok(()) => self.succeed(1, 0),
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
    };
    r.emit();
    let items = match req.action {
        FileAction::Rename | FileAction::NewFolder => req.items.iter().take(1).cloned().collect(),
        _ => outermost(&req.items),
    };

    // Resolve every item first, so a bad path fails alone and up front.
    let mut work: Vec<(String, Place, Option<Place>)> = Vec::new();
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
            for (shown, at, to) in work {
                if r.cancelled() {
                    break;
                }
                let node = match at.stat() {
                    Ok(Some(meta)) if deep => r.scan(&at, at.name(), meta, &shown),
                    Ok(Some(meta)) => Ok(Node {
                        name: at.name(),
                        meta,
                        children: Vec::new(),
                    }),
                    Ok(None) => Err(io::Error::new(
                        io::ErrorKind::NotFound,
                        "it no longer exists",
                    )),
                    Err(e) => Err(e),
                };
                match node {
                    Ok(node) => scanned.push((shown, at, to, node)),
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
            let now = now_ms();
            for (shown, at, to, node) in &scanned {
                if r.cancelled() {
                    break;
                }
                match req.action {
                    FileAction::Copy | FileAction::Move => {
                        let to = to.as_ref().expect("copies have a destination");
                        if to.within(at) {
                            let why = if at.within(to) {
                                "the source and the destination are the same"
                            } else {
                                "can't copy a folder into itself"
                            };
                            r.fail(shown, invalid(why));
                            continue;
                        }
                        r.copy_node(at, node, to, shown, req.action == FileAction::Move);
                    }
                    FileAction::Attributes => r.attributes(at, node, shown),
                    _ => r.touch(at, node, to.as_ref(), now, shown),
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
    };
    const S3: WriteCaps = WriteCaps {
        rename: false,
        set_modified: false,
        resume_write: false,
        range_read: true,
        folders: false,
    };

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
}
