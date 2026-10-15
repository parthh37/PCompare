//! Folder comparison reports: a summary (status and relative path of every
//! file) or a detailed side-by-side listing with sizes, dates and status, as
//! HTML, plain text, CSV or XML. Reports honour a display filter and name
//! filter like the Folder Compare view. HTML reports can link a text-compare
//! report for every differing text file, written to `<report>_files/`, and
//! show bookmark comments; XML reports can embed those comparisons instead.

use crate::html::{self, escape, Style};
use crate::text::{self, TextReportFormat, TextReportOptions, TextSection};
use crate::{csv_row, display_width, format_size, format_time, pad, xml};
use pcompare_core::folder::{EntryMeta, EntryStatus, FolderCompareResult, FolderEntry, Side};
use pcompare_core::text::{TextDiffOptions, TextDiffResult};
use pcompare_core::{textfile, CoreError, Result};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fmt::Write;
use std::path::{Path, PathBuf};

// ---------- input ----------

/// [`EntryStatus`], readable back from the UI's JSON.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Status {
    Same,
    Similar,
    Different,
    LeftOnly,
    RightOnly,
    TypeMismatch,
    Error,
}

impl From<EntryStatus> for Status {
    fn from(s: EntryStatus) -> Self {
        match s {
            EntryStatus::Same => Status::Same,
            EntryStatus::Similar => Status::Similar,
            EntryStatus::Different => Status::Different,
            EntryStatus::LeftOnly => Status::LeftOnly,
            EntryStatus::RightOnly => Status::RightOnly,
            EntryStatus::TypeMismatch => Status::TypeMismatch,
            EntryStatus::Error => Status::Error,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Newer {
    Left,
    Right,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ReportMeta {
    pub size: u64,
    pub modified_ms: Option<i64>,
    /// DOS attribute bits (`folder::attr`), where the file system has them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attributes: Option<u32>,
    /// CRC-32, when the comparison computed one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub crc: Option<u32>,
}

impl From<&EntryMeta> for ReportMeta {
    fn from(m: &EntryMeta) -> Self {
        Self {
            size: m.size,
            modified_ms: m.modified_ms,
            attributes: m.attributes,
            crc: None,
        }
    }
}

/// One row of a folder comparison, in the same JSON shape as the engine's
/// `FolderEntry` plus the UI's "ignored" mark, so the view can send exactly
/// what it shows (content re-checks and ignore marks included).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReportEntry {
    pub name: String,
    pub rel_path: String,
    pub is_dir: bool,
    pub status: Status,
    #[serde(default)]
    pub newer: Option<Newer>,
    #[serde(default)]
    pub left: Option<ReportMeta>,
    #[serde(default)]
    pub right: Option<ReportMeta>,
    #[serde(default)]
    pub children: Vec<ReportEntry>,
    #[serde(default)]
    pub error: Option<String>,
    /// The left side's own relative path when alignment paired differently
    /// named entries; `name` / `rel_path` describe the right side.
    #[serde(default)]
    pub left_rel_path: Option<String>,
    /// Marked "ignore" in the view: reported (and counted) as the same.
    #[serde(default)]
    pub ignored: bool,
}

impl From<&FolderEntry> for ReportEntry {
    fn from(e: &FolderEntry) -> Self {
        Self {
            name: e.name.clone(),
            rel_path: e.rel_path.clone(),
            is_dir: e.is_dir,
            status: e.status.into(),
            newer: e.newer.map(|s| match s {
                Side::Left => Newer::Left,
                Side::Right => Newer::Right,
            }),
            left: e.left.as_ref().map(|m| ReportMeta {
                crc: e.crc.map(|c| c[0]),
                ..ReportMeta::from(m)
            }),
            right: e.right.as_ref().map(|m| ReportMeta {
                crc: e.crc.map(|c| c[1]),
                ..ReportMeta::from(m)
            }),
            children: e.children.iter().map(ReportEntry::from).collect(),
            error: e.error.clone(),
            left_rel_path: e.left_rel_path.clone(),
            ignored: false,
        }
    }
}

impl ReportEntry {
    /// The status filters and totals go by: ignored entries count as same.
    pub fn effective(&self) -> Status {
        if self.ignored {
            Status::Same
        } else {
            self.status
        }
    }

    /// The name on the left side (differs from `name` for aligned pairs).
    pub fn left_name(&self) -> &str {
        match &self.left_rel_path {
            Some(p) => p.rsplit('/').next().unwrap_or(p),
            None => &self.name,
        }
    }

    pub fn left_rel(&self) -> &str {
        self.left_rel_path.as_deref().unwrap_or(&self.rel_path)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FolderReportInput {
    pub left_root: String,
    pub right_root: String,
    pub entries: Vec<ReportEntry>,
}

impl From<&FolderCompareResult> for FolderReportInput {
    fn from(r: &FolderCompareResult) -> Self {
        Self {
            left_root: r.left_root.clone(),
            right_root: r.right_root.clone(),
            entries: r.entries.iter().map(ReportEntry::from).collect(),
        }
    }
}

// ---------- options ----------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FolderLayout {
    /// Status and relative path of each file.
    Summary,
    /// The folder tree side by side with sizes and dates.
    #[default]
    #[serde(alias = "sideBySide")]
    Detailed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FolderReportFormat {
    #[default]
    Html,
    Text,
    Csv,
    /// Every listed row as XML (schema in [`crate::xml`]).
    Xml,
}

/// A bookmarked entry (by relative path) with an optional comment, shown
/// as an annotation in HTML reports and as `<comment>` in XML ones.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FolderBookmark {
    pub path: String,
    #[serde(default)]
    pub comment: Option<String>,
}

impl FolderBookmark {
    fn comment(&self) -> Option<&str> {
        self.comment
            .as_deref()
            .map(str::trim)
            .filter(|c| !c.is_empty())
    }
}

/// Bookmarks by relative path (a duplicate keeps whichever has a comment).
fn bookmark_map(bookmarks: &[FolderBookmark]) -> HashMap<&str, &FolderBookmark> {
    let mut map: HashMap<&str, &FolderBookmark> = HashMap::new();
    for b in bookmarks {
        let path = b.path.trim_end_matches('/');
        let e = map.entry(path).or_insert(b);
        if e.comment().is_none() {
            *e = b;
        }
    }
    map
}

/// The Folder Compare display filters (same names as the UI's).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DisplayFilter {
    #[default]
    All,
    Diffs,
    DiffsNoOrphans,
    Same,
    Orphans,
    LeftOrphans,
    RightOrphans,
    LeftNewer,
    RightNewer,
}

impl DisplayFilter {
    pub fn label(self) -> &'static str {
        match self {
            DisplayFilter::All => "All",
            DisplayFilter::Diffs => "Differences",
            DisplayFilter::DiffsNoOrphans => "Differences (no orphans)",
            DisplayFilter::Same => "Same",
            DisplayFilter::Orphans => "Orphans",
            DisplayFilter::LeftOrphans => "Left orphans",
            DisplayFilter::RightOrphans => "Right orphans",
            DisplayFilter::LeftNewer => "Left newer",
            DisplayFilter::RightNewer => "Right newer",
        }
    }

    /// Mirrors `matchesFilter` in `src/lib/folderTree.ts`: similar and
    /// ignored entries count as same.
    pub fn matches(self, e: &ReportEntry) -> bool {
        let status = e.effective();
        let newer = if e.ignored { None } else { e.newer };
        let orphan = matches!(status, Status::LeftOnly | Status::RightOnly);
        let same = matches!(status, Status::Same | Status::Similar);
        match self {
            DisplayFilter::All => true,
            DisplayFilter::Diffs => !same,
            DisplayFilter::DiffsNoOrphans => !same && !orphan,
            DisplayFilter::Same => same,
            DisplayFilter::Orphans => orphan,
            DisplayFilter::LeftOrphans => status == Status::LeftOnly,
            DisplayFilter::RightOrphans => status == Status::RightOnly,
            DisplayFilter::LeftNewer => newer == Some(Newer::Left),
            DisplayFilter::RightNewer => newer == Some(Newer::Right),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct FolderReportOptions {
    pub layout: FolderLayout,
    pub format: FolderReportFormat,
    pub filter: DisplayFilter,
    /// The view's quick name filter: `*`/`?` wildcards, otherwise a
    /// case-insensitive substring. Empty for none.
    pub name_filter: String,
    /// Detailed layout columns.
    pub show_size: bool,
    pub show_modified: bool,
    /// CRC-32 column (filled where the comparison computed CRCs).
    pub show_crc: bool,
    /// DOS attributes column (`RHSA`).
    pub show_attributes: bool,
    pub mono: bool,
    /// Report heading; empty for "Folder Compare".
    pub title: String,
    /// Folders whose contents are listed; `None` lists everything. Scripts
    /// use this for "expand" semantics.
    #[serde(skip)]
    pub expanded: Option<HashSet<String>>,
}

impl Default for FolderReportOptions {
    fn default() -> Self {
        Self {
            layout: FolderLayout::Detailed,
            format: FolderReportFormat::Html,
            filter: DisplayFilter::All,
            name_filter: String::new(),
            show_size: true,
            show_modified: true,
            show_crc: false,
            show_attributes: false,
            mono: false,
            title: String::new(),
            expanded: None,
        }
    }
}

// ---------- visibility ----------

/// `*`/`?` wildcard match, case-insensitive (`name` and `pattern` lowercased).
fn wildcard(pattern: &[char], name: &[char]) -> bool {
    let (mut p, mut n) = (0, 0);
    let (mut star, mut mark) = (None, 0);
    while n < name.len() {
        if p < pattern.len() && (pattern[p] == '?' || pattern[p] == name[n]) {
            p += 1;
            n += 1;
        } else if p < pattern.len() && pattern[p] == '*' {
            star = Some(p);
            mark = n;
            p += 1;
        } else if let Some(s) = star {
            p = s + 1;
            mark += 1;
            n = mark;
        } else {
            return false;
        }
    }
    pattern[p..].iter().all(|&c| c == '*')
}

/// A name predicate from the quick filter.
type NameMatcher = Box<dyn Fn(&str) -> bool>;

/// Mirrors `nameMatcher` in `src/lib/folderTree.ts`.
fn name_matcher(query: &str) -> Option<NameMatcher> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return None;
    }
    if !q.contains(['*', '?']) {
        return Some(Box::new(move |name: &str| name.to_lowercase().contains(&q)));
    }
    let pattern: Vec<char> = q.chars().collect();
    Some(Box::new(move |name: &str| {
        let name: Vec<char> = name.to_lowercase().chars().collect();
        wildcard(&pattern, &name)
    }))
}

/// Relative paths shown under the filters (mirrors `visibleSet`): a file
/// passes both filters; a folder shows if anything below it does (or, with
/// no name filter, if it matches itself).
fn visible_set(entries: &[ReportEntry], opts: &FolderReportOptions) -> HashSet<String> {
    let name = name_matcher(&opts.name_filter);
    let mut out = HashSet::new();
    fn visit(
        e: &ReportEntry,
        f: DisplayFilter,
        name: &Option<NameMatcher>,
        out: &mut HashSet<String>,
    ) -> bool {
        let mut child_visible = false;
        for c in &e.children {
            child_visible |= visit(c, f, name, out);
        }
        let own = f.matches(e) && name.as_ref().is_none_or(|n| !e.is_dir && n(&e.name));
        let show = (e.is_dir && child_visible) || (own && (!e.is_dir || name.is_none()));
        if show {
            out.insert(e.rel_path.clone());
        }
        show
    }
    for e in entries {
        visit(e, opts.filter, &name, &mut out);
    }
    out
}

/// A report row: an entry and its depth in the tree.
struct Row<'a> {
    entry: &'a ReportEntry,
    depth: usize,
}

fn expanded(opts: &FolderReportOptions, e: &ReportEntry) -> bool {
    opts.expanded
        .as_ref()
        .is_none_or(|set| set.contains(&e.rel_path))
}

/// The rows the report lists, in tree order. The summary is flat: files,
/// plus folders that are collapsed or empty.
fn rows<'a>(input: &'a FolderReportInput, opts: &FolderReportOptions) -> Vec<Row<'a>> {
    let visible = visible_set(&input.entries, opts);
    let mut out = Vec::new();
    fn walk<'a>(
        list: &'a [ReportEntry],
        depth: usize,
        visible: &HashSet<String>,
        opts: &FolderReportOptions,
        out: &mut Vec<Row<'a>>,
    ) {
        for e in list {
            if !visible.contains(&e.rel_path) {
                continue;
            }
            let open = e.is_dir && expanded(opts, e) && !e.children.is_empty();
            if opts.layout == FolderLayout::Detailed || !open {
                out.push(Row { entry: e, depth });
            }
            if open {
                walk(&e.children, depth + 1, visible, opts, out);
            }
        }
    }
    walk(&input.entries, 0, &visible, opts, &mut out);
    out
}

// ---------- labels ----------

#[derive(Default)]
struct Tally {
    same: usize,
    similar: usize,
    different: usize,
    left_only: usize,
    right_only: usize,
    errors: usize,
    ignored: usize,
}

/// Counts files like `tally` in pcompare-core (ignored ones as same).
fn tally(entries: &[ReportEntry], t: &mut Tally) {
    for e in entries {
        if !e.is_dir {
            t.ignored += e.ignored as usize;
            match e.effective() {
                Status::Same => t.same += 1,
                Status::Similar => t.similar += 1,
                Status::Different | Status::TypeMismatch => t.different += 1,
                Status::LeftOnly => t.left_only += 1,
                Status::RightOnly => t.right_only += 1,
                Status::Error => t.errors += 1,
            }
        } else if e.status == Status::Error {
            t.errors += 1;
        }
        tally(&e.children, t);
    }
}

fn tally_text(t: &Tally) -> String {
    let mut s = format!(
        "Same {}, similar {}, different {}, left orphans {}, right orphans {}, errors {}",
        t.same, t.similar, t.different, t.left_only, t.right_only, t.errors
    );
    if t.ignored > 0 {
        let _ = write!(s, " ({} ignored)", t.ignored);
    }
    s
}

/// Human-readable status of an entry.
pub fn status_label(e: &ReportEntry) -> String {
    if e.ignored {
        return "Ignored".into();
    }
    match e.status {
        Status::Same => "Same".into(),
        Status::Similar => "Similar".into(),
        Status::Different => match (e.is_dir, e.newer) {
            (false, Some(Newer::Left)) => "Left newer".into(),
            (false, Some(Newer::Right)) => "Right newer".into(),
            _ => "Different".into(),
        },
        Status::LeftOnly => "Left orphan".into(),
        Status::RightOnly => "Right orphan".into(),
        Status::TypeMismatch => "Type mismatch".into(),
        Status::Error => match &e.error {
            Some(msg) => format!("Error: {msg}"),
            None => "Error".into(),
        },
    }
}

fn status_class(e: &ReportEntry) -> &'static str {
    if e.ignored {
        return "s-ignored";
    }
    match e.status {
        Status::Same => "s-same",
        Status::Similar => "s-similar",
        Status::Different => "s-different",
        Status::LeftOnly => "s-leftOnly",
        Status::RightOnly => "s-rightOnly",
        Status::TypeMismatch => "s-typeMismatch",
        Status::Error => "s-error",
    }
}

/// One-character status for fixed-width text.
fn status_mark(e: &ReportEntry) -> char {
    if e.ignored {
        return 'i';
    }
    match e.status {
        Status::Same => '=',
        Status::Similar => '~',
        Status::Different => '!',
        Status::LeftOnly => '<',
        Status::RightOnly => '>',
        Status::TypeMismatch => 'x',
        Status::Error => 'E',
    }
}

fn status_symbol(e: &ReportEntry) -> &'static str {
    if e.ignored {
        return "–";
    }
    match e.status {
        Status::Same => "=",
        Status::Similar => "≈",
        Status::Different => "≠",
        Status::LeftOnly => "◂",
        Status::RightOnly => "▸",
        Status::TypeMismatch | Status::Error => "!",
    }
}

fn display_path(e: &ReportEntry) -> String {
    let rel = match &e.left_rel_path {
        Some(l) if *l != e.rel_path => format!("{l} → {}", e.rel_path),
        _ => e.rel_path.clone(),
    };
    if e.is_dir {
        format!("{rel}/")
    } else {
        rel
    }
}

fn size_of(m: &Option<ReportMeta>, is_dir: bool) -> String {
    match m {
        Some(m) if !is_dir => format_size(m.size),
        _ => String::new(),
    }
}

fn crc_of(m: &Option<ReportMeta>) -> String {
    m.as_ref()
        .and_then(|m| m.crc)
        .map(|c| format!("{c:08X}"))
        .unwrap_or_default()
}

/// `RHSA`-style attribute letters, `-` for a cleared bit.
fn attributes_of(m: &Option<ReportMeta>) -> String {
    let Some(bits) = m.as_ref().and_then(|m| m.attributes) else {
        return String::new();
    };
    use pcompare_core::folder::attr;
    [(attr::READONLY, 'R'), (attr::HIDDEN, 'H'), (attr::SYSTEM, 'S'), (attr::ARCHIVE, 'A')]
        .iter()
        .map(|&(b, c)| if bits & b != 0 { c } else { '-' })
        .collect()
}

/// The optional detailed-layout columns after size and modified:
/// (heading, value) for one side.
fn extra_columns(opts: &FolderReportOptions, m: &Option<ReportMeta>) -> Vec<(&'static str, String)> {
    let mut out = Vec::new();
    if opts.show_crc {
        out.push(("CRC", crc_of(m)));
    }
    if opts.show_attributes {
        out.push(("Attributes", attributes_of(m)));
    }
    out
}

fn date_of(m: &Option<ReportMeta>) -> String {
    m.as_ref()
        .and_then(|m| m.modified_ms)
        .map(format_time)
        .unwrap_or_default()
}

fn title(opts: &FolderReportOptions) -> &str {
    if opts.title.trim().is_empty() {
        "Folder Compare"
    } else {
        opts.title.trim()
    }
}

fn filter_text(opts: &FolderReportOptions) -> String {
    let mut s = format!("Display: {}", opts.filter.label());
    if !opts.name_filter.trim().is_empty() {
        let _ = write!(s, " · names matching “{}”", opts.name_filter.trim());
    }
    s
}

// ---------- rendering ----------

/// Renders the report. `links` maps relative paths to hrefs of per-file
/// reports (HTML only; see [`write_folder_report`]).
pub fn render(
    input: &FolderReportInput,
    opts: &FolderReportOptions,
    generated: &str,
    links: &HashMap<String, String>,
) -> String {
    render_annotated(input, opts, &[], generated, links)
}

/// [`render`] with bookmarks (HTML and XML show their comments).
pub fn render_annotated(
    input: &FolderReportInput,
    opts: &FolderReportOptions,
    bookmarks: &[FolderBookmark],
    generated: &str,
    links: &HashMap<String, String>,
) -> String {
    render_full(input, opts, bookmarks, generated, links, &HashMap::new())
}

/// An embedded per-file comparison of an XML report: the diff with the
/// sides' paths, or why there is none.
type Embedded = std::result::Result<(String, String, TextDiffResult), String>;

fn render_full(
    input: &FolderReportInput,
    opts: &FolderReportOptions,
    bookmarks: &[FolderBookmark],
    generated: &str,
    links: &HashMap<String, String>,
    embedded: &HashMap<String, (Embedded, &TextReportOptions)>,
) -> String {
    let rows = rows(input, opts);
    let mut t = Tally::default();
    tally(&input.entries, &mut t);
    let marks = bookmark_map(bookmarks);
    match opts.format {
        FolderReportFormat::Html => render_html(input, opts, generated, &rows, &t, links, &marks),
        FolderReportFormat::Text => render_text(input, opts, generated, &rows, &t),
        FolderReportFormat::Csv => render_csv(opts, &rows),
        FolderReportFormat::Xml => render_xml(input, opts, generated, &rows, &t, &marks, embedded),
    }
}

fn linked_name(name: &str, href: Option<&String>) -> String {
    match href {
        Some(h) => format!("<a href=\"{}\">{}</a>", escape(h), escape(name)),
        None => escape(name),
    }
}

fn render_html(
    input: &FolderReportInput,
    opts: &FolderReportOptions,
    generated: &str,
    rows: &[Row],
    t: &Tally,
    links: &HashMap<String, String>,
    marks: &HashMap<&str, &FolderBookmark>,
) -> String {
    let title = title(opts);
    let mut body = html::header(title, generated, &input.left_root, &input.right_root);
    // Bookmarked rows: their index (for the comment list's links) and comment row.
    let mut anchors: HashMap<&str, usize> = HashMap::new();
    let mut mark = |e: &ReportEntry, i: usize| -> (String, &'static str) {
        let Some(b) = marks.get(e.rel_path.as_str()) else {
            return (String::new(), "");
        };
        anchors.insert(b.path.trim_end_matches('/'), i);
        (format!(" id=\"bm-{i}\""), " bm")
    };
    let note = |out: &mut String, e: &ReportEntry, cols: usize| {
        if let Some(c) = marks.get(e.rel_path.as_str()).and_then(|b| b.comment()) {
            let _ = writeln!(
                out,
                "<tr class=\"note\"><td colspan=\"{cols}\"><span class=\"tag\">Comment</span>{}</td></tr>",
                escape(c)
            );
        }
    };
    let _ = writeln!(
        body,
        "<div class=\"stats\">{} · {}</div>",
        escape(&filter_text(opts)),
        escape(&tally_text(t))
    );
    if rows.is_empty() {
        body.push_str("<div class=\"note\">Nothing to show for this filter.</div>\n");
    } else if opts.layout == FolderLayout::Summary {
        body.push_str("<table class=\"grid\">\n<tr><th>Status</th><th>Path</th></tr>\n");
        for (i, Row { entry: e, .. }) in rows.iter().enumerate() {
            let (id, bm) = mark(e, i);
            let _ = writeln!(
                body,
                "<tr class=\"{}{bm}\"{id}><td>{}</td><td>{}</td></tr>",
                status_class(e),
                escape(&status_label(e)),
                linked_name(&display_path(e), links.get(&e.rel_path))
            );
            note(&mut body, e, 2);
        }
        body.push_str("</table>\n");
    } else {
        let extra = extra_columns(opts, &None).len();
        let side_cols = 1 + opts.show_size as usize + opts.show_modified as usize + extra;
        let head = |label: &str| {
            let mut h = format!("<th>{label}</th>");
            if opts.show_size {
                h.push_str("<th>Size</th>");
            }
            if opts.show_modified {
                h.push_str("<th>Modified</th>");
            }
            for (name, _) in extra_columns(opts, &None) {
                let _ = write!(h, "<th>{name}</th>");
            }
            h
        };
        let _ = writeln!(
            body,
            "<table class=\"grid\">\n<tr>{}<th></th>{}</tr>",
            head("Left"),
            head("Right")
        );
        for (i, Row { entry: e, depth }) in rows.iter().enumerate() {
            let (id, bm) = mark(e, i);
            let indent = 6 + depth * 16;
            let href = links.get(&e.rel_path);
            let side = |meta: &Option<ReportMeta>, name: &str| -> String {
                if meta.is_none() {
                    return format!("<td class=\"absent\" colspan=\"{side_cols}\"></td>");
                }
                let shown = if e.is_dir {
                    format!("{name}/")
                } else {
                    name.to_owned()
                };
                let mut s = format!(
                    "<td class=\"name\" style=\"padding-left:{indent}px\">{}</td>",
                    linked_name(&shown, href)
                );
                if opts.show_size {
                    let _ = write!(s, "<td class=\"num\">{}</td>", size_of(meta, e.is_dir));
                }
                if opts.show_modified {
                    let _ = write!(s, "<td class=\"date\">{}</td>", escape(&date_of(meta)));
                }
                for (_, value) in extra_columns(opts, meta) {
                    let _ = write!(s, "<td class=\"num\">{}</td>", escape(&value));
                }
                s
            };
            let _ = writeln!(
                body,
                "<tr class=\"{}{}{bm}\"{id}>{}<td class=\"st\" title=\"{}\">{}</td>{}</tr>",
                if e.is_dir { "dir " } else { "" },
                status_class(e),
                side(&e.left, e.left_name()),
                escape(&status_label(e)),
                status_symbol(e),
                side(&e.right, &e.name),
            );
            note(&mut body, e, side_cols * 2 + 1);
        }
        body.push_str("</table>\n");
    }
    if !marks.is_empty() {
        let mut all: Vec<&&FolderBookmark> = marks.values().collect();
        all.sort_by(|a, b| a.path.cmp(&b.path));
        body.push_str("<h3>Comments</h3>\n<ul class=\"comments\">\n");
        for b in all {
            let path = b.path.trim_end_matches('/');
            let label = match anchors.get(path) {
                Some(i) => format!("<a href=\"#bm-{i}\">{}</a>", escape(path)),
                None => escape(path),
            };
            let _ = writeln!(
                body,
                "<li><span class=\"tag\">{label}</span>{}</li>",
                b.comment()
                    .map(escape)
                    .unwrap_or_else(|| "<span class=\"note-empty\">(bookmark)</span>".into())
            );
        }
        body.push_str("</ul>\n");
    }
    html::page(
        title,
        Style {
            mono: opts.mono,
            wrap: true,
        },
        &body,
    )
}

fn render_text(
    input: &FolderReportInput,
    opts: &FolderReportOptions,
    generated: &str,
    rows: &[Row],
    t: &Tally,
) -> String {
    let mut out = format!("{}\n", title(opts));
    if !generated.is_empty() {
        let _ = writeln!(out, "Produced {generated} by PCompare");
    }
    let _ = writeln!(
        out,
        "Left:  {}\nRight: {}",
        input.left_root, input.right_root
    );
    let _ = writeln!(out, "{}\n{}\n", filter_text(opts), tally_text(t));
    if rows.is_empty() {
        out.push_str("Nothing to show for this filter.\n");
        return out;
    }
    let line = |out: &mut String, s: String| {
        out.push_str(s.trim_end());
        out.push('\n');
    };
    if opts.layout == FolderLayout::Summary {
        let w = rows
            .iter()
            .map(|r| display_width(&status_label(r.entry)))
            .max()
            .unwrap_or(0)
            .min(40);
        for Row { entry: e, .. } in rows {
            line(
                &mut out,
                format!("{}  {}", pad(&status_label(e), w), display_path(e)),
            );
        }
        return out;
    }
    let names = |e: &ReportEntry, left: bool| {
        let (meta, name) = if left {
            (&e.left, e.left_name())
        } else {
            (&e.right, e.name.as_str())
        };
        meta.as_ref()
            .map(|_| format!("{name}{}", if e.is_dir { "/" } else { "" }))
    };
    let name_w = rows
        .iter()
        .flat_map(|r| [names(r.entry, true), names(r.entry, false)].map(|n| (r.depth, n)))
        .filter_map(|(d, n)| n.map(|n| d * 2 + display_width(&n)))
        .max()
        .unwrap_or(4)
        .clamp(4, 60);
    let size_w = rows
        .iter()
        .flat_map(|r| {
            [
                size_of(&r.entry.left, r.entry.is_dir),
                size_of(&r.entry.right, r.entry.is_dir),
            ]
        })
        .map(|s| s.len())
        .max()
        .unwrap_or(0)
        .max(4);
    let side = |e: &ReportEntry, depth: usize, left: bool| {
        let meta = if left { &e.left } else { &e.right };
        let name = names(e, left)
            .map(|n| format!("{}{n}", "  ".repeat(depth)))
            .unwrap_or_default();
        let mut s = pad(&name, name_w);
        if opts.show_size {
            let _ = write!(s, "  {:>size_w$}", size_of(meta, e.is_dir));
        }
        if opts.show_modified {
            let _ = write!(s, "  {:<19}", date_of(meta));
        }
        for (name, value) in extra_columns(opts, meta) {
            let _ = write!(s, "  {value:<w$}", w = name.len().max(8));
        }
        s
    };
    let mut head = pad("Name", name_w);
    if opts.show_size {
        let _ = write!(head, "  {:>size_w$}", "Size");
    }
    if opts.show_modified {
        let _ = write!(head, "  {:<19}", "Modified");
    }
    for (name, _) in extra_columns(opts, &None) {
        let _ = write!(head, "  {name:<w$}", w = name.len().max(8));
    }
    line(&mut out, format!("{head}     {head}"));
    for Row { entry: e, depth } in rows {
        line(
            &mut out,
            format!(
                "{}  {}  {}",
                side(e, *depth, true),
                status_mark(e),
                side(e, *depth, false)
            ),
        );
    }
    out
}

fn render_csv(opts: &FolderReportOptions, rows: &[Row]) -> String {
    if opts.layout == FolderLayout::Summary {
        let mut out = csv_row(&["Status", "Path"]);
        for Row { entry: e, .. } in rows {
            out.push_str(&csv_row(&[status_label(e), display_path(e)]));
        }
        return out;
    }
    let mut head = vec!["Path", "Type", "Status"];
    for side in ["Left", "Right"] {
        if opts.show_size {
            head.push(if side == "Left" {
                "Left Size"
            } else {
                "Right Size"
            });
        }
        if opts.show_modified {
            head.push(if side == "Left" {
                "Left Modified"
            } else {
                "Right Modified"
            });
        }
        for (name, _) in extra_columns(opts, &None) {
            head.push(match (side, name) {
                ("Left", "CRC") => "Left CRC",
                ("Left", _) => "Left Attributes",
                (_, "CRC") => "Right CRC",
                _ => "Right Attributes",
            });
        }
    }
    let mut out = csv_row(&head);
    for Row { entry: e, .. } in rows {
        let mut fields = vec![
            display_path(e),
            if e.is_dir { "Folder" } else { "File" }.to_owned(),
            status_label(e),
        ];
        for meta in [&e.left, &e.right] {
            if opts.show_size {
                // Plain digits: spreadsheets read them as numbers.
                fields.push(
                    meta.as_ref()
                        .filter(|_| !e.is_dir)
                        .map(|m| m.size.to_string())
                        .unwrap_or_default(),
                );
            }
            if opts.show_modified {
                fields.push(date_of(meta));
            }
            fields.extend(extra_columns(opts, meta).into_iter().map(|(_, v)| v));
        }
        out.push_str(&csv_row(&fields));
    }
    out
}

fn status_name(s: Status) -> &'static str {
    match s {
        Status::Same => "same",
        Status::Similar => "similar",
        Status::Different => "different",
        Status::LeftOnly => "leftOnly",
        Status::RightOnly => "rightOnly",
        Status::TypeMismatch => "typeMismatch",
        Status::Error => "error",
    }
}

fn render_xml(
    input: &FolderReportInput,
    opts: &FolderReportOptions,
    generated: &str,
    rows: &[Row],
    t: &Tally,
    marks: &HashMap<&str, &FolderBookmark>,
    embedded: &HashMap<String, (Embedded, &TextReportOptions)>,
) -> String {
    let mut out = String::from(xml::DECLARATION);
    let _ = write!(
        out,
        "<folderReport version=\"1\" generator=\"PCompare\" title=\"{}\"",
        xml::attr(title(opts))
    );
    if !generated.is_empty() {
        let _ = write!(out, " produced=\"{}\"", xml::attr(generated));
    }
    let layout = match opts.layout {
        FolderLayout::Summary => "summary",
        FolderLayout::Detailed => "detailed",
    };
    let filter = filter_name(opts.filter);
    let _ = writeln!(
        out,
        ">\n  <left root=\"{}\"/>\n  <right root=\"{}\"/>\n  <options layout=\"{layout}\" filter=\"{filter}\" nameFilter=\"{}\"/>",
        xml::attr(&input.left_root),
        xml::attr(&input.right_root),
        xml::attr(opts.name_filter.trim())
    );
    let _ = writeln!(
        out,
        "  <summary same=\"{}\" similar=\"{}\" different=\"{}\" leftOnly=\"{}\" rightOnly=\"{}\" errors=\"{}\" ignored=\"{}\"/>",
        t.same, t.similar, t.different, t.left_only, t.right_only, t.errors, t.ignored
    );
    let _ = writeln!(out, "  <entries count=\"{}\">", rows.len());
    for Row { entry: e, depth } in rows {
        let _ = write!(
            out,
            "    <entry path=\"{}\" name=\"{}\" type=\"{}\" depth=\"{depth}\" status=\"{}\" label=\"{}\"",
            xml::attr(&e.rel_path),
            xml::attr(&e.name),
            if e.is_dir { "folder" } else { "file" },
            status_name(e.status),
            xml::attr(&status_label(e)),
        );
        if let Some(n) = e.newer {
            let _ = write!(
                out,
                " newer=\"{}\"",
                if n == Newer::Left { "left" } else { "right" }
            );
        }
        if e.ignored {
            out.push_str(" ignored=\"true\"");
        }
        if let Some(l) = e.left_rel_path.as_ref().filter(|l| **l != e.rel_path) {
            let _ = write!(out, " leftPath=\"{}\"", xml::attr(l));
        }
        if let Some(err) = &e.error {
            let _ = write!(out, " error=\"{}\"", xml::attr(err));
        }
        let mut inner = String::new();
        for (tag, meta) in [("left", &e.left), ("right", &e.right)] {
            let Some(m) = meta else { continue };
            let _ = write!(inner, "      <{tag}");
            if !e.is_dir {
                let _ = write!(inner, " size=\"{}\"", m.size);
            }
            if let Some(ms) = m.modified_ms {
                let _ = write!(inner, " modified=\"{}\"", xml::attr(&format_time(ms)));
            }
            if let Some(c) = m.crc {
                let _ = write!(inner, " crc=\"{c:08X}\"");
            }
            if m.attributes.is_some() {
                let _ = write!(inner, " attributes=\"{}\"", attributes_of(meta));
            }
            inner.push_str("/>\n");
        }
        if let Some(c) = marks.get(e.rel_path.as_str()).and_then(|b| b.comment()) {
            let _ = writeln!(inner, "      <comment>{}</comment>", xml::text(c));
        }
        match embedded.get(&e.rel_path) {
            Some((Ok((left, right, diff)), text_opts)) => {
                let section = TextSection {
                    name: "",
                    left,
                    right,
                    diff,
                };
                text::xml_comparison(&mut inner, &section, "", text_opts, &[], "      ");
            }
            Some((Err(reason), _)) => {
                let _ = writeln!(
                    inner,
                    "      <comparison skipped=\"{}\"/>",
                    xml::attr(reason)
                );
            }
            None => {}
        }
        if inner.is_empty() {
            out.push_str("/>\n");
        } else {
            let _ = write!(out, ">\n{inner}    </entry>\n");
        }
    }
    out.push_str("  </entries>\n</folderReport>\n");
    out
}

/// The filter's serialized name, as the app spells it.
fn filter_name(f: DisplayFilter) -> &'static str {
    match f {
        DisplayFilter::All => "all",
        DisplayFilter::Diffs => "diffs",
        DisplayFilter::DiffsNoOrphans => "diffsNoOrphans",
        DisplayFilter::Same => "same",
        DisplayFilter::Orphans => "orphans",
        DisplayFilter::LeftOrphans => "leftOrphans",
        DisplayFilter::RightOrphans => "rightOrphans",
        DisplayFilter::LeftNewer => "leftNewer",
        DisplayFilter::RightNewer => "rightNewer",
    }
}

// ---------- writing with linked file reports ----------

/// Per-file text reports linked from an HTML folder report (or embedded in
/// an XML one).
pub struct LinkedReports<'a> {
    /// Layout and options of each file's report (always written as HTML).
    pub text: &'a TextReportOptions,
    /// Text comparison rules (whitespace, case, ignored patterns…).
    pub rules: &'a TextDiffOptions,
    /// Turns a side's path (root joined with the relative path) into a
    /// readable local file, e.g. downloading a remote one.
    pub local: &'a dyn Fn(&str) -> Result<PathBuf>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WrittenReport {
    pub path: String,
    /// Folder holding the linked file reports, when any were written.
    pub files_dir: Option<String>,
    pub linked: usize,
    /// Differing files that got no report, as "relative path: reason".
    pub skipped: Vec<String>,
}

/// Joins a root (local folder or `scheme://` URL) and a `/`-separated relative path.
pub fn join_root(root: &str, rel: &str) -> String {
    if root.is_empty() {
        rel.to_owned()
    } else if root.contains("://") {
        format!("{}/{rel}", root.trim_end_matches('/'))
    } else {
        Path::new(root).join(rel).to_string_lossy().into_owned()
    }
}

/// Percent-encodes a relative path for an href, keeping `/` separators.
fn href_path(rel: &str) -> String {
    let mut out = String::with_capacity(rel.len());
    for b in rel.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~/".contains(&b) {
            out.push(b as char);
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

/// Diffs one file pair: the sides' paths and the diff, `Ok(None)` for
/// binary files.
fn file_diff(
    input: &FolderReportInput,
    e: &ReportEntry,
    linked: &LinkedReports,
) -> Result<Option<(String, String, TextDiffResult)>> {
    let left = join_root(&input.left_root, e.left_rel());
    let right = join_root(&input.right_root, &e.rel_path);
    let (lp, rp) = ((linked.local)(&left)?, (linked.local)(&right)?);
    if textfile::file_is_binary(&lp)? || textfile::file_is_binary(&rp)? {
        return Ok(None);
    }
    let (l, r) = (textfile::load_text(&lp)?, textfile::load_text(&rp)?);
    let diff = pcompare_core::text::diff_named_text(
        &l.text,
        Some(&left),
        &r.text,
        Some(&right),
        linked.rules,
    )?;
    Ok(Some((left, right, diff)))
}

/// Diffs one file pair and renders its report; `Ok(None)` for binary files.
fn file_report(
    input: &FolderReportInput,
    e: &ReportEntry,
    linked: &LinkedReports,
    generated: &str,
) -> Result<Option<String>> {
    let Some((left, right, diff)) = file_diff(input, e, linked)? else {
        return Ok(None);
    };
    let opts = TextReportOptions {
        format: TextReportFormat::Html,
        title: e.rel_path.clone(),
        ..linked.text.clone()
    };
    let section = TextSection {
        name: &e.rel_path,
        left: &left,
        right: &right,
        diff: &diff,
    };
    Ok(Some(text::render(&[section], &opts, generated)))
}

/// The differing (or similar) file pairs a report lists, which get per-file
/// reports.
fn file_candidates<'a>(
    input: &'a FolderReportInput,
    opts: &FolderReportOptions,
) -> Vec<&'a ReportEntry> {
    rows(input, opts)
        .into_iter()
        .map(|r| r.entry)
        .filter(|e| !e.is_dir && e.left.is_some() && e.right.is_some())
        .filter(|e| matches!(e.effective(), Status::Different | Status::Similar))
        .collect()
}

/// Renders the report to `out`. With `linked`, an HTML report first writes
/// a text-compare report for every differing (or similar) text file it
/// lists into `<report name>_files/` next to it and links them; an XML
/// report embeds each file's comparison in its `<entry>` instead.
pub fn write_folder_report(
    input: &FolderReportInput,
    opts: &FolderReportOptions,
    generated: &str,
    out: &Path,
    linked: Option<&LinkedReports>,
) -> Result<WrittenReport> {
    write_folder_report_annotated(input, opts, &[], generated, out, linked)
}

/// [`write_folder_report`] with bookmarks (see [`render_annotated`]).
pub fn write_folder_report_annotated(
    input: &FolderReportInput,
    opts: &FolderReportOptions,
    bookmarks: &[FolderBookmark],
    generated: &str,
    out: &Path,
    linked: Option<&LinkedReports>,
) -> Result<WrittenReport> {
    let mut written = WrittenReport {
        path: out.to_string_lossy().into_owned(),
        ..Default::default()
    };
    let mut links = HashMap::new();
    let mut embedded = HashMap::new();
    if let Some(linked) = linked.filter(|_| opts.format == FolderReportFormat::Xml) {
        for e in file_candidates(input, opts) {
            let found = match file_diff(input, e, linked) {
                Ok(Some(d)) => {
                    written.linked += 1;
                    Ok(d)
                }
                Ok(None) => Err("binary file".to_owned()),
                Err(err) => Err(err.to_string()),
            };
            if let Err(reason) = &found {
                written.skipped.push(format!("{}: {reason}", e.rel_path));
            }
            embedded.insert(e.rel_path.clone(), (found, linked.text));
        }
    }
    if let Some(linked) = linked.filter(|_| opts.format == FolderReportFormat::Html) {
        let stem = out
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "report".into());
        let dir_name = format!("{stem}_files");
        let dir = out.with_file_name(&dir_name);
        for e in file_candidates(input, opts) {
            match file_report(input, e, linked, generated) {
                Ok(Some(html)) => {
                    let target = dir.join(format!("{}.html", e.rel_path));
                    if let Some(parent) = target.parent() {
                        std::fs::create_dir_all(parent)
                            .map_err(|err| CoreError::io(parent, err))?;
                    }
                    std::fs::write(&target, html).map_err(|err| CoreError::io(&target, err))?;
                    links.insert(
                        e.rel_path.clone(),
                        format!("{}/{}.html", href_path(&dir_name), href_path(&e.rel_path)),
                    );
                    written.linked += 1;
                }
                Ok(None) => written.skipped.push(format!("{}: binary file", e.rel_path)),
                Err(err) => written.skipped.push(format!("{}: {err}", e.rel_path)),
            }
        }
        if written.linked > 0 {
            written.files_dir = Some(dir.to_string_lossy().into_owned());
        }
    }
    let report = render_full(input, opts, bookmarks, generated, &links, &embedded);
    std::fs::write(out, report).map_err(|e| CoreError::io(out, e))?;
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(size: u64) -> Option<ReportMeta> {
        Some(ReportMeta {
            size,
            modified_ms: None,
            ..Default::default()
        })
    }

    fn file(rel: &str, status: Status, left: Option<u64>, right: Option<u64>) -> ReportEntry {
        ReportEntry {
            name: rel.rsplit('/').next().unwrap().to_owned(),
            rel_path: rel.to_owned(),
            is_dir: false,
            status,
            newer: None,
            left: left.and_then(meta),
            right: right.and_then(meta),
            children: Vec::new(),
            error: None,
            left_rel_path: None,
            ignored: false,
        }
    }

    fn sample() -> FolderReportInput {
        let mut src = file("src", Status::Different, Some(0), Some(0));
        src.is_dir = true;
        src.children = vec![
            file("src/a.rs", Status::Same, Some(10), Some(10)),
            ReportEntry {
                newer: Some(Newer::Left),
                ..file("src/b.rs", Status::Different, Some(1200), Some(1100))
            },
            file("src/c.rs", Status::LeftOnly, Some(5), None),
        ];
        FolderReportInput {
            left_root: "/l".into(),
            right_root: "/r".into(),
            entries: vec![src, file("readme.md", Status::RightOnly, None, Some(7))],
        }
    }

    fn opts(f: impl FnOnce(&mut FolderReportOptions)) -> FolderReportOptions {
        let mut o = FolderReportOptions::default();
        f(&mut o);
        o
    }

    #[test]
    fn summary_csv_honours_the_display_filter() {
        let csv = render(
            &sample(),
            &opts(|o| {
                o.layout = FolderLayout::Summary;
                o.format = FolderReportFormat::Csv;
                o.filter = DisplayFilter::Diffs;
            }),
            "",
            &HashMap::new(),
        );
        assert_eq!(
            csv,
            "Status,Path\r\nLeft newer,src/b.rs\r\nLeft orphan,src/c.rs\r\nRight orphan,readme.md\r\n"
        );
    }

    #[test]
    fn detailed_csv_lists_folders_and_sizes() {
        let csv = render(
            &sample(),
            &opts(|o| {
                o.format = FolderReportFormat::Csv;
                o.show_modified = false;
                o.filter = DisplayFilter::LeftOrphans;
            }),
            "",
            &HashMap::new(),
        );
        assert_eq!(
            csv,
            "Path,Type,Status,Left Size,Right Size\r\nsrc/,Folder,Different,,\r\nsrc/c.rs,File,Left orphan,5,\r\n"
        );
    }

    #[test]
    fn detailed_text_is_a_fixed_width_tree() {
        let t = render(
            &sample(),
            &opts(|o| {
                o.format = FolderReportFormat::Text;
                o.show_modified = false;
            }),
            "",
            &HashMap::new(),
        );
        let lines: Vec<&str> = t.lines().collect();
        assert_eq!(lines[0], "Folder Compare");
        assert_eq!(lines[1], "Left:  /l");
        assert_eq!(lines[3], "Display: All");
        assert_eq!(
            lines[4],
            "Same 1, similar 0, different 1, left orphans 1, right orphans 1, errors 0"
        );
        assert_eq!(lines[6], "Name        Size     Name        Size");
        assert_eq!(lines[7], "src/              !  src/");
        assert_eq!(lines[8], "  a.rs        10  =    a.rs        10");
        assert_eq!(lines[9], "  b.rs     1,200  !    b.rs     1,100");
        assert_eq!(lines[10], "  c.rs         5  <");
        assert_eq!(lines[11], "                  >  readme.md      7");
    }

    #[test]
    fn name_filter_and_expansion_shape_the_rows() {
        let input = sample();
        let o = opts(|o| {
            o.layout = FolderLayout::Summary;
            o.format = FolderReportFormat::Text;
            o.name_filter = "*.RS".into();
        });
        let t = render(&input, &o, "", &HashMap::new());
        assert!(
            t.contains("Same         src/a.rs\n") && !t.contains("readme"),
            "{t}"
        );
        // A collapsed folder is listed as itself.
        let collapsed = opts(|o| {
            o.layout = FolderLayout::Summary;
            o.format = FolderReportFormat::Text;
            o.expanded = Some(HashSet::new());
        });
        let t = render(&input, &collapsed, "", &HashMap::new());
        assert!(
            t.ends_with("Different     src/\nRight orphan  readme.md\n"),
            "{t}"
        );
    }

    #[test]
    fn ignored_entries_count_and_filter_as_same() {
        let mut input = sample();
        input.entries[1].ignored = true;
        let csv = render(
            &input,
            &opts(|o| {
                o.layout = FolderLayout::Summary;
                o.format = FolderReportFormat::Csv;
                o.filter = DisplayFilter::Same;
            }),
            "",
            &HashMap::new(),
        );
        assert_eq!(csv, "Status,Path\r\nSame,src/a.rs\r\nIgnored,readme.md\r\n");
    }

    #[test]
    fn html_detailed_marks_status_and_absent_sides() {
        let h = render(
            &sample(),
            &FolderReportOptions::default(),
            "today",
            &HashMap::new(),
        );
        assert!(h.contains("<tr class=\"dir s-different\">"), "{h}");
        assert!(h.contains("<td class=\"st\" title=\"Left orphan\">◂</td><td class=\"absent\" colspan=\"3\"></td></tr>"));
        assert!(h.contains("<td class=\"num\">1,200</td>"));
        assert!(h.contains("Same 1, similar 0, different 1"));
    }

    #[test]
    fn linked_reports_are_written_next_to_the_folder_report() {
        let l = tempfile::tempdir().unwrap();
        let r = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        for (dir, text, tag) in [(&l, "one\ntwo\n", 1u8), (&r, "one\nTWO\n", 2u8)] {
            std::fs::create_dir_all(dir.path().join("sub dir")).unwrap();
            std::fs::write(dir.path().join("sub dir/x.txt"), text).unwrap();
            std::fs::write(dir.path().join("bin.dat"), [0u8, 1, 2, tag]).unwrap();
        }
        let cmp = pcompare_core::folder::compare_folders(
            l.path(),
            r.path(),
            &pcompare_core::folder::FolderCompareOptions {
                content: pcompare_core::folder::ContentMethod::Binary,
                compare_timestamp: false,
                ..Default::default()
            },
        )
        .unwrap();
        let input = FolderReportInput::from(&cmp);
        let report = out.path().join("diff report.html");
        let local = |p: &str| Ok(PathBuf::from(p));
        let text_opts = TextReportOptions::default();
        let rules = TextDiffOptions::default();
        let written = write_folder_report(
            &input,
            &FolderReportOptions::default(),
            "",
            &report,
            Some(&LinkedReports {
                text: &text_opts,
                rules: &rules,
                local: &local,
            }),
        )
        .unwrap();
        assert_eq!(written.linked, 1, "{written:?}");
        assert_eq!(written.skipped.len(), 1, "{written:?}");
        assert!(written.skipped[0].starts_with("bin.dat: binary"));
        let file_report =
            std::fs::read_to_string(out.path().join("diff report_files/sub dir/x.txt.html"))
                .unwrap();
        assert!(
            file_report.contains("<span class=\"m\">TWO</span>"),
            "{file_report}"
        );
        let folder = std::fs::read_to_string(&report).unwrap();
        assert!(
            folder.contains("<a href=\"diff%20report_files/sub%20dir/x.txt.html\">x.txt</a>"),
            "{folder}"
        );
    }

    #[test]
    fn xml_report_is_well_formed_and_lists_every_row() {
        let mut input = sample();
        input.entries[0].children[1].left_rel_path = Some("src/B <old>.rs".into());
        input.entries[1].ignored = true;
        let marks = [FolderBookmark {
            path: "src/c.rs".into(),
            comment: Some("gone? & why".into()),
        }];
        let x = render_annotated(
            &input,
            &opts(|o| {
                o.format = FolderReportFormat::Xml;
                o.name_filter = "\"q\"".into();
            }),
            &marks,
            "today",
            &HashMap::new(),
        );
        let names = crate::xml::tests::parse(&x);
        assert_eq!(names[0], "folderReport");
        assert_eq!(
            names.iter().filter(|n| *n == "entry").count(),
            0,
            "name filter hides all"
        );
        let x = render_annotated(
            &input,
            &opts(|o| o.format = FolderReportFormat::Xml),
            &marks,
            "today",
            &HashMap::new(),
        );
        let names = crate::xml::tests::parse(&x);
        assert_eq!(names.iter().filter(|n| *n == "entry").count(), 5, "{x}");
        assert!(x.contains("<entries count=\"5\">"));
        assert!(x.contains("<summary same=\"2\" similar=\"0\" different=\"1\" leftOnly=\"1\" rightOnly=\"0\" errors=\"0\" ignored=\"1\"/>"), "{x}");
        assert!(x.contains("<entry path=\"src/b.rs\" name=\"b.rs\" type=\"file\" depth=\"1\" status=\"different\" label=\"Left newer\" newer=\"left\" leftPath=\"src/B &lt;old&gt;.rs\">"), "{x}");
        assert!(x.contains("      <left size=\"1200\"/>\n      <right size=\"1100\"/>"));
        assert!(x.contains("<comment>gone? &amp; why</comment>"));
        assert!(
            x.contains("status=\"rightOnly\" label=\"Ignored\" ignored=\"true\">"),
            "{x}"
        );
    }

    #[test]
    fn html_shows_bookmark_comments_on_rows() {
        let marks = [
            FolderBookmark {
                path: "src/b.rs".into(),
                comment: Some("review <this>".into()),
            },
            FolderBookmark {
                path: "nowhere.txt".into(),
                comment: None,
            },
        ];
        let h = render_annotated(
            &sample(),
            &opts(|o| o.show_modified = false),
            &marks,
            "",
            &HashMap::new(),
        );
        assert!(
            h.contains("<tr class=\"s-different bm\" id=\"bm-2\">"),
            "{h}"
        );
        assert!(h.contains("<tr class=\"note\"><td colspan=\"5\"><span class=\"tag\">Comment</span>review &lt;this&gt;</td></tr>"), "{h}");
        assert!(h.contains("<a href=\"#bm-2\">src/b.rs</a>"));
        assert!(h.contains(
            "<span class=\"tag\">nowhere.txt</span><span class=\"note-empty\">(bookmark)</span>"
        ));
        let s = render_annotated(
            &sample(),
            &opts(|o| o.layout = FolderLayout::Summary),
            &marks,
            "",
            &HashMap::new(),
        );
        assert!(s.contains("<td colspan=\"2\"><span class=\"tag\">Comment</span>"));
    }

    #[test]
    fn xml_reports_embed_file_comparisons() {
        let l = tempfile::tempdir().unwrap();
        let r = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        for (dir, text, tag) in [(&l, "one\ntwo\n", 1u8), (&r, "one\nTWO\n", 2u8)] {
            std::fs::write(dir.path().join("x.txt"), text).unwrap();
            std::fs::write(dir.path().join("bin.dat"), [0u8, 1, 2, tag]).unwrap();
        }
        let cmp = pcompare_core::folder::compare_folders(
            l.path(),
            r.path(),
            &pcompare_core::folder::FolderCompareOptions {
                content: pcompare_core::folder::ContentMethod::Binary,
                compare_timestamp: false,
                ..Default::default()
            },
        )
        .unwrap();
        let input = FolderReportInput::from(&cmp);
        let report = out.path().join("report.xml");
        let local = |p: &str| Ok(PathBuf::from(p));
        let text_opts = TextReportOptions::default();
        let rules = TextDiffOptions::default();
        let written = write_folder_report(
            &input,
            &opts(|o| o.format = FolderReportFormat::Xml),
            "",
            &report,
            Some(&LinkedReports {
                text: &text_opts,
                rules: &rules,
                local: &local,
            }),
        )
        .unwrap();
        assert_eq!(
            (written.linked, written.skipped.len()),
            (1, 1),
            "{written:?}"
        );
        assert_eq!(written.files_dir, None);
        let x = std::fs::read_to_string(&report).unwrap();
        let names = crate::xml::tests::parse(&x);
        assert_eq!(
            names.iter().filter(|n| *n == "comparison").count(),
            2,
            "{x}"
        );
        assert!(x.contains("<comparison skipped=\"binary file\"/>"));
        assert!(x.contains("<line side=\"right\" n=\"2\">TWO</line>"), "{x}");
    }

    #[test]
    fn wildcards_match_like_the_view() {
        let m = name_matcher("*.r?").unwrap();
        assert!(m("main.rs") && m("A.RB") && !m("main.rsx"));
        let sub = name_matcher("Read").unwrap();
        assert!(sub("README.md"));
        assert!(name_matcher("  ").is_none());
    }
}
