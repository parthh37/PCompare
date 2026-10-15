//! Headless quick compare (`pcompare /qc left right`): decides whether two
//! files or folders are the same without opening a window, and exits with
//! the standard codes:
//!
//! | code | meaning |
//! |------|---------|
//! | 1    | binary same (size, CRC and binary compares: same) |
//! | 2    | rules-based same (only encoding or line endings differ) |
//! | 11   | binary differences |
//! | 12   | similar (only unimportant differences) |
//! | 13   | rules-based differences |
//! | 100  | error |

use pcompare_core::folder::{self, ContentMethod, FolderCompareOptions};
use pcompare_core::text::TextDiffOptions;
use pcompare_core::vfs::LocalFs;
use pcompare_core::{textfile, CoreError, PathKind, Result};
use std::path::Path;

/// How contents are compared.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum QuickMethod {
    /// Sizes only.
    Size,
    Crc,
    Binary,
    /// Text rules for text files (only unimportant differences = similar),
    /// byte comparison for binary ones.
    #[default]
    Rules,
}

impl QuickMethod {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "size" => Some(QuickMethod::Size),
            "crc" => Some(QuickMethod::Crc),
            "binary" => Some(QuickMethod::Binary),
            "rules" | "rules-based" => Some(QuickMethod::Rules),
            _ => None,
        }
    }

    fn content(self) -> ContentMethod {
        match self {
            QuickMethod::Size => ContentMethod::Off,
            QuickMethod::Crc => ContentMethod::Crc,
            QuickMethod::Binary => ContentMethod::Binary,
            QuickMethod::Rules => ContentMethod::Rules,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Byte for byte (or, for a size compare, size for size).
    Same,
    /// The same text: only the encoding or line endings differ.
    RulesSame,
    /// Only unimportant differences (rules-based).
    Similar,
    Different,
}

/// Exit code: binary same.
pub const EXIT_BINARY_SAME: i32 = 1;
/// Exit code: rules-based same.
pub const EXIT_RULES_SAME: i32 = 2;
/// Exit code: binary differences.
pub const EXIT_BINARY_DIFFERENT: i32 = 11;
/// Exit code: similar (only unimportant differences).
pub const EXIT_SIMILAR: i32 = 12;
/// Exit code: rules-based differences.
pub const EXIT_RULES_DIFFERENT: i32 = 13;
/// Exit code: the comparison failed.
pub const EXIT_ERROR: i32 = 100;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuickResult {
    pub verdict: Verdict,
    pub method: QuickMethod,
    /// One line for the console, e.g. "Different: 3 differing files, 1 left orphan".
    pub message: String,
}

impl QuickResult {
    /// The quick compare exit code (see the module docs).
    pub fn exit_code(&self) -> i32 {
        let rules = self.method == QuickMethod::Rules;
        match self.verdict {
            Verdict::Same => EXIT_BINARY_SAME,
            Verdict::RulesSame => EXIT_RULES_SAME,
            Verdict::Similar => EXIT_SIMILAR,
            Verdict::Different if rules => EXIT_RULES_DIFFERENT,
            Verdict::Different => EXIT_BINARY_DIFFERENT,
        }
    }
}

/// The default text rules for a rules-based quick compare:
/// whitespace changes are unimportant (similar, exit 12), not differences.
fn rules() -> TextDiffOptions {
    TextDiffOptions {
        whitespace: pcompare_core::text::WhitespaceMode::IgnoreChanges,
        ..TextDiffOptions::default()
    }
}

fn plural(n: usize, word: &str) -> String {
    format!("{n} {word}{}", if n == 1 { "" } else { "s" })
}

pub fn quick_compare(left: &Path, right: &Path, method: QuickMethod) -> Result<QuickResult> {
    let missing = |p: &Path| CoreError::Other(format!("{}: no such file or folder", p.display()));
    let (lk, rk) = (
        pcompare_core::path_kind(left),
        pcompare_core::path_kind(right),
    );
    match (lk, rk) {
        (PathKind::Missing, _) => Err(missing(left)),
        (_, PathKind::Missing) => Err(missing(right)),
        (PathKind::File, PathKind::File) => files(left, right, method),
        (PathKind::Directory, PathKind::Directory) => folders(left, right, method),
        _ => Ok(QuickResult {
            verdict: Verdict::Different,
            method,
            message: "Different: a file and a folder".into(),
        }),
    }
}

fn files(left: &Path, right: &Path, method: QuickMethod) -> Result<QuickResult> {
    let size = |p: &Path| {
        std::fs::metadata(p)
            .map(|m| m.len())
            .map_err(|e| CoreError::io(p, e))
    };
    let (ls, rs) = (size(left)?, size(right)?);
    let status = if method == QuickMethod::Size {
        if ls == rs {
            folder::EntryStatus::Same
        } else {
            folder::EntryStatus::Different
        }
    } else {
        let (l, r) = (left.to_string_lossy(), right.to_string_lossy());
        folder::compare_contents(
            &LocalFs,
            &l,
            ls,
            &LocalFs,
            &r,
            rs,
            method.content(),
            &rules(),
        )?
        .status
    };
    let (verdict, message) = match status {
        folder::EntryStatus::Same => (Verdict::Same, "Same".to_owned()),
        folder::EntryStatus::Similar if !has_unimportant(left, right)? => (
            Verdict::RulesSame,
            "Same text: only the encoding or line endings differ".to_owned(),
        ),
        folder::EntryStatus::Similar => (
            Verdict::Similar,
            "Similar: only unimportant differences".to_owned(),
        ),
        _ if ls != rs => (Verdict::Different, format!("Different: {ls} vs {rs} bytes")),
        _ => (Verdict::Different, "Different".to_owned()),
    };
    Ok(QuickResult {
        verdict,
        method,
        message,
    })
}

/// Whether two texts that compare as similar have unimportant lines
/// (rather than differing only in encoding or line endings).
fn has_unimportant(left: &Path, right: &Path) -> Result<bool> {
    let (l, r) = (textfile::load_text(left)?, textfile::load_text(right)?);
    let name = |p: &Path| p.file_name().map(|n| n.to_string_lossy().into_owned());
    let diff = pcompare_core::text::diff_named_text(
        &l.text,
        name(left).as_deref(),
        &r.text,
        name(right).as_deref(),
        &rules(),
    )?;
    Ok(diff.stats.unimportant > 0)
}

fn folders(left: &Path, right: &Path, method: QuickMethod) -> Result<QuickResult> {
    let opts = FolderCompareOptions {
        compare_size: true,
        compare_timestamp: false,
        content: method.content(),
        rules: rules(),
        ..Default::default()
    };
    let r = folder::compare_folders(left, right, &opts)?;
    let s = &r.stats;
    if s.errors > 0 {
        return Err(CoreError::Other(format!(
            "{} could not be compared",
            plural(s.errors, "item")
        )));
    }
    let files = s.same + s.similar + s.different + s.left_only + s.right_only;
    if s.different + s.left_only + s.right_only > 0 {
        return Ok(QuickResult {
            verdict: Verdict::Different,
            method,
            message: format!(
                "Different: {} {}, {}, {} (of {})",
                plural(s.different, "file"),
                if s.different == 1 {
                    "differs"
                } else {
                    "differ"
                },
                plural(s.left_only, "left orphan"),
                plural(s.right_only, "right orphan"),
                plural(files, "file")
            ),
        });
    }
    if s.similar > 0 {
        return Ok(QuickResult {
            verdict: Verdict::Similar,
            method,
            message: format!(
                "Similar: {} with only unimportant differences (of {})",
                plural(s.similar, "file"),
                plural(files, "file")
            ),
        });
    }
    Ok(QuickResult {
        verdict: Verdict::Same,
        method,
        message: format!("Same ({})", plural(files, "file")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files_by_method() {
        let d = tempfile::tempdir().unwrap();
        let (a, b, c) = (
            d.path().join("a.txt"),
            d.path().join("b.txt"),
            d.path().join("c.txt"),
        );
        std::fs::write(&a, "x\r\ny\r\n").unwrap();
        std::fs::write(&b, "x\ny\n").unwrap();
        std::fs::write(&c, "x\nz\n").unwrap();
        // Line endings don't matter to the text rules, they do byte-wise.
        let rules = quick_compare(&a, &b, QuickMethod::Rules).unwrap();
        assert_eq!((rules.verdict, rules.exit_code()), (Verdict::RulesSame, 2));
        let bin = quick_compare(&a, &b, QuickMethod::Binary).unwrap();
        assert_eq!((bin.verdict, bin.exit_code()), (Verdict::Different, 11));
        let diff = quick_compare(&a, &c, QuickMethod::Rules).unwrap();
        assert_eq!(diff.exit_code(), 13);
        let ws = d.path().join("ws.txt");
        std::fs::write(&ws, "x  \ny\n").unwrap();
        let sim = quick_compare(&b, &ws, QuickMethod::Rules).unwrap();
        assert_eq!((sim.verdict, sim.exit_code()), (Verdict::Similar, 12));
        assert_eq!(quick_compare(&b, &b, QuickMethod::Rules).unwrap().exit_code(), 1);
        assert_eq!(
            quick_compare(&b, &c, QuickMethod::Size).unwrap().verdict,
            Verdict::Same
        );
        assert_eq!(
            quick_compare(&b, &c, QuickMethod::Crc).unwrap().verdict,
            Verdict::Different
        );
        assert_eq!(
            quick_compare(&b, &b, QuickMethod::Crc).unwrap().exit_code(),
            1
        );
        assert!(quick_compare(&a, &d.path().join("nope"), QuickMethod::Rules).is_err());
        assert_eq!(
            quick_compare(&a, d.path(), QuickMethod::Rules)
                .unwrap()
                .verdict,
            Verdict::Different
        );
    }

    #[test]
    fn folders_summarize_their_differences() {
        let (l, r) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        std::fs::write(l.path().join("same.txt"), "1").unwrap();
        std::fs::write(r.path().join("same.txt"), "1").unwrap();
        let same = quick_compare(l.path(), r.path(), QuickMethod::Binary).unwrap();
        assert_eq!(
            (same.verdict, same.message.as_str()),
            (Verdict::Same, "Same (1 file)")
        );
        std::fs::write(l.path().join("only.txt"), "2").unwrap();
        let diff = quick_compare(l.path(), r.path(), QuickMethod::Binary).unwrap();
        assert_eq!(
            diff.message,
            "Different: 0 files differ, 1 left orphan, 0 right orphans (of 2 files)"
        );
    }
}
