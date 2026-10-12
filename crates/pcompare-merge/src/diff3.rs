//! diff3-style merge of two descendants against their common ancestor.
//!
//! Both descendants are diffed against the base; every run of non-equal
//! lines becomes a *hunk* expressed as a base range. Hunks from the two sides
//! that overlap in the base (or insert at the very same base position) are
//! grouped; a group touched by one side only is that side's change, a group
//! touched by both is either an identical change or a conflict. Changes that
//! merely sit next to each other (e.g. left edits line 2, right edits line 3)
//! do not conflict.

use pcompare_core::text::{LineNormalizer, TextDiffOptions};
use serde::Serialize;
use similar::{capture_diff_slices, DiffOp};

/// First line of an unresolved conflict in rendered output, followed by
/// ` Left #<n>`.
pub const CONFLICT_START: &str = "<<<<<<<";
/// Separates the left and right versions inside a conflict block.
pub const CONFLICT_SEPARATOR: &str = "=======";
/// Last line of a conflict block, followed by ` Right #<n>`.
pub const CONFLICT_END: &str = ">>>>>>>";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ChunkKind {
    /// All three files agree.
    Stable,
    /// Only the left file changed this region; its version is taken.
    LeftChanged,
    /// Only the right file changed this region; its version is taken.
    RightChanged,
    /// Both files made the same change; it is taken once.
    BothSame,
    /// Both files changed this region differently; needs a decision.
    Conflict,
}

/// A 0-based, half-open range of line indices.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct LineRange {
    pub start: usize,
    pub end: usize,
}

impl LineRange {
    pub fn new(start: usize, end: usize) -> Self {
        debug_assert!(start <= end);
        Self { start, end }
    }

    pub fn len(&self) -> usize {
        self.end - self.start
    }

    pub fn is_empty(&self) -> bool {
        self.start == self.end
    }

    fn slice<'a, S>(&self, lines: &'a [S]) -> &'a [S] {
        &lines[self.start..self.end]
    }
}

/// One region of the merge. The chunks of a result tile all three files: each
/// chunk's ranges start where the previous chunk's ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MergeChunk {
    pub kind: ChunkKind,
    pub base: LineRange,
    pub left: LineRange,
    pub right: LineRange,
    /// 1-based conflict number for `Conflict` chunks; matches the number in
    /// the rendered conflict markers.
    pub conflict: Option<usize>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MergeStats {
    pub conflicts: usize,
    pub left_changes: usize,
    pub right_changes: usize,
    pub both_same: usize,
}

impl MergeStats {
    /// Changes that were applied without user input.
    pub fn auto_merged(&self) -> usize {
        self.left_changes + self.right_changes + self.both_same
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MergeResult {
    pub chunks: Vec<MergeChunk>,
    pub stats: MergeStats,
}

/// A run of changed lines in one descendant, relative to the base.
#[derive(Debug, Clone, Copy)]
struct Hunk {
    base: LineRange,
    side: LineRange,
}

impl Hunk {
    /// How many lines this hunk adds to (or removes from) the side.
    fn delta(&self) -> isize {
        self.side.len() as isize - self.base.len() as isize
    }
}

fn hunks(base: &[String], side: &[String], opts: &TextDiffOptions) -> Vec<Hunk> {
    let mut out: Vec<Hunk> = Vec::new();
    let mut open: Option<Hunk> = None;
    for op in capture_diff_slices(opts.algorithm.into(), base, side) {
        if let DiffOp::Equal { .. } = op {
            out.extend(open.take());
            continue;
        }
        let (b, s) = (op.old_range(), op.new_range());
        match &mut open {
            Some(h) => {
                h.base.end = b.end;
                h.side.end = s.end;
            }
            None => {
                open = Some(Hunk {
                    base: LineRange::new(b.start, b.end),
                    side: LineRange::new(s.start, s.end),
                })
            }
        }
    }
    out.extend(open);
    out
}

/// Whether `h` belongs to the group currently spanning base `[start, end)`.
/// Pure insertions at the same position always group; otherwise the ranges
/// must properly overlap, so a change that only touches the group's edge
/// stays separate.
fn joins_group(h: &Hunk, start: usize, end: usize) -> bool {
    (h.base.start < end && start < h.base.end) || (h.base.is_empty() && start == end && h.base.start == start)
}

fn offset(pos: usize, delta: isize) -> usize {
    pos.checked_add_signed(delta).expect("hunk deltas never move a position below zero")
}

/// Merges `left` and `right`, both derived from `base`.
///
/// Lines are matched through a [`LineNormalizer`], so whitespace, case and
/// ignore-pattern options decide what counts as a change. Where the files
/// agree only up to those options, output takes the left file's text. Fails
/// only when an ignore pattern is not a valid regex.
pub fn merge3<S: AsRef<str>>(
    base: &[S],
    left: &[S],
    right: &[S],
    opts: &TextDiffOptions,
) -> pcompare_core::Result<MergeResult> {
    let norm = LineNormalizer::new(opts)?;
    let keys = |lines: &[S]| -> Vec<String> { lines.iter().map(|l| norm.key(l.as_ref())).collect() };
    let (base_keys, left_keys, right_keys) = (keys(base), keys(left), keys(right));
    let left_hunks = hunks(&base_keys, &left_keys, opts);
    let right_hunks = hunks(&base_keys, &right_keys, opts);

    let mut chunks = Vec::new();
    let mut stats = MergeStats::default();
    let (mut li, mut ri) = (0, 0);
    // Outside hunks each side equals the base shifted by these deltas.
    let (mut left_delta, mut right_delta) = (0isize, 0isize);
    let mut pos = 0;

    let push_stable = |chunks: &mut Vec<MergeChunk>, from: usize, to: usize, dl: isize, dr: isize| {
        if from < to {
            chunks.push(MergeChunk {
                kind: ChunkKind::Stable,
                base: LineRange::new(from, to),
                left: LineRange::new(offset(from, dl), offset(to, dl)),
                right: LineRange::new(offset(from, dr), offset(to, dr)),
                conflict: None,
            });
        }
    };

    loop {
        let key = |h: &Hunk| (h.base.start, h.base.end);
        let first = match (left_hunks.get(li), right_hunks.get(ri)) {
            (None, None) => break,
            (Some(l), Some(r)) => key(l).min(key(r)),
            (Some(l), None) => key(l),
            (None, Some(r)) => key(r),
        };
        let (start, mut end) = first;
        push_stable(&mut chunks, pos, start, left_delta, right_delta);

        let left_start = offset(start, left_delta);
        let right_start = offset(start, right_delta);
        let (mut took_left, mut took_right) = (false, false);
        loop {
            if let Some(h) = left_hunks.get(li).filter(|h| joins_group(h, start, end)) {
                end = end.max(h.base.end);
                left_delta += h.delta();
                took_left = true;
                li += 1;
            } else if let Some(h) = right_hunks.get(ri).filter(|h| joins_group(h, start, end)) {
                end = end.max(h.base.end);
                right_delta += h.delta();
                took_right = true;
                ri += 1;
            } else {
                break;
            }
        }

        let left_range = LineRange::new(left_start, offset(end, left_delta));
        let right_range = LineRange::new(right_start, offset(end, right_delta));
        let (kind, conflict) = match (took_left, took_right) {
            (true, false) => {
                stats.left_changes += 1;
                (ChunkKind::LeftChanged, None)
            }
            (false, true) => {
                stats.right_changes += 1;
                (ChunkKind::RightChanged, None)
            }
            _ if left_range.slice(&left_keys) == right_range.slice(&right_keys) => {
                stats.both_same += 1;
                (ChunkKind::BothSame, None)
            }
            _ => {
                stats.conflicts += 1;
                (ChunkKind::Conflict, Some(stats.conflicts))
            }
        };
        chunks.push(MergeChunk {
            kind,
            base: LineRange::new(start, end),
            left: left_range,
            right: right_range,
            conflict,
        });
        pos = end;
    }
    push_stable(&mut chunks, pos, base.len(), left_delta, right_delta);

    Ok(MergeResult { chunks, stats })
}

/// The opening marker line of conflict `n`.
pub fn conflict_start_marker(n: usize) -> String {
    format!("{CONFLICT_START} Left #{n}")
}

/// The closing marker line of conflict `n`.
pub fn conflict_end_marker(n: usize) -> String {
    format!("{CONFLICT_END} Right #{n}")
}

/// Renders the merged output: automatic resolutions applied and every
/// conflict written as
///
/// ```text
/// <<<<<<< Left #n
/// (left lines)
/// =======
/// (right lines)
/// >>>>>>> Right #n
/// ```
///
/// Only the descendants are needed: every non-conflict chunk takes their text.
pub fn render_output<S: AsRef<str>>(result: &MergeResult, left: &[S], right: &[S]) -> Vec<String> {
    let mut out = Vec::new();
    let take = |out: &mut Vec<String>, lines: &[S], range: LineRange| {
        out.extend(range.slice(lines).iter().map(|l| l.as_ref().to_owned()));
    };
    for chunk in &result.chunks {
        match chunk.kind {
            ChunkKind::Stable | ChunkKind::LeftChanged | ChunkKind::BothSame => take(&mut out, left, chunk.left),
            ChunkKind::RightChanged => take(&mut out, right, chunk.right),
            ChunkKind::Conflict => {
                let n = chunk.conflict.unwrap_or_default();
                out.push(conflict_start_marker(n));
                take(&mut out, left, chunk.left);
                out.push(CONFLICT_SEPARATOR.to_owned());
                take(&mut out, right, chunk.right);
                out.push(conflict_end_marker(n));
            }
        }
    }
    out
}

/// 3-way merge of the "file ends with a line break" flag: whichever side
/// changed it wins (they cannot disagree, since it is a boolean).
pub fn merge_trailing_newline(base: bool, left: bool, right: bool) -> bool {
    if left == base {
        right
    } else {
        left
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::split_lines;
    use pcompare_core::text::WhitespaceMode;

    struct Merged {
        result: MergeResult,
        output: Vec<String>,
    }

    fn lines(text: &str) -> Vec<String> {
        split_lines(text).0
    }

    fn merge_with(base: &str, left: &str, right: &str, opts: &TextDiffOptions) -> Merged {
        let (b, l, r) = (lines(base), lines(left), lines(right));
        let result = merge3(&b, &l, &r, opts).unwrap();
        assert_tiles(&result, &b, &l, &r);
        let output = render_output(&result, &l, &r);
        Merged { result, output }
    }

    fn merge(base: &str, left: &str, right: &str) -> Merged {
        merge_with(base, left, right, &TextDiffOptions::default())
    }

    /// Chunks must cover every line of all three files exactly once, in order.
    fn assert_tiles(result: &MergeResult, base: &[String], left: &[String], right: &[String]) {
        let (mut b, mut l, mut r) = (0, 0, 0);
        for c in &result.chunks {
            assert_eq!((c.base.start, c.left.start, c.right.start), (b, l, r), "gap before {c:?}");
            if c.kind == ChunkKind::Stable {
                assert!(c.base.len() == c.left.len() && c.base.len() == c.right.len());
                assert!(!c.base.is_empty());
            }
            (b, l, r) = (c.base.end, c.left.end, c.right.end);
        }
        assert_eq!((b, l, r), (base.len(), left.len(), right.len()));
    }

    fn kinds(m: &Merged) -> Vec<ChunkKind> {
        m.result.chunks.iter().map(|c| c.kind).collect()
    }

    use ChunkKind::*;

    #[test]
    fn identical_files_are_one_stable_chunk() {
        let m = merge("a\nb\n", "a\nb\n", "a\nb\n");
        assert_eq!(kinds(&m), vec![Stable]);
        assert_eq!(m.output, lines("a\nb"));
        assert_eq!(m.result.stats, MergeStats::default());
    }

    #[test]
    fn non_overlapping_edits_merge_automatically() {
        let m = merge("a\nb\nc\nd\ne\n", "a\nB\nc\nd\ne\n", "a\nb\nc\nD\ne\n");
        assert_eq!(kinds(&m), vec![Stable, LeftChanged, Stable, RightChanged, Stable]);
        assert_eq!(m.output, lines("a\nB\nc\nD\ne"));
        assert_eq!(m.result.stats.auto_merged(), 2);
        assert_eq!(m.result.stats.conflicts, 0);
    }

    #[test]
    fn adjacent_edits_do_not_conflict() {
        let m = merge("a\nb\nc\nd\n", "a\nB\nc\nd\n", "a\nb\nC\nd\n");
        assert_eq!(kinds(&m), vec![Stable, LeftChanged, RightChanged, Stable]);
        assert_eq!(m.output, lines("a\nB\nC\nd"));
    }

    #[test]
    fn identical_edits_are_taken_once() {
        let m = merge("a\nb\nc\n", "a\nX\nY\nc\n", "a\nX\nY\nc\n");
        assert_eq!(kinds(&m), vec![Stable, BothSame, Stable]);
        assert_eq!(m.output, lines("a\nX\nY\nc"));
        assert_eq!(m.result.stats.both_same, 1);
    }

    #[test]
    fn overlapping_edits_conflict() {
        let m = merge("a\nb\nc\n", "a\nleft\nc\n", "a\nright\nc\n");
        assert_eq!(kinds(&m), vec![Stable, Conflict, Stable]);
        let c = &m.result.chunks[1];
        assert_eq!(c.conflict, Some(1));
        assert_eq!((c.base, c.left, c.right), (LineRange::new(1, 2), LineRange::new(1, 2), LineRange::new(1, 2)));
        assert_eq!(
            m.output,
            vec!["a", "<<<<<<< Left #1", "left", "=======", "right", ">>>>>>> Right #1", "c"]
        );
    }

    #[test]
    fn conflicts_are_numbered_in_order() {
        let m = merge("a\nb\nc\nd\ne\n", "A1\nb\nc\nd\nE1\n", "A2\nb\nc\nd\nE2\n");
        let numbers: Vec<_> = m.result.chunks.iter().filter_map(|c| c.conflict).collect();
        assert_eq!(numbers, vec![1, 2]);
        assert_eq!(m.result.stats.conflicts, 2);
        assert!(m.output.contains(&conflict_end_marker(2)));
    }

    #[test]
    fn partially_overlapping_edits_form_one_conflict() {
        // Left rewrites b..c, right rewrites c..d: the overlap on c chains them.
        let m = merge("a\nb\nc\nd\ne\n", "a\nB\nC\nd\ne\n", "a\nb\nc2\nd2\ne\n");
        assert_eq!(kinds(&m), vec![Stable, Conflict, Stable]);
        let c = &m.result.chunks[1];
        assert_eq!(c.base, LineRange::new(1, 4));
        assert_eq!(c.left, LineRange::new(1, 4));
        assert_eq!(c.right, LineRange::new(1, 4));
    }

    #[test]
    fn insertions_at_same_point() {
        let different = merge("a\nb\n", "a\nX\nb\n", "a\nY\nb\n");
        assert_eq!(kinds(&different), vec![Stable, Conflict, Stable]);
        assert_eq!(different.result.chunks[1].base, LineRange::new(1, 1));

        let same = merge("a\nb\n", "a\nX\nb\n", "a\nX\nb\n");
        assert_eq!(kinds(&same), vec![Stable, BothSame, Stable]);
        assert_eq!(same.output, lines("a\nX\nb"));
    }

    #[test]
    fn insertion_next_to_other_sides_change_is_separate() {
        // Right inserts X before b while left deletes b.
        let m = merge("a\nb\nc\n", "a\nc\n", "a\nX\nb\nc\n");
        assert_eq!(kinds(&m), vec![Stable, RightChanged, LeftChanged, Stable]);
        assert_eq!(m.output, lines("a\nX\nc"));
    }

    #[test]
    fn deletion_versus_modification_conflicts() {
        let m = merge("a\nb\nc\n", "a\nc\n", "a\nB\nc\n");
        assert_eq!(kinds(&m), vec![Stable, Conflict, Stable]);
        let c = &m.result.chunks[1];
        assert!(c.left.is_empty());
        assert_eq!(m.output, vec!["a", "<<<<<<< Left #1", "=======", "B", ">>>>>>> Right #1", "c"]);
    }

    #[test]
    fn one_sided_deletion_is_applied() {
        let m = merge("a\nb\nc\n", "a\nb\nc\n", "a\nc\n");
        assert_eq!(kinds(&m), vec![Stable, RightChanged, Stable]);
        assert_eq!(m.output, lines("a\nc"));
    }

    #[test]
    fn both_sides_deleting_the_same_lines_agree() {
        let m = merge("a\nb\nc\n", "a\nc\n", "a\nc\n");
        assert_eq!(kinds(&m), vec![Stable, BothSame, Stable]);
        assert_eq!(m.output, lines("a\nc"));
    }

    #[test]
    fn edits_at_file_edges() {
        let m = merge("a\nb\nc\n", "start\na\nb\nc\n", "a\nb\nc\nend\n");
        assert_eq!(kinds(&m), vec![LeftChanged, Stable, RightChanged]);
        assert_eq!(m.output, lines("start\na\nb\nc\nend"));
    }

    #[test]
    fn empty_files() {
        assert!(merge("", "", "").result.chunks.is_empty());

        let one_side = merge("", "new\n", "");
        assert_eq!(kinds(&one_side), vec![LeftChanged]);
        assert_eq!(one_side.output, lines("new"));

        let both = merge("", "l\n", "r\n");
        assert_eq!(kinds(&both), vec![Conflict]);

        let emptied = merge("a\nb\n", "", "a\nb\n");
        assert_eq!(kinds(&emptied), vec![LeftChanged]);
        assert!(emptied.output.is_empty());
    }

    #[test]
    fn trailing_newline_is_merged_three_way() {
        assert!(merge_trailing_newline(true, true, true));
        assert!(!merge_trailing_newline(true, false, true));
        assert!(!merge_trailing_newline(true, true, false));
        assert!(merge_trailing_newline(false, true, false));
        assert!(merge_trailing_newline(false, false, true));
        // The flag does not affect line matching.
        let m = merge("a\nb", "a\nb\n", "a\nB");
        assert_eq!(kinds(&m), vec![Stable, RightChanged]);
    }

    #[test]
    fn whitespace_options_decide_what_conflicts() {
        let (base, left, right) = ("fn f() {\nx\n}\n", "fn f() {\n    x\n}\n", "fn f() {\ny\n}\n");
        let strict = merge(base, left, right);
        assert_eq!(kinds(&strict), vec![Stable, Conflict, Stable]);

        let lenient = merge_with(
            base,
            left,
            right,
            &TextDiffOptions {
                whitespace: WhitespaceMode::IgnoreAll,
                ..Default::default()
            },
        );
        assert_eq!(kinds(&lenient), vec![Stable, RightChanged, Stable]);
        assert_eq!(lenient.output, lines("fn f() {\ny\n}"));
    }

    #[test]
    fn ignore_case_makes_case_only_edits_identical() {
        let opts = TextDiffOptions {
            ignore_case: true,
            ..Default::default()
        };
        let m = merge_with("a\nb\n", "a\nX\n", "a\nx\n", &opts);
        assert_eq!(kinds(&m), vec![Stable, BothSame]);
        assert_eq!(m.output, lines("a\nX"));
    }

    #[test]
    fn ignore_patterns_hide_irrelevant_edits() {
        let opts = TextDiffOptions {
            ignore_patterns: vec![r"\$Rev: \d+ \$".into()],
            ..Default::default()
        };
        let m = merge_with("$Rev: 1 $\nx\n", "$Rev: 2 $\nx\n", "$Rev: 3 $\ny\n", &opts);
        assert_eq!(kinds(&m), vec![Stable, RightChanged]);
        assert_eq!(m.output, lines("$Rev: 2 $\ny"));

        let bad = TextDiffOptions {
            ignore_patterns: vec!["(".into()],
            ..Default::default()
        };
        assert!(merge3(&["a"], &["b"], &["c"], &bad).is_err());
    }

    #[test]
    fn many_mixed_changes_tile_correctly() {
        let base = (0..40).map(|i| format!("line {i}\n")).collect::<String>();
        let left = base.replace("line 3\n", "").replace("line 10\n", "L10\nL10b\n").replace("line 30\n", "both\n");
        let right = base
            .replace("line 20\n", "line 20\nR\n")
            .replace("line 10\n", "R10\n")
            .replace("line 30\n", "both\n");
        let m = merge(&base, &left, &right);
        assert_eq!(m.result.stats.conflicts, 1);
        assert_eq!(m.result.stats.left_changes, 1);
        assert_eq!(m.result.stats.right_changes, 1);
        assert_eq!(m.result.stats.both_same, 1);
    }
}
