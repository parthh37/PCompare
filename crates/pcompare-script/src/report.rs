//! The file report commands (`file-report`, `text-report`, `hex-report`,
//! `data-report`, …): which file pairs they cover, how each pair is read
//! (the "File formats" rules convert documents to text; binary files get a
//! hex comparison) and the standard layouts — side-by-side,
//! interleaved, summary, statistics, XML and patches in normal, context or
//! unified form.

use crate::interp::{
    detail, folders, info, left_rel, prepare_output, resolve, selected_pairs, Env, Session, State,
};
use crate::parse::{
    Comparison, FileLayout, FileReport, LineDisplay, OutputFormat, PatchStyle, ReportKind,
};
use pcompare_convert::ConvertConfig;
use pcompare_core::text::{DiffRow, RowKind, TextDiffOptions, TextDiffResult};
use pcompare_core::textfile;
use pcompare_report::text::{TextLayout, TextReportFormat, TextReportOptions, TextSection};
use std::fmt::Write as _;
use std::io::Read;

type CmdResult = Result<(), String>;

/// One file pair to report on, read into memory.
struct Pair {
    /// Heading in multi-pair reports (the relative path); empty for one pair.
    name: String,
    left: String,
    right: String,
    lbytes: Vec<u8>,
    rbytes: Vec<u8>,
}

/// How a pair is compared.
enum Content {
    Text {
        left: String,
        right: String,
        grammar: Option<String>,
    },
    Binary,
}

fn read_local(env: &Env, path: &str) -> Result<Vec<u8>, String> {
    let local = env.host.local_file(path)?;
    std::fs::read(&local).map_err(|e| format!("{}: {e}", local.display()))
}

fn named_pair(env: &Env, l: &str, r: &str) -> Result<Pair, String> {
    let (l, r) = (resolve(env, l), resolve(env, r));
    Ok(Pair {
        name: String::new(),
        lbytes: read_local(env, &l)?,
        rbytes: read_local(env, &r)?,
        left: l,
        right: r,
    })
}

/// The pairs a report covers: its `<comparison>`, else the loaded files,
/// else the files below the selection that exist on both sides.
fn pairs(st: &mut State, env: &Env, r: &FileReport) -> Result<Vec<Pair>, String> {
    match &r.comparison {
        Some(Comparison::Pair(l, rt)) => return Ok(vec![named_pair(env, l, rt)?]),
        Some(Comparison::Session(name)) => {
            let s = env
                .host
                .session(name)
                .ok_or_else(|| format!("no saved session named \"{name}\""))?;
            if crate::interp::is_folder_kind(&s.kind) {
                return Err(format!(
                    "\"{name}\" is a folder session; {} needs a file comparison",
                    r.kind.command()
                ));
            }
            return Ok(vec![named_pair(env, &s.left, &s.right)?]);
        }
        None => {}
    }
    match &st.session {
        Session::None => Err("nothing loaded (load files, or name two files to compare)".into()),
        Session::Files { left, right } => Ok(vec![named_pair(env, left, right)?]),
        Session::Folders(_) => {
            let f = folders(st)?;
            let read = |side: &crate::host::Side, rel: &str| -> Result<Vec<u8>, String> {
                let path = side.join(rel);
                let mut buf = Vec::new();
                side.fs
                    .open(&path)
                    .and_then(|mut r| {
                        r.read_to_end(&mut buf)
                            .map_err(|e| pcompare_core::CoreError::io(&path, e))
                    })
                    .map_err(|e| e.to_string())?;
                Ok(buf)
            };
            selected_pairs(f)?
                .into_iter()
                .map(|e| {
                    Ok(Pair {
                        name: e.rel_path.clone(),
                        left: pcompare_report::folder::join_root(&f.left.shown, left_rel(e)),
                        right: pcompare_report::folder::join_root(&f.right.shown, &e.rel_path),
                        lbytes: read(&f.left, left_rel(e))?,
                        rbytes: read(&f.right, &e.rel_path)?,
                    })
                })
                .collect()
        }
    }
}

fn file_name(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

/// Reads a pair the way the report kind (and, for `file-report`, the file
/// format rules) say.
fn content(kind: ReportKind, formats: &ConvertConfig, p: &Pair) -> Result<Content, String> {
    if kind == ReportKind::Hex {
        return Ok(Content::Binary);
    }
    let rule = formats
        .format_for(file_name(&p.left))
        .or_else(|| formats.format_for(file_name(&p.right)));
    let grammar = rule
        .map(|r| r.grammar.trim().to_owned())
        .filter(|g| !g.is_empty());
    if let Some(rule) = rule.filter(|r| r.converts()) {
        let convert = |bytes: &[u8], name: &str| -> Result<String, String> {
            if !rule.command.trim().is_empty() {
                let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
                let input = dir.path().join(file_name(name));
                let output = dir.path().join("converted.txt");
                std::fs::write(&input, bytes).map_err(|e| e.to_string())?;
                return pcompare_convert::run_conversion_command(&rule.command, &input, &output)
                    .map_err(|e| e.to_string());
            }
            match pcompare_convert::convert_bytes(rule.converter, bytes, file_name(name)) {
                Ok(Some(t)) => Ok(t),
                Ok(None) => Ok(textfile::decode_auto(bytes).text),
                Err(e) => Err(format!("{name}: {e}")),
            }
        };
        return Ok(Content::Text {
            left: convert(&p.lbytes, &p.left)?,
            right: convert(&p.rbytes, &p.right)?,
            grammar,
        });
    }
    if textfile::is_probably_binary(&p.lbytes) || textfile::is_probably_binary(&p.rbytes) {
        return Ok(Content::Binary);
    }
    Ok(Content::Text {
        left: textfile::decode_auto(&p.lbytes).text,
        right: textfile::decode_auto(&p.rbytes).text,
        grammar,
    })
}

pub(crate) fn file_report(st: &mut State, env: &mut Env, r: &FileReport) -> CmdResult {
    let out = prepare_output(env, &r.output)?;
    let pairs = pairs(st, env, r)?;
    if pairs.is_empty() {
        return Err(format!("no file pairs selected for {}", r.kind.command()));
    }
    let formats = env.host.file_formats();
    let ignore_unimportant = r
        .ignore_unimportant
        .unwrap_or(st.criteria.ignore_unimportant);
    let mut texts = Vec::new();
    let mut binaries = Vec::new();
    for p in &pairs {
        match content(r.kind, &formats, p)? {
            Content::Text { left, right, grammar } => {
                let rules = TextDiffOptions {
                    grammar,
                    ..TextDiffOptions::default()
                };
                let diff = pcompare_core::text::diff_named_text(
                    &left,
                    Some(file_name(&p.left)),
                    &right,
                    Some(file_name(&p.right)),
                    &rules,
                )
                .map_err(|e| e.to_string())?;
                texts.push((p, diff));
            }
            Content::Binary if r.kind == ReportKind::Text => {
                detail(st, &format!("skipped binary file {}", p.left));
            }
            Content::Binary => binaries.push(p),
        }
    }
    if texts.is_empty() && binaries.is_empty() {
        return Err("every selected file is binary; text reports need text files".into());
    }
    let html = matches!(r.format, OutputFormat::HtmlColor | OutputFormat::HtmlMono);
    let title = if r.title.is_empty() {
        match r.kind {
            ReportKind::Hex => "Hex Compare",
            ReportKind::Text => "Text Compare",
            _ => "File Compare",
        }
        .to_owned()
    } else {
        r.title.clone()
    };
    let shown: Vec<(&Pair, TextDiffResult)> = texts
        .into_iter()
        .map(|(p, d)| {
            let d = if r.display == LineDisplay::Matches {
                only_matches(d, ignore_unimportant)
            } else {
                d
            };
            (p, d)
        })
        .collect();
    let sections: Vec<TextSection> = shown
        .iter()
        .map(|(p, d)| TextSection {
            name: &p.name,
            left: &p.left,
            right: &p.right,
            diff: d,
        })
        .collect();
    let stamp = pcompare_report::now_stamp();
    let (diffs_only, context) = match r.display {
        LineDisplay::All | LineDisplay::Matches => (false, 3),
        LineDisplay::Mismatches => (true, 0),
        LineDisplay::Context(n) => (true, n),
    };
    let opts = |format, layout| TextReportOptions {
        layout,
        format,
        differences_only: diffs_only,
        context,
        line_numbers: r.line_numbers,
        ignore_unimportant,
        mono: r.format == OutputFormat::HtmlMono,
        wrap: r.wrap,
        title: if r.title.is_empty() {
            String::new()
        } else {
            r.title.clone()
        },
    };
    let page_format = if html {
        TextReportFormat::Html
    } else {
        TextReportFormat::Text
    };
    let mut report = if sections.is_empty() {
        String::new()
    } else {
        match r.layout {
            FileLayout::SideBySide => pcompare_report::text::render(
                &sections,
                &opts(page_format, TextLayout::SideBySide),
                &stamp,
            ),
            FileLayout::Interleaved => pcompare_report::text::render(
                &sections,
                &opts(page_format, TextLayout::Unified),
                &stamp,
            ),
            FileLayout::Xml => pcompare_report::text::render(
                &sections,
                &opts(TextReportFormat::Xml, TextLayout::SideBySide),
                &stamp,
            ),
            FileLayout::Patch => match r.patch {
                PatchStyle::Normal => pcompare_report::text::render(
                    &sections,
                    &opts(TextReportFormat::NormalDiff, TextLayout::Unified),
                    &stamp,
                ),
                PatchStyle::Unified => pcompare_report::text::render(
                    &sections,
                    &TextReportOptions {
                        context: if diffs_only { context } else { 3 },
                        ..opts(TextReportFormat::Patch, TextLayout::Unified)
                    },
                    &stamp,
                ),
                PatchStyle::Context => sections
                    .iter()
                    .map(|s| context_diff(s, if diffs_only { context } else { 3 }, ignore_unimportant))
                    .collect(),
            },
            FileLayout::Statistics => statistics(&sections),
            FileLayout::Summary => summary(&sections, ignore_unimportant, html, &title),
        }
    };
    if !binaries.is_empty() {
        let hex: String = binaries
            .iter()
            .map(|p| hex_section(p, r, html, pairs.len() > 1))
            .collect();
        report = if report.is_empty() {
            wrap_page(&hex, html, &title)
        } else if html {
            match report.rfind("</body>") {
                Some(i) => format!("{}{hex}{}", &report[..i], &report[i..]),
                None => report + &hex,
            }
        } else {
            report + "\n" + &hex
        };
    }
    std::fs::write(&out, report).map_err(|e| format!("{}: {e}", out.display()))?;
    if r.strikeout_left || r.strikeout_right {
        detail(st, "strikeout options are not drawn in PCompare reports");
    }
    info(
        st,
        env,
        &format!(
            "Wrote {} {} ({} file pair(s))",
            r.kind.command(),
            out.display(),
            sections.len() + binaries.len()
        ),
    );
    Ok(())
}

/// `display-matches`: only the lines that match.
fn only_matches(mut d: TextDiffResult, ignore_unimportant: bool) -> TextDiffResult {
    d.rows.retain(|row| {
        row.kind == RowKind::Equal || (ignore_unimportant && row.kind == RowKind::Unimportant)
    });
    d.hunks.clear();
    d
}

fn row_text(segs: &Option<Vec<pcompare_core::text::Segment>>) -> String {
    segs.as_ref()
        .map(|s| s.iter().map(|g| g.text.as_str()).collect())
        .unwrap_or_default()
}

fn is_change(row: &DiffRow, ignore_unimportant: bool) -> bool {
    match row.kind {
        RowKind::Equal => false,
        RowKind::Unimportant => !ignore_unimportant,
        _ => true,
    }
}

/// `diff -c` output for one pair.
fn context_diff(s: &TextSection, context: usize, ignore_unimportant: bool) -> String {
    let rows = &s.diff.rows;
    let changed: Vec<usize> = (0..rows.len())
        .filter(|&i| is_change(&rows[i], ignore_unimportant))
        .collect();
    let mut out = String::new();
    if changed.is_empty() {
        return out;
    }
    let _ = writeln!(out, "*** {}", s.left);
    let _ = writeln!(out, "--- {}", s.right);
    // Group changes whose context overlaps.
    let mut groups: Vec<(usize, usize)> = Vec::new();
    for &i in &changed {
        let start = i.saturating_sub(context);
        let end = (i + context + 1).min(rows.len());
        match groups.last_mut() {
            Some(g) if start <= g.1 => g.1 = end,
            _ => groups.push((start, end)),
        }
    }
    // A run of changed rows marks `!` when it has lines on both sides.
    let mut marks = vec![' '; rows.len()];
    let mut i = 0;
    while i < rows.len() {
        if !is_change(&rows[i], ignore_unimportant) {
            i += 1;
            continue;
        }
        let start = i;
        while i < rows.len() && is_change(&rows[i], ignore_unimportant) {
            i += 1;
        }
        let both = rows[start..i].iter().any(|r| r.left.is_some())
            && rows[start..i].iter().any(|r| r.right.is_some());
        for m in &mut marks[start..i] {
            *m = if both { '!' } else { 'x' };
        }
    }
    let range = |nums: Vec<usize>, before: usize| match (nums.first(), nums.last()) {
        (Some(a), Some(b)) if a == b => a.to_string(),
        (Some(a), Some(b)) => format!("{a},{b}"),
        _ => before.to_string(),
    };
    for (start, end) in groups {
        let slice = &rows[start..end];
        let lnums: Vec<usize> = slice.iter().filter_map(|r| r.left_no).collect();
        let rnums: Vec<usize> = slice.iter().filter_map(|r| r.right_no).collect();
        let before = |f: fn(&DiffRow) -> Option<usize>| {
            rows[..start].iter().rev().find_map(f).unwrap_or(0)
        };
        out.push_str("***************\n");
        let _ = writeln!(out, "*** {} ****", range(lnums, before(|r| r.left_no)));
        let left_changes = (start..end).any(|i| marks[i] != ' ' && rows[i].left.is_some());
        if left_changes {
            for i in start..end {
                if rows[i].left.is_none() {
                    continue;
                }
                let m = match marks[i] {
                    'x' => '-',
                    m => m,
                };
                let _ = writeln!(out, "{m} {}", row_text(&rows[i].left));
            }
        }
        let _ = writeln!(out, "--- {} ----", range(rnums, before(|r| r.right_no)));
        let right_changes = (start..end).any(|i| marks[i] != ' ' && rows[i].right.is_some());
        if right_changes {
            for i in start..end {
                if rows[i].right.is_none() {
                    continue;
                }
                let m = match marks[i] {
                    'x' => '+',
                    m => m,
                };
                let _ = writeln!(out, "{m} {}", row_text(&rows[i].right));
            }
        }
    }
    out
}

fn csv_field(s: &str) -> String {
    if s.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_owned()
    }
}

/// The statistics layout: line counts per pair as CSV.
fn statistics(sections: &[TextSection]) -> String {
    let mut out = String::from(
        "Left File,Right File,Matching Lines,Unimportant Lines,Changed Lines,Left Orphan Lines,Right Orphan Lines,Difference Sections\r\n",
    );
    for s in sections {
        let st = &s.diff.stats;
        let _ = write!(
            out,
            "{},{},{},{},{},{},{},{}\r\n",
            csv_field(s.left),
            csv_field(s.right),
            st.equal,
            st.unimportant,
            st.changed,
            st.left_only,
            st.right_only,
            s.diff.hunks.len()
        );
    }
    out
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn wrap_page(body: &str, html: bool, title: &str) -> String {
    if html {
        format!(
            "<!DOCTYPE html>\n<html><head><meta charset=\"utf-8\"><title>{t}</title>\
             <style>body{{font-family:sans-serif}}pre{{font-family:monospace}}.d{{background:#fdd}}</style>\
             </head><body>\n<h1>{t}</h1>\n<p>Produced: {stamp}</p>\n{body}</body></html>\n",
            t = escape(title),
            stamp = escape(&pcompare_report::now_stamp())
        )
    } else {
        format!(
            "{title}\nProduced: {}\n\n{body}",
            pcompare_report::now_stamp()
        )
    }
}

/// The summary layout: one verdict per pair.
fn summary(sections: &[TextSection], ignore_unimportant: bool, html: bool, title: &str) -> String {
    let mut body = String::new();
    if html {
        body.push_str("<table border=\"1\" cellspacing=\"0\" cellpadding=\"3\">\n<tr><th>Left</th><th>Right</th><th>Result</th></tr>\n");
    }
    for s in sections {
        let st = &s.diff.stats;
        let important = st.changed + st.left_only + st.right_only;
        let verdict = if important == 0 && st.unimportant == 0 {
            "Same".to_owned()
        } else if important == 0 {
            if ignore_unimportant {
                format!("Same ({} unimportant line(s))", st.unimportant)
            } else {
                format!("Only unimportant differences ({} line(s))", st.unimportant)
            }
        } else {
            format!(
                "{} difference section(s): {} changed, {} left orphan, {} right orphan line(s)",
                s.diff.hunks.len(),
                st.changed,
                st.left_only,
                st.right_only
            )
        };
        if html {
            let _ = writeln!(
                body,
                "<tr><td>{}</td><td>{}</td><td>{}</td></tr>",
                escape(s.left),
                escape(s.right),
                escape(&verdict)
            );
        } else {
            let _ = writeln!(body, "Left:  {}\nRight: {}\n{verdict}\n", s.left, s.right);
        }
    }
    if html {
        body.push_str("</table>\n");
    }
    wrap_page(&body, html, title)
}

/// Byte ranges where two buffers differ (same offsets on both sides).
fn byte_diffs(l: &[u8], r: &[u8]) -> Vec<(usize, usize)> {
    let mut out: Vec<(usize, usize)> = Vec::new();
    for i in 0..l.len().max(r.len()) {
        if l.get(i) != r.get(i) {
            match out.last_mut() {
                Some(last) if last.1 == i => last.1 = i + 1,
                _ => out.push((i, i + 1)),
            }
        }
    }
    out
}

fn hex_cells(bytes: &[u8], at: usize) -> (String, String) {
    let chunk = bytes.get(at..(at + 16).min(bytes.len())).unwrap_or(&[]);
    let mut hex = String::new();
    for i in 0..16 {
        match chunk.get(i) {
            Some(b) => {
                let _ = write!(hex, "{b:02X} ");
            }
            None => hex.push_str("   "),
        }
    }
    let text: String = chunk
        .iter()
        .map(|&b| if (0x20..0x7f).contains(&b) { b as char } else { '.' })
        .collect();
    (hex, format!("{text:<16}"))
}

/// A hex comparison of one binary pair in the report's layout.
fn hex_section(p: &Pair, r: &FileReport, html: bool, named: bool) -> String {
    let ranges = byte_diffs(&p.lbytes, &p.rbytes);
    let differing: usize = ranges.iter().map(|(a, b)| b - a).sum();
    let mut text = String::new();
    if named && !p.name.is_empty() {
        let _ = writeln!(text, "== {} ==", p.name);
    }
    let _ = writeln!(text, "Left:  {} ({} bytes)", p.left, p.lbytes.len());
    let _ = writeln!(text, "Right: {} ({} bytes)", p.right, p.rbytes.len());
    let _ = writeln!(
        text,
        "{}",
        if ranges.is_empty() {
            "Same".to_owned()
        } else {
            format!("{differing} byte(s) differ in {} range(s)", ranges.len())
        }
    );
    if r.layout != FileLayout::Summary {
        text.push('\n');
        let rows = p.lbytes.len().max(p.rbytes.len()).div_ceil(16);
        let differs = |row: usize| {
            let (a, b) = (row * 16, row * 16 + 16);
            ranges.iter().any(|&(s, e)| s < b && e > a)
        };
        let context = match r.display {
            LineDisplay::Context(n) => Some(n),
            LineDisplay::Mismatches => Some(0),
            _ => None,
        };
        let shown = |row: usize| match (r.display, context) {
            (LineDisplay::Matches, _) => !differs(row),
            (_, None) => true,
            (_, Some(n)) => (row.saturating_sub(n)..=(row + n).min(rows.saturating_sub(1))).any(differs),
        };
        let mut gap = false;
        for row in 0..rows {
            if !shown(row) {
                gap = true;
                continue;
            }
            if gap {
                text.push_str("...\n");
                gap = false;
            }
            let at = row * 16;
            let mark = if differs(row) { '*' } else { ' ' };
            let (lh, lt) = hex_cells(&p.lbytes, at);
            let (rh, rt) = hex_cells(&p.rbytes, at);
            if r.layout == FileLayout::Interleaved {
                let _ = writeln!(text, "{mark}{at:08X} < {lh} {lt}");
                let _ = writeln!(text, "{mark}{at:08X} > {rh} {rt}");
            } else {
                let _ = writeln!(text, "{mark}{at:08X}  {lh} {lt} | {rh} {rt}");
            }
        }
    }
    text.push('\n');
    if html {
        format!("<pre>{}</pre>\n", escape(&text))
    } else {
        text
    }
}

#[cfg(test)]
mod tests {
    use crate::host::LocalHost;
    use crate::interp::tests::{fixture, run_in, run_with};

    fn read(d: &std::path::Path, p: &str) -> String {
        std::fs::read_to_string(d.join(p)).unwrap()
    }

    #[test]
    fn folder_selection_reports_in_every_text_layout() {
        let d = fixture();
        let r = run_in(
            d.path(),
            "criteria binary\nload l r\nexpand all\nselect all.diff.files\n\
             text-report layout:patch output-to:out/normal.diff\n\
             text-report layout:patch options:patch-unified output-to:out/unified.diff\n\
             text-report layout:patch options:patch-context output-to:out/context.diff\n\
             text-report layout:side-by-side options:line-numbers output-to:out/sbs.html\n\
             text-report layout:interleaved output-to:out/inter.txt\n\
             text-report layout:summary output-to:out/summary.txt\n\
             text-report layout:statistics output-to:out/stats.csv\n\
             text-report layout:xml output-to:out/r.xml\n",
            false,
        );
        assert_eq!(r.outcome.errors, 0, "{}{}", r.out, r.err);
        assert!(read(d.path(), "out/normal.diff").contains("2c2\n< two\n---\n> TWO!\n"));
        assert!(read(d.path(), "out/unified.diff").contains("-two\n+TWO!\n"));
        let ctx = read(d.path(), "out/context.diff");
        assert!(
            ctx.contains("***************\n*** 1,2 ****\n  one\n! two\n--- 1,2 ----\n  one\n! TWO!\n"),
            "{ctx}"
        );
        assert!(read(d.path(), "out/sbs.html").contains("<span class=\"m\">"));
        assert!(read(d.path(), "out/summary.txt").contains("1 difference section(s)"));
        let stats = read(d.path(), "out/stats.csv");
        assert!(stats.starts_with("Left File,Right File,Matching Lines"), "{stats}");
        assert!(stats.contains(",1,0,1,0,0,1\r\n"), "{stats}");
        assert!(read(d.path(), "out/r.xml").contains("<textReport"));
    }

    #[test]
    fn the_comparison_argument_names_files_or_a_session() {
        let d = fixture();
        // Sample: file-report with the pair as %1 %2 and the report as %3.
        let script = "file-report layout:side-by-side &\n options:display-mismatches &\n output-to:\"%3\" \"%1\" \"%2\"\n";
        let params = ["l/sub/changed.txt", "r/sub/changed.txt", "out/pair.txt"].map(String::from);
        let r = run_with(d.path(), script, &params, &LocalHost::temporary(), false, false);
        assert_eq!(r.outcome.errors, 0, "{}{}", r.out, r.err);
        let txt = read(d.path(), "out/pair.txt");
        assert!(txt.contains("TWO!") && !txt.contains("Produced: \n"), "{txt}");
        let store = pcompare_core::session::SessionStore::new(d.path().join("s.json"));
        store
            .save(pcompare_core::session::SessionInput {
                id: None,
                name: "Changed".into(),
                kind: "text".into(),
                left: d.path().join("l/sub/changed.txt").to_string_lossy().into(),
                right: d.path().join("r/sub/changed.txt").to_string_lossy().into(),
                base: None,
                output: None,
                settings: serde_json::Value::Null,
            })
            .unwrap();
        let host = LocalHost::temporary().with_sessions(store);
        let r = run_with(
            d.path(),
            "text-report layout:patch options:patch-unified output-to:out/s.diff Changed\n",
            &[],
            &host,
            false,
            false,
        );
        assert_eq!(r.outcome.errors, 0, "{}{}", r.out, r.err);
        assert!(read(d.path(), "out/s.diff").contains("+TWO!"));
        let r = run_with(
            d.path(),
            "text-report layout:summary output-to:x.txt Nope\n",
            &[],
            &host,
            false,
            false,
        );
        assert!(r.err.contains("no saved session named \"Nope\""), "{}", r.err);
    }

    #[test]
    fn file_report_uses_hex_for_binary_files_and_rules_for_documents() {
        let d = fixture();
        std::fs::write(d.path().join("l/b.bin"), [0u8, 1, 2, 3, 4, 5, 6, 7, 8, 9]).unwrap();
        std::fs::write(d.path().join("r/b.bin"), [0u8, 1, 2, 0xFF, 4, 5, 6, 7, 8, 9]).unwrap();
        let r = run_in(
            d.path(),
            "criteria binary\nload l r\nexpand all\nselect all.diff.files\n\
             file-report layout:side-by-side options:display-mismatches output-to:out/f.txt\n\
             hex-report layout:summary output-to:out/h.txt\n\
             hex-report layout:interleaved output-to:out/i.html output-options:html-color\n",
            false,
        );
        assert_eq!(r.outcome.errors, 0, "{}{}", r.out, r.err);
        let f = read(d.path(), "out/f.txt");
        assert!(f.contains("TWO!") && f.contains("1 byte(s) differ in 1 range(s)"), "{f}");
        assert!(f.contains("*00000000  00 01 02 03"), "{f}");
        let h = read(d.path(), "out/h.txt");
        // Every pair is hex in a hex report, text files too.
        assert!(h.contains("1 byte(s) differ") && !h.contains("*00000000"), "{h}");
        assert!(read(d.path(), "out/i.html").contains("&gt; 00 01 02 FF"));
    }

    #[test]
    fn display_matches_and_printer_output() {
        let d = fixture();
        let r = run_in(
            d.path(),
            "load l/sub/changed.txt r/sub/changed.txt\n\
             text-report layout:side-by-side options:display-matches output-to:m.txt\n\
             text-report layout:side-by-side output-to:printer\n",
            false,
        );
        assert_eq!(r.outcome.errors, 1);
        assert!(r.err.contains("output-to:printer needs the app window"), "{}", r.err);
        let m = read(d.path(), "m.txt");
        assert!(m.contains("one") && !m.contains("TWO!"), "{m}");
    }
}
