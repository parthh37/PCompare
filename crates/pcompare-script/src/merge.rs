//! Headless text merge (`pcompare left right base output /automerge`), the
//! way version control systems call Beyond Compare as a merge tool.
//!
//! - Without conflicts the merge is written to the output and the exit code
//!   is 0.
//! - With conflicts the output is left alone (exit 101) unless `/force`
//!   writes it with conflict markers (exit 14); `/reviewconflicts` opens the
//!   merge in the app instead (the caller decides that from
//!   [`Merged::conflicts`]).
//! - `/iu` ignores unimportant (whitespace) differences; changes that are
//!   only unimportant are taken from the favored side (`/favorleft`,
//!   `/favorright`) or else from the base.

use pcompare_core::text::{TextDiffOptions, WhitespaceMode};
use pcompare_core::textfile;
use pcompare_merge::{ChunkKind, MergeResult};
use std::path::Path;

/// Beyond Compare's exit code: the merge had conflicts (written with
/// markers because of `/force`).
pub const EXIT_CONFLICTS: i32 = 14;
/// Beyond Compare's exit code: the merge had conflicts and the output was
/// not written.
pub const EXIT_CONFLICTS_NOT_WRITTEN: i32 = 101;
/// Beyond Compare's exit code for errors (unreadable files, …).
pub const EXIT_ERROR: i32 = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Favor {
    #[default]
    None,
    Left,
    Right,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AutoMerge {
    pub left: String,
    pub right: String,
    /// The common ancestor (`/center=`, the third path); without one every
    /// difference is a conflict.
    pub base: Option<String>,
    pub output: String,
    /// Write conflicts with markers instead of refusing.
    pub force: bool,
    pub ignore_unimportant: bool,
    pub favor: Favor,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Merged {
    pub conflicts: usize,
    /// Changes taken without a conflict.
    pub merged: usize,
    pub written: bool,
}

impl Merged {
    pub fn exit_code(&self) -> i32 {
        match (self.conflicts, self.written) {
            (0, _) => 0,
            (_, true) => EXIT_CONFLICTS,
            (_, false) => EXIT_CONFLICTS_NOT_WRITTEN,
        }
    }

    pub fn message(&self, output: &str) -> String {
        match (self.conflicts, self.written) {
            (0, _) => format!("Merged {} change(s) into {output}", self.merged),
            (n, true) => format!("{n} conflict(s) written to {output} with conflict markers"),
            (n, false) => format!("{n} conflict(s): {output} was not written (use /force to write them with markers)"),
        }
    }
}

fn load(path: &str) -> Result<textfile::LoadedText, String> {
    textfile::load_text(Path::new(path)).map_err(|e| format!("{path}: {e}"))
}

/// The merged lines: one side per chunk by kind and favor; `None` for an
/// unresolved conflict when `force` is off.
fn render(
    m: &MergeResult,
    base: &[String],
    left: &[String],
    right: &[String],
    favor: Favor,
    iu: bool,
    force: bool,
) -> Option<Vec<String>> {
    let mut out = Vec::new();
    let take = |out: &mut Vec<String>, lines: &[String], r: pcompare_merge::LineRange| {
        out.extend(lines[r.start..r.end].iter().cloned());
    };
    for c in &m.chunks {
        match c.kind {
            // Equal up to unimportant differences: whose text is kept.
            ChunkKind::Stable => match favor {
                Favor::Left => take(&mut out, left, c.left),
                Favor::Right => take(&mut out, right, c.right),
                Favor::None if iu && !base.is_empty() => take(&mut out, base, c.base),
                Favor::None => take(&mut out, left, c.left),
            },
            ChunkKind::BothSame => match favor {
                Favor::Right => take(&mut out, right, c.right),
                _ => take(&mut out, left, c.left),
            },
            ChunkKind::LeftChanged => take(&mut out, left, c.left),
            ChunkKind::RightChanged => take(&mut out, right, c.right),
            ChunkKind::Conflict if !force => return None,
            ChunkKind::Conflict => {
                let n = c.conflict.unwrap_or_default();
                out.push(pcompare_merge::conflict_start_marker(n));
                take(&mut out, left, c.left);
                out.push(pcompare_merge::CONFLICT_SEPARATOR.to_owned());
                take(&mut out, right, c.right);
                out.push(pcompare_merge::conflict_end_marker(n));
            }
        }
    }
    Some(out)
}

/// Merges and (unless conflicts block it) writes the output in the left
/// file's encoding and line endings.
pub fn auto_merge(a: &AutoMerge) -> Result<Merged, String> {
    let (l, r) = (load(&a.left)?, load(&a.right)?);
    let b = a.base.as_deref().map(load).transpose()?;
    let opts = TextDiffOptions {
        whitespace: if a.ignore_unimportant {
            WhitespaceMode::IgnoreChanges
        } else {
            WhitespaceMode::Compare
        },
        ..TextDiffOptions::default()
    };
    let (ll, lt) = pcompare_merge::split_lines(&l.text);
    let (rl, rt) = pcompare_merge::split_lines(&r.text);
    let (bl, trailing, result) = match &b {
        Some(b) => {
            let (bl, bt) = pcompare_merge::split_lines(&b.text);
            let m = pcompare_merge::merge3(&bl, &ll, &rl, &opts).map_err(|e| e.to_string())?;
            (bl, pcompare_merge::merge_trailing_newline(bt, lt, rt), m)
        }
        None => (
            Vec::new(),
            lt,
            pcompare_merge::merge2(&ll, &rl, &opts).map_err(|e| e.to_string())?,
        ),
    };
    let conflicts = result.stats.conflicts;
    let merged = result.stats.auto_merged();
    let Some(lines) = render(&result, &bl, &ll, &rl, a.favor, a.ignore_unimportant, a.force) else {
        return Ok(Merged {
            conflicts,
            merged,
            written: false,
        });
    };
    let text = pcompare_merge::join_lines(&lines, trailing);
    let out = Path::new(&a.output);
    if let Some(dir) = out.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    textfile::save_text(out, &text, &l.format()).map_err(|e| format!("{}: {e}", a.output))?;
    Ok(Merged {
        conflicts,
        merged,
        written: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Files {
        dir: tempfile::TempDir,
    }

    impl Files {
        fn new(base: &str, left: &str, right: &str) -> Self {
            let dir = tempfile::tempdir().unwrap();
            for (n, t) in [("base", base), ("left", left), ("right", right)] {
                std::fs::write(dir.path().join(n), t).unwrap();
            }
            Files { dir }
        }

        fn p(&self, n: &str) -> String {
            self.dir.path().join(n).to_string_lossy().into_owned()
        }

        fn merge(&self, f: impl FnOnce(&mut AutoMerge)) -> (Merged, Option<String>) {
            let mut a = AutoMerge {
                left: self.p("left"),
                right: self.p("right"),
                base: Some(self.p("base")),
                output: self.p("out/merged"),
                ..Default::default()
            };
            f(&mut a);
            let m = auto_merge(&a).unwrap();
            (m, std::fs::read_to_string(self.p("out/merged")).ok())
        }
    }

    #[test]
    fn clean_merges_are_written_and_exit_0() {
        let f = Files::new("a\nb\nc\n", "A\nb\nc\n", "a\nb\nC\n");
        let (m, out) = f.merge(|_| {});
        assert_eq!((m.conflicts, m.exit_code()), (0, 0));
        assert_eq!(out.as_deref(), Some("A\nb\nC\n"));
    }

    #[test]
    fn conflicts_need_force_to_be_written() {
        let f = Files::new("a\nb\n", "L\nb\n", "R\nb\n");
        let (m, out) = f.merge(|_| {});
        assert_eq!((m.conflicts, m.exit_code(), out), (1, 101, None));
        let (m, out) = f.merge(|a| a.force = true);
        assert_eq!(m.exit_code(), 14);
        assert_eq!(
            out.as_deref(),
            Some("<<<<<<< Left #1\nL\n=======\nR\n>>>>>>> Right #1\nb\n")
        );
        assert!(m.message("x").contains("conflict markers"));
    }

    #[test]
    fn ignore_unimportant_takes_the_favored_side() {
        // Both sides only re-indent line 1 differently; the right also changes line 2.
        let f = Files::new("x = 1\ny\n", "x  = 1\ny\n", "x =  1\nY\n");
        let (m, _) = f.merge(|_| {});
        assert_eq!(m.exit_code(), 101);
        let (m, out) = f.merge(|a| a.ignore_unimportant = true);
        assert_eq!(m.exit_code(), 0);
        assert_eq!(out.as_deref(), Some("x = 1\nY\n"));
        let (_, out) = f.merge(|a| {
            a.ignore_unimportant = true;
            a.favor = Favor::Right;
        });
        assert_eq!(out.as_deref(), Some("x =  1\nY\n"));
        let (_, out) = f.merge(|a| {
            a.ignore_unimportant = true;
            a.favor = Favor::Left;
        });
        assert_eq!(out.as_deref(), Some("x  = 1\nY\n"));
    }

    #[test]
    fn crlf_and_two_way_merges() {
        let f = Files::new("a\r\nb\r\n", "a\r\nB\r\n", "a\r\nb\r\n");
        let (_, out) = f.merge(|_| {});
        assert_eq!(out.as_deref(), Some("a\r\nB\r\n"));
        let (m, _) = f.merge(|a| a.base = None);
        assert_eq!(m.conflicts, 1);
        assert!(auto_merge(&AutoMerge {
            left: f.p("nope"),
            ..Default::default()
        })
        .is_err());
    }
}
