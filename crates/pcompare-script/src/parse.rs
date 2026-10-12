//! Parser for PCompare scripts, a subset of Beyond Compare's script language:
//! one command per line, `#` comments, double-quoted arguments, `%1`–`%9`
//! script parameters and `%NAME%` environment variables, and keywords that
//! are case-insensitive.
//!
//! The whole script is parsed before anything runs, so a typo on line 40
//! can't leave a sync half done.

use pcompare_core::folder::ContentMethod;
use pcompare_report::folder::{DisplayFilter, FolderLayout};
use pcompare_report::text::TextLayout;
use pcompare_sync::sync::SyncMode;
use std::fmt;

/// One command and the line it came from (1-based).
#[derive(Debug, Clone, PartialEq)]
pub struct Statement {
    pub line: usize,
    /// The line as written (after variable expansion), for logs.
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

/// Comparison criteria (`criteria timestamp:2sec size crc`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Criteria {
    pub timestamp: bool,
    pub tolerance_secs: u64,
    pub ignore_dst: bool,
    pub size: bool,
    pub content: ContentMethod,
    pub attributes: bool,
}

impl Default for Criteria {
    /// Beyond Compare's default: timestamp (2 s) and size.
    fn default() -> Self {
        Self {
            timestamp: true,
            tolerance_secs: 2,
            ignore_dst: false,
            size: true,
            content: ContentMethod::Off,
            attributes: false,
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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderReport {
    pub layout: FolderLayout,
    pub filter: DisplayFilter,
    pub file_links: bool,
    pub show_size: bool,
    pub show_modified: bool,
    pub format: OutputFormat,
    pub title: String,
    pub output: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextReport {
    pub layout: TextLayout,
    /// `layout:patch` — a unified diff.
    pub patch: bool,
    pub differences_only: bool,
    pub context: usize,
    pub line_numbers: bool,
    pub ignore_unimportant: bool,
    pub format: OutputFormat,
    pub wrap: bool,
    pub title: String,
    pub output: String,
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

#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    Log {
        verbose: bool,
        append: bool,
        path: String,
    },
    Criteria(Criteria),
    Filter(Filter),
    Load {
        left: String,
        right: String,
    },
    /// `None` is "all".
    Expand(Option<String>),
    Collapse(Option<String>),
    Select(Vec<SelectTerm>),
    /// Content-compares the selected files; `None` uses the criteria's
    /// content method, or rules-based when it has none.
    Compare(Option<ContentMethod>),
    FolderReport(FolderReport),
    TextReport(TextReport),
    Sync {
        mode: SyncMode,
        create_empty: bool,
    },
    Copy(Direction),
    Delete(Sides),
    Option(ScriptOption),
}

/// Replaces `%1`..`%9` with script parameters (missing ones become empty)
/// and `%NAME%` with environment variables (unknown names stay as written).
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

/// Parses a whole script; every error is reported, not just the first.
pub fn parse_script(
    text: &str,
    params: &[String],
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Vec<Statement>, Vec<ParseError>> {
    let mut stmts = Vec::new();
    let mut errors = Vec::new();
    for (i, raw) in text.lines().enumerate() {
        let line = i + 1;
        let trimmed = raw.trim().trim_start_matches('\u{feff}');
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let expanded = expand_vars(trimmed, params, env);
        let parsed = tokenize(&expanded).and_then(|tokens| parse_command(&tokens));
        match parsed {
            Ok(command) => stmts.push(Statement {
                line,
                text: expanded,
                command,
            }),
            Err(message) => errors.push(ParseError { line, message }),
        }
    }
    if errors.is_empty() {
        Ok(stmts)
    } else {
        Err(errors)
    }
}

/// `key:value` split, case-insensitive key.
fn key_value(token: &str) -> Option<(String, &str)> {
    let (k, v) = token.split_once(':')?;
    // A Windows drive (`C:\x`) is a path, not a key.
    if k.len() == 1 && k.chars().all(|c| c.is_ascii_alphabetic()) {
        return None;
    }
    Some((k.to_ascii_lowercase(), v))
}

fn parse_command(tokens: &[String]) -> Result<Command, String> {
    let (head, args) = tokens.split_first().ok_or("empty command")?;
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match head.to_ascii_lowercase().as_str() {
        "log" => parse_log(&args),
        "criteria" => parse_criteria(&args).map(Command::Criteria),
        "filter" => parse_filter(&args).map(Command::Filter),
        "load" => match args.as_slice() {
            [l, r] => Ok(Command::Load {
                left: (*l).to_owned(),
                right: (*r).to_owned(),
            }),
            _ => Err("load needs a left and a right path".into()),
        },
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
        "text-report" => parse_text_report(&args).map(Command::TextReport),
        "sync" => parse_sync(&args),
        "copy" => match args.as_slice() {
            [] => Ok(Command::Copy(Direction::LeftToRight)),
            [d] => parse_direction(d).map(Command::Copy),
            _ => Err("copy takes one direction: left->right or right->left".into()),
        },
        "delete" => match args.as_slice() {
            [] => Ok(Command::Delete(Sides::Both)),
            [s] => match s.to_ascii_lowercase().as_str() {
                "left" => Ok(Command::Delete(Sides::Left)),
                "right" => Ok(Command::Delete(Sides::Right)),
                "all" => Ok(Command::Delete(Sides::Both)),
                other => Err(format!("delete takes left, right or all, not `{other}`")),
            },
            _ => Err("delete takes one side: left, right or all".into()),
        },
        "option" => match args.as_slice() {
            [o] => parse_option(o).map(Command::Option),
            _ => Err("option takes one setting, e.g. confirm:yes-to-all".into()),
        },
        other => Err(format!("unknown command `{other}`")),
    }
}

fn parse_log(args: &[&str]) -> Result<Command, String> {
    let mut verbose = false;
    let mut rest = args;
    if let Some((first, tail)) = rest.split_first() {
        match first.to_ascii_lowercase().as_str() {
            "verbose" => {
                verbose = true;
                rest = tail;
            }
            "normal" => rest = tail,
            _ => {}
        }
    }
    let [file] = rest else {
        return Err("log needs a file: log [normal|verbose] [append:]<file>".into());
    };
    match key_value(file) {
        Some((k, v)) if k == "append" => Ok(Command::Log {
            verbose,
            append: true,
            path: v.to_owned(),
        }),
        _ => Ok(Command::Log {
            verbose,
            append: false,
            path: (*file).to_owned(),
        }),
    }
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

fn parse_criteria(args: &[&str]) -> Result<Criteria, String> {
    if args.is_empty() {
        return Err(
            "criteria needs at least one of timestamp, size, crc, binary, rules-based".into(),
        );
    }
    let mut c = Criteria {
        timestamp: false,
        size: false,
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
            "attrib" | "attributes" => c.attributes = true,
            _ => match content_method(word) {
                Some(m) if opts.is_empty() => c.content = m,
                _ => return Err(format!("unknown criterion `{a}`")),
            },
        }
    }
    Ok(c)
}

/// Turns a BC folder mask (`obj\`, `\src\gen\`) into a glob.
fn folder_glob(mask: &str) -> String {
    mask.replace('\\', "/").trim_matches('/').to_owned()
}

fn parse_filter(args: &[&str]) -> Result<Filter, String> {
    if args.is_empty() {
        return Err("filter needs masks, e.g. filter \"*.rs;-target\\\"".into());
    }
    let mut f = Filter::default();
    let mut include_folders = Vec::new();
    for mask in args
        .iter()
        .flat_map(|a| a.split(';'))
        .map(str::trim)
        .filter(|m| !m.is_empty())
    {
        let (exclude, mask) = match mask.strip_prefix('-') {
            Some(m) => (true, m.trim()),
            None => (false, mask),
        };
        let folder = mask.ends_with('\\') || mask.ends_with('/');
        match (exclude, folder) {
            (true, true) => f.exclude_folders.push(folder_glob(mask)),
            (true, false) => f.exclude_files.push(mask.to_owned()),
            (false, true) => include_folders.push(folder_glob(mask)),
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
    Ok(f)
}

fn parse_target(cmd: &str, args: &[&str]) -> Result<Option<String>, String> {
    match args {
        [] => Err(format!("{cmd} needs `all` or a folder")),
        [a] if a.eq_ignore_ascii_case("all") => Ok(None),
        [a] => Ok(Some(a.replace('\\', "/").trim_matches('/').to_owned())),
        _ => Err(format!("{cmd} takes one folder (or all)")),
    }
}

/// `[left|right|all].[all|diff|same|exact|orphan|newer|older].[files|folders]`,
/// any part optional; `all` alone selects everything.
fn parse_select(term: &str) -> Result<SelectTerm, String> {
    let lower = term.to_ascii_lowercase();
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

fn output_format(v: &str) -> Result<OutputFormat, String> {
    match v.to_ascii_lowercase().as_str() {
        "html-color" | "html" => Ok(OutputFormat::HtmlColor),
        "html-mono" => Ok(OutputFormat::HtmlMono),
        "text" | "plain" | "text-plain" => Ok(OutputFormat::Text),
        "csv" => Ok(OutputFormat::Csv),
        other => Err(format!("unknown output option `{other}`")),
    }
}

/// The format implied by an output file's extension.
fn format_for(path: &str) -> OutputFormat {
    let lower = path.to_ascii_lowercase();
    if lower.ends_with(".html") || lower.ends_with(".htm") {
        OutputFormat::HtmlColor
    } else if lower.ends_with(".csv") {
        OutputFormat::Csv
    } else {
        OutputFormat::Text
    }
}

fn output_path(v: &str) -> Result<String, String> {
    match v.to_ascii_lowercase().as_str() {
        "" => Err("output-to needs a file".into()),
        "printer" | "clipboard" => Err(format!(
            "output-to:{v} needs the app window; write to a file and print or open that instead"
        )),
        _ => Ok(v.to_owned()),
    }
}

fn parse_folder_report(args: &[&str]) -> Result<FolderReport, String> {
    let mut r = FolderReport {
        layout: FolderLayout::Summary,
        filter: DisplayFilter::All,
        file_links: false,
        show_size: true,
        show_modified: true,
        format: OutputFormat::HtmlColor,
        title: String::new(),
        output: String::new(),
    };
    let mut format = None;
    let mut columns: Option<(bool, bool)> = None;
    for a in args {
        let (k, v) = key_value(a).ok_or_else(|| format!("expected key:value, got `{a}`"))?;
        match k.as_str() {
            "layout" => {
                r.layout = match v.to_ascii_lowercase().as_str() {
                    "summary" => FolderLayout::Summary,
                    "side-by-side" | "detailed" => FolderLayout::Detailed,
                    other => {
                        return Err(format!(
                            "unknown folder-report layout `{other}` (summary, side-by-side)"
                        ))
                    }
                }
            }
            "options" => {
                for o in v.split(',').map(str::trim).filter(|o| !o.is_empty()) {
                    let o = o.to_ascii_lowercase();
                    r.filter = match o.as_str() {
                        "display-all" => DisplayFilter::All,
                        "display-mismatches" => DisplayFilter::Diffs,
                        "display-mismatches-no-orphans" => DisplayFilter::DiffsNoOrphans,
                        "display-matches" => DisplayFilter::Same,
                        "display-orphans" => DisplayFilter::Orphans,
                        "display-left-orphans" => DisplayFilter::LeftOrphans,
                        "display-right-orphans" => DisplayFilter::RightOrphans,
                        "display-left-newer" => DisplayFilter::LeftNewer,
                        "display-right-newer" => DisplayFilter::RightNewer,
                        "include-file-links" => {
                            r.file_links = true;
                            continue;
                        }
                        "column-size" | "column-timestamp" | "column-none" => {
                            let (size, time) = columns.get_or_insert((false, false));
                            *size |= o == "column-size";
                            *time |= o == "column-timestamp";
                            continue;
                        }
                        _ => return Err(format!("unknown folder-report option `{o}`")),
                    };
                }
            }
            "title" => r.title = v.to_owned(),
            "output-to" => r.output = output_path(v)?,
            "output-options" => {
                for o in v.split(',').map(str::trim).filter(|o| !o.is_empty()) {
                    format = Some(output_format(o)?);
                }
            }
            other => return Err(format!("unknown folder-report argument `{other}`")),
        }
    }
    if r.output.is_empty() {
        return Err("folder-report needs output-to:<file>".into());
    }
    if let Some((size, time)) = columns {
        (r.show_size, r.show_modified) = (size, time);
    }
    r.format = format.unwrap_or_else(|| format_for(&r.output));
    if r.file_links && !matches!(r.format, OutputFormat::HtmlColor | OutputFormat::HtmlMono) {
        return Err("include-file-links needs an HTML report".into());
    }
    Ok(r)
}

fn parse_text_report(args: &[&str]) -> Result<TextReport, String> {
    let mut r = TextReport {
        layout: TextLayout::SideBySide,
        patch: false,
        differences_only: false,
        context: 3,
        line_numbers: false,
        ignore_unimportant: false,
        format: OutputFormat::HtmlColor,
        wrap: true,
        title: String::new(),
        output: String::new(),
    };
    let mut format = None;
    for a in args {
        let (k, v) = key_value(a).ok_or_else(|| format!("expected key:value, got `{a}`"))?;
        match k.as_str() {
            "layout" => match v.to_ascii_lowercase().as_str() {
                "side-by-side" => r.layout = TextLayout::SideBySide,
                "over-under" | "interleaved" | "unified" => r.layout = TextLayout::Unified,
                "patch" => r.patch = true,
                other => {
                    return Err(format!(
                        "unknown text-report layout `{other}` (side-by-side, interleaved, patch)"
                    ))
                }
            },
            "options" => {
                for o in v.split(',').map(str::trim).filter(|o| !o.is_empty()) {
                    let o = o.to_ascii_lowercase();
                    match o.as_str() {
                        "display-all" => r.differences_only = false,
                        "display-mismatches" => (r.differences_only, r.context) = (true, 0),
                        "display-context" => (r.differences_only, r.context) = (true, 3),
                        "line-numbers" => r.line_numbers = true,
                        "ignore-unimportant" => r.ignore_unimportant = true,
                        _ => match o
                            .strip_prefix("display-context=")
                            .and_then(|n| n.parse().ok())
                        {
                            Some(n) => (r.differences_only, r.context) = (true, n),
                            None => return Err(format!("unknown text-report option `{o}`")),
                        },
                    }
                }
            }
            "title" => r.title = v.to_owned(),
            "output-to" => r.output = output_path(v)?,
            "output-options" => {
                for o in v.split(',').map(str::trim).filter(|o| !o.is_empty()) {
                    match o.to_ascii_lowercase().as_str() {
                        "wrap-word" | "wrap-character" => r.wrap = true,
                        "wrap-none" => r.wrap = false,
                        _ => format = Some(output_format(o)?),
                    }
                }
            }
            other => return Err(format!("unknown text-report argument `{other}`")),
        }
    }
    if r.output.is_empty() {
        return Err("text-report needs output-to:<file>".into());
    }
    r.format = format.unwrap_or_else(|| format_for(&r.output));
    if r.format == OutputFormat::Csv {
        return Err("text reports are HTML or text, not CSV".into());
    }
    Ok(r)
}

fn parse_direction(s: &str) -> Result<Direction, String> {
    match s.to_ascii_lowercase().as_str() {
        "left->right" | "lt->rt" => Ok(Direction::LeftToRight),
        "right->left" | "rt->lt" => Ok(Direction::RightToLeft),
        other => Err(format!(
            "expected left->right or right->left, got `{other}`"
        )),
    }
}

fn parse_sync(args: &[&str]) -> Result<Command, String> {
    let mut create_empty = false;
    let mut mode = None;
    for a in args {
        if a.eq_ignore_ascii_case("create-empty") {
            create_empty = true;
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
    Ok(Command::Sync { mode, create_empty })
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
            .unwrap()
            .into_iter()
            .map(|s| s.command)
            .collect()
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
    fn comments_blank_lines_and_case_are_ignored() {
        let cmds = parse("# a comment\n\n  LOAD a b\nExpand ALL\n");
        assert_eq!(
            cmds,
            [
                Command::Load {
                    left: "a".into(),
                    right: "b".into()
                },
                Command::Expand(None)
            ]
        );
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
                attributes: false
            })
        );
        assert_eq!(
            cmds[1],
            Command::Filter(Filter {
                include_files: vec!["*.rs".into(), "*.toml".into()],
                exclude_files: vec!["*.bak".into()],
                exclude_folders: vec!["target".into(), ".git".into()],
            })
        );
        let Command::Filter(f) = &parse("filter \"src\\;*.rs\"")[0] else {
            panic!()
        };
        assert_eq!(f.include_files, ["src/**/*.rs", "**/src/**/*.rs"]);
    }

    #[test]
    fn selection_terms() {
        let Command::Select(terms) =
            &parse("select all.diff.files left.orphan right.newer.folders all")[0]
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
            ]
        );
        assert!(parse_script("select left.banana", &[], &no_env).is_err());
    }

    #[test]
    fn reports_sync_copy_and_options() {
        let cmds = parse(
            "folder-report layout:side-by-side options:display-mismatches,include-file-links,column-size \
             title:\"My report\" output-to:\"out dir/r.html\"\n\
             text-report layout:patch options:display-context=1 output-to:a.diff\n\
             sync create-empty mirror:left->right\nsync update:all\ncopy right->left\ndelete left\n\
             option confirm:yes-to-all\n",
        );
        assert_eq!(
            cmds[0],
            Command::FolderReport(FolderReport {
                layout: FolderLayout::Detailed,
                filter: DisplayFilter::Diffs,
                file_links: true,
                show_size: true,
                show_modified: false,
                format: OutputFormat::HtmlColor,
                title: "My report".into(),
                output: "out dir/r.html".into(),
            })
        );
        let Command::TextReport(t) = &cmds[1] else {
            panic!()
        };
        assert!(t.patch && t.differences_only && t.context == 1 && t.format == OutputFormat::Text);
        assert_eq!(
            cmds[2],
            Command::Sync {
                mode: SyncMode::MirrorLeftToRight,
                create_empty: true
            }
        );
        assert_eq!(
            cmds[3],
            Command::Sync {
                mode: SyncMode::UpdateBoth,
                create_empty: false
            }
        );
        assert_eq!(cmds[4], Command::Copy(Direction::RightToLeft));
        assert_eq!(cmds[5], Command::Delete(Sides::Left));
        assert_eq!(
            cmds[6],
            Command::Option(ScriptOption::Confirm(Confirm::YesToAll))
        );
    }

    #[test]
    fn every_error_is_reported_with_its_line() {
        let errs = parse_script(
            "load a\nfrobnicate\nsync mirror:all\nfolder-report layout:summary",
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
                verbose: true,
                append: true,
                path: "C:\\logs\\x.txt".into()
            }
        );
        assert_eq!(
            parse("log C:\\x.txt")[0],
            Command::Log {
                verbose: false,
                append: false,
                path: "C:\\x.txt".into()
            }
        );
    }
}
