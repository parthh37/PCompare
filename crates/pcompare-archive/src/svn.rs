//! Subversion repositories as read-only folders, through the `svn`
//! command-line client (which brings its own authentication, proxies and
//! certificate handling).
//!
//! A path is `svn:<repository URL>[@<revision>][/<path inside>]`, e.g.
//! `svn:https://svn.example.com/repo/trunk@1234` or
//! `svn:svn+ssh://host/repo/branches/x`. Without a revision, `HEAD` is used.
//! The tree is listed once with `svn list --xml --recursive`; files are read
//! with `svn cat`.

use std::io::{self, Read};
use std::process::{Child, ChildStderr, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex};

use pcompare_core::folder::EntryMeta;
use pcompare_core::vfs::{Vfs, VfsEntry};
use pcompare_core::vfs_write::{VfsWrite, WriteCaps};
use pcompare_core::{CoreError, Result};
use serde::Deserialize;

use crate::tree::{self, normalize, Loc, Tree};
use crate::SVN_PREFIX;

pub const READ_ONLY: &str =
    "Subversion repositories are read-only here — commit changes with your svn client";

const MISSING: &str = "The Subversion command-line client `svn` isn't installed (or isn't on PATH). \
Install it — e.g. `brew install subversion` on macOS, your package manager's `subversion` on Linux, \
or TortoiseSVN with \"command line client tools\" on Windows — to compare svn: paths.";

/// A parsed `svn:` path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SvnPath {
    /// The repository URL (without revision).
    pub url: String,
    /// `HEAD`, a revision number or `{date}`.
    pub rev: String,
    /// The path below `url`, `/`-separated (`""` for `url` itself).
    pub inner: String,
}

const SCHEMES: &[&str] = &["svn://", "svn+ssh://", "http://", "https://", "file://"];

fn is_revision(s: &str) -> bool {
    !s.is_empty()
        && (s.bytes().all(|b| b.is_ascii_digit())
            || s.eq_ignore_ascii_case("HEAD")
            || (s.starts_with('{') && s.ends_with('}')))
}

impl SvnPath {
    pub fn parse(path: &str) -> Result<Self> {
        let bad = || {
            CoreError::Other(format!(
                "`{path}` isn't a Subversion path — write svn:<URL>[@<revision>], e.g. svn:https://host/repo/trunk@123"
            ))
        };
        let s = path
            .get(SVN_PREFIX.len()..)
            .filter(|_| crate::is_svn(path))
            .ok_or_else(bad)?
            .trim();
        let scheme = SCHEMES
            .iter()
            .find(|sc| {
                s.get(..sc.len())
                    .is_some_and(|p| p.eq_ignore_ascii_case(sc))
            })
            .ok_or_else(bad)?;
        // The revision `@` comes after the host (svn+ssh://user@host has one too).
        let host_end = s[scheme.len()..]
            .find('/')
            .map_or(s.len(), |i| scheme.len() + i);
        let mut url_end = s.len();
        let mut rev = "HEAD".to_owned();
        let mut inner = "";
        let mut from = host_end;
        while let Some(at) = s[from..].find('@').map(|i| from + i) {
            let tail = &s[at + 1..];
            let (cand, rest) = tail.split_once(['/', '\\']).unwrap_or((tail, ""));
            if is_revision(cand) {
                url_end = at;
                rev = if cand.eq_ignore_ascii_case("HEAD") {
                    "HEAD".into()
                } else {
                    cand.to_owned()
                };
                inner = rest;
                break;
            }
            from = at + 1;
        }
        let url = s[..url_end].trim_end_matches(['/', '\\']).to_owned();
        let inner = normalize(inner).ok_or_else(bad)?;
        Ok(SvnPath { url, rev, inner })
    }

    /// A fixed revision never changes, so its listing can be kept.
    pub fn pinned(&self) -> bool {
        self.rev != "HEAD"
    }

    /// `svn:<url>@<rev>` — the root as the user sees it.
    pub fn label(&self) -> String {
        if self.pinned() {
            format!("{SVN_PREFIX}{}@{}", self.url, self.rev)
        } else {
            format!("{SVN_PREFIX}{}", self.url)
        }
    }

    /// The URL of a path below the root, with the peg revision.
    fn target(&self, rel: &str) -> String {
        if rel.is_empty() {
            format!("{}@{}", self.url, self.rev)
        } else {
            format!("{}/{rel}@{}", self.url, self.rev)
        }
    }
}

// ---------- XML ----------

#[derive(Debug, Deserialize)]
struct ListsXml {
    #[serde(rename = "list", default)]
    lists: Vec<ListXml>,
}

#[derive(Debug, Deserialize)]
struct ListXml {
    #[serde(rename = "entry", default)]
    entries: Vec<EntryXml>,
}

#[derive(Debug, Deserialize)]
struct EntryXml {
    #[serde(rename = "@kind")]
    kind: String,
    name: String,
    size: Option<u64>,
    commit: Option<CommitXml>,
}

#[derive(Debug, Deserialize)]
struct CommitXml {
    #[serde(rename = "@revision")]
    revision: Option<u64>,
    author: Option<String>,
    date: Option<String>,
}

#[derive(Debug, Deserialize)]
struct InfoXml {
    #[serde(rename = "entry", default)]
    entries: Vec<InfoEntryXml>,
}

#[derive(Debug, Deserialize)]
struct InfoEntryXml {
    #[serde(rename = "@kind")]
    kind: String,
    #[serde(rename = "@revision")]
    revision: Option<u64>,
    commit: Option<CommitXml>,
}

/// One entry of `svn list --xml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListEntry {
    pub is_dir: bool,
    /// Path relative to the listed URL.
    pub name: String,
    pub size: u64,
    pub revision: Option<u64>,
    pub author: Option<String>,
    pub modified_ms: Option<i64>,
}

fn xml_err(e: impl std::fmt::Display) -> CoreError {
    CoreError::Other(format!("unexpected output from svn: {e}"))
}

fn date_ms(s: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(s.trim())
        .ok()
        .map(|d| d.timestamp_millis())
}

/// Parses `svn list --xml [--recursive]`.
pub fn parse_list(xml: &str) -> Result<Vec<ListEntry>> {
    let doc: ListsXml = quick_xml::de::from_str(xml).map_err(xml_err)?;
    Ok(doc
        .lists
        .into_iter()
        .flat_map(|l| l.entries)
        .map(|e| {
            let commit = e.commit.as_ref();
            ListEntry {
                is_dir: e.kind == "dir",
                size: e.size.unwrap_or(0),
                revision: commit.and_then(|c| c.revision),
                author: commit.and_then(|c| c.author.clone()),
                modified_ms: commit.and_then(|c| c.date.as_deref()).and_then(date_ms),
                name: e.name,
            }
        })
        .collect())
}

/// What `svn info --xml` says about a URL: folder or file, and its last
/// change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InfoEntry {
    pub is_dir: bool,
    pub revision: Option<u64>,
    pub modified_ms: Option<i64>,
}

pub fn parse_info(xml: &str) -> Result<InfoEntry> {
    let doc: InfoXml = quick_xml::de::from_str(xml).map_err(xml_err)?;
    let e = doc
        .entries
        .into_iter()
        .next()
        .ok_or_else(|| xml_err("no entry in svn info"))?;
    Ok(InfoEntry {
        is_dir: e.kind == "dir",
        revision: e.revision,
        modified_ms: e.commit.and_then(|c| c.date).as_deref().and_then(date_ms),
    })
}

// ---------- running svn ----------

fn svn_command() -> Command {
    let mut cmd = Command::new("svn");
    cmd.arg("--non-interactive");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

fn spawn_err(e: io::Error) -> io::Error {
    if e.kind() == io::ErrorKind::NotFound {
        io::Error::new(io::ErrorKind::NotFound, MISSING)
    } else {
        e
    }
}

/// Runs `svn args...` and returns its output, or its error message.
fn run(args: &[&str]) -> io::Result<String> {
    let out = svn_command().args(args).output().map_err(spawn_err)?;
    if !out.status.success() {
        return Err(failure(&out.stderr));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn failure(stderr: &[u8]) -> io::Error {
    let msg = String::from_utf8_lossy(stderr);
    let msg = msg
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    let kind = if msg.contains("E170000") || msg.contains("E200009") || msg.contains("non-existent")
    {
        io::ErrorKind::NotFound
    } else {
        io::ErrorKind::Other
    };
    io::Error::new(
        kind,
        if msg.is_empty() {
            "svn failed".to_owned()
        } else {
            msg
        },
    )
}

/// `svn cat` output as a stream; the exit status is checked at the end so
/// a failure mid-way isn't mistaken for a short file.
struct CatReader {
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
                return Err(failure(&err));
            }
        }
        Ok(n)
    }
}

impl Drop for CatReader {
    fn drop(&mut self) {
        // A reader dropped early (a difference was found) stops svn too.
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}

// ---------- the file system ----------

struct Listing {
    /// Set when the root URL is a file rather than a folder.
    root_file: Option<EntryMeta>,
    tree: Tree,
}

/// A repository folder (or file) at one revision.
pub struct SvnFs {
    root: SvnPath,
    listing: Mutex<Option<Arc<Listing>>>,
}

impl SvnFs {
    pub fn new(root: SvnPath) -> Self {
        let root = SvnPath {
            inner: String::new(),
            ..root
        };
        Self {
            root,
            listing: Mutex::new(None),
        }
    }

    pub fn root(&self) -> &SvnPath {
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
        let target = self.root.target("");
        let info = parse_info(&run(&["info", "--xml", &target]).map_err(|e| self.err(e))?)?;
        let depth = if info.is_dir { "infinity" } else { "empty" };
        let xml = run(&["list", "--xml", "--depth", depth, &target]).map_err(|e| self.err(e))?;
        let mut tree = Tree::default();
        let mut root_file = None;
        for e in parse_list(&xml)? {
            let meta = EntryMeta {
                size: if e.is_dir { 0 } else { e.size },
                modified_ms: e.modified_ms,
                is_dir: e.is_dir,
                ..Default::default()
            };
            if !info.is_dir {
                root_file = Some(meta);
                continue;
            }
            tree.insert(
                &e.name,
                meta,
                None,
                if e.is_dir { Loc::None } else { Loc::Single },
            );
        }
        if !info.is_dir && root_file.is_none() {
            root_file = Some(EntryMeta {
                modified_ms: info.modified_ms,
                ..Default::default()
            });
        }
        let l = Arc::new(Listing { root_file, tree });
        *slot = Some(l.clone());
        Ok(l)
    }

    fn cat(&self, path: &str) -> io::Result<Box<dyn Read + Send>> {
        let rel = normalize(path)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid path"))?;
        let mut child = svn_command()
            .args(["cat", &self.root.target(&rel)])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(spawn_err)?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("svn produced no output"))?;
        let stderr = child.stderr.take();
        Ok(Box::new(CatReader {
            child,
            stdout,
            stderr,
        }))
    }
}

fn read_only() -> io::Error {
    io::Error::new(io::ErrorKind::Unsupported, READ_ONLY)
}

impl Vfs for SvnFs {
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

impl VfsWrite for SvnFs {
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
        Err(read_only())
    }

    fn create_dir_all(&self, _path: &str) -> io::Result<()> {
        Err(read_only())
    }

    fn remove_file(&self, _path: &str) -> io::Result<()> {
        Err(read_only())
    }

    fn remove_dir_all(&self, _path: &str) -> io::Result<()> {
        Err(read_only())
    }

    fn rename(&self, _from: &str, _to: &str) -> io::Result<()> {
        Err(read_only())
    }

    fn set_modified(&self, _path: &str, _modified_ms: i64) -> io::Result<()> {
        Err(read_only())
    }
}

/// Whether the `svn` client can be run at all.
pub fn available() -> bool {
    run(&["--version", "--quiet"]).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIST: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<lists>
<list
   path="https://svn.example.com/repo/trunk@42">
<entry
   kind="dir">
<name>src</name>
<commit
   revision="40">
<author>alice</author>
<date>2024-03-01T10:20:30.123456Z</date>
</commit>
</entry>
<entry
   kind="file">
<name>src/main &amp; co.c</name>
<size>1234</size>
<commit
   revision="41">
<author>bob</author>
<date>2024-03-02T00:00:00.000000Z</date>
</commit>
</entry>
<entry
   kind="file">
<name>README</name>
<size>0</size>
<commit
   revision="1">
</commit>
</entry>
</list>
</lists>
"#;

    const INFO: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<info>
<entry
   kind="dir"
   path="trunk"
   revision="42">
<url>https://svn.example.com/repo/trunk</url>
<relative-url>^/trunk</relative-url>
<repository>
<root>https://svn.example.com/repo</root>
<uuid>13f79535-47bb-0310-9956-ffa450edef68</uuid>
</repository>
<commit
   revision="41">
<author>bob</author>
<date>2024-03-02T00:00:00.000000Z</date>
</commit>
</entry>
</info>
"#;

    #[test]
    fn parses_svn_list_xml() {
        let entries = parse_list(LIST).unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(
            entries[0],
            ListEntry {
                is_dir: true,
                name: "src".into(),
                size: 0,
                revision: Some(40),
                author: Some("alice".into()),
                modified_ms: Some(1_709_288_430_123),
            }
        );
        assert_eq!(entries[1].name, "src/main & co.c");
        assert_eq!((entries[1].is_dir, entries[1].size), (false, 1234));
        assert_eq!(entries[2].modified_ms, None);
        assert!(parse_list("<lists><list></list></lists>")
            .unwrap()
            .is_empty());
        assert!(parse_list("svn: E170013: Unable to connect").is_err());
    }

    #[test]
    fn parses_svn_info_xml() {
        let info = parse_info(INFO).unwrap();
        assert_eq!(
            info,
            InfoEntry {
                is_dir: true,
                revision: Some(42),
                modified_ms: Some(1_709_337_600_000),
            }
        );
    }

    #[test]
    fn parses_svn_paths() {
        let p = SvnPath::parse("svn:https://h/repo/trunk@123/src/a.c").unwrap();
        assert_eq!(
            (p.url.as_str(), p.rev.as_str(), p.inner.as_str()),
            ("https://h/repo/trunk", "123", "src/a.c")
        );
        assert!(p.pinned());
        assert_eq!(p.label(), "svn:https://h/repo/trunk@123");
        assert_eq!(p.target("x/y"), "https://h/repo/trunk/x/y@123");

        let p = SvnPath::parse("SVN:svn+ssh://me@host/repo/").unwrap();
        assert_eq!(
            (p.url.as_str(), p.rev.as_str(), p.inner.as_str()),
            ("svn+ssh://me@host/repo", "HEAD", "")
        );
        assert!(!p.pinned());

        // An `@` that isn't a revision is part of the path.
        let p = SvnPath::parse("svn:file:///srv/repo/img@2x.png").unwrap();
        assert_eq!(
            (p.url.as_str(), p.rev.as_str()),
            ("file:///srv/repo/img@2x.png", "HEAD")
        );
        let p = SvnPath::parse("svn:https://h/r@head").unwrap();
        assert_eq!(p.rev, "HEAD");

        assert!(SvnPath::parse("svn:/local/path").is_err());
        assert!(SvnPath::parse("https://h/r").is_err());
    }
}
