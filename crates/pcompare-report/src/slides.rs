//! The HTML slideshow report: one self-contained page (inline CSS and
//! script, works offline) that shows one difference at a time with its
//! context lines. Prev/Next buttons, ←/→ (and Page Up/Down, Home/End) move
//! between slides; `#slide-N` opens on slide N. Without script, and when
//! printed, every slide is shown, one per page.

use crate::html::{self, escape, Style};
use crate::text::{
    self, block_kind, block_span, difference_blocks, side_by_side_html, summarize, summary_text,
    unified_html, Notes, TextBookmark, TextLayout, TextReportOptions, TextSection,
};
use std::fmt::Write;

const CSS: &str = r#"<style>
.ss-nav { position: sticky; top: 0; z-index: 1; display: flex; align-items: center; gap: 10px; padding: 6px 0; margin-bottom: 10px; background: #fff; border-bottom: 1px solid var(--border); }
.ss-nav button { font: inherit; padding: 3px 14px; border: 1px solid var(--border); border-radius: 4px; background: var(--panel); color: var(--text); cursor: pointer; }
.ss-nav button:disabled { opacity: 0.45; cursor: default; }
#ss-count { min-width: 5em; text-align: center; font-weight: 600; font-variant-numeric: tabular-nums; }
.ss-hint { margin-left: auto; color: var(--muted); font-size: 12px; }
html:not(.js) .ss-nav { display: none; }
html.js .slide { display: none; }
html.js .slide.on { display: block; }
.slide h2 { margin-top: 4px; }
.slide-meta { color: var(--muted); margin-bottom: 6px; }
@media print {
  .ss-nav { display: none; }
  html.js .slide { display: block; }
  .slide { break-after: page; }
  .slide:last-of-type { break-after: auto; }
}
html.pc-preview .ss-nav { display: none; }
html.pc-preview.js .slide { display: block; }
</style>
"#;

const SCRIPT: &str = r#"<script>
(function () {
  var slides = document.querySelectorAll(".slide"), at = 0;
  var prev = document.getElementById("ss-prev"), next = document.getElementById("ss-next"), count = document.getElementById("ss-count");
  if (!slides.length) return;
  document.documentElement.classList.add("js");
  function show(n) {
    at = Math.max(0, Math.min(slides.length - 1, n));
    for (var i = 0; i < slides.length; i++) slides[i].classList.toggle("on", i === at);
    count.textContent = (at + 1) + " / " + slides.length;
    prev.disabled = at === 0;
    next.disabled = at === slides.length - 1;
  }
  prev.onclick = function () { show(at - 1); };
  next.onclick = function () { show(at + 1); };
  document.addEventListener("keydown", function (e) {
    if (e.altKey || e.ctrlKey || e.metaKey) return;
    var to = { ArrowRight: at + 1, PageDown: at + 1, ArrowLeft: at - 1, PageUp: at - 1, Home: 0, End: slides.length - 1 }[e.key];
    if (to === undefined) return;
    e.preventDefault();
    show(to);
  });
  var m = /^#slide-(\d+)$/.exec(location.hash);
  show(m ? Number(m[1]) - 1 : 0);
})();
</script>
"#;

fn lines_label(side: &str, (start, count): (usize, usize)) -> String {
    match count {
        0 => format!("nothing on the {side} (after line {start})"),
        1 => format!("{side} line {start}"),
        n => format!("{side} lines {start}–{}", start + n - 1),
    }
}

fn kind_label(kind: &str) -> &'static str {
    match kind {
        "leftOnly" => "Removed",
        "rightOnly" => "Added",
        "unimportant" => "Unimportant change",
        _ => "Changed",
    }
}

pub(crate) fn render(
    sections: &[TextSection],
    opts: &TextReportOptions,
    bookmarks: &[TextBookmark],
    generated: &str,
) -> String {
    let title = text::title(opts);
    let blocks: Vec<_> = sections
        .iter()
        .map(|s| difference_blocks(&s.diff.rows, opts.ignore_unimportant))
        .collect();
    let total: usize = blocks.iter().map(Vec::len).sum();
    let mut body = format!("<h1>{}</h1>\n", escape(title));
    if !generated.is_empty() {
        let _ = writeln!(
            body,
            "<div class=\"meta\">Produced {} by PCompare</div>",
            escape(generated)
        );
    }
    if let [s] = sections {
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
    }
    body.push_str(
        "<nav class=\"ss-nav\"><button id=\"ss-prev\" title=\"Previous difference (←)\">◀ Prev</button>\
         <span id=\"ss-count\"></span>\
         <button id=\"ss-next\" title=\"Next difference (→)\">Next ▶</button>\
         <span class=\"ss-hint\">← → to move · Home / End</span></nav>\n",
    );
    // Slides repeat context rows, so their line numbers carry no anchors.
    let notes = Notes::new(bookmarks, None);
    let mut n = 0;
    for (s, blocks) in sections.iter().zip(&blocks) {
        let rows = &s.diff.rows;
        for block in blocks {
            n += 1;
            let shown = block.start.saturating_sub(opts.context)
                ..(block.end + opts.context).min(rows.len());
            let _ = writeln!(
                body,
                "<section class=\"slide\" id=\"slide-{n}\">\n<h2>Difference {n} of {total}</h2>"
            );
            let mut meta = String::new();
            if sections.len() > 1 {
                let name = if s.name.is_empty() { s.left } else { s.name };
                let _ = write!(meta, "{} · ", escape(name));
            }
            let _ = write!(
                meta,
                "{}: {}, {}",
                kind_label(block_kind(&rows[block.clone()])),
                lines_label("left", block_span(rows, block, |r| r.left_no)),
                lines_label("right", block_span(rows, block, |r| r.right_no)),
            );
            let _ = writeln!(body, "<div class=\"slide-meta\">{meta}</div>");
            let ranges = [shown];
            match opts.layout {
                TextLayout::SideBySide => {
                    side_by_side_html(&mut body, s.diff, &ranges, opts, &notes, false)
                }
                TextLayout::Unified => {
                    unified_html(&mut body, s.diff, &ranges, opts, &notes, false)
                }
            }
            body.push_str("</section>\n");
        }
    }
    if total == 0 {
        let what = match sections {
            [s] => summary_text(
                &summarize(s.diff, opts.ignore_unimportant),
                opts.ignore_unimportant,
            ),
            _ => "No differences".to_owned(),
        };
        let _ = writeln!(
            body,
            "<section class=\"slide\" id=\"slide-1\">\n<div class=\"note\">{}</div>\n</section>",
            escape(&what)
        );
    }
    body.push_str(SCRIPT);
    html::page_with(
        title,
        Style {
            mono: opts.mono,
            wrap: opts.wrap,
        },
        CSS,
        &body,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::{render, TextReportFormat};
    use pcompare_core::text::{diff_text, TextDiffOptions};

    #[test]
    fn one_slide_per_difference_with_context() {
        let left: String = (1..=30).map(|i| format!("line {i}\n")).collect();
        let right = left
            .replace("line 5\n", "line five\n")
            .replace("line 20\n", "")
            .replace("line 28\n", "line 28\nextra\n");
        let d = diff_text(&left, &right, &TextDiffOptions::default()).unwrap();
        let section = TextSection {
            name: "",
            left: "a.txt",
            right: "b.txt",
            diff: &d,
        };
        let opts = TextReportOptions {
            format: TextReportFormat::Slideshow,
            context: 2,
            ..Default::default()
        };
        let h = render(&[section], &opts, "");
        assert_eq!(h.matches("<section class=\"slide\"").count(), 3, "{h}");
        assert!(h.contains("<h2>Difference 3 of 3</h2>"));
        assert!(h.contains("Removed: left line 20, nothing on the right (after line 19)"));
        assert!(h.contains("Added: nothing on the left (after line 28), right line 28"));
        // Two context lines either side of the change on line 5, no separators.
        let first = &h[h.find("id=\"slide-1\"").unwrap()..h.find("id=\"slide-2\"").unwrap()];
        assert_eq!(first.matches("<tr class=").count(), 5, "{first}");
        assert!(!first.contains("class=\"gap\""));
        assert!(h.contains("id=\"ss-prev\"") && h.contains("ArrowRight"));
        assert!(!h.contains("<script src") && !h.contains("<link"));
    }

    #[test]
    fn identical_files_get_a_single_slide() {
        let d = diff_text("x\n", "x\n", &TextDiffOptions::default()).unwrap();
        let section = TextSection {
            name: "",
            left: "a",
            right: "b",
            diff: &d,
        };
        let opts = TextReportOptions {
            format: TextReportFormat::Slideshow,
            ..Default::default()
        };
        let h = render(&[section], &opts, "");
        assert_eq!(h.matches("<section class=\"slide\"").count(), 1);
        assert!(h.contains("The files are identical"));
    }
}
