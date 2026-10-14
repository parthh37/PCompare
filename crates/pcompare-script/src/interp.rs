//! Runs parsed scripts against local folders: loads and compares them,
//! tracks expansion and selection like Beyond Compare's folder view, writes
//! reports, and copies, deletes and syncs through pcompare-sync's fail-safe
//! executor.

use crate::parse::{
    Command, Confirm, Criteria, Direction, Filter, FolderReport, OutputFormat, ScriptOption,
    SelKind, SelSide, SelStatus, SelectTerm, Sides, Statement, TextReport,
};
use pcompare_core::folder::{
    self, ContentMethod, EntryStatus, FolderCompareOptions, FolderCompareResult, FolderEntry,
    FolderFilters, Side,
};
use pcompare_core::text::TextDiffOptions;
use pcompare_core::vfs::LocalFs;
use pcompare_core::{textfile, PathKind};
use pcompare_report::folder::{
    FolderReportFormat, FolderReportInput, FolderReportOptions, LinkedReports,
};
use pcompare_report::text::{TextReportFormat, TextReportOptions, TextSection};
use pcompare_sync::exec::{self, ErrorPolicy, ExecOptions, ExecReport};
use pcompare_sync::fsops::{Deleter, PermanentDeleter, TrashDeleter};
use pcompare_sync::journal::NullJournal;
use pcompare_sync::sync::{self, OpKind, PlanOptions, SyncMode, SyncOp};
use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

/// Where a script runs and talks to.
pub struct Env<'a> {
    /// `/silent`: no progress output, and confirmations are answered "no".
    pub silent: bool,
    /// Relative paths in the script resolve against this folder.
    pub cwd: PathBuf,
    pub out: &'a mut dyn Write,
    pub err: &'a mut dyn Write,
    /// Asks the user a yes/no question (`option confirm:prompt`).
    pub confirm: &'a mut dyn FnMut(&str) -> bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Outcome {
    /// Commands that failed.
    pub errors: usize,
    /// `option stop-on-error` ended the run early.
    pub stopped: bool,
}

impl Outcome {
    /// 0 = every command succeeded, 1 = some failed.
    pub fn exit_code(&self) -> i32 {
        if self.errors == 0 {
            0
        } else {
            1
        }
    }
}

type CmdResult = Result<(), String>;

struct Folders {
    left: PathBuf,
    right: PathBuf,
    result: FolderCompareResult,
    expand_all: bool,
    expanded: HashSet<String>,
    /// Selected relative paths, in tree order.
    selection: Vec<String>,
}

enum Session {
    None,
    Files { left: PathBuf, right: PathBuf },
    Folders(Box<Folders>),
}

struct Log {
    file: File,
    verbose: bool,
}

struct State {
    criteria: Criteria,
    filter: Filter,
    confirm: Confirm,
    stop_on_error: bool,
    recycle: bool,
    session: Session,
    log: Option<Log>,
}

/// Runs `stmts` in order. Failing commands are reported on `env.err` (and
/// the log) and the script goes on unless `option stop-on-error` is set.
pub fn run(stmts: &[Statement], env: &mut Env) -> Outcome {
    let mut st = State {
        criteria: Criteria::default(),
        filter: Filter::default(),
        confirm: Confirm::Prompt,
        stop_on_error: false,
        recycle: true,
        session: Session::None,
        log: None,
    };
    let mut outcome = Outcome {
        errors: 0,
        stopped: false,
    };
    for s in stmts {
        info(&mut st, env, &format!("> {}", s.text));
        if let Err(msg) = execute(&mut st, env, &s.command) {
            outcome.errors += 1;
            error(&mut st, env, &format!("line {}: {msg}", s.line));
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
fn info(st: &mut State, env: &mut Env, msg: &str) {
    if !env.silent {
        let _ = writeln!(env.out, "{msg}");
    }
    write_log(st, msg);
}

/// Per-item detail: only in a verbose log.
fn detail(st: &mut State, msg: &str) {
    if st.log.as_ref().is_some_and(|l| l.verbose) {
        write_log(st, &format!("  {msg}"));
    }
}

fn error(st: &mut State, env: &mut Env, msg: &str) {
    let _ = writeln!(env.err, "error: {msg}");
    write_log(st, &format!("ERROR {msg}"));
}

fn resolve(env: &Env, p: &str) -> PathBuf {
    let path = Path::new(p);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        env.cwd.join(path)
    }
}

// ---------- commands ----------

fn execute(st: &mut State, env: &mut Env, cmd: &Command) -> CmdResult {
    match cmd {
        Command::Log {
            verbose,
            append,
            path,
        } => {
            let path = resolve(env, path);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("{}: {e}", parent.display()))?;
            }
            let file = OpenOptions::new()
                .create(true)
                .write(true)
                .append(*append)
                .truncate(!*append)
                .open(&path)
                .map_err(|e| format!("cannot open log {}: {e}", path.display()))?;
            st.log = Some(Log {
                file,
                verbose: *verbose,
            });
            write_log(
                st,
                &format!(
                    "Log started ({})",
                    if *verbose { "verbose" } else { "normal" }
                ),
            );
            Ok(())
        }
        Command::Criteria(c) => {
            st.criteria = c.clone();
            recompare_if_loaded(st, env)
        }
        Command::Filter(f) => {
            st.filter = f.clone();
            recompare_if_loaded(st, env)
        }
        Command::Load { left, right } => load(st, env, left, right),
        Command::Expand(target) => {
            let f = folders(st)?;
            match target {
                None => f.expand_all = true,
                Some(p) => {
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
            }
            Ok(())
        }
        Command::Collapse(target) => {
            let f = folders(st)?;
            match target {
                None => {
                    f.expand_all = false;
                    f.expanded.clear();
                }
                Some(p) => {
                    if f.expand_all {
                        f.expand_all = false;
                        f.expanded = all_dirs(&f.result.entries);
                    }
                    f.expanded.remove(p);
                }
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
        Command::TextReport(r) => text_report(st, env, r),
        Command::Sync { mode, create_empty } => sync_folders(st, env, *mode, *create_empty),
        Command::Copy(dir) => copy_selection(st, env, *dir),
        Command::Delete(sides) => delete_selection(st, env, *sides),
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

fn folders(st: &mut State) -> Result<&mut Folders, String> {
    match &mut st.session {
        Session::Folders(f) => Ok(f),
        Session::Files { .. } => Err("this command needs two folders loaded, not files".into()),
        Session::None => Err("nothing loaded (use load <left> <right> first)".into()),
    }
}

fn compare_options(st: &State) -> FolderCompareOptions {
    let c = &st.criteria;
    FolderCompareOptions {
        compare_size: c.size,
        compare_timestamp: c.timestamp,
        time_tolerance_secs: c.tolerance_secs,
        ignore_dst_shift: c.ignore_dst,
        content: c.content,
        compare_attributes: c.attributes,
        recursive: true,
        filters: FolderFilters {
            include_files: st.filter.include_files.clone(),
            exclude_files: st.filter.exclude_files.clone(),
            exclude_folders: st.filter.exclude_folders.clone(),
            ..Default::default()
        },
        ..Default::default()
    }
}

fn compare(st: &State, left: &Path, right: &Path) -> Result<FolderCompareResult, String> {
    folder::compare_folders(left, right, &compare_options(st)).map_err(|e| e.to_string())
}

fn stats_line(r: &FolderCompareResult) -> String {
    let s = &r.stats;
    format!(
        "same {}, similar {}, different {}, left orphans {}, right orphans {}, errors {}",
        s.same, s.similar, s.different, s.left_only, s.right_only, s.errors
    )
}

fn load(st: &mut State, env: &mut Env, left: &str, right: &str) -> CmdResult {
    let (l, r) = (resolve(env, left), resolve(env, right));
    match (pcompare_core::path_kind(&l), pcompare_core::path_kind(&r)) {
        (PathKind::Missing, _) => Err(format!("{}: no such file or folder", l.display())),
        (_, PathKind::Missing) => Err(format!("{}: no such file or folder", r.display())),
        (PathKind::Directory, PathKind::Directory) => {
            let result = compare(st, &l, &r)?;
            info(
                st,
                env,
                &format!("Compared folders: {}", stats_line(&result)),
            );
            st.session = Session::Folders(Box::new(Folders {
                left: l,
                right: r,
                result,
                expand_all: false,
                expanded: HashSet::new(),
                selection: Vec::new(),
            }));
            Ok(())
        }
        (PathKind::File, PathKind::File) => {
            st.session = Session::Files { left: l, right: r };
            Ok(())
        }
        _ => Err("a file and a folder can't be compared".into()),
    }
}

/// Compares the loaded folders again (after criteria, filters or file
/// operations changed); expansion survives, the selection doesn't.
fn recompare_if_loaded(st: &mut State, env: &mut Env) -> CmdResult {
    let Session::Folders(f) = &st.session else {
        return Ok(());
    };
    let result = compare(st, &f.left, &f.right)?;
    let line = format!("Compared folders: {}", stats_line(&result));
    if let Session::Folders(f) = &mut st.session {
        f.result = result;
        f.selection.clear();
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
            SelSide::Left => e.newer == Some(Side::Left),
            SelSide::Right => e.newer == Some(Side::Right),
            SelSide::Any => e.newer.is_some(),
        },
        SelStatus::Older => match t.side {
            SelSide::Left => e.newer == Some(Side::Right),
            SelSide::Right => e.newer == Some(Side::Left),
            SelSide::Any => e.newer.is_some(),
        },
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
fn selected_pairs(f: &Folders) -> Result<Vec<&FolderEntry>, String> {
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

fn side_path(root: &Path, rel: &str) -> PathBuf {
    root.join(rel)
}

// ---------- compare ----------

fn compare_selection(st: &mut State, env: &mut Env, method: Option<ContentMethod>) -> CmdResult {
    let method = method.unwrap_or(match st.criteria.content {
        ContentMethod::Off => ContentMethod::Rules,
        m => m,
    });
    let rules = TextDiffOptions::default();
    let f = folders(st)?;
    let mut verdicts = HashMap::new();
    let mut failures = Vec::new();
    for e in selected_pairs(f)? {
        let (lp, rp) = (
            side_path(&f.left, left_rel(e)),
            side_path(&f.right, &e.rel_path),
        );
        let (ls, rs) = (
            e.left.as_ref().map_or(0, |m| m.size),
            e.right.as_ref().map_or(0, |m| m.size),
        );
        let (lps, rps) = (lp.to_string_lossy(), rp.to_string_lossy());
        match folder::compare_contents(&LocalFs, &lps, ls, &LocalFs, &rps, rs, method, &rules) {
            Ok(v) => {
                verdicts.insert(e.rel_path.clone(), v);
            }
            Err(err) => failures.push(format!("{}: {err}", e.rel_path)),
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

fn left_rel(e: &FolderEntry) -> &str {
    e.left_rel_path.as_deref().unwrap_or(&e.rel_path)
}

/// Applies content verdicts and recomputes the status of every folder above them.
fn apply_verdicts(
    entries: &mut [FolderEntry],
    verdicts: &HashMap<String, folder::ContentVerdict>,
    method: ContentMethod,
) {
    for e in entries {
        if e.is_dir {
            apply_verdicts(&mut e.children, verdicts, method);
            let keep = e.children.is_empty()
                || matches!(
                    e.status,
                    EntryStatus::LeftOnly | EntryStatus::RightOnly | EntryStatus::Error
                );
            if !keep {
                e.status = folder::folder_status(&e.children);
            }
        } else if let Some(v) = verdicts.get(&e.rel_path) {
            e.status = v.status;
            e.crc = v.crc.or(e.crc);
            e.content_checked = Some(method);
        }
    }
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

fn prepare_output(env: &Env, path: &str) -> Result<PathBuf, String> {
    let out = resolve(env, path);
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    Ok(out)
}

fn folder_report(st: &mut State, env: &mut Env, r: &FolderReport) -> CmdResult {
    let out = prepare_output(env, &r.output)?;
    let f = folders(st)?;
    let opts = FolderReportOptions {
        layout: r.layout,
        format: match r.format {
            OutputFormat::HtmlColor | OutputFormat::HtmlMono => FolderReportFormat::Html,
            OutputFormat::Text => FolderReportFormat::Text,
            OutputFormat::Csv => FolderReportFormat::Csv,
        },
        filter: r.filter,
        name_filter: String::new(),
        show_size: r.show_size,
        show_modified: r.show_modified,
        mono: r.format == OutputFormat::HtmlMono,
        title: r.title.clone(),
        expanded: (!f.expand_all).then(|| f.expanded.clone()),
    };
    let input = FolderReportInput::from(&f.result);
    let text = TextReportOptions {
        line_numbers: true,
        ..Default::default()
    };
    let rules = TextDiffOptions::default();
    let local = |p: &str| Ok(PathBuf::from(p));
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

fn text_report(st: &mut State, env: &mut Env, r: &TextReport) -> CmdResult {
    let out = prepare_output(env, &r.output)?;
    // (name, left path, right path)
    let pairs: Vec<(String, PathBuf, PathBuf)> = match &st.session {
        Session::None => return Err("nothing loaded (use load <left> <right> first)".into()),
        Session::Files { left, right } => vec![(String::new(), left.clone(), right.clone())],
        Session::Folders(f) => selected_pairs(f)?
            .into_iter()
            .map(|e| {
                (
                    e.rel_path.clone(),
                    side_path(&f.left, left_rel(e)),
                    side_path(&f.right, &e.rel_path),
                )
            })
            .collect(),
    };
    if pairs.is_empty() {
        return Err("no file pairs selected for the text report".into());
    }
    let rules = TextDiffOptions::default();
    let mut loaded = Vec::new();
    for (name, l, r) in pairs {
        let binary = textfile::file_is_binary(&l).map_err(|e| e.to_string())?
            || textfile::file_is_binary(&r).map_err(|e| e.to_string())?;
        if binary {
            detail(st, &format!("skipped binary file {}", l.display()));
            continue;
        }
        let lt = textfile::load_text(&l).map_err(|e| e.to_string())?;
        let rt = textfile::load_text(&r).map_err(|e| e.to_string())?;
        let (ls, rs) = (
            l.to_string_lossy().into_owned(),
            r.to_string_lossy().into_owned(),
        );
        let diff =
            pcompare_core::text::diff_named_text(&lt.text, Some(&ls), &rt.text, Some(&rs), &rules)
                .map_err(|e| e.to_string())?;
        loaded.push((name, ls, rs, diff));
    }
    if loaded.is_empty() {
        return Err("every selected file is binary; text reports need text files".into());
    }
    let opts = TextReportOptions {
        layout: r.layout,
        format: if r.patch {
            TextReportFormat::Patch
        } else if r.format == OutputFormat::Text {
            TextReportFormat::Text
        } else {
            TextReportFormat::Html
        },
        differences_only: r.differences_only,
        context: r.context,
        line_numbers: r.line_numbers,
        ignore_unimportant: r.ignore_unimportant,
        mono: r.format == OutputFormat::HtmlMono,
        wrap: r.wrap,
        title: r.title.clone(),
    };
    let sections: Vec<TextSection> = loaded
        .iter()
        .map(|(name, l, r, diff)| TextSection {
            name,
            left: l,
            right: r,
            diff,
        })
        .collect();
    let report = pcompare_report::text::render(&sections, &opts, &pcompare_report::now_stamp());
    std::fs::write(&out, report).map_err(|e| format!("{}: {e}", out.display()))?;
    info(
        st,
        env,
        &format!(
            "Wrote text report {} ({} file pair(s))",
            out.display(),
            sections.len()
        ),
    );
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
        SyncMode::Custom => "custom",
    }
}

fn sync_folders(st: &mut State, env: &mut Env, mode: SyncMode, create_empty: bool) -> CmdResult {
    let opts = compare_options(st);
    let f = folders(st)?;
    let mut plan = sync::plan_from_comparison_with(&f.result, mode, &opts, &PlanOptions::default());
    if !create_empty {
        // Copies create their parent folders; only empty ones would be left over.
        plan.ops.retain(|o| o.kind != OpKind::Mkdir);
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
            category: None,
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
                category: None,
            });
        }
    }
    Ok(ops)
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
    let yes = match st.confirm {
        Confirm::YesToAll => true,
        Confirm::NoToAll => false,
        Confirm::Prompt => !env.silent && (env.confirm)(&question),
    };
    if yes {
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

fn delete_selection(st: &mut State, env: &mut Env, sides: Sides) -> CmdResult {
    let ops = delete_ops(folders(st)?, sides)?;
    let ops = confirm_ops(st, env, ops, "Delete");
    execute_ops(st, env, ops, "Delete")
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
    let opts = ExecOptions {
        on_error: if st.stop_on_error {
            ErrorPolicy::Stop
        } else {
            ErrorPolicy::Continue
        },
        ..Default::default()
    };
    let deleter: Box<dyn Deleter> = if st.recycle {
        Box::new(TrashDeleter::new())
    } else {
        Box::new(PermanentDeleter)
    };
    let (left, right) = {
        let f = folders(st)?;
        (f.left.clone(), f.right.clone())
    };
    let cancel = AtomicBool::new(false);
    let report: ExecReport = exec::execute(
        &left,
        &right,
        &ops,
        &opts,
        deleter.as_ref(),
        &mut NullJournal,
        &cancel,
        &mut |_| {},
    );
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse_script;

    struct Run {
        outcome: Outcome,
        out: String,
        err: String,
    }

    fn run_in(dir: &Path, script: &str, answer: bool) -> Run {
        let stmts = parse_script(script, &[], &|_| None).unwrap_or_else(|e| panic!("{e:?}"));
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let mut confirm = |_: &str| answer;
        let outcome = run(
            &stmts,
            &mut Env {
                silent: false,
                cwd: dir.to_path_buf(),
                out: &mut out,
                err: &mut err,
                confirm: &mut confirm,
            },
        );
        Run {
            outcome,
            out: String::from_utf8(out).unwrap(),
            err: String::from_utf8(err).unwrap(),
        }
    }

    /// `l/` and `r/` with a same, a changed, a left-only and a right-only
    /// file, plus a left-only folder.
    fn fixture() -> tempfile::TempDir {
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
        assert_eq!(
            std::fs::read_to_string(d.path().join("r/sub/changed.txt")).unwrap(),
            "one\ntwo\n"
        );
        assert_eq!(
            std::fs::read_to_string(d.path().join("r/newdir/deep.txt")).unwrap(),
            "deep\n"
        );
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
    fn selection_copy_needs_confirmation_to_overwrite() {
        let d = fixture();
        let script = "option recycle:no\ncriteria binary\nload l r\nexpand all\nselect all.diff.files\ncopy left->right\n";
        let no = run_in(d.path(), script, false);
        assert_eq!(no.outcome.errors, 0, "{}", no.err);
        // The orphans were copied, the changed file was not overwritten.
        assert!(
            d.path().join("r/left.txt").exists() && d.path().join("r/newdir/deep.txt").exists()
        );
        assert_eq!(
            std::fs::read_to_string(d.path().join("r/sub/changed.txt")).unwrap(),
            "one\nTWO!\n"
        );
        assert!(no.out.contains("skipped 1 overwrite/delete"), "{}", no.out);
        let yes = run_in(
            d.path(),
            &format!("option confirm:yes-to-all\n{script}"),
            false,
        );
        assert_eq!(yes.outcome.errors, 0, "{}", yes.err);
        assert_eq!(
            std::fs::read_to_string(d.path().join("r/sub/changed.txt")).unwrap(),
            "one\ntwo\n"
        );
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
    fn delete_removes_selected_orphans_with_prompt_consent() {
        let d = fixture();
        let r = run_in(
            d.path(),
            "option recycle:no\nload l r\nselect right.orphan.files\ndelete right\n",
            true,
        );
        assert_eq!(r.outcome.errors, 0, "{}", r.err);
        assert!(!d.path().join("r/right.txt").exists());
    }

    #[test]
    fn reports_are_written() {
        let d = fixture();
        let r = run_in(
            d.path(),
            "criteria binary\nload l r\nexpand all\n\
             folder-report layout:summary options:display-mismatches output-to:out/summary.txt\n\
             folder-report layout:side-by-side options:display-all,include-file-links output-to:out/report.html\n\
             select all.diff.files\n\
             text-report layout:patch output-to:out/changes.diff\n\
             load l/sub/changed.txt r/sub/changed.txt\n\
             text-report layout:side-by-side options:line-numbers output-to:out/file.html\n",
            false,
        );
        assert_eq!(r.outcome.errors, 0, "{}", r.err);
        let summary = std::fs::read_to_string(d.path().join("out/summary.txt")).unwrap();
        assert!(
            summary.contains("Left orphan   left.txt") && !summary.contains("same.txt"),
            "{summary}"
        );
        let html = std::fs::read_to_string(d.path().join("out/report.html")).unwrap();
        assert!(
            html.contains("href=\"report_files/sub/changed.txt.html\""),
            "{html}"
        );
        assert!(d
            .path()
            .join("out/report_files/sub/changed.txt.html")
            .exists());
        let patch = std::fs::read_to_string(d.path().join("out/changes.diff")).unwrap();
        assert!(patch.contains("-two\n+TWO!\n"), "{patch}");
        let file = std::fs::read_to_string(d.path().join("out/file.html")).unwrap();
        assert!(file.contains("<span class=\"m\">"), "{file}");
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
    fn errors_are_counted_and_stop_on_error_stops() {
        let d = fixture();
        let r = run_in(d.path(), "load l missing\nselect all\nload l r\n", false);
        assert_eq!(r.outcome.errors, 2);
        assert!(
            r.err.contains("line 1:") && r.err.contains("line 2: nothing loaded"),
            "{}",
            r.err
        );
        let r = run_in(
            d.path(),
            "option stop-on-error\nload l missing\nload l r\n",
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
        let log = std::fs::read_to_string(d.path().join("logs/run.log")).unwrap();
        assert!(
            log.contains("> load l nope") && log.contains("ERROR line 2:"),
            "{log}"
        );
    }
}
