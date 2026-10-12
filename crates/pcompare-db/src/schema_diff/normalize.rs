//! SQL text normalisation for comparisons.
//!
//! A small tokenizer that knows string literals, quoted identifiers and
//! comments, so whitespace collapsing, comment stripping and case folding
//! never touch text inside a literal (`'a -- b'` stays intact).

use std::collections::HashSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tok {
    /// Keyword, identifier or number (`select`, `@p1`, `$1`, `42`).
    Word,
    /// `'...'` string literal, quotes included.
    Str,
    /// `"..."`, `[...]` or `` `...` `` identifier, quotes included.
    Quoted,
    Comment,
    Space,
    /// Any other single character.
    Punct,
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '$' | '@' | '#')
}

/// Splits `sql` into tokens; concatenating the slices gives back `sql`.
pub(crate) fn tokenize(sql: &str) -> Vec<(Tok, &str)> {
    let mut out = Vec::new();
    let mut rest = sql;
    while let Some(c) = rest.chars().next() {
        let next = rest[c.len_utf8()..].chars().next();
        let (tok, len) = match c {
            '\'' => (Tok::Str, quoted_len(rest, '\'')),
            '"' | '`' => (Tok::Quoted, quoted_len(rest, c)),
            '[' => (Tok::Quoted, quoted_len(rest, ']')),
            '-' if next == Some('-') => (Tok::Comment, rest.find('\n').unwrap_or(rest.len())),
            '/' if next == Some('*') => (Tok::Comment, block_comment_len(rest)),
            c if c.is_whitespace() => (Tok::Space, run_len(rest, char::is_whitespace)),
            c if is_word_char(c) => (Tok::Word, run_len(rest, is_word_char)),
            c => (Tok::Punct, c.len_utf8()),
        };
        out.push((tok, &rest[..len]));
        rest = &rest[len..];
    }
    out
}

fn run_len(s: &str, f: impl Fn(char) -> bool) -> usize {
    s.find(|c: char| !f(c)).unwrap_or(s.len())
}

/// Length of a quoted token starting at `s[0]`, where a doubled closer is an
/// escape (`'it''s'`, `[a]]b]`). Unterminated tokens run to the end.
fn quoted_len(s: &str, close: char) -> usize {
    let mut chars = s.char_indices().skip(1).peekable();
    while let Some((i, c)) = chars.next() {
        if c == close {
            if chars.peek().map(|&(_, n)| n) == Some(close) {
                chars.next();
            } else {
                return i + c.len_utf8();
            }
        }
    }
    s.len()
}

/// `/* ... */`, nested as in SQL Server and PostgreSQL.
fn block_comment_len(s: &str) -> usize {
    let b = s.as_bytes();
    let (mut depth, mut i) = (0usize, 0usize);
    while i + 1 < b.len() {
        match (b[i], b[i + 1]) {
            (b'/', b'*') => {
                depth += 1;
                i += 2;
            }
            (b'*', b'/') => {
                depth -= 1;
                i += 2;
                if depth == 0 {
                    return i;
                }
            }
            _ => i += 1,
        }
    }
    s.len()
}

/// `"Foo"` / `[Foo]` / `` `Foo` `` → `Foo`.
pub(crate) fn unquote(tok: &str) -> String {
    let mut chars = tok.chars();
    let (Some(open), Some(_)) = (chars.next(), chars.next_back()) else { return tok.to_owned() };
    let close = if open == '[' { ']' } else { open };
    let inner = &tok[open.len_utf8()..tok.len() - close.len_utf8()];
    inner.replace(&format!("{close}{close}"), &close.to_string())
}

/// Joins tokens without whitespace except a single space between two words
/// — the canonical form for "ignore whitespace" comparisons.
fn join_compact<S: AsRef<str>>(toks: &[(Tok, S)]) -> String {
    let mut out = String::new();
    let mut prev_word = false;
    for (tok, text) in toks {
        let word = *tok == Tok::Word;
        if word && prev_word {
            out.push(' ');
        }
        out.push_str(text.as_ref());
        prev_word = word;
    }
    out
}

/// Canonical type name: lower-case, whitespace only between words
/// (`NUMERIC(10, 2)` == `numeric(10,2)`).
pub(crate) fn normalize_type(t: &str) -> String {
    let toks: Vec<(Tok, String)> = tokenize(t)
        .into_iter()
        .filter(|(k, _)| !matches!(k, Tok::Space | Tok::Comment))
        .map(|(k, s)| (k, if k == Tok::Str { s.to_owned() } else { s.to_lowercase() }))
        .collect();
    join_compact(&toks)
}

/// Canonical default/check/filter expression: comments and whitespace
/// dropped, words and quoted identifiers folded to bare lower case, and
/// redundant parentheses removed so `((0))` == `(0)` == `0` and
/// `([qty]>(0))` == `qty > 0`. String literals are kept verbatim.
pub(crate) fn normalize_expr(e: &str) -> String {
    let mut toks: Vec<(Tok, String)> = tokenize(e)
        .into_iter()
        .filter_map(|(k, s)| match k {
            Tok::Space | Tok::Comment => None,
            Tok::Str => Some((k, s.to_owned())),
            Tok::Quoted => Some((Tok::Word, unquote(s).to_lowercase())),
            _ => Some((k, s.to_lowercase())),
        })
        .collect();
    strip_atom_parens(&mut toks);
    while wrapped_in_parens(&toks) {
        toks.pop();
        toks.remove(0);
    }
    join_compact(&toks)
}

fn is_punct(t: &(Tok, String), c: &str) -> bool {
    t.0 == Tok::Punct && t.1 == c
}

/// True when the first `(` closes at the very last token.
fn wrapped_in_parens(toks: &[(Tok, String)]) -> bool {
    if toks.len() < 2 || !is_punct(&toks[0], "(") || !is_punct(&toks[toks.len() - 1], ")") {
        return false;
    }
    let mut depth = 0i32;
    for (i, t) in toks.iter().enumerate() {
        if is_punct(t, "(") {
            depth += 1;
        } else if is_punct(t, ")") {
            depth -= 1;
            if depth == 0 {
                return i == toks.len() - 1;
            }
        }
    }
    false
}

/// Removes parentheses around a single (optionally signed) atom that are
/// not a call's argument list: `>(0)` → `>0`, `((-1))` → `-1`, but `f(0)`
/// and `in (1)` keep theirs.
fn strip_atom_parens(toks: &mut Vec<(Tok, String)>) {
    let mut changed = true;
    while changed {
        changed = false;
        let mut i = 0;
        while i < toks.len() {
            if is_punct(&toks[i], "(") {
                let signed = toks.get(i + 1).is_some_and(|t| is_punct(t, "-") || is_punct(t, "+"));
                let atom = i + 1 + signed as usize;
                let close = atom + 1;
                let atom_ok = toks.get(atom).is_some_and(|t| matches!(t.0, Tok::Word | Tok::Str));
                let call = i > 0 && toks[i - 1].0 == Tok::Word;
                if atom_ok && !call && toks.get(close).is_some_and(|t| is_punct(t, ")")) {
                    toks.remove(close);
                    toks.remove(i);
                    changed = true;
                    continue;
                }
            }
            i += 1;
        }
    }
}

/// Canonical code-object definition for comparison.
///
/// - The leading `CREATE` / `CREATE OR REPLACE` / `CREATE OR ALTER` /
///   `ALTER` becomes `CREATE` (case-insensitively).
/// - `strip_comments` removes `--` and `/* */` comments.
/// - `collapse_ws` drops whitespace except a single space between words;
///   otherwise line endings and trailing spaces are normalised only.
/// - A trailing `;` is ignored.
pub(crate) fn normalize_definition(def: &str, strip_comments: bool, collapse_ws: bool) -> String {
    let mut toks: Vec<(Tok, &str)> = tokenize(def);
    if strip_comments {
        // A comment separates tokens like whitespace does.
        for t in toks.iter_mut().filter(|t| t.0 == Tok::Comment) {
            *t = (Tok::Space, " ");
        }
    }
    while toks.last().is_some_and(|t| t.0 == Tok::Space || (t.0 == Tok::Punct && t.1 == ";")) {
        toks.pop();
    }
    let start = toks.iter().position(|t| !matches!(t.0, Tok::Space | Tok::Comment)).unwrap_or(toks.len());
    let body_start = leading_keyword_end(&toks, start);
    let mut parts: Vec<(Tok, &str)> = Vec::with_capacity(toks.len());
    if let Some(end) = body_start {
        parts.push((Tok::Word, "CREATE"));
        parts.extend_from_slice(&toks[end..]);
    } else {
        parts.extend_from_slice(&toks[start..]);
    }
    // The object-type keyword after CREATE is case-insensitive too (`PROC` = `PROCEDURE`).
    let kind_kw: String;
    let kw_at = body_start.and(parts.iter().skip(1).position(|t| !matches!(t.0, Tok::Space | Tok::Comment)).map(|i| i + 1));
    if let Some(i) = kw_at.filter(|&i| parts[i].0 == Tok::Word) {
        kind_kw = match parts[i].1.to_uppercase().as_str() {
            "PROC" => "PROCEDURE".into(),
            upper => upper.into(),
        };
        parts[i].1 = &kind_kw;
    }
    if collapse_ws {
        let compact: Vec<(Tok, &str)> = parts.into_iter().filter(|t| t.0 != Tok::Space).collect();
        return join_compact(&compact);
    }
    let raw: String = parts.iter().map(|t| t.1).collect();
    raw.lines()
        .map(str::trim_end)
        .filter(|l| !(strip_comments && l.trim().is_empty()))
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_owned()
}

/// Index just past `CREATE [OR REPLACE | OR ALTER]` or `ALTER` starting at
/// `start`, or `None` when the definition doesn't begin with one.
fn leading_keyword_end(toks: &[(Tok, &str)], start: usize) -> Option<usize> {
    let word_at = |from: usize| -> Option<(usize, &str)> {
        let i = (from..toks.len()).find(|&i| !matches!(toks[i].0, Tok::Space | Tok::Comment))?;
        (toks[i].0 == Tok::Word).then(|| (i, toks[i].1))
    };
    let (i, first) = word_at(start)?;
    if first.eq_ignore_ascii_case("alter") {
        return Some(i + 1);
    }
    if !first.eq_ignore_ascii_case("create") {
        return None;
    }
    if let Some((j, _)) = word_at(i + 1).filter(|(_, w)| w.eq_ignore_ascii_case("or")) {
        if let Some((k, _)) =
            word_at(j + 1).filter(|(_, w)| w.eq_ignore_ascii_case("replace") || w.eq_ignore_ascii_case("alter"))
        {
            return Some(k + 1);
        }
    }
    Some(i + 1)
}

/// Lower-cased identifiers (words and quoted names) used in `sql`, for
/// best-effort dependency detection between code objects.
pub(crate) fn identifiers(sql: &str) -> HashSet<String> {
    tokenize(sql)
        .into_iter()
        .filter_map(|(k, s)| match k {
            Tok::Word => Some(s.to_lowercase()),
            Tok::Quoted => Some(unquote(s).to_lowercase()),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenize_round_trips_and_respects_literals() {
        let sql = "SELECT 'a -- b' AS [x]] y], \"q\" /* c /* n */ */ -- tail\nFROM t";
        let toks = tokenize(sql);
        assert_eq!(toks.iter().map(|t| t.1).collect::<String>(), sql);
        assert!(toks.contains(&(Tok::Str, "'a -- b'")));
        assert!(toks.contains(&(Tok::Quoted, "[x]] y]")));
        assert!(toks.contains(&(Tok::Comment, "/* c /* n */ */")));
        assert!(toks.contains(&(Tok::Comment, "-- tail")));
        assert_eq!(unquote("[x]] y]"), "x] y");
        assert_eq!(unquote("\"a\"\"b\""), "a\"b");
    }

    #[test]
    fn expressions_ignore_redundant_parens() {
        assert_eq!(normalize_expr("((0))"), "0");
        assert_eq!(normalize_expr("(0)"), normalize_expr("0"));
        assert_eq!(normalize_expr("((-1))"), "-1");
        assert_eq!(normalize_expr("([Qty]>(0))"), normalize_expr("qty > 0"));
        assert_eq!(normalize_expr("(getdate())"), "getdate()");
        assert_eq!(normalize_expr("f(0)"), "f(0)");
        assert_eq!(normalize_expr("(a) + (b)"), "a+b");
        assert_ne!(normalize_expr("(a + b) * c"), normalize_expr("a + b * c"));
        assert_eq!(normalize_expr("'A  B'"), "'A  B'");
        assert_ne!(normalize_expr("'A'"), normalize_expr("'a'"));
    }

    #[test]
    fn types_normalise_case_and_spacing() {
        assert_eq!(normalize_type("NVARCHAR( 50 )"), "nvarchar(50)");
        assert_eq!(normalize_type("numeric(10, 2)"), normalize_type("numeric(10,2)"));
        assert_eq!(normalize_type("int   unsigned"), "int unsigned");
    }

    #[test]
    fn definitions_normalise_keyword_whitespace_and_comments() {
        let a = "CREATE VIEW v AS\n  SELECT  a, '--keep' AS s -- note\n  FROM t;";
        let b = "create or replace VIEW v AS SELECT a, '--keep' AS s FROM t";
        assert_ne!(normalize_definition(a, false, true), normalize_definition(b, false, true));
        assert_eq!(normalize_definition(a, true, true), normalize_definition(b, true, true));
        // The literal's `--` survives comment stripping.
        assert!(normalize_definition(a, true, true).contains("'--keep'"));
        let c = "CREATE VIEW v AS SELECT a, '--drop' AS s FROM t";
        assert_ne!(normalize_definition(c, true, true), normalize_definition(b, true, true));
        assert_eq!(normalize_definition("ALTER PROCEDURE p AS SELECT 1", false, false), "CREATE PROCEDURE p AS SELECT 1");
        assert_eq!(
            normalize_definition("CREATE OR ALTER PROCEDURE p AS SELECT 1;\r\n", false, false),
            normalize_definition("create PROC p AS SELECT 1", false, false)
        );
        // Without collapsing, inner whitespace still matters.
        assert_ne!(normalize_definition("SELECT  1", false, false), normalize_definition("SELECT 1", false, false));
    }

    #[test]
    fn identifiers_include_quoted_names() {
        let ids = identifiers("SELECT * FROM [dbo].[Orders] o JOIN \"Lines\" l ON 'x' = 'v_fake'");
        assert!(ids.contains("orders") && ids.contains("lines") && ids.contains("dbo"));
        assert!(!ids.contains("v_fake"));
    }
}
