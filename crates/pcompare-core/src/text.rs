//! Line-oriented text comparison producing aligned side-by-side rows.

use crate::{CoreError, Result};
use regex::Regex;
use serde::{Deserialize, Serialize};
use similar::{capture_diff_slices, Algorithm, ChangeTag, DiffOp, TextDiff};

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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum RowKind {
    Equal,
    /// Lines differ only in ways the current options consider unimportant.
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

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffRow {
    pub kind: RowKind,
    /// 1-based line numbers; `None` on the side where the line is absent.
    pub left_no: Option<usize>,
    pub right_no: Option<usize>,
    pub left: Option<Vec<Segment>>,
    pub right: Option<Vec<Segment>>,
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
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TextDiffResult {
    pub rows: Vec<DiffRow>,
    pub hunks: Vec<Hunk>,
    pub stats: DiffStats,
    pub identical: bool,
}

/// Turns a line into the string that is actually compared. Built once per
/// diff so regexes are compiled a single time.
pub struct LineNormalizer {
    whitespace: WhitespaceMode,
    ignore_case: bool,
    ignore: Vec<Regex>,
}

impl LineNormalizer {
    pub fn new(opts: &TextDiffOptions) -> Result<Self> {
        let ignore = opts
            .ignore_patterns
            .iter()
            .map(|p| p.trim())
            .filter(|p| !p.is_empty())
            .map(|p| {
                Regex::new(p).map_err(|e| CoreError::BadPattern {
                    pattern: p.to_owned(),
                    message: e.to_string(),
                })
            })
            .collect::<Result<_>>()?;
        Ok(Self {
            whitespace: opts.whitespace,
            ignore_case: opts.ignore_case,
            ignore,
        })
    }

    pub fn key(&self, line: &str) -> String {
        let mut stripped = std::borrow::Cow::Borrowed(line);
        for re in &self.ignore {
            if let std::borrow::Cow::Owned(s) = re.replace_all(&stripped, "") {
                stripped = std::borrow::Cow::Owned(s);
            }
        }
        let key = match self.whitespace {
            WhitespaceMode::Compare => stripped.into_owned(),
            WhitespaceMode::IgnoreTrailing => stripped.trim_end().to_owned(),
            WhitespaceMode::IgnoreChanges => stripped.split_whitespace().collect::<Vec<_>>().join(" "),
            WhitespaceMode::IgnoreAll => stripped.chars().filter(|c| !c.is_whitespace()).collect(),
        };
        if self.ignore_case {
            key.to_lowercase()
        } else {
            key
        }
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

pub fn diff_lines(left: &[&str], right: &[&str], opts: &TextDiffOptions) -> Result<TextDiffResult> {
    let norm = LineNormalizer::new(opts)?;
    let left_keys: Vec<String> = left.iter().map(|l| norm.key(l)).collect();
    let right_keys: Vec<String> = right.iter().map(|l| norm.key(l)).collect();
    let ops = capture_diff_slices(opts.algorithm.into(), &left_keys, &right_keys);

    let mut rows = Vec::with_capacity(left.len().max(right.len()));
    let mut stats = DiffStats::default();

    let left_only = |i: usize, rows: &mut Vec<DiffRow>, stats: &mut DiffStats| {
        stats.left_only += 1;
        rows.push(DiffRow {
            kind: RowKind::LeftOnly,
            left_no: Some(i + 1),
            right_no: None,
            left: Some(whole(left[i], true)),
            right: None,
        });
    };
    let right_only = |j: usize, rows: &mut Vec<DiffRow>, stats: &mut DiffStats| {
        stats.right_only += 1;
        rows.push(DiffRow {
            kind: RowKind::RightOnly,
            left_no: None,
            right_no: Some(j + 1),
            left: None,
            right: Some(whole(right[j], true)),
        });
    };

    for op in ops {
        match op {
            DiffOp::Equal {
                old_index,
                new_index,
                len,
            } => {
                for k in 0..len {
                    let (i, j) = (old_index + k, new_index + k);
                    let (kind, l, r) = if left[i] == right[j] {
                        stats.equal += 1;
                        (RowKind::Equal, whole(left[i], false), whole(right[j], false))
                    } else {
                        stats.unimportant += 1;
                        let (l, r) = inline_diff(left[i], right[j]);
                        (RowKind::Unimportant, l, r)
                    };
                    rows.push(DiffRow {
                        kind,
                        left_no: Some(i + 1),
                        right_no: Some(j + 1),
                        left: Some(l),
                        right: Some(r),
                    });
                }
            }
            DiffOp::Delete {
                old_index, old_len, ..
            } => (old_index..old_index + old_len).for_each(|i| left_only(i, &mut rows, &mut stats)),
            DiffOp::Insert {
                new_index, new_len, ..
            } => (new_index..new_index + new_len).for_each(|j| right_only(j, &mut rows, &mut stats)),
            DiffOp::Replace {
                old_index,
                old_len,
                new_index,
                new_len,
            } => {
                let paired = old_len.min(new_len);
                for k in 0..paired {
                    let (i, j) = (old_index + k, new_index + k);
                    let (l, r) = inline_diff(left[i], right[j]);
                    stats.changed += 1;
                    rows.push(DiffRow {
                        kind: RowKind::Changed,
                        left_no: Some(i + 1),
                        right_no: Some(j + 1),
                        left: Some(l),
                        right: Some(r),
                    });
                }
                (old_index + paired..old_index + old_len)
                    .for_each(|i| left_only(i, &mut rows, &mut stats));
                (new_index + paired..new_index + new_len)
                    .for_each(|j| right_only(j, &mut rows, &mut stats));
            }
        }
    }

    let hunks = collect_hunks(&rows);
    Ok(TextDiffResult {
        identical: hunks.is_empty() && stats.unimportant == 0,
        rows,
        hunks,
        stats,
    })
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

pub fn diff_text(left: &str, right: &str, opts: &TextDiffOptions) -> Result<TextDiffResult> {
    let l: Vec<&str> = left.lines().collect();
    let r: Vec<&str> = right.lines().collect();
    diff_lines(&l, &r, opts)
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
        let res = diff_text("// $Id: a.c 41 $\nint x;\n", "// $Id: a.c 42 $\nint x;\n", &opts).unwrap();
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
}
