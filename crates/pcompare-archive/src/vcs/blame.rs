//! Who last changed each line: `git blame` for a working-tree file (as
//! currently edited) or for a file at a revision.
//!
//! Mercurial isn't covered yet: `hg annotate` has no equivalent of
//! `--contents`, so unsaved edits couldn't be blamed line for line.

use std::collections::HashMap;
use std::io::{self, Write};
use std::path::Path;
use std::process::Stdio;

use pcompare_core::{CoreError, Result};
use serde::Serialize;

use super::{command, failure, locate, spawn_err, VcsKind, VcsPath};

/// A commit some lines were last changed in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BlameCommit {
    /// Full hash.
    pub id: String,
    pub short: String,
    pub author: String,
    pub time_ms: i64,
    /// First line of the message.
    pub summary: String,
    /// The file's path in this commit, from the repository root
    /// (`/`-separated); differs from today's after a rename.
    pub path: String,
    /// The parent commit and the file's path there; `None` when the commit
    /// added the file (or is a shallow clone's boundary).
    pub previous: Option<BlamePrevious>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BlamePrevious {
    pub id: String,
    pub path: String,
}

/// Shape (JSON): `{ "root": "/src/app", "commits": [...], "lines": [0, 0, null, 1] }`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Blame {
    /// The repository's working-tree root, for building `git:` paths to
    /// a commit's version of the file.
    pub root: String,
    pub commits: Vec<BlameCommit>,
    /// Per line (0-based): an index into `commits`, or `None` for a line
    /// that isn't committed yet.
    pub lines: Vec<Option<u32>>,
}

const UNCOMMITTED: &str = "0000000000000000000000000000000000000000";

fn is_header(line: &str) -> Option<(&str, usize)> {
    let mut f = line.split(' ');
    let id = f.next()?;
    if !matches!(id.len(), 40 | 64) || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let _orig: usize = f.next()?.parse().ok()?;
    let fin: usize = f.next()?.parse().ok()?;
    Some((id, fin))
}

/// Parses `git blame --porcelain`. Commit details come once, after a
/// commit's first header; every line's header names its commit and final
/// line number.
pub fn parse_git_blame(out: &str, root: String) -> Blame {
    let mut index: HashMap<String, u32> = HashMap::new();
    let mut commits: Vec<BlameCommit> = Vec::new();
    let mut lines: Vec<Option<u32>> = Vec::new();
    // The commit the current header belongs to (`None`: not committed).
    let mut cur: Option<u32> = None;
    for line in out.split('\n') {
        if line.starts_with('\t') {
            continue;
        }
        if let Some((id, fin)) = is_header(line) {
            cur = if id.starts_with(UNCOMMITTED) {
                None
            } else {
                Some(*index.entry(id.to_owned()).or_insert_with(|| {
                    commits.push(BlameCommit {
                        id: id.to_owned(),
                        short: id[..7].to_owned(),
                        author: String::new(),
                        time_ms: 0,
                        summary: String::new(),
                        path: String::new(),
                        previous: None,
                    });
                    (commits.len() - 1) as u32
                }))
            };
            if fin >= 1 {
                if lines.len() < fin {
                    lines.resize(fin, None);
                }
                lines[fin - 1] = cur;
            }
            continue;
        }
        let Some(c) = cur.map(|i| &mut commits[i as usize]) else {
            continue;
        };
        let (key, value) = line.split_once(' ').unwrap_or((line, ""));
        match key {
            "author" => c.author = value.to_owned(),
            "author-time" => c.time_ms = value.trim().parse::<i64>().unwrap_or(0) * 1000,
            "summary" => c.summary = value.to_owned(),
            "filename" => c.path = value.to_owned(),
            "previous" => {
                if let Some((id, path)) = value.split_once(' ') {
                    c.previous = Some(BlamePrevious {
                        id: id.to_owned(),
                        path: path.to_owned(),
                    });
                }
            }
            _ => {}
        }
    }
    Blame {
        root,
        commits,
        lines,
    }
}

/// Runs `git blame` in `root` on `path` (repository-relative), optionally
/// at `rev` or with `contents` standing in for the working-tree file.
fn git_blame(root: &Path, path: &str, rev: Option<&str>, contents: Option<&str>) -> io::Result<Blame> {
    let kind = VcsKind::Git;
    let mut cmd = command(kind, root);
    // -M: lines moved within the file keep the commit that wrote them.
    cmd.args(["blame", "--porcelain", "-M"]);
    if contents.is_some() {
        cmd.args(["--contents", "-"]);
    }
    if let Some(rev) = rev {
        cmd.arg(rev);
    }
    cmd.arg("--").arg(path);
    cmd.stdin(if contents.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    })
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| spawn_err(kind, e))?;
    // Written from a thread so a large file can't deadlock against git's
    // own output filling the pipe.
    let feeder = contents.zip(child.stdin.take()).map(|(text, mut stdin)| {
        let text = text.to_owned();
        std::thread::spawn(move || stdin.write_all(text.as_bytes()))
    });
    let out = child.wait_with_output()?;
    if let Some(f) = feeder {
        // git may stop reading early on an error; its stderr says why.
        let _ = f.join();
    }
    if !out.status.success() {
        return Err(failure(kind, &out.stderr));
    }
    Ok(parse_git_blame(
        &String::from_utf8_lossy(&out.stdout),
        root.to_string_lossy().into_owned(),
    ))
}

fn not_git(kind: VcsKind) -> CoreError {
    CoreError::Other(format!(
        "Blame works with Git only; {} isn't supported yet",
        kind.name()
    ))
}

/// Blames a working-tree file. With `contents` (the file as edited, not
/// yet saved) lines are matched against that text, so edits show as
/// not committed and line numbers stay right.
pub fn blame_working(file: &str, contents: Option<&str>) -> Result<Blame> {
    let fail = |e: io::Error| CoreError::remote(file.to_owned(), e);
    let (root, path) = locate(VcsKind::Git, file).map_err(|e| {
        if e.kind() == io::ErrorKind::NotFound {
            CoreError::Other(format!("{file} isn't inside a Git working tree"))
        } else {
            fail(e)
        }
    })?;
    if path.is_empty() {
        return Err(CoreError::Other(format!("{file} is the repository itself, not a file")));
    }
    git_blame(&root, &path, None, contents).map_err(fail)
}

/// Blames a `git:` file at its revision.
pub fn blame_rev(p: &VcsPath) -> Result<Blame> {
    if p.kind != VcsKind::Git {
        return Err(not_git(p.kind));
    }
    let fail = |e: io::Error| CoreError::remote(p.label(), e);
    if p.rev.starts_with('-') {
        return Err(CoreError::Other(format!("bad revision `{}`", p.rev)));
    }
    let (root, prefix) = locate(p.kind, &p.location).map_err(fail)?;
    let path = match (prefix.is_empty(), p.inner.is_empty()) {
        (_, true) => prefix,
        (true, false) => p.inner.clone(),
        (false, false) => format!("{prefix}/{}", p.inner),
    };
    git_blame(&root, &path, Some(&p.rev), None).map_err(fail)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PORCELAIN: &str = "\
1111111111111111111111111111111111111111 1 1 2
author Alice
author-mail <a@example.com>
author-time 1700000000
author-tz +0000
committer Alice
summary First version
boundary
filename src/a.txt
\tone
1111111111111111111111111111111111111111 2 2
\ttwo
0000000000000000000000000000000000000000 3 3 1
author Not Committed Yet
author-time 1800000000
summary Version of a.txt from a.txt
previous 1111111111111111111111111111111111111111 src/a.txt
filename src/a.txt
\tedited
2222222222222222222222222222222222222222 3 4 1
author Bob
author-time 1710000000
summary Rename and extend
previous 1111111111111111111111111111111111111111 src/old.txt
filename src/a.txt
\tfour
";

    #[test]
    fn parses_porcelain() {
        let b = parse_git_blame(PORCELAIN, "/repo".into());
        assert_eq!(b.lines, [Some(0), Some(0), None, Some(1)]);
        assert_eq!(b.commits.len(), 2);
        let (a, bob) = (&b.commits[0], &b.commits[1]);
        assert_eq!((a.short.as_str(), a.author.as_str()), ("1111111", "Alice"));
        assert_eq!(a.time_ms, 1_700_000_000_000);
        assert_eq!(a.summary, "First version");
        assert_eq!(a.previous, None);
        assert_eq!(
            bob.previous,
            Some(BlamePrevious {
                id: "1".repeat(40),
                path: "src/old.txt".into()
            })
        );
    }
}
