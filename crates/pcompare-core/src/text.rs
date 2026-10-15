//! Line-oriented text comparison producing aligned side-by-side rows.

use crate::grammar::{self, Grammar, LineToken, TokenKind};
pub use crate::numeric::NumericTolerance;
use crate::numeric::{self, TolerantPiece};
use crate::textfile::{split_lines_with_endings, LineEnding};
use crate::{CoreError, Result};
use regex::Regex;
use serde::{Deserialize, Serialize};
use similar::algorithms::{myers, Capture};
use similar::{capture_diff_slices, Algorithm, ChangeTag, DiffOp, TextDiff};
use std::borrow::Cow;

mod moves;
pub use moves::MovedBlock;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum WhitespaceMode {
    /// Every whitespace character is significant.
    #[default]
    Compare,
    /// Trailing whitespace is ignored.
    IgnoreTrailing,
    /// Runs of whitespace compare equal to a single space; ends are trimmed.
    IgnoreChanges,
    /// All whitespace is ignored.
    IgnoreAll,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DiffAlgorithm {
    /// Anchors on unique lines; produces the most readable diffs for code.
    #[default]
    Patience,
    /// Classic minimal edit script.
    Myers,
}

impl From<DiffAlgorithm> for Algorithm {
    fn from(a: DiffAlgorithm) -> Self {
        match a {
            DiffAlgorithm::Patience => Algorithm::Patience,
            DiffAlgorithm::Myers => Algorithm::Myers,
        }
    }
}

/// How lines inside a block of differences are paired up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Alignment {
    /// Pair the most similar lines; lines with no similar partner are shown
    /// on their own (Beyond Compare's "Normal").
    #[default]
    Similar,
    /// Pair lines by their position inside the block.
    Positional,
    /// Never pair changed lines: the block is shown side by side as a whole,
    /// without intra-line highlights.
    Unaligned,
}

/// A 1-based, inclusive character-column range; `end: None` runs to the end
/// of the line. Columns are counted in characters (a tab is one column).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ColumnRange {
    pub start: usize,
    pub end: Option<usize>,
}

/// A 1-based, inclusive line range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LineRange {
    pub start: usize,
    pub end: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SideRanges {
    pub left: Vec<LineRange>,
    pub right: Vec<LineRange>,
}

/// A left line and a right line, both 1-based.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LinePair {
    pub left: usize,
    pub right: usize,
}

/// "Ignore replaced text": `left` on the left side corresponds to `right` on
/// the right side (a renamed identifier, a changed prefix), so lines that
/// differ only by that replacement are unimportant. With `regex`, `left` is
/// a regular expression and `right` may refer to its groups (`$1`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Replacement {
    pub left: String,
    pub right: String,
    pub regex: bool,
}

/// A block expression: every line from one matching `begin` through the
/// next line matching `end` (both inclusive) is unimportant, such as an
/// auto-generated region. A nested `begin` of the same pair needs its own
/// `end`; a block that never ends runs to the end of the file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct BlockPattern {
    pub begin: String,
    pub end: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TextDiffOptions {
    pub whitespace: WhitespaceMode,
    pub ignore_case: bool,
    pub algorithm: DiffAlgorithm,
    /// Regular expressions for text that never matters (version stamps,
    /// generated timestamps, ...). Matches are removed before lines are
    /// compared, so lines differing only there show as unimportant.
    pub ignore_patterns: Vec<String>,

    /// Language grammar id (see [`crate::grammar::GRAMMARS`]); `None` or
    /// empty picks one per side from its file name, an unknown id such as
    /// `text` turns grammars off.
    pub grammar: Option<String>,
    /// Comments (per the grammar) are unimportant.
    pub ignore_comments: bool,
    /// The contents of string literals (per the grammar) are unimportant;
    /// the delimiters still count.
    pub ignore_strings: bool,
    /// Send each line's grammar tokens with the result (for highlighting).
    pub syntax_spans: bool,
    /// Character columns that never matter (COBOL sequence numbers in 1-6
    /// and 73-80, fixed-width report stamps).
    pub ignore_columns: Vec<ColumnRange>,
    /// Lines matching any of these regexes are page headers or footers and
    /// wholly unimportant.
    pub page_header_patterns: Vec<String>,
    /// The first N lines of every page (pages break at form feeds) are headers.
    pub page_header_lines: usize,
    /// The last N lines of every page are footers.
    pub page_footer_lines: usize,
    /// A different line terminator (CRLF / LF / CR) makes lines differ.
    /// Needs per-line terminators, see [`SideInput::eols`].
    pub compare_line_endings: bool,
    pub replacements: Vec<Replacement>,
    pub alignment: Alignment,
    /// Lines the user marked unimportant ("Mark as unimportant").
    pub unimportant_lines: SideRanges,
    /// Forced alignments: each left line is shown next to its right line.
    pub anchors: Vec<LinePair>,
    /// Forced separations: these lines are never shown side by side.
    pub breaks: Vec<LinePair>,
    /// Numbers compare by value, within a tolerance; lines differing only
    /// in such numbers are unimportant.
    pub numeric_tolerance: NumericTolerance,
    /// Whole blocks of lines that are unimportant.
    pub ignore_blocks: Vec<BlockPattern>,
}

impl TextDiffOptions {
    /// Page-header handling is on, which also makes form feeds unimportant.
    fn pages(&self) -> bool {
        self.page_header_lines > 0
            || self.page_footer_lines > 0
            || self
                .page_header_patterns
                .iter()
                .any(|p| !p.trim().is_empty())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum RowKind {
    Equal,
    /// Lines differ only in ways the current options consider unimportant.
    /// One side may be absent: a line that is entirely unimportant (only a
    /// comment, a page header, manually ignored) with no partner.
    Unimportant,
    Changed,
    LeftOnly,
    RightOnly,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Segment {
    pub text: String,
    pub changed: bool,
}

/// A grammar token of one line, for syntax highlighting.
///
/// Shape (JSON): `{ "start": 4, "end": 11, "kind": "comment" }` where
/// `start..end` are UTF-16 code-unit offsets into the line's full text (the
/// concatenated segment texts), so they index a JavaScript string directly;
/// `kind` is `comment`, `string`, `keyword` or `number`. Spans are sorted
/// and never overlap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct TokenSpan {
    pub start: u32,
    pub end: u32,
    pub kind: TokenKind,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffRow {
    pub kind: RowKind,
    /// 1-based line numbers; `None` on the side where the line is absent.
    pub left_no: Option<usize>,
    pub right_no: Option<usize>,
    pub left: Option<Vec<Segment>>,
    pub right: Option<Vec<Segment>>,
    /// Grammar tokens of each side's line; only sent with
    /// [`TextDiffOptions::syntax_spans`] (omitted when empty).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub left_spans: Vec<TokenSpan>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub right_spans: Vec<TokenSpan>,
    /// Each side's line terminator; only sent with
    /// [`TextDiffOptions::compare_line_endings`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub left_eol: Option<LineEnding>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub right_eol: Option<LineEnding>,
    /// The [`MovedBlock`] this side's line belongs to: a left-only line that
    /// reappears elsewhere on the right, or a right-only line that came from
    /// elsewhere on the left. The row's kind is unchanged.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub left_move: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub right_move: Option<u32>,
}

/// A contiguous block of rows that are neither `Equal` nor `Unimportant`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Hunk {
    pub start_row: usize,
    pub end_row: usize,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffStats {
    pub equal: usize,
    pub unimportant: usize,
    pub changed: usize,
    pub left_only: usize,
    pub right_only: usize,
    /// Lines in moved blocks (counted once per side; they are also in
    /// `left_only` / `right_only`).
    pub moved: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TextDiffResult {
    pub rows: Vec<DiffRow>,
    pub hunks: Vec<Hunk>,
    pub stats: DiffStats,
    pub identical: bool,
    /// Blocks that moved; their rows carry `left_move` / `right_move`.
    pub moves: Vec<MovedBlock>,
    /// The grammar id used for each side, if any.
    pub left_grammar: Option<&'static str>,
    pub right_grammar: Option<&'static str>,
}

fn compile(p: &str) -> Result<Regex> {
    Regex::new(p).map_err(|e| CoreError::BadPattern {
        pattern: p.to_owned(),
        message: e.to_string(),
    })
}

fn compile_all(patterns: &[String]) -> Result<Vec<Regex>> {
    patterns
        .iter()
        .map(|p| p.trim())
        .filter(|p| !p.is_empty())
        .map(compile)
        .collect()
}

enum Replacer {
    Literal(String, String),
    Regex(Regex, String),
}

/// Turns a line into the string that is actually compared. Built once per
/// diff so regexes are compiled a single time.
pub struct LineNormalizer {
    whitespace: WhitespaceMode,
    ignore_case: bool,
    ignore: Vec<Regex>,
    columns: Vec<ColumnRange>,
    headers: Vec<Regex>,
    replacements: Vec<Replacer>,
    strip_form_feeds: bool,
    blocks: Vec<(Regex, Regex)>,
}

impl LineNormalizer {
    pub fn new(opts: &TextDiffOptions) -> Result<Self> {
        let replacements = opts
            .replacements
            .iter()
            .filter(|r| !r.left.is_empty())
            .map(|r| {
                Ok(if r.regex {
                    Replacer::Regex(compile(&r.left)?, r.right.clone())
                } else {
                    Replacer::Literal(r.left.clone(), r.right.clone())
                })
            })
            .collect::<Result<_>>()?;
        let blocks = opts
            .ignore_blocks
            .iter()
            .filter(|b| !b.begin.trim().is_empty() && !b.end.trim().is_empty())
            .map(|b| Ok((compile(b.begin.trim())?, compile(b.end.trim())?)))
            .collect::<Result<_>>()?;
        Ok(Self {
            whitespace: opts.whitespace,
            ignore_case: opts.ignore_case,
            ignore: compile_all(&opts.ignore_patterns)?,
            columns: opts
                .ignore_columns
                .iter()
                .copied()
                .filter(|c| c.start > 0)
                .collect(),
            headers: compile_all(&opts.page_header_patterns)?,
            replacements,
            strip_form_feeds: opts.pages(),
            blocks,
        })
    }

    fn in_ignored_column(&self, col: usize) -> bool {
        self.columns
            .iter()
            .any(|c| col >= c.start && c.end.is_none_or(|end| col <= end))
    }

    /// The comparison key of a single line: ignored columns and regex
    /// matches removed, whitespace and case folded. Grammar-based and
    /// side-specific rules need the whole document; see [`diff_sides`].
    pub fn key(&self, line: &str) -> String {
        self.key_parts(line, &[], false).0
    }

    /// Builds a key, also dropping the byte ranges in `skip` (sorted, from
    /// the grammar) and applying replacements when `replace` is set. Returns
    /// whether any text was dropped as unimportant.
    fn key_parts(
        &self,
        line: &str,
        skip: &[(usize, usize, bool)],
        replace: bool,
    ) -> (String, bool) {
        let mut removed = false;
        let mut stripped = Cow::Borrowed(line);
        if !self.columns.is_empty()
            || !skip.is_empty()
            || (self.strip_form_feeds && line.contains('\x0c'))
        {
            let mut kept = String::with_capacity(line.len());
            let mut next = 0;
            for (col, (pos, c)) in line.char_indices().enumerate() {
                while next < skip.len() && skip[next].1 <= pos {
                    next += 1;
                }
                if let Some(&(start, _, comment)) = skip.get(next).filter(|s| s.0 <= pos) {
                    // A dropped comment takes the whitespace before it along,
                    // so `x; // a` matches `x;`.
                    if comment && pos == start {
                        kept.truncate(kept.trim_end().len());
                    }
                    removed = true;
                    continue;
                }
                if self.in_ignored_column(col + 1) || (self.strip_form_feeds && c == '\x0c') {
                    removed = true;
                    continue;
                }
                kept.push(c);
            }
            stripped = Cow::Owned(kept);
        }
        if replace {
            for r in &self.replacements {
                let next = match r {
                    Replacer::Literal(from, to) => {
                        if stripped.contains(from.as_str()) {
                            Some(stripped.replace(from.as_str(), to))
                        } else {
                            None
                        }
                    }
                    Replacer::Regex(re, to) => match re.replace_all(&stripped, to.as_str()) {
                        Cow::Owned(s) => Some(s),
                        Cow::Borrowed(_) => None,
                    },
                };
                if let Some(s) = next {
                    stripped = Cow::Owned(s);
                }
            }
        }
        for re in &self.ignore {
            if let Cow::Owned(s) = re.replace_all(&stripped, "") {
                stripped = Cow::Owned(s);
                removed = true;
            }
        }
        let key = match self.whitespace {
            WhitespaceMode::Compare => stripped.into_owned(),
            WhitespaceMode::IgnoreTrailing => stripped.trim_end().to_owned(),
            WhitespaceMode::IgnoreChanges => {
                stripped.split_whitespace().collect::<Vec<_>>().join(" ")
            }
            WhitespaceMode::IgnoreAll => stripped.chars().filter(|c| !c.is_whitespace()).collect(),
        };
        let key = if self.ignore_case {
            key.to_lowercase()
        } else {
            key
        };
        (key, removed)
    }
}

fn whole(line: &str, changed: bool) -> Vec<Segment> {
    vec![Segment {
        text: line.to_owned(),
        changed,
    }]
}

fn push_segment(out: &mut Vec<Segment>, text: &str, changed: bool) {
    match out.last_mut() {
        Some(last) if last.changed == changed => last.text.push_str(text),
        _ => out.push(Segment {
            text: text.to_owned(),
            changed,
        }),
    }
}

/// [`inline_diff`] where numbers equal within `tol` are not highlighted.
/// Lines are split finer than by words (numbers, words, whitespace runs and
/// single symbols), so `x=1.5` and `x=2.5` mark just the number.
pub fn inline_diff_tolerant(
    left: &str,
    right: &str,
    tol: &NumericTolerance,
) -> (Vec<Segment>, Vec<Segment>) {
    let wrap = |line| {
        numeric::pieces(line)
            .into_iter()
            .map(|piece| TolerantPiece { piece, tol })
            .collect::<Vec<_>>()
    };
    let (a, b) = (wrap(left), wrap(right));
    let mut capture = Capture::new();
    let Ok(()) = myers::diff(&mut capture, &a, 0..a.len(), &b, 0..b.len());
    let (mut l, mut r) = (Vec::new(), Vec::new());
    for op in capture.into_ops() {
        let (_, old, new) = op.as_tag_tuple();
        let changed = !matches!(op, DiffOp::Equal { .. });
        for p in &a[old] {
            push_segment(&mut l, p.piece.text, changed);
        }
        for p in &b[new] {
            push_segment(&mut r, p.piece.text, changed);
        }
    }
    (l, r)
}

/// Word-level diff inside a pair of changed lines.
pub fn inline_diff(left: &str, right: &str) -> (Vec<Segment>, Vec<Segment>) {
    let diff = TextDiff::configure()
        .algorithm(Algorithm::Myers)
        .diff_words(left, right);
    let (mut l, mut r) = (Vec::new(), Vec::new());
    for change in diff.iter_all_changes() {
        let text = change.value();
        match change.tag() {
            ChangeTag::Equal => {
                push_segment(&mut l, text, false);
                push_segment(&mut r, text, false);
            }
            ChangeTag::Delete => push_segment(&mut l, text, true),
            ChangeTag::Insert => push_segment(&mut r, text, true),
        }
    }
    (l, r)
}

/// One side of a comparison.
#[derive(Debug, Clone, Copy, Default)]
pub struct SideInput<'a> {
    pub lines: &'a [&'a str],
    /// Each line's terminator (see [`crate::textfile::split_lines_with_endings`]);
    /// needed only for [`TextDiffOptions::compare_line_endings`].
    pub eols: Option<&'a [LineEnding]>,
    /// File name or path, used to pick the grammar.
    pub path: Option<&'a str>,
}

/// A side after normalization.
struct Prepared {
    keys: Vec<String>,
    /// The line has nothing important in it, so it is unimportant even
    /// without a partner on the other side.
    ignorable: Vec<bool>,
    spans: Vec<Vec<TokenSpan>>,
    grammar: Option<&'static Grammar>,
}

fn in_ranges(ranges: &[LineRange], line_no: usize) -> bool {
    ranges
        .iter()
        .any(|r| r.start <= line_no && line_no <= r.end)
}

/// Page-header and footer lines: regex matches plus the first / last N lines
/// of every page, where a line containing a form feed starts a new page.
fn page_lines(lines: &[&str], norm: &LineNormalizer, opts: &TextDiffOptions) -> Vec<bool> {
    let mut out: Vec<bool> = lines
        .iter()
        .map(|l| norm.headers.iter().any(|re| re.is_match(l)))
        .collect();
    if opts.page_header_lines == 0 && opts.page_footer_lines == 0 {
        return out;
    }
    let mut starts: Vec<usize> = std::iter::once(0)
        .chain((1..lines.len()).filter(|&i| lines[i].contains('\x0c')))
        .collect();
    starts.push(lines.len());
    for page in starts.windows(2) {
        let (start, end) = (page[0], page[1]);
        let head_end = (start + opts.page_header_lines).min(end);
        let foot_start = end.saturating_sub(opts.page_footer_lines).max(start);
        out[start..head_end].iter_mut().for_each(|b| *b = true);
        out[foot_start..end].iter_mut().for_each(|b| *b = true);
    }
    out
}

/// Lines inside block expressions (see [`BlockPattern`]).
fn block_lines(lines: &[&str], norm: &LineNormalizer) -> Vec<bool> {
    let mut out = vec![false; lines.len()];
    if norm.blocks.is_empty() {
        return out;
    }
    // The pair that opened the current block and its nesting depth.
    let mut open: Option<(usize, usize)> = None;
    for (i, line) in lines.iter().enumerate() {
        let inside = open.is_some();
        open = match open {
            None => norm
                .blocks
                .iter()
                .position(|(begin, _)| begin.is_match(line))
                .map(|k| (k, 1)),
            Some((k, depth)) => {
                let (begin, end) = &norm.blocks[k];
                // `end` first, so a pair whose begin and end are the same
                // fence still closes.
                if end.is_match(line) {
                    (depth > 1).then(|| (k, depth - 1))
                } else if begin.is_match(line) {
                    Some((k, depth + 1))
                } else {
                    Some((k, depth))
                }
            }
        };
        // Both the opening and the closing line belong to the block.
        out[i] = inside || open.is_some();
    }
    out
}

/// Converts a line's byte-offset tokens to UTF-16 offsets.
fn utf16_spans(line: &str, tokens: &[LineToken]) -> Vec<TokenSpan> {
    let mut out = Vec::with_capacity(tokens.len());
    let (mut byte, mut unit) = (0usize, 0u32);
    let mut chars = line.chars();
    let mut advance = |to: usize| {
        while byte < to {
            match chars.next() {
                Some(c) => {
                    byte += c.len_utf8();
                    unit += c.len_utf16() as u32;
                }
                None => break,
            }
        }
        unit
    };
    for t in tokens {
        let start = advance(t.start);
        let end = advance(t.end);
        out.push(TokenSpan {
            start,
            end,
            kind: t.kind,
        });
    }
    out
}

fn eol_tag(eol: LineEnding) -> &'static str {
    match eol {
        LineEnding::Lf => "\0lf",
        LineEnding::Crlf => "\0crlf",
        LineEnding::Cr => "\0cr",
        LineEnding::Mixed | LineEnding::None => "\0",
    }
}

/// Normalizes one side. `replace` applies the replacements (they rewrite
/// left-side text into its right-side form); `manual` are the side's
/// manually unimportant lines.
fn prepare(
    side: &SideInput,
    norm: &LineNormalizer,
    opts: &TextDiffOptions,
    replace: bool,
    manual: &[LineRange],
) -> Prepared {
    let grammar = match opts.grammar.as_deref().map(str::trim) {
        Some(id) if !id.is_empty() => grammar::by_id(id),
        _ => side.path.and_then(grammar::for_path),
    };
    let want_tokens =
        grammar.is_some() && (opts.ignore_comments || opts.ignore_strings || opts.syntax_spans);
    let tokens = match grammar {
        Some(g) if want_tokens => g.tokenize(side.lines, opts.syntax_spans),
        _ => Vec::new(),
    };
    let pages = page_lines(side.lines, norm, opts);
    let blocks = block_lines(side.lines, norm);
    let mut keys = Vec::with_capacity(side.lines.len());
    let mut ignorable = Vec::with_capacity(side.lines.len());
    for (i, line) in side.lines.iter().enumerate() {
        let (mut key, unimportant) = if pages[i] || blocks[i] || in_ranges(manual, i + 1) {
            (String::new(), true)
        } else {
            let skip: Vec<(usize, usize, bool)> = tokens
                .get(i)
                .map(|toks| {
                    toks.iter()
                        .filter_map(|t| match t.kind {
                            TokenKind::Comment if opts.ignore_comments => {
                                Some((t.start, t.end, true))
                            }
                            TokenKind::String
                                if opts.ignore_strings && t.inner_end > t.inner_start =>
                            {
                                Some((t.inner_start, t.inner_end, false))
                            }
                            _ => None,
                        })
                        .collect()
                })
                .unwrap_or_default();
            let (key, removed) = norm.key_parts(line, &skip, replace);
            let blank = key.chars().all(char::is_whitespace);
            (key, removed && blank)
        };
        if opts.compare_line_endings {
            if let Some(eol) = side.eols.and_then(|e| e.get(i)) {
                key.push_str(eol_tag(*eol));
            }
        }
        keys.push(key);
        ignorable.push(unimportant);
    }
    let spans = if opts.syntax_spans {
        tokens
            .iter()
            .zip(side.lines)
            .map(|(t, l)| utf16_spans(l, t))
            .collect()
    } else {
        Vec::new()
    };
    Prepared {
        keys,
        ignorable,
        spans,
        grammar,
    }
}

/// How one row of the output is made up, before classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Item {
    /// Two lines shown side by side; `inline` asks for intra-line highlights.
    Pair {
        l: usize,
        r: usize,
        inline: bool,
    },
    Left(usize),
    Right(usize),
}

/// Blocks with more line pairs than this are paired by position: similarity
/// alignment is quadratic in the block size.
const MAX_SIMILARITY_CELLS: usize = 250_000;
/// Lines less similar than this are not paired.
const MIN_SIMILARITY: f32 = 0.5;

/// Character-bigram multiset of a line, sorted, for [`similarity`].
fn bigrams(s: &str) -> Vec<(char, char)> {
    let chars: Vec<char> = s.chars().filter(|c| !c.is_whitespace()).collect();
    let mut out: Vec<(char, char)> = chars.windows(2).map(|w| (w[0], w[1])).collect();
    if out.is_empty() {
        if let Some(&c) = chars.first() {
            out.push((c, '\0'));
        }
    }
    out.sort_unstable();
    out
}

/// Dice coefficient of two bigram multisets: 1.0 for identical lines, 0.0
/// for lines sharing nothing.
fn similarity(a: &[(char, char)], b: &[(char, char)]) -> f32 {
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    let (mut i, mut j, mut common) = (0, 0, 0usize);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                common += 1;
                i += 1;
                j += 1;
            }
        }
    }
    2.0 * common as f32 / (a.len() + b.len()) as f32
}

/// Pairs the lines of a replace block: the order-preserving set of pairs
/// with the highest total similarity, ignoring pairs below
/// [`MIN_SIMILARITY`]. Unpaired lines become one-sided items.
/// With a numeric tolerance, lines equal within it count as identical.
fn align_similar(
    lk: &[String],
    rk: &[String],
    l0: usize,
    r0: usize,
    tol: Option<&NumericTolerance>,
    out: &mut Vec<Item>,
) {
    let (n, m) = (lk.len(), rk.len());
    let positional = |out: &mut Vec<Item>| {
        let paired = n.min(m);
        out.extend((0..paired).map(|k| Item::Pair {
            l: l0 + k,
            r: r0 + k,
            inline: true,
        }));
        out.extend((paired..n).map(|k| Item::Left(l0 + k)));
        out.extend((paired..m).map(|k| Item::Right(r0 + k)));
    };
    if n.saturating_mul(m) > MAX_SIMILARITY_CELLS || n == 0 || m == 0 {
        return positional(out);
    }
    let lb: Vec<_> = lk.iter().map(|s| bigrams(s)).collect();
    let rb: Vec<_> = rk.iter().map(|s| bigrams(s)).collect();
    let score: Vec<f32> = lb
        .iter()
        .enumerate()
        .flat_map(|(i, a)| {
            rb.iter().enumerate().map(move |(j, b)| match tol {
                Some(t) if numeric::lines_equal(&lk[i], &rk[j], t) => 1.0,
                _ => similarity(a, b),
            })
        })
        .collect();
    // best[(i, j)]: best total over the first i left and j right lines.
    let w = m + 1;
    let mut best = vec![0f32; (n + 1) * w];
    for i in 1..=n {
        for j in 1..=m {
            let s = score[(i - 1) * m + (j - 1)];
            let mut v = best[(i - 1) * w + j].max(best[i * w + j - 1]);
            if s >= MIN_SIMILARITY {
                v = v.max(best[(i - 1) * w + j - 1] + s);
            }
            best[i * w + j] = v;
        }
    }
    let mut pairs = Vec::new();
    let (mut i, mut j) = (n, m);
    while i > 0 && j > 0 {
        let s = score[(i - 1) * m + (j - 1)];
        let here = best[i * w + j];
        if s >= MIN_SIMILARITY && here == best[(i - 1) * w + j - 1] + s {
            pairs.push((i - 1, j - 1));
            i -= 1;
            j -= 1;
        } else if here == best[(i - 1) * w + j] {
            i -= 1;
        } else {
            j -= 1;
        }
    }
    if pairs.is_empty() {
        // Nothing alike: show the block as is rather than as two orphan runs.
        return positional(out);
    }
    pairs.reverse();
    let (mut i, mut j) = (0, 0);
    for (pi, pj) in pairs.into_iter().chain(std::iter::once((n, m))) {
        out.extend((i..pi).map(|k| Item::Left(l0 + k)));
        out.extend((j..pj).map(|k| Item::Right(r0 + k)));
        if pi < n && pj < m {
            out.push(Item::Pair {
                l: l0 + pi,
                r: r0 + pj,
                inline: true,
            });
        }
        (i, j) = (pi + 1, pj + 1);
    }
}

/// Diffs `left[lr] / right[rr]` and appends the rows' items.
fn align_range(
    lk: &[String],
    rk: &[String],
    lr: std::ops::Range<usize>,
    rr: std::ops::Range<usize>,
    opts: &TextDiffOptions,
    out: &mut Vec<Item>,
) {
    let (l0, r0) = (lr.start, rr.start);
    let ops = capture_diff_slices(opts.algorithm.into(), &lk[lr], &rk[rr]);
    for op in ops {
        match op {
            DiffOp::Equal {
                old_index,
                new_index,
                len,
            } => out.extend((0..len).map(|k| Item::Pair {
                l: l0 + old_index + k,
                r: r0 + new_index + k,
                inline: true,
            })),
            DiffOp::Delete {
                old_index, old_len, ..
            } => out.extend((old_index..old_index + old_len).map(|i| Item::Left(l0 + i))),
            DiffOp::Insert {
                new_index, new_len, ..
            } => out.extend((new_index..new_index + new_len).map(|j| Item::Right(r0 + j))),
            DiffOp::Replace {
                old_index,
                old_len,
                new_index,
                new_len,
            } => {
                let (li, rj) = (l0 + old_index, r0 + new_index);
                match opts.alignment {
                    Alignment::Similar => align_similar(
                        &lk[li..li + old_len],
                        &rk[rj..rj + new_len],
                        li,
                        rj,
                        opts.numeric_tolerance
                            .enabled
                            .then_some(&opts.numeric_tolerance),
                        out,
                    ),
                    Alignment::Positional | Alignment::Unaligned => {
                        let paired = old_len.min(new_len);
                        let inline = opts.alignment == Alignment::Positional;
                        out.extend((0..paired).map(|k| Item::Pair {
                            l: li + k,
                            r: rj + k,
                            inline,
                        }));
                        out.extend((paired..old_len).map(|k| Item::Left(li + k)));
                        out.extend((paired..new_len).map(|k| Item::Right(rj + k)));
                    }
                }
            }
        }
    }
}

/// Aligns both sides, honoring manual anchors (diffing the stretches
/// between them separately) and alignment breaks.
fn align(lk: &[String], rk: &[String], opts: &TextDiffOptions) -> Vec<Item> {
    let mut anchors: Vec<(usize, usize)> = opts
        .anchors
        .iter()
        .filter(|a| (1..=lk.len()).contains(&a.left) && (1..=rk.len()).contains(&a.right))
        .map(|a| (a.left - 1, a.right - 1))
        .collect();
    anchors.sort_unstable();
    // Anchors that would cross an earlier one are dropped.
    let mut kept: Vec<(usize, usize)> = Vec::with_capacity(anchors.len());
    for a in anchors {
        if kept.last().is_none_or(|k| a.0 > k.0 && a.1 > k.1) {
            kept.push(a);
        }
    }
    let mut items = Vec::with_capacity(lk.len().max(rk.len()));
    let (mut l, mut r) = (0, 0);
    for (al, ar) in kept {
        align_range(lk, rk, l..al, r..ar, opts, &mut items);
        items.push(Item::Pair {
            l: al,
            r: ar,
            inline: true,
        });
        (l, r) = (al + 1, ar + 1);
    }
    align_range(lk, rk, l..lk.len(), r..rk.len(), opts, &mut items);

    if opts.breaks.is_empty() {
        return items;
    }
    let breaks: std::collections::HashSet<(usize, usize)> = opts
        .breaks
        .iter()
        .map(|b| (b.left.wrapping_sub(1), b.right.wrapping_sub(1)))
        .collect();
    items
        .into_iter()
        .flat_map(|item| match item {
            Item::Pair { l, r, .. } if breaks.contains(&(l, r)) => {
                vec![Item::Left(l), Item::Right(r)]
            }
            other => vec![other],
        })
        .collect()
}

/// A document's role, for the rules that differ per side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeySide {
    Left,
    Right,
    /// A merge's common ancestor. Replacements apply to it as to the left
    /// side, so text the right side rewrote per a replacement counts as
    /// unchanged against both; no manually unimportant lines.
    Base,
}

/// The comparison key of every line of `side` under the full rule set, as
/// [`diff_sides`] compares them: grammar comment/string importance, ignored
/// columns, regexes, page headers, blocks, replacements, whitespace and
/// case. Lines that are wholly unimportant get an empty key. Without a
/// `path` the grammar comes from [`TextDiffOptions::grammar`] only.
pub fn line_keys(side: &SideInput, norm: &LineNormalizer, opts: &TextDiffOptions, role: KeySide) -> Vec<String> {
    let opts = TextDiffOptions {
        syntax_spans: false,
        ..opts.clone()
    };
    let (replace, manual): (bool, &[LineRange]) = match role {
        KeySide::Left => (true, &opts.unimportant_lines.left),
        KeySide::Right => (false, &opts.unimportant_lines.right),
        KeySide::Base => (true, &[]),
    };
    prepare(side, norm, &opts, replace, manual).keys
}

/// Compares two documents. This is the full engine: grammars, page headers,
/// replacements, manual unimportant lines, line terminators and manual
/// alignment all need whole documents (and file names), not single lines.
pub fn diff_sides(
    left: SideInput,
    right: SideInput,
    opts: &TextDiffOptions,
) -> Result<TextDiffResult> {
    let norm = LineNormalizer::new(opts)?;
    let lp = prepare(&left, &norm, opts, true, &opts.unimportant_lines.left);
    let rp = prepare(&right, &norm, opts, false, &opts.unimportant_lines.right);
    let items = align(&lp.keys, &rp.keys, opts);
    let (ltext, rtext) = (left.lines, right.lines);

    let eol = |side: &SideInput, i: usize| {
        if opts.compare_line_endings {
            side.eols.and_then(|e| e.get(i).copied())
        } else {
            None
        }
    };
    let spans = |p: &Prepared, i: usize| p.spans.get(i).cloned().unwrap_or_default();
    let tol = opts
        .numeric_tolerance
        .enabled
        .then_some(&opts.numeric_tolerance);
    let inline_pair = |a: &str, b: &str| match tol {
        Some(t) => inline_diff_tolerant(a, b, t),
        None => inline_diff(a, b),
    };

    let mut rows = Vec::with_capacity(items.len());
    let mut stats = DiffStats::default();
    for item in items {
        let (l, r) = match item {
            Item::Pair { l, r, .. } => (Some(l), Some(r)),
            Item::Left(l) => (Some(l), None),
            Item::Right(r) => (None, Some(r)),
        };
        let (kind, lseg, rseg) = match item {
            Item::Pair { l, r, inline } => {
                let same = lp.keys[l] == rp.keys[r]
                    || tol.is_some_and(|t| numeric::lines_equal(&lp.keys[l], &rp.keys[r], t));
                if same {
                    if ltext[l] == rtext[r] {
                        stats.equal += 1;
                        (
                            RowKind::Equal,
                            whole(ltext[l], false),
                            whole(rtext[r], false),
                        )
                    } else {
                        stats.unimportant += 1;
                        let (a, b) = inline_pair(ltext[l], rtext[r]);
                        (RowKind::Unimportant, a, b)
                    }
                } else {
                    stats.changed += 1;
                    let (a, b) = if inline {
                        inline_pair(ltext[l], rtext[r])
                    } else {
                        (whole(ltext[l], true), whole(rtext[r], true))
                    };
                    (RowKind::Changed, a, b)
                }
            }
            Item::Left(l) if lp.ignorable[l] => {
                stats.unimportant += 1;
                (RowKind::Unimportant, whole(ltext[l], false), Vec::new())
            }
            Item::Left(l) => {
                stats.left_only += 1;
                (RowKind::LeftOnly, whole(ltext[l], true), Vec::new())
            }
            Item::Right(r) if rp.ignorable[r] => {
                stats.unimportant += 1;
                (RowKind::Unimportant, Vec::new(), whole(rtext[r], false))
            }
            Item::Right(r) => {
                stats.right_only += 1;
                (RowKind::RightOnly, Vec::new(), whole(rtext[r], true))
            }
        };
        rows.push(DiffRow {
            kind,
            left_no: l.map(|i| i + 1),
            right_no: r.map(|j| j + 1),
            left: l.map(|_| lseg),
            right: r.map(|_| rseg),
            left_spans: l.map(|i| spans(&lp, i)).unwrap_or_default(),
            right_spans: r.map(|j| spans(&rp, j)).unwrap_or_default(),
            left_eol: l.and_then(|i| eol(&left, i)),
            right_eol: r.and_then(|j| eol(&right, j)),
            left_move: None,
            right_move: None,
        });
    }

    let moves = moves::detect(&mut rows, &lp.keys, &rp.keys, ltext);
    stats.moved = moves.iter().map(|m| m.len).sum();
    let hunks = collect_hunks(&rows);
    Ok(TextDiffResult {
        identical: hunks.is_empty() && stats.unimportant == 0,
        rows,
        hunks,
        moves,
        stats,
        left_grammar: lp.grammar.map(|g| g.id),
        right_grammar: rp.grammar.map(|g| g.id),
    })
}

/// Compares two line lists (no file names, no terminators).
pub fn diff_lines(left: &[&str], right: &[&str], opts: &TextDiffOptions) -> Result<TextDiffResult> {
    diff_sides(
        SideInput {
            lines: left,
            ..Default::default()
        },
        SideInput {
            lines: right,
            ..Default::default()
        },
        opts,
    )
}

fn collect_hunks(rows: &[DiffRow]) -> Vec<Hunk> {
    let mut hunks = Vec::new();
    let mut start = None;
    for (idx, row) in rows.iter().enumerate() {
        let important = !matches!(row.kind, RowKind::Equal | RowKind::Unimportant);
        match (important, start) {
            (true, None) => start = Some(idx),
            (false, Some(s)) => {
                hunks.push(Hunk {
                    start_row: s,
                    end_row: idx,
                });
                start = None;
            }
            _ => {}
        }
    }
    if let Some(s) = start {
        hunks.push(Hunk {
            start_row: s,
            end_row: rows.len(),
        });
    }
    hunks
}

/// Compares two whole texts; lines break at `\r\n`, `\n` and `\r`.
pub fn diff_text(left: &str, right: &str, opts: &TextDiffOptions) -> Result<TextDiffResult> {
    diff_named_text(left, None, right, None, opts)
}

/// [`diff_text`] with file names, so grammars can be picked per side.
pub fn diff_named_text(
    left: &str,
    left_name: Option<&str>,
    right: &str,
    right_name: Option<&str>,
    opts: &TextDiffOptions,
) -> Result<TextDiffResult> {
    let (l, le) = split_lines_with_endings(left);
    let (r, re) = split_lines_with_endings(right);
    diff_sides(
        SideInput {
            lines: &l,
            eols: Some(&le),
            path: left_name,
        },
        SideInput {
            lines: &r,
            eols: Some(&re),
            path: right_name,
        },
        opts,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(res: &TextDiffResult) -> Vec<RowKind> {
        res.rows.iter().map(|r| r.kind).collect()
    }

    #[test]
    fn identical_text() {
        let res = diff_text("a\nb\n", "a\r\nb\r\n", &TextDiffOptions::default()).unwrap();
        assert!(res.identical);
        assert_eq!(res.stats.equal, 2);
    }

    #[test]
    fn aligns_insertions_and_changes() {
        let res = diff_text(
            "one\ntwo\nthree\n",
            "one\ntwo!\nthree\nfour\n",
            &TextDiffOptions::default(),
        )
        .unwrap();
        use RowKind::*;
        assert_eq!(kinds(&res), vec![Equal, Changed, Equal, RightOnly]);
        assert_eq!(res.hunks.len(), 2);
        assert_eq!(res.rows[3].right_no, Some(4));
        assert_eq!(res.rows[3].left_no, None);
    }

    #[test]
    fn whitespace_differences_become_unimportant() {
        let opts = TextDiffOptions {
            whitespace: WhitespaceMode::IgnoreChanges,
            ..Default::default()
        };
        let res = diff_text("let  x = 1;\n", "let x = 1;  \n", &opts).unwrap();
        assert_eq!(kinds(&res), vec![RowKind::Unimportant]);
        assert!(res.hunks.is_empty());
        assert!(!res.identical);
    }

    #[test]
    fn ignore_case() {
        let opts = TextDiffOptions {
            ignore_case: true,
            ..Default::default()
        };
        let res = diff_text("SELECT 1\n", "select 1\n", &opts).unwrap();
        assert!(res.hunks.is_empty());
    }

    #[test]
    fn ignore_patterns_make_lines_unimportant() {
        let opts = TextDiffOptions {
            ignore_patterns: vec![r"\$Id:[^$]*\$".into()],
            ..Default::default()
        };
        let res = diff_text(
            "// $Id: a.c 41 $\nint x;\n",
            "// $Id: a.c 42 $\nint x;\n",
            &opts,
        )
        .unwrap();
        assert_eq!(kinds(&res), vec![RowKind::Unimportant, RowKind::Equal]);
        assert!(res.hunks.is_empty());
    }

    #[test]
    fn bad_ignore_pattern_is_an_error() {
        let opts = TextDiffOptions {
            ignore_patterns: vec!["(".into()],
            ..Default::default()
        };
        assert!(diff_text("a", "b", &opts).is_err());
    }

    #[test]
    fn inline_diff_highlights_changed_word() {
        let (l, r) = inline_diff("the quick fox", "the slow fox");
        assert!(l.iter().any(|s| s.changed && s.text == "quick"));
        assert!(r.iter().any(|s| s.changed && s.text == "slow"));
        let rebuilt: String = l.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(rebuilt, "the quick fox");
    }

    fn diff_named(l: &str, ln: &str, r: &str, rn: &str, opts: &TextDiffOptions) -> TextDiffResult {
        diff_named_text(l, Some(ln), r, Some(rn), opts).unwrap()
    }

    #[test]
    fn ignore_comments_per_grammar() {
        let l = "int x = 1; // old note\n/* header\n   v1 */\nint y;\n";
        let r = "int x = 1;\n/* header\n   v2 */\nint y; // new\n";
        let plain = diff_named(l, "a.c", r, "a.c", &TextDiffOptions::default());
        assert!(!plain.hunks.is_empty());
        let opts = TextDiffOptions {
            ignore_comments: true,
            ..Default::default()
        };
        let res = diff_named(l, "a.c", r, "a.c", &opts);
        assert_eq!(res.left_grammar, Some("c"));
        assert!(res.hunks.is_empty(), "{:?}", kinds(&res));
        assert_eq!(res.stats.unimportant, 3);
        assert!(!res.identical);
        // Code changes still count.
        let res = diff_named("x = 1 # a\n", "a.py", "x = 2 # a\n", "a.py", &opts);
        assert_eq!(kinds(&res), vec![RowKind::Changed]);
        // Without a grammar (unknown extension) comments are ordinary text.
        let res = diff_named(l, "a.txt", r, "a.txt", &opts);
        assert!(!res.hunks.is_empty());
        assert_eq!(res.left_grammar, None);
    }

    #[test]
    fn comment_only_lines_are_unimportant_orphans() {
        let opts = TextDiffOptions {
            ignore_comments: true,
            ..Default::default()
        };
        let res = diff_named(
            "a();\n// added note\nb();\n",
            "x.js",
            "a();\nb();\n",
            "x.js",
            &opts,
        );
        use RowKind::*;
        assert_eq!(kinds(&res), vec![Equal, Unimportant, Equal]);
        assert_eq!(res.rows[1].right_no, None);
        assert!(res.hunks.is_empty());
        // A blank line is not "only comment".
        let res = diff_named("a();\n\nb();\n", "x.js", "a();\nb();\n", "x.js", &opts);
        assert_eq!(kinds(&res), vec![Equal, LeftOnly, Equal]);
    }

    #[test]
    fn ignore_string_contents_keeps_delimiters() {
        let opts = TextDiffOptions {
            ignore_strings: true,
            ..Default::default()
        };
        let res = diff_named(
            "msg = \"Hello\";\n",
            "a.cs",
            "msg = \"Bonjour\";\n",
            "a.cs",
            &opts,
        );
        assert_eq!(kinds(&res), vec![RowKind::Unimportant]);
        // Removing the string altogether is still a difference.
        let res = diff_named("f(\"x\");\n", "a.cs", "f();\n", "a.cs", &opts);
        assert_eq!(kinds(&res), vec![RowKind::Changed]);
        // Explicit grammar overrides the extension.
        let opts = TextDiffOptions {
            ignore_strings: true,
            grammar: Some("sql".into()),
            ..Default::default()
        };
        let res = diff_named("select 'a'\n", "q.txt", "select 'b'\n", "q.txt", &opts);
        assert_eq!(kinds(&res), vec![RowKind::Unimportant]);
    }

    #[test]
    fn syntax_spans_use_utf16_offsets() {
        let opts = TextDiffOptions {
            syntax_spans: true,
            ..Default::default()
        };
        let res = diff_named(
            "let é = \"😀\"; // c\n",
            "a.rs",
            "let é = 1;\n",
            "a.rs",
            &opts,
        );
        let spans = &res.rows[0].left_spans;
        assert_eq!(
            spans[0],
            TokenSpan {
                start: 0,
                end: 3,
                kind: TokenKind::Keyword
            }
        );
        // `"😀"` starts after `let é = ` (8 units) and the emoji is 2 units.
        assert_eq!(
            spans[1],
            TokenSpan {
                start: 8,
                end: 12,
                kind: TokenKind::String
            }
        );
        assert_eq!(
            spans[2],
            TokenSpan {
                start: 14,
                end: 18,
                kind: TokenKind::Comment
            }
        );
        assert_eq!(res.rows[0].right_spans[1].kind, TokenKind::Number);
        let json = serde_json::to_value(&res.rows[0]).unwrap();
        assert_eq!(json["leftSpans"][2]["kind"], "comment");
        // Not sent unless asked for.
        let res = diff_named(
            "let a;\n",
            "a.rs",
            "let b;\n",
            "a.rs",
            &TextDiffOptions::default(),
        );
        assert!(serde_json::to_value(&res.rows[0])
            .unwrap()
            .get("leftSpans")
            .is_none());
    }

    #[test]
    fn ignore_columns() {
        let opts = TextDiffOptions {
            ignore_columns: vec![
                ColumnRange {
                    start: 1,
                    end: Some(6),
                },
                ColumnRange {
                    start: 15,
                    end: None,
                },
            ],
            ..Default::default()
        };
        let l = "000100 MOVE A.  SEQ0001\n000200 STOP.\n";
        let r = "000110 MOVE A.  SEQ0099\n000210 STOP!\n";
        let res = diff_text(l, r, &opts).unwrap();
        assert_eq!(kinds(&res), vec![RowKind::Unimportant, RowKind::Changed]);
    }

    #[test]
    fn page_headers_and_footers() {
        let opts = TextDiffOptions {
            page_header_patterns: vec![r"^Printed on \d+".into()],
            page_header_lines: 1,
            page_footer_lines: 1,
            ..Default::default()
        };
        let l = "REPORT A\nPrinted on 2024\nrow 1\nPage 1\n\x0cREPORT A\nrow 2\nPage 2\n";
        let r = "REPORT B\nPrinted on 2025\nrow 1\nPage one\n\x0cREPORT B\nrow 2\nPage two\n";
        let res = diff_text(l, r, &opts).unwrap();
        assert!(res.hunks.is_empty(), "{:?}", kinds(&res));
        assert_eq!(res.stats.equal, 2);
        // Body lines still count.
        let r2 = r.replace("row 2", "row 3");
        assert_eq!(diff_text(l, &r2, &opts).unwrap().hunks.len(), 1);
        // A form feed alone is unimportant when page handling is on.
        let ff = TextDiffOptions {
            page_header_patterns: vec!["^=+$".into()],
            ..Default::default()
        };
        let res = diff_text("a\n\x0cb\n", "a\nb\n", &ff).unwrap();
        assert_eq!(kinds(&res), vec![RowKind::Equal, RowKind::Unimportant]);
    }

    #[test]
    fn line_terminators_can_matter() {
        let l = "a\r\nb\r\nc\r\n";
        let r = "a\r\nb\nc\r\n";
        assert!(
            diff_text(l, r, &TextDiffOptions::default())
                .unwrap()
                .identical
        );
        let opts = TextDiffOptions {
            compare_line_endings: true,
            ..Default::default()
        };
        let res = diff_text(l, r, &opts).unwrap();
        assert_eq!(
            kinds(&res),
            vec![RowKind::Equal, RowKind::Changed, RowKind::Equal]
        );
        assert_eq!(res.rows[1].left_eol, Some(LineEnding::Crlf));
        assert_eq!(res.rows[1].right_eol, Some(LineEnding::Lf));
        // Lone CRs break lines too.
        let res = diff_text("a\rb\r", "a\nb\n", &opts).unwrap();
        assert_eq!(res.rows.len(), 2);
        assert_eq!(res.stats.changed, 2);
        // The last line without a terminator differs from one with.
        let res = diff_text("a\n", "a", &opts).unwrap();
        assert_eq!(kinds(&res), vec![RowKind::Changed]);
    }

    #[test]
    fn replaced_text_is_unimportant() {
        let opts = TextDiffOptions {
            replacements: vec![
                Replacement {
                    left: "oldName".into(),
                    right: "newName".into(),
                    regex: false,
                },
                Replacement {
                    left: r"v(\d)\.0".into(),
                    right: "version$1".into(),
                    regex: true,
                },
            ],
            ..Default::default()
        };
        let res = diff_text(
            "let oldName = 1;\nuse v2.0;\nother\n",
            "let newName = 1;\nuse version2;\nOTHER\n",
            &opts,
        )
        .unwrap();
        assert_eq!(
            kinds(&res),
            vec![RowKind::Unimportant, RowKind::Unimportant, RowKind::Changed]
        );
        // Replacements apply left → right only.
        let res = diff_text("newName\n", "oldName\n", &opts).unwrap();
        assert_eq!(kinds(&res), vec![RowKind::Changed]);
        let bad = TextDiffOptions {
            replacements: vec![Replacement {
                left: "(".into(),
                right: String::new(),
                regex: true,
            }],
            ..Default::default()
        };
        assert!(diff_text("a", "b", &bad).is_err());
    }

    #[test]
    fn manually_ignored_lines() {
        let opts = TextDiffOptions {
            unimportant_lines: SideRanges {
                left: vec![LineRange { start: 2, end: 3 }],
                right: vec![LineRange { start: 2, end: 2 }],
            },
            ..Default::default()
        };
        let res = diff_text("a\nx1\nx2\nb\n", "a\ny\nb\n", &opts).unwrap();
        use RowKind::*;
        assert_eq!(kinds(&res), vec![Equal, Unimportant, Unimportant, Equal]);
        assert!(res.hunks.is_empty());
    }

    #[test]
    fn similar_lines_are_paired_inside_replace_blocks() {
        let l = "start\nalpha = compute(1)\nend\n";
        let r = "start\ninserted line here\nalpha = compute(2)\nend\n";
        let res = diff_text(l, r, &TextDiffOptions::default()).unwrap();
        use RowKind::*;
        assert_eq!(kinds(&res), vec![Equal, RightOnly, Changed, Equal]);
        assert_eq!(
            (res.rows[2].left_no, res.rows[2].right_no),
            (Some(2), Some(3))
        );

        let positional = TextDiffOptions {
            alignment: Alignment::Positional,
            ..Default::default()
        };
        let res = diff_text(l, r, &positional).unwrap();
        assert_eq!(kinds(&res), vec![Equal, Changed, RightOnly, Equal]);
        assert_eq!(res.rows[1].right_no, Some(2));

        // Wholly different blocks are still shown side by side.
        let res = diff_text("x\nqqqq\ny\n", "x\nzzzz\ny\n", &TextDiffOptions::default()).unwrap();
        assert_eq!(kinds(&res), vec![Equal, Changed, Equal]);
    }

    #[test]
    fn unaligned_blocks_have_no_inline_highlights() {
        let opts = TextDiffOptions {
            alignment: Alignment::Unaligned,
            ..Default::default()
        };
        let res = diff_text("a\nthe quick fox\n", "a\nthe slow fox\nmore\n", &opts).unwrap();
        use RowKind::*;
        assert_eq!(kinds(&res), vec![Equal, Changed, RightOnly]);
        assert_eq!(res.rows[1].left.as_ref().unwrap().len(), 1);
        assert!(res.rows[1].left.as_ref().unwrap()[0].changed);
    }

    #[test]
    fn manual_alignment_anchors_and_breaks() {
        let l = "a\nb\nc\nd\n";
        let r = "x\nc\ny\nd\n";
        let auto = diff_text(l, r, &TextDiffOptions::default()).unwrap();
        // c is aligned with c automatically.
        assert!(auto
            .rows
            .iter()
            .any(|row| row.left_no == Some(3) && row.right_no == Some(2)));

        let mut opts = TextDiffOptions {
            anchors: vec![LinePair { left: 2, right: 3 }],
            ..Default::default()
        };
        let res = diff_text(l, r, &opts).unwrap();
        assert!(res
            .rows
            .iter()
            .any(|row| row.left_no == Some(2) && row.right_no == Some(3)));
        // Line numbers stay in order on both sides.
        let ln: Vec<_> = res.rows.iter().filter_map(|r| r.left_no).collect();
        let rn: Vec<_> = res.rows.iter().filter_map(|r| r.right_no).collect();
        assert_eq!(ln, vec![1, 2, 3, 4]);
        assert_eq!(rn, vec![1, 2, 3, 4]);
        // Crossing and out-of-range anchors are ignored.
        opts.anchors.push(LinePair { left: 3, right: 1 });
        opts.anchors.push(LinePair { left: 99, right: 1 });
        assert!(diff_text(l, r, &opts).is_ok());

        let opts = TextDiffOptions {
            breaks: vec![LinePair { left: 4, right: 4 }],
            ..Default::default()
        };
        let res = diff_text(l, r, &opts).unwrap();
        assert!(!res
            .rows
            .iter()
            .any(|row| row.left_no == Some(4) && row.right_no == Some(4)));
        let last = &res.rows[res.rows.len() - 2..];
        assert_eq!(last[0].kind, RowKind::LeftOnly);
        assert_eq!(last[1].kind, RowKind::RightOnly);
    }

    #[test]
    fn old_options_json_still_deserializes() {
        let opts: TextDiffOptions = serde_json::from_str(
            r#"{"whitespace":"ignoreAll","ignoreCase":true,"algorithm":"myers","ignorePatterns":[]}"#,
        )
        .unwrap();
        assert_eq!(opts.alignment, Alignment::Similar);
        assert!(opts.grammar.is_none());
        let full: TextDiffOptions = serde_json::from_str(
            r#"{"ignoreColumns":[{"start":73,"end":null}],"anchors":[{"left":1,"right":2}],
                "unimportantLines":{"left":[{"start":1,"end":2}]},"alignment":"unaligned",
                "replacements":[{"left":"a","right":"b","regex":false}],"grammar":"c"}"#,
        )
        .unwrap();
        assert_eq!(full.ignore_columns[0].end, None);
        assert_eq!(full.unimportant_lines.left.len(), 1);
    }

    fn tolerance(absolute: f64, relative_percent: f64) -> TextDiffOptions {
        TextDiffOptions {
            numeric_tolerance: NumericTolerance {
                enabled: true,
                absolute,
                relative_percent,
            },
            ..Default::default()
        }
    }

    #[test]
    fn numeric_tolerance_makes_close_numbers_unimportant() {
        use RowKind::*;
        // Off by default: any change in digits is a change.
        let res = diff_text("x=1.001\n", "x=1.002\n", &TextDiffOptions::default()).unwrap();
        assert_eq!(kinds(&res), vec![Changed]);

        let opts = tolerance(0.01, 0.0);
        let res = diff_text("x=1.001\n", "x=1.002\n", &opts).unwrap();
        assert_eq!(kinds(&res), vec![Unimportant]);
        assert!(res.hunks.is_empty());
        assert!(!res.identical);
        // In-tolerance numbers are not highlighted.
        assert!(res.rows[0]
            .left
            .as_ref()
            .unwrap()
            .iter()
            .all(|s| !s.changed));

        // Same value, different spelling.
        let exact = tolerance(0.0, 0.0);
        let res = diff_text(
            "a 1.0\nb 1e3\ncount 7\n",
            "a 1.00\nb 1000\ncount 8\n",
            &exact,
        )
        .unwrap();
        assert_eq!(kinds(&res), vec![Unimportant, Unimportant, Changed]);

        // The sign counts.
        let res = diff_text("v -0.5\n", "v 0.5\n", &tolerance(0.5, 0.0)).unwrap();
        assert_eq!(kinds(&res), vec![Changed]);
        // The text around the numbers must still match.
        let res = diff_text("x=1.001\n", "y=1.001\n", &opts).unwrap();
        assert_eq!(kinds(&res), vec![Changed]);

        // Relative tolerance, in percent of the larger value.
        let rel = tolerance(0.0, 1.0);
        let res = diff_text("total 1000\ntotal 1000\n", "total 1009\ntotal 1011\n", &rel).unwrap();
        assert_eq!(kinds(&res), vec![Unimportant, Changed]);
    }

    #[test]
    fn numeric_tolerance_inline_highlights_only_real_changes() {
        let opts = tolerance(0.01, 0.0);
        let res = diff_text("p = 1.001 + 5;\n", "p = 1.004 + 6;\n", &opts).unwrap();
        assert_eq!(kinds(&res), vec![RowKind::Changed]);
        let changed: Vec<&str> = res.rows[0]
            .right
            .as_ref()
            .unwrap()
            .iter()
            .filter(|s| s.changed)
            .map(|s| s.text.as_str())
            .collect();
        assert_eq!(changed, vec!["6"]);
        let rebuilt: String = res.rows[0]
            .left
            .as_ref()
            .unwrap()
            .iter()
            .map(|s| s.text.as_str())
            .collect();
        assert_eq!(rebuilt, "p = 1.001 + 5;");
    }

    #[test]
    fn numeric_tolerance_pairs_lines_inside_blocks() {
        use RowKind::*;
        // `1e3` and `1000` share no bigrams; the tolerance still pairs them.
        let res = diff_text(
            "a\nn 1e3\nb\n",
            "a\nadded line\nn 1000\nb\n",
            &tolerance(0.0, 0.0),
        )
        .unwrap();
        assert_eq!(kinds(&res), vec![Equal, RightOnly, Unimportant, Equal]);
    }

    fn blocks(pairs: &[(&str, &str)]) -> TextDiffOptions {
        TextDiffOptions {
            ignore_blocks: pairs
                .iter()
                .map(|(b, e)| BlockPattern {
                    begin: (*b).into(),
                    end: (*e).into(),
                })
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn block_expressions_ignore_whole_regions() {
        use RowKind::*;
        let opts = blocks(&[
            ("^// BEGIN GENERATED", "^// END GENERATED"),
            ("^<auto>$", "^</auto>$"),
        ]);
        let l = "a\n// BEGIN GENERATED\nx = 1\n// END GENERATED\nb\n";
        let r = "a\n// BEGIN GENERATED\nx = 2\ny = 3\n// END GENERATED\nb\n";
        let res = diff_text(l, r, &opts).unwrap();
        assert!(res.hunks.is_empty(), "{:?}", kinds(&res));
        assert_eq!(kinds(&res).first(), Some(&Equal));
        assert_eq!(kinds(&res).last(), Some(&Equal));
        // Without the rule the region differs.
        assert!(!diff_text(l, r, &TextDiffOptions::default())
            .unwrap()
            .hunks
            .is_empty());

        // A block on one side only is unimportant, not a left-only run.
        let res = diff_text("a\n<auto>\ngen\n</auto>\nb\n", "a\nb\n", &opts).unwrap();
        assert_eq!(
            kinds(&res),
            vec![Equal, Unimportant, Unimportant, Unimportant, Equal]
        );
        assert_eq!(res.rows[1].right_no, None);

        // Lines after the block count again.
        let res = diff_text("<auto>\n</auto>\nkeep\n", "<auto>\n</auto>\nKEEP\n", &opts).unwrap();
        assert_eq!(kinds(&res).last(), Some(&Changed));

        // A bad regex is an error; a half-filled pair is skipped.
        assert!(diff_text("a", "b", &blocks(&[("(", "x")])).is_err());
        assert!(
            diff_text("a", "a", &blocks(&[("a", "")]))
                .unwrap()
                .identical
        );
    }

    #[test]
    fn block_expressions_nest_and_run_to_eof() {
        let norm = LineNormalizer::new(&blocks(&[(r"^\{$", r"^\}$")])).unwrap();
        // Nested blocks need their own ends.
        let lines = ["a", "{", "{", "}", "x", "}", "b"];
        assert_eq!(
            block_lines(&lines, &norm),
            vec![false, true, true, true, true, true, false]
        );
        // An unterminated block runs to the end of the file.
        let lines = ["a", "{", "x", "{", "}", "y"];
        assert_eq!(
            block_lines(&lines, &norm),
            vec![false, true, true, true, true, true]
        );
        // A fence used as both begin and end closes on its next occurrence.
        let fence = LineNormalizer::new(&blocks(&[("^---$", "^---$")])).unwrap();
        let lines = ["---", "meta", "---", "body"];
        assert_eq!(block_lines(&lines, &fence), vec![true, true, true, false]);
    }

    #[test]
    fn new_rule_options_deserialize() {
        let opts: TextDiffOptions = serde_json::from_str(
            r#"{"numericTolerance":{"enabled":true,"absolute":0.5},"ignoreBlocks":[{"begin":"a","end":"b"}]}"#,
        )
        .unwrap();
        assert!(opts.numeric_tolerance.enabled);
        assert_eq!(opts.numeric_tolerance.relative_percent, 0.0);
        assert_eq!(opts.ignore_blocks[0].end, "b");
        let old: TextDiffOptions = serde_json::from_str(r#"{"ignoreCase":true}"#).unwrap();
        assert!(!old.numeric_tolerance.enabled);
        assert!(old.ignore_blocks.is_empty());
    }

    #[test]
    fn similarity_measure() {
        let a = bigrams("alpha = compute(1)");
        assert_eq!(similarity(&a, &a), 1.0);
        assert!(similarity(&a, &bigrams("alpha = compute(2)")) > 0.8);
        assert!(similarity(&a, &bigrams("zzz")) < 0.1);
        assert_eq!(similarity(&[], &[]), 1.0);
    }
}
