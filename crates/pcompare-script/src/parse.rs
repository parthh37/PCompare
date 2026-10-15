//! Parser for PCompare scripts, which use the common compare-script language:
//! one command per line (a trailing `&` continues it on the next line), `#`
//! comments (whole lines, or after whitespace at the end of one),
//! double-quoted arguments, `%1`–`%9` script parameters, `%date%`,
//! `%time%` and `%fn_time%`, `%NAME%` environment variables, `lt`/`rt` for
//! `left`/`right`, and keywords that are case-insensitive.
//!
//! The whole script is parsed before anything runs, so a typo on line 40
//! can't leave a sync half done.

use chrono::{DateTime, Local, NaiveDate, NaiveDateTime, NaiveTime, TimeZone};
use pcompare_core::folder::{attr, ContentMethod};
use pcompare_report::folder::DisplayFilter;
use pcompare_sync::sync::SyncMode;
use std::fmt;

/// One command and the line it came from (1-based; the first line of a
/// command continued with `&`).
#[derive(Debug, Clone, PartialEq)]
pub struct Statement {
    pub line: usize,
    /// The command as written (lines joined, variables expanded), for logs.
    pub text: String,
    pub command: Command,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub line: usize,
    pub message: String,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}: {}", self.line, self.message)
    }
}

/// `criteria timezone:`: how the two sides' clocks relate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Timezone {
    #[default]
    Same,
    /// Timestamps a whole number of hours apart (up to 14) match.
    Ignore,
    /// Hours added to one side's timestamps before they are compared.
    Offset { left: bool, hours: i32 },
}

/// Comparison criteria (`criteria timestamp:2sec size crc`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Criteria {
    pub timestamp: bool,
    pub tolerance_secs: u64,
    pub ignore_dst: bool,
    pub size: bool,
    pub content: ContentMethod,
    pub attributes: bool,
    pub timezone: Timezone,
    pub follow_links: bool,
    /// Unimportant differences count as matches (reports take this as
    /// their default).
    pub ignore_unimportant: bool,
}

impl Default for Criteria {
    /// The usual default: timestamp (2 s) and size.
    fn default() -> Self {
        Self {
            timestamp: true,
            tolerance_secs: 2,
            ignore_dst: false,
            size: true,
            content: ContentMethod::Off,
            attributes: false,
            timezone: Timezone::Same,
            follow_links: true,
            ignore_unimportant: false,
        }
    }
}

/// `filter "*.rs;-target\"` split into the engine's glob lists.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Filter {
    pub include_files: Vec<String>,
    pub exclude_files: Vec<String>,
    pub exclude_folders: Vec<String>,
}

/// A point in time given in a script: absolute, or a number of days before
/// the moment the command runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum When {
    /// Milliseconds since the Unix epoch.
    At(i64),
    DaysAgo(u32),
}

/// `filter cutoff:<7days`: `older` excludes files older than the time
/// (`<`, the default), otherwise newer ones (`>`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cutoff {
    pub older: bool,
    pub when: When,
}

/// `filter size:>10MB`: `smaller` excludes files smaller than `bytes` (`<`,
/// the default), otherwise larger ones (`>`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SizeLimit {
    pub smaller: bool,
    pub bytes: u64,
}

/// `filter attrib:+r-h`: files must have every `required` bit and none of
/// the `excluded` ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AttribFilter {
    pub required: u32,
    pub excluded: u32,
}

/// What one `filter` command changes; `None` fields keep their setting and
/// `Some(None)` clears one (`cutoff:none`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FilterCmd {
    pub names: Option<Filter>,
    pub cutoff: Option<Option<Cutoff>>,
    pub size: Option<Option<SizeLimit>>,
    pub attrib: Option<Option<AttribFilter>>,
    /// `exclude-protected` (true) / `include-protected` (false): files with
    /// both the system and hidden attributes.
    pub exclude_protected: Option<bool>,
}

/// One term of `select`, e.g. `left.orphan.files`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectTerm {
    pub side: SelSide,
    pub status: SelStatus,
    pub kind: SelKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelSide {
    Left,
    Right,
    Any,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelStatus {
    All,
    /// Different, type mismatch or orphan.
    Diff,
    /// Same or similar.
    Same,
    /// Byte-for-byte same (not similar).
    Exact,
    Orphan,
    Newer,
    Older,
    /// `empty.folders`: folders with no files below them.
    Empty,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelKind {
    Files,
    Folders,
    Both,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    LeftToRight,
    RightToLeft,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sides {
    Left,
    Right,
    Both,
}

/// `output-options:` of the report commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    HtmlColor,
    HtmlMono,
    Text,
    Csv,
    Xml,
}

/// Where a report goes (`output-to:`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutputTarget {
    File(String),
    Printer,
    Clipboard,
}

/// The `<comparison>` a report names instead of the script's selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Comparison {
    Pair(String, String),
    Session(String),
}

/// Optional folder-report columns (`options:column-…`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Columns {
    pub size: bool,
    pub timestamp: bool,
    pub crc: bool,
    pub attributes: bool,
    pub version: bool,
    pub owner: bool,
    pub group: bool,
    pub vcs: bool,
}

impl Default for Columns {
    /// The usual default: size and timestamp.
    fn default() -> Self {
        Self {
            size: true,
            timestamp: true,
            crc: false,
            attributes: false,
            version: false,
            owner: false,
            group: false,
            vcs: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FolderLayout {
    Summary,
    SideBySide,
    Xml,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderReport {
    pub layout: FolderLayout,
    pub filter: DisplayFilter,
    pub file_links: bool,
    pub columns: Columns,
    pub format: OutputFormat,
    pub title: String,
    pub output: OutputTarget,
}

/// Which `*-report` command: `file-report` picks the kind per file pair
/// from the file format rules and the contents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportKind {
    File,
    Text,
    Hex,
    Data,
    Picture,
    Media,
    Registry,
    Version,
}

impl ReportKind {
    pub fn command(self) -> &'static str {
        match self {
            ReportKind::File => "file-report",
            ReportKind::Text => "text-report",
            ReportKind::Hex => "hex-report",
            ReportKind::Data => "data-report",
            ReportKind::Picture => "picture-report",
            ReportKind::Media => "media-report",
            ReportKind::Registry => "registry-report",
            ReportKind::Version => "version-report",
        }
    }

    fn layouts(self) -> &'static [FileLayout] {
        use FileLayout::*;
        match self {
            ReportKind::File | ReportKind::Text => {
                &[SideBySide, Summary, Interleaved, Patch, Statistics, Xml]
            }
            ReportKind::Hex | ReportKind::Data => &[SideBySide, Summary, Interleaved],
            _ => &[SideBySide, Summary],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileLayout {
    SideBySide,
    Summary,
    /// One column, differences one above the other (the deprecated
    /// `over-under` and `composite`).
    Interleaved,
    Patch,
    /// Line counts per file pair as CSV.
    Statistics,
    Xml,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatchStyle {
    /// Classic `diff` output (`3c3`).
    Normal,
    /// `diff -c` (`***` / `---`).
    Context,
    /// `diff -u` (`@@`).
    Unified,
}

/// Which lines a file report shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineDisplay {
    All,
    Mismatches,
    /// Differences with this many lines of context.
    Context(usize),
    Matches,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileReport {
    pub kind: ReportKind,
    pub layout: FileLayout,
    pub patch: PatchStyle,
    pub display: LineDisplay,
    pub line_numbers: bool,
    /// `None`: the criteria's `ignore-unimportant` decides.
    pub ignore_unimportant: Option<bool>,
    pub strikeout_left: bool,
    pub strikeout_right: bool,
    pub format: OutputFormat,
    pub wrap: bool,
    pub title: String,
    pub output: OutputTarget,
    pub comparison: Option<Comparison>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confirm {
    /// Ask on the terminal (never in `/silent` runs or without a terminal,
    /// where the answer is "no").
    Prompt,
    YesToAll,
    NoToAll,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScriptOption {
    Confirm(Confirm),
    StopOnError(bool),
    /// Deleted and overwritten files go to the Trash / Recycle Bin (default)
    /// or are removed for good.
    Recycle(bool),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    None,
    Normal,
    Verbose,
}

/// `load [create:(all|left|right)] <left> [<right>]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Load {
    /// `load <default>`: default settings, nothing loaded.
    Default,
    /// One argument may name a saved session or a lone left folder; two are
    /// the base folders (or files).
    Paths {
        create: Option<Sides>,
        left: String,
        right: Option<String>,
    },
}

/// `copyto` / `moveto` `path:` option.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathMode {
    /// Items land directly in the target.
    None,
    /// The structure below the deepest folder all selected items share.
    Relative,
    /// The structure below the base folder.
    Base,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Touch {
    /// Copies timestamps from one side to the other.
    Copy(Direction),
    /// Sets timestamps on a side; `None` is now.
    Set { sides: Sides, at: Option<i64> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnapshotSource {
    Left,
    Right,
    Path(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub save_crc: bool,
    pub save_version: bool,
    pub expand_archives: bool,
    pub follow_symlinks: bool,
    pub include_empty: bool,
    pub no_filters: bool,
    pub source: SnapshotSource,
    pub output: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    /// `path: None` is `Log.txt` in the current folder.
    Log {
        level: LogLevel,
        append: bool,
        path: Option<String>,
    },
    Criteria(Criteria),
    Filter(FilterCmd),
    Load(Load),
    /// Empty is "all".
    Expand(Vec<String>),
    Collapse(Vec<String>),
    Select(Vec<SelectTerm>),
    /// Content-compares the selected files; `None` uses the criteria's
    /// content method, or rules-based when it has none.
    Compare(Option<ContentMethod>),
    FolderReport(FolderReport),
    FileReport(FileReport),
    Sync {
        mode: SyncMode,
        create_empty: bool,
        /// Only items the expansion state shows.
        visible: bool,
    },
    /// `copy` / `move` to the other side.
    Copy {
        dir: Direction,
        remove_source: bool,
    },
    /// `copyto` / `moveto` a folder (or zip archive).
    CopyTo {
        sides: Sides,
        path: PathMode,
        target: String,
        remove_source: bool,
    },
    /// `recycle: None` follows `option recycle`.
    Delete {
        sides: Sides,
        recycle: Option<bool>,
    },
    /// `rename [regexpr <old>] <new>`.
    Rename {
        regex: Option<String>,
        new: String,
    },
    Touch(Touch),
    /// Bits to set and to clear.
    Attrib {
        set: u32,
        clear: u32,
    },
    Snapshot(Snapshot),
    Beep,
    Option(ScriptOption),
}

/// `%date%` (2012-12-31), `%time%` (23:59:59) and `%fn_time%` (23-59-59),
/// as compare scripts expect them.
pub fn builtin_var(name: &str, now: &DateTime<Local>) -> Option<String> {
    let fmt = match name.to_ascii_lowercase().as_str() {
        "date" => "%Y-%m-%d",
        "time" => "%H:%M:%S",
        "fn_time" => "%H-%M-%S",
        _ => return None,
    };
    Some(now.format(fmt).to_string())
}

/// Replaces `%1`..`%9` with script parameters (missing ones become empty)
/// and `%NAME%` with variables (unknown names stay as written).
pub fn expand_vars(line: &str, params: &[String], env: &dyn Fn(&str) -> Option<String>) -> String {
    let chars: Vec<char> = line.chars().collect();
    let mut out = String::with_capacity(line.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] != '%' {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        if let Some(d) = chars
            .get(i + 1)
            .and_then(|c| c.to_digit(10))
            .filter(|&d| d >= 1)
        {
            out.push_str(params.get(d as usize - 1).map(String::as_str).unwrap_or(""));
            i += 2;
            continue;
        }
        let end = chars[i + 1..]
            .iter()
            .position(|&c| c == '%')
            .map(|p| p + i + 1);
        let name: Option<String> = end.map(|e| chars[i + 1..e].iter().collect());
        match (end, name) {
            (Some(e), Some(n))
                if !n.is_empty() && n.chars().all(|c| c.is_alphanumeric() || c == '_') =>
            {
                match env(&n) {
                    Some(v) => out.push_str(&v),
                    None => out.extend(&chars[i..=e]),
                }
                i = e + 1;
            }
            _ => {
                out.push('%');
                i += 1;
            }
        }
    }
    out
}

/// Splits a line into arguments. Double quotes group spaces and are removed
/// wherever they appear, so `output-to:"C:\My Reports\a.html"` is one token.
pub fn tokenize(line: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_token = false;
    let mut quoted = false;
    for c in line.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                in_token = true;
            }
            c if c.is_whitespace() && !quoted => {
                if in_token {
                    out.push(std::mem::take(&mut cur));
                    in_token = false;
                }
            }
            c => {
                cur.push(c);
                in_token = true;
            }
        }
    }
    if quoted {
        return Err("unterminated quote".into());
    }
    if in_token {
        out.push(cur);
    }
    Ok(out)
}

/// Cuts a `#` comment off a line: a `#` outside quotes that starts the line
/// or follows whitespace (so `C#` and `"#tmp#"` stay).
fn strip_comment(line: &str) -> &str {
    let mut quoted = false;
    let mut prev_space = true;
    for (i, c) in line.char_indices() {
        match c {
            '"' => quoted = !quoted,
            '#' if !quoted && prev_space => return &line[..i],
            _ => {}
        }
        prev_space = c.is_whitespace();
    }
    line
}

/// Parses a whole script; every error is reported, not just the first.
/// `%date%`, `%time%` and `%fn_time%` are taken from the clock now.
pub fn parse_script(
    text: &str,
    params: &[String],
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Vec<Statement>, Vec<ParseError>> {
    parse_script_at(text, params, env, &Local::now())
}

/// [`parse_script`] with a fixed clock.
pub fn parse_script_at(
    text: &str,
    params: &[String],
    env: &dyn Fn(&str) -> Option<String>,
    now: &DateTime<Local>,
) -> Result<Vec<Statement>, Vec<ParseError>> {
    let vars = |n: &str| builtin_var(n, now).or_else(|| env(n));
    let mut stmts = Vec::new();
    let mut errors = Vec::new();
    // A command being continued with `&`: its first line and text so far.
    let mut pending: Option<(usize, String)> = None;
    let lines: Vec<&str> = text.lines().collect();
    for (i, raw) in lines.iter().enumerate() {
        let line = i + 1;
        let trimmed = strip_comment(raw.trim_start_matches('\u{feff}')).trim();
        let (body, continues) = match trimmed.strip_suffix('&') {
            Some(b) => (b.trim_end(), true),
            None => (trimmed, false),
        };
        let (start, mut joined) = pending.take().unwrap_or((line, String::new()));
        if !body.is_empty() {
            if !joined.is_empty() {
                joined.push(' ');
            }
            joined.push_str(body);
        }
        if continues && i + 1 < lines.len() {
            pending = Some((start, joined));
            continue;
        }
        if joined.is_empty() {
            continue;
        }
        let expanded = expand_vars(&joined, params, &vars);
        let parsed = tokenize(&expanded).and_then(|tokens| parse_command(&tokens, now));
        match parsed {
            Ok(command) => stmts.push(Statement {
                line: start,
                text: expanded,
                command,
            }),
            Err(message) => errors.push(ParseError {
                line: start,
                message,
            }),
        }
    }
    if errors.is_empty() {
        Ok(stmts)
    } else {
        Err(errors)
    }
}

/// `key:value` split, case-insensitive key. Only words (letters, digits,
/// dashes) count as keys, so `C:\x`, `/tmp/a:b` and `ftp://host` are paths.
fn key_value(token: &str) -> Option<(String, &str)> {
    let (k, v) = token.split_once(':')?;
    let word = k.len() > 1
        && k.starts_with(|c: char| c.is_ascii_alphabetic())
        && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
    if !word || v.starts_with("//") {
        return None;
    }
    Some((k.to_ascii_lowercase(), v))
}

fn parse_command(tokens: &[String], now: &DateTime<Local>) -> Result<Command, String> {
    let (head, args) = tokens.split_first().ok_or("empty command")?;
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let cmd = head.to_ascii_lowercase();
    match cmd.as_str() {
        "log" => parse_log(&args),
        "criteria" => parse_criteria(&args).map(Command::Criteria),
        "filter" => parse_filter(&args, now).map(Command::Filter),
        "load" => parse_load(&args).map(Command::Load),
        "expand" => parse_target("expand", &args).map(Command::Expand),
        "collapse" => parse_target("collapse", &args).map(Command::Collapse),
        "select" => {
            if args.is_empty() {
                return Err("select needs at least one term, e.g. all.diff.files".into());
            }
            args.iter()
                .map(|a| parse_select(a))
                .collect::<Result<_, _>>()
                .map(Command::Select)
        }
        "compare" => match args.as_slice() {
            [] => Ok(Command::Compare(None)),
            [m] => content_method(m)
                .map(|m| Command::Compare(Some(m)))
                .ok_or_else(|| format!("unknown compare method `{m}` (crc, binary, rules-based)")),
            _ => Err("compare takes at most one method".into()),
        },
        "folder-report" => parse_folder_report(&args).map(Command::FolderReport),
        "file-report" => parse_file_report(ReportKind::File, &args),
        "text-report" => parse_file_report(ReportKind::Text, &args),
        "hex-report" => parse_file_report(ReportKind::Hex, &args),
        "data-report" | "table-report" => parse_file_report(ReportKind::Data, &args),
        "picture-report" => parse_file_report(ReportKind::Picture, &args),
        "media-report" | "mp3-report" => parse_file_report(ReportKind::Media, &args),
        "registry-report" => parse_file_report(ReportKind::Registry, &args),
        "version-report" => parse_file_report(ReportKind::Version, &args),
        "sync" => parse_sync(&args),
        "copy" | "move" => {
            let remove_source = cmd == "move";
            match args.as_slice() {
                [] if !remove_source => Ok(Command::Copy {
                    dir: Direction::LeftToRight,
                    remove_source,
                }),
                [d] => parse_direction(d).map(|dir| Command::Copy { dir, remove_source }),
                _ => Err(format!(
                    "{cmd} takes one direction: left->right or right->left"
                )),
            }
        }
        "copyto" | "moveto" => parse_copyto(&cmd, &args),
        "delete" => parse_delete(&args),
        "rename" => parse_rename(&args),
        "touch" => parse_touch(&args).map(Command::Touch),
        "attrib" => parse_attrib(&args),
        "snapshot" => parse_snapshot(&args).map(Command::Snapshot),
        "beep" => match args.as_slice() {
            [] => Ok(Command::Beep),
            _ => Err("beep takes no arguments".into()),
        },
        "option" => match args.as_slice() {
            [o] => parse_option(o).map(Command::Option),
            _ => Err("option takes one setting, e.g. confirm:yes-to-all".into()),
        },
        other => Err(format!("unknown command `{other}`")),
    }
}

fn parse_log(args: &[&str]) -> Result<Command, String> {
    let mut level = None;
    let mut rest = args;
    if let Some((first, tail)) = rest.split_first() {
        level = match first.to_ascii_lowercase().as_str() {
            "none" => Some(LogLevel::None),
            "normal" => Some(LogLevel::Normal),
            "verbose" => Some(LogLevel::Verbose),
            _ => None,
        };
        if level.is_some() {
            rest = tail;
        }
    }
    let level = level.unwrap_or(LogLevel::Normal);
    let (append, path) = match rest {
        [] => (false, None),
        [file] => match key_value(file) {
            Some((k, v)) if k == "append" => (true, Some(v.to_owned())),
            _ => (false, Some((*file).to_owned())),
        },
        _ => {
            return Err(
                "log takes a level and one file: log [none|normal|verbose] [[append:]<file>]"
                    .into(),
            )
        }
    };
    if path.as_deref() == Some("") {
        return Err("log needs a file name after append:".into());
    }
    Ok(Command::Log {
        level,
        append,
        path,
    })
}

fn content_method(s: &str) -> Option<ContentMethod> {
    match s.to_ascii_lowercase().as_str() {
        "crc" => Some(ContentMethod::Crc),
        "binary" => Some(ContentMethod::Binary),
        "rules" | "rules-based" => Some(ContentMethod::Rules),
        "version" => Some(ContentMethod::Version),
        _ => None,
    }
}

/// `left+6`, `right-3`, `ignore`.
fn parse_timezone(v: &str) -> Result<Timezone, String> {
    let lower = v.to_ascii_lowercase();
    if lower == "ignore" {
        return Ok(Timezone::Ignore);
    }
    let bad = || format!("timezone takes ignore, left±<0..12> or right±<0..12>, not `{v}`");
    let (left, rest) = if let Some(r) = lower.strip_prefix("left").or(lower.strip_prefix("lt")) {
        (true, r)
    } else if let Some(r) = lower.strip_prefix("right").or(lower.strip_prefix("rt")) {
        (false, r)
    } else {
        return Err(bad());
    };
    let (sign, n) = match rest.split_at_checked(1) {
        Some(("+", n)) => (1, n),
        Some(("-", n)) => (-1, n),
        _ => return Err(bad()),
    };
    let hours: i32 = n.parse().ok().filter(|h| (0..=12).contains(h)).ok_or_else(bad)?;
    Ok(Timezone::Offset {
        left,
        hours: sign * hours,
    })
}

fn parse_criteria(args: &[&str]) -> Result<Criteria, String> {
    if args.is_empty() {
        return Err(
            "criteria needs at least one of timestamp, size, crc, binary, rules-based".into(),
        );
    }
    let mut c = Criteria {
        timestamp: false,
        size: false,
        follow_links: false,
        ignore_unimportant: false,
        ..Criteria::default()
    };
    for a in args {
        let lower = a.to_ascii_lowercase();
        let (word, opts) = lower.split_once(':').unwrap_or((&lower, ""));
        match word {
            "timestamp" => {
                c.timestamp = true;
                for o in opts.split([';', ',']).filter(|o| !o.is_empty()) {
                    if o == "ignoredst" {
                        c.ignore_dst = true;
                    } else if let Some(n) =
                        o.strip_suffix("sec").and_then(|n| n.parse::<u64>().ok())
                    {
                        c.tolerance_secs = n;
                    } else if let Some(n) =
                        o.strip_suffix("min").and_then(|n| n.parse::<u64>().ok())
                    {
                        c.tolerance_secs = n * 60;
                    } else {
                        return Err(format!(
                            "unknown timestamp option `{o}` (e.g. timestamp:2sec;IgnoreDST)"
                        ));
                    }
                }
            }
            "size" => c.size = true,
            // `attrib:sh` names the attributes compared; the engine compares
            // read-only, hidden and system together.
            "attrib" | "attributes" => {
                parse_attr_set(opts)?;
                c.attributes = true;
            }
            "timezone" => c.timezone = parse_timezone(opts)?,
            "follow-symlinks" => c.follow_links = true,
            "ignore-unimportant" => c.ignore_unimportant = true,
            _ => match content_method(word) {
                Some(m) if opts.is_empty() => {
                    // `version` adds to a content method rather than replacing it.
                    if m != ContentMethod::Version || c.content == ContentMethod::Off {
                        c.content = m;
                    }
                }
                _ => return Err(format!("unknown criterion `{a}`")),
            },
        }
    }
    Ok(c)
}

/// `[a][s][h][r]` → attribute bits.
fn parse_attr_set(s: &str) -> Result<u32, String> {
    s.chars().try_fold(0, |bits, c| {
        Ok(bits
            | match c.to_ascii_lowercase() {
                'a' => attr::ARCHIVE,
                's' => attr::SYSTEM,
                'h' => attr::HIDDEN,
                'r' => attr::READONLY,
                other => return Err(format!("unknown attribute `{other}` (a, s, h, r)")),
            })
    })
}

/// `+sh -a` / `+r-h` → (set, clear).
fn parse_attr_changes<'a>(parts: impl IntoIterator<Item = &'a str>) -> Result<(u32, u32), String> {
    let (mut set, mut clear) = (0, 0);
    let mut any = false;
    for part in parts {
        let mut sign = None;
        let mut run = String::new();
        let mut flush = |sign: Option<char>, run: &mut String| -> Result<(), String> {
            if run.is_empty() {
                return Ok(());
            }
            let bits = parse_attr_set(run)?;
            match sign {
                Some('+') => set |= bits,
                Some('-') => clear |= bits,
                _ => return Err("attributes need + or - before them, e.g. +r -h".into()),
            }
            run.clear();
            Ok(())
        };
        for c in part.chars() {
            if c == '+' || c == '-' {
                flush(sign, &mut run)?;
                sign = Some(c);
                any = true;
            } else {
                run.push(c);
            }
        }
        flush(sign, &mut run)?;
    }
    if !any || (set == 0 && clear == 0) {
        return Err("expected attributes such as +r or -sh".into());
    }
    Ok((set, clear))
}

/// Turns a script folder mask (`obj\`, `\src\gen\`) into a glob.
fn folder_glob(mask: &str) -> String {
    mask.replace('\\', "/").trim_matches('/').to_owned()
}

fn parse_name_filter(masks: &[&str]) -> Filter {
    let mut f = Filter::default();
    let mut include_folders = Vec::new();
    for mask in masks
        .iter()
        .flat_map(|a| a.split(';'))
        .map(str::trim)
        .filter(|m| !m.is_empty())
    {
        let (exclude, mask) = match mask.strip_prefix('-') {
            Some(m) => (true, m.trim()),
            None => (false, mask.strip_prefix('+').unwrap_or(mask)),
        };
        let folder = mask.ends_with('\\') || mask.ends_with('/');
        match (exclude, folder) {
            (true, true) => f.exclude_folders.push(folder_glob(mask)),
            (true, false) => f.exclude_files.push(mask.to_owned()),
            (false, true) => include_folders.push(folder_glob(mask)),
            // `*` and `*.*` include everything, i.e. don't restrict.
            (false, false) if mask == "*" || mask == "*.*" => {}
            (false, false) => f.include_files.push(mask.to_owned()),
        }
    }
    if !include_folders.is_empty() {
        // Only files inside an included folder (at any depth), matching the
        // file masks if there are any.
        let files = if f.include_files.is_empty() {
            vec!["*".to_owned()]
        } else {
            std::mem::take(&mut f.include_files)
        };
        for dir in &include_folders {
            for file in &files {
                f.include_files.push(format!("{dir}/**/{file}"));
                f.include_files.push(format!("**/{dir}/**/{file}"));
            }
        }
    }
    f
}

/// `<`/`>` prefix: (`true` for `<` or none, rest).
fn comparison_prefix(v: &str) -> (bool, &str) {
    match v.strip_prefix('<') {
        Some(r) => (true, r.trim_start()),
        None => match v.strip_prefix('>') {
            Some(r) => (false, r.trim_start()),
            None => (true, v),
        },
    }
}

/// `10`, `10KB`, `1.5MB`.
fn parse_size(v: &str) -> Option<u64> {
    let upper = v.trim().to_ascii_uppercase();
    let (num, mult) = [
        ("TB", 1u64 << 40),
        ("GB", 1 << 30),
        ("MB", 1 << 20),
        ("KB", 1 << 10),
        ("B", 1),
    ]
    .iter()
    .find_map(|(suffix, m)| upper.strip_suffix(suffix).map(|n| (n.trim(), *m)))
    .unwrap_or((upper.as_str(), 1));
    let n: f64 = num.parse().ok().filter(|n: &f64| *n >= 0.0)?;
    Some((n * mult as f64).round() as u64)
}

/// A date with an optional time, read leniently: ISO (`2012-12-31 13:00`),
/// US (`12/31/2012 12:00pm`) or with a month name (`Dec 31, 2012`), in
/// local time.
pub fn parse_timestamp(s: &str) -> Option<i64> {
    let s = s.trim();
    let mut norm = s.replace(',', " ");
    // ISO `2012-12-31T13:00`.
    if norm.len() > 10 && norm.as_bytes()[10] == b'T' {
        norm.replace_range(10..11, " ");
    }
    let words: Vec<&str> = norm.split_whitespace().collect();
    const DATES: &[&str] = &["%Y-%m-%d", "%m/%d/%Y", "%d.%m.%Y", "%Y/%m/%d", "%m-%d-%Y"];
    const TIMES: &[&str] = &["%H:%M:%S", "%H:%M", "%I:%M:%S%p", "%I:%M%p", "%I%p"];
    // The date takes the first one to three words; the rest is the time.
    for take in (1..=words.len().min(3)).rev() {
        let date_text = words[..take].join(" ");
        let date = DATES
            .iter()
            .chain(&["%b %d %Y", "%B %d %Y", "%d %b %Y", "%d %B %Y"])
            .find_map(|f| NaiveDate::parse_from_str(&date_text, f).ok());
        let Some(date) = date else { continue };
        let time_text = words[take..].join("").to_ascii_uppercase();
        let time = if time_text.is_empty() {
            NaiveTime::MIN
        } else {
            TIMES
                .iter()
                .find_map(|f| NaiveTime::parse_from_str(&time_text, f).ok())?
        };
        let dt = NaiveDateTime::new(date, time);
        return Local
            .from_local_datetime(&dt)
            .earliest()
            .map(|t| t.timestamp_millis());
    }
    None
}

fn parse_when(v: &str, now: &DateTime<Local>) -> Option<When> {
    let lower = v.trim().to_ascii_lowercase();
    if lower == "now" {
        return Some(When::At(now.timestamp_millis()));
    }
    if let Some(n) = lower
        .strip_suffix("days")
        .or_else(|| lower.strip_suffix("day"))
        .or(Some(&lower))
        .and_then(|n| n.trim().parse::<u32>().ok())
    {
        return Some(When::DaysAgo(n));
    }
    parse_timestamp(v).map(When::At)
}

fn parse_filter(args: &[&str], now: &DateTime<Local>) -> Result<FilterCmd, String> {
    if args.is_empty() {
        return Err("filter needs masks, e.g. filter \"*.rs;-target\\\"".into());
    }
    let mut f = FilterCmd::default();
    let mut masks = Vec::new();
    for a in args {
        let lower = a.to_ascii_lowercase();
        match key_value(a) {
            Some((k, v)) if k == "cutoff" => {
                f.cutoff = Some(if v.eq_ignore_ascii_case("none") {
                    None
                } else {
                    let (older, rest) = comparison_prefix(v);
                    let when = parse_when(rest, now).ok_or_else(|| {
                        format!("cannot read cutoff `{v}` (e.g. cutoff:<7days or cutoff:>\"2012-12-31\")")
                    })?;
                    Some(Cutoff { older, when })
                })
            }
            Some((k, v)) if k == "size" => {
                f.size = Some(if v.eq_ignore_ascii_case("none") {
                    None
                } else {
                    let (smaller, rest) = comparison_prefix(v);
                    let bytes = parse_size(rest)
                        .ok_or_else(|| format!("cannot read size `{v}` (e.g. size:>10MB)"))?;
                    Some(SizeLimit { smaller, bytes })
                })
            }
            Some((k, v)) if k == "attrib" => {
                f.attrib = Some(if v.eq_ignore_ascii_case("none") {
                    None
                } else {
                    let (required, excluded) = parse_attr_changes([v])?;
                    Some(AttribFilter { required, excluded })
                })
            }
            _ if lower == "exclude-protected" => f.exclude_protected = Some(true),
            _ if lower == "include-protected" => f.exclude_protected = Some(false),
            _ => masks.push(*a),
        }
    }
    if !masks.is_empty() {
        f.names = Some(parse_name_filter(&masks));
    }
    Ok(f)
}

fn parse_sides(s: &str) -> Option<Sides> {
    match s.to_ascii_lowercase().as_str() {
        "left" | "lt" => Some(Sides::Left),
        "right" | "rt" => Some(Sides::Right),
        "all" | "both" => Some(Sides::Both),
        _ => None,
    }
}

fn parse_load(args: &[&str]) -> Result<Load, String> {
    let mut create = None;
    let mut paths = Vec::new();
    for a in args {
        match key_value(a) {
            Some((k, v)) if k == "create" => {
                create = Some(parse_sides(v).ok_or_else(|| {
                    format!("create takes all, left or right, not `{v}`")
                })?)
            }
            _ => paths.push(*a),
        }
    }
    match paths.as_slice() {
        [d] if d.eq_ignore_ascii_case("<default>") && create.is_none() => Ok(Load::Default),
        [l] => Ok(Load::Paths {
            create,
            left: (*l).to_owned(),
            right: None,
        }),
        [l, r] => Ok(Load::Paths {
            create,
            left: (*l).to_owned(),
            right: Some((*r).to_owned()),
        }),
        [] => Err("load needs a session, <default>, or a left and right path".into()),
        _ => Err("load takes at most two paths".into()),
    }
}

fn parse_target(cmd: &str, args: &[&str]) -> Result<Vec<String>, String> {
    match args {
        [] => Err(format!("{cmd} needs `all` or folders")),
        [a] if a.eq_ignore_ascii_case("all") => Ok(Vec::new()),
        _ => Ok(args
            .iter()
            .map(|a| a.replace('\\', "/").trim_matches('/').to_owned())
            .collect()),
    }
}

/// `[left|right|all].[all|diff|same|exact|orphan|newer|older].[files|folders]`,
/// any part optional; `all` alone selects everything; `empty.folders`.
fn parse_select(term: &str) -> Result<SelectTerm, String> {
    let lower = term.to_ascii_lowercase();
    if lower == "empty.folders" {
        return Ok(SelectTerm {
            side: SelSide::Any,
            status: SelStatus::Empty,
            kind: SelKind::Folders,
        });
    }
    let parts: Vec<&str> = lower.split('.').filter(|p| !p.is_empty()).collect();
    let mut t = SelectTerm {
        side: SelSide::Any,
        status: SelStatus::All,
        kind: SelKind::Both,
    };
    let (mut side_set, mut status_set, mut kind_set) = (false, false, false);
    for (i, p) in parts.iter().enumerate() {
        let multi = parts.len() > 1;
        match *p {
            "left" | "lt" if !side_set && !status_set => (t.side, side_set) = (SelSide::Left, true),
            "right" | "rt" if !side_set && !status_set => {
                (t.side, side_set) = (SelSide::Right, true)
            }
            // `all.diff`: a leading `all` is the side when more parts follow.
            "all" if i == 0 && multi && !side_set => side_set = true,
            "all" | "any" if !status_set => (t.status, status_set) = (SelStatus::All, true),
            // `all.diff.all`: a trailing `all` is the kind.
            "all" if !kind_set => kind_set = true,
            "diff" | "mismatch" | "different" if !status_set => {
                (t.status, status_set) = (SelStatus::Diff, true)
            }
            "same" | "match" if !status_set => (t.status, status_set) = (SelStatus::Same, true),
            "exact" if !status_set => (t.status, status_set) = (SelStatus::Exact, true),
            "orphan" | "orphans" if !status_set => {
                (t.status, status_set) = (SelStatus::Orphan, true)
            }
            "newer" if !status_set => (t.status, status_set) = (SelStatus::Newer, true),
            "older" if !status_set => (t.status, status_set) = (SelStatus::Older, true),
            "files" | "file" if !kind_set => (t.kind, kind_set) = (SelKind::Files, true),
            "folders" | "folder" if !kind_set => (t.kind, kind_set) = (SelKind::Folders, true),
            _ => return Err(format!("cannot read selection `{term}` at `{p}`")),
        }
    }
    if parts.is_empty() {
        return Err("empty selection term".into());
    }
    Ok(t)
}

fn output_format(v: &str) -> Option<OutputFormat> {
    match v.to_ascii_lowercase().as_str() {
        "html-color" | "html" | "html-custom" => Some(OutputFormat::HtmlColor),
        "html-mono" => Some(OutputFormat::HtmlMono),
        "text" | "plain" | "text-plain" => Some(OutputFormat::Text),
        "csv" => Some(OutputFormat::Csv),
        "xml" => Some(OutputFormat::Xml),
        _ => None,
    }
}

/// Output options that only matter for printing (accepted and ignored).
fn print_option(v: &str) -> bool {
    matches!(
        v.to_ascii_lowercase().as_str(),
        "print-color" | "print-mono" | "print-portrait" | "print-landscape"
    )
}

/// Reports are plain text unless an `html-*` output option asks
/// for HTML; PCompare also goes by the extension (`.html`, `.csv`, `.xml`).
fn format_for(path: &str) -> OutputFormat {
    let lower = path.to_ascii_lowercase();
    if lower.ends_with(".html") || lower.ends_with(".htm") {
        OutputFormat::HtmlColor
    } else if lower.ends_with(".csv") {
        OutputFormat::Csv
    } else if lower.ends_with(".xml") {
        OutputFormat::Xml
    } else {
        OutputFormat::Text
    }
}

fn output_target(v: &str) -> Result<OutputTarget, String> {
    match v.to_ascii_lowercase().as_str() {
        "" => Err("output-to needs a file".into()),
        "printer" => Ok(OutputTarget::Printer),
        "clipboard" => Ok(OutputTarget::Clipboard),
        _ => Ok(OutputTarget::File(v.to_owned())),
    }
}

/// The arguments every report shares.
#[derive(Default)]
struct Common {
    title: String,
    output: Option<OutputTarget>,
    format: Option<OutputFormat>,
    wrap: Option<bool>,
    /// Arguments that aren't `key:value`: the `<comparison>`.
    loose: Vec<String>,
}

impl Common {
    /// Takes `a` if it is a common argument; `Ok(false)` leaves it to the
    /// command.
    fn take(&mut self, a: &str) -> Result<bool, String> {
        let Some((k, v)) = key_value(a) else {
            self.loose.push(a.to_owned());
            return Ok(true);
        };
        match k.as_str() {
            "title" => self.title = v.to_owned(),
            "output-to" => self.output = Some(output_target(v)?),
            "output-options" => {
                for o in v.split(',').map(str::trim).filter(|o| !o.is_empty()) {
                    match o.to_ascii_lowercase().as_str() {
                        "wrap-word" | "wrap-character" => self.wrap = Some(true),
                        "wrap-none" => self.wrap = Some(false),
                        _ if print_option(o) => {}
                        _ => {
                            // `html-custom` may be followed by a style sheet name.
                            if self.format == Some(OutputFormat::HtmlColor)
                                && !o.contains('-')
                                && output_format(o).is_none()
                            {
                                continue;
                            }
                            self.format = Some(
                                output_format(o)
                                    .ok_or_else(|| format!("unknown output option `{o}`"))?,
                            )
                        }
                    }
                }
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    fn output(&self, cmd: &str) -> Result<OutputTarget, String> {
        self.output
            .clone()
            .ok_or_else(|| format!("{cmd} needs output-to:<file>"))
    }

    fn format(&self, output: &OutputTarget) -> OutputFormat {
        self.format.unwrap_or(match output {
            OutputTarget::File(p) => format_for(p),
            _ => OutputFormat::Text,
        })
    }
}

fn parse_folder_report(args: &[&str]) -> Result<FolderReport, String> {
    let mut layout = FolderLayout::Summary;
    let mut filter = DisplayFilter::All;
    let mut file_links = false;
    let mut columns: Option<Columns> = None;
    let mut common = Common::default();
    for a in args {
        if common.take(a)? {
            continue;
        }
        let (k, v) = key_value(a).expect("common.take keeps key:value args");
        match k.as_str() {
            "layout" => {
                layout = match v.to_ascii_lowercase().as_str() {
                    "summary" => FolderLayout::Summary,
                    "side-by-side" | "detailed" => FolderLayout::SideBySide,
                    "xml" => FolderLayout::Xml,
                    other => {
                        return Err(format!(
                            "unknown folder-report layout `{other}` (summary, side-by-side, xml)"
                        ))
                    }
                }
            }
            "options" => {
                for o in v.split(',').map(str::trim).filter(|o| !o.is_empty()) {
                    let o = o.to_ascii_lowercase();
                    if let Some(col) = o.strip_prefix("column-") {
                        let c = columns.get_or_insert(Columns {
                            size: false,
                            timestamp: false,
                            ..Columns::default()
                        });
                        match col {
                            "size" => c.size = true,
                            "timestamp" => c.timestamp = true,
                            "crc" => c.crc = true,
                            "attributes" | "attrib" => c.attributes = true,
                            "version" => c.version = true,
                            "owner" => c.owner = true,
                            "group" => c.group = true,
                            "vcs" => c.vcs = true,
                            "none" => {}
                            _ => return Err(format!("unknown folder-report column `{o}`")),
                        }
                        continue;
                    }
                    filter = match o.as_str() {
                        "display-all" => DisplayFilter::All,
                        "display-mismatches" => DisplayFilter::Diffs,
                        "display-mismatches-no-orphans" | "display-no-orphans" => {
                            DisplayFilter::DiffsNoOrphans
                        }
                        "display-matches" => DisplayFilter::Same,
                        "display-orphans" => DisplayFilter::Orphans,
                        "display-left-orphans" => DisplayFilter::LeftOrphans,
                        "display-right-orphans" => DisplayFilter::RightOrphans,
                        "display-left-newer" | "display-left-newer-orphans" => {
                            DisplayFilter::LeftNewer
                        }
                        "display-right-newer" | "display-right-newer-orphans" => {
                            DisplayFilter::RightNewer
                        }
                        "include-file-links" => {
                            file_links = true;
                            continue;
                        }
                        _ => return Err(format!("unknown folder-report option `{o}`")),
                    };
                }
            }
            other => return Err(format!("unknown folder-report argument `{other}`")),
        }
    }
    if let Some(extra) = common.loose.first() {
        return Err(format!(
            "folder-report reports the loaded folders; unexpected `{extra}`"
        ));
    }
    let output = common.output("folder-report")?;
    let format = if layout == FolderLayout::Xml {
        OutputFormat::Xml
    } else {
        match common.format(&output) {
            OutputFormat::Xml => OutputFormat::Text,
            f => f,
        }
    };
    if file_links && !matches!(format, OutputFormat::HtmlColor | OutputFormat::HtmlMono) {
        return Err("include-file-links needs an HTML report".into());
    }
    Ok(FolderReport {
        layout,
        filter,
        file_links,
        columns: columns.unwrap_or_default(),
        format,
        title: common.title,
        output,
    })
}

fn parse_file_report(kind: ReportKind, args: &[&str]) -> Result<Command, String> {
    let cmd = kind.command();
    let mut r = FileReport {
        kind,
        layout: FileLayout::SideBySide,
        patch: PatchStyle::Normal,
        display: LineDisplay::All,
        line_numbers: false,
        ignore_unimportant: None,
        strikeout_left: false,
        strikeout_right: false,
        format: OutputFormat::Text,
        wrap: false,
        title: String::new(),
        output: OutputTarget::Clipboard,
        comparison: None,
    };
    let mut layout_given = false;
    let mut common = Common::default();
    for a in args {
        if common.take(a)? {
            continue;
        }
        let (k, v) = key_value(a).expect("common.take keeps key:value args");
        match k.as_str() {
            "layout" => {
                layout_given = true;
                r.layout = match v.to_ascii_lowercase().as_str() {
                    "side-by-side" => FileLayout::SideBySide,
                    "summary" => FileLayout::Summary,
                    "interleaved" | "over-under" | "composite" | "unified" => {
                        FileLayout::Interleaved
                    }
                    "patch" => FileLayout::Patch,
                    "statistics" => FileLayout::Statistics,
                    "xml" => FileLayout::Xml,
                    other => return Err(format!("unknown {cmd} layout `{other}`")),
                };
                if !kind.layouts().contains(&r.layout) {
                    return Err(format!("{cmd} has no `{v}` layout"));
                }
            }
            "options" => {
                for o in v.split(',').map(str::trim).filter(|o| !o.is_empty()) {
                    let o = o.to_ascii_lowercase();
                    match o.as_str() {
                        "display-all" => r.display = LineDisplay::All,
                        "display-mismatches" => r.display = LineDisplay::Mismatches,
                        "display-context" => r.display = LineDisplay::Context(3),
                        "display-matches" => r.display = LineDisplay::Matches,
                        "line-numbers" => r.line_numbers = true,
                        "ignore-unimportant" => r.ignore_unimportant = Some(true),
                        "strikeout-left-diffs" | "strikeout-left-orphans" => {
                            r.strikeout_left = true
                        }
                        "strikeout-right-diffs" | "strikeout-right-orphans" => {
                            r.strikeout_right = true
                        }
                        "patch-normal" => r.patch = PatchStyle::Normal,
                        "patch-context" => r.patch = PatchStyle::Context,
                        "patch-unified" => r.patch = PatchStyle::Unified,
                        // Deprecated: descriptive stats became the summary layout,
                        // tabular is what the statistics layout always is.
                        "stats-descriptive" => r.layout = FileLayout::Summary,
                        "stats-tabular" => {}
                        _ => match o
                            .strip_prefix("display-context=")
                            .and_then(|n| n.parse().ok())
                        {
                            Some(n) => r.display = LineDisplay::Context(n),
                            None => return Err(format!("unknown {cmd} option `{o}`")),
                        },
                    }
                }
            }
            other => return Err(format!("unknown {cmd} argument `{other}`")),
        }
    }
    if !layout_given {
        return Err(format!("{cmd} needs layout:<layout>"));
    }
    r.output = common.output(cmd)?;
    r.title = common.title.clone();
    r.wrap = common.wrap.unwrap_or(false);
    r.format = match r.layout {
        FileLayout::Xml => OutputFormat::Xml,
        FileLayout::Statistics => OutputFormat::Csv,
        FileLayout::Patch => OutputFormat::Text,
        _ => match common.format(&r.output) {
            OutputFormat::Csv | OutputFormat::Xml => OutputFormat::Text,
            f => f,
        },
    };
    r.comparison = match common.loose.as_slice() {
        [] => None,
        [s] => Some(Comparison::Session(s.clone())),
        [l, rt] => Some(Comparison::Pair(l.clone(), rt.clone())),
        more => {
            return Err(format!(
                "{cmd} takes a session name or two files to compare, got {} extra arguments",
                more.len()
            ))
        }
    };
    Ok(Command::FileReport(r))
}

fn parse_direction(s: &str) -> Result<Direction, String> {
    match s.to_ascii_lowercase().as_str() {
        "left->right" | "lt->rt" | "left->rt" | "lt->right" => Ok(Direction::LeftToRight),
        "right->left" | "rt->lt" | "right->lt" | "rt->left" => Ok(Direction::RightToLeft),
        other => Err(format!(
            "expected left->right or right->left, got `{other}`"
        )),
    }
}

fn parse_sync(args: &[&str]) -> Result<Command, String> {
    let mut create_empty = false;
    let mut visible = false;
    let mut mode = None;
    for a in args {
        if a.eq_ignore_ascii_case("create-empty") {
            create_empty = true;
            continue;
        }
        if a.eq_ignore_ascii_case("visible") {
            visible = true;
            continue;
        }
        let (kind, dir) = key_value(a).ok_or_else(|| {
            format!("expected mirror:<direction> or update:<direction>, got `{a}`")
        })?;
        let all = dir.eq_ignore_ascii_case("all");
        mode = Some(match (kind.as_str(), all) {
            ("update", true) => SyncMode::UpdateBoth,
            ("mirror", true) => {
                return Err(
                    "mirror needs a direction: mirror:left->right or mirror:right->left".into(),
                )
            }
            ("update", false) => match parse_direction(dir)? {
                Direction::LeftToRight => SyncMode::UpdateLeftToRight,
                Direction::RightToLeft => SyncMode::UpdateRightToLeft,
            },
            ("mirror", false) => match parse_direction(dir)? {
                Direction::LeftToRight => SyncMode::MirrorLeftToRight,
                Direction::RightToLeft => SyncMode::MirrorRightToLeft,
            },
            (other, _) => return Err(format!("unknown sync type `{other}` (mirror, update)")),
        });
    }
    let mode = mode.ok_or("sync needs mirror:<direction> or update:<direction|all>")?;
    Ok(Command::Sync {
        mode,
        create_empty,
        visible,
    })
}

fn parse_copyto(cmd: &str, args: &[&str]) -> Result<Command, String> {
    let mut sides = Sides::Both;
    let mut path = PathMode::None;
    let mut target = None;
    for (i, a) in args.iter().enumerate() {
        if let Some(s) = parse_sides(a).filter(|_| i == 0) {
            sides = s;
            continue;
        }
        match key_value(a) {
            Some((k, v)) if k == "path" => {
                path = match v.to_ascii_lowercase().as_str() {
                    "none" => PathMode::None,
                    "relative" => PathMode::Relative,
                    "base" => PathMode::Base,
                    other => {
                        return Err(format!("path takes relative, base or none, not `{other}`"))
                    }
                }
            }
            _ if target.is_none() => target = Some((*a).to_owned()),
            _ => return Err(format!("{cmd} takes one target folder; unexpected `{a}`")),
        }
    }
    let target = target.ok_or_else(|| {
        format!("{cmd} needs a target: {cmd} [left|right|all] [path:relative|base|none] <path>")
    })?;
    Ok(Command::CopyTo {
        sides,
        path,
        target,
        remove_source: cmd == "moveto",
    })
}

fn parse_delete(args: &[&str]) -> Result<Command, String> {
    let mut sides = None;
    let mut recycle = None;
    for a in args {
        let lower = a.to_ascii_lowercase();
        if let Some(v) = lower
            .strip_prefix("recyclebin=")
            .or(lower.strip_prefix("recyclebin:"))
        {
            recycle = Some(match v {
                "yes" | "on" | "true" => true,
                "no" | "off" | "false" => false,
                _ => return Err(format!("recyclebin takes yes or no, not `{v}`")),
            });
            continue;
        }
        match parse_sides(a) {
            Some(s) if sides.is_none() => sides = Some(s),
            _ => return Err(format!("delete takes [recyclebin=yes|no] left, right or all, not `{a}`")),
        }
    }
    Ok(Command::Delete {
        sides: sides.unwrap_or(Sides::Both),
        recycle,
    })
}

fn parse_rename(args: &[&str]) -> Result<Command, String> {
    match args {
        [new] => Ok(Command::Rename {
            regex: None,
            new: (*new).to_owned(),
        }),
        [kw, old, new] if kw.eq_ignore_ascii_case("regexpr") || kw.eq_ignore_ascii_case("regex") => {
            regex::Regex::new(old).map_err(|e| format!("bad regular expression `{old}`: {e}"))?;
            Ok(Command::Rename {
                regex: Some((*old).to_owned()),
                new: (*new).to_owned(),
            })
        }
        _ => Err("rename takes a new mask, or regexpr <old> <new>".into()),
    }
}

fn parse_touch(args: &[&str]) -> Result<Touch, String> {
    let usage = "touch takes left->right, right->left, or (left|right|all):(now|<timestamp>)";
    let [a] = args else {
        // `touch left:12/31/2012 12:00pm` without quotes splits in two.
        if args.len() > 1 {
            return parse_touch(&[&args.join(" ")]);
        }
        return Err(usage.into());
    };
    if let Ok(d) = parse_direction(a) {
        return Ok(Touch::Copy(d));
    }
    let (side, time) = a.split_once(':').ok_or(usage)?;
    let sides = parse_sides(side).ok_or(usage)?;
    let at = if time.trim().eq_ignore_ascii_case("now") {
        None
    } else {
        Some(parse_timestamp(time).ok_or_else(|| format!("cannot read timestamp `{time}`"))?)
    };
    Ok(Touch::Set { sides, at })
}

fn parse_attrib(args: &[&str]) -> Result<Command, String> {
    let (set, clear) = parse_attr_changes(args.iter().copied())?;
    if set & clear != 0 {
        return Err("an attribute can't be both set and cleared".into());
    }
    Ok(Command::Attrib { set, clear })
}

fn parse_snapshot(args: &[&str]) -> Result<Snapshot, String> {
    let mut s = Snapshot {
        save_crc: false,
        save_version: false,
        expand_archives: false,
        follow_symlinks: false,
        include_empty: false,
        no_filters: false,
        source: SnapshotSource::Left,
        output: None,
    };
    let mut source = None;
    for a in args {
        match a.to_ascii_lowercase().as_str() {
            "save-crc" => s.save_crc = true,
            "save-version" => s.save_version = true,
            "expand-archives" => s.expand_archives = true,
            "follow-symlinks" => s.follow_symlinks = true,
            "include-empty" => s.include_empty = true,
            "no-filters" => s.no_filters = true,
            "left" | "lt" => source = Some(SnapshotSource::Left),
            "right" | "rt" => source = Some(SnapshotSource::Right),
            _ => match key_value(a) {
                Some((k, v)) if k == "path" && !v.is_empty() => {
                    source = Some(SnapshotSource::Path(v.to_owned()))
                }
                Some((k, v)) if k == "output" && !v.is_empty() => s.output = Some(v.to_owned()),
                _ => return Err(format!("unknown snapshot argument `{a}`")),
            },
        }
    }
    s.source = source.ok_or("snapshot needs left, right or path:<folder>")?;
    Ok(s)
}

fn parse_option(o: &str) -> Result<ScriptOption, String> {
    match o.to_ascii_lowercase().as_str() {
        "confirm:yes-to-all" => Ok(ScriptOption::Confirm(Confirm::YesToAll)),
        "confirm:no-to-all" => Ok(ScriptOption::Confirm(Confirm::NoToAll)),
        "confirm:prompt" => Ok(ScriptOption::Confirm(Confirm::Prompt)),
        "stop-on-error" => Ok(ScriptOption::StopOnError(true)),
        "continue-on-error" => Ok(ScriptOption::StopOnError(false)),
        "recycle:yes" | "recycle:on" => Ok(ScriptOption::Recycle(true)),
        "recycle:no" | "recycle:off" => Ok(ScriptOption::Recycle(false)),
        other => Err(format!(
            "unknown option `{other}` (confirm:yes-to-all|no-to-all|prompt, stop-on-error, recycle:yes|no)"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_env(_: &str) -> Option<String> {
        None
    }

    fn parse(text: &str) -> Vec<Command> {
        parse_script(text, &[], &no_env)
            .unwrap_or_else(|e| panic!("{e:?}"))
            .into_iter()
            .map(|s| s.command)
            .collect()
    }

    fn parse_err(text: &str) -> String {
        parse_script(text, &[], &no_env).unwrap_err()[0].message.clone()
    }

    fn load(left: &str, right: Option<&str>) -> Command {
        Command::Load(Load::Paths {
            create: None,
            left: left.into(),
            right: right.map(Into::into),
        })
    }

    #[test]
    fn tokenizer_keeps_quoted_spaces_and_drops_quotes() {
        assert_eq!(
            tokenize(r#"load "C:\My Docs\a" b  output-to:"x y.html""#).unwrap(),
            ["load", r"C:\My Docs\a", "b", "output-to:x y.html"]
        );
        assert_eq!(tokenize(r#"x """#).unwrap(), ["x", ""]);
        assert!(tokenize("load \"a").is_err());
    }

    #[test]
    fn variables_expand_parameters_and_environment() {
        let env = |n: &str| (n == "HOME").then(|| "/home/me".to_owned());
        let params = ["one".to_owned(), "two".to_owned()];
        assert_eq!(expand_vars("load %1 %2 %3", &params, &env), "load one two ");
        assert_eq!(
            expand_vars("log %HOME%/x %NOPE% 100%", &params, &env),
            "log /home/me/x %NOPE% 100%"
        );
    }

    #[test]
    fn date_and_time_variables() {
        let now = Local.with_ymd_and_hms(2012, 12, 31, 9, 5, 7).unwrap();
        assert_eq!(builtin_var("date", &now).as_deref(), Some("2012-12-31"));
        assert_eq!(builtin_var("TIME", &now).as_deref(), Some("09:05:07"));
        assert_eq!(builtin_var("fn_time", &now).as_deref(), Some("09-05-07"));
        let stmts = parse_script_at(
            "log verbose append:\"C:\\My Log %date% %fn_time%.txt\"",
            &[],
            &no_env,
            &now,
        )
        .unwrap();
        assert_eq!(
            stmts[0].command,
            Command::Log {
                level: LogLevel::Verbose,
                append: true,
                path: Some("C:\\My Log 2012-12-31 09-05-07.txt".into())
            }
        );
    }

    #[test]
    fn comments_blank_lines_and_case_are_ignored() {
        let cmds = parse("# a comment\n\n  LOAD a b # trailing comment\nExpand ALL\n");
        assert_eq!(cmds, [load("a", Some("b")), Command::Expand(Vec::new())]);
        // `#` inside quotes or a word is not a comment.
        let cmds = parse("filter \"-#tmp#;C#\\\"\nload C#dir d\n");
        let Command::Filter(f) = &cmds[0] else {
            panic!()
        };
        assert_eq!(f.names.as_ref().unwrap().exclude_files, ["#tmp#"]);
        assert_eq!(cmds[1], load("C#dir", Some("d")));
    }

    #[test]
    fn ampersand_continues_a_command() {
        let stmts = parse_script(
            "file-report layout:side-by-side &\n options:display-mismatches & # why\n output-to:\"%3\" \"%1\" \"%2\"\nbeep\n",
            &["a.txt".into(), "b.txt".into(), "r.txt".into()],
            &no_env,
        )
        .unwrap();
        assert_eq!(stmts.len(), 2);
        assert_eq!((stmts[0].line, stmts[1].line), (1, 4));
        let Command::FileReport(r) = &stmts[0].command else {
            panic!()
        };
        assert_eq!(r.display, LineDisplay::Mismatches);
        assert_eq!(r.output, OutputTarget::File("r.txt".into()));
        assert_eq!(
            r.comparison,
            Some(Comparison::Pair("a.txt".into(), "b.txt".into()))
        );
        // An error inside a continued command points at its first line.
        let errs = parse_script("beep\nsync &\n  mirror:sideways\n", &[], &no_env).unwrap_err();
        assert_eq!(errs[0].line, 2);
    }

    #[test]
    fn criteria_and_filters() {
        let cmds = parse("criteria timestamp:5sec;IgnoreDST size rules-based\nfilter \"*.rs;*.toml;-*.bak;-target\\;-.git/\"");
        assert_eq!(
            cmds[0],
            Command::Criteria(Criteria {
                timestamp: true,
                tolerance_secs: 5,
                ignore_dst: true,
                size: true,
                content: ContentMethod::Rules,
                attributes: false,
                timezone: Timezone::Same,
                follow_links: false,
                ignore_unimportant: false,
            })
        );
        assert_eq!(
            cmds[1],
            Command::Filter(FilterCmd {
                names: Some(Filter {
                    include_files: vec!["*.rs".into(), "*.toml".into()],
                    exclude_files: vec!["*.bak".into()],
                    exclude_folders: vec!["target".into(), ".git".into()],
                }),
                ..Default::default()
            })
        );
        let Command::Filter(f) = &parse("filter \"src\\;*.rs\"")[0] else {
            panic!()
        };
        assert_eq!(
            f.names.as_ref().unwrap().include_files,
            ["src/**/*.rs", "**/src/**/*.rs"]
        );
    }

    #[test]
    fn criteria_timezone_and_extras() {
        let Command::Criteria(c) =
            &parse("criteria attrib:sh timestamp:2sec;IgnoreDST rules-based timezone:left+6 follow-symlinks ignore-unimportant version")[0]
        else {
            panic!()
        };
        assert!(c.attributes && c.follow_links && c.ignore_unimportant && c.ignore_dst);
        assert_eq!(c.content, ContentMethod::Rules);
        assert_eq!(
            c.timezone,
            Timezone::Offset {
                left: true,
                hours: 6
            }
        );
        let Command::Criteria(c) = &parse("criteria timezone:rt-3 crc")[0] else {
            panic!()
        };
        assert_eq!(
            c.timezone,
            Timezone::Offset {
                left: false,
                hours: -3
            }
        );
        assert_eq!(c.content, ContentMethod::Crc);
        let Command::Criteria(c) = &parse("criteria timezone:ignore")[0] else {
            panic!()
        };
        assert_eq!(c.timezone, Timezone::Ignore);
        assert!(parse_err("criteria timezone:left+13").contains("timezone"));
        assert!(parse_err("criteria attrib:x").contains("attribute"));
    }

    #[test]
    fn filter_cutoff_size_attrib_and_protected() {
        let now = Local.with_ymd_and_hms(2020, 6, 1, 12, 0, 0).unwrap();
        let p = |s: &str| match &parse_script_at(s, &[], &no_env, &now).unwrap()[0].command {
            Command::Filter(f) => f.clone(),
            other => panic!("{other:?}"),
        };
        assert_eq!(
            p("filter cutoff:<7days").cutoff,
            Some(Some(Cutoff {
                older: true,
                when: When::DaysAgo(7)
            }))
        );
        let dec31 = Local
            .with_ymd_and_hms(2012, 12, 31, 0, 0, 0)
            .unwrap()
            .timestamp_millis();
        assert_eq!(
            p("filter cutoff:>\"Dec 31, 2012\"").cutoff,
            Some(Some(Cutoff {
                older: false,
                when: When::At(dec31)
            }))
        );
        let noon = dec31 + 12 * 3600 * 1000;
        assert_eq!(
            p("filter cutoff:\"12/31/2012 12:00pm\"").cutoff,
            Some(Some(Cutoff {
                older: true,
                when: When::At(noon)
            }))
        );
        assert_eq!(p("filter cutoff:none").cutoff, Some(None));
        assert_eq!(
            p("filter size:>1.5KB").size,
            Some(Some(SizeLimit {
                smaller: false,
                bytes: 1536
            }))
        );
        assert_eq!(
            p("filter size:100").size,
            Some(Some(SizeLimit {
                smaller: true,
                bytes: 100
            }))
        );
        assert_eq!(
            p("filter attrib:-sh").attrib,
            Some(Some(AttribFilter {
                required: 0,
                excluded: attr::SYSTEM | attr::HIDDEN
            }))
        );
        // Masks and other filters in one command.
        let f = p("filter \"*.txt;-My Folder\\\" attrib:+r exclude-protected");
        assert_eq!(f.names.unwrap().exclude_folders, ["My Folder"]);
        assert_eq!(
            f.attrib,
            Some(Some(AttribFilter {
                required: attr::READONLY,
                excluded: 0
            }))
        );
        assert_eq!(f.exclude_protected, Some(true));
        assert_eq!(p("filter \"*.*\"").names, Some(Filter::default()));
        assert!(parse_err("filter cutoff:<soon").contains("cutoff"));
    }

    #[test]
    fn timestamps_are_read_leniently() {
        let at = |y, mo, d, h, mi| {
            Local
                .with_ymd_and_hms(y, mo, d, h, mi, 0)
                .unwrap()
                .timestamp_millis()
        };
        assert_eq!(parse_timestamp("2012-12-31"), Some(at(2012, 12, 31, 0, 0)));
        assert_eq!(
            parse_timestamp("2012-12-31 13:45"),
            Some(at(2012, 12, 31, 13, 45))
        );
        assert_eq!(
            parse_timestamp("12/31/2012 12:00 am"),
            Some(at(2012, 12, 31, 0, 0))
        );
        assert_eq!(
            parse_timestamp("December 31, 2012 1:30pm"),
            Some(at(2012, 12, 31, 13, 30))
        );
        assert_eq!(parse_timestamp("soon"), None);
    }

    #[test]
    fn load_forms() {
        assert_eq!(parse("load <default>")[0], Command::Load(Load::Default));
        assert_eq!(parse("load \"Sync Laptop\"")[0], load("Sync Laptop", None));
        assert_eq!(
            parse("load \"C:\\My Folder\" ftp://user@website.com/folder")[0],
            load("C:\\My Folder", Some("ftp://user@website.com/folder"))
        );
        assert_eq!(
            parse("load create:right a b")[0],
            Command::Load(Load::Paths {
                create: Some(Sides::Right),
                left: "a".into(),
                right: Some("b".into())
            })
        );
        assert!(parse_err("load a b c").contains("at most two"));
    }

    #[test]
    fn selection_terms() {
        let Command::Select(terms) =
            &parse("select all.diff.files left.orphan right.newer.folders all empty.folders lt.exact")[0]
        else {
            panic!()
        };
        let t = |side, status, kind| SelectTerm { side, status, kind };
        assert_eq!(
            terms,
            &[
                t(SelSide::Any, SelStatus::Diff, SelKind::Files),
                t(SelSide::Left, SelStatus::Orphan, SelKind::Both),
                t(SelSide::Right, SelStatus::Newer, SelKind::Folders),
                t(SelSide::Any, SelStatus::All, SelKind::Both),
                t(SelSide::Any, SelStatus::Empty, SelKind::Folders),
                t(SelSide::Left, SelStatus::Exact, SelKind::Both),
            ]
        );
        assert!(parse_script("select left.banana", &[], &no_env).is_err());
    }

    #[test]
    fn folder_reports() {
        let cmds = parse(
            "folder-report layout:side-by-side options:display-mismatches,include-file-links,column-size \
             title:\"My report\" output-to:\"out dir/r.html\"\n\
             folder-report layout:xml output-to:\"C:\\My Report.xml\"\n\
             folder-report layout:summary options:display-mismatches,column-attributes,column-timestamp,column-size,column-crc output-to:\"My Report.txt\"\n\
             folder-report layout:side-by-side options:column-none output-to:clipboard output-options:html-color\n",
        );
        assert_eq!(
            cmds[0],
            Command::FolderReport(FolderReport {
                layout: FolderLayout::SideBySide,
                filter: DisplayFilter::Diffs,
                file_links: true,
                columns: Columns {
                    size: true,
                    timestamp: false,
                    ..Columns::default()
                },
                format: OutputFormat::HtmlColor,
                title: "My report".into(),
                output: OutputTarget::File("out dir/r.html".into()),
            })
        );
        let Command::FolderReport(x) = &cmds[1] else {
            panic!()
        };
        assert_eq!((x.layout, x.format), (FolderLayout::Xml, OutputFormat::Xml));
        let Command::FolderReport(s) = &cmds[2] else {
            panic!()
        };
        assert!(s.columns.attributes && s.columns.crc && s.columns.size && s.columns.timestamp);
        assert_eq!(s.format, OutputFormat::Text);
        let Command::FolderReport(c) = &cmds[3] else {
            panic!()
        };
        assert_eq!(c.output, OutputTarget::Clipboard);
        assert!(!c.columns.size && !c.columns.timestamp);
        assert!(parse_err("folder-report layout:summary").contains("output-to"));
    }

    #[test]
    fn file_reports_and_their_comparison() {
        let cmds = parse(
            "text-report layout:patch options:patch-unified,display-context=1 output-to:a.diff\n\
             text-report layout:patch options:patch-context output-to:b.diff a.txt b.txt\n\
             text-report layout:interleaved options:display-context,strikeout-left-diffs output-to:printer output-options:print-color,wrap-word\n\
             file-report layout:summary output-to:clipboard output-options:wrap-word,html-color \"My Session\"\n\
             text-report layout:statistics output-to:s.csv\n\
             text-report layout:xml output-to:s.xml\n\
             hex-report layout:interleaved output-to:h.txt\n\
             data-report layout:interleaved output-to:printer output-options:print-color,print-landscape\n\
             version-report layout:side-by-side options:ignore-unimportant output-to:v.html\n",
        );
        let r = |i: usize| match &cmds[i] {
            Command::FileReport(r) => r.clone(),
            other => panic!("{other:?}"),
        };
        let a = r(0);
        assert_eq!(
            (a.kind, a.layout, a.patch, a.display, a.format),
            (
                ReportKind::Text,
                FileLayout::Patch,
                PatchStyle::Unified,
                LineDisplay::Context(1),
                OutputFormat::Text
            )
        );
        assert_eq!(a.comparison, None);
        let b = r(1);
        assert_eq!(b.patch, PatchStyle::Context);
        assert_eq!(
            b.comparison,
            Some(Comparison::Pair("a.txt".into(), "b.txt".into()))
        );
        let c = r(2);
        assert!(c.strikeout_left && c.wrap && c.output == OutputTarget::Printer);
        assert_eq!(c.layout, FileLayout::Interleaved);
        let d = r(3);
        assert_eq!(
            (d.kind, d.format, d.comparison),
            (
                ReportKind::File,
                OutputFormat::HtmlColor,
                Some(Comparison::Session("My Session".into()))
            )
        );
        assert_eq!(r(4).format, OutputFormat::Csv);
        assert_eq!(r(5).format, OutputFormat::Xml);
        assert_eq!(r(6).kind, ReportKind::Hex);
        assert_eq!(r(7).kind, ReportKind::Data);
        assert_eq!(r(8).ignore_unimportant, Some(true));
        assert!(parse_err("hex-report layout:patch output-to:x").contains("no `patch` layout"));
        assert!(parse_err("text-report output-to:x").contains("layout"));
        assert!(parse_err("text-report layout:summary output-to:x a b c").contains("extra"));
    }

    #[test]
    fn sync_copy_move_delete_and_options() {
        let cmds = parse(
            "sync create-empty mirror:left->right\nsync update:all\nsync visible update:rt->lt\n\
             copy right->left\nmove lt->rt\ndelete left\ndelete recyclebin=no all\n\
             option confirm:yes-to-all\n",
        );
        assert_eq!(
            cmds[0],
            Command::Sync {
                mode: SyncMode::MirrorLeftToRight,
                create_empty: true,
                visible: false
            }
        );
        assert_eq!(
            cmds[1],
            Command::Sync {
                mode: SyncMode::UpdateBoth,
                create_empty: false,
                visible: false
            }
        );
        assert_eq!(
            cmds[2],
            Command::Sync {
                mode: SyncMode::UpdateRightToLeft,
                create_empty: false,
                visible: true
            }
        );
        assert_eq!(
            cmds[3],
            Command::Copy {
                dir: Direction::RightToLeft,
                remove_source: false
            }
        );
        assert_eq!(
            cmds[4],
            Command::Copy {
                dir: Direction::LeftToRight,
                remove_source: true
            }
        );
        assert_eq!(
            cmds[5],
            Command::Delete {
                sides: Sides::Left,
                recycle: None
            }
        );
        assert_eq!(
            cmds[6],
            Command::Delete {
                sides: Sides::Both,
                recycle: Some(false)
            }
        );
        assert_eq!(
            cmds[7],
            Command::Option(ScriptOption::Confirm(Confirm::YesToAll))
        );
        assert!(parse_err("move").contains("direction"));
    }

    #[test]
    fn copyto_moveto_rename_touch_attrib_snapshot() {
        let now = Local.with_ymd_and_hms(2020, 6, 1, 12, 0, 0).unwrap();
        let cmds: Vec<Command> = parse_script_at(
            "copyto left path:relative \"C:\\My Archive.zip\"\nmoveto path:base out\ncopyto x\n\
             rename *.bak\nrename regexpr (...)(...)\\.txt $2$1.txt\n\
             touch left->right\ntouch right:now\ntouch all:\"2012-12-31 12:00\"\n\
             attrib +h\nattrib +sh -a\n\
             snapshot save-crc save-version no-filters path:C:\\ output:D:\\\n\
             snapshot left output:\"My Snapshot.bcss\"\nbeep\n",
            &[],
            &no_env,
            &now,
        )
        .unwrap()
        .into_iter()
        .map(|s| s.command)
        .collect();
        assert_eq!(
            cmds[0],
            Command::CopyTo {
                sides: Sides::Left,
                path: PathMode::Relative,
                target: "C:\\My Archive.zip".into(),
                remove_source: false
            }
        );
        assert_eq!(
            cmds[1],
            Command::CopyTo {
                sides: Sides::Both,
                path: PathMode::Base,
                target: "out".into(),
                remove_source: true
            }
        );
        assert_eq!(
            cmds[2],
            Command::CopyTo {
                sides: Sides::Both,
                path: PathMode::None,
                target: "x".into(),
                remove_source: false
            }
        );
        assert_eq!(
            cmds[3],
            Command::Rename {
                regex: None,
                new: "*.bak".into()
            }
        );
        assert_eq!(
            cmds[4],
            Command::Rename {
                regex: Some("(...)(...)\\.txt".into()),
                new: "$2$1.txt".into()
            }
        );
        assert_eq!(cmds[5], Command::Touch(Touch::Copy(Direction::LeftToRight)));
        assert_eq!(
            cmds[6],
            Command::Touch(Touch::Set {
                sides: Sides::Right,
                at: None
            })
        );
        let noon = Local
            .with_ymd_and_hms(2012, 12, 31, 12, 0, 0)
            .unwrap()
            .timestamp_millis();
        assert_eq!(
            cmds[7],
            Command::Touch(Touch::Set {
                sides: Sides::Both,
                at: Some(noon)
            })
        );
        assert_eq!(
            cmds[8],
            Command::Attrib {
                set: attr::HIDDEN,
                clear: 0
            }
        );
        assert_eq!(
            cmds[9],
            Command::Attrib {
                set: attr::SYSTEM | attr::HIDDEN,
                clear: attr::ARCHIVE
            }
        );
        assert_eq!(
            cmds[10],
            Command::Snapshot(Snapshot {
                save_crc: true,
                save_version: true,
                expand_archives: false,
                follow_symlinks: false,
                include_empty: false,
                no_filters: true,
                source: SnapshotSource::Path("C:\\".into()),
                output: Some("D:\\".into())
            })
        );
        let Command::Snapshot(s) = &cmds[11] else {
            panic!()
        };
        assert_eq!(s.source, SnapshotSource::Left);
        assert_eq!(s.output.as_deref(), Some("My Snapshot.bcss"));
        assert_eq!(cmds[12], Command::Beep);
        assert!(parse_err("rename regexpr ( x").contains("regular expression"));
        assert!(parse_err("attrib h").contains("+ or -"));
        assert!(parse_err("touch sideways").contains("touch"));
        assert!(parse_err("snapshot save-crc").contains("left, right or path"));
    }

    #[test]
    fn every_error_is_reported_with_its_line() {
        let errs = parse_script(
            "load\nfrobnicate\nsync mirror:all\nfolder-report layout:summary",
            &[],
            &no_env,
        )
        .unwrap_err();
        let lines: Vec<usize> = errs.iter().map(|e| e.line).collect();
        assert_eq!(lines, [1, 2, 3, 4]);
        assert!(errs[1].to_string().contains("unknown command `frobnicate`"));
        assert!(errs[3].message.contains("output-to"));
    }

    #[test]
    fn log_variants_and_drive_letters_are_paths() {
        assert_eq!(
            parse("log verbose append:C:\\logs\\x.txt")[0],
            Command::Log {
                level: LogLevel::Verbose,
                append: true,
                path: Some("C:\\logs\\x.txt".into())
            }
        );
        assert_eq!(
            parse("log C:\\x.txt")[0],
            Command::Log {
                level: LogLevel::Normal,
                append: false,
                path: Some("C:\\x.txt".into())
            }
        );
        assert_eq!(
            parse("log none")[0],
            Command::Log {
                level: LogLevel::None,
                append: false,
                path: None
            }
        );
        assert_eq!(
            parse("log normal")[0],
            Command::Log {
                level: LogLevel::Normal,
                append: false,
                path: None
            }
        );
    }

    #[test]
    fn beyond_compare_sample_scripts_parse() {
        let params = ["My File.txt", "Your File.txt", "My Report.txt"].map(String::from);
        for (i, script) in crate::tests::SAMPLE_SCRIPTS.iter().enumerate() {
            if let Err(e) = parse_script(script, &params, &no_env) {
                panic!("sample {i}: {e:?}");
            }
        }
    }
}
