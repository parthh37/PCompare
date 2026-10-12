//! Comparison reports for PCompare, in the spirit of Beyond Compare's
//! "Session → Report…": text compares as side-by-side or unified HTML, an
//! HTML slideshow (one difference per slide), plain text, a unified patch, a
//! classic `diff` listing or XML; folder compares as a summary or a detailed
//! side-by-side listing (HTML, text, CSV or XML, optionally linking — or for
//! XML embedding — a text report per differing file); table compares as an
//! HTML grid or CSV. HTML reports can carry bookmark comments.
//!
//! Renderers are pure functions from engine results to a `String`, so the
//! Tauri shell (print, clipboard, save) and the script processor share them
//! and tests can assert on the output directly.

pub mod folder;
mod html;
mod slides;
pub mod table;
pub mod text;
pub mod xml;

use chrono::{Local, TimeZone};

/// "Produced" stamp for report headers: local time, minutes resolution.
pub fn now_stamp() -> String {
    Local::now().format("%Y-%m-%d %H:%M").to_string()
}

/// A modification time (ms since the Unix epoch) as local `YYYY-MM-DD HH:MM:SS`.
pub fn format_time(ms: i64) -> String {
    match Local.timestamp_millis_opt(ms) {
        chrono::LocalResult::Single(t) | chrono::LocalResult::Ambiguous(t, _) => {
            t.format("%Y-%m-%d %H:%M:%S").to_string()
        }
        chrono::LocalResult::None => String::new(),
    }
}

/// A byte count with thousands separators, as Beyond Compare lists sizes.
pub fn format_size(bytes: u64) -> String {
    let digits = bytes.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// One CSV field, quoted when it holds a separator, quote or line break (RFC 4180).
pub(crate) fn csv_field(s: &str) -> String {
    if s.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_owned()
    }
}

/// A CSV record terminated with CRLF, which Excel and RFC 4180 expect.
pub(crate) fn csv_row<S: AsRef<str>>(fields: &[S]) -> String {
    let mut line = fields
        .iter()
        .map(|f| csv_field(f.as_ref()))
        .collect::<Vec<_>>()
        .join(",");
    line.push_str("\r\n");
    line
}

/// Characters a monospaced column needs for `s` (tabs expand to 4).
pub(crate) fn display_width(s: &str) -> usize {
    s.chars().map(|c| if c == '\t' { 4 } else { 1 }).sum()
}

/// `s` with tabs expanded to the next multiple of 4, for fixed-width text.
pub(crate) fn expand_tabs(s: &str) -> String {
    if !s.contains('\t') {
        return s.to_owned();
    }
    let mut out = String::with_capacity(s.len() + 8);
    let mut col = 0;
    for c in s.chars() {
        if c == '\t' {
            let n = 4 - col % 4;
            out.extend(std::iter::repeat_n(' ', n));
            col += n;
        } else {
            out.push(c);
            col += 1;
        }
    }
    out
}

/// Pads `s` with spaces to `width` display columns (never truncates).
pub(crate) fn pad(s: &str, width: usize) -> String {
    let w = display_width(s);
    if w >= width {
        s.to_owned()
    } else {
        format!("{s}{}", " ".repeat(width - w))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_get_thousands_separators() {
        assert_eq!(format_size(0), "0");
        assert_eq!(format_size(999), "999");
        assert_eq!(format_size(1000), "1,000");
        assert_eq!(format_size(1234567), "1,234,567");
    }

    #[test]
    fn csv_quotes_only_when_needed() {
        assert_eq!(
            csv_row(&["a", "b,c", "say \"hi\"", "x\ny"]),
            "a,\"b,c\",\"say \"\"hi\"\"\",\"x\ny\"\r\n"
        );
    }

    #[test]
    fn tabs_expand_to_four_columns() {
        assert_eq!(expand_tabs("a\tb"), "a   b");
        assert_eq!(expand_tabs("\tx"), "    x");
        assert_eq!(display_width("\tab"), 6);
        assert_eq!(pad("ab", 4), "ab  ");
    }
}
