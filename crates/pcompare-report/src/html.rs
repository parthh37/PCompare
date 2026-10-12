//! The HTML page every report is wrapped in: a self-contained document (no
//! external assets) styled with the app's light-theme diff colours, which
//! prints the same way it looks on screen.

use std::fmt::Write;

/// Escapes text for element content and double-quoted attributes.
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// Look of a report page.
#[derive(Debug, Clone, Copy, Default)]
pub struct Style {
    /// Black and white: differences are marked with bold, underlines and
    /// marker columns instead of background colours.
    pub mono: bool,
    /// Long lines wrap instead of scrolling (always on when printing).
    pub wrap: bool,
}

/// The light theme's diff colours (see `src/styles.css`), opaque enough to
/// survive printing.
const CSS: &str = r#"
:root {
  --text: #1d2330; --muted: #5e697b; --border: #d3d9e2; --panel: #eceff4;
  --chg-bg: rgba(245, 158, 11, 0.16);
  --del-bg: rgba(239, 68, 68, 0.13); --del-mark: rgba(239, 68, 68, 0.34);
  --add-bg: rgba(34, 197, 94, 0.15); --add-mark: rgba(34, 197, 94, 0.36);
  --unimportant: #4a6fa5; --orphan: #8b3fd0; --different: #d0383b;
  --mono: "Cascadia Mono", "JetBrains Mono", Consolas, "SF Mono", Menlo, monospace;
}
* { -webkit-print-color-adjust: exact; print-color-adjust: exact; }
body { font: 13px "Segoe UI", system-ui, -apple-system, sans-serif; color: var(--text); background: #fff; margin: 18px; }
h1 { font-size: 17px; margin: 0 0 6px; }
h2 { font-size: 14px; margin: 22px 0 6px; }
.meta { color: var(--muted); margin-bottom: 10px; }
table.paths { border-collapse: collapse; margin-bottom: 8px; }
table.paths th { text-align: left; padding: 1px 10px 1px 0; color: var(--muted); font-weight: 600; }
table.paths td { padding: 1px 0; font-family: var(--mono); font-size: 12px; word-break: break-all; }
.stats { margin: 6px 0 12px; color: var(--muted); }
.stats b { color: var(--text); font-weight: 600; }
.note { color: var(--muted); font-style: italic; margin: 8px 0; }
table.grid { border-collapse: collapse; width: 100%; font-size: 12px; }
table.grid th { text-align: left; background: var(--panel); border: 1px solid var(--border); padding: 3px 6px; font-weight: 600; }
table.grid td { border: 1px solid var(--border); padding: 2px 6px; vertical-align: top; }
table.diff { border-collapse: collapse; width: 100%; table-layout: fixed; font: 12px/1.45 var(--mono); }
table.diff td { padding: 0 6px; vertical-align: top; white-space: pre; overflow: hidden; tab-size: 4; }
body.wrap table.diff td { white-space: pre-wrap; overflow-wrap: anywhere; }
table.diff td.ln { width: 4.5em; text-align: right; color: var(--muted); border-right: 1px solid var(--border); user-select: none; }
table.diff td.sign { width: 1.4em; text-align: center; color: var(--muted); user-select: none; }
table.diff td.mid { width: 1.6em; text-align: center; color: var(--muted); border-left: 1px solid var(--border); border-right: 1px solid var(--border); }
table.diff tr.gap td { background: var(--panel); color: var(--muted); text-align: center; font-family: system-ui, sans-serif; font-size: 11px; padding: 2px; border-block: 1px solid var(--border); }
tr.chg td.t { background: var(--chg-bg); }
tr.chg td.l .m, tr.del .m { background: var(--del-mark); border-radius: 2px; }
tr.chg td.r .m, tr.add .m { background: var(--add-mark); border-radius: 2px; }
tr.lo td.l, tr.del td.t { background: var(--del-bg); }
tr.ro td.r, tr.add td.t { background: var(--add-bg); }
tr.lo td.r, tr.ro td.l { background: repeating-linear-gradient(135deg, transparent 0 5px, rgba(0,0,0,0.045) 5px 10px); }
tr.uni td.t { color: var(--unimportant); }
tr.uni .m { text-decoration: underline dotted; }
.s-different { color: var(--different); }
.s-similar { color: var(--unimportant); }
.s-leftOnly, .s-rightOnly { color: var(--orphan); }
.s-typeMismatch, .s-error { color: var(--different); font-weight: 600; }
.s-ignored { color: var(--muted); }
td.num { text-align: right; white-space: nowrap; font-variant-numeric: tabular-nums; }
td.date { white-space: nowrap; }
td.st { text-align: center; white-space: nowrap; }
tr.dir td.name { font-weight: 600; }
td.absent { background: repeating-linear-gradient(135deg, transparent 0 5px, rgba(0,0,0,0.045) 5px 10px); }
td.cd { background: var(--chg-bg); }
td.cd .l { background: var(--del-mark); }
td.cd .r { background: var(--add-mark); }
td.cd.ign, tr.uni td.cd { background: none; color: var(--unimportant); }
td.cd.ign .l, td.cd.ign .r, tr.uni td.cd .l, tr.uni td.cd .r { background: none; text-decoration: underline dotted; }
tr.lo td.cell { background: var(--del-bg); }
tr.ro td.cell { background: var(--add-bg); }
th.key::after { content: " (key)"; font-weight: normal; color: var(--muted); }
th.ign, td.ign { color: var(--muted); }
a { color: inherit; }
body.mono tr.chg td.t, body.mono tr.lo td.l, body.mono tr.ro td.r, body.mono tr.del td.t, body.mono tr.add td.t,
body.mono td.cd, body.mono tr.lo td.cell, body.mono tr.ro td.cell { background: none; }
body.mono .m, body.mono td.cd .l, body.mono td.cd .r { background: none !important; font-weight: 700; text-decoration: underline; }
body.mono [class^="s-"], body.mono tr.uni td.t { color: inherit; }
body.mono td.cd .l::before { content: "< "; }
body.mono td.cd .r::before { content: "> "; }
@media print { body { margin: 0; } body table.diff td { white-space: pre-wrap; overflow-wrap: anywhere; } tr { break-inside: avoid; } }
"#;

/// Wraps `body` into a complete page titled `title`.
pub fn page(title: &str, style: Style, body: &str) -> String {
    let mut classes = Vec::new();
    if style.mono {
        classes.push("mono");
    }
    if style.wrap {
        classes.push("wrap");
    }
    let mut out = String::with_capacity(body.len() + CSS.len() + 400);
    let _ = write!(
        out,
        "<!DOCTYPE html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n\
         <meta name=\"generator\" content=\"PCompare\">\n<title>{}</title>\n<style>{CSS}</style>\n</head>\n\
         <body class=\"{}\">\n{body}</body>\n</html>\n",
        escape(title),
        classes.join(" "),
    );
    out
}

/// The `<h1>`, "produced" line and the left/right path table.
pub fn header(title: &str, generated: &str, left: &str, right: &str) -> String {
    let mut out = format!("<h1>{}</h1>\n", escape(title));
    if !generated.is_empty() {
        let _ = writeln!(
            out,
            "<div class=\"meta\">Produced {} by PCompare</div>",
            escape(generated)
        );
    }
    let _ = writeln!(
        out,
        "<table class=\"paths\"><tr><th>Left</th><td>{}</td></tr><tr><th>Right</th><td>{}</td></tr></table>",
        escape(left),
        escape(right)
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_markup_and_quotes() {
        assert_eq!(
            escape("<a href=\"x\">&'"),
            "&lt;a href=&quot;x&quot;&gt;&amp;&#39;"
        );
    }

    #[test]
    fn page_is_a_complete_document() {
        let p = page(
            "A <b>",
            Style {
                mono: true,
                wrap: false,
            },
            "<p>x</p>\n",
        );
        assert!(p.starts_with("<!DOCTYPE html>"));
        assert!(p.contains("<title>A &lt;b&gt;</title>"));
        assert!(p.contains("<body class=\"mono\">\n<p>x</p>\n</body>"));
        assert!(p.trim_end().ends_with("</html>"));
    }
}
