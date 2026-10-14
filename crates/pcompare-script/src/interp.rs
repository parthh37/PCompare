//! Runs parsed scripts: loads and compares base folders (local, archives,
//! snapshots and — through the app's [`Host`] — remote profiles), tracks
//! expansion and selection like Beyond Compare's folder view, writes
//! reports, and changes files through pcompare-sync's fail-safe executors
//! (sync plans, and the copy / move / rename / touch / attribute file
//! operations).

use crate::host::{Host, Side};
use crate::parse::{
    AttribFilter, Command, Confirm, Criteria, Cutoff, Direction, Filter, FilterCmd,
    FolderLayout, FolderReport, Load, LogLevel, OutputFormat, OutputTarget, PathMode,
    ScriptOption, SelKind, SelSide, SelStatus, SelectTerm, Sides, SizeLimit, Snapshot,
    SnapshotSource, Statement, Timezone, Touch, When,
};
use pcompare_archive::compare::StoredChecksums;
use pcompare_core::folder::{
    self, attr, ChecksumCache, ContentMethod, EntryStatus, FolderCompareOptions,
    FolderCompareResult, FolderEntry, FolderFilters, ScanControl, Side as EngineSide,
};
use pcompare_core::session::SavedSession;
use pcompare_core::text::TextDiffOptions;
use pcompare_core::vfs::{EmptyFs, ListOptions, Vfs, VfsEntry};
use pcompare_core::PathKind;
use pcompare_report::folder::{
    FolderReportFormat, FolderReportInput, FolderReportOptions, LinkedReports,
};
use pcompare_report::text::TextReportOptions;
use pcompare_sync::exec::{self, ErrorPolicy, ExecOptions, ExecReport};
use pcompare_sync::fileops::{
    self, ConflictAnswer, ConflictMode, Control, FileAction, FileItem, FileOpOptions,
    FileOpReport, FileOpRequest, TouchTime,
};
use pcompare_sync::fsops::{AttrChange, Deleter, PermanentDeleter, TrashDeleter};
use pcompare_sync::journal::NullJournal;
use pcompare_sync::sync::{self, OpKind, PlanOptions, SyncMode, SyncOp};
use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

/// Where a script runs and talks to.
pub struct Env<'a> {
    /// `/silent`: no progress output, and confirmations are answered "no".
    pub silent: bool,
    /// File operations are listed, not performed (reports and logs are
    /// still written).
    pub dry_run: bool,
    /// Relative paths in the script resolve against this folder.
    pub cwd: PathBuf,
    pub out: &'a mut dyn Write,
    pub err: &'a mut dyn Write,
    /// Asks the user a yes/no question (`option confirm:prompt`).
    pub confirm: &'a mut dyn FnMut(&str) -> bool,
    /// Resolves folders, sessions, files and file formats.
    pub host: &'a dyn Host,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Outcome {
    /// Commands that failed.
    pub errors: usize,
    /// The run ended early: `option stop-on-error`, or a `load` failed.
    pub stopped: bool,
    /// A `load` failed, which always ends a script (as in Beyond Compare).
    pub load_failed: bool,
}

/// Beyond Compare's exit code for "unknown error": some commands failed.
pub const EXIT_FAILED: i32 = 100;
/// Beyond Compare's "Script failed to load folders or files".
pub const EXIT_LOAD_FAILED: i32 = 107;

impl Outcome {
    /// 0 = every command succeeded, 107 = a `load` failed (the script
    /// stopped there), 100 = other commands failed.
    pub fn exit_code(&self) -> i32 {
        if self.load_failed {
            EXIT_LOAD_FAILED
        } else if self.errors > 0 {
            EXIT_FAILED
        } else {
            0
        }
    }
}

type CmdResult = Result<(), String>;

pub(crate) struct Folders {
    pub(crate) left: Side,
    pub(crate) right: Side,
    pub(crate) result: FolderCompareResult,
    expand_all: bool,
    expanded: HashSet<String>,
    /// Selected relative paths, in tree order.
    selection: Vec<String>,
}

pub(crate) enum Session {
    None,
    /// Two files: paths as given (resolved against the script's folder).
    Files { left: String, right: String },
    Folders(Box<Folders>),
}

struct Log {
    file: File,
    verbose: bool,
}

/// Filters beyond name masks (`filter cutoff:`, `size:`, `attrib:`).
#[derive(Debug, Clone, Default)]
struct OtherFilters {
    cutoff: Option<Cutoff>,
    size: Option<SizeLimit>,
    attrib: Option<AttribFilter>,
    exclude_protected: bool,
}

pub(crate) struct State {
    pub(crate) criteria: Criteria,
    filter: Filter,
    other: OtherFilters,
    /// Engine settings a loaded session brought along that scripts can't
    /// set themselves (exclusions, text rules, alignment …).
    base: FolderCompareOptions,
    confirm: Confirm,
    stop_on_error: bool,
    recycle: bool,
    pub(crate) session: Session,
    log: Option<Log>,
}

fn fresh_state() -> State {
    State {
        criteria: Criteria::default(),
        filter: Filter::default(),
        other: OtherFilters::default(),
        base: FolderCompareOptions::default(),
        confirm: Confirm::Prompt,
        stop_on_error: false,
        recycle: true,
        session: Session::None,
        log: None,
    }
}

/// Runs `stmts` in order. Failing commands are reported on `env.err` (and
/// the log) and the script goes on unless `option stop-on-error` is set or
/// a `load` failed.
pub fn run(stmts: &[Statement], env: &mut Env) -> Outcome {
    let mut st = fresh_state();
    let mut outcome = Outcome {
        errors: 0,
        stopped: false,
        load_failed: false,
    };
    for s in stmts {
        info(&mut st, env, &format!("> {}", s.text));
        if let Err(msg) = execute(&mut st, env, &s.command) {
            outcome.errors += 1;
            error(&mut st, env, &format!("line {}: {msg}", s.line));
            if matches!(s.command, Command::Load(_)) {
                outcome.load_failed = true;
                outcome.stopped = true;
                info(&mut st, env, "Script stopped: the folders could not be loaded");
                break;
            }
            if st.stop_on_error {
                outcome.stopped = true;
                info(&mut st, env, "Script stopped (option stop-on-error)");
                break;
            }
        }
    }
    let summary = match outcome.errors {
        0 => "Script completed".to_owned(),
        1 => "Script completed with 1 error".to_owned(),
        n => format!("Script completed with {n} errors"),
    };
    info(&mut st, env, &summary);
    outcome
}

// ---------- output ----------

fn stamp() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

fn write_log(st: &mut State, line: &str) {
    if let Some(log) = &mut st.log {
        let _ = writeln!(log.file, "{} {line}", stamp());
    }
}

/// Progress for the console (unless silent) and the log.
pub(crate) fn info(st: &mut State, env: &mut Env, msg: &str) {
    if !env.silent {
        let _ = writeln!(env.out, "{msg}");
    }
    write_log(st, msg);
}

/// Per-item detail: only in a verbose log.
pub(crate) fn detail(st: &mut State, msg: &str) {
    if st.log.as_ref().is_some_and(|l| l.verbose) {
        write_log(st, &format!("  {msg}"));
    }
}

fn error(st: &mut State, env: &mut Env, msg: &str) {
    let _ = writeln!(env.err, "error: {msg}");
    write_log(st, &format!("ERROR {msg}"));
}

/// A script path made absolute against the script's folder; URLs and
/// `remote://` / `svn:` paths stay as written.
pub(crate) fn resolve(env: &Env, p: &str) -> String {
    if p.contains("://") || pcompare_archive::is_repo_path(p) {
        return p.to_owned();
    }
    let path = Path::new(p);
    if path.is_absolute() {
        p.to_owned()
    } else {
        env.cwd.join(path).to_string_lossy().into_owned()
    }
}

// ---------- commands ----------

fn execute(st: &mut State, env: &mut Env, cmd: &Command) -> CmdResult {
    match cmd {
        Command::Log {
            level,
            append,
            path,
        } => set_log(st, env, *level, *append, path.as_deref()),
        Command::Criteria(c) => {
            st.criteria = c.clone();
            recompare_if_loaded(st, env)
        }
        Command::Filter(f) => {
            apply_filter(st, f);
            recompare_if_loaded(st, env)
        }
        Command::Load(l) => load(st, env, l),
        Command::Expand(targets) => {
            let f = folders(st)?;
            if targets.is_empty() {
                f.expand_all = true;
            }
            for p in targets {
                if !is_dir(&f.result.entries, p) {
                    return Err(format!("no folder `{p}` in the comparison"));
                }
                // Expanding a nested folder opens its parents too.
                let mut cur = p.as_str();
                loop {
                    f.expanded.insert(cur.to_owned());
                    match cur.rsplit_once('/') {
                        Some((parent, _)) => cur = parent,
                        None => break,
                    }
                }
            }
            Ok(())
        }
        Command::Collapse(targets) => {
            let f = folders(st)?;
            if targets.is_empty() {
                f.expand_all = false;
                f.expanded.clear();
            }
            for p in targets {
                if f.expand_all {
                    f.expand_all = false;
                    f.expanded = all_dirs(&f.result.entries);
                }
                f.expanded.remove(p);
            }
            Ok(())
        }
        Command::Select(terms) => {
            let f = folders(st)?;
            let mut sel = Vec::new();
            visit_shown(&f.result.entries, f, &mut |e| {
                if terms.iter().any(|t| term_matches(t, e)) {
                    sel.push(e.rel_path.clone());
                }
            });
            f.selection = sel;
            let n = f.selection.len();
            info(
                st,
                env,
                &format!("Selected {n} item{}", if n == 1 { "" } else { "s" }),
            );
            Ok(())
        }
        Command::Compare(method) => compare_selection(st, env, *method),
        Command::FolderReport(r) => folder_report(st, env, r),
        Command::FileReport(r) => crate::report::file_report(st, env, r),
        Command::Sync {
            mode,
            create_empty,
            visible,
        } => sync_folders(st, env, *mode, *create_empty, *visible),
        Command::Copy {
            dir,
            remove_source: false,
        } => copy_selection(st, env, *dir),
        Command::Copy {
            dir,
            remove_source: true,
        } => move_selection(st, env, *dir),
        Command::CopyTo {
            sides,
            path,
            target,
            remove_source,
        } => copy_to(st, env, *sides, *path, target, *remove_source),
        Command::Delete { sides, recycle } => delete_selection(st, env, *sides, *recycle),
        Command::Rename { regex, new } => rename_selection(st, env, regex.as_deref(), new),
        Command::Touch(t) => touch_selection(st, env, t),
        Command::Attrib { set, clear } => attrib_selection(st, env, *set, *clear),
        Command::Snapshot(s) => snapshot(st, env, s),
        Command::Beep => {
            // Beyond Compare beeps even with /silent.
            let _ = env.out.write_all(b"\x07");
            let _ = env.out.flush();
            Ok(())
        }
        Command::Option(o) => {
            match o {
                ScriptOption::Confirm(c) => st.confirm = *c,
                ScriptOption::StopOnError(on) => st.stop_on_error = *on,
                ScriptOption::Recycle(on) => st.recycle = *on,
            }
            Ok(())
        }
    }
}

fn set_log(
    st: &mut State,
    env: &mut Env,
    level: LogLevel,
    append: bool,
    path: Option<&str>,
) -> CmdResult {
    if level == LogLevel::None {
        write_log(st, "Log stopped");
        st.log = None;
        return Ok(());
    }
    let path = PathBuf::from(resolve(env, path.unwrap_or("Log.txt")));
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .append(append)
        .truncate(!append)
        .open(&path)
        .map_err(|e| format!("cannot open log {}: {e}", path.display()))?;
    let verbose = level == LogLevel::Verbose;
    st.log = Some(Log { file, verbose });
    write_log(
        st,
        &format!(
            "Log started ({})",
            if verbose { "verbose" } else { "normal" }
        ),
    );
    Ok(())
}

fn apply_filter(st: &mut State, f: &FilterCmd) {
    if let Some(names) = &f.names {
        st.filter = names.clone();
    }
    if let Some(c) = f.cutoff {
        st.other.cutoff = c;
    }
    if let Some(s) = f.size {
        st.other.size = s;
    }
    if let Some(a) = f.attrib {
        st.other.attrib = a;
    }
    if let Some(p) = f.exclude_protected {
        st.other.exclude_protected = p;
    }
}

pub(crate) fn folders(st: &mut State) -> Result<&mut Folders, String> {
    match &mut st.session {
        Session::Folders(f) => Ok(f),
        Session::Files { .. } => Err("this command needs two folders loaded, not files".into()),
        Session::None => Err("nothing loaded (use load <left> <right> first)".into()),
    }
}

fn when_ms(w: When) -> i64 {
    match w {
        When::At(ms) => ms,
        When::DaysAgo(n) => chrono::Local::now().timestamp_millis() - i64::from(n) * 86_400_000,
    }
}

fn compare_options(st: &State) -> FolderCompareOptions {
    let c = &st.criteria;
    let o = &st.other;
    let mut opts = st.base.clone();
    opts.compare_size = c.size;
    opts.compare_timestamp = c.timestamp;
    opts.time_tolerance_secs = c.tolerance_secs;
    opts.ignore_dst_shift = c.ignore_dst;
    opts.content = c.content;
    opts.compare_attributes = c.attributes;
    opts.follow_links = c.follow_links;
    opts.recursive = true;
    // The script's own size / date / attribute filters, else a loaded session's.
    let b = &st.base.filters;
    opts.filters = FolderFilters {
        include_files: st.filter.include_files.clone(),
        exclude_files: st.filter.exclude_files.clone(),
        exclude_folders: st.filter.exclude_folders.clone(),
        min_size: o.size.filter(|s| s.smaller).map(|s| s.bytes).or(b.min_size),
        max_size: o.size.filter(|s| !s.smaller).map(|s| s.bytes).or(b.max_size),
        modified_after_ms: o
            .cutoff
            .filter(|c| c.older)
            .map(|c| when_ms(c.when))
            .or(b.modified_after_ms),
        modified_before_ms: o
            .cutoff
            .filter(|c| !c.older)
            .map(|c| when_ms(c.when))
            .or(b.modified_before_ms),
        exclude_attributes: o.attrib.map_or(0, |a| a.excluded) | b.exclude_attributes,
        ..b.clone()
    };
    opts
}

/// A file system whose timestamps are shifted by a fixed amount
/// (`criteria timezone:left+6`).
struct ShiftedFs {
    inner: Arc<dyn Vfs>,
    shift_ms: i64,
}

impl ShiftedFs {
    fn shift(&self, mut list: Vec<VfsEntry>) -> Vec<VfsEntry> {
        for e in &mut list {
            e.meta.modified_ms = e.meta.modified_ms.map(|m| m + self.shift_ms);
        }
        list
    }
}

impl Vfs for ShiftedFs {
    fn read_dir(&self, dir: &str) -> pcompare_core::Result<Vec<VfsEntry>> {
        self.inner.read_dir(dir).map(|l| self.shift(l))
    }
    fn read_dir_with(&self, dir: &str, opts: ListOptions) -> pcompare_core::Result<Vec<VfsEntry>> {
        self.inner.read_dir_with(dir, opts).map(|l| self.shift(l))
    }
    fn open(&self, path: &str) -> pcompare_core::Result<Box<dyn Read + Send>> {
        self.inner.open(path)
    }
    fn join(&self, dir: &str, name: &str) -> String {
        self.inner.join(dir, name)
    }
    fn canonical(&self, path: &str) -> Option<String> {
        self.inner.canonical(path)
    }
}

fn compare(st: &State, left: &Side, right: &Side) -> Result<FolderCompareResult, String> {
    let opts = pcompare_archive::compare::options_for_snapshots(
        &compare_options(st),
        [left.snapshot, right.snapshot],
    )
    .0;
    let shifted = |side: &Side, left_side: bool| -> Arc<dyn Vfs> {
        match st.criteria.timezone {
            Timezone::Offset { left, hours } if left == left_side && hours != 0 => {
                Arc::new(ShiftedFs {
                    inner: side.fs.clone(),
                    shift_ms: i64::from(hours) * 3_600_000,
                })
            }
            _ => side.fs.clone(),
        }
    };
    let (lfs, rfs) = (shifted(left, true), shifted(right, false));
    let stored = (left.crcs.is_some() || right.crcs.is_some()).then(|| StoredChecksums {
        sides: [left.crcs.clone(), right.crcs.clone()],
        inner: None,
    });
    let control = ScanControl {
        cache: stored.as_ref().map(|s| s as &dyn ChecksumCache),
        ..Default::default()
    };
    let mut result =
        folder::compare_folders_controlled(&*lfs, &left.path, &*rfs, &right.path, &opts, control)
            .map_err(|e| e.to_string())?;
    result.left_root = left.shown.clone();
    result.right_root = right.shown.clone();
    post_filter(st, &mut result.entries);
    if st.criteria.timezone == Timezone::Ignore {
        ignore_timezones(&mut result.entries, &opts);
    }
    result.stats = retally(&result.entries);
    Ok(result)
}

/// The filters the engine has no setting for: required attributes
/// (`attrib:+r`) and `exclude-protected` (system and hidden together).
fn post_filter(st: &State, entries: &mut Vec<FolderEntry>) {
    let required = st.other.attrib.map_or(0, |a| a.required);
    let protected = st.other.exclude_protected;
    if required == 0 && !protected {
        return;
    }
    let attrs = |e: &FolderEntry| -> [u32; 2] {
        [&e.left, &e.right].map(|m| m.as_ref().and_then(|m| m.attributes).unwrap_or(0))
    };
    let keep = |e: &FolderEntry| -> bool {
        let a = attrs(e);
        let present = [e.left.is_some(), e.right.is_some()];
        let both = attr::SYSTEM | attr::HIDDEN;
        if protected && (0..2).any(|i| present[i] && a[i] & both == both) {
            return false;
        }
        e.is_dir || required == 0 || (0..2).any(|i| present[i] && a[i] & required == required)
    };
    fn walk(list: &mut Vec<FolderEntry>, keep: &dyn Fn(&FolderEntry) -> bool) {
        list.retain(|e| keep(e));
        for e in list.iter_mut().filter(|e| e.is_dir) {
            walk(&mut e.children, keep);
        }
    }
    walk(entries, &keep);
    refresh_folder_status(entries);
}

/// `timezone:ignore`: file pairs that differ only by a whole number of
/// hours (up to 14, within the tolerance) count as the same.
fn ignore_timezones(entries: &mut [FolderEntry], opts: &FolderCompareOptions) {
    let tol = opts.time_tolerance_secs as i64 * 1000;
    for e in entries.iter_mut() {
        if e.is_dir {
            ignore_timezones(&mut e.children, opts);
            continue;
        }
        let (Some(l), Some(r)) = (&e.left, &e.right) else {
            continue;
        };
        let (Some(lm), Some(rm)) = (l.modified_ms, r.modified_ms) else {
            continue;
        };
        let delta = (lm - rm).abs();
        let hours = (delta + 1_800_000) / 3_600_000;
        let hour_shift = (1..=14).contains(&hours) && (delta - hours * 3_600_000).abs() <= tol;
        let same_size = !opts.compare_size || l.size == r.size;
        if hour_shift && same_size && e.status == EntryStatus::Different && e.content_checked.is_none() {
            e.status = EntryStatus::Same;
            e.newer = None;
        }
    }
    refresh_folder_status(entries);
}

/// Recomputes the status of folders from their children after rows were
/// removed or changed.
fn refresh_folder_status(entries: &mut [FolderEntry]) {
    for e in entries.iter_mut().filter(|e| e.is_dir) {
        refresh_folder_status(&mut e.children);
        let keep = e.children.is_empty()
            || matches!(
                e.status,
                EntryStatus::LeftOnly | EntryStatus::RightOnly | EntryStatus::Error
            );
        if !keep {
            e.status = folder::folder_status(&e.children);
        }
    }
}

fn stats_line(r: &FolderCompareResult) -> String {
    let s = &r.stats;
    format!(
        "same {}, similar {}, different {}, left orphans {}, right orphans {}, errors {}",
        s.same, s.similar, s.different, s.left_only, s.right_only, s.errors
    )
}

/// A side with nothing loaded (`load <left>` alone).
fn empty_side() -> Side {
    Side {
        shown: String::new(),
        fs: Arc::new(EmptyFs),
        path: String::new(),
        local: None,
        write: None,
        crcs: None,
        snapshot: None,
    }
}

fn load(st: &mut State, env: &mut Env, l: &Load) -> CmdResult {
    let (create, left, right) = match l {
        Load::Default => {
            let log = st.log.take();
            *st = fresh_state();
            st.log = log;
            info(st, env, "Loaded default settings");
            return Ok(());
        }
        Load::Paths {
            create,
            left,
            right,
        } => (*create, left, right),
    };
    // One argument naming a saved session loads the session.
    if right.is_none() && create.is_none() {
        if let Some(s) = env.host.session(left) {
            return load_session(st, env, &s);
        }
    }
    let lp = resolve(env, left);
    let rp = right.as_ref().map(|r| resolve(env, r));
    // A dry run doesn't create folders; they load as empty sides.
    let mut phantom = [false; 2];
    for (i, path, side) in [(0, Some(&lp), Sides::Left), (1, rp.as_ref(), Sides::Right)] {
        let Some(path) = path else { continue };
        let wanted = create.is_some_and(|c| c == side || c == Sides::Both);
        if wanted && env.host.kind(path) == PathKind::Missing {
            if env.dry_run {
                info(st, env, &format!("Would create {path}"));
                phantom[i] = true;
            } else {
                env.host.create_folder(path)?;
                info(st, env, &format!("Created {path}"));
            }
        }
    }
    load_sides(st, env, &lp, rp.as_deref(), phantom)
}

fn load_paths(st: &mut State, env: &mut Env, lp: &str, rp: Option<&str>) -> CmdResult {
    load_sides(st, env, lp, rp, [false; 2])
}

fn load_sides(
    st: &mut State,
    env: &mut Env,
    lp: &str,
    rp: Option<&str>,
    phantom: [bool; 2],
) -> CmdResult {
    let kind = |p: &str, phantom: bool| {
        if phantom {
            PathKind::Directory
        } else {
            env.host.kind(p)
        }
    };
    let kinds = (
        kind(lp, phantom[0]),
        rp.map_or(PathKind::Directory, |r| kind(r, phantom[1])),
    );
    // Remote folders can't be checked cheaply; they load as folders.
    let missing = |p: &str, k: PathKind| k == PathKind::Missing && !p.contains("://");
    if missing(lp, kinds.0) {
        return Err(format!("{lp}: no such file or folder"));
    }
    if let Some(r) = rp.filter(|r| missing(r, kinds.1)) {
        return Err(format!("{r}: no such file or folder"));
    }
    match (kinds, rp) {
        ((PathKind::File, PathKind::File), Some(r)) => {
            st.session = Session::Files {
                left: lp.to_owned(),
                right: r.to_owned(),
            };
            info(st, env, &format!("Loaded files {lp} and {r}"));
            Ok(())
        }
        ((PathKind::File, _), None) => Err(format!(
            "{lp} is a file: load two files, or a folder to compare"
        )),
        ((PathKind::File, _), _) | ((_, PathKind::File), Some(_)) => {
            Err("a file and a folder can't be compared".into())
        }
        _ => {
            let open = |p: &str, phantom: bool| -> Result<Side, String> {
                if phantom {
                    Ok(Side {
                        shown: p.to_owned(),
                        ..empty_side()
                    })
                } else {
                    env.host.folder(p)
                }
            };
            let left = open(lp, phantom[0])?;
            let right = match rp {
                Some(r) => open(r, phantom[1])?,
                None => empty_side(),
            };
            let result = compare(st, &left, &right)?;
            info(
                st,
                env,
                &format!("Compared folders: {}", stats_line(&result)),
            );
            st.session = Session::Folders(Box::new(Folders {
                left,
                right,
                result,
                expand_all: false,
                expanded: HashSet::new(),
                selection: Vec::new(),
            }));
            Ok(())
        }
    }
}

/// Session kinds that hold two folders.
const FOLDER_KINDS: &[&str] = &["folder", "sync", "folderMerge"];

pub(crate) fn is_folder_kind(kind: &str) -> bool {
    FOLDER_KINDS.contains(&kind)
}

fn load_session(st: &mut State, env: &mut Env, s: &SavedSession) -> CmdResult {
    info(st, env, &format!("Loading session \"{}\"", s.name));
    if FOLDER_KINDS.contains(&s.kind.as_str()) {
        apply_session_settings(st, &s.settings);
    }
    let (l, r) = (resolve(env, &s.left), resolve(env, &s.right));
    load_paths(st, env, &l, Some(&r).filter(|r| !r.is_empty()).map(String::as_str))
}

/// Takes a saved folder session's settings (the UI's `FolderSettings`) as
/// the current criteria and filters.
fn apply_session_settings(st: &mut State, v: &serde_json::Value) {
    let b = |k: &str| v.get(k).and_then(serde_json::Value::as_bool);
    let n = |k: &str| v.get(k).and_then(serde_json::Value::as_f64);
    let s = |k: &str| v.get(k).and_then(serde_json::Value::as_str).unwrap_or("");
    let list = |k: &str| -> Vec<String> {
        s(k).split([',', ';', '\n'])
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map(String::from)
            .collect()
    };
    let date = |k: &str| crate::parse::parse_timestamp(s(k));
    let c = &mut st.criteria;
    c.size = b("compareSize").unwrap_or(c.size);
    c.timestamp = b("compareTimestamp").unwrap_or(c.timestamp);
    c.tolerance_secs = n("timeToleranceSecs").map_or(c.tolerance_secs, |t| t as u64);
    c.ignore_dst = b("ignoreDstShift").unwrap_or(c.ignore_dst);
    c.attributes = b("compareAttributes").unwrap_or(c.attributes);
    c.follow_links = b("followLinks").unwrap_or(c.follow_links);
    c.content = match s("content") {
        "binary" => ContentMethod::Binary,
        "crc" => ContentMethod::Crc,
        "rules" => ContentMethod::Rules,
        "version" => ContentMethod::Version,
        "off" => ContentMethod::Off,
        _ => c.content,
    };
    st.filter = Filter {
        include_files: list("includeFiles"),
        exclude_files: list("excludeFiles"),
        exclude_folders: list("excludeFolders"),
    };
    st.other = OtherFilters::default();
    let base = &mut st.base;
    base.exclude = list("exclude");
    base.include_hidden = b("includeHidden").unwrap_or(base.include_hidden);
    if let Some(cs) = b("caseSensitiveNames") {
        base.ignore_name_case = !cs;
    }
    base.override_quick_tests = b("overrideQuickTests").unwrap_or(base.override_quick_tests);
    if let Some(rules) = v.get("rules") {
        if let Ok(r) = serde_json::from_value::<TextDiffOptions>(rules.clone()) {
            base.rules = r;
        }
    }
    base.filters = FolderFilters {
        min_size: n("minSizeKb").map(|k| (k * 1024.0) as u64),
        max_size: n("maxSizeKb").map(|k| (k * 1024.0) as u64),
        modified_after_ms: date("modifiedAfter"),
        modified_before_ms: date("modifiedBefore"),
        exclude_attributes: n("excludeAttributes").map_or(0, |a| a as u32),
        exclude_executable: b("excludeExecutable").unwrap_or(false),
        include_containing: s("includeContaining").to_owned(),
        exclude_containing: s("excludeContaining").to_owned(),
        containing_regex: b("containingRegex").unwrap_or(false),
        containing_match_case: b("containingMatchCase").unwrap_or(false),
        ..FolderFilters::default()
    };
}

/// Compares the loaded folders again (after criteria, filters or file
/// operations changed); expansion survives, and so do selected items that
/// are still there.
fn recompare_if_loaded(st: &mut State, env: &mut Env) -> CmdResult {
    let Session::Folders(f) = &st.session else {
        return Ok(());
    };
    let result = compare(st, &f.left, &f.right)?;
    let line = format!("Compared folders: {}", stats_line(&result));
    if let Session::Folders(f) = &mut st.session {
        f.result = result;
        let entries = &f.result.entries;
        f.selection.retain(|p| find(entries, p).is_some());
    }
    info(st, env, &line);
    Ok(())
}

// ---------- tree helpers ----------

fn find<'a>(entries: &'a [FolderEntry], rel: &str) -> Option<&'a FolderEntry> {
    for e in entries {
        if e.rel_path == rel {
            return Some(e);
        }
        if e.is_dir && rel.starts_with(&format!("{}/", e.rel_path)) {
            return find(&e.children, rel);
        }
    }
    None
}

fn is_dir(entries: &[FolderEntry], rel: &str) -> bool {
    find(entries, rel).is_some_and(|e| e.is_dir)
}

fn all_dirs(entries: &[FolderEntry]) -> HashSet<String> {
    let mut out = HashSet::new();
    fn walk(list: &[FolderEntry], out: &mut HashSet<String>) {
        for e in list.iter().filter(|e| e.is_dir) {
            out.insert(e.rel_path.clone());
            walk(&e.children, out);
        }
    }
    walk(entries, &mut out);
    out
}

/// Every entry the folder view shows, given the expansion state.
fn visit_shown(entries: &[FolderEntry], f: &Folders, visit: &mut dyn FnMut(&FolderEntry)) {
    for e in entries {
        visit(e);
        if e.is_dir && (f.expand_all || f.expanded.contains(&e.rel_path)) {
            visit_shown(&e.children, f, visit);
        }
    }
}

/// Whether every folder above `rel` is expanded (the row is on screen).
fn is_shown(f: &Folders, rel: &str) -> bool {
    if f.expand_all {
        return true;
    }
    let mut cur = rel;
    while let Some((parent, _)) = cur.rsplit_once('/') {
        if !f.expanded.contains(parent) {
            return false;
        }
        cur = parent;
    }
    true
}

fn has_files(e: &FolderEntry) -> bool {
    e.children.iter().any(|c| !c.is_dir || has_files(c))
}

fn term_matches(t: &SelectTerm, e: &FolderEntry) -> bool {
    let kind = match t.kind {
        SelKind::Files => !e.is_dir,
        SelKind::Folders => e.is_dir,
        SelKind::Both => true,
    };
    let side = match t.side {
        SelSide::Left => e.left.is_some(),
        SelSide::Right => e.right.is_some(),
        SelSide::Any => true,
    };
    let status = match t.status {
        SelStatus::All => true,
        SelStatus::Diff => !matches!(e.status, EntryStatus::Same | EntryStatus::Similar),
        SelStatus::Same => matches!(e.status, EntryStatus::Same | EntryStatus::Similar),
        SelStatus::Exact => e.status == EntryStatus::Same,
        SelStatus::Orphan => match t.side {
            SelSide::Left => e.status == EntryStatus::LeftOnly,
            SelSide::Right => e.status == EntryStatus::RightOnly,
            SelSide::Any => matches!(e.status, EntryStatus::LeftOnly | EntryStatus::RightOnly),
        },
        SelStatus::Newer => match t.side {
            SelSide::Left => e.newer == Some(EngineSide::Left),
            SelSide::Right => e.newer == Some(EngineSide::Right),
            SelSide::Any => e.newer.is_some(),
        },
        SelStatus::Older => match t.side {
            SelSide::Left => e.newer == Some(EngineSide::Right),
            SelSide::Right => e.newer == Some(EngineSide::Left),
            SelSide::Any => e.newer.is_some(),
        },
        SelStatus::Empty => e.is_dir && !has_files(e),
    };
    kind && side && status
}

/// Selected entries that aren't inside another selected folder.
fn selection_roots(f: &Folders) -> Result<Vec<&FolderEntry>, String> {
    if f.selection.is_empty() {
        return Err("nothing is selected (use select first)".into());
    }
    let dirs: Vec<&String> = f
        .selection
        .iter()
        .filter(|p| is_dir(&f.result.entries, p))
        .collect();
    Ok(f.selection
        .iter()
        .filter(|p| !dirs.iter().any(|d| p.starts_with(&format!("{d}/"))))
        .filter_map(|p| find(&f.result.entries, p))
        .collect())
}

/// Files below the selection (selected files, and everything in selected
/// folders) that exist on both sides.
pub(crate) fn selected_pairs(f: &Folders) -> Result<Vec<&FolderEntry>, String> {
    let mut out = Vec::new();
    fn walk<'a>(e: &'a FolderEntry, out: &mut Vec<&'a FolderEntry>) {
        if e.is_dir {
            e.children.iter().for_each(|c| walk(c, out));
        } else if e.left.is_some() && e.right.is_some() && e.status != EntryStatus::TypeMismatch {
            out.push(e);
        }
    }
    for e in selection_roots(f)? {
        walk(e, &mut out);
    }
    Ok(out)
}

pub(crate) fn left_rel(e: &FolderEntry) -> &str {
    e.left_rel_path.as_deref().unwrap_or(&e.rel_path)
}

// ---------- compare ----------

fn compare_selection(st: &mut State, env: &mut Env, method: Option<ContentMethod>) -> CmdResult {
    let method = method.unwrap_or(match st.criteria.content {
        ContentMethod::Off => ContentMethod::Rules,
        m => m,
    });
    let rules = st.base.rules.clone();
    let f = folders(st)?;
    let mut verdicts = HashMap::new();
    let mut failures = Vec::new();
    for e in selected_pairs(f)? {
        let (lp, rp) = (f.left.join(left_rel(e)), f.right.join(&e.rel_path));
        let (ls, rs) = (
            e.left.as_ref().map_or(0, |m| m.size),
            e.right.as_ref().map_or(0, |m| m.size),
        );
        let stored = |side: &Side, p: &str| side.crcs.as_ref().and_then(|c| c.stored_crc(p));
        let verdict = if f.left.snapshot.is_some() || f.right.snapshot.is_some() {
            // Snapshots hold no contents: only their stored CRCs compare.
            let crc = |side: &Side, p: &str| match stored(side, p) {
                Some(c) => Ok(c),
                None if side.snapshot.is_some() => Err(format!(
                    "{}: the snapshot holds no CRCs (save it with save-crc)",
                    e.rel_path
                )),
                None => pcompare_archive::snapshot::crc32(&*side.fs, p).map_err(|x| x.to_string()),
            };
            crc(&f.left, &lp).and_then(|l| {
                let r = crc(&f.right, &rp)?;
                Ok(folder::ContentVerdict {
                    status: if l == r && ls == rs {
                        EntryStatus::Same
                    } else {
                        EntryStatus::Different
                    },
                    crc: Some([l, r]),
                })
            })
        } else {
            folder::compare_contents(&*f.left.fs, &lp, ls, &*f.right.fs, &rp, rs, method, &rules)
                .map_err(|err| format!("{}: {err}", e.rel_path))
        };
        match verdict {
            Ok(v) => {
                verdicts.insert(e.rel_path.clone(), v);
            }
            Err(err) => failures.push(err),
        }
    }
    let checked = verdicts.len();
    let differ = verdicts
        .values()
        .filter(|v| v.status == EntryStatus::Different)
        .count();
    apply_verdicts(&mut f.result.entries, &verdicts, method);
    f.result.stats = retally(&f.result.entries);
    info(
        st,
        env,
        &format!("Compared contents of {checked} file pair(s): {differ} differ"),
    );
    if failures.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "{} file pair(s) could not be compared: {}",
            failures.len(),
            failures.join("; ")
        ))
    }
}

/// Applies content verdicts and recomputes the status of every folder above them.
fn apply_verdicts(
    entries: &mut [FolderEntry],
    verdicts: &HashMap<String, folder::ContentVerdict>,
    method: ContentMethod,
) {
    for e in entries.iter_mut() {
        if e.is_dir {
            apply_verdicts(&mut e.children, verdicts, method);
        } else if let Some(v) = verdicts.get(&e.rel_path) {
            e.status = v.status;
            e.crc = v.crc.or(e.crc);
            e.content_checked = Some(method);
        }
    }
    refresh_folder_status(entries);
}

fn retally(entries: &[FolderEntry]) -> folder::FolderStats {
    let mut s = folder::FolderStats::default();
    fn walk(list: &[FolderEntry], s: &mut folder::FolderStats) {
        for e in list {
            if !e.is_dir {
                match e.status {
                    EntryStatus::Same => s.same += 1,
                    EntryStatus::Similar => s.similar += 1,
                    EntryStatus::Different | EntryStatus::TypeMismatch => s.different += 1,
                    EntryStatus::LeftOnly => s.left_only += 1,
                    EntryStatus::RightOnly => s.right_only += 1,
                    EntryStatus::Error => s.errors += 1,
                }
            } else if e.status == EntryStatus::Error {
                s.errors += 1;
            }
            walk(&e.children, s);
        }
    }
    walk(entries, &mut s);
    s
}

// ---------- reports ----------

/// The file a report is written to, its folder created.
pub(crate) fn prepare_output(env: &Env, target: &OutputTarget) -> Result<PathBuf, String> {
    let path = match target {
        OutputTarget::File(p) => p,
        OutputTarget::Printer => {
            return Err(
                "output-to:printer needs the app window; write the report to a file and print that"
                    .into(),
            )
        }
        OutputTarget::Clipboard => {
            return Err(
                "output-to:clipboard needs the app window; write the report to a file instead"
                    .into(),
            )
        }
    };
    let out = PathBuf::from(resolve(env, path));
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    Ok(out)
}

fn folder_report(st: &mut State, env: &mut Env, r: &FolderReport) -> CmdResult {
    let out = prepare_output(env, &r.output)?;
    let host = env.host;
    let rules = st.base.rules.clone();
    folders(st)?;
    for (wanted, name) in [
        (r.columns.version, "version"),
        (r.columns.owner, "owner"),
        (r.columns.group, "group"),
        (r.columns.vcs, "vcs"),
    ] {
        if wanted {
            detail(st, &format!("column-{name} isn't available in PCompare reports"));
        }
    }
    let f = folders(st)?;
    let opts = FolderReportOptions {
        layout: match r.layout {
            FolderLayout::Summary => pcompare_report::folder::FolderLayout::Summary,
            FolderLayout::SideBySide | FolderLayout::Xml => {
                pcompare_report::folder::FolderLayout::Detailed
            }
        },
        format: match r.format {
            OutputFormat::HtmlColor | OutputFormat::HtmlMono => FolderReportFormat::Html,
            OutputFormat::Text => FolderReportFormat::Text,
            OutputFormat::Csv => FolderReportFormat::Csv,
            OutputFormat::Xml => FolderReportFormat::Xml,
        },
        filter: r.filter,
        name_filter: String::new(),
        show_size: r.columns.size,
        show_modified: r.columns.timestamp,
        show_crc: r.columns.crc,
        show_attributes: r.columns.attributes,
        mono: r.format == OutputFormat::HtmlMono,
        title: r.title.clone(),
        expanded: (!f.expand_all).then(|| f.expanded.clone()),
    };
    let input = FolderReportInput::from(&f.result);
    let text = TextReportOptions {
        line_numbers: true,
        ..Default::default()
    };
    let local = |p: &str| {
        host.local_file(p)
            .map_err(pcompare_core::CoreError::Other)
    };
    let linked = LinkedReports {
        text: &text,
        rules: &rules,
        local: &local,
    };
    let written = pcompare_report::folder::write_folder_report(
        &input,
        &opts,
        &pcompare_report::now_stamp(),
        &out,
        r.file_links.then_some(&linked),
    )
    .map_err(|e| e.to_string())?;
    let mut msg = format!("Wrote folder report {}", out.display());
    if r.file_links {
        msg.push_str(&format!(" with {} linked file report(s)", written.linked));
    }
    info(st, env, &msg);
    for s in &written.skipped {
        detail(st, &format!("no file report for {s}"));
    }
    Ok(())
}

// ---------- file operations ----------

fn mode_label(mode: SyncMode) -> &'static str {
    match mode {
        SyncMode::UpdateLeftToRight => "update left->right",
        SyncMode::UpdateRightToLeft => "update right->left",
        SyncMode::UpdateBoth => "update all",
        SyncMode::MirrorLeftToRight => "mirror left->right",
        SyncMode::MirrorRightToLeft => "mirror right->left",
        SyncMode::TwoWay => "two-way",
    }
}

fn sync_folders(
    st: &mut State,
    env: &mut Env,
    mode: SyncMode,
    create_empty: bool,
    visible: bool,
) -> CmdResult {
    let opts = compare_options(st);
    let f = folders(st)?;
    let mut plan = sync::plan_from_comparison_with(&f.result, mode, &opts, &PlanOptions::default());
    if !create_empty {
        // Copies create their parent folders; only empty ones would be left over.
        plan.ops.retain(|o| o.kind != OpKind::Mkdir);
    }
    if visible {
        plan.ops.retain(|o| is_shown(f, &o.rel_path));
    }
    let skipped: Vec<String> = plan
        .skipped
        .iter()
        .filter(|s| s.conflict)
        .map(|s| format!("{}: {}", s.rel_path, s.reason))
        .collect();
    for s in &skipped {
        detail(st, &format!("skipped {s}"));
    }
    let label = format!("Sync {}", mode_label(mode));
    let result = execute_ops(st, env, plan.ops, &label);
    if !skipped.is_empty() {
        info(
            st,
            env,
            &format!("{label}: {} conflict(s) left alone", skipped.len()),
        );
    }
    result
}

/// Ops that copy the selection in `dir` (whole folders included).
fn copy_ops(f: &Folders, dir: Direction) -> Result<(Vec<SyncOp>, Vec<String>), String> {
    let (to, from_left) = match dir {
        Direction::LeftToRight => (sync::Direction::ToRight, true),
        Direction::RightToLeft => (sync::Direction::ToLeft, false),
    };
    let mut ops = Vec::new();
    let mut skipped = Vec::new();
    fn walk(
        e: &FolderEntry,
        to: sync::Direction,
        from_left: bool,
        ops: &mut Vec<SyncOp>,
        skipped: &mut Vec<String>,
    ) {
        let (src, dst) = if from_left {
            (&e.left, &e.right)
        } else {
            (&e.right, &e.left)
        };
        let Some(src) = src else { return };
        if e.status == EntryStatus::TypeMismatch {
            skipped.push(format!(
                "{}: a file on one side and a folder on the other",
                e.rel_path
            ));
            return;
        }
        let op = |kind, overwrite| SyncOp {
            id: 0,
            kind,
            direction: to,
            rel_path: e.rel_path.clone(),
            is_dir: e.is_dir,
            size: if e.is_dir { 0 } else { src.size },
            overwrite,
            reason: "script copy".into(),
            dst_before: None,
            left_rel_path: e.left_rel_path.clone().filter(|p| *p != e.rel_path),
        };
        if e.is_dir {
            if dst.is_none() {
                ops.push(op(OpKind::Mkdir, false));
            }
            e.children
                .iter()
                .for_each(|c| walk(c, to, from_left, ops, skipped));
        } else {
            ops.push(op(OpKind::Copy, dst.is_some()));
        }
    }
    for e in selection_roots(f)? {
        walk(e, to, from_left, &mut ops, &mut skipped);
    }
    Ok((ops, skipped))
}

fn tree_size(e: &FolderEntry, left: bool) -> u64 {
    let own = if e.is_dir {
        0
    } else {
        let m = if left { &e.left } else { &e.right };
        m.as_ref().map_or(0, |m| m.size)
    };
    own + e.children.iter().map(|c| tree_size(c, left)).sum::<u64>()
}

fn delete_ops(f: &Folders, sides: Sides) -> Result<Vec<SyncOp>, String> {
    let mut ops = Vec::new();
    for e in selection_roots(f)? {
        for left in [true, false] {
            let wanted = match sides {
                Sides::Left => left,
                Sides::Right => !left,
                Sides::Both => true,
            };
            let exists = if left {
                e.left.is_some()
            } else {
                e.right.is_some()
            };
            if !wanted || !exists {
                continue;
            }
            ops.push(SyncOp {
                id: 0,
                kind: OpKind::Delete,
                direction: if left {
                    sync::Direction::ToLeft
                } else {
                    sync::Direction::ToRight
                },
                rel_path: e.rel_path.clone(),
                is_dir: e.is_dir,
                size: tree_size(e, left),
                overwrite: false,
                reason: "script delete".into(),
                dst_before: None,
                left_rel_path: e.left_rel_path.clone().filter(|p| *p != e.rel_path),
            });
        }
    }
    Ok(ops)
}

/// The answer to an overwrite/delete question under the confirm option.
fn confirmed(st: &State, env: &mut Env, question: &str) -> bool {
    match st.confirm {
        Confirm::YesToAll => true,
        Confirm::NoToAll => false,
        Confirm::Prompt => !env.silent && (env.confirm)(question),
    }
}

/// Asks (per the confirm option) before overwriting or deleting; the
/// unconfirmed ops are dropped.
fn confirm_ops(st: &mut State, env: &mut Env, ops: Vec<SyncOp>, what: &str) -> Vec<SyncOp> {
    let risky = ops
        .iter()
        .filter(|o| o.overwrite || o.kind == OpKind::Delete)
        .count();
    if risky == 0 {
        return ops;
    }
    let question = format!("{what} will overwrite or delete {risky} item(s). Continue?");
    if confirmed(st, env, &question) {
        return ops;
    }
    info(
        st,
        env,
        &format!("{what}: skipped {risky} overwrite/delete operation(s) (not confirmed; use option confirm:yes-to-all)"),
    );
    ops.into_iter()
        .filter(|o| !(o.overwrite || o.kind == OpKind::Delete))
        .collect()
}

fn copy_selection(st: &mut State, env: &mut Env, dir: Direction) -> CmdResult {
    let (ops, skipped) = copy_ops(folders(st)?, dir)?;
    for s in &skipped {
        detail(st, &format!("not copied: {s}"));
    }
    let what = match dir {
        Direction::LeftToRight => "Copy left->right",
        Direction::RightToLeft => "Copy right->left",
    };
    let ops = confirm_ops(st, env, ops, what);
    execute_ops(st, env, ops, what)
}

fn delete_selection(
    st: &mut State,
    env: &mut Env,
    sides: Sides,
    recycle: Option<bool>,
) -> CmdResult {
    let ops = delete_ops(folders(st)?, sides)?;
    let ops = confirm_ops(st, env, ops, "Delete");
    let saved = st.recycle;
    st.recycle = recycle.unwrap_or(saved);
    let result = execute_ops(st, env, ops, "Delete");
    st.recycle = saved;
    result
}

fn deleter(st: &State) -> Box<dyn Deleter> {
    if st.recycle {
        Box::new(TrashDeleter::new())
    } else {
        Box::new(PermanentDeleter)
    }
}

/// Runs ops through the fail-safe executor, logs the outcome and compares
/// the folders again so later commands see the new state.
fn execute_ops(st: &mut State, env: &mut Env, mut ops: Vec<SyncOp>, label: &str) -> CmdResult {
    for (i, op) in ops.iter_mut().enumerate() {
        op.id = i as u32;
    }
    if ops.is_empty() {
        info(st, env, &format!("{label}: nothing to do"));
        return Ok(());
    }
    if env.dry_run {
        for op in &ops {
            let to = match op.direction {
                sync::Direction::ToLeft => "left",
                sync::Direction::ToRight => "right",
            };
            info(
                st,
                env,
                &format!("  would {:?} {} ({to})", op.kind, op.rel_path).to_lowercase(),
            );
        }
        info(st, env, &format!("{label}: {} operation(s) (dry run)", ops.len()));
        return Ok(());
    }
    let opts = ExecOptions {
        on_error: if st.stop_on_error {
            ErrorPolicy::Stop
        } else {
            ErrorPolicy::Continue
        },
        ..Default::default()
    };
    let deleter = deleter(st);
    let report: ExecReport = {
        let f = folders(st)?;
        let (left, right) = (f.left.root()?, f.right.root()?);
        let cancel = AtomicBool::new(false);
        exec::execute_on(
            left,
            right,
            &ops,
            &opts,
            deleter.as_ref(),
            &mut NullJournal,
            &cancel,
            &mut |_| {},
        )
    };
    let count = |kind: OpKind| {
        report
            .results
            .iter()
            .filter(|r| r.ok && r.kind == kind)
            .count()
    };
    info(
        st,
        env,
        &format!(
            "{label}: {} copied, {} deleted, {} folder(s) created, {} failed",
            count(OpKind::Copy),
            count(OpKind::Delete),
            count(OpKind::Mkdir),
            report.failed
        ),
    );
    let failures: Vec<String> = report
        .results
        .iter()
        .filter(|r| !r.ok)
        .map(|r| format!("{}: {}", r.rel_path, r.error.as_deref().unwrap_or("failed")))
        .collect();
    for r in report.results.iter().filter(|r| r.ok) {
        detail(
            st,
            &format!("{:?} {:?} {}", r.kind, r.direction, r.rel_path),
        );
    }
    recompare_if_loaded(st, env)?;
    if let Some(why) = &report.stopped {
        return Err(format!("{label} stopped: {why}"));
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "{label}: {} operation(s) failed: {}",
            failures.len(),
            failures.join("; ")
        ))
    }
}

/// How name clashes are settled under the confirm option.
fn conflict_mode(st: &State, env: &Env) -> ConflictMode {
    match st.confirm {
        Confirm::YesToAll => ConflictMode::Overwrite,
        Confirm::NoToAll => ConflictMode::Skip,
        Confirm::Prompt if env.silent => ConflictMode::Skip,
        Confirm::Prompt => ConflictMode::Ask,
    }
}

/// One file operation on the selection, through pcompare-sync's file
/// operations. `src` / `dst` pick the roots; logs the outcome and compares
/// again.
fn file_op(
    st: &mut State,
    env: &mut Env,
    label: &str,
    req: FileOpRequest,
    src: &Side,
    dst: Option<&Side>,
) -> CmdResult {
    if req.items.is_empty() {
        info(st, env, &format!("{label}: nothing to do"));
        return Ok(());
    }
    if env.dry_run {
        for i in &req.items {
            let to = if i.dst.is_empty() || i.dst == i.src {
                String::new()
            } else {
                format!(" -> {}", i.dst)
            };
            info(st, env, &format!("  would {} {}{to}", label.to_lowercase(), i.src));
        }
        info(st, env, &format!("{label}: {} item(s) (dry run)", req.items.len()));
        return Ok(());
    }
    let deleter = deleter(st);
    let ctl = Control::default();
    let report: FileOpReport = {
        let src_root = src.root()?;
        let dst_root = dst.map(Side::root).transpose()?;
        let mut ask = |q: &fileops::ConflictQuestion| {
            if confirmed(st, env, &format!("{} exists. Overwrite?", q.path)) {
                ConflictAnswer::Overwrite
            } else {
                ConflictAnswer::Skip
            }
        };
        fileops::run(
            &req,
            src_root,
            dst_root,
            deleter.as_ref(),
            &ctl,
            &mut |_| {},
            &mut ask,
        )
    };
    info(
        st,
        env,
        &format!(
            "{label}: {} done, {} skipped, {} failed",
            report.succeeded, report.skipped, report.failed
        ),
    );
    for n in &report.notes {
        detail(st, n);
    }
    if !env.dry_run {
        recompare_if_loaded(st, env)?;
    }
    if report.failures.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "{label}: {} item(s) failed: {}",
            report.failures.len(),
            report
                .failures
                .iter()
                .map(|f| format!("{}: {}", f.path, f.message))
                .collect::<Vec<_>>()
                .join("; ")
        ))
    }
}

fn options(st: &State, env: &Env) -> FileOpOptions {
    FileOpOptions {
        conflicts: conflict_mode(st, env),
        permanent: !st.recycle,
        trash_replaced: st.recycle,
        ..Default::default()
    }
}

/// `move left->right`: moves the selection to the other side.
fn move_selection(st: &mut State, env: &mut Env, dir: Direction) -> CmdResult {
    let f = folders(st)?;
    let from_left = dir == Direction::LeftToRight;
    let items: Vec<FileItem> = selection_roots(f)?
        .into_iter()
        .filter(|e| if from_left { e.left.is_some() } else { e.right.is_some() })
        .map(|e| {
            let (l, r) = (left_rel(e).to_owned(), e.rel_path.clone());
            if from_left {
                FileItem { src: l, dst: r }
            } else {
                FileItem { src: r, dst: l }
            }
        })
        .collect();
    let (src, dst) = if from_left {
        (f.left.clone(), f.right.clone())
    } else {
        (f.right.clone(), f.left.clone())
    };
    let req = FileOpRequest {
        action: FileAction::Move,
        items,
        options: options(st, env),
    };
    let label = if from_left {
        "Move left->right"
    } else {
        "Move right->left"
    };
    file_op(st, env, label, req, &src, Some(&dst))
}

/// The deepest folder every path in `paths` is inside ("" for none).
fn common_parent(paths: &[&str]) -> String {
    let mut parts: Option<Vec<&str>> = None;
    for p in paths {
        let dirs: Vec<&str> = p.split('/').collect();
        let dirs = &dirs[..dirs.len().saturating_sub(1)];
        parts = Some(match parts {
            None => dirs.to_vec(),
            Some(prev) => prev
                .iter()
                .zip(dirs)
                .take_while(|(a, b)| a == b)
                .map(|(a, _)| *a)
                .collect(),
        });
    }
    parts.unwrap_or_default().join("/")
}

/// Where `rel` lands below a `copyto` target.
fn target_rel(rel: &str, mode: PathMode, common: &str) -> String {
    match mode {
        PathMode::None => rel.rsplit('/').next().unwrap_or(rel).to_owned(),
        PathMode::Base => rel.to_owned(),
        PathMode::Relative if common.is_empty() => rel.to_owned(),
        PathMode::Relative => rel
            .strip_prefix(common)
            .map(|r| r.trim_start_matches('/'))
            .unwrap_or(rel)
            .to_owned(),
    }
}

/// An empty zip archive (just the end-of-central-directory record).
const EMPTY_ZIP: [u8; 22] = [
    0x50, 0x4b, 0x05, 0x06, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
];

/// Opens a `copyto` / `snapshot` target folder, creating it (or an empty
/// zip archive for a `.zip` name) when it doesn't exist.
fn open_target(env: &Env, target: &str) -> Result<Side, String> {
    if env.host.kind(target) == PathKind::Missing && !target.contains("://") {
        let zip = pcompare_archive::ArchiveKind::from_name(target, false)
            .is_some_and(pcompare_archive::ArchiveKind::writable);
        if zip {
            if let Some(parent) = Path::new(target).parent() {
                std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
            }
            std::fs::write(target, EMPTY_ZIP).map_err(|e| format!("{target}: {e}"))?;
        } else {
            env.host.create_folder(target)?;
        }
    }
    env.host.folder(target)
}

fn copy_to(
    st: &mut State,
    env: &mut Env,
    sides: Sides,
    mode: PathMode,
    target: &str,
    remove_source: bool,
) -> CmdResult {
    let target = resolve(env, target);
    let f = folders(st)?;
    let roots = selection_roots(f)?;
    // Items per side; with `all`, the right side only adds what the left lacks.
    let mut per_side: Vec<(bool, Vec<String>)> = Vec::new();
    for left in [true, false] {
        let wanted = match sides {
            Sides::Left => left,
            Sides::Right => !left,
            Sides::Both => true,
        };
        if !wanted {
            continue;
        }
        let items: Vec<String> = roots
            .iter()
            .filter(|e| {
                if left {
                    e.left.is_some()
                } else {
                    e.right.is_some() && (sides != Sides::Both || e.left.is_none())
                }
            })
            .map(|e| {
                if left {
                    left_rel(e).to_owned()
                } else {
                    e.rel_path.clone()
                }
            })
            .collect();
        per_side.push((left, items));
    }
    let all: Vec<&str> = per_side
        .iter()
        .flat_map(|(_, items)| items.iter().map(String::as_str))
        .collect();
    let common = common_parent(&all);
    let (left, right) = (f.left.clone(), f.right.clone());
    let dest = if env.dry_run {
        env.host.folder(&target).ok()
    } else {
        Some(open_target(env, &target)?)
    };
    let dest = dest.unwrap_or_else(|| Side::local(Path::new(&target)));
    let verb = if remove_source { "Move" } else { "Copy" };
    let mut errors = Vec::new();
    for (is_left, items) in per_side {
        let req = FileOpRequest {
            action: if remove_source {
                FileAction::Move
            } else {
                FileAction::Copy
            },
            items: items
                .iter()
                .map(|rel| FileItem {
                    src: rel.clone(),
                    dst: target_rel(rel, mode, &common),
                })
                .collect(),
            options: options(st, env),
        };
        let side = if is_left { &left } else { &right };
        let label = format!(
            "{verb} {} to {target}",
            if is_left { "left" } else { "right" }
        );
        if let Err(e) = file_op(st, env, &label, req, side, Some(&dest)) {
            errors.push(e);
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

/// DOS-style rename: `*` keeps the matching part of the old name, `?` its
/// character at that position; the stem and the extension are matched
/// separately (`*.bak` turns `a.txt` into `a.bak`).
pub fn dos_rename(name: &str, mask: &str) -> String {
    fn part(orig: &str, mask: &str) -> String {
        let chars: Vec<char> = orig.chars().collect();
        let mut out = String::new();
        for (i, c) in mask.chars().enumerate() {
            match c {
                '*' => {
                    out.extend(chars.iter().skip(i));
                    break;
                }
                '?' => out.extend(chars.get(i)),
                c => out.push(c),
            }
        }
        out
    }
    let split = |s: &str| match s.rsplit_once('.') {
        Some((a, b)) if !a.is_empty() => (a.to_owned(), Some(b.to_owned())),
        _ => (s.to_owned(), None),
    };
    let (stem, ext) = split(name);
    match split(mask) {
        (mstem, Some(mext)) => {
            let new_stem = part(&stem, &mstem);
            let new_ext = part(ext.as_deref().unwrap_or(""), &mext);
            if new_ext.is_empty() {
                new_stem
            } else {
                format!("{new_stem}.{new_ext}")
            }
        }
        (mstem, None) => part(name, &mstem),
    }
}

/// `$1` → `${1}` so a group number followed by letters stays a number.
fn regex_replacement(new: &str) -> String {
    let mut out = String::new();
    let mut chars = new.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '$' && chars.peek().is_some_and(char::is_ascii_digit) {
            let mut n = String::new();
            while let Some(d) = chars.peek().filter(|d| d.is_ascii_digit()) {
                n.push(*d);
                chars.next();
            }
            out.push_str(&format!("${{{n}}}"));
        } else {
            out.push(c);
        }
    }
    out
}

fn rename_selection(st: &mut State, env: &mut Env, regex: Option<&str>, new: &str) -> CmdResult {
    let re = regex
        .map(regex::Regex::new)
        .transpose()
        .map_err(|e| e.to_string())?;
    let replacement = regex_replacement(new);
    let f = folders(st)?;
    let mut jobs: Vec<(Side, String, String)> = Vec::new();
    for e in selection_roots(f)? {
        for (side, rel) in [(&f.left, left_rel(e)), (&f.right, e.rel_path.as_str())] {
            let exists = if std::ptr::eq(side, &f.left) {
                e.left.is_some()
            } else {
                e.right.is_some()
            };
            if !exists {
                continue;
            }
            let name = rel.rsplit('/').next().unwrap_or(rel);
            let new_name = match &re {
                Some(re) if re.is_match(name) => re.replace(name, replacement.as_str()).into_owned(),
                Some(_) => continue,
                None => dos_rename(name, new),
            };
            if new_name != name && !new_name.is_empty() {
                jobs.push((side.clone(), rel.to_owned(), new_name));
            }
        }
    }
    let mut errors = Vec::new();
    if jobs.is_empty() {
        info(st, env, "Rename: nothing to do");
    }
    for (side, rel, new_name) in jobs {
        let req = FileOpRequest {
            action: FileAction::Rename,
            items: vec![FileItem {
                src: rel.clone(),
                dst: String::new(),
            }],
            options: FileOpOptions {
                new_name: new_name.clone(),
                ..options(st, env)
            },
        };
        if let Err(e) = file_op(st, env, &format!("Rename to {new_name}"), req, &side, None) {
            errors.push(e);
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

/// Items of the selection on one side, by the side's own relative path.
fn side_items(f: &Folders, left: bool) -> Result<Vec<FileItem>, String> {
    Ok(selection_roots(f)?
        .into_iter()
        .filter(|e| if left { e.left.is_some() } else { e.right.is_some() })
        .map(|e| {
            let (l, r) = (left_rel(e).to_owned(), e.rel_path.clone());
            if left {
                FileItem { src: l, dst: r }
            } else {
                FileItem { src: r, dst: l }
            }
        })
        .collect())
}

fn touch_selection(st: &mut State, env: &mut Env, t: &Touch) -> CmdResult {
    let f = folders(st)?;
    let (left, right) = (f.left.clone(), f.right.clone());
    // (side touched, the other side, items, time)
    let mut jobs: Vec<(bool, Vec<FileItem>, TouchTime)> = Vec::new();
    match t {
        Touch::Copy(dir) => {
            // left->right: the right side's items take the left side's times.
            let to_left = *dir == Direction::RightToLeft;
            let items = side_items(f, to_left)?
                .into_iter()
                .filter(|i| !i.dst.is_empty())
                .collect::<Vec<_>>();
            let present: Vec<FileItem> = items
                .into_iter()
                .filter(|i| {
                    find(&f.result.entries, if to_left { &i.dst } else { &i.src })
                        .is_some_and(|e| e.left.is_some() && e.right.is_some())
                })
                .collect();
            jobs.push((to_left, present, TouchTime::OtherSide));
        }
        Touch::Set { sides, at } => {
            let time = match at {
                Some(ms) => TouchTime::At { ms: *ms },
                None => TouchTime::Now,
            };
            for left in [true, false] {
                let wanted = match sides {
                    Sides::Left => left,
                    Sides::Right => !left,
                    Sides::Both => true,
                };
                if wanted {
                    jobs.push((left, side_items(f, left)?, time));
                }
            }
        }
    }
    let mut errors = Vec::new();
    for (is_left, items, time) in jobs {
        let (side, other) = if is_left {
            (&left, &right)
        } else {
            (&right, &left)
        };
        let req = FileOpRequest {
            action: FileAction::Touch,
            items,
            options: FileOpOptions {
                touch: time,
                recursive: true,
                ..options(st, env)
            },
        };
        let label = format!("Touch {}", if is_left { "left" } else { "right" });
        let dst = (time == TouchTime::OtherSide).then_some(other);
        if let Err(e) = file_op(st, env, &label, req, side, dst) {
            errors.push(e);
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

fn attrib_selection(st: &mut State, env: &mut Env, set: u32, clear: u32) -> CmdResult {
    let f = folders(st)?;
    let (left, right) = (f.left.clone(), f.right.clone());
    let jobs = [(true, side_items(f, true)?), (false, side_items(f, false)?)];
    let mut errors = Vec::new();
    for (is_left, items) in jobs {
        let req = FileOpRequest {
            action: FileAction::Attributes,
            items,
            options: FileOpOptions {
                attributes: AttrChange {
                    set,
                    clear,
                    ..Default::default()
                },
                ..options(st, env)
            },
        };
        let side = if is_left { &left } else { &right };
        let label = format!("Attributes {}", if is_left { "left" } else { "right" });
        if let Err(e) = file_op(st, env, &label, req, side, None) {
            errors.push(e);
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

// ---------- snapshots ----------

/// Relative paths present on one side of a comparison (what the filters let through).
fn side_paths(entries: &[FolderEntry], left: bool, out: &mut HashSet<String>) {
    for e in entries {
        let present = if left { e.left.is_some() } else { e.right.is_some() };
        if present {
            out.insert(if left {
                left_rel(e).to_owned()
            } else {
                e.rel_path.clone()
            });
        }
        side_paths(&e.children, left, out);
    }
}

/// Drops entries outside `keep` (when given) and, unless `include_empty`,
/// folders left with no files.
fn prune(
    entries: &mut Vec<pcompare_archive::snapshot::SnapEntry>,
    prefix: &str,
    keep: Option<&HashSet<String>>,
    include_empty: bool,
) {
    entries.retain_mut(|e| {
        let rel = if prefix.is_empty() {
            e.name.clone()
        } else {
            format!("{prefix}/{}", e.name)
        };
        if keep.is_some_and(|k| !k.contains(&rel)) {
            return false;
        }
        if e.dir {
            prune(&mut e.children, &rel, keep, include_empty);
            return include_empty || !e.children.is_empty();
        }
        true
    });
}

/// `<folder name>_<date>.pcsnap` in `dir`, numbered when taken.
fn snapshot_name(dir: &Path, source: &str) -> PathBuf {
    let name = Path::new(source.trim_end_matches(['/', '\\']))
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "snapshot".into());
    let date = chrono::Local::now().format("%Y-%m-%d");
    let ext = pcompare_archive::SNAPSHOT_EXT;
    let mut out = dir.join(format!("{name}_{date}.{ext}"));
    let mut n = 2;
    while out.exists() {
        out = dir.join(format!("{name}_{date}_{n}.{ext}"));
        n += 1;
    }
    out
}

fn snapshot(st: &mut State, env: &mut Env, s: &Snapshot) -> CmdResult {
    let (side, keep) = match &s.source {
        SnapshotSource::Path(p) => (env.host.folder(&resolve(env, p))?, None),
        source => {
            let left = *source == SnapshotSource::Left;
            let f = folders(st)?;
            let side = if left { f.left.clone() } else { f.right.clone() };
            let mut keep = HashSet::new();
            side_paths(&f.result.entries, left, &mut keep);
            (side, (!s.no_filters).then_some(keep))
        }
    };
    if side.shown.is_empty() {
        return Err("that side has no folder loaded".into());
    }
    let out = match &s.output {
        None => snapshot_name(&env.cwd, &side.shown),
        Some(o) => {
            let p = PathBuf::from(resolve(env, o));
            if p.is_dir() || o.ends_with(['/', '\\']) {
                snapshot_name(&p, &side.shown)
            } else if p.extension().is_none() {
                p.with_extension(crate::host::BC_SNAPSHOT_EXT)
            } else {
                p
            }
        }
    };
    if env.dry_run {
        info(
            st,
            env,
            &format!("Would save a snapshot of {} to {}", side.shown, out.display()),
        );
        return Ok(());
    }
    let fs: Arc<dyn Vfs> = if s.expand_archives {
        let local = side.local.is_some();
        Arc::new(env.host.archives().expand(side.fs.clone(), local, side.crcs.clone()))
    } else {
        side.fs.clone()
    };
    let opts = pcompare_archive::snapshot::SnapshotOptions {
        include_crcs: s.save_crc,
    };
    let (mut snap, _) =
        pcompare_archive::snapshot::take(&*fs, &side.path, &side.shown, opts, None, None)
            .map_err(|e| e.to_string())?;
    prune(&mut snap.entries, "", keep.as_ref(), s.include_empty);
    let sum = pcompare_archive::snapshot::summarize(&snap.entries);
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    pcompare_archive::snapshot::save(&snap, &out).map_err(|e| e.to_string())?;
    if s.save_version {
        detail(st, "save-version: version information isn't stored in PCompare snapshots");
    }
    info(
        st,
        env,
        &format!(
            "Saved snapshot of {} to {}: {} files, {} folders",
            side.shown,
            out.display(),
            sum.files,
            sum.folders
        ),
    );
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::host::LocalHost;
    use crate::parse::parse_script;

    pub(crate) struct Run {
        pub outcome: Outcome,
        pub out: String,
        pub err: String,
    }

    pub(crate) fn run_with(
        dir: &Path,
        script: &str,
        params: &[String],
        host: &dyn Host,
        answer: bool,
        dry_run: bool,
    ) -> Run {
        let stmts = parse_script(script, params, &|_| None).unwrap_or_else(|e| panic!("{e:?}"));
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let mut confirm = |_: &str| answer;
        let outcome = run(
            &stmts,
            &mut Env {
                silent: false,
                dry_run,
                cwd: dir.to_path_buf(),
                out: &mut out,
                err: &mut err,
                confirm: &mut confirm,
                host,
            },
        );
        Run {
            outcome,
            out: String::from_utf8(out).unwrap(),
            err: String::from_utf8(err).unwrap(),
        }
    }

    pub(crate) fn run_in(dir: &Path, script: &str, answer: bool) -> Run {
        run_with(dir, script, &[], &LocalHost::temporary(), answer, false)
    }

    /// `l/` and `r/` with a same, a changed, a left-only and a right-only
    /// file, plus a left-only folder.
    pub(crate) fn fixture() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        let w = |p: &str, s: &str| {
            let path = d.path().join(p);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, s).unwrap();
        };
        w("l/same.txt", "same\n");
        w("r/same.txt", "same\n");
        w("l/sub/changed.txt", "one\ntwo\n");
        w("r/sub/changed.txt", "one\nTWO!\n");
        w("l/left.txt", "left only\n");
        w("r/right.txt", "right only\n");
        w("l/newdir/deep.txt", "deep\n");
        d
    }

    fn read(d: &Path, p: &str) -> String {
        std::fs::read_to_string(d.join(p)).unwrap()
    }

    #[test]
    fn mirror_sync_makes_the_right_side_identical() {
        let d = fixture();
        let r = run_in(
            d.path(),
            "option recycle:no\ncriteria binary\nload l r\nsync create-empty mirror:left->right\n",
            false,
        );
        assert_eq!(r.outcome.exit_code(), 0, "{}{}", r.out, r.err);
        assert!(!d.path().join("r/right.txt").exists());
        assert_eq!(read(d.path(), "r/sub/changed.txt"), "one\ntwo\n");
        assert_eq!(read(d.path(), "r/newdir/deep.txt"), "deep\n");
        assert!(
            r.out.contains(
                "Compared folders: same 4, similar 0, different 0, left orphans 0, right orphans 0"
            ),
            "{}",
            r.out
        );
    }

    #[test]
    fn update_never_deletes() {
        let d = fixture();
        let r = run_in(
            d.path(),
            "criteria binary\nload l r\nsync update:left->right\n",
            false,
        );
        assert_eq!(r.outcome.errors, 0, "{}", r.err);
        assert!(d.path().join("r/right.txt").exists());
        assert!(d.path().join("r/left.txt").exists());
    }

    #[test]
    fn sync_visible_only_touches_expanded_folders() {
        let d = fixture();
        let r = run_in(
            d.path(),
            "criteria binary\nload l r\nsync visible update:left->right\n",
            false,
        );
        assert_eq!(r.outcome.errors, 0, "{}", r.err);
        // Top level only: left.txt is copied, newdir/deep.txt is not.
        assert!(d.path().join("r/left.txt").exists());
        assert!(!d.path().join("r/newdir").exists());
        let r = run_in(
            d.path(),
            "criteria binary\nload l r\nexpand newdir\nsync visible update:left->right\n",
            false,
        );
        assert_eq!(r.outcome.errors, 0, "{}", r.err);
        assert!(d.path().join("r/newdir/deep.txt").exists());
    }

    #[test]
    fn selection_copy_needs_confirmation_to_overwrite() {
        let d = fixture();
        let script = "option recycle:no\ncriteria binary\nload l r\nexpand all\nselect all.diff.files\ncopy left->right\n";
        let no = run_in(d.path(), script, false);
        assert_eq!(no.outcome.errors, 0, "{}", no.err);
        // The orphans were copied, the changed file was not overwritten.
        assert!(
            d.path().join("r/left.txt").exists() && d.path().join("r/newdir/deep.txt").exists()
        );
        assert_eq!(read(d.path(), "r/sub/changed.txt"), "one\nTWO!\n");
        assert!(no.out.contains("skipped 1 overwrite/delete"), "{}", no.out);
        let yes = run_in(
            d.path(),
            &format!("option confirm:yes-to-all\n{script}"),
            false,
        );
        assert_eq!(yes.outcome.errors, 0, "{}", yes.err);
        assert_eq!(read(d.path(), "r/sub/changed.txt"), "one\ntwo\n");
    }

    #[test]
    fn collapsed_folders_hide_their_contents_from_select() {
        let d = fixture();
        let r = run_in(
            d.path(),
            "criteria binary\nload l r\nselect all.diff.files\n",
            false,
        );
        // Top level only: left.txt and right.txt.
        assert!(r.out.contains("Selected 2 items"), "{}", r.out);
        let r = run_in(
            d.path(),
            "criteria binary\nload l r\nexpand sub\nselect left.diff.files\n",
            false,
        );
        // left.txt and sub/changed.txt.
        assert!(r.out.contains("Selected 2 items"), "{}", r.out);
    }

    #[test]
    fn select_empty_folders() {
        let d = fixture();
        std::fs::create_dir_all(d.path().join("l/empty/inner")).unwrap();
        let r = run_in(
            d.path(),
            "load l r\nexpand all\nselect empty.folders\noption confirm:yes-to-all\noption recycle:no\ndelete left\n",
            false,
        );
        // empty and empty/inner; deleting the outer one takes both.
        assert!(r.out.contains("Selected 2 items"), "{}", r.out);
        assert_eq!(r.outcome.errors, 0, "{}", r.err);
        assert!(!d.path().join("l/empty").exists());
        assert!(d.path().join("l/newdir").exists());
    }

    #[test]
    fn delete_removes_selected_orphans_with_prompt_consent() {
        let d = fixture();
        let r = run_in(
            d.path(),
            "option recycle:no\nload l r\nselect right.orphan.files\ndelete right\n",
            true,
        );
        assert_eq!(r.outcome.errors, 0, "{}", r.err);
        assert!(!d.path().join("r/right.txt").exists());
        let r = run_in(
            d.path(),
            "load l r\nselect left.orphan.files\noption confirm:yes-to-all\ndelete recyclebin=no left\n",
            false,
        );
        assert_eq!(r.outcome.errors, 0, "{}", r.err);
        assert!(!d.path().join("l/left.txt").exists());
    }

    #[test]
    fn move_sends_the_selection_to_the_other_side() {
        let d = fixture();
        let r = run_in(
            d.path(),
            "load l r\nselect left.orphan\nmove left->right\n",
            false,
        );
        assert_eq!(r.outcome.errors, 0, "{}", r.err);
        assert!(!d.path().join("l/left.txt").exists());
        assert_eq!(read(d.path(), "r/left.txt"), "left only\n");
        assert_eq!(read(d.path(), "r/newdir/deep.txt"), "deep\n");
        assert!(!d.path().join("l/newdir").exists());
    }

    #[test]
    fn copyto_and_moveto_keep_the_requested_structure() {
        let d = fixture();
        std::fs::write(d.path().join("l/sub/other.txt"), "x").unwrap();
        let r = run_in(
            d.path(),
            "load l r\nexpand all\nselect left.files\ncopyto left path:base out/base\n\
             copyto left path:none out/none\n\
             filter \"sub\\\"\nexpand all\nselect left.files\ncopyto left path:relative out/rel\n\
             filter \"*\"\nexpand all\nselect left.files\nmoveto left path:relative out/moved\n",
            false,
        );
        assert_eq!(r.outcome.errors, 0, "{}{}", r.out, r.err);
        assert_eq!(read(d.path(), "out/base/newdir/deep.txt"), "deep\n");
        assert_eq!(read(d.path(), "out/base/sub/changed.txt"), "one\ntwo\n");
        assert_eq!(read(d.path(), "out/none/deep.txt"), "deep\n");
        assert_eq!(read(d.path(), "out/none/changed.txt"), "one\ntwo\n");
        // Everything selected is in sub/: path:relative drops it.
        assert_eq!(read(d.path(), "out/rel/other.txt"), "x");
        assert_eq!(read(d.path(), "out/rel/changed.txt"), "one\ntwo\n");
        // Files from several folders: relative to the base (nothing shared).
        assert_eq!(read(d.path(), "out/moved/sub/changed.txt"), "one\ntwo\n");
        assert!(!d.path().join("l/sub/changed.txt").exists());
        assert!(!d.path().join("l/same.txt").exists());
    }

    #[test]
    fn copyto_path_modes() {
        let common = common_parent(&["a/b/x.txt", "a/b/c/y.txt"]);
        assert_eq!(common, "a/b");
        assert_eq!(target_rel("a/b/c/y.txt", PathMode::Relative, &common), "c/y.txt");
        assert_eq!(target_rel("a/b/c/y.txt", PathMode::Base, &common), "a/b/c/y.txt");
        assert_eq!(target_rel("a/b/c/y.txt", PathMode::None, &common), "y.txt");
        assert_eq!(common_parent(&["x.txt", "a/y.txt"]), "");
    }

    #[test]
    fn copyto_a_zip_archive_creates_it() {
        let d = fixture();
        let r = run_in(
            d.path(),
            "load l r\nexpand all\nselect left.diff.files left.orphan.files\ncopyto left path:relative out/a.zip\n",
            false,
        );
        assert_eq!(r.outcome.errors, 0, "{}{}", r.out, r.err);
        let r = run_in(d.path(), "load out/a.zip l\nexpand all\nselect all.diff.files\n", false);
        assert_eq!(r.outcome.errors, 0, "{}{}", r.out, r.err);
        // same.txt is the only file not copied into the archive.
        assert!(r.out.contains("Selected 1 item"), "{}", r.out);
    }

    #[test]
    fn rename_dos_masks_and_regexes() {
        assert_eq!(dos_rename("a.txt", "*.bak"), "a.bak");
        // Windows semantics: a literal replaces the character at its position.
        assert_eq!(dos_rename("report.txt", "??_*.*"), "re_ort.txt");
        assert_eq!(dos_rename("abc", "x*"), "xbc");
        assert_eq!(regex_replacement("$2$1.txt"), "${2}${1}.txt");
        let d = fixture();
        let r = run_in(
            d.path(),
            "load l r\nselect left.orphan.files\nrename *.bak\n",
            false,
        );
        assert_eq!(r.outcome.errors, 0, "{}", r.err);
        assert!(d.path().join("l/left.bak").exists());
        let r = run_in(
            d.path(),
            "load l r\nselect all.files\nrename regexpr (s)(ame)\\.txt $2$1.txt\n",
            false,
        );
        assert_eq!(r.outcome.errors, 0, "{}", r.err);
        assert!(d.path().join("l/ames.txt").exists() && d.path().join("r/ames.txt").exists());
    }

    fn mtime(p: &Path) -> i64 {
        std::fs::metadata(p)
            .unwrap()
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64
    }

    #[test]
    fn touch_sets_and_copies_timestamps() {
        let d = fixture();
        let r = run_in(
            d.path(),
            "load l r\nexpand all\nselect all.files\ntouch left:\"2012-12-31 12:00\"\n",
            false,
        );
        assert_eq!(r.outcome.errors, 0, "{}", r.err);
        let noon = crate::parse::parse_timestamp("2012-12-31 12:00").unwrap();
        assert_eq!(mtime(&d.path().join("l/same.txt")), noon);
        assert_ne!(mtime(&d.path().join("r/same.txt")), noon);
        let r = run_in(
            d.path(),
            "load l r\nexpand all\nselect all.files\ntouch left->right\n",
            false,
        );
        assert_eq!(r.outcome.errors, 0, "{}", r.err);
        assert_eq!(mtime(&d.path().join("r/same.txt")), noon);
        assert_eq!(mtime(&d.path().join("r/sub/changed.txt")), noon);
    }

    #[cfg(unix)]
    #[test]
    fn attrib_sets_read_only() {
        let d = fixture();
        let r = run_in(d.path(), "load l r\nselect left.orphan.files\nattrib +r\n", false);
        assert_eq!(r.outcome.errors, 0, "{}", r.err);
        assert!(std::fs::metadata(d.path().join("l/left.txt"))
            .unwrap()
            .permissions()
            .readonly());
        let r = run_in(d.path(), "load l r\nselect left.orphan.files\nattrib -r\n", false);
        assert_eq!(r.outcome.errors, 0, "{}", r.err);
        assert!(!std::fs::metadata(d.path().join("l/left.txt"))
            .unwrap()
            .permissions()
            .readonly());
    }

    #[test]
    fn filters_cutoff_size_and_attrib() {
        let d = fixture();
        std::fs::write(d.path().join("l/big.bin"), vec![0u8; 4096]).unwrap();
        let r = run_in(
            d.path(),
            "load l r\nfilter size:>1KB\nexpand all\nselect left.files\n",
            false,
        );
        // Everything but big.bin: same, left, sub/changed, newdir/deep.
        assert!(r.out.contains("Selected 4 items"), "{}", r.out);
        let r = run_in(
            d.path(),
            "load l r\nfilter size:<1KB\nselect left.files\n",
            false,
        );
        assert!(r.out.contains("Selected 1 item\n"), "{}", r.out);
        let r = run_in(
            d.path(),
            "filter cutoff:<1days\nload l r\nexpand all\nselect left.files\nfilter cutoff:>1days\nselect all.files\nfilter cutoff:none\nselect all.files\n",
            false,
        );
        let sel: Vec<&str> = r.out.lines().filter(|l| l.starts_with("Selected")).collect();
        // big.bin is back once the cutoff is gone.
        assert_eq!(sel, ["Selected 5 items", "Selected 0 items", "Selected 6 items"], "{}", r.out);
        #[cfg(unix)]
        {
            let mut p = std::fs::metadata(d.path().join("l/left.txt")).unwrap().permissions();
            p.set_readonly(true);
            std::fs::set_permissions(d.path().join("l/left.txt"), p).unwrap();
            let r = run_in(d.path(), "filter attrib:+r\nload l r\nselect all.files\n", false);
            assert!(r.out.contains("Selected 1 item\n"), "{}", r.out);
            let r = run_in(d.path(), "filter attrib:-r\nload l r\nselect left.files\n", false);
            assert!(r.out.contains("Selected 2 items"), "{}", r.out);
        }
    }

    fn set_mtime(p: &Path, ms: i64) {
        let t = filetime::FileTime::from_unix_time(ms / 1000, 0);
        filetime::set_file_mtime(p, t).unwrap();
    }

    #[test]
    fn criteria_timezone_shifts_or_ignores_hours() {
        let d = fixture();
        let base = 1_600_000_000_000;
        set_mtime(&d.path().join("l/same.txt"), base);
        set_mtime(&d.path().join("r/same.txt"), base + 6 * 3_600_000);
        let r = run_in(d.path(), "load l r\nselect all.diff.files\n", false);
        assert!(r.out.contains("Selected 3 items"), "{}", r.out);
        let r = run_in(
            d.path(),
            "criteria timestamp size timezone:left+6\nload l r\nselect all.diff.files\n",
            false,
        );
        assert!(r.out.contains("Selected 2 items"), "{}", r.out);
        let r = run_in(
            d.path(),
            "criteria timestamp size timezone:right-6\nload l r\nselect all.diff.files\n",
            false,
        );
        assert!(r.out.contains("Selected 2 items"), "{}", r.out);
        let r = run_in(
            d.path(),
            "criteria timestamp size timezone:ignore\nload l r\nselect all.diff.files\n",
            false,
        );
        assert!(r.out.contains("Selected 2 items"), "{}", r.out);
    }

    #[test]
    fn compare_rechecks_selected_contents() {
        let d = tempfile::tempdir().unwrap();
        for side in ["l", "r"] {
            std::fs::create_dir_all(d.path().join(side)).unwrap();
        }
        std::fs::write(d.path().join("l/a.txt"), "x\r\n").unwrap();
        std::fs::write(d.path().join("r/a.txt"), "x\n").unwrap();
        let r = run_in(
            d.path(),
            "criteria size\nload l r\nselect all.files\ncompare rules-based\nselect all.diff\n",
            false,
        );
        assert_eq!(r.outcome.errors, 0, "{}", r.err);
        // Sizes differ, but only by line endings: similar after the rules compare.
        assert!(
            r.out
                .contains("Compared contents of 1 file pair(s): 0 differ"),
            "{}",
            r.out
        );
        assert!(
            r.out.ends_with("Selected 0 items\nScript completed\n"),
            "{}",
            r.out
        );
    }

    #[test]
    fn errors_are_counted_and_a_failed_load_ends_the_script() {
        let d = fixture();
        let r = run_in(d.path(), "select all\nload l missing\nload l r\n", false);
        assert_eq!(r.outcome.errors, 2);
        assert!(r.outcome.load_failed);
        assert_eq!(r.outcome.exit_code(), EXIT_LOAD_FAILED);
        assert!(
            r.err.contains("line 1: nothing loaded") && r.err.contains("line 2:"),
            "{}",
            r.err
        );
        assert!(!r.out.contains("> load l r"));
        let r = run_in(d.path(), "select all\nload l r\n", false);
        assert_eq!((r.outcome.errors, r.outcome.exit_code()), (1, EXIT_FAILED));
        let r = run_in(
            d.path(),
            "option stop-on-error\nselect all\nload l r\n",
            false,
        );
        assert!(r.outcome.stopped && r.outcome.errors == 1);
        assert!(!r.out.contains("> load l r"));
    }

    #[test]
    fn log_file_records_commands_and_errors() {
        let d = fixture();
        run_in(d.path(), "log verbose logs/run.log\nload l r\n", false);
        let r = run_in(d.path(), "log verbose logs/run.log\nload l nope\n", false);
        assert_eq!(r.outcome.errors, 1);
        let log = read(d.path(), "logs/run.log");
        assert!(
            log.contains("> load l nope") && log.contains("ERROR line 2:"),
            "{log}"
        );
    }

    #[test]
    fn log_none_stops_logging_and_writes_no_file() {
        let d = fixture();
        let r = run_in(d.path(), "log none\nload l r\n", false);
        assert_eq!(r.outcome.errors, 0, "{}", r.err);
        assert!(!d.path().join("none").exists());
        let r = run_in(d.path(), "log normal\nload l r\nlog none\nselect all\n", false);
        assert_eq!(r.outcome.errors, 0, "{}", r.err);
        let log = read(d.path(), "Log.txt");
        assert!(log.contains("> load l r") && !log.contains("> select all"), "{log}");
    }

    #[test]
    fn load_one_folder_default_and_create() {
        let d = fixture();
        let r = run_in(d.path(), "load l\nexpand all\nselect left.orphan.files\n", false);
        assert_eq!(r.outcome.errors, 0, "{}", r.err);
        assert!(r.out.contains("Selected 4 items"), "{}", r.out);
        let r = run_in(
            d.path(),
            "criteria binary\nfilter \"*.md\"\nload <default>\nload l r\nselect all.files\n",
            false,
        );
        // The default settings again: no filter.
        assert!(r.out.contains("Selected 3 items"), "{}", r.out);
        let r = run_in(d.path(), "load create:right l fresh/new\nsync update:left->right\n", false);
        assert_eq!(r.outcome.errors, 0, "{}", r.err);
        assert_eq!(read(d.path(), "fresh/new/newdir/deep.txt"), "deep\n");
    }

    #[test]
    fn saved_sessions_load_by_name_with_their_settings() {
        let d = fixture();
        let store = pcompare_core::session::SessionStore::new(d.path().join("sessions.json"));
        store
            .save(pcompare_core::session::SessionInput {
                id: None,
                name: "Sync Laptop".into(),
                kind: "folder".into(),
                left: d.path().join("l").to_string_lossy().into_owned(),
                right: d.path().join("r").to_string_lossy().into_owned(),
                base: None,
                output: None,
                settings: serde_json::json!({
                    "excludeFiles": "left.txt",
                    "content": "binary",
                    "maxSizeKb": 1,
                }),
            })
            .unwrap();
        let host = LocalHost::temporary().with_sessions(store);
        let r = run_with(
            d.path(),
            "load \"sync laptop\"\nsync update:left->right\n",
            &[],
            &host,
            false,
            false,
        );
        assert_eq!(r.outcome.errors, 0, "{}", r.err);
        assert!(!d.path().join("r/left.txt").exists());
        assert!(d.path().join("r/newdir/deep.txt").exists());
        // The session's size limit applies (big.bin is over 1 KB).
        std::fs::write(d.path().join("l/big.bin"), vec![0u8; 4096]).unwrap();
        let r = run_with(
            d.path(),
            "load \"Sync Laptop\"\nsync update:left->right\n",
            &[],
            &host,
            false,
            false,
        );
        assert_eq!(r.outcome.errors, 0, "{}", r.err);
        assert!(!d.path().join("r/big.bin").exists());
    }

    #[test]
    fn archives_and_snapshots_load_as_folders() {
        let d = fixture();
        let r = run_in(
            d.path(),
            "load l r\nexpand all\nsnapshot save-crc left output:snaps/l.bcss\nsnapshot right output:snaps/\n",
            false,
        );
        assert_eq!(r.outcome.errors, 0, "{}{}", r.out, r.err);
        assert!(d.path().join("snaps/l.bcss").exists());
        let generated = std::fs::read_dir(d.path().join("snaps"))
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| e.file_name().to_string_lossy().starts_with("r_"));
        assert!(generated);
        std::fs::write(d.path().join("l/same.txt"), "changed!\n").unwrap();
        let r = run_in(
            d.path(),
            "criteria crc\nload l snaps/l.bcss\nexpand all\nselect left.diff.files left.orphan.files\n",
            false,
        );
        assert_eq!(r.outcome.errors, 0, "{}{}", r.out, r.err);
        assert!(r.out.contains("Selected 1 item\n"), "{}", r.out);
    }

    #[test]
    fn snapshot_honours_filters_unless_no_filters() {
        let d = fixture();
        let r = run_in(
            d.path(),
            "filter \"-*.txt\"\nload l r\nsnapshot left output:a.pcsnap\nsnapshot no-filters include-empty left output:b.pcsnap\n",
            false,
        );
        assert_eq!(r.outcome.errors, 0, "{}{}", r.out, r.err);
        let a = pcompare_archive::snapshot::load(&d.path().join("a.pcsnap")).unwrap();
        assert!(a.entries.is_empty(), "{:?}", a.entries);
        let b = pcompare_archive::snapshot::load(&d.path().join("b.pcsnap")).unwrap();
        assert_eq!(pcompare_archive::snapshot::summarize(&b.entries).files, 4);
    }

    #[test]
    fn dry_run_changes_nothing() {
        let d = fixture();
        let r = run_with(
            d.path(),
            "option confirm:yes-to-all\nload create:right l fresh\nload l r\nsync mirror:left->right\nexpand all\nselect all.files\ncopyto left out\ntouch left:now\nsnapshot left\n",
            &[],
            &LocalHost::temporary(),
            false,
            true,
        );
        assert_eq!(r.outcome.errors, 0, "{}{}", r.out, r.err);
        assert!(r.out.contains("would copy left.txt"), "{}", r.out);
        assert!(r.out.contains("would delete right.txt"), "{}", r.out);
        assert!(d.path().join("r/right.txt").exists());
        assert!(!d.path().join("out").exists() && !d.path().join("fresh").exists());
    }
}
