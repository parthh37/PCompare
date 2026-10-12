//! Splitting text into lines and joining it back.

/// Splits `text` at every line break (`\r\n`, `\n` or a lone `\r`).
///
/// Returns the lines without their terminators plus whether the text ended
/// with a line break, so `join_lines(&split_lines(t).0, split_lines(t).1)`
/// reproduces `t` with all line breaks normalized to `\n`. Empty text has no
/// lines.
pub fn split_lines(text: &str) -> (Vec<String>, bool) {
    let mut lines = Vec::new();
    let mut rest = text;
    while let Some(pos) = rest.find(['\r', '\n']) {
        lines.push(rest[..pos].to_owned());
        let skip = if rest[pos..].starts_with("\r\n") { 2 } else { 1 };
        rest = &rest[pos + skip..];
    }
    let trailing_newline = rest.is_empty() && !lines.is_empty();
    if !rest.is_empty() {
        lines.push(rest.to_owned());
    }
    (lines, trailing_newline)
}

/// Joins lines with `\n`, appending a final `\n` when `trailing_newline` is
/// set and there is at least one line.
pub fn join_lines<S: AsRef<str>>(lines: &[S], trailing_newline: bool) -> String {
    let mut out = String::with_capacity(lines.iter().map(|l| l.as_ref().len() + 1).sum());
    for (i, line) in lines.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(line.as_ref());
    }
    if trailing_newline && !lines.is_empty() {
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_every_line_break_style() {
        assert_eq!(split_lines("a\r\nb\nc\rd"), (vec!["a".into(), "b".into(), "c".into(), "d".into()], false));
        assert_eq!(split_lines("a\n"), (vec!["a".into()], true));
        assert_eq!(split_lines("\n"), (vec![String::new()], true));
        assert_eq!(split_lines("a\n\n"), (vec!["a".into(), String::new()], true));
        assert_eq!(split_lines(""), (vec![], false));
    }

    #[test]
    fn join_reverses_split() {
        for text in ["", "a", "a\n", "a\nb", "a\n\nb\n", "\n"] {
            let (lines, trailing) = split_lines(text);
            assert_eq!(join_lines(&lines, trailing), text);
        }
        assert_eq!(join_lines::<&str>(&[], true), "");
    }
}
