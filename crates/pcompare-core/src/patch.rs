//! Patch files: parsing unified diffs (plain, `diff -ruN` and git's, with
//! renames, copies, mode changes, new/deleted and binary files) and context
//! diffs, laying their hunks out as side-by-side rows for the Patch Viewer,
//! and applying them to the real files of a folder the way `patch` does
//! (searching nearby when lines moved, dropping outer context lines as fuzz).

use crate::text::{inline_diff, DiffRow, RowKind, Segment};
use crate::textfile::{self, LineEnding};
use crate::{CoreError, Result};
use serde::Serialize;
use std::path::{Component, Path, PathBuf};

/// The most outer context lines a hunk may lose to still apply (`patch -F2`).
const MAX_FUZZ: usize = 2;
/// Strip levels tried when detecting how a patch's paths map onto a folder.
const MAX_STRIP: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum LineKind {
    Context,
    Removed,
    Added,
}

/// One line of a hunk body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HunkLine {
    pub kind: LineKind,
    pub text: String,
    /// Followed by `\ No newline at end of file`.
    pub no_newline: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PatchHunk {
    /// 1-based first line of the hunk in the old file (0 when the old side is empty).
    pub old_start: usize,
    pub old_lines: usize,
    pub new_start: usize,
    pub new_lines: usize,
    /// Text after the closing `@@` (often the enclosing function).
    pub section: String,
    pub lines: Vec<HunkLine>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ChangeKind {
    Modified,
    Added,
    Deleted,
    Renamed,
    Copied,
}

/// Everything a patch says about one file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PatchFile {
    /// Paths as written in the patch (prefixes like `a/` included); `None`
    /// for `/dev/null`.
    pub old_path: Option<String>,
    pub new_path: Option<String>,
    pub change: ChangeKind,
    /// Binary contents (`Binary files … differ` or a `GIT binary patch`).
    pub binary: bool,
    pub old_mode: Option<String>,
    pub new_mode: Option<String>,
    /// git's similarity index of a rename or copy, in percent.
    pub similarity: Option<u32>,
    /// Header lines between the file's start and its first hunk, as written.
    pub headers: Vec<String>,
    pub hunks: Vec<PatchHunk>,
    pub added: usize,
    pub removed: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum PatchFormat {
    Unified,
    Git,
    Context,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Patch {
    pub format: PatchFormat,
    /// Text before the first file: a commit message, mail headers, a `diff` command line.
    pub preamble: String,
    pub files: Vec<PatchFile>,
    /// Leading path components to drop by default (`-p1` for git's `a/`, `b/`).
    pub strip: usize,
    /// Hunks that ended early (the patch looks truncated or hand-edited).
    pub warnings: Vec<String>,
}

// ---------- parsing ----------

/// Parses a unified or context diff. Fails only when no file changes are found.
pub fn parse_patch(text: &str) -> Result<Patch> {
    let mut p = Parser {
        lines: text.lines().collect(),
        i: 0,
        files: Vec::new(),
        warnings: Vec::new(),
        preamble: Vec::new(),
        format: PatchFormat::Unified,
    };
    p.run();
    if p.files.is_empty() {
        return Err(CoreError::Other(
            "no file changes found: this does not look like a patch".into(),
        ));
    }
    let strip = default_strip(&p.files);
    let mut preamble = p.preamble.join("\n");
    if !preamble.trim().is_empty() {
        preamble.push('\n');
    } else {
        preamble.clear();
    }
    Ok(Patch {
        format: p.format,
        preamble,
        files: p.files,
        strip,
        warnings: p.warnings,
    })
}

struct Parser<'a> {
    lines: Vec<&'a str>,
    i: usize,
    files: Vec<PatchFile>,
    warnings: Vec<String>,
    preamble: Vec<&'a str>,
    format: PatchFormat,
}

fn new_file() -> PatchFile {
    PatchFile {
        old_path: None,
        new_path: None,
        change: ChangeKind::Modified,
        binary: false,
        old_mode: None,
        new_mode: None,
        similarity: None,
        headers: Vec::new(),
        hunks: Vec::new(),
        added: 0,
        removed: 0,
    }
}

impl<'a> Parser<'a> {
    fn peek(&self, k: usize) -> Option<&'a str> {
        self.lines.get(self.i + k).copied()
    }

    fn run(&mut self) {
        // Lines like `Index: x` or `diff -ruN a b` that introduce the next file.
        let mut pending: Vec<String> = Vec::new();
        while let Some(line) = self.peek(0) {
            if line.starts_with("diff --git ") {
                self.format = PatchFormat::Git;
                self.git_file(std::mem::take(&mut pending));
            } else if is_unified_header(line, self.peek(1)) {
                let mut f = new_file();
                f.headers = std::mem::take(&mut pending);
                self.unified_paths(&mut f);
                self.unified_hunks(&mut f);
                self.finish(f, false);
            } else if is_context_header(line, self.peek(1), self.peek(2)) {
                self.format = PatchFormat::Context;
                let mut f = new_file();
                f.headers = std::mem::take(&mut pending);
                f.old_path = header_path(&line[4..]);
                f.new_path = header_path(&self.peek(1).unwrap_or_default()[4..]);
                self.i += 2;
                self.context_hunks(&mut f);
                self.finish(f, false);
            } else if let Some((old, new)) = binary_differ(line) {
                let mut f = new_file();
                f.headers = std::mem::take(&mut pending);
                f.headers.push(line.to_owned());
                f.old_path = old;
                f.new_path = new;
                f.binary = true;
                self.i += 1;
                self.finish(f, false);
            } else {
                if self.files.is_empty() && pending.is_empty() && !is_file_intro(line) {
                    self.preamble.push(line);
                } else if is_file_intro(line) {
                    pending.push(line.to_owned());
                }
                self.i += 1;
            }
        }
    }

    /// `diff --git a/x b/y`, its extended headers, then optional `---`/`+++` and hunks.
    fn git_file(&mut self, intro: Vec<String>) {
        let line = self.peek(0).unwrap_or_default();
        let mut f = new_file();
        f.headers = intro;
        f.headers.push(line.to_owned());
        if let Some((a, b)) = split_git_paths(&line["diff --git ".len()..]) {
            f.old_path = Some(a);
            f.new_path = Some(b);
        }
        self.i += 1;
        while let Some(l) = self.peek(0) {
            if is_unified_header(l, self.peek(1))
                || l.starts_with("diff --git ")
                || l.starts_with("@@ ")
            {
                break;
            }
            let value = |prefix: &str| l.strip_prefix(prefix).map(str::to_owned);
            if let Some(m) = value("old mode ") {
                f.old_mode = Some(m);
            } else if let Some(m) = value("new mode ") {
                f.new_mode = Some(m);
            } else if let Some(m) = value("deleted file mode ") {
                f.change = ChangeKind::Deleted;
                f.old_mode = Some(m);
                f.new_path = None;
            } else if let Some(m) = value("new file mode ") {
                f.change = ChangeKind::Added;
                f.new_mode = Some(m);
                f.old_path = None;
            } else if let Some(s) = value("similarity index ") {
                f.similarity = s.trim_end_matches('%').parse().ok();
            } else if let Some(p) = l.strip_prefix("rename from ") {
                f.change = ChangeKind::Renamed;
                f.old_path = Some(with_prefix(&f.old_path, p));
            } else if let Some(p) = l.strip_prefix("rename to ") {
                f.change = ChangeKind::Renamed;
                f.new_path = Some(with_prefix(&f.new_path, p));
            } else if let Some(p) = l.strip_prefix("copy from ") {
                f.change = ChangeKind::Copied;
                f.old_path = Some(with_prefix(&f.old_path, p));
            } else if let Some(p) = l.strip_prefix("copy to ") {
                f.change = ChangeKind::Copied;
                f.new_path = Some(with_prefix(&f.new_path, p));
            } else if l.starts_with("index ") || l.starts_with("dissimilarity index ") {
                // Recorded as a header line below.
            } else if binary_differ(l).is_some() {
                f.binary = true;
            } else if l == "GIT binary patch" {
                f.binary = true;
                f.headers.push(l.to_owned());
                self.i += 1;
                // Base85 `literal`/`delta` blocks run up to the next file.
                while self.peek(0).is_some_and(|l| !l.starts_with("diff --git ")) {
                    self.i += 1;
                }
                break;
            } else {
                break;
            }
            f.headers.push(l.to_owned());
            self.i += 1;
        }
        // Renames, copies and new/deleted files already know their paths from
        // the extended headers; `---`/`+++` confirm them (with git's prefixes).
        if self
            .peek(0)
            .is_some_and(|l| is_unified_header(l, self.peek(1)))
        {
            let (old, new) = (f.old_path.clone(), f.new_path.clone());
            self.unified_paths(&mut f);
            if f.change == ChangeKind::Renamed || f.change == ChangeKind::Copied {
                f.old_path = f.old_path.or(old);
                f.new_path = f.new_path.or(new);
            }
        }
        self.unified_hunks(&mut f);
        self.finish(f, true);
    }

    /// Reads the `---` / `+++` pair.
    fn unified_paths(&mut self, f: &mut PatchFile) {
        let old = self.peek(0).unwrap_or_default();
        let new = self.peek(1).unwrap_or_default();
        f.headers.push(old.to_owned());
        f.headers.push(new.to_owned());
        f.old_path = header_path(&old[4..]);
        f.new_path = header_path(&new[4..]);
        self.i += 2;
    }

    fn unified_hunks(&mut self, f: &mut PatchFile) {
        while let Some(line) = self.peek(0) {
            let Some(h) = parse_unified_range(line) else {
                break;
            };
            self.i += 1;
            f.hunks.push(self.unified_body(h));
        }
    }

    /// Reads hunk lines until both sides' line counts are used up.
    fn unified_body(&mut self, mut h: PatchHunk) -> PatchHunk {
        let (mut old_left, mut new_left) = (h.old_lines, h.new_lines);
        while old_left > 0 || new_left > 0 {
            let Some(line) = self.peek(0) else { break };
            let (kind, text) = match line.as_bytes().first() {
                Some(b' ') => (LineKind::Context, &line[1..]),
                // Some mailers and editors strip the space of empty context lines.
                None => (LineKind::Context, ""),
                Some(b'-') => (LineKind::Removed, &line[1..]),
                Some(b'+') => (LineKind::Added, &line[1..]),
                Some(b'\\') => {
                    mark_no_newline(&mut h.lines);
                    self.i += 1;
                    continue;
                }
                _ => break,
            };
            match kind {
                LineKind::Context => {
                    old_left = old_left.saturating_sub(1);
                    new_left = new_left.saturating_sub(1);
                }
                LineKind::Removed => old_left = old_left.saturating_sub(1),
                LineKind::Added => new_left = new_left.saturating_sub(1),
            }
            h.lines.push(HunkLine {
                kind,
                text: text.to_owned(),
                no_newline: false,
            });
            self.i += 1;
        }
        if old_left > 0 || new_left > 0 {
            self.warnings.push(format!(
                "hunk @@ -{},{} +{},{} @@ ends early (patch truncated?)",
                h.old_start, h.old_lines, h.new_start, h.new_lines
            ));
        }
        if self.peek(0).is_some_and(|l| l.starts_with('\\')) {
            mark_no_newline(&mut h.lines);
            self.i += 1;
        }
        h
    }

    /// Context-diff hunks: `***************`, `*** a,b ****` + old lines, `--- c,d ----` + new lines.
    fn context_hunks(&mut self, f: &mut PatchFile) {
        while self
            .peek(0)
            .is_some_and(|l| l.starts_with("***************"))
        {
            self.i += 1;
            let Some((old_start, old_end)) =
                self.peek(0).and_then(|l| context_range(l, "*** ", " ****"))
            else {
                break;
            };
            self.i += 1;
            let old = self.context_section(b" -!");
            let Some((new_start, new_end)) =
                self.peek(0).and_then(|l| context_range(l, "--- ", " ----"))
            else {
                break;
            };
            self.i += 1;
            let new = self.context_section(b" +!");
            let count = |start: usize, end: usize| {
                if start == 0 {
                    0
                } else {
                    end + 1 - start.min(end + 1)
                }
            };
            f.hunks.push(PatchHunk {
                old_start,
                old_lines: count(old_start, old_end),
                new_start,
                new_lines: count(new_start, new_end),
                section: String::new(),
                lines: merge_context_sections(old, new),
            });
        }
    }

    /// One side of a context hunk: lines tagged with one of `tags` and a space.
    fn context_section(&mut self, tags: &[u8]) -> Vec<(u8, String, bool)> {
        let mut out: Vec<(u8, String, bool)> = Vec::new();
        while let Some(line) = self.peek(0) {
            if context_range(line, "--- ", " ----").is_some() || line.starts_with("***************")
            {
                break;
            }
            let b = line.as_bytes();
            if b.first() == Some(&b'\\') {
                if let Some(last) = out.last_mut() {
                    last.2 = true;
                }
            } else if b.len() >= 2 && tags.contains(&b[0]) && b[1] == b' ' {
                out.push((b[0], line[2..].to_owned(), false));
            } else if line.is_empty() || (b.len() == 1 && tags.contains(&b[0])) {
                out.push((b.first().copied().unwrap_or(b' '), String::new(), false));
            } else {
                break;
            }
            self.i += 1;
        }
        out
    }

    fn finish(&mut self, mut f: PatchFile, git: bool) {
        for h in &f.hunks {
            for l in &h.lines {
                match l.kind {
                    LineKind::Added => f.added += 1,
                    LineKind::Removed => f.removed += 1,
                    LineKind::Context => {}
                }
            }
        }
        if !git && f.change == ChangeKind::Modified {
            // `diff -N` writes real paths for absent files; their hunks are all at line 0.
            let all =
                |pred: fn(&PatchHunk) -> bool| !f.hunks.is_empty() && f.hunks.iter().all(pred);
            if f.old_path.is_none() || all(|h| h.old_start == 0 && h.old_lines == 0) {
                f.change = ChangeKind::Added;
            } else if f.new_path.is_none() || all(|h| h.new_start == 0 && h.new_lines == 0) {
                f.change = ChangeKind::Deleted;
            }
        }
        self.files.push(f);
    }
}

/// A `rename from`/`copy to` path (written without git's `a/`/`b/` prefix),
/// kept as the `diff --git` line's prefixed spelling when that names it.
fn with_prefix(git_path: &Option<String>, raw: &str) -> String {
    let p = unquote(raw);
    match git_path.as_deref().and_then(|g| g.strip_suffix(p.as_str())) {
        Some(prefix) if prefix.ends_with('/') => format!("{prefix}{p}"),
        _ => p,
    }
}

fn mark_no_newline(lines: &mut [HunkLine]) {
    if let Some(last) = lines.last_mut() {
        last.no_newline = true;
    }
}

/// Lines that introduce a file without being part of it (svn, hg, `diff -r`).
fn is_file_intro(line: &str) -> bool {
    line.starts_with("Index: ")
        || line.starts_with("diff ")
        || (line.len() >= 10 && line.bytes().all(|b| b == b'='))
}

fn is_unified_header(line: &str, next: Option<&str>) -> bool {
    line.starts_with("--- ") && next.is_some_and(|n| n.starts_with("+++ "))
}

fn is_context_header(line: &str, next: Option<&str>, after: Option<&str>) -> bool {
    line.starts_with("*** ")
        && next.is_some_and(|n| n.starts_with("--- "))
        && after.is_some_and(|a| a.starts_with("***************"))
}

/// `Binary files a/x and b/y differ` → the two paths.
fn binary_differ(line: &str) -> Option<(Option<String>, Option<String>)> {
    let rest = line
        .strip_prefix("Binary files ")?
        .strip_suffix(" differ")?;
    let (a, b) = rest.split_once(" and ")?;
    Some((header_path(a), header_path(b)))
}

/// A path from a `---`/`+++`/`***` line: the timestamp after a tab is
/// dropped, quotes are undone, and `/dev/null` means "no file".
fn header_path(raw: &str) -> Option<String> {
    let raw = raw.split('\t').next().unwrap_or_default().trim_end();
    let path = unquote(raw);
    (path != "/dev/null" && !path.is_empty()).then_some(path)
}

/// Undoes git's C-style quoting (`"a/sp\303\244ce"`); unquoted text is returned as is.
fn unquote(s: &str) -> String {
    let Some(inner) = s.strip_prefix('"').and_then(|s| s.strip_suffix('"')) else {
        return s.to_owned();
    };
    let mut bytes = Vec::with_capacity(inner.len());
    let mut it = inner.bytes().peekable();
    while let Some(b) = it.next() {
        if b != b'\\' {
            bytes.push(b);
            continue;
        }
        match it.next() {
            Some(b'n') => bytes.push(b'\n'),
            Some(b't') => bytes.push(b'\t'),
            Some(b'r') => bytes.push(b'\r'),
            Some(b'a') => bytes.push(7),
            Some(b'b') => bytes.push(8),
            Some(b'f') => bytes.push(12),
            Some(b'v') => bytes.push(11),
            Some(d @ b'0'..=b'7') => {
                let mut v = (d - b'0') as u32;
                for _ in 0..2 {
                    match it.peek() {
                        Some(&n @ b'0'..=b'7') => {
                            v = v * 8 + (n - b'0') as u32;
                            it.next();
                        }
                        _ => break,
                    }
                }
                bytes.push(v as u8);
            }
            Some(other) => bytes.push(other),
            None => bytes.push(b'\\'),
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// The two paths of a `diff --git` line. Unquoted paths may contain spaces,
/// so the line is split where both halves name the same file.
fn split_git_paths(rest: &str) -> Option<(String, String)> {
    if let Some(stripped) = rest.strip_prefix('"') {
        let end = quoted_end(stripped)?;
        let a = unquote(&rest[..end + 2]);
        let b = rest[end + 2..].trim_start();
        return Some((a, unquote(b)));
    }
    if let Some(pos) = rest.find(" \"") {
        return Some((rest[..pos].to_owned(), unquote(&rest[pos + 1..])));
    }
    let n = rest.len();
    if n % 2 == 1 {
        let mid = n / 2;
        if rest.as_bytes()[mid] == b' ' {
            let (a, b) = (&rest[..mid], &rest[mid + 1..]);
            let tail = |p: &str| {
                p.split_once('/')
                    .map(|(_, t)| t.to_owned())
                    .unwrap_or_else(|| p.to_owned())
            };
            if a == b || tail(a) == tail(b) {
                return Some((a.to_owned(), b.to_owned()));
            }
        }
    }
    let pos = rest.rfind(" b/").or_else(|| rest.find(' '))?;
    Some((rest[..pos].to_owned(), rest[pos + 1..].to_owned()))
}

/// Byte index (within `s`, after the opening quote) of the closing quote.
fn quoted_end(s: &str) -> Option<usize> {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'\\' => i += 2,
            b'"' => return Some(i),
            _ => i += 1,
        }
    }
    None
}

/// `@@ -a,b +c,d @@ section` (counts default to 1).
fn parse_unified_range(line: &str) -> Option<PatchHunk> {
    let rest = line.strip_prefix("@@ -")?;
    let (ranges, section) = rest.split_once(" @@")?;
    let (old, new) = ranges.split_once(" +")?;
    let range = |r: &str| -> Option<(usize, usize)> {
        match r.split_once(',') {
            Some((s, c)) => Some((s.parse().ok()?, c.parse().ok()?)),
            None => Some((r.parse().ok()?, 1)),
        }
    };
    let (old_start, old_lines) = range(old)?;
    let (new_start, new_lines) = range(new)?;
    Some(PatchHunk {
        old_start,
        old_lines,
        new_start,
        new_lines,
        section: section.trim().to_owned(),
        lines: Vec::new(),
    })
}

/// `*** 3,8 ****` → (3, 8); a single number means a one-line range.
fn context_range(line: &str, prefix: &str, suffix: &str) -> Option<(usize, usize)> {
    let inner = line.strip_prefix(prefix)?.strip_suffix(suffix)?;
    match inner.split_once(',') {
        Some((a, b)) => Some((a.trim().parse().ok()?, b.trim().parse().ok()?)),
        None => {
            let a: usize = inner.trim().parse().ok()?;
            Some((a, a))
        }
    }
}

/// Interleaves a context hunk's old and new sections into unified lines. A
/// section left out (only context on that side) is the other side's context.
fn merge_context_sections(
    old: Vec<(u8, String, bool)>,
    new: Vec<(u8, String, bool)>,
) -> Vec<HunkLine> {
    let context_of = |side: &[(u8, String, bool)]| -> Vec<(u8, String, bool)> {
        side.iter().filter(|l| l.0 == b' ').cloned().collect()
    };
    let (old, new) = match (old.is_empty(), new.is_empty()) {
        (true, false) => (context_of(&new), new),
        (false, true) => {
            let n = context_of(&old);
            (old, n)
        }
        _ => (old, new),
    };
    let line = |kind, l: &(u8, String, bool)| HunkLine {
        kind,
        text: l.1.clone(),
        no_newline: l.2,
    };
    let (mut a, mut b) = (0, 0);
    let mut out = Vec::new();
    loop {
        while a < old.len() && old[a].0 != b' ' {
            out.push(line(LineKind::Removed, &old[a]));
            a += 1;
        }
        while b < new.len() && new[b].0 != b' ' {
            out.push(line(LineKind::Added, &new[b]));
            b += 1;
        }
        match (old.get(a), new.get(b)) {
            (Some(o), Some(n)) => {
                // A context line ends both files only when both sides say so.
                out.push(HunkLine {
                    kind: LineKind::Context,
                    text: o.1.clone(),
                    no_newline: o.2 && n.2,
                });
                a += 1;
                b += 1;
            }
            (Some(o), None) => {
                out.push(line(LineKind::Context, o));
                a += 1;
            }
            (None, Some(n)) => {
                out.push(line(LineKind::Context, n));
                b += 1;
            }
            (None, None) => break,
        }
    }
    out
}

fn components(path: &str) -> Vec<&str> {
    path.split(['/', '\\'])
        .filter(|c| !c.is_empty() && *c != ".")
        .collect()
}

/// `-p1` when every file carries git's `a/`/`b/` prefixes or old and new
/// paths start in different top folders (`diff -ruN old/ new/`); else `-p0`.
fn default_strip(files: &[PatchFile]) -> usize {
    let prefixed = files.iter().all(|f| {
        f.old_path.as_deref().is_none_or(|p| p.starts_with("a/"))
            && f.new_path.as_deref().is_none_or(|p| p.starts_with("b/"))
    });
    if prefixed {
        return 1;
    }
    let pairs: Vec<(Vec<&str>, Vec<&str>)> = files
        .iter()
        .filter_map(|f| {
            Some((
                components(f.old_path.as_deref()?),
                components(f.new_path.as_deref()?),
            ))
        })
        .collect();
    let split_roots = !pairs.is_empty()
        && pairs
            .iter()
            .all(|(a, b)| a.len() >= 2 && b.len() >= 2 && a[0] != b[0] && a[1..] == b[1..]);
    usize::from(split_roots)
}

/// `path` without its first `strip` components, or `None` when it has no more.
pub fn strip_path(path: &str, strip: usize) -> Option<String> {
    let parts = components(path);
    (parts.len() > strip).then(|| parts[strip..].join("/"))
}

impl PatchFile {
    /// The path shown for the file: the new name, or the old one for deletions.
    pub fn display_path(&self, strip: usize) -> String {
        let p = self
            .new_path
            .as_deref()
            .or(self.old_path.as_deref())
            .unwrap_or_default();
        strip_path(p, strip).unwrap_or_else(|| p.to_owned())
    }
}

// ---------- side-by-side rows ----------

/// A hunk as Text Compare rows: context on both sides, and each run of
/// removed and added lines paired up line by line with word highlights.
pub fn hunk_rows(h: &PatchHunk) -> Vec<DiffRow> {
    let mut rows = Vec::with_capacity(h.lines.len());
    let (mut old_no, mut new_no) = (h.old_start.max(1), h.new_start.max(1));
    let row = |kind, l: Option<(usize, Vec<Segment>)>, r: Option<(usize, Vec<Segment>)>| {
        let (left_no, left) = l.map_or((None, None), |(n, s)| (Some(n), Some(s)));
        let (right_no, right) = r.map_or((None, None), |(n, s)| (Some(n), Some(s)));
        DiffRow {
            kind,
            left_no,
            right_no,
            left,
            right,
            left_spans: Vec::new(),
            right_spans: Vec::new(),
            left_eol: None,
            right_eol: None,
        }
    };
    let whole = |text: &str, changed: bool| {
        vec![Segment {
            text: text.to_owned(),
            changed,
        }]
    };
    let mut i = 0;
    while i < h.lines.len() {
        if h.lines[i].kind == LineKind::Context {
            let t = &h.lines[i].text;
            rows.push(row(
                RowKind::Equal,
                Some((old_no, whole(t, false))),
                Some((new_no, whole(t, false))),
            ));
            old_no += 1;
            new_no += 1;
            i += 1;
            continue;
        }
        let start = i;
        while i < h.lines.len() && h.lines[i].kind != LineKind::Context {
            i += 1;
        }
        let block = &h.lines[start..i];
        let removed: Vec<&str> = block
            .iter()
            .filter(|l| l.kind == LineKind::Removed)
            .map(|l| l.text.as_str())
            .collect();
        let added: Vec<&str> = block
            .iter()
            .filter(|l| l.kind == LineKind::Added)
            .map(|l| l.text.as_str())
            .collect();
        for k in 0..removed.len().max(added.len()) {
            match (removed.get(k), added.get(k)) {
                (Some(a), Some(b)) => {
                    let (l, r) = inline_diff(a, b);
                    rows.push(row(RowKind::Changed, Some((old_no, l)), Some((new_no, r))));
                    old_no += 1;
                    new_no += 1;
                }
                (Some(a), None) => {
                    rows.push(row(RowKind::LeftOnly, Some((old_no, whole(a, true))), None));
                    old_no += 1;
                }
                (None, Some(b)) => {
                    rows.push(row(
                        RowKind::RightOnly,
                        None,
                        Some((new_no, whole(b, true))),
                    ));
                    new_no += 1;
                }
                (None, None) => {}
            }
        }
    }
    rows
}

// ---------- applying ----------

/// What happened to one hunk when applying it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HunkOutcome {
    pub applied: bool,
    /// 1-based line of the original file where the hunk's lines were found
    /// (where they were expected, for a failed hunk).
    pub line: usize,
    /// How far the hunk moved from where its header placed it.
    pub offset: isize,
    /// Outer context lines that had to be ignored.
    pub fuzz: usize,
    /// A failed hunk whose new text is already there: the patch looks applied.
    pub already_applied: bool,
}

/// The result of applying a file's hunks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Applied {
    pub lines: Vec<String>,
    pub trailing_newline: bool,
    pub hunks: Vec<HunkOutcome>,
}

impl Applied {
    pub fn failed(&self) -> usize {
        self.hunks.iter().filter(|h| !h.applied).count()
    }
}

/// Searches `lines` for `pattern` starting at `from`, trying positions in
/// order of distance from `expected` (as `patch` does).
fn find_block(lines: &[&str], pattern: &[&str], from: usize, expected: usize) -> Option<usize> {
    let last = lines.len().checked_sub(pattern.len())?;
    if from > last {
        return None;
    }
    let expected = expected.clamp(from, last);
    let fits = |p: usize| lines[p..p + pattern.len()] == *pattern;
    for d in 0..=last - from {
        let after = expected + d;
        if after <= last && fits(after) {
            return Some(after);
        }
        if d > 0 && expected >= from + d && fits(expected - d) {
            return Some(expected - d);
        }
        if after > last && expected < from + d {
            break;
        }
    }
    None
}

/// Applies `hunks` to a file's lines. Hunks that don't fit are skipped and
/// reported; the rest still apply, in order and without overlapping.
pub fn apply_hunks(original: &[&str], trailing_newline: bool, hunks: &[PatchHunk]) -> Applied {
    let mut out: Vec<String> = Vec::with_capacity(original.len());
    let mut outcomes = Vec::with_capacity(hunks.len());
    let mut pos = 0;
    // Like `patch`, a hunk found away from its stated line shifts the guesses for the next ones.
    let mut drift: isize = 0;
    let mut trailing = trailing_newline;
    for h in hunks {
        let old: Vec<&str> = h
            .lines
            .iter()
            .filter(|l| l.kind != LineKind::Added)
            .map(|l| l.text.as_str())
            .collect();
        let new: Vec<&str> = h
            .lines
            .iter()
            .filter(|l| l.kind != LineKind::Removed)
            .map(|l| l.text.as_str())
            .collect();
        let lead = h
            .lines
            .iter()
            .take_while(|l| l.kind == LineKind::Context)
            .count();
        let trail = h
            .lines
            .iter()
            .rev()
            .take_while(|l| l.kind == LineKind::Context)
            .count()
            .min(h.lines.len() - lead);
        let stated = if h.old_lines == 0 {
            h.old_start
        } else {
            h.old_start.saturating_sub(1)
        };
        let guess = (stated as isize + drift).max(0) as usize;

        let mut found = None;
        for fuzz in 0..=MAX_FUZZ.min(lead.max(trail)) {
            let (cut_lead, cut_trail) = (fuzz.min(lead), fuzz.min(trail));
            if fuzz > 0 && cut_lead + cut_trail == 0 {
                continue;
            }
            let pattern = &old[cut_lead..old.len() - cut_trail];
            let at = if pattern.is_empty() {
                // Pure insertion without context: trust the header.
                (guess.max(pos) <= original.len()).then_some(guess.max(pos))
            } else {
                find_block(original, pattern, pos, guess + cut_lead)
            };
            if let Some(at) = at {
                found = Some((at, cut_lead, cut_trail, fuzz));
                break;
            }
        }
        let Some((at, cut_lead, cut_trail, fuzz)) = found else {
            let already = new != old
                && (0..=MAX_FUZZ.min(lead.max(trail))).any(|fuzz| {
                    let part = &new[fuzz.min(lead)..new.len() - fuzz.min(trail)];
                    !part.is_empty() && find_block(original, part, 0, guess).is_some()
                });
            outcomes.push(HunkOutcome {
                applied: false,
                line: guess + 1,
                offset: 0,
                fuzz: 0,
                already_applied: already,
            });
            continue;
        };
        let offset = at as isize - (stated + cut_lead) as isize;
        drift = offset;
        out.extend(original[pos..at].iter().map(|s| (*s).to_owned()));
        out.extend(
            new[cut_lead..new.len() - cut_trail]
                .iter()
                .map(|s| (*s).to_owned()),
        );
        pos = at + (old.len() - cut_lead - cut_trail);
        // Hunks that reach the end of the file say whether it ends with a line break.
        if pos == original.len() && cut_trail == 0 {
            if let Some(last_new) = h.lines.iter().rev().find(|l| l.kind != LineKind::Removed) {
                if last_new.no_newline {
                    trailing = false;
                } else if h
                    .lines
                    .iter()
                    .any(|l| l.kind != LineKind::Added && l.no_newline)
                    || !trailing
                {
                    trailing = !out.is_empty();
                }
            }
        }
        outcomes.push(HunkOutcome {
            applied: true,
            line: at + 1 - cut_lead.min(at),
            offset,
            fuzz,
            already_applied: false,
        });
    }
    out.extend(original[pos..].iter().map(|s| (*s).to_owned()));
    if out.is_empty() {
        trailing = false;
    }
    Applied {
        lines: out,
        trailing_newline: trailing,
        hunks: outcomes,
    }
}

// ---------- applying against a folder ----------

/// Where a file of the patch lives under the base folder.
fn target(base: &Path, path: Option<&str>, strip: usize) -> Result<Option<PathBuf>> {
    let Some(path) = path else { return Ok(None) };
    let Some(rel) = strip_path(path, strip) else {
        return Ok(None);
    };
    let rel = Path::new(&rel);
    if rel.components().any(|c| !matches!(c, Component::Normal(_))) {
        return Err(CoreError::Other(format!(
            "{}: refusing a path outside the folder",
            rel.display()
        )));
    }
    Ok(Some(base.join(rel)))
}

/// The strip level under which the most of the patch's existing files are
/// found in `base` (the patch's own default when nothing matches).
pub fn detect_strip(patch: &Patch, base: &Path) -> usize {
    let mut best = (0, patch.strip);
    for strip in 0..=MAX_STRIP {
        let hits = patch
            .files
            .iter()
            .filter(|f| f.change != ChangeKind::Added)
            .filter(|f| {
                target(base, f.old_path.as_deref(), strip)
                    .ok()
                    .flatten()
                    .is_some_and(|p| p.is_file())
            })
            .count();
        if hits > best.0 || (hits == best.0 && hits > 0 && strip == patch.strip) {
            best = (hits, strip);
        }
    }
    best.1
}

/// One file of a patch applied against a folder.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileApply {
    /// The original file, when it exists (absent for new files).
    pub original: Option<String>,
    /// Where the patched file belongs (`None` when the patch deletes it).
    pub target: Option<String>,
    /// The original was expected but is missing.
    pub missing: bool,
    pub hunks: Vec<HunkOutcome>,
    /// Why the file could not be patched at all (missing, binary, unreadable).
    pub error: Option<String>,
    /// The patched text, joined with the original's line endings.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

/// Applies file `index` of `patch` to the files under `base`, read in their
/// detected encodings. `with_text` includes the patched text.
pub fn apply_to_folder(
    patch: &Patch,
    base: &Path,
    strip: usize,
    index: usize,
    with_text: bool,
) -> Result<FileApply> {
    let f = patch
        .files
        .get(index)
        .ok_or_else(|| CoreError::Other(format!("the patch has no file #{}", index + 1)))?;
    let old = if f.change == ChangeKind::Added {
        None
    } else {
        target(base, f.old_path.as_deref(), strip)?
    };
    let new = target(base, f.new_path.as_deref(), strip)?;
    let mut result = FileApply {
        original: None,
        target: new.as_ref().map(|p| p.display().to_string()),
        missing: false,
        hunks: Vec::new(),
        error: None,
        text: None,
    };
    if f.binary {
        result.error = Some("binary changes can't be applied here".into());
        return Ok(result);
    }
    let (text, eol) = match &old {
        Some(path) if path.is_file() => {
            result.original = Some(path.display().to_string());
            let loaded = textfile::load_text(path)?;
            let eol = textfile::save_line_ending(&loaded.text);
            (loaded.text, eol)
        }
        Some(path) => {
            result.missing = true;
            result.error = Some(format!("{} not found", path.display()));
            return Ok(result);
        }
        None => (String::new(), LineEnding::Lf),
    };
    let (lines, eols) = textfile::split_lines_with_endings(&text);
    let trailing = eols.last().is_some_and(|e| *e != LineEnding::None);
    let applied = apply_hunks(&lines, trailing, &f.hunks);
    if with_text {
        let sep = match eol {
            LineEnding::Crlf => "\r\n",
            LineEnding::Cr => "\r",
            _ => "\n",
        };
        let mut joined = applied.lines.join(sep);
        if applied.trailing_newline {
            joined.push_str(sep);
        }
        result.text = Some(joined);
    }
    result.hunks = applied.hunks;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIT: &str = "From 1234 Mon Sep 17 00:00:00 2001
Subject: [PATCH] demo

---
diff --git a/src/main.c b/src/main.c
index 83db48f..bf269f4 100644
--- a/src/main.c
+++ b/src/main.c
@@ -1,4 +1,4 @@ int main()
 #include <stdio.h>
-int x = 1;
+int x = 2;
 int y;
 int z;
@@ -10,3 +10,4 @@ void f()
 a
 b
 c
+d
diff --git a/old name.txt b/new name.txt
similarity index 90%
rename from old name.txt
rename to new name.txt
index 1111111..2222222 100644
--- a/old name.txt
+++ b/new name.txt
@@ -1 +1 @@
-hello
+hello world
diff --git a/added.txt b/added.txt
new file mode 100644
index 0000000..3333333
--- /dev/null
+++ b/added.txt
@@ -0,0 +1,2 @@
+one
+two
\\ No newline at end of file
diff --git a/gone.txt b/gone.txt
deleted file mode 100644
index 4444444..0000000
--- a/gone.txt
+++ /dev/null
@@ -1 +0,0 @@
-bye
diff --git a/img.png b/img.png
index 5555555..6666666 100644
Binary files a/img.png and b/img.png differ
diff --git a/run.sh b/run.sh
old mode 100644
new mode 100755
--
2.40.0
";

    #[test]
    fn parses_git_patches() {
        let p = parse_patch(GIT).unwrap();
        assert_eq!(p.format, PatchFormat::Git);
        assert_eq!(p.strip, 1);
        assert!(p.preamble.contains("Subject: [PATCH] demo"));
        assert_eq!(p.files.len(), 6);

        let main = &p.files[0];
        assert_eq!(main.old_path.as_deref(), Some("a/src/main.c"));
        assert_eq!(main.display_path(1), "src/main.c");
        assert_eq!(main.change, ChangeKind::Modified);
        assert_eq!((main.added, main.removed), (2, 1));
        assert_eq!(main.hunks.len(), 2);
        assert_eq!(main.hunks[0].section, "int main()");
        assert_eq!((main.hunks[1].old_start, main.hunks[1].new_lines), (10, 4));

        let ren = &p.files[1];
        assert_eq!(ren.change, ChangeKind::Renamed);
        assert_eq!(ren.old_path.as_deref(), Some("a/old name.txt"));
        assert_eq!(ren.new_path.as_deref(), Some("b/new name.txt"));
        assert_eq!(ren.similarity, Some(90));

        let add = &p.files[2];
        assert_eq!(add.change, ChangeKind::Added);
        assert_eq!(add.old_path, None);
        assert!(add.hunks[0].lines[1].no_newline);

        assert_eq!(p.files[3].change, ChangeKind::Deleted);
        assert_eq!(p.files[3].new_path, None);
        assert!(p.files[4].binary);
        let mode = &p.files[5];
        assert_eq!(
            (mode.old_mode.as_deref(), mode.new_mode.as_deref()),
            (Some("100644"), Some("100755"))
        );
        assert!(mode.hunks.is_empty());
        assert!(p.warnings.is_empty(), "{:?}", p.warnings);
    }

    #[test]
    fn parses_plain_unified_and_quoted_paths() {
        let text = "diff -ruN old/a.txt new/a.txt
--- old/a.txt\t2024-01-01 10:00:00.000000000 +0000
+++ new/a.txt\t2024-01-02 10:00:00.000000000 +0000
@@ -2,2 +2,3 @@
-b
+B

+c
--- \"old/sp\\303\\244ce.txt\"
+++ \"new/sp\\303\\244ce.txt\"
@@ -1 +1 @@
-x
+y
";
        let p = parse_patch(text).unwrap();
        assert_eq!(p.format, PatchFormat::Unified);
        assert_eq!(p.strip, 1, "old/ and new/ roots");
        assert_eq!(p.files.len(), 2);
        assert_eq!(p.files[0].headers[0], "diff -ruN old/a.txt new/a.txt");
        assert_eq!(p.files[0].old_path.as_deref(), Some("old/a.txt"));
        // An empty line is context; count-less ranges are one line long.
        assert_eq!(p.files[0].hunks[0].lines.len(), 4);
        assert_eq!(p.files[0].hunks[0].lines[2].kind, LineKind::Context);
        assert_eq!(p.files[1].hunks[0].old_lines, 1);
        assert_eq!(p.files[1].display_path(1), "späce.txt");
    }

    #[test]
    fn splits_git_paths_with_spaces() {
        assert_eq!(
            split_git_paths("a/x y.c b/x y.c"),
            Some(("a/x y.c".into(), "b/x y.c".into()))
        );
        assert_eq!(
            split_git_paths("a/old b/new"),
            Some(("a/old".into(), "b/new".into()))
        );
        assert_eq!(
            split_git_paths("\"a/t\\tab\" \"b/t\\tab\""),
            Some(("a/t\tab".into(), "b/t\tab".into()))
        );
    }

    #[test]
    fn parses_context_diffs() {
        let text = "*** a.c\t2024-01-01
--- b.c\t2024-01-02
***************
*** 1,4 ****
  one
! two
  three
- four
--- 1,4 ----
  one
! TWO
  three
+ five
***************
*** 10,11 ****
--- 10,12 ----
  ten
+ ten and a half
  eleven
";
        let p = parse_patch(text).unwrap();
        assert_eq!(p.format, PatchFormat::Context);
        let f = &p.files[0];
        assert_eq!(
            (f.old_path.as_deref(), f.new_path.as_deref()),
            (Some("a.c"), Some("b.c"))
        );
        assert_eq!(f.hunks.len(), 2);
        let kinds: Vec<(LineKind, &str)> = f.hunks[0]
            .lines
            .iter()
            .map(|l| (l.kind, l.text.as_str()))
            .collect();
        assert_eq!(
            kinds,
            vec![
                (LineKind::Context, "one"),
                (LineKind::Removed, "two"),
                (LineKind::Added, "TWO"),
                (LineKind::Context, "three"),
                (LineKind::Removed, "four"),
                (LineKind::Added, "five"),
            ]
        );
        // The omitted old section is the new section's context.
        let h = &f.hunks[1];
        assert_eq!((h.old_start, h.old_lines, h.new_lines), (10, 2, 3));
        assert_eq!(
            h.lines
                .iter()
                .filter(|l| l.kind == LineKind::Context)
                .count(),
            2
        );
        assert_eq!((f.added, f.removed), (3, 2));
    }

    #[test]
    fn rejects_text_without_changes() {
        assert!(parse_patch("just some notes\n").is_err());
    }

    #[test]
    fn reports_truncated_hunks() {
        let p = parse_patch("--- a\n+++ b\n@@ -1,3 +1,3 @@\n x\n-y\n").unwrap();
        assert_eq!(p.warnings.len(), 1);
    }

    #[test]
    fn lays_hunks_out_side_by_side() {
        let p = parse_patch(GIT).unwrap();
        let rows = hunk_rows(&p.files[0].hunks[0]);
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[1].kind, RowKind::Changed);
        assert_eq!((rows[1].left_no, rows[1].right_no), (Some(2), Some(2)));
        assert!(rows[1]
            .left
            .as_ref()
            .unwrap()
            .iter()
            .any(|s| s.changed && s.text.contains('1')));
        let rows = hunk_rows(&p.files[0].hunks[1]);
        assert_eq!(rows[3].kind, RowKind::RightOnly);
        assert_eq!((rows[3].left_no, rows[3].right_no), (None, Some(13)));
    }

    fn hunks(text: &str) -> Vec<PatchHunk> {
        parse_patch(text).unwrap().files.remove(0).hunks
    }

    #[test]
    fn applies_at_stated_lines_and_with_offset() {
        let h =
            hunks("--- a\n+++ b\n@@ -2,3 +2,3 @@\n b\n-c\n+C\n d\n@@ -6,2 +6,3 @@\n f\n g\n+h\n");
        let original = ["a", "b", "c", "d", "e", "f", "g"];
        let r = apply_hunks(&original, true, &h);
        assert_eq!(r.lines, ["a", "b", "C", "d", "e", "f", "g", "h"]);
        assert!(r.trailing_newline);
        assert_eq!(r.failed(), 0);
        assert_eq!((r.hunks[0].line, r.hunks[0].offset), (2, 0));

        // Two lines inserted at the top: both hunks are found 2 lines down.
        let shifted = ["x", "y", "a", "b", "c", "d", "e", "f", "g"];
        let r = apply_hunks(&shifted, true, &h);
        assert_eq!(r.lines, ["x", "y", "a", "b", "C", "d", "e", "f", "g", "h"]);
        assert_eq!((r.hunks[0].offset, r.hunks[1].offset), (2, 2));
    }

    #[test]
    fn applies_with_fuzz_and_reports_failures() {
        let h = hunks("--- a\n+++ b\n@@ -1,5 +1,5 @@\n a\n b\n-c\n+C\n d\n e\n@@ -8,3 +8,3 @@\n x\n-y\n+Y\n z\n");
        // The outer context lines changed: fuzz 1 drops "a" and "e".
        let original = ["A", "b", "c", "d", "E", "f", "g", "h", "i", "j"];
        let r = apply_hunks(&original, false, &h);
        assert!(r.hunks[0].applied);
        assert_eq!(r.hunks[0].fuzz, 1);
        assert!(!r.hunks[1].applied, "x/y/z is nowhere in the file");
        assert_eq!(r.failed(), 1);
        assert_eq!(r.lines, ["A", "b", "C", "d", "E", "f", "g", "h", "i", "j"]);
        assert!(!r.trailing_newline);

        // Applying again: the first hunk is already there.
        let again: Vec<&str> = r.lines.iter().map(String::as_str).collect();
        let r2 = apply_hunks(&again, false, &h[..1]);
        assert!(!r2.hunks[0].applied);
        assert!(r2.hunks[0].already_applied);
    }

    #[test]
    fn handles_missing_newline_markers_and_new_files() {
        let h = hunks("--- a\n+++ b\n@@ -1,2 +1,2 @@\n a\n-b\n\\ No newline at end of file\n+b\n");
        let r = apply_hunks(&["a", "b"], false, &h);
        assert_eq!(r.lines, ["a", "b"]);
        assert!(r.trailing_newline, "the new side ends with a line break");

        let p = parse_patch(GIT).unwrap();
        let r = apply_hunks(&[], false, &p.files[2].hunks);
        assert_eq!(r.lines, ["one", "two"]);
        assert!(!r.trailing_newline);
        let r = apply_hunks(&["bye"], true, &p.files[3].hunks);
        assert!(r.lines.is_empty());
    }

    #[test]
    fn applies_against_a_folder() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        let main: String = (1..=12)
            .map(|n| match n {
                1 => "#include <stdio.h>\r\n".to_owned(),
                2 => "int x = 1;\r\n".to_owned(),
                3 => "int y;\r\n".to_owned(),
                4 => "int z;\r\n".to_owned(),
                10 => "a\r\n".to_owned(),
                11 => "b\r\n".to_owned(),
                12 => "c\r\n".to_owned(),
                n => format!("line {n}\r\n"),
            })
            .collect();
        std::fs::write(dir.path().join("src/main.c"), main).unwrap();
        std::fs::write(dir.path().join("old name.txt"), "hello\n").unwrap();
        let p = parse_patch(GIT).unwrap();
        assert_eq!(detect_strip(&p, dir.path()), 1);

        let r = apply_to_folder(&p, dir.path(), 1, 0, true).unwrap();
        assert_eq!(r.hunks.iter().filter(|h| h.applied).count(), 2);
        let text = r.text.unwrap();
        assert!(
            text.starts_with("#include <stdio.h>\r\nint x = 2;\r\n"),
            "{text:?}"
        );
        assert!(text.ends_with("c\r\nd\r\n"));

        let r = apply_to_folder(&p, dir.path(), 1, 1, true).unwrap();
        assert!(r.original.unwrap().ends_with("old name.txt"));
        assert!(r.target.unwrap().ends_with("new name.txt"));
        assert_eq!(r.text.as_deref(), Some("hello world\n"));

        let r = apply_to_folder(&p, dir.path(), 1, 2, true).unwrap();
        assert_eq!((r.original, r.text.as_deref()), (None, Some("one\ntwo")));

        let r = apply_to_folder(&p, dir.path(), 1, 3, false).unwrap();
        assert!(r.missing);
        assert!(r.error.is_some());
        assert!(
            apply_to_folder(&p, dir.path(), 1, 4, false)
                .unwrap()
                .error
                .is_some(),
            "binary"
        );

        let evil = parse_patch("--- a/../x\n+++ b/../x\n@@ -1 +1 @@\n-a\n+b\n").unwrap();
        assert!(apply_to_folder(&evil, dir.path(), 1, 0, true).is_err());
    }
}
