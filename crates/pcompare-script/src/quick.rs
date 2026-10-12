//! Headless quick compare (`pcompare /qc left right`): decides whether two
//! files or folders are the same without opening a window.

use pcompare_core::folder::{self, ContentMethod, FolderCompareOptions};
use pcompare_core::text::TextDiffOptions;
use pcompare_core::vfs::LocalFs;
use pcompare_core::{CoreError, PathKind, Result};
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
    Same,
    /// Only unimportant differences (rules-based).
    Similar,
    Different,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuickResult {
    pub verdict: Verdict,
    /// One line for the console, e.g. "Different: 3 differing files, 1 left orphan".
    pub message: String,
}

impl QuickResult {
    /// 0 = same (or similar), 1 = different.
    pub fn exit_code(&self) -> i32 {
        match self.verdict {
            Verdict::Same | Verdict::Similar => 0,
            Verdict::Different => 1,
        }
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
            &TextDiffOptions::default(),
        )?
        .status
    };
    Ok(match status {
        folder::EntryStatus::Same => QuickResult {
            verdict: Verdict::Same,
            message: "Same".into(),
        },
        folder::EntryStatus::Similar => QuickResult {
            verdict: Verdict::Similar,
            message: "Similar: only unimportant differences".into(),
        },
        _ if ls != rs => QuickResult {
            verdict: Verdict::Different,
            message: format!("Different: {ls} vs {rs} bytes"),
        },
        _ => QuickResult {
            verdict: Verdict::Different,
            message: "Different".into(),
        },
    })
}

fn folders(left: &Path, right: &Path, method: QuickMethod) -> Result<QuickResult> {
    let opts = FolderCompareOptions {
        compare_size: true,
        compare_timestamp: false,
        content: method.content(),
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
            message: format!(
                "Similar: {} with only unimportant differences (of {})",
                plural(s.similar, "file"),
                plural(files, "file")
            ),
        });
    }
    Ok(QuickResult {
        verdict: Verdict::Same,
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
        // Line endings are unimportant under the text rules, not byte-wise.
        assert_eq!(
            quick_compare(&a, &b, QuickMethod::Rules).unwrap().verdict,
            Verdict::Similar
        );
        let bin = quick_compare(&a, &b, QuickMethod::Binary).unwrap();
        assert_eq!((bin.verdict, bin.exit_code()), (Verdict::Different, 1));
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
            0
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
