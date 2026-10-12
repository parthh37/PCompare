//! Git and Mercurial revisions as read-only folders and files, through the
//! `git` and `hg` command-line clients (araxis:vcs).
//!
//! A path is `git:<working-tree path>[@<revision>][/<path inside>]` (or
//! `hg:` …), e.g. `git:/src/app/main.c@HEAD~1` (one file one commit back),
//! `git:C:\src\app@v1.2` (the whole tree at a tag) or
//! `hg:/src/app@tip/docs`. The working-tree path may be any file or folder
//! inside a clone — it needn't exist any more, as long as a folder above it
//! does. Without a revision, `HEAD` (git) or `.` (hg, the working copy's
//! parent) is used.
//!
//! Folder Compare appends child paths to the root it was given, so the
//! revision ends at a path separator unless it is a ref with slashes in it
//! (`origin/main`, `feature/x`): those are recognised by asking the client
//! which prefix names a revision ([`split_rev`]). A working-tree path that
//! contains `@` needs an explicit revision (`git:/x/img@2x.png@HEAD`).
//!
//! Trees are listed once per revision (`git ls-tree -r -l`, `hg files -v`);
//! files are read with `git cat-file --filters` (so line-ending and smudge
//! filters apply, as in a checkout) or `hg cat`. Every entry carries the
//! commit's date.

use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, ChildStdout, Command, Output, Stdio};
use std::sync::{Arc, Mutex};

use pcompare_core::folder::EntryMeta;
use pcompare_core::vfs::{Vfs, VfsEntry};
use pcompare_core::vfs_write::{VfsWrite, WriteCaps};
use pcompare_core::{CoreError, Result};
use serde::{Deserialize, Serialize};

use crate::tree::{self, normalize, Loc, Tree};

/// The `git:` path prefix.
pub const GIT_PREFIX: &str = "git:";
/// The `hg:` path prefix.
pub const HG_PREFIX: &str = "hg:";

/// Which client a path goes through.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum VcsKind {
    Git,
    Hg,
}

impl VcsKind {
    /// The kind a path's prefix names, if any.
    pub fn of(path: &str) -> Option<Self> {
        [VcsKind::Git, VcsKind::Hg].into_iter().find(|k| {
            path.get(..k.prefix().len())
                .is_some_and(|p| p.eq_ignore_ascii_case(k.prefix()))
        })
    }

    pub fn prefix(self) -> &'static str {
        match self {
            VcsKind::Git => GIT_PREFIX,
            VcsKind::Hg => HG_PREFIX,
        }
    }

    pub fn program(self) -> &'static str {
        match self {
            VcsKind::Git => "git",
            VcsKind::Hg => "hg",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            VcsKind::Git => "Git",
            VcsKind::Hg => "Mercurial",
        }
    }

    /// The revision used when a path names none.
    pub fn default_rev(self) -> &'static str {
        match self {
            VcsKind::Git => "HEAD",
            VcsKind::Hg => ".",
        }
    }

    /// Why writes are refused.
    pub fn read_only(self) -> &'static str {
        match self {
            VcsKind::Git => "Git revisions are read-only — commit changes with git",
            VcsKind::Hg => "Mercurial revisions are read-only — commit changes with hg",
        }
    }

    fn missing(self) -> String {
        match self {
            VcsKind::Git => "Git's command-line client `git` isn't installed (or isn't on PATH). \
Install Git — e.g. `xcode-select --install` or `brew install git` on macOS, your package manager's \
`git` on Linux, or Git for Windows — to compare git: paths."
                .into(),
            VcsKind::Hg => {
                "Mercurial's command-line client `hg` isn't installed (or isn't on PATH). \
Install Mercurial — e.g. `brew install mercurial` on macOS, your package manager's `mercurial` on \
Linux, or TortoiseHg on Windows — to compare hg: paths."
                    .into()
            }
        }
    }
}

/// Whether a path is a `git:` or `hg:` path.
pub fn is_vcs(path: &str) -> bool {
    VcsKind::of(path).is_some()
}

// ---------- paths ----------

/// A `git:` / `hg:` path split at its revision `@`. Where the revision ends
/// and the path inside begins needs the repository ([`split_rev`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VcsSpec {
    pub kind: VcsKind,
    /// The working-tree file or folder, as written.
    pub location: String,
    /// Everything after the revision `@` (`""` without one).
    pub tail: String,
}

impl VcsSpec {
    pub fn parse(path: &str) -> Result<Self> {
        Self::parse_with(path, |p| Path::new(p).exists())
    }

    /// [`parse`](Self::parse) with `exists` telling whether a working-tree
    /// path is on disk, which decides between several `@`s.
    pub fn parse_with(path: &str, exists: impl Fn(&str) -> bool) -> Result<Self> {
        let kind = VcsKind::of(path).ok_or_else(|| {
            CoreError::Other(format!(
                "`{path}` isn't a git: or hg: path — write git:<path>[@<revision>], e.g. git:/src/app@HEAD~1"
            ))
        })?;
        let body = path[kind.prefix().len()..].trim();
        if body.is_empty() {
            return Err(CoreError::Other(format!(
                "`{path}` names no folder — write {}<path to a clone>[@<revision>]",
                kind.prefix()
            )));
        }
        // `@{…}` belongs to a revision (`HEAD@{2}`), never separates one.
        let ats: Vec<usize> = body
            .match_indices('@')
            .map(|(i, _)| i)
            .filter(|&i| i > 0 && !body[i + 1..].starts_with('{'))
            .collect();
        let at = ats
            .iter()
            .copied()
            .find(|&i| exists(&body[..i]))
            .or_else(|| ats.first().copied());
        let (location, tail) = match at {
            Some(i) => (&body[..i], &body[i + 1..]),
            None => (body, ""),
        };
        Ok(VcsSpec {
            kind,
            location: location.to_owned(),
            tail: tail.to_owned(),
        })
    }
}

/// A fully resolved `git:` / `hg:` path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VcsPath {
    pub kind: VcsKind,
    pub location: String,
    pub rev: String,
    /// The path below `location`, `/`-separated (`""` for `location` itself).
    pub inner: String,
}

impl VcsPath {
    /// A full commit hash never moves, so its listing can be kept.
    pub fn pinned(&self) -> bool {
        is_full_hash(&self.rev)
    }

    /// `git:<location>[@<rev>]` — the root as the user sees it.
    pub fn label(&self) -> String {
        if self.rev == self.kind.default_rev() {
            format!("{}{}", self.kind.prefix(), self.location)
        } else {
            format!("{}{}@{}", self.kind.prefix(), self.location, self.rev)
        }
    }
}

fn is_full_hash(s: &str) -> bool {
    matches!(s.len(), 40 | 64) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// A revision written without slashes for sure: git ref names can't hold
/// `~ ^ :` and hg's `.`, numbers and hashes are never bookmarks with slashes.
fn plain_rev(kind: VcsKind, s: &str) -> bool {
    if s.contains(['~', '^', '(', ')', ':']) || s.contains("@{") {
        return true;
    }
    let hexish = s.len() >= 7 && s.bytes().all(|b| b.is_ascii_hexdigit());
    hexish
        || match kind {
            VcsKind::Git => matches!(
                s,
                "HEAD" | "FETCH_HEAD" | "ORIG_HEAD" | "MERGE_HEAD" | "CHERRY_PICK_HEAD"
            ),
            VcsKind::Hg => s == "." || s == "tip" || s.bytes().all(|b| b.is_ascii_digit()),
        }
}

/// Splits what follows the revision `@` into the revision and the path
/// inside. `known` holds revisions with slashes seen before for this
/// working tree; `resolves` asks the client whether a candidate names a
/// revision (longest first) — only called for refs that might have slashes.
pub fn split_rev(
    kind: VcsKind,
    tail: &str,
    known: &[String],
    mut resolves: impl FnMut(&str) -> bool,
) -> (String, String) {
    let tail = tail.trim_end_matches(['/', '\\']);
    if tail.is_empty() {
        return (kind.default_rev().to_owned(), String::new());
    }
    let seps: Vec<usize> = tail.match_indices(['/', '\\']).map(|(i, _)| i).collect();
    let Some(&first) = seps.first() else {
        return (tail.to_owned(), String::new());
    };
    let rest = |at: usize| tail.get(at + 1..).unwrap_or("").to_owned();
    if plain_rev(kind, &tail[..first]) {
        return (tail[..first].to_owned(), rest(first));
    }
    let mut known: Vec<&String> = known.iter().collect();
    known.sort_by_key(|k| std::cmp::Reverse(k.len()));
    for k in known {
        if tail == k.as_str() {
            return (k.clone(), String::new());
        }
        if tail.starts_with(k.as_str()) && tail[k.len()..].starts_with(['/', '\\']) {
            return (k.clone(), rest(k.len()));
        }
    }
    if resolves(tail) {
        return (tail.to_owned(), String::new());
    }
    for &at in seps.iter().rev() {
        if at == first || resolves(&tail[..at]) {
            return (tail[..at].to_owned(), rest(at));
        }
    }
    (tail[..first].to_owned(), rest(first))
}

/// Anchors a relative working-tree path at `cwd` (a second launch's
/// directory); `None` when `path` isn't a `git:`/`hg:` path or is absolute.
pub fn with_base_dir(path: &str, cwd: &Path) -> Option<String> {
    let kind = VcsKind::of(path)?;
    let body = &path[kind.prefix().len()..];
    if body.is_empty() || Path::new(body).is_absolute() || body.starts_with(['/', '\\']) {
        return None;
    }
    Some(format!(
        "{}{}",
        kind.prefix(),
        cwd.join(body).to_string_lossy()
    ))
}

// ---------- client output ----------

/// What a tree entry is in `git ls-tree`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitObject {
    Blob,
    Tree,
    /// A submodule's commit.
    Commit,
}

/// One record of `git ls-tree -r -t -l -z`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitTreeEntry {
    pub mode: String,
    pub object: GitObject,
    pub size: u64,
    pub path: String,
}

fn bad_output(program: &str, e: impl std::fmt::Display) -> CoreError {
    CoreError::Other(format!("unexpected output from {program}: {e}"))
}

/// Parses `git ls-tree -l -z`: `<mode> <type> <object> <size>\t<path>\0`
/// (size right-aligned, `-` for trees and submodules).
pub fn parse_ls_tree(out: &[u8]) -> Result<Vec<GitTreeEntry>> {
    let mut entries = Vec::new();
    for rec in out.split(|&b| b == 0).filter(|r| !r.is_empty()) {
        let rec = String::from_utf8_lossy(rec);
        let (head, path) = rec
            .split_once('\t')
            .ok_or_else(|| bad_output("git", format!("no tab in `{rec}`")))?;
        let mut fields = head.split_whitespace();
        let (Some(mode), Some(kind), Some(_id), Some(size)) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            return Err(bad_output("git", format!("short record `{rec}`")));
        };
        let object = match kind {
            "blob" => GitObject::Blob,
            "tree" => GitObject::Tree,
            "commit" => GitObject::Commit,
            other => return Err(bad_output("git", format!("object type `{other}`"))),
        };
        entries.push(GitTreeEntry {
            mode: mode.to_owned(),
            object,
            size: size.parse().unwrap_or(0),
            path: path.to_owned(),
        });
    }
    Ok(entries)
}

/// One file of `hg files -v -T json`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct HgFile {
    /// Relative to the repository root.
    pub path: String,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub flags: String,
}

pub fn parse_hg_files(json: &[u8]) -> Result<Vec<HgFile>> {
    if json.iter().all(u8::is_ascii_whitespace) {
        return Ok(Vec::new());
    }
    serde_json::from_slice(json).map_err(|e| bad_output("hg", e))
}

/// A commit, for the revision picker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Revision {
    /// Full hash.
    pub id: String,
    pub short: String,
    /// First line of the message.
    pub summary: String,
    pub author: String,
    pub time_ms: i64,
    /// Branches, tags and bookmarks pointing here.
    pub refs: Vec<String>,
}

/// Parses `git log --format=%H%x1f%h%x1f%s%x1f%an%x1f%ct%x1f%D%x1e`.
pub fn parse_git_log(out: &str) -> Vec<Revision> {
    out.split('\x1e')
        .filter_map(|rec| {
            let f: Vec<&str> = rec.trim_start_matches('\n').split('\x1f').collect();
            let [id, short, summary, author, time, refs] = f.as_slice() else {
                return None;
            };
            Some(Revision {
                id: (*id).to_owned(),
                short: (*short).to_owned(),
                summary: (*summary).to_owned(),
                author: (*author).to_owned(),
                time_ms: time.trim().parse::<i64>().ok()? * 1000,
                refs: refs
                    .split(", ")
                    .map(|r| {
                        r.trim()
                            .trim_start_matches("HEAD -> ")
                            .trim_start_matches("tag: ")
                    })
                    .filter(|r| !r.is_empty())
                    .map(str::to_owned)
                    .collect(),
            })
        })
        .collect()
}

#[derive(Deserialize)]
struct HgLogJson {
    node: String,
    #[serde(default)]
    desc: String,
    #[serde(default)]
    user: String,
    /// `[seconds, tz offset]`.
    #[serde(default)]
    date: (f64, i64),
    #[serde(default)]
    bookmarks: Vec<String>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    branch: String,
}

/// Parses `hg log -T json`.
pub fn parse_hg_log(json: &[u8]) -> Result<Vec<Revision>> {
    if json.iter().all(u8::is_ascii_whitespace) {
        return Ok(Vec::new());
    }
    let log: Vec<HgLogJson> = serde_json::from_slice(json).map_err(|e| bad_output("hg", e))?;
    Ok(log
        .into_iter()
        .map(|c| {
            let mut refs = c.bookmarks;
            refs.extend(c.tags.into_iter().filter(|t| t != "tip"));
            if !c.branch.is_empty() && c.branch != "default" {
                refs.push(c.branch);
            }
            Revision {
                short: c.node.chars().take(12).collect(),
                id: c.node,
                summary: c.desc.lines().next().unwrap_or("").to_owned(),
                author: c.user,
                time_ms: (c.date.0 * 1000.0) as i64,
                refs,
            }
        })
        .collect())
}

// ---------- running the clients ----------

fn command(kind: VcsKind, dir: &Path) -> Command {
    let mut cmd = Command::new(kind.program());
    match kind {
        VcsKind::Git => {
            cmd.arg("-C").arg(dir);
            cmd.env("GIT_TERMINAL_PROMPT", "0");
        }
        VcsKind::Hg => {
            cmd.arg("--cwd").arg(dir).arg("--noninteractive");
            // Plain, stable output whatever the user's hgrc says.
            cmd.env("HGPLAIN", "1");
        }
    }
    cmd.stdin(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

fn spawn_err(kind: VcsKind, e: io::Error) -> io::Error {
    if e.kind() == io::ErrorKind::NotFound {
        io::Error::new(io::ErrorKind::NotFound, kind.missing())
    } else {
        e
    }
}

fn failure(kind: VcsKind, stderr: &[u8]) -> io::Error {
    let msg = String::from_utf8_lossy(stderr);
    let msg = msg
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    let lower = msg.to_ascii_lowercase();
    let not_found = [
        "does not exist in",
        "exists on disk, but not in",
        "not a valid object name",
        "no such file in rev",
        "unknown revision",
        "not a git repository",
        "no repository found",
    ]
    .iter()
    .any(|m| lower.contains(m));
    io::Error::new(
        if not_found {
            io::ErrorKind::NotFound
        } else {
            io::ErrorKind::Other
        },
        if msg.is_empty() {
            format!("{} failed", kind.program())
        } else {
            msg
        },
    )
}

fn output(kind: VcsKind, dir: &Path, args: &[&str]) -> io::Result<Output> {
    command(kind, dir)
        .args(args)
        .output()
        .map_err(|e| spawn_err(kind, e))
}

/// Runs the client and returns its standard output, or its error message.
fn run(kind: VcsKind, dir: &Path, args: &[&str]) -> io::Result<Vec<u8>> {
    let out = output(kind, dir, args)?;
    if !out.status.success() {
        return Err(failure(kind, &out.stderr));
    }
    Ok(out.stdout)
}

fn run_text(kind: VcsKind, dir: &Path, args: &[&str]) -> io::Result<String> {
    run(kind, dir, args).map(|b| String::from_utf8_lossy(&b).into_owned())
}

/// Whether the client can be run at all.
pub fn available(kind: VcsKind) -> bool {
    output(kind, &std::env::temp_dir(), &["--version"]).is_ok_and(|o| o.status.success())
}

/// The nearest folder at or above `location` that exists, and the path
/// components below it.
fn anchor(location: &str) -> io::Result<(PathBuf, Vec<String>)> {
    let mut p = PathBuf::from(location);
    let mut rest = Vec::new();
    loop {
        if p.is_dir() {
            rest.reverse();
            return Ok((p, rest));
        }
        let name = p
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("{location}: no folder of it exists on disk"),
                )
            })?;
        rest.push(name);
        if !p.pop() || p.as_os_str().is_empty() {
            p = PathBuf::from(".");
            if !p.is_dir() {
                return Err(io::ErrorKind::NotFound.into());
            }
        }
    }
}

/// Whether `rev` names a commit in the repository around `location`.
pub fn rev_exists(kind: VcsKind, location: &str, rev: &str) -> bool {
    if rev.starts_with('-') || rev.is_empty() {
        return false;
    }
    let Ok((dir, _)) = anchor(location) else {
        return false;
    };
    let ok = |args: &[&str]| output(kind, &dir, args).is_ok_and(|o| o.status.success());
    match kind {
        VcsKind::Git => ok(&[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{rev}^{{commit}}"),
        ]),
        VcsKind::Hg => ok(&["log", "-r", rev, "-l", "1", "-T", "x"]),
    }
}

/// The repository root and the location's path inside it (`/`-separated).
fn locate(kind: VcsKind, location: &str) -> io::Result<(PathBuf, String)> {
    let (dir, rest) = anchor(location)?;
    let (root, mut prefix) = match kind {
        VcsKind::Git => {
            let out = run_text(
                kind,
                &dir,
                &["rev-parse", "--show-toplevel", "--show-prefix"],
            )?;
            let mut lines = out.lines();
            let root = lines
                .next()
                .filter(|l| !l.is_empty())
                .ok_or_else(|| io::Error::other("git rev-parse printed no top level"))?;
            let prefix = lines.next().unwrap_or("").trim_end_matches('/').to_owned();
            (PathBuf::from(root), prefix)
        }
        VcsKind::Hg => {
            let root = PathBuf::from(run_text(kind, &dir, &["root"])?.trim());
            let (canon_root, canon_dir) = (root.canonicalize()?, dir.canonicalize()?);
            let rel = canon_dir.strip_prefix(&canon_root).map_err(|_| {
                io::Error::other(format!("{} isn't inside {}", dir.display(), root.display()))
            })?;
            let prefix = rel
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");
            (root, prefix)
        }
    };
    for part in rest {
        if !prefix.is_empty() {
            prefix.push('/');
        }
        prefix.push_str(&part);
    }
    Ok((root, prefix))
}

/// Which kind of working tree `location` is in (Git first), if any.
pub fn detect(location: &str) -> Option<VcsKind> {
    [VcsKind::Git, VcsKind::Hg]
        .into_iter()
        .find(|&k| locate(k, location).is_ok())
}

/// The last commits touching `location` (newest first), for the revision
/// picker.
pub fn recent_revisions(kind: VcsKind, location: &str, limit: usize) -> Result<Vec<Revision>> {
    let fail = |e: io::Error| CoreError::remote(format!("{}{location}", kind.prefix()), e);
    let (dir, rest) = anchor(location).map_err(fail)?;
    let target = if rest.is_empty() {
        ".".to_owned()
    } else {
        rest.join("/")
    };
    let n = limit.to_string();
    match kind {
        VcsKind::Git => {
            let out = run_text(
                kind,
                &dir,
                &[
                    "log",
                    "-n",
                    &n,
                    "--format=%H%x1f%h%x1f%s%x1f%an%x1f%ct%x1f%D%x1e",
                    "--",
                    &target,
                ],
            )
            .map_err(fail)?;
            Ok(parse_git_log(&out))
        }
        VcsKind::Hg => {
            let out = run(kind, &dir, &["log", "-l", &n, "-T", "json", &target]).map_err(fail)?;
            parse_hg_log(&out)
        }
    }
}

/// The client's output as a stream; the exit status is checked at the end
/// so a failure mid-way isn't mistaken for a short file.
struct CatReader {
    kind: VcsKind,
    child: Child,
    stdout: ChildStdout,
    stderr: Option<ChildStderr>,
}

impl Read for CatReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.stdout.read(buf)?;
        if n == 0 && !buf.is_empty() {
            let mut err = Vec::new();
            if let Some(mut e) = self.stderr.take() {
                let _ = e.read_to_end(&mut err);
            }
            if !self.child.wait()?.success() {
                return Err(failure(self.kind, &err));
            }
        }
        Ok(n)
    }
}

impl Drop for CatReader {
    fn drop(&mut self) {
        // A reader dropped early (a difference was found) stops the client too.
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}

// ---------- the file system ----------

struct Listing {
    root_dir: PathBuf,
    /// The resolved commit.
    commit: String,
    /// The location's path inside the repository.
    prefix: String,
    /// Set when the location is a file at that revision.
    root_file: Option<EntryMeta>,
    tree: Tree,
}

/// A working-tree file or folder at one revision.
pub struct VcsFs {
    root: VcsPath,
    listing: Mutex<Option<Arc<Listing>>>,
}

impl VcsFs {
    pub fn new(root: VcsPath) -> Self {
        let root = VcsPath {
            inner: String::new(),
            ..root
        };
        Self {
            root,
            listing: Mutex::new(None),
        }
    }

    pub fn root(&self) -> &VcsPath {
        &self.root
    }

    fn label(&self) -> String {
        self.root.label()
    }

    fn err(&self, e: io::Error) -> CoreError {
        CoreError::remote(self.label(), e)
    }

    fn listing(&self) -> Result<Arc<Listing>> {
        let mut slot = self.listing.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(l) = slot.as_ref() {
            return Ok(l.clone());
        }
        let l = Arc::new(self.load().map_err(|e| self.err(e))?);
        *slot = Some(l.clone());
        Ok(l)
    }

    fn load(&self) -> io::Result<Listing> {
        let kind = self.root.kind;
        let rev = &self.root.rev;
        if rev.starts_with('-') {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("bad revision `{rev}`"),
            ));
        }
        let (root_dir, prefix) = locate(kind, &self.root.location)?;
        let not_at_rev = || {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("{} doesn't exist at revision {rev}", self.root.location),
            )
        };
        let (commit, secs) = match kind {
            VcsKind::Git => {
                let out = run_text(
                    kind,
                    &root_dir,
                    &[
                        "show",
                        "-s",
                        "--format=%H %ct",
                        &format!("{rev}^{{commit}}"),
                        "--",
                    ],
                )?;
                let (id, t) = out.trim().split_once(' ').unwrap_or((out.trim(), ""));
                (id.to_owned(), t.parse::<i64>().ok())
            }
            VcsKind::Hg => {
                let out = run_text(
                    kind,
                    &root_dir,
                    &["log", "-r", rev, "-l", "1", "-T", "{node} {date|hgdate}"],
                )?;
                let mut f = out.split_whitespace();
                let id = f.next().ok_or_else(|| {
                    io::Error::new(io::ErrorKind::NotFound, format!("unknown revision {rev}"))
                })?;
                (id.to_owned(), f.next().and_then(|t| t.parse::<i64>().ok()))
            }
        };
        let modified_ms = secs.map(|s| s * 1000);
        let file = |size: u64| EntryMeta {
            size,
            modified_ms,
            ..Default::default()
        };
        let dir = || EntryMeta {
            is_dir: true,
            modified_ms,
            ..Default::default()
        };
        let mut tree = Tree::default();
        let mut root_file = None;
        match kind {
            VcsKind::Git => {
                let object = format!("{commit}:{prefix}");
                let is_tree = prefix.is_empty() || {
                    let t = run_text(kind, &root_dir, &["cat-file", "-t", &object])
                        .map_err(|_| not_at_rev())?;
                    match t.trim() {
                        "tree" => true,
                        "blob" => false,
                        _ => return Err(not_at_rev()),
                    }
                };
                if is_tree {
                    let out = run(
                        kind,
                        &root_dir,
                        &["ls-tree", "-r", "-t", "-l", "-z", &object],
                    )?;
                    for e in parse_ls_tree(&out).map_err(|e| io::Error::other(e.to_string()))? {
                        match e.object {
                            GitObject::Blob => {
                                tree.insert(&e.path, file(e.size), None, Loc::Single)
                            }
                            GitObject::Tree => tree.insert(&e.path, dir(), None, Loc::None),
                            // A submodule's contents aren't in this repository.
                            GitObject::Commit => {}
                        }
                    }
                } else {
                    let size = run_text(kind, &root_dir, &["cat-file", "-s", &object])?;
                    root_file = Some(file(size.trim().parse().unwrap_or(0)));
                }
            }
            VcsKind::Hg => {
                let mut args = vec!["files", "-v", "-r", &commit, "-T", "json"];
                let pattern = format!("path:{prefix}");
                if !prefix.is_empty() {
                    args.push(&pattern);
                }
                let out = output(kind, &root_dir, &args)?;
                // `hg files` exits with 1 when nothing matches.
                if !out.status.success() && out.status.code() != Some(1) {
                    return Err(failure(kind, &out.stderr));
                }
                let files =
                    parse_hg_files(&out.stdout).map_err(|e| io::Error::other(e.to_string()))?;
                if !prefix.is_empty() && files.is_empty() {
                    return Err(not_at_rev());
                }
                for f in files {
                    let path = f.path.replace('\\', "/");
                    if path == prefix {
                        root_file = Some(file(f.size));
                    } else if prefix.is_empty() {
                        tree.insert(&path, file(f.size), None, Loc::Single);
                    } else if let Some(rel) =
                        path.strip_prefix(&prefix).and_then(|r| r.strip_prefix('/'))
                    {
                        tree.insert(rel, file(f.size), None, Loc::Single);
                    }
                }
            }
        }
        Ok(Listing {
            root_dir,
            commit,
            prefix,
            root_file,
            tree,
        })
    }

    fn cat(&self, path: &str) -> io::Result<Box<dyn Read + Send>> {
        let rel = normalize(path)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid path"))?;
        let l = self
            .listing()
            .map_err(|e| io::Error::other(e.to_string()))?;
        if l.root_file.is_none() && l.tree.get(&rel).is_none_or(|n| n.meta.is_dir) {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("{} isn't a file", tree::shown(&self.label(), &rel)),
            ));
        }
        let full = match (l.prefix.is_empty(), rel.is_empty()) {
            (true, _) => rel,
            (false, true) => l.prefix.clone(),
            (false, false) => format!("{}/{rel}", l.prefix),
        };
        let kind = self.root.kind;
        let mut cmd = command(kind, &l.root_dir);
        match kind {
            VcsKind::Git => cmd.args(["cat-file", "--filters", &format!("{}:{full}", l.commit)]),
            VcsKind::Hg => cmd.args(["cat", "-r", &l.commit, &format!("path:{full}")]),
        };
        let mut child = cmd
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| spawn_err(kind, e))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other(format!("{} produced no output", kind.program())))?;
        let stderr = child.stderr.take();
        Ok(Box::new(CatReader {
            kind,
            child,
            stdout,
            stderr,
        }))
    }
}

fn read_only(kind: VcsKind) -> io::Error {
    io::Error::new(io::ErrorKind::Unsupported, kind.read_only())
}

impl Vfs for VcsFs {
    fn read_dir(&self, dir: &str) -> Result<Vec<VfsEntry>> {
        let l = self.listing()?;
        if l.root_file.is_some() {
            return Err(CoreError::Other(format!(
                "{} is a file, not a folder",
                self.label()
            )));
        }
        l.tree.list(&self.label(), dir)
    }

    fn open(&self, path: &str) -> Result<Box<dyn Read + Send>> {
        self.cat(path)
            .map_err(|e| CoreError::remote(tree::shown(&self.label(), path), e))
    }

    fn join(&self, dir: &str, name: &str) -> String {
        tree::join(dir, name)
    }
}

impl VfsWrite for VcsFs {
    fn caps(&self) -> WriteCaps {
        WriteCaps::default()
    }

    fn stat(&self, path: &str) -> io::Result<Option<EntryMeta>> {
        let l = self
            .listing()
            .map_err(|e| io::Error::other(e.to_string()))?;
        if let Some(m) = &l.root_file {
            return Ok(normalize(path)
                .is_some_and(|p| p.is_empty())
                .then(|| m.clone()));
        }
        Ok(l.tree.get(path).map(|n| n.meta.clone()))
    }

    fn open_at(&self, path: &str, offset: u64) -> io::Result<Box<dyn Read + Send>> {
        let mut r = self.cat(path)?;
        io::copy(&mut (&mut r).take(offset), &mut io::sink())?;
        Ok(r)
    }

    fn write(
        &self,
        _path: &str,
        _offset: u64,
        _src: &mut dyn Read,
        _on_bytes: &mut dyn FnMut(u64) -> io::Result<()>,
    ) -> io::Result<u64> {
        Err(read_only(self.root.kind))
    }

    fn create_dir_all(&self, _path: &str) -> io::Result<()> {
        Err(read_only(self.root.kind))
    }

    fn remove_file(&self, _path: &str) -> io::Result<()> {
        Err(read_only(self.root.kind))
    }

    fn remove_dir_all(&self, _path: &str) -> io::Result<()> {
        Err(read_only(self.root.kind))
    }

    fn rename(&self, _from: &str, _to: &str) -> io::Result<()> {
        Err(read_only(self.root.kind))
    }

    fn set_modified(&self, _path: &str, _modified_ms: i64) -> io::Result<()> {
        Err(read_only(self.root.kind))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_from_prefixes() {
        assert_eq!(VcsKind::of("git:/x"), Some(VcsKind::Git));
        assert_eq!(VcsKind::of("HG:C:\\x"), Some(VcsKind::Hg));
        assert_eq!(VcsKind::of("svn:https://h/r"), None);
        assert_eq!(VcsKind::of("g:\\x"), None);
        assert_eq!(VcsKind::of("gi"), None);
    }

    #[test]
    fn parses_specs() {
        let on_disk = ["/src/app", "/src/app/img@2x.png", r"C:\src\app"];
        let exists = |p: &str| on_disk.contains(&p);
        let s = VcsSpec::parse_with("git:/src/app/main.c@HEAD~1", exists).unwrap();
        assert_eq!(
            (s.kind, s.location.as_str(), s.tail.as_str()),
            (VcsKind::Git, "/src/app/main.c", "HEAD~1")
        );
        let s = VcsSpec::parse_with("git:/src/app", exists).unwrap();
        assert_eq!((s.location.as_str(), s.tail.as_str()), ("/src/app", ""));
        // The `@` after an existing path wins; `@{n}` is part of a revision.
        let s = VcsSpec::parse_with("git:/src/app/img@2x.png@HEAD@{2}", exists).unwrap();
        assert_eq!(
            (s.location.as_str(), s.tail.as_str()),
            ("/src/app/img@2x.png", "HEAD@{2}")
        );
        let s = VcsSpec::parse_with("git:/src/app@v1/docs/a@b.txt", exists).unwrap();
        assert_eq!(
            (s.location.as_str(), s.tail.as_str()),
            ("/src/app", "v1/docs/a@b.txt")
        );
        let s = VcsSpec::parse_with(r"hg:C:\src\app@tip\docs", exists).unwrap();
        assert_eq!(
            (s.kind, s.location.as_str(), s.tail.as_str()),
            (VcsKind::Hg, r"C:\src\app", r"tip\docs")
        );
        assert!(VcsSpec::parse_with("git:", exists).is_err());
        assert!(VcsSpec::parse_with("/src/app", exists).is_err());
    }

    #[test]
    fn splits_revisions_from_inner_paths() {
        let never = |_: &str| -> bool { panic!("plain revisions need no lookup") };
        let g = VcsKind::Git;
        assert_eq!(split_rev(g, "", &[], never), ("HEAD".into(), "".into()));
        assert_eq!(
            split_rev(VcsKind::Hg, "", &[], never),
            (".".into(), "".into())
        );
        assert_eq!(split_rev(g, "v1.2", &[], never), ("v1.2".into(), "".into()));
        assert_eq!(
            split_rev(g, "HEAD~1/src/a.c", &[], never),
            ("HEAD~1".into(), "src/a.c".into())
        );
        assert_eq!(
            split_rev(g, r"abc1234\src\a.c", &[], never),
            ("abc1234".into(), r"src\a.c".into())
        );
        assert_eq!(
            split_rev(VcsKind::Hg, "tip/docs/", &[], never),
            ("tip".into(), "docs".into())
        );

        // Refs with slashes: the longest prefix the client accepts.
        let branches = ["origin/main", "feature/x/y"];
        let mut asked = Vec::new();
        let got = split_rev(g, "origin/main/src/a.c", &[], |c| {
            asked.push(c.to_owned());
            branches.contains(&c)
        });
        assert_eq!(got, ("origin/main".into(), "src/a.c".into()));
        assert_eq!(
            asked,
            ["origin/main/src/a.c", "origin/main/src", "origin/main"]
        );
        let got = split_rev(g, "feature/x/y", &[], |c| branches.contains(&c));
        assert_eq!(got, ("feature/x/y".into(), "".into()));
        // Known refs skip the lookups; nothing resolving falls back to the first segment.
        let known = vec!["origin/main".to_owned()];
        assert_eq!(
            split_rev(g, "origin/main/x", &known, never),
            ("origin/main".into(), "x".into())
        );
        assert_eq!(
            split_rev(g, "nope/x", &[], |_| false),
            ("nope".into(), "x".into())
        );
    }

    #[test]
    fn labels_and_pins() {
        let p = VcsPath {
            kind: VcsKind::Git,
            location: "/src/app".into(),
            rev: "HEAD".into(),
            inner: String::new(),
        };
        assert_eq!(p.label(), "git:/src/app");
        assert!(!p.pinned());
        let p = VcsPath {
            rev: "0123456789abcdef0123456789abcdef01234567".into(),
            ..p
        };
        assert!(p.pinned());
        assert_eq!(
            p.label(),
            "git:/src/app@0123456789abcdef0123456789abcdef01234567"
        );
    }

    #[test]
    fn relative_locations_are_anchored() {
        let cwd = std::env::temp_dir();
        assert_eq!(
            with_base_dir("git:repo@HEAD~1", &cwd),
            Some(format!("git:{}", cwd.join("repo@HEAD~1").to_string_lossy()))
        );
        let abs = format!("hg:{}", cwd.join("r").to_string_lossy());
        assert_eq!(with_base_dir(&abs, &cwd), None);
        assert_eq!(with_base_dir("/x/y", &cwd), None);
    }

    #[test]
    fn parses_ls_tree() {
        let out = b"040000 tree 9bc2f8e7d1f0d6a6e2f1c0b0a1e2d3c4b5a69788       -\tsrc\0\
100644 blob 3b18e512dba79e4c8300dd08aeb37f8e728b8dad      12\tsrc/a b.txt\0\
100755 blob 0b18e512dba79e4c8300dd08aeb37f8e728b8dad 1048576\trun.sh\0\
160000 commit 1b18e512dba79e4c8300dd08aeb37f8e728b8dad       -\tvendor/lib\0";
        let e = parse_ls_tree(out).unwrap();
        assert_eq!(e.len(), 4);
        assert_eq!(
            (e[0].object, e[0].size, e[0].path.as_str()),
            (GitObject::Tree, 0, "src")
        );
        assert_eq!(
            (e[1].object, e[1].size, e[1].path.as_str()),
            (GitObject::Blob, 12, "src/a b.txt")
        );
        assert_eq!((e[2].mode.as_str(), e[2].size), ("100755", 1_048_576));
        assert_eq!(e[3].object, GitObject::Commit);
        assert!(parse_ls_tree(b"").unwrap().is_empty());
        assert!(parse_ls_tree(b"garbage\0").is_err());
    }

    #[test]
    fn parses_hg_output() {
        let files = br#"[
 {
  "flags": "",
  "path": "README",
  "size": 12
 },
 {
  "flags": "x",
  "path": "src/run.sh",
  "size": 300
 }
]"#;
        let f = parse_hg_files(files).unwrap();
        assert_eq!(f.len(), 2);
        assert_eq!(
            (f[1].path.as_str(), f[1].size, f[1].flags.as_str()),
            ("src/run.sh", 300, "x")
        );
        assert!(parse_hg_files(b"\n").unwrap().is_empty());
        assert!(parse_hg_files(b"abort: unknown revision").is_err());

        let log = br#"[
 {
  "bookmarks": ["work"],
  "branch": "stable",
  "date": [1709337600.0, 0],
  "desc": "Fix the parser\n\nLonger text.",
  "node": "0123456789abcdef0123456789abcdef01234567",
  "parents": [],
  "phase": "public",
  "rev": 4,
  "tags": ["tip", "v1"],
  "user": "Alice <a@example.com>"
 }
]"#;
        let r = parse_hg_log(log).unwrap();
        assert_eq!(r[0].short, "0123456789ab");
        assert_eq!(r[0].summary, "Fix the parser");
        assert_eq!(r[0].time_ms, 1_709_337_600_000);
        assert_eq!(r[0].refs, ["work", "v1", "stable"]);
    }

    /// Runs `git` in `dir`; `false` when git isn't installed.
    fn git(dir: &Path, args: &[&str]) -> bool {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args([
                "-c",
                "user.name=T",
                "-c",
                "user.email=t@example.com",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .output();
        match out {
            Ok(o) => {
                assert!(
                    o.status.success(),
                    "git {args:?}: {}",
                    String::from_utf8_lossy(&o.stderr)
                );
                true
            }
            Err(_) => false,
        }
    }

    #[test]
    fn compares_revisions_of_a_real_git_repo() {
        use pcompare_core::folder::{
            compare_folders_with, ContentMethod, EntryStatus, FolderCompareOptions,
        };
        use pcompare_core::vfs::LocalFs;

        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(repo.join("src")).unwrap();
        if !git(&repo, &["init", "-q", "-b", "main"]) {
            eprintln!("git isn't installed; skipping");
            return;
        }
        std::fs::write(repo.join("src/a.txt"), "one\n").unwrap();
        std::fs::write(repo.join("gone.txt"), "old\n").unwrap();
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-q", "-m", "first"]);
        std::fs::write(repo.join("src/a.txt"), "two\n").unwrap();
        std::fs::remove_file(repo.join("gone.txt")).unwrap();
        std::fs::write(repo.join("new.txt"), "new\n").unwrap();
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-q", "-m", "second"]);
        git(&repo, &["branch", "feature/x", "HEAD~1"]);
        // An uncommitted edit in the working copy.
        std::fs::write(repo.join("src/a.txt"), "three\n").unwrap();

        let archives = crate::Archives::new(tmp.path().join("cache"));
        let loc = repo.to_string_lossy().into_owned();
        let read = |path: &str| {
            let copy = archives
                .local_copy(path)
                .unwrap()
                .expect("a container path");
            std::fs::read_to_string(copy).unwrap()
        };
        // A file at a revision, written either way round.
        let a = repo
            .join("src")
            .join("a.txt")
            .to_string_lossy()
            .into_owned();
        assert_eq!(read(&format!("git:{a}@HEAD~1")), "one\n");
        assert_eq!(read(&format!("git:{a}")), "two\n");
        assert_eq!(read(&format!("git:{loc}@HEAD~1/src/a.txt")), "one\n");
        // A branch with a slash, and a file deleted from the working copy.
        assert_eq!(read(&format!("git:{loc}@feature/x/src/a.txt")), "one\n");
        let gone = repo.join("gone.txt").to_string_lossy().into_owned();
        assert_eq!(read(&format!("git:{gone}@HEAD~1")), "old\n");
        assert!(archives.local_copy(&format!("git:{gone}")).is_err());

        // Folder Compare: HEAD~1 against HEAD, and HEAD against the working copy.
        let side = |path: &str| archives.resolve(path, true).unwrap().unwrap();
        let (old, new) = (
            side(&format!("git:{loc}@HEAD~1")),
            side(&format!("git:{loc}@main")),
        );
        assert_eq!(old.kind, crate::ContainerKind::Git);
        assert!(old.read_only.is_some());
        let opts = FolderCompareOptions {
            content: ContentMethod::Binary,
            compare_timestamp: false,
            ..Default::default()
        };
        let r = compare_folders_with(&*old.fs, &old.path, &*new.fs, &new.path, &opts).unwrap();
        let status = |entries: &[pcompare_core::folder::FolderEntry], name: &str| {
            entries.iter().find(|e| e.name == name).map(|e| e.status)
        };
        assert_eq!(status(&r.entries, "gone.txt"), Some(EntryStatus::LeftOnly));
        assert_eq!(status(&r.entries, "new.txt"), Some(EntryStatus::RightOnly));
        let src = r.entries.iter().find(|e| e.name == "src").unwrap();
        assert_eq!(status(&src.children, "a.txt"), Some(EntryStatus::Different));

        let r = compare_folders_with(&*new.fs, &new.path, &LocalFs, &loc, &opts).unwrap();
        let src = r.entries.iter().find(|e| e.name == "src").unwrap();
        assert_eq!(status(&src.children, "a.txt"), Some(EntryStatus::Different));
        assert_eq!(status(&r.entries, "new.txt"), Some(EntryStatus::Same));
        assert_eq!(status(&r.entries, ".git"), Some(EntryStatus::RightOnly));

        // A sub-folder as the root, and history for the picker.
        let sub = side(&format!(
            "git:{}@HEAD~1",
            repo.join("src").to_string_lossy()
        ));
        let names: Vec<_> = sub
            .fs
            .read_dir(&sub.path)
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(names, ["a.txt"]);
        let revs = recent_revisions(VcsKind::Git, &a, 10).unwrap();
        assert_eq!(
            revs.iter().map(|r| r.summary.as_str()).collect::<Vec<_>>(),
            ["second", "first"]
        );
        assert!(revs[0].refs.contains(&"main".to_owned()));
        assert!(available(VcsKind::Git));
        assert_eq!(detect(&a), Some(VcsKind::Git));
        assert_eq!(detect(&tmp.path().to_string_lossy()), None);
        assert!(archives
            .resolve(&format!("git:{loc}@no-such-rev"), true)
            .unwrap()
            .unwrap()
            .fs
            .read_dir("")
            .is_err());
    }

    #[test]
    fn parses_git_log() {
        let out = "abc\x1fa\x1fFirst line\x1fBob\x1f1709337600\x1fHEAD -> main, tag: v1, origin/main\x1e\n\
def\x1fd\x1fOlder\x1fAl\x1f1709300000\x1f\x1e\n";
        let r = parse_git_log(out);
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].refs, ["main", "v1", "origin/main"]);
        assert_eq!(r[0].time_ms, 1_709_337_600_000);
        assert!(r[1].refs.is_empty());
    }
}
