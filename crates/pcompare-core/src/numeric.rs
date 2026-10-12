//! Numeric tolerance for text comparison: finds the numbers in a line so two
//! lines that differ only in nearly equal values can be told apart from real
//! changes.
//!
//! A number is an optional sign, digits with an optional fraction (or a bare
//! fraction such as `.5`) and an optional exponent: `42`, `-0.5`, `+1.25e-3`.
//! Digits inside identifiers (`var2`, `x86`) are text, and so is a sign that
//! follows a word, a digit or a closing bracket (`a-1`, `2024-01-02`), where
//! it is an operator or separator rather than part of the value. Dotted
//! chains such as versions (`1.2.3`, `v2.0`) keep only their leading number.

use serde::{Deserialize, Serialize};

/// When numbers count as equal. With `enabled`, numbers are compared by
/// value (`1.0` = `1.00`, `1e3` = `1000`) and also match when they differ by
/// at most `absolute`, or by at most `relative_percent` percent of the larger
/// magnitude.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct NumericTolerance {
    pub enabled: bool,
    pub absolute: f64,
    pub relative_percent: f64,
}

impl NumericTolerance {
    /// `a` and `b` are equal within the tolerance. The bounds are inclusive,
    /// with a little slack for binary rounding (`1.1 - 1.0` is a hair over 0.1).
    pub fn within(&self, a: f64, b: f64) -> bool {
        if a == b {
            return true;
        }
        let diff = (a - b).abs();
        let scale = a.abs().max(b.abs());
        let slack = f64::EPSILON * 16.0 * scale.max(1.0);
        diff <= self.absolute.max(0.0) + slack
            || diff <= self.relative_percent.max(0.0) / 100.0 * scale + slack
    }
}

/// A piece of a line: a number (with its value), a word, a whitespace run or
/// a single other character.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Piece<'a> {
    pub text: &'a str,
    pub num: Option<f64>,
}

/// The end (char index, exclusive) of the number starting at `chars[i]`, if
/// one starts there. `prev` and `prev2` are the two characters before it.
fn number_end(
    chars: &[(usize, char)],
    i: usize,
    prev: Option<char>,
    prev2: Option<char>,
) -> Option<usize> {
    let at = |k: usize| chars.get(k).map(|&(_, c)| c);
    let digit = |k: usize| at(k).is_some_and(|c| c.is_ascii_digit());
    if prev == Some('.') && prev2.is_some_and(char::is_alphanumeric) {
        return None;
    }
    let mut j = i;
    let signed = matches!(at(j), Some('+' | '-'));
    if signed {
        if prev.is_some_and(|p| p.is_alphanumeric() || matches!(p, '_' | ')' | ']' | '}' | '.')) {
            return None;
        }
        j += 1;
    }
    let int_start = j;
    while digit(j) {
        j += 1;
    }
    let int_digits = j - int_start;
    let mut frac_digits = 0;
    if at(j) == Some('.') && digit(j + 1) {
        // `1.2.3`: the second dot is a separator, not the start of `.3`.
        if int_digits == 0 && !signed && prev.is_some_and(|p| p.is_ascii_digit()) {
            return None;
        }
        j += 1;
        while digit(j) {
            j += 1;
            frac_digits += 1;
        }
    }
    if int_digits == 0 && frac_digits == 0 {
        return None;
    }
    if matches!(at(j), Some('e' | 'E')) {
        let mut k = j + 1;
        if matches!(at(k), Some('+' | '-')) {
            k += 1;
        }
        if digit(k) {
            while digit(k) {
                k += 1;
            }
            j = k;
        }
    }
    Some(j)
}

/// Splits a line into pieces; concatenating their texts gives the line back.
pub fn pieces(line: &str) -> Vec<Piece<'_>> {
    let chars: Vec<(usize, char)> = line.char_indices().collect();
    let byte = |k: usize| chars.get(k).map_or(line.len(), |&(b, _)| b);
    let mut out = Vec::new();
    let mut prev: Option<char> = None;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i].1;
        let mut num = None;
        let end = if c.is_alphabetic() || c == '_' {
            let mut j = i + 1;
            while chars
                .get(j)
                .is_some_and(|&(_, c)| c.is_alphanumeric() || c == '_')
            {
                j += 1;
            }
            j
        } else if let Some(j) = number_end(&chars, i, prev, i.checked_sub(2).map(|k| chars[k].1)) {
            num = line[byte(i)..byte(j)].parse::<f64>().ok();
            j
        } else if c.is_whitespace() {
            let mut j = i + 1;
            while chars.get(j).is_some_and(|&(_, c)| c.is_whitespace()) {
                j += 1;
            }
            j
        } else {
            i + 1
        };
        out.push(Piece {
            text: &line[byte(i)..byte(end)],
            num,
        });
        prev = Some(chars[end - 1].1);
        i = end;
    }
    out
}

/// The line as alternating text runs and numbers.
fn runs(line: &str) -> Vec<Piece<'_>> {
    let mut out: Vec<Piece> = Vec::new();
    let mut text_start: Option<usize> = None;
    let mut pos = 0;
    for p in pieces(line) {
        if p.num.is_some() {
            if let Some(s) = text_start.take() {
                out.push(Piece {
                    text: &line[s..pos],
                    num: None,
                });
            }
            out.push(p);
        } else if text_start.is_none() {
            text_start = Some(pos);
        }
        pos += p.text.len();
    }
    if let Some(s) = text_start {
        out.push(Piece {
            text: &line[s..],
            num: None,
        });
    }
    out
}

/// The lines have the same text between their numbers and every pair of
/// numbers is equal within `tol`.
pub fn lines_equal(a: &str, b: &str, tol: &NumericTolerance) -> bool {
    let (ra, rb) = (runs(a), runs(b));
    ra.len() == rb.len()
        && ra.iter().zip(&rb).all(|(x, y)| match (x.num, y.num) {
            (Some(p), Some(q)) => tol.within(p, q),
            (None, None) => x.text == y.text,
            _ => false,
        })
}

/// A piece compared under a tolerance, for the intra-line diff.
#[derive(Debug, Clone, Copy)]
pub struct TolerantPiece<'a> {
    pub piece: Piece<'a>,
    pub tol: &'a NumericTolerance,
}

impl PartialEq for TolerantPiece<'_> {
    fn eq(&self, other: &Self) -> bool {
        match (self.piece.num, other.piece.num) {
            (Some(a), Some(b)) => self.tol.within(a, b),
            (None, None) => self.piece.text == other.piece.text,
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nums(line: &str) -> Vec<f64> {
        pieces(line).iter().filter_map(|p| p.num).collect()
    }

    fn tol(absolute: f64, relative_percent: f64) -> NumericTolerance {
        NumericTolerance {
            enabled: true,
            absolute,
            relative_percent,
        }
    }

    #[test]
    fn finds_numbers() {
        assert_eq!(
            nums("x=1.001, y=-2, z=+3e2, w=.5"),
            vec![1.001, -2.0, 300.0, 0.5]
        );
        assert_eq!(nums("1E-3 7e"), vec![0.001, 7.0]);
        // Digits in identifiers are text; signs after words or digits are operators.
        assert_eq!(nums("var2 x86_64 a-1 f(x)-3"), vec![1.0, 3.0]);
        assert_eq!(nums("2024-01-02"), vec![2024.0, 1.0, 2.0]);
        assert_eq!(nums("v1.2.3"), Vec::<f64>::new());
        assert_eq!(nums("1.2.3 v2.0"), vec![1.2]);
        assert_eq!(nums("10px 1. end"), vec![10.0, 1.0]);
        let line = "é = -1.5e3; // ok";
        assert_eq!(
            pieces(line).iter().map(|p| p.text).collect::<String>(),
            line
        );
    }

    #[test]
    fn tolerance_bounds() {
        let t = tol(0.1, 0.0);
        assert!(t.within(1.0, 1.1));
        assert!(!t.within(1.0, 1.2));
        assert!(!t.within(-0.5, 0.5));
        let r = tol(0.0, 1.0);
        assert!(r.within(1000.0, 1010.0));
        assert!(!r.within(1000.0, 1020.0));
        assert!(tol(0.0, 0.0).within(1e3, 1000.0));
    }

    #[test]
    fn line_equality() {
        let exact = tol(0.0, 0.0);
        assert!(lines_equal("a 1.0 b", "a 1.00 b", &exact));
        assert!(lines_equal("n=1e3", "n=1000", &exact));
        assert!(!lines_equal("a 1 b", "a 1 c", &exact));
        assert!(!lines_equal("a 1", "a 1 2", &exact));
        assert!(!lines_equal("x 1", "x y", &exact));
        assert!(lines_equal("x=1.001", "x=1.002", &tol(0.01, 0.0)));
        assert!(!lines_equal("x=-0.5", "x=0.5", &tol(0.5, 0.0)));
    }
}
