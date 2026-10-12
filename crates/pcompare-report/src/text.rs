//! Text comparison reports: side-by-side or unified layouts as HTML (with
//! the app's diff colours and intra-line highlights) or plain text, and
//! unified patches.

use crate::html::{self, escape, Style};
use crate::{display_width, expand_tabs, pad};
use pcompare_core::text::{DiffRow, RowKind, Segment, TextDiffResult};
use serde::{Deserialize, Serialize};
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
    match opts.format {
        TextReportFormat::Html => render_html(sections, opts, generated),
        TextReportFormat::Text => render_text(sections, opts, generated),
        TextReportFormat::Patch => sections.iter().map(|s| patch(s, opts)).collect(),
    }
}

fn title(opts: &TextReportOptions) -> &str {
    if opts.title.trim().is_empty() {
        "Text Compare"
    } else {
        opts.title.trim()
    }
}

// ---------- shared helpers ----------

fn joined(side: &Option<Vec<Segment>>) -> Option<String> {
    side.as_ref()
        .map(|segs| segs.iter().map(|s| s.text.as_str()).collect())
}

/// Whether a row is a difference worth reporting.
fn is_difference(row: &DiffRow, ignore_unimportant: bool) -> bool {
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

/// Counts for the summary line, honouring "ignore unimportant".
struct Summary {
    sections: usize,
    changed: usize,
    left_only: usize,
    right_only: usize,
    unimportant: usize,
}

fn summarize(diff: &TextDiffResult, ignore_unimportant: bool) -> Summary {
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

fn summary_text(s: &Summary, ignore_unimportant: bool) -> String {
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

fn no(n: Option<usize>) -> String {
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

fn render_html(sections: &[TextSection], opts: &TextReportOptions, generated: &str) -> String {
    let title = title(opts);
    let mut body = format!("<h1>{}</h1>\n", escape(title));
    if !generated.is_empty() {
        let _ = writeln!(
            body,
            "<div class=\"meta\">Produced {} by PCompare</div>",
            escape(generated)
        );
    }
    for s in sections {
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
        let ranges = shown_ranges(&s.diff.rows, !opts.differences_only, opts.context, |r| {
            is_difference(r, opts.ignore_unimportant)
        });
        if ranges.is_empty() {
            continue;
        }
        match opts.layout {
            TextLayout::SideBySide => side_by_side_html(&mut body, s.diff, &ranges, opts),
            TextLayout::Unified => unified_html(&mut body, s.diff, &ranges, opts),
        }
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

fn for_each_range(
    out: &mut String,
    rows: usize,
    ranges: &[Range<usize>],
    cols: usize,
    mut emit: impl FnMut(&mut String, Range<usize>),
) {
    let mut next = 0;
    for r in ranges {
        gap_row(out, r.start - next, cols);
        next = r.end;
        emit(out, r.clone());
    }
    gap_row(out, rows - next, cols);
}

fn side_by_side_html(
    out: &mut String,
    diff: &TextDiffResult,
    ranges: &[Range<usize>],
    opts: &TextReportOptions,
) {
    let cols = if opts.line_numbers { 5 } else { 3 };
    out.push_str("<table class=\"diff sbs\">\n");
    for_each_range(out, diff.rows.len(), ranges, cols, |out, range| {
        for row in &diff.rows[range] {
            let marks = matches!(row.kind, RowKind::Changed | RowKind::Unimportant);
            let _ = write!(out, "<tr class=\"{}\">", row_class(row.kind));
            if opts.line_numbers {
                let _ = write!(out, "<td class=\"ln\">{}</td>", no(row.left_no));
            }
            let _ = write!(
                out,
                "<td class=\"t l\">{}</td><td class=\"mid\">{}</td>",
                segments_html(&row.left, marks),
                row_marker(row.kind)
            );
            if opts.line_numbers {
                let _ = write!(out, "<td class=\"ln\">{}</td>", no(row.right_no));
            }
            let _ = writeln!(
                out,
                "<td class=\"t r\">{}</td></tr>",
                segments_html(&row.right, marks)
            );
        }
    });
    out.push_str("</table>\n");
}

fn unified_html(
    out: &mut String,
    diff: &TextDiffResult,
    ranges: &[Range<usize>],
    opts: &TextReportOptions,
) {
    let cols = if opts.line_numbers { 4 } else { 2 };
    let line = |out: &mut String,
                class: &str,
                l: Option<usize>,
                r: Option<usize>,
                sign: &str,
                text: String| {
        let _ = write!(out, "<tr class=\"{class}\">");
        if opts.line_numbers {
            let _ = write!(
                out,
                "<td class=\"ln\">{}</td><td class=\"ln\">{}</td>",
                no(l),
                no(r)
            );
        }
        let _ = writeln!(
            out,
            "<td class=\"sign\">{sign}</td><td class=\"t\">{text}</td></tr>"
        );
    };
    out.push_str("<table class=\"diff uni\">\n");
    for_each_range(out, diff.rows.len(), ranges, cols, |out, range| {
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
}
