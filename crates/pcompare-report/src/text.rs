//! Text comparison reports: side-by-side or unified layouts as HTML (with
//! the app's diff colours, intra-line highlights and bookmark comments), an
//! HTML slideshow or plain text; unified patches, classic `diff` output and
//! XML.

use crate::html::{self, escape, Style};
use crate::{display_width, expand_tabs, pad, xml};
use pcompare_core::text::{DiffRow, RowKind, Segment, TextDiffResult};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt::Write;
use std::ops::Range;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TextLayout {
    /// Left and right next to each other, aligned row by row.
    #[default]
    SideBySide,
    /// One column: removed lines (`-`) above added ones (`+`), like a patch.
    Unified,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TextReportFormat {
    #[default]
    Html,
    /// Fixed-width plain text.
    Text,
    /// A unified diff (`--- / +++ / @@`) that `patch` and `git apply` read;
    /// ignores the layout and always shows `context` lines around changes.
    Patch,
    /// Classic (normal) `diff` output: `3c3`, `5a6,7`, `8d7` commands with
    /// `< ` / `> ` lines and no context; `patch` reads it too.
    #[serde(alias = "normal")]
    NormalDiff,
    /// Summary counts and every difference as XML (schema in [`crate::xml`]).
    Xml,
    /// A self-contained HTML page showing one difference at a time, with its
    /// context lines, Prev/Next buttons and the ←/→ keys.
    Slideshow,
}

/// Side of a bookmarked line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum BookmarkSide {
    Left,
    Right,
}

/// A bookmarked line (1-based) with an optional comment, shown as an
/// annotation in HTML reports (and listed in XML ones).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TextBookmark {
    pub side: BookmarkSide,
    pub line: usize,
    #[serde(default)]
    pub comment: Option<String>,
}

impl TextBookmark {
    fn comment(&self) -> Option<&str> {
        self.comment
            .as_deref()
            .map(str::trim)
            .filter(|c| !c.is_empty())
    }

    fn label(&self) -> String {
        format!("{} line {}", side_name(self.side), self.line)
    }
}

fn side_name(side: BookmarkSide) -> &'static str {
    match side {
        BookmarkSide::Left => "Left",
        BookmarkSide::Right => "Right",
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TextReportOptions {
    pub layout: TextLayout,
    pub format: TextReportFormat,
    /// Show only differences, each with `context` lines around it.
    pub differences_only: bool,
    pub context: usize,
    pub line_numbers: bool,
    /// Unimportant differences (per the comparison's rules) are not
    /// differences: they don't count, and differences-only views and patches
    /// treat them as context.
    pub ignore_unimportant: bool,
    /// HTML in black and white (bold/underlined changes, marker column).
    pub mono: bool,
    /// HTML wraps long lines instead of letting them run off the page.
    pub wrap: bool,
    /// Report heading; empty for "Text Compare".
    pub title: String,
}

impl Default for TextReportOptions {
    fn default() -> Self {
        Self {
            layout: TextLayout::SideBySide,
            format: TextReportFormat::Html,
            differences_only: false,
            context: 3,
            line_numbers: true,
            ignore_unimportant: true,
            mono: false,
            wrap: true,
            title: String::new(),
        }
    }
}

/// One compared file pair of a report.
#[derive(Debug, Clone, Copy)]
pub struct TextSection<'a> {
    /// Heading when a report holds several pairs (e.g. a relative path).
    pub name: &'a str,
    /// The sides as the reader should see them (paths or tab titles).
    pub left: &'a str,
    pub right: &'a str,
    pub diff: &'a TextDiffResult,
}

/// Renders `sections` in the format and layout `opts` asks for. `generated`
/// is the "produced" stamp (empty to leave it out).
pub fn render(sections: &[TextSection], opts: &TextReportOptions, generated: &str) -> String {
    render_annotated(sections, opts, &[], generated)
}

/// [`render`] with bookmarks: HTML and slideshow reports mark the lines and
/// show their comments (HTML also lists them, and its differences-only view
/// keeps bookmarked lines visible); XML lists them. They apply to every
/// section (reports from the app have one).
pub fn render_annotated(
    sections: &[TextSection],
    opts: &TextReportOptions,
    bookmarks: &[TextBookmark],
    generated: &str,
) -> String {
    match opts.format {
        TextReportFormat::Html => render_html(sections, opts, bookmarks, generated),
        TextReportFormat::Text => render_text(sections, opts, generated),
        TextReportFormat::Patch => sections.iter().map(|s| patch(s, opts)).collect(),
        TextReportFormat::NormalDiff => sections
            .iter()
            .map(|s| normal_diff(s, opts, sections.len() > 1))
            .collect(),
        TextReportFormat::Xml => render_xml(sections, opts, bookmarks, generated),
        TextReportFormat::Slideshow => crate::slides::render(sections, opts, bookmarks, generated),
    }
}

pub(crate) fn title(opts: &TextReportOptions) -> &str {
    if opts.title.trim().is_empty() {
        "Text Compare"
    } else {
        opts.title.trim()
    }
}

// ---------- shared helpers ----------

pub(crate) fn joined(side: &Option<Vec<Segment>>) -> Option<String> {
    side.as_ref()
        .map(|segs| segs.iter().map(|s| s.text.as_str()).collect())
}

/// Whether a row is a difference worth reporting.
pub(crate) fn is_difference(row: &DiffRow, ignore_unimportant: bool) -> bool {
    match row.kind {
        RowKind::Equal => false,
        RowKind::Unimportant => !ignore_unimportant,
        RowKind::Changed | RowKind::LeftOnly | RowKind::RightOnly => true,
    }
}

/// The runs of rows to show: all of them, or each difference (per `diff`)
/// with `context` rows on either side, overlapping runs merged.
fn shown_ranges(
    rows: &[DiffRow],
    all: bool,
    context: usize,
    diff: impl Fn(&DiffRow) -> bool,
) -> Vec<Range<usize>> {
    if all {
        // One run holding every row (not a vector of its indices).
        #[allow(clippy::single_range_in_vec_init)]
        return if rows.is_empty() {
            Vec::new()
        } else {
            vec![0..rows.len()]
        };
    }
    let mut out: Vec<Range<usize>> = Vec::new();
    for (i, row) in rows.iter().enumerate() {
        if !diff(row) {
            continue;
        }
        let (start, end) = (i.saturating_sub(context), (i + context + 1).min(rows.len()));
        match out.last_mut() {
            Some(last) if start <= last.end => last.end = last.end.max(end),
            _ => out.push(start..end),
        }
    }
    out
}

/// Each run of consecutive differences (per [`is_difference`]); there are as
/// many as the summary's difference sections.
pub(crate) fn difference_blocks(rows: &[DiffRow], ignore_unimportant: bool) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < rows.len() {
        if !is_difference(&rows[i], ignore_unimportant) {
            i += 1;
            continue;
        }
        let start = i;
        while i < rows.len() && is_difference(&rows[i], ignore_unimportant) {
            i += 1;
        }
        out.push(start..i);
    }
    out
}

/// What a block of differences does, as XML and slideshows name it.
pub(crate) fn block_kind(block: &[DiffRow]) -> &'static str {
    if block.iter().all(|r| r.kind == RowKind::LeftOnly) {
        "leftOnly"
    } else if block.iter().all(|r| r.kind == RowKind::RightOnly) {
        "rightOnly"
    } else if block.iter().all(|r| r.kind == RowKind::Unimportant) {
        "unimportant"
    } else {
        "changed"
    }
}

/// First line number and count of one side within `block`; a side with no
/// lines there gives the line before it (0 at the top), as patches do.
pub(crate) fn block_span(
    rows: &[DiffRow],
    block: &Range<usize>,
    pick: fn(&DiffRow) -> Option<usize>,
) -> (usize, usize) {
    let lines = &rows[block.clone()];
    let count = lines.iter().filter(|r| pick(r).is_some()).count();
    let start = lines
        .iter()
        .find_map(pick)
        .unwrap_or_else(|| rows[..block.start].iter().rev().find_map(pick).unwrap_or(0));
    (start, count)
}

/// Counts for the summary line, honouring "ignore unimportant".
pub(crate) struct Summary {
    sections: usize,
    changed: usize,
    left_only: usize,
    right_only: usize,
    unimportant: usize,
}

pub(crate) fn summarize(diff: &TextDiffResult, ignore_unimportant: bool) -> Summary {
    let mut s = Summary {
        sections: 0,
        changed: 0,
        left_only: 0,
        right_only: 0,
        unimportant: 0,
    };
    let mut in_block = false;
    for row in &diff.rows {
        match row.kind {
            RowKind::Changed => s.changed += 1,
            RowKind::LeftOnly => s.left_only += 1,
            RowKind::RightOnly => s.right_only += 1,
            RowKind::Unimportant => s.unimportant += 1,
            RowKind::Equal => {}
        }
        let d = is_difference(row, ignore_unimportant);
        if d && !in_block {
            s.sections += 1;
        }
        in_block = d;
    }
    s
}

pub(crate) fn summary_text(s: &Summary, ignore_unimportant: bool) -> String {
    if s.sections == 0 {
        return if s.unimportant > 0 && ignore_unimportant {
            format!(
                "No important differences ({} unimportant lines)",
                s.unimportant
            )
        } else {
            "The files are identical".to_owned()
        };
    }
    let mut out = format!(
        "{} difference section{}: {} changed, {} left only, {} right only",
        s.sections,
        if s.sections == 1 { "" } else { "s" },
        s.changed,
        s.left_only,
        s.right_only
    );
    if s.unimportant > 0 {
        let _ = write!(out, ", {} unimportant", s.unimportant);
    }
    out
}

fn hidden_label(n: usize) -> String {
    format!("{n} unchanged line{}", if n == 1 { "" } else { "s" })
}

pub(crate) fn no(n: Option<usize>) -> String {
    n.map(|n| n.to_string()).unwrap_or_default()
}

// ---------- HTML ----------

/// A side's text with changed segments wrapped in `<span class="m">`.
fn segments_html(side: &Option<Vec<Segment>>, marks: bool) -> String {
    let Some(segs) = side else {
        return String::new();
    };
    let mut out = String::new();
    for s in segs {
        if marks && s.changed && !s.text.is_empty() {
            let _ = write!(out, "<span class=\"m\">{}</span>", escape(&s.text));
        } else {
            out.push_str(&escape(&s.text));
        }
    }
    out
}

fn row_class(kind: RowKind) -> &'static str {
    match kind {
        RowKind::Equal => "eq",
        RowKind::Unimportant => "uni",
        RowKind::Changed => "chg",
        RowKind::LeftOnly => "lo",
        RowKind::RightOnly => "ro",
    }
}

fn row_marker(kind: RowKind) -> &'static str {
    match kind {
        RowKind::Equal => "",
        RowKind::Unimportant => "~",
        RowKind::Changed => "|",
        RowKind::LeftOnly => "&lt;",
        RowKind::RightOnly => "&gt;",
    }
}

/// Bookmarks by line, for marking line numbers and adding comment rows.
pub(crate) struct Notes<'a> {
    marks: HashMap<(BookmarkSide, usize), &'a TextBookmark>,
    /// Prefix of the `id`s given to bookmarked line numbers (for links from
    /// the comment list); `None` for no ids.
    ids: Option<String>,
}

impl<'a> Notes<'a> {
    pub(crate) fn new(bookmarks: &'a [TextBookmark], ids: Option<String>) -> Self {
        let mut marks: HashMap<_, &TextBookmark> = HashMap::new();
        for b in bookmarks {
            // A duplicate keeps whichever has a comment.
            let e = marks.entry((b.side, b.line)).or_insert(b);
            if e.comment().is_none() {
                *e = b;
            }
        }
        Self { marks, ids }
    }

    fn get(&self, side: BookmarkSide, n: Option<usize>) -> Option<&'a TextBookmark> {
        n.and_then(|n| self.marks.get(&(side, n)).copied())
    }

    fn marked(&self, row: &DiffRow) -> bool {
        self.get(BookmarkSide::Left, row.left_no).is_some()
            || self.get(BookmarkSide::Right, row.right_no).is_some()
    }

    fn anchor(&self, side: BookmarkSide, n: usize) -> Option<String> {
        let tag = if side == BookmarkSide::Left { 'l' } else { 'r' };
        self.ids.as_ref().map(|p| format!("{p}bm-{tag}{n}"))
    }

    /// A line-number cell, marked (and anchored) when bookmarked.
    fn ln(&self, side: BookmarkSide, n: Option<usize>) -> String {
        match (n, self.get(side, n)) {
            (Some(n), Some(b)) => {
                let id = self
                    .anchor(side, n)
                    .map(|a| format!(" id=\"{a}\""))
                    .unwrap_or_default();
                let tip = b.comment().map(escape).unwrap_or_else(|| "Bookmark".into());
                format!("<td class=\"ln bm\"{id} title=\"{tip}\">{n}</td>")
            }
            _ => format!("<td class=\"ln\">{}</td>", no(n)),
        }
    }

    /// Comment rows for the bookmarks on these lines (left first).
    fn rows(&self, out: &mut String, left: Option<usize>, right: Option<usize>, cols: usize) {
        for (side, n) in [(BookmarkSide::Left, left), (BookmarkSide::Right, right)] {
            if let Some(b) = self.get(side, n) {
                if let Some(c) = b.comment() {
                    let _ = writeln!(
                        out,
                        "<tr class=\"note\"><td colspan=\"{cols}\"><span class=\"tag\">{}</span>{}</td></tr>",
                        escape(&b.label()),
                        escape(c)
                    );
                }
            }
        }
    }

    /// The "Comments" list: every bookmark, linked to its line.
    fn list(&self, out: &mut String) {
        if self.marks.is_empty() {
            return;
        }
        let mut all: Vec<&TextBookmark> = self.marks.values().copied().collect();
        all.sort_by_key(|b| (b.side, b.line));
        out.push_str("<h3>Comments</h3>\n<ul class=\"comments\">\n");
        for b in all {
            let label = match self.anchor(b.side, b.line) {
                Some(a) => format!("<a href=\"#{a}\">{}</a>", escape(&b.label())),
                None => escape(&b.label()),
            };
            let _ = writeln!(
                out,
                "<li><span class=\"tag\">{label}</span>{}</li>",
                b.comment()
                    .map(escape)
                    .unwrap_or_else(|| "<span class=\"note-empty\">(bookmark)</span>".into())
            );
        }
        out.push_str("</ul>\n");
    }
}

fn render_html(
    sections: &[TextSection],
    opts: &TextReportOptions,
    bookmarks: &[TextBookmark],
    generated: &str,
) -> String {
    let title = title(opts);
    let mut body = format!("<h1>{}</h1>\n", escape(title));
    if !generated.is_empty() {
        let _ = writeln!(
            body,
            "<div class=\"meta\">Produced {} by PCompare</div>",
            escape(generated)
        );
    }
    for (n, s) in sections.iter().enumerate() {
        if sections.len() > 1 {
            let _ = writeln!(
                body,
                "<h2>{}</h2>",
                escape(if s.name.is_empty() { s.left } else { s.name })
            );
        }
        let _ = writeln!(
            body,
            "<table class=\"paths\"><tr><th>Left</th><td>{}</td></tr><tr><th>Right</th><td>{}</td></tr></table>",
            escape(s.left),
            escape(s.right)
        );
        let summary = summarize(s.diff, opts.ignore_unimportant);
        let _ = writeln!(
            body,
            "<div class=\"stats\">{}</div>",
            escape(&summary_text(&summary, opts.ignore_unimportant))
        );
        let ids = if sections.len() > 1 {
            format!("s{}-", n + 1)
        } else {
            String::new()
        };
        let notes = Notes::new(bookmarks, Some(ids));
        // Bookmarked lines stay visible in a differences-only report.
        let ranges = shown_ranges(&s.diff.rows, !opts.differences_only, opts.context, |r| {
            is_difference(r, opts.ignore_unimportant) || notes.marked(r)
        });
        if !ranges.is_empty() {
            match opts.layout {
                TextLayout::SideBySide => {
                    side_by_side_html(&mut body, s.diff, &ranges, opts, &notes, true)
                }
                TextLayout::Unified => unified_html(&mut body, s.diff, &ranges, opts, &notes, true),
            }
        }
        notes.list(&mut body);
    }
    html::page(
        title,
        Style {
            mono: opts.mono,
            wrap: opts.wrap,
        },
        &body,
    )
}

/// Emits a "N unchanged lines" separator row when `n > 0`.
fn gap_row(out: &mut String, n: usize, cols: usize) {
    if n > 0 {
        let _ = writeln!(
            out,
            "<tr class=\"gap\"><td colspan=\"{cols}\">⋯ {} ⋯</td></tr>",
            hidden_label(n)
        );
    }
}

/// Emits each range, with separators for the rows between them (and, with
/// `edges`, before the first and after the last).
fn for_each_range(
    out: &mut String,
    rows: usize,
    ranges: &[Range<usize>],
    cols: usize,
    edges: bool,
    mut emit: impl FnMut(&mut String, Range<usize>),
) {
    let mut next = if edges {
        0
    } else {
        ranges.first().map_or(0, |r| r.start)
    };
    for r in ranges {
        gap_row(out, r.start - next, cols);
        next = r.end;
        emit(out, r.clone());
    }
    if edges {
        gap_row(out, rows - next, cols);
    }
}

/// Side-by-side table of `ranges`; `edges` adds the "unchanged lines"
/// separators before the first and after the last range.
pub(crate) fn side_by_side_html(
    out: &mut String,
    diff: &TextDiffResult,
    ranges: &[Range<usize>],
    opts: &TextReportOptions,
    notes: &Notes,
    edges: bool,
) {
    let cols = if opts.line_numbers { 5 } else { 3 };
    out.push_str("<table class=\"diff sbs\">\n");
    for_each_range(out, diff.rows.len(), ranges, cols, edges, |out, range| {
        for row in &diff.rows[range] {
            let marks = matches!(row.kind, RowKind::Changed | RowKind::Unimportant);
            let bm = if notes.marked(row) { " bm" } else { "" };
            let _ = write!(out, "<tr class=\"{}{bm}\">", row_class(row.kind));
            if opts.line_numbers {
                out.push_str(&notes.ln(BookmarkSide::Left, row.left_no));
            }
            let _ = write!(
                out,
                "<td class=\"t l\">{}</td><td class=\"mid\">{}</td>",
                segments_html(&row.left, marks),
                row_marker(row.kind)
            );
            if opts.line_numbers {
                out.push_str(&notes.ln(BookmarkSide::Right, row.right_no));
            }
            let _ = writeln!(
                out,
                "<td class=\"t r\">{}</td></tr>",
                segments_html(&row.right, marks)
            );
            notes.rows(out, row.left_no, row.right_no, cols);
        }
    });
    out.push_str("</table>\n");
}

/// Unified table of `ranges` (see [`side_by_side_html`]).
pub(crate) fn unified_html(
    out: &mut String,
    diff: &TextDiffResult,
    ranges: &[Range<usize>],
    opts: &TextReportOptions,
    notes: &Notes,
    edges: bool,
) {
    let cols = if opts.line_numbers { 4 } else { 2 };
    let line = |out: &mut String,
                class: &str,
                l: Option<usize>,
                r: Option<usize>,
                sign: &str,
                text: String| {
        let marked = notes.get(BookmarkSide::Left, l).is_some()
            || notes.get(BookmarkSide::Right, r).is_some();
        let bm = if marked { " bm" } else { "" };
        let _ = write!(out, "<tr class=\"{class}{bm}\">");
        if opts.line_numbers {
            out.push_str(&notes.ln(BookmarkSide::Left, l));
            out.push_str(&notes.ln(BookmarkSide::Right, r));
        }
        let _ = writeln!(
            out,
            "<td class=\"sign\">{sign}</td><td class=\"t\">{text}</td></tr>"
        );
        notes.rows(out, l, r, cols);
    };
    out.push_str("<table class=\"diff uni\">\n");
    for_each_range(out, diff.rows.len(), ranges, cols, edges, |out, range| {
        let rows = &diff.rows[range];
        let mut i = 0;
        while i < rows.len() {
            if !is_difference(&rows[i], opts.ignore_unimportant) {
                let row = &rows[i];
                let text = if row.left.is_some() {
                    &row.left
                } else {
                    &row.right
                };
                let class = if row.kind == RowKind::Unimportant {
                    "uni"
                } else {
                    "eq"
                };
                line(
                    out,
                    class,
                    row.left_no,
                    row.right_no,
                    "&nbsp;",
                    segments_html(text, false),
                );
                i += 1;
                continue;
            }
            let start = i;
            while i < rows.len() && is_difference(&rows[i], opts.ignore_unimportant) {
                i += 1;
            }
            let block = &rows[start..i];
            for row in block.iter().filter(|r| r.left.is_some()) {
                let marks = row.kind != RowKind::LeftOnly;
                line(
                    out,
                    "del",
                    row.left_no,
                    None,
                    "-",
                    segments_html(&row.left, marks),
                );
            }
            for row in block.iter().filter(|r| r.right.is_some()) {
                let marks = row.kind != RowKind::RightOnly;
                line(
                    out,
                    "add",
                    None,
                    row.right_no,
                    "+",
                    segments_html(&row.right, marks),
                );
            }
        }
    });
    out.push_str("</table>\n");
}

// ---------- plain text ----------

fn render_text(sections: &[TextSection], opts: &TextReportOptions, generated: &str) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "{}", title(opts));
    if !generated.is_empty() {
        let _ = writeln!(out, "Produced {generated} by PCompare");
    }
    for (n, s) in sections.iter().enumerate() {
        out.push('\n');
        if sections.len() > 1 {
            let heading = if s.name.is_empty() { s.left } else { s.name };
            if n > 0 {
                let _ = writeln!(out, "{}", "=".repeat(78));
            }
            let _ = writeln!(out, "{heading}");
        }
        let _ = writeln!(out, "Left:  {}", s.left);
        let _ = writeln!(out, "Right: {}", s.right);
        let summary = summarize(s.diff, opts.ignore_unimportant);
        let _ = writeln!(out, "{}", summary_text(&summary, opts.ignore_unimportant));
        let ranges = shown_ranges(&s.diff.rows, !opts.differences_only, opts.context, |r| {
            is_difference(r, opts.ignore_unimportant)
        });
        if ranges.is_empty() {
            continue;
        }
        out.push('\n');
        match opts.layout {
            TextLayout::SideBySide => side_by_side_text(&mut out, s.diff, &ranges, opts),
            TextLayout::Unified => unified_text(&mut out, s.diff, &ranges, opts),
        }
    }
    out
}

fn push_line(out: &mut String, line: &str) {
    out.push_str(line.trim_end());
    out.push('\n');
}

fn number_width(diff: &TextDiffResult) -> usize {
    let max = diff
        .rows
        .iter()
        .flat_map(|r| [r.left_no, r.right_no])
        .flatten()
        .max()
        .unwrap_or(0);
    max.to_string().len()
}

fn text_ranges(
    out: &mut String,
    rows: usize,
    ranges: &[Range<usize>],
    mut emit: impl FnMut(&mut String, Range<usize>),
) {
    let mut next = 0;
    let gap = |out: &mut String, n: usize| {
        if n > 0 {
            push_line(out, &format!("    ... {} ...", hidden_label(n)));
        }
    };
    for r in ranges {
        gap(out, r.start - next);
        next = r.end;
        emit(out, r.clone());
    }
    gap(out, rows - next);
}

fn text_marker(kind: RowKind) -> char {
    match kind {
        RowKind::Equal => ' ',
        RowKind::Unimportant => '~',
        RowKind::Changed => '|',
        RowKind::LeftOnly => '<',
        RowKind::RightOnly => '>',
    }
}

fn side_by_side_text(
    out: &mut String,
    diff: &TextDiffResult,
    ranges: &[Range<usize>],
    opts: &TextReportOptions,
) {
    let shown = || ranges.iter().flat_map(|r| &diff.rows[r.clone()]);
    let width = shown()
        .filter_map(|r| joined(&r.left))
        .map(|t| display_width(&t))
        .max()
        .unwrap_or(0)
        .clamp(8, 80);
    let nw = number_width(diff);
    text_ranges(out, diff.rows.len(), ranges, |out, range| {
        for row in &diff.rows[range] {
            let left = expand_tabs(&joined(&row.left).unwrap_or_default());
            let right = expand_tabs(&joined(&row.right).unwrap_or_default());
            let mut line = String::new();
            if opts.line_numbers {
                let _ = write!(line, "{:>nw$}  ", no(row.left_no));
            }
            let _ = write!(line, "{} {} ", pad(&left, width), text_marker(row.kind));
            if opts.line_numbers {
                let _ = write!(line, "{:>nw$}  ", no(row.right_no));
            }
            line.push_str(&right);
            push_line(out, &line);
        }
    });
}

fn unified_text(
    out: &mut String,
    diff: &TextDiffResult,
    ranges: &[Range<usize>],
    opts: &TextReportOptions,
) {
    let nw = number_width(diff);
    let line =
        |out: &mut String, l: Option<usize>, r: Option<usize>, sign: char, text: Option<String>| {
            let mut s = String::new();
            if opts.line_numbers {
                let _ = write!(s, "{:>nw$} {:>nw$} ", no(l), no(r));
            }
            let _ = write!(s, "{sign} {}", expand_tabs(&text.unwrap_or_default()));
            push_line(out, &s);
        };
    text_ranges(out, diff.rows.len(), ranges, |out, range| {
        let rows = &diff.rows[range];
        let mut i = 0;
        while i < rows.len() {
            let row = &rows[i];
            if !is_difference(row, opts.ignore_unimportant) {
                line(
                    out,
                    row.left_no,
                    row.right_no,
                    ' ',
                    joined(&row.left).or_else(|| joined(&row.right)),
                );
                i += 1;
                continue;
            }
            let start = i;
            while i < rows.len() && is_difference(&rows[i], opts.ignore_unimportant) {
                i += 1;
            }
            for r in rows[start..i].iter().filter(|r| r.left.is_some()) {
                line(out, r.left_no, None, '-', joined(&r.left));
            }
            for r in rows[start..i].iter().filter(|r| r.right.is_some()) {
                line(out, None, r.right_no, '+', joined(&r.right));
            }
        }
    });
}

// ---------- unified patch ----------

/// Whether a row is a change in a patch. A one-sided row always is: as
/// context it would throw the hunk's line counts off.
fn patch_change(row: &DiffRow, ignore_unimportant: bool) -> bool {
    is_difference(row, ignore_unimportant) || row.left.is_none() || row.right.is_none()
}

fn hunk_range(start: usize, count: usize) -> String {
    if count == 1 {
        start.to_string()
    } else {
        format!("{start},{count}")
    }
}

/// One file pair as a unified diff; empty when there is nothing to change.
fn patch(s: &TextSection, opts: &TextReportOptions) -> String {
    let rows = &s.diff.rows;
    let ignore = opts.ignore_unimportant;
    let ranges = shown_ranges(rows, false, opts.context, |r| patch_change(r, ignore));
    if ranges.is_empty() {
        return String::new();
    }
    let mut out = format!("--- {}\n+++ {}\n", s.left, s.right);
    for range in ranges {
        let block = &rows[range.clone()];
        let left_count = block.iter().filter(|r| r.left.is_some()).count();
        let right_count = block.iter().filter(|r| r.right.is_some()).count();
        // A side with no lines in the hunk is addressed by the line before it.
        let before = |pick: fn(&DiffRow) -> Option<usize>| {
            rows[..range.start].iter().rev().find_map(pick).unwrap_or(0)
        };
        let left_start = block
            .iter()
            .find_map(|r| r.left_no)
            .unwrap_or_else(|| before(|r| r.left_no));
        let right_start = block
            .iter()
            .find_map(|r| r.right_no)
            .unwrap_or_else(|| before(|r| r.right_no));
        let _ = writeln!(
            out,
            "@@ -{} +{} @@",
            hunk_range(left_start, left_count),
            hunk_range(right_start, right_count)
        );
        let mut i = 0;
        while i < block.len() {
            if !patch_change(&block[i], ignore) {
                let _ = writeln!(out, " {}", joined(&block[i].left).unwrap_or_default());
                i += 1;
                continue;
            }
            let start = i;
            while i < block.len() && patch_change(&block[i], ignore) {
                i += 1;
            }
            for r in &block[start..i] {
                if let Some(t) = joined(&r.left) {
                    let _ = writeln!(out, "-{t}");
                }
            }
            for r in &block[start..i] {
                if let Some(t) = joined(&r.right) {
                    let _ = writeln!(out, "+{t}");
                }
            }
        }
    }
    out
}

// ---------- normal diff ----------

/// `n` or `first,last` for a normal-diff command.
fn diff_range(lines: &[(usize, String)]) -> String {
    match (lines.first(), lines.last()) {
        (Some(a), Some(b)) if a.0 != b.0 => format!("{},{}", a.0, b.0),
        (Some(a), _) => a.0.to_string(),
        _ => String::new(),
    }
}

/// One file pair as classic `diff` output; empty when there is nothing to
/// change. `header` adds a `diff left right` line (several pairs).
fn normal_diff(s: &TextSection, opts: &TextReportOptions, header: bool) -> String {
    let rows = &s.diff.rows;
    let ignore = opts.ignore_unimportant;
    let mut out = String::new();
    // Last line of each side before the current block.
    let (mut last_l, mut last_r) = (0, 0);
    let mut i = 0;
    while i < rows.len() {
        if !patch_change(&rows[i], ignore) {
            last_l = rows[i].left_no.unwrap_or(last_l);
            last_r = rows[i].right_no.unwrap_or(last_r);
            i += 1;
            continue;
        }
        let start = i;
        while i < rows.len() && patch_change(&rows[i], ignore) {
            i += 1;
        }
        let block = &rows[start..i];
        let left: Vec<(usize, String)> = block
            .iter()
            .filter_map(|r| Some((r.left_no?, joined(&r.left)?)))
            .collect();
        let right: Vec<(usize, String)> = block
            .iter()
            .filter_map(|r| Some((r.right_no?, joined(&r.right)?)))
            .collect();
        let command = match (left.is_empty(), right.is_empty()) {
            (false, true) => format!("{}d{last_r}", diff_range(&left)),
            (true, false) => format!("{last_l}a{}", diff_range(&right)),
            _ => format!("{}c{}", diff_range(&left), diff_range(&right)),
        };
        let _ = writeln!(out, "{command}");
        for (_, t) in &left {
            let _ = writeln!(out, "< {t}");
        }
        if !left.is_empty() && !right.is_empty() {
            out.push_str("---\n");
        }
        for (_, t) in &right {
            let _ = writeln!(out, "> {t}");
        }
        last_l = left.last().map_or(last_l, |l| l.0);
        last_r = right.last().map_or(last_r, |r| r.0);
    }
    if header && !out.is_empty() {
        out.insert_str(0, &format!("diff {} {}\n", s.left, s.right));
    }
    out
}

// ---------- XML ----------

fn render_xml(
    sections: &[TextSection],
    opts: &TextReportOptions,
    bookmarks: &[TextBookmark],
    generated: &str,
) -> String {
    let mut out = String::from(xml::DECLARATION);
    let _ = write!(
        out,
        "<textReport version=\"1\" generator=\"PCompare\" title=\"{}\"",
        xml::attr(title(opts))
    );
    if !generated.is_empty() {
        let _ = write!(out, " produced=\"{}\"", xml::attr(generated));
    }
    out.push_str(">\n");
    let layout = match opts.layout {
        TextLayout::SideBySide => "sideBySide",
        TextLayout::Unified => "unified",
    };
    let _ = writeln!(
        out,
        "  <options layout=\"{layout}\" ignoreUnimportant=\"{}\" context=\"{}\" differencesOnly=\"{}\"/>",
        opts.ignore_unimportant, opts.context, opts.differences_only
    );
    for s in sections {
        let name = if sections.len() > 1 { s.name } else { "" };
        xml_comparison(&mut out, s, name, opts, bookmarks, "  ");
    }
    out.push_str("</textReport>\n");
    out
}

/// One `<comparison>` element (shared with folder XML reports), indented by
/// `indent`.
pub(crate) fn xml_comparison(
    out: &mut String,
    s: &TextSection,
    name: &str,
    opts: &TextReportOptions,
    bookmarks: &[TextBookmark],
    indent: &str,
) {
    let rows = &s.diff.rows;
    let ignore = opts.ignore_unimportant;
    if name.is_empty() {
        let _ = writeln!(out, "{indent}<comparison>");
    } else {
        let _ = writeln!(out, "{indent}<comparison name=\"{}\">", xml::attr(name));
    }
    let lines = |pick: fn(&DiffRow) -> Option<usize>| rows.iter().filter_map(pick).count();
    let _ = writeln!(
        out,
        "{indent}  <left path=\"{}\" lines=\"{}\"/>\n{indent}  <right path=\"{}\" lines=\"{}\"/>",
        xml::attr(s.left),
        lines(|r| r.left_no),
        xml::attr(s.right),
        lines(|r| r.right_no)
    );
    let sum = summarize(s.diff, ignore);
    let _ = writeln!(
        out,
        "{indent}  <summary differences=\"{}\" changed=\"{}\" leftOnly=\"{}\" rightOnly=\"{}\" unimportant=\"{}\"/>",
        sum.sections, sum.changed, sum.left_only, sum.right_only, sum.unimportant
    );
    for (n, block) in difference_blocks(rows, ignore).into_iter().enumerate() {
        let (ls, lc) = block_span(rows, &block, |r| r.left_no);
        let (rs, rc) = block_span(rows, &block, |r| r.right_no);
        let _ = writeln!(
            out,
            "{indent}  <difference index=\"{}\" kind=\"{}\" leftStart=\"{ls}\" leftCount=\"{lc}\" rightStart=\"{rs}\" rightCount=\"{rc}\">",
            n + 1,
            block_kind(&rows[block.clone()])
        );
        let block_rows = &rows[block];
        let sides = block_rows
            .iter()
            .map(|r| ("left", r.left_no, &r.left))
            .chain(block_rows.iter().map(|r| ("right", r.right_no, &r.right)));
        for (side, n, text) in sides {
            if let (Some(n), Some(t)) = (n, joined(text)) {
                let _ = writeln!(
                    out,
                    "{indent}    <line side=\"{side}\" n=\"{n}\">{}</line>",
                    xml::text(&t)
                );
            }
        }
        let _ = writeln!(out, "{indent}  </difference>");
    }
    let mut marks: Vec<&TextBookmark> = bookmarks.iter().collect();
    marks.sort_by_key(|b| (b.side, b.line));
    for b in marks {
        let side = if b.side == BookmarkSide::Left {
            "left"
        } else {
            "right"
        };
        match b.comment() {
            Some(c) => {
                let _ = writeln!(
                    out,
                    "{indent}  <bookmark side=\"{side}\" line=\"{}\">{}</bookmark>",
                    b.line,
                    xml::text(c)
                );
            }
            None => {
                let _ = writeln!(
                    out,
                    "{indent}  <bookmark side=\"{side}\" line=\"{}\"/>",
                    b.line
                );
            }
        }
    }
    let _ = writeln!(out, "{indent}</comparison>");
}

#[cfg(test)]
mod tests {
    use super::*;
    use pcompare_core::text::{diff_text, TextDiffOptions, WhitespaceMode};

    const LEFT: &str = "one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\n";
    const RIGHT: &str = "one\ntwo\nTHREE!\nfour\nfive\nsix\nseven\neight\nnine\n";

    fn diff(l: &str, r: &str) -> TextDiffResult {
        diff_text(l, r, &TextDiffOptions::default()).unwrap()
    }

    fn section(d: &TextDiffResult) -> TextSection<'_> {
        TextSection {
            name: "",
            left: "a.txt",
            right: "b.txt",
            diff: d,
        }
    }

    fn opts(f: impl FnOnce(&mut TextReportOptions)) -> TextReportOptions {
        let mut o = TextReportOptions::default();
        f(&mut o);
        o
    }

    #[test]
    fn patch_matches_gnu_unified_format() {
        let d = diff(LEFT, RIGHT);
        let p = render(
            &[section(&d)],
            &opts(|o| o.format = TextReportFormat::Patch),
            "",
        );
        // Changes 6 lines apart share one hunk with 3 lines of context, like GNU diff.
        assert_eq!(
            p,
            "--- a.txt\n+++ b.txt\n\
             @@ -1,8 +1,9 @@\n one\n two\n-three\n+THREE!\n four\n five\n six\n seven\n eight\n+nine\n"
        );
        let p = render(
            &[section(&d)],
            &opts(|o| {
                o.format = TextReportFormat::Patch;
                o.context = 1;
            }),
            "",
        );
        assert_eq!(
            p,
            "--- a.txt\n+++ b.txt\n\
             @@ -2,3 +2,3 @@\n two\n-three\n+THREE!\n four\n\
             @@ -8 +8,2 @@\n eight\n+nine\n"
        );
    }

    #[test]
    fn patch_hunks_merge_when_context_overlaps_and_handle_empty_sides() {
        let d = diff("a\nb\n", "a\nX\nb\nY\n");
        let p = render(
            &[section(&d)],
            &opts(|o| o.format = TextReportFormat::Patch),
            "",
        );
        assert_eq!(p, "--- a.txt\n+++ b.txt\n@@ -1,2 +1,4 @@\n a\n+X\n b\n+Y\n");
        let d = diff("", "new\n");
        let p = render(
            &[section(&d)],
            &opts(|o| o.format = TextReportFormat::Patch),
            "",
        );
        assert_eq!(p, "--- a.txt\n+++ b.txt\n@@ -0,0 +1 @@\n+new\n");
        let same = diff("x\n", "x\n");
        assert_eq!(
            render(
                &[section(&same)],
                &opts(|o| o.format = TextReportFormat::Patch),
                ""
            ),
            ""
        );
    }

    #[test]
    fn ignored_whitespace_is_context_in_a_patch_unless_it_matters() {
        let rules = TextDiffOptions {
            whitespace: WhitespaceMode::IgnoreAll,
            ..Default::default()
        };
        let d = diff_text("a b\nc\n", "ab\nC\n", &rules).unwrap();
        let p = render(
            &[section(&d)],
            &opts(|o| o.format = TextReportFormat::Patch),
            "",
        );
        assert_eq!(p, "--- a.txt\n+++ b.txt\n@@ -1,2 +1,2 @@\n a b\n-c\n+C\n");
        let strict = render(
            &[section(&d)],
            &opts(|o| {
                o.format = TextReportFormat::Patch;
                o.ignore_unimportant = false;
            }),
            "",
        );
        assert!(strict.contains("-a b\n-c\n+ab\n+C\n"), "{strict}");
    }

    #[test]
    fn side_by_side_text_aligns_columns_and_marks_rows() {
        let d = diff("alpha\nbeta\n", "alpha\nBETA\ngamma\n");
        let t = render(
            &[section(&d)],
            &opts(|o| o.format = TextReportFormat::Text),
            "2026-01-01 10:00",
        );
        let got: Vec<&str> = t.lines().collect();
        assert_eq!(
            got,
            [
                "Text Compare",
                "Produced 2026-01-01 10:00 by PCompare",
                "",
                "Left:  a.txt",
                "Right: b.txt",
                "1 difference section: 1 changed, 0 left only, 1 right only",
                "",
                "1  alpha      1  alpha",
                "2  beta     | 2  BETA",
                "            > 3  gamma",
            ]
        );
    }

    #[test]
    fn differences_only_hides_unchanged_runs_with_context() {
        let left: String = (1..=20).map(|i| format!("line {i}\n")).collect();
        let right = left.replace("line 10\n", "line ten\n");
        let d = diff(&left, &right);
        let t = render(
            &[section(&d)],
            &opts(|o| {
                o.format = TextReportFormat::Text;
                o.layout = TextLayout::Unified;
                o.differences_only = true;
                o.context = 1;
                o.line_numbers = false;
            }),
            "",
        );
        let body: Vec<&str> = t.lines().skip(6).collect();
        assert_eq!(
            body,
            [
                "    ... 8 unchanged lines ...",
                "  line 9",
                "- line 10",
                "+ line ten",
                "  line 11",
                "    ... 9 unchanged lines ...",
            ]
        );
    }

    #[test]
    fn html_side_by_side_highlights_changed_words() {
        let d = diff("let x = 1;\n", "let y = 1;\n");
        let h = render(&[section(&d)], &TextReportOptions::default(), "now");
        assert!(h.starts_with("<!DOCTYPE html>"));
        assert!(h.contains("<title>Text Compare</title>"));
        assert!(h.contains("<tr class=\"chg\"><td class=\"ln\">1</td><td class=\"t l\">let <span class=\"m\">x</span> = 1;</td><td class=\"mid\">|</td><td class=\"ln\">1</td><td class=\"t r\">let <span class=\"m\">y</span> = 1;</td></tr>"), "{h}");
        assert!(h.contains("1 difference section: 1 changed"));
        assert!(h.contains("Produced now by PCompare"));
    }

    #[test]
    fn html_unified_lists_removed_then_added_lines_and_escapes() {
        let d = diff("<a>\nsame\n", "<b>\nsame\n");
        let h = render(
            &[section(&d)],
            &opts(|o| {
                o.layout = TextLayout::Unified;
                o.line_numbers = false;
            }),
            "",
        );
        let del = h
            .find("<tr class=\"del\"><td class=\"sign\">-</td><td class=\"t\"><span class=\"m\">&lt;a&gt;</span></td></tr>")
            .expect(&h);
        let add = h
            .find("<tr class=\"add\"><td class=\"sign\">+</td><td class=\"t\"><span class=\"m\">&lt;b&gt;</span></td></tr>")
            .expect(&h);
        assert!(del < add);
        assert!(h.contains(
            "<tr class=\"eq\"><td class=\"sign\">&nbsp;</td><td class=\"t\">same</td></tr>"
        ));
    }

    #[test]
    fn identical_files_say_so_and_several_sections_get_headings() {
        let same = diff("x\n", "x\n");
        let changed = diff("x\n", "y\n");
        let sections = [
            TextSection {
                name: "dir/one.txt",
                ..section(&same)
            },
            TextSection {
                name: "dir/two.txt",
                ..section(&changed)
            },
        ];
        let h = render(&sections, &opts(|o| o.differences_only = true), "");
        assert!(h.contains("<h2>dir/one.txt</h2>") && h.contains("<h2>dir/two.txt</h2>"));
        assert!(h.contains("The files are identical"));
        let t = render(&sections, &opts(|o| o.format = TextReportFormat::Text), "");
        assert!(t.contains("dir/one.txt\nLeft:  a.txt") && t.contains(&"=".repeat(78)));
    }

    #[test]
    fn normal_diff_matches_gnu_output() {
        let d = diff(LEFT, RIGHT);
        let o = opts(|o| o.format = TextReportFormat::NormalDiff);
        assert_eq!(
            render(&[section(&d)], &o, ""),
            "3c3\n< three\n---\n> THREE!\n8a9\n> nine\n"
        );
        // Deletions name the line they follow on the right; ranges use first,last.
        let d = diff("a\nb\nc\nd\ne\n", "a\nd\ne\nX\nY\n");
        assert_eq!(
            render(&[section(&d)], &o, ""),
            "2,3d1\n< b\n< c\n5a4,5\n> X\n> Y\n"
        );
        let d = diff("1\n2\n3\n4\n", "1\nA\nB\n4\n");
        assert_eq!(
            render(&[section(&d)], &o, ""),
            "2,3c2,3\n< 2\n< 3\n---\n> A\n> B\n"
        );
        let same = diff("x\n", "x\n");
        assert_eq!(render(&[section(&same)], &o, ""), "");
    }

    #[test]
    fn xml_report_is_well_formed_and_complete() {
        let d = diff(
            "keep\n<old> & \"q\"\nsame\n",
            "keep\n<new> & 'q'\nsame\nadded\n",
        );
        let marks = [
            TextBookmark {
                side: BookmarkSide::Left,
                line: 2,
                comment: Some("check </comparison> & co".into()),
            },
            TextBookmark {
                side: BookmarkSide::Right,
                line: 4,
                comment: None,
            },
        ];
        let x = render_annotated(
            &[section(&d)],
            &opts(|o| {
                o.format = TextReportFormat::Xml;
                o.title = "A \"title\" <x>".into();
            }),
            &marks,
            "2026-01-01 10:00",
        );
        let names = crate::xml::tests::parse(&x);
        assert_eq!(names.iter().filter(|n| *n == "difference").count(), 2);
        assert_eq!(names.iter().filter(|n| *n == "bookmark").count(), 2);
        assert_eq!(names[0], "textReport");
        assert!(
            x.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<textReport version=\"1\"")
        );
        assert!(x.contains("title=\"A &quot;title&quot; &lt;x&gt;\""), "{x}");
        assert!(x.contains("<summary differences=\"2\" changed=\"1\" leftOnly=\"0\" rightOnly=\"1\" unimportant=\"0\"/>"), "{x}");
        assert!(x.contains("<difference index=\"1\" kind=\"changed\" leftStart=\"2\" leftCount=\"1\" rightStart=\"2\" rightCount=\"1\">"));
        assert!(
            x.contains("<line side=\"left\" n=\"2\">&lt;old&gt; &amp; \"q\"</line>"),
            "{x}"
        );
        assert!(x.contains("<difference index=\"2\" kind=\"rightOnly\" leftStart=\"3\" leftCount=\"0\" rightStart=\"4\" rightCount=\"1\">"));
        assert!(x.contains("<bookmark side=\"right\" line=\"4\"/>"));
        // Several pairs: one named comparison each.
        let same = diff("x\n", "x\n");
        let two = [
            TextSection {
                name: "one",
                ..section(&same)
            },
            TextSection {
                name: "two & three",
                ..section(&d)
            },
        ];
        let x = render(&two, &opts(|o| o.format = TextReportFormat::Xml), "");
        assert_eq!(
            crate::xml::tests::parse(&x)
                .iter()
                .filter(|n| *n == "comparison")
                .count(),
            2
        );
        assert!(x.contains("<comparison name=\"two &amp; three\">"));
    }

    #[test]
    fn html_shows_bookmark_comments_and_keeps_them_in_differences_only() {
        let left: String = (1..=20).map(|i| format!("line {i}\n")).collect();
        let right = left.replace("line 10\n", "line ten\n");
        let d = diff(&left, &right);
        let marks = [
            TextBookmark {
                side: BookmarkSide::Left,
                line: 2,
                comment: Some("why <this>?".into()),
            },
            TextBookmark {
                side: BookmarkSide::Right,
                line: 10,
                comment: None,
            },
        ];
        let h = render_annotated(
            &[section(&d)],
            &opts(|o| {
                o.differences_only = true;
                o.context = 0;
            }),
            &marks,
            "",
        );
        assert!(
            h.contains("<td class=\"ln bm\" id=\"bm-l2\" title=\"why &lt;this&gt;?\">2</td>"),
            "{h}"
        );
        assert!(h.contains("<tr class=\"note\"><td colspan=\"5\"><span class=\"tag\">Left line 2</span>why &lt;this&gt;?</td></tr>"), "{h}");
        assert!(h.contains("<tr class=\"chg bm\">"));
        assert!(h.contains("<h3>Comments</h3>"));
        assert!(h.contains("<a href=\"#bm-l2\">Left line 2</a>"));
        assert!(h.contains("<a href=\"#bm-r10\">Right line 10</a>"));
        // Unified layout marks the right-side line on its added row.
        let u = render_annotated(
            &[section(&d)],
            &opts(|o| o.layout = TextLayout::Unified),
            &marks,
            "",
        );
        assert!(
            u.contains(
                "<tr class=\"add bm\"><td class=\"ln\"></td><td class=\"ln bm\" id=\"bm-r10\""
            ),
            "{u}"
        );
    }
}
