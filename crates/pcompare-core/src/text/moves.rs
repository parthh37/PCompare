//! Moved-block detection: runs of left-only lines that reappear, line for
//! line, as a run of right-only lines somewhere else in the file.
//!
//! This is a pass over finished rows. Row kinds and hunks are unchanged
//! (a moved block is still a difference on both sides). The pass only tags
//! each such row with the block's id, so a viewer can tint the pair and
//! link one end to the other.

use std::collections::HashMap;

use serde::Serialize;

use super::{DiffRow, RowKind};

/// A block of lines that left one place and arrived at another.
///
/// Shape (JSON): `{ "id": 0, "leftStart": 12, "rightStart": 40, "len": 5 }`,
/// with 1-based line numbers. Left lines `leftStart..leftStart+len` are the
/// same (under the comparison rules) as right lines `rightStart..rightStart+len`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MovedBlock {
    pub id: u32,
    pub left_start: usize,
    pub right_start: usize,
    pub len: usize,
}

/// Same-key candidates tried per starting line. Keeps lines like `}` that
/// occur hundreds of times from making the pass quadratic.
const MAX_CANDIDATES: usize = 64;

/// Whether a matched run of lines is worth calling a move rather than a
/// coincidence. `lines` is the block's original text (one side's; both
/// sides compare equal under the rules).
///
/// Follows `git diff --color-moved`: a block needs at least 20 alphanumeric
/// characters, so a moved `}` or a lone blank line never counts, while a
/// single substantial line (an `import`, a long call) does.
pub(crate) fn is_significant_move(lines: &[&str]) -> bool {
    const MIN_ALNUM: usize = 20;
    lines
        .iter()
        .flat_map(|l| l.chars())
        .filter(|c| c.is_alphanumeric())
        .take(MIN_ALNUM)
        .count()
        >= MIN_ALNUM
}

/// Finds moved blocks among the rows' left-only and right-only lines and
/// tags those rows. `lkeys` / `rkeys` are every line's comparison key and
/// `ltext` the left side's text (0-based, as the rows' numbers − 1).
pub(crate) fn detect(
    rows: &mut [DiffRow],
    lkeys: &[String],
    rkeys: &[String],
    ltext: &[&str],
) -> Vec<MovedBlock> {
    // Line index → row index, for the lines that only exist on one side.
    let mut lrow: Vec<Option<usize>> = vec![None; lkeys.len()];
    let mut rrow: Vec<Option<usize>> = vec![None; rkeys.len()];
    for (i, row) in rows.iter().enumerate() {
        match row.kind {
            RowKind::LeftOnly => {
                if let Some(n) = row.left_no {
                    lrow[n - 1] = Some(i);
                }
            }
            RowKind::RightOnly => {
                if let Some(n) = row.right_no {
                    rrow[n - 1] = Some(i);
                }
            }
            _ => {}
        }
    }

    let mut by_key: HashMap<&str, Vec<usize>> = HashMap::new();
    for (r, key) in rkeys.iter().enumerate() {
        if rrow[r].is_some() && !key.is_empty() {
            by_key.entry(key.as_str()).or_default().push(r);
        }
    }
    if by_key.is_empty() {
        return Vec::new();
    }

    let mut lused = vec![false; lkeys.len()];
    let mut rused = vec![false; rkeys.len()];
    let mut moves = Vec::new();
    for l in 0..lkeys.len() {
        if lrow[l].is_none() || lused[l] || lkeys[l].is_empty() {
            continue;
        }
        let Some(cands) = by_key.get(lkeys[l].as_str()) else {
            continue;
        };
        let run = |r: usize| {
            (0..)
                .take_while(|&k| {
                    let (a, b) = (l + k, r + k);
                    a < lkeys.len()
                        && b < rkeys.len()
                        && lrow[a].is_some()
                        && rrow[b].is_some()
                        && !lused[a]
                        && !rused[b]
                        && lkeys[a] == rkeys[b]
                })
                .count()
        };
        let best = cands
            .iter()
            .copied()
            .filter(|&r| !rused[r])
            .take(MAX_CANDIDATES)
            .map(|r| (run(r), r))
            // Longest run; the earliest candidate wins a tie.
            .max_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));
        let Some((len, r)) = best else { continue };
        if len == 0 || !is_significant_move(&ltext[l..l + len]) {
            continue;
        }
        let id = moves.len() as u32;
        for k in 0..len {
            lused[l + k] = true;
            rused[r + k] = true;
            if let Some(i) = lrow[l + k] {
                rows[i].left_move = Some(id);
            }
            if let Some(i) = rrow[r + k] {
                rows[i].right_move = Some(id);
            }
        }
        moves.push(MovedBlock {
            id,
            left_start: l + 1,
            right_start: r + 1,
            len,
        });
    }
    moves
}

#[cfg(test)]
mod tests {
    use super::super::{diff_text, RowKind, TextDiffOptions, WhitespaceMode};
    use super::*;

    const FN_A: &str = "fn alpha() {\n    let first = compute_first();\n    first + 1\n}\n";
    const FN_B: &str = "fn beta() {\n    let second = compute_second();\n    second * 2\n}\n";
    const FN_C: &str = "fn gamma() {\n    let third = compute_third();\n    third - 3\n}\n";

    #[test]
    fn swapped_functions_are_moves() {
        let left = format!("{FN_A}\n{FN_B}\n{FN_C}");
        let right = format!("{FN_B}\n{FN_C}\n{FN_A}");
        let d = diff_text(&left, &right, &TextDiffOptions::default()).unwrap();
        assert_eq!(d.moves.len(), 1, "{:?}", d.moves);
        let m = d.moves[0];
        assert_eq!((m.left_start, m.len), (1, 4));
        assert_eq!(m.right_start, 11);
        // Tagged rows, kinds untouched.
        for row in &d.rows {
            if row.left_move.is_some() {
                assert_eq!(row.kind, RowKind::LeftOnly);
            }
            if row.right_move.is_some() {
                assert_eq!(row.kind, RowKind::RightOnly);
            }
        }
        assert_eq!(d.stats.moved, 4);
        assert!(!d.hunks.is_empty());
    }

    #[test]
    fn trivial_lines_are_not_moves() {
        let d = diff_text("}\nkeep this line\n", "keep this line\n}\n", &TextDiffOptions::default()).unwrap();
        assert!(d.moves.is_empty());
        assert!(d.rows.iter().all(|r| r.left_move.is_none() && r.right_move.is_none()));
    }

    #[test]
    fn edited_lines_are_not_moves() {
        let left = format!("{FN_A}\n{FN_B}\n{FN_C}");
        let edited = FN_A.replace("first + 1", "first + 2");
        let right = format!("{FN_B}\n{FN_C}\n{edited}");
        let d = diff_text(&left, &right, &TextDiffOptions::default()).unwrap();
        // The first two lines moved verbatim; the edited third ends the block.
        assert_eq!(d.moves.len(), 1, "{:?}", d.moves);
        assert_eq!((d.moves[0].left_start, d.moves[0].right_start, d.moves[0].len), (1, 11, 2));
    }

    #[test]
    fn moves_follow_the_comparison_rules() {
        // Strictly, the import lines differ; ignoring whitespace, one of
        // the two lines anchors the diff and the other one moved.
        let left = "import alpha.beta.gamma;\nlet total = compute(alpha, beta);\n";
        let right = "let total = compute(alpha, beta);\n    import   alpha.beta.gamma;\n";
        let strict = diff_text(left, right, &TextDiffOptions::default()).unwrap();
        assert!(strict.moves.is_empty());
        let loose = TextDiffOptions {
            whitespace: WhitespaceMode::IgnoreAll,
            ..Default::default()
        };
        let d = diff_text(left, right, &loose).unwrap();
        assert_eq!(d.moves.len(), 1);
    }

    #[test]
    fn significance_threshold() {
        assert!(!is_significant_move(&["}", "", "  return x;"]));
        assert!(is_significant_move(&["use std::collections::HashMap;"]));
    }
}
