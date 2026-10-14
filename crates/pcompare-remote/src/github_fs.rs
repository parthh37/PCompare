//! GitHub repositories as a read-only file system.
//!
//! A path is `<owner>/<repo>/<ref>/<path in the repository>`, minus the
//! parts the profile pins (an owner, or an owner and a repository). The
//! top levels are virtual folders — owners, their repositories, then each
//! repository's branches and tags — so the remote browser can walk down to
//! a ref, and two refs compare like any two folders.
//!
//! A ref is resolved to a commit and that commit's whole tree is fetched in
//! one request, so a compare reads one consistent snapshot and spends few
//! of the API's 5,000 requests an hour. Branch names may contain `/`, which
//! is written `%2F` inside the ref's path segment.

use crate::graph_fs::{body_err, Body};
use crate::profile::ResolvedProfile;
use crate::{rt, NetRead};
use chrono::DateTime;
use pcompare_core::folder::EntryMeta;
use pcompare_core::vfs::{Vfs, VfsEntry};
use pcompare_core::vfs_write::{VfsWrite, WriteCaps};
use pcompare_core::{CoreError, Result};
use reqwest::header::{ACCEPT, LINK, USER_AGENT};
use reqwest::{RequestBuilder, Response, StatusCode};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::Value;
use std::collections::{BTreeSet, HashMap};
use std::io::{self, Read};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Tries per request when the link drops or GitHub has a hiccup.
const ATTEMPTS: u32 = 4;
/// Pages fetched at most when listing repositories, branches or tags.
const MAX_PAGES: usize = 10;
/// How long a branch name keeps pointing at the commit it resolved to. A
/// refresh after a push should see the push, a compare shouldn't re-ask
/// for every folder.
const REF_TTL: Duration = Duration::from_secs(60);

fn read_only() -> io::Error {
    io::Error::new(io::ErrorKind::Unsupported, "GitHub connections are read-only")
}

/// Percent-encodes one URL path segment (everything but RFC 3986 unreserved).
fn encode_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// A ref as one path segment: `feature/x` → `feature%2Fx`.
pub fn encode_ref(r: &str) -> String {
    r.replace('%', "%25").replace('/', "%2F")
}

/// Inverse of [`encode_ref`]; tolerates lower-case hex and a raw `%`.
pub fn decode_ref(seg: &str) -> String {
    let mut out = String::with_capacity(seg.len());
    let mut rest = seg;
    while let Some(i) = rest.find('%') {
        out.push_str(&rest[..i]);
        let code = rest.get(i + 1..i + 3).unwrap_or("");
        if code.eq_ignore_ascii_case("2F") {
            out.push('/');
            rest = &rest[i + 3..];
        } else if code == "25" {
            out.push('%');
            rest = &rest[i + 3..];
        } else {
            out.push('%');
            rest = &rest[i + 1..];
        }
    }
    out.push_str(rest);
    out
}

fn parse_ms(s: &str) -> Option<i64> {
    DateTime::parse_from_rfc3339(s).ok().map(|t| t.timestamp_millis())
}

fn status_err(status: StatusCode, body: &str, rate_left: Option<&str>, reset: Option<&str>) -> io::Error {
    use io::ErrorKind as K;
    let message = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| v["message"].as_str().map(str::to_owned))
        .unwrap_or_else(|| body.chars().take(300).collect());
    if matches!(status.as_u16(), 403 | 429) && rate_left == Some("0") {
        let when = reset
            .and_then(|r| r.parse::<i64>().ok())
            .and_then(|s| DateTime::from_timestamp(s, 0))
            .map(|t| format!(" until {} UTC", t.format("%H:%M")))
            .unwrap_or_default();
        // PermissionDenied: retrying straight away can't help.
        return io::Error::new(
            K::PermissionDenied,
            format!("GitHub's API rate limit is used up{when}. Signing in (a token or the GitHub CLI) raises it to 5,000 requests an hour."),
        );
    }
    let kind = match status.as_u16() {
        // GitHub answers 422 for a ref that doesn't exist.
        404 | 422 => K::NotFound,
        401 => K::PermissionDenied,
        403 => K::PermissionDenied,
        400 => K::InvalidInput,
        429 | 500..=599 => K::ConnectionReset,
        _ => K::Other,
    };
    let hint = if status == StatusCode::UNAUTHORIZED { " — the token was rejected; check or replace it" } else { "" };
    io::Error::new(kind, format!("{status}: {message}{hint}"))
}

/// The `rel="next"` URL of a paged response.
fn next_page(resp: &Response) -> Option<String> {
    next_link(resp.headers().get(LINK)?.to_str().ok()?)
}

/// The `rel="next"` URL in a `Link` header.
fn next_link(link: &str) -> Option<String> {
    link.split(',').find_map(|part| {
        let (url, rel) = part.split_once(';')?;
        rel.contains("rel=\"next\"")
            .then(|| url.trim().trim_start_matches('<').trim_end_matches('>').to_owned())
    })
}

/// Sends the request `make` builds, retrying dropped connections and
/// server errors.
async fn send(make: impl Fn() -> RequestBuilder) -> io::Result<Response> {
    let mut attempt = 1;
    loop {
        match make().send().await {
            Ok(resp) if resp.status().is_success() => return Ok(resp),
            Ok(resp) if resp.status().is_server_error() && attempt < ATTEMPTS => {}
            Ok(resp) => {
                let status = resp.status();
                let header = |k: &str| resp.headers().get(k).and_then(|v| v.to_str().ok()).map(str::to_owned);
                let (left, reset) = (header("x-ratelimit-remaining"), header("x-ratelimit-reset"));
                let body = resp.text().await.unwrap_or_default();
                return Err(status_err(status, &body, left.as_deref(), reset.as_deref()));
            }
            Err(e) if attempt < ATTEMPTS && (e.is_connect() || e.is_timeout()) => {}
            Err(e) => return Err(io::Error::new(io::ErrorKind::ConnectionReset, e.to_string())),
        }
        tokio::time::sleep(Duration::from_millis(500 << attempt)).await;
        attempt += 1;
    }
}

#[derive(Deserialize)]
struct Owner {
    login: String,
}

#[derive(Deserialize)]
struct Repo {
    name: String,
    owner: Owner,
    pushed_at: Option<String>,
}

#[derive(Deserialize)]
struct Named {
    name: String,
}

#[derive(Deserialize)]
struct Commit {
    sha: String,
    commit: CommitInfo,
}

#[derive(Deserialize)]
struct CommitInfo {
    committer: Option<Signature>,
}

#[derive(Deserialize)]
struct Signature {
    date: Option<String>,
}

#[derive(Deserialize)]
struct Tree {
    tree: Vec<TreeItem>,
    #[serde(default)]
    truncated: bool,
}

#[derive(Deserialize)]
struct TreeItem {
    path: String,
    mode: String,
    #[serde(rename = "type")]
    kind: String,
    sha: String,
    size: Option<u64>,
}

#[derive(Clone, Copy, PartialEq)]
enum NodeKind {
    File,
    Dir,
    /// A submodule: shown as a small file holding the commit it points at,
    /// the way `git diff` shows one.
    Submodule,
}

#[derive(Clone)]
struct Node {
    name: String,
    kind: NodeKind,
    sha: String,
    size: u64,
    mode: u32,
}

fn submodule_text(sha: &str) -> String {
    format!("Subproject commit {sha}\n")
}

impl Node {
    fn from_item(name: &str, item: &TreeItem) -> Option<Self> {
        let kind = match item.kind.as_str() {
            "blob" => NodeKind::File,
            "tree" => NodeKind::Dir,
            "commit" => NodeKind::Submodule,
            _ => return None,
        };
        let size = match kind {
            NodeKind::File => item.size.unwrap_or(0),
            NodeKind::Dir => 0,
            NodeKind::Submodule => submodule_text(&item.sha).len() as u64,
        };
        // Symbolic links (120000) read as their target path, like in git.
        let mode = if item.mode == "100755" { 0o755 } else { 0o644 };
        Some(Self { name: name.to_owned(), kind, sha: item.sha.clone(), size, mode })
    }

    fn meta(&self, modified_ms: Option<i64>) -> EntryMeta {
        let is_dir = self.kind == NodeKind::Dir;
        EntryMeta {
            size: self.size,
            modified_ms,
            is_dir,
            mode: (!is_dir).then_some(self.mode),
            content_id: (!is_dir).then(|| self.sha.clone()),
            ..Default::default()
        }
    }
}

#[derive(Default)]
struct Listing {
    /// Folders listed so far, by path inside the repository ("" = top).
    dirs: HashMap<String, Vec<Node>>,
    /// Folders seen but not listed yet, with their tree id. Only used when
    /// the repository is too big for GitHub's one-request listing.
    unlisted: HashMap<String, String>,
}

/// One commit's tree.
struct Snapshot {
    owner: String,
    repo: String,
    /// When the commit was made; every file in it carries this time.
    modified_ms: Option<i64>,
    listing: Mutex<Listing>,
}

fn parent_and_name(path: &str) -> (&str, &str) {
    path.rsplit_once('/').unwrap_or(("", path))
}

/// Where a path points.
#[derive(Debug, PartialEq)]
enum Loc {
    /// The owners the signed-in account has repositories under.
    Owners,
    /// An owner's repositories.
    Repos { owner: String },
    /// A repository's branches and tags.
    Refs { owner: String, repo: String },
    /// A path inside a repository at a ref ("" = the ref's top folder).
    Tree { owner: String, repo: String, git_ref: String, path: String },
}

pub struct GithubFs {
    http: reqwest::Client,
    api: String,
    token: Option<String>,
    owner: Option<String>,
    repo: Option<String>,
    /// The signed-in account, when there is one.
    login: Option<String>,
    my_repos: Mutex<Option<Arc<Vec<Repo>>>>,
    /// `owner/repo@ref` → commit, and when it was looked up.
    refs: Mutex<HashMap<String, (String, Instant)>>,
    /// Commit id → its tree. Commits never change, so these are kept for as
    /// long as the connection.
    snapshots: Mutex<HashMap<String, Arc<Snapshot>>>,
}

/// The token the GitHub CLI signed in with, from `gh auth token`.
fn gh_cli_token(host: &str) -> Result<String> {
    // Apps started from the Dock or Start menu don't see a shell's PATH.
    let mut candidates = vec!["gh".to_owned()];
    #[cfg(target_os = "macos")]
    candidates.extend(["/opt/homebrew/bin/gh".to_owned(), "/usr/local/bin/gh".to_owned()]);
    #[cfg(windows)]
    candidates.extend(
        ["ProgramFiles", "LOCALAPPDATA"]
            .iter()
            .filter_map(|v| std::env::var(v).ok())
            .flat_map(|dir| [format!(r"{dir}\GitHub CLI\gh.exe"), format!(r"{dir}\Programs\GitHub CLI\gh.exe")]),
    );
    for exe in candidates {
        let mut cmd = std::process::Command::new(&exe);
        cmd.args(["auth", "token", "--hostname", host]);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        match cmd.output() {
            Ok(out) if out.status.success() => {
                let token = String::from_utf8_lossy(&out.stdout).trim().to_owned();
                if !token.is_empty() {
                    return Ok(token);
                }
            }
            Ok(_) => {
                return Err(CoreError::Other(format!(
                    "the GitHub CLI isn't signed in to {host} — run `gh auth login` in a terminal, then try again"
                )))
            }
            Err(_) => continue,
        }
    }
    Err(CoreError::Other(
        "the GitHub CLI (gh) isn't installed — get it from https://cli.github.com, or use a personal access token".into(),
    ))
}

impl GithubFs {
    pub fn connect(p: &ResolvedProfile) -> Result<Self> {
        let name = p.profile.name.as_str();
        let host = p
            .get("host")
            .unwrap_or("github.com")
            .trim_start_matches("https://")
            .trim_end_matches('/')
            .to_owned();
        let api = if host.eq_ignore_ascii_case("github.com") {
            "https://api.github.com".to_owned()
        } else {
            format!("https://{host}/api/v3")
        };
        let token = match p.get("auth").unwrap_or("token") {
            "anonymous" => None,
            "github-cli" => Some(gh_cli_token(&host)?),
            _ => Some(
                p.get("token")
                    .ok_or_else(|| CoreError::Other("enter a personal access token, or pick another sign-in".into()))?
                    .to_owned(),
            ),
        };
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(30))
            .read_timeout(Duration::from_secs(120))
            .build()
            .map_err(|e| CoreError::remote(name, e))?;
        let mut fs = Self {
            http,
            api,
            token,
            owner: p.get("owner").map(str::to_owned),
            repo: p.get("repo").map(str::to_owned),
            login: None,
            my_repos: Mutex::new(None),
            refs: Mutex::new(HashMap::new()),
            snapshots: Mutex::new(HashMap::new()),
        };
        if fs.repo.is_some() && fs.owner.is_none() {
            return Err(CoreError::Other("a repository needs its owner too".into()));
        }
        if fs.token.is_some() {
            // Checks the token, and names the account for the top folder.
            let me: Owner = fs.json(&format!("{}/user", fs.api)).map_err(|e| CoreError::remote(name, e))?;
            fs.login = Some(me.login);
        }
        Ok(fs)
    }

    fn request(&self, url: &str) -> RequestBuilder {
        let req = self
            .http
            .get(url)
            .header(USER_AGENT, concat!("PCompare/", env!("CARGO_PKG_VERSION")))
            .header(ACCEPT, "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28");
        match &self.token {
            Some(t) => req.bearer_auth(t),
            None => req,
        }
    }

    fn json<T: DeserializeOwned>(&self, url: &str) -> io::Result<T> {
        rt().block_on(async { send(|| self.request(url)).await?.json::<T>().await.map_err(body_err) })
    }

    /// Every item of a paged list, up to [`MAX_PAGES`] pages.
    fn all<T: DeserializeOwned>(&self, url: String) -> io::Result<Vec<T>> {
        rt().block_on(async {
            let mut out = Vec::new();
            let mut url = Some(url);
            for _ in 0..MAX_PAGES {
                let Some(u) = url.take() else { break };
                let resp = send(|| self.request(&u)).await?;
                url = next_page(&resp);
                out.extend(resp.json::<Vec<T>>().await.map_err(body_err)?);
            }
            Ok(out)
        })
    }

    fn repo_url(&self, owner: &str, repo: &str) -> String {
        format!("{}/repos/{}/{}", self.api, encode_segment(owner), encode_segment(repo))
    }

    fn locate(&self, path: &str) -> Loc {
        let mut segs = path.split('/').filter(|s| !s.is_empty());
        let owner = match &self.owner {
            Some(o) => o.clone(),
            None => match segs.next() {
                Some(o) => o.to_owned(),
                None => return Loc::Owners,
            },
        };
        let repo = match &self.repo {
            Some(r) => r.clone(),
            None => match segs.next() {
                Some(r) => r.to_owned(),
                None => return Loc::Repos { owner },
            },
        };
        let Some(git_ref) = segs.next() else {
            return Loc::Refs { owner, repo };
        };
        Loc::Tree {
            owner,
            repo,
            git_ref: decode_ref(git_ref),
            path: segs.collect::<Vec<_>>().join("/"),
        }
    }

    /// Repositories the signed-in account can see (fetched once).
    fn my_repos(&self) -> io::Result<Arc<Vec<Repo>>> {
        if let Some(r) = self.my_repos.lock().unwrap().as_ref() {
            return Ok(r.clone());
        }
        let url = format!(
            "{}/user/repos?per_page=100&sort=pushed&affiliation=owner,collaborator,organization_member",
            self.api
        );
        let repos = Arc::new(self.all::<Repo>(url)?);
        *self.my_repos.lock().unwrap() = Some(repos.clone());
        Ok(repos)
    }

    fn list_owners(&self) -> io::Result<Vec<VfsEntry>> {
        let Some(login) = &self.login else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "without signing in, enter a path such as `owner/repository`, or set an owner in the profile",
            ));
        };
        let mut owners = BTreeSet::from([login.clone()]);
        owners.extend(self.my_repos()?.iter().map(|r| r.owner.login.clone()));
        Ok(owners.into_iter().map(|name| dir_entry(name, None)).collect())
    }

    fn list_repos(&self, owner: &str) -> io::Result<Vec<VfsEntry>> {
        // The account's own list includes private repositories; any other
        // owner's public ones come from the owner's page.
        let mut repos: Vec<(String, Option<String>)> = Vec::new();
        if self.token.is_some() {
            repos = self
                .my_repos()?
                .iter()
                .filter(|r| r.owner.login.eq_ignore_ascii_case(owner))
                .map(|r| (r.name.clone(), r.pushed_at.clone()))
                .collect();
        }
        if repos.is_empty() {
            let url = format!("{}/users/{}/repos?per_page=100&sort=pushed&type=all", self.api, encode_segment(owner));
            repos = self.all::<Repo>(url)?.into_iter().map(|r| (r.name, r.pushed_at)).collect();
        }
        Ok(repos
            .into_iter()
            .map(|(name, pushed)| dir_entry(name, pushed.as_deref().and_then(parse_ms)))
            .collect())
    }

    /// A repository's branches and tags, as folders. A tag named like a
    /// branch is left out: the path would resolve to the branch anyway.
    fn list_refs(&self, owner: &str, repo: &str) -> io::Result<Vec<VfsEntry>> {
        let base = self.repo_url(owner, repo);
        let branches = self.all::<Named>(format!("{base}/branches?per_page=100"))?;
        let tags = self.all::<Named>(format!("{base}/tags?per_page=100"))?;
        let mut seen = BTreeSet::new();
        Ok(branches
            .into_iter()
            .chain(tags)
            .filter(|r| seen.insert(r.name.clone()))
            .map(|r| dir_entry(encode_ref(&r.name), None))
            .collect())
    }

    /// The commit a ref points at now (re-checked after [`REF_TTL`]).
    fn snapshot(&self, owner: &str, repo: &str, git_ref: &str) -> io::Result<Arc<Snapshot>> {
        let key = format!("{owner}/{repo}@{git_ref}");
        let cached = self.refs.lock().unwrap().get(&key).filter(|(_, at)| at.elapsed() < REF_TTL).map(|(c, _)| c.clone());
        if let Some(snap) = cached.and_then(|c| self.snapshots.lock().unwrap().get(&c).cloned()) {
            return Ok(snap);
        }
        let commit: Commit = self.json(&format!("{}/commits/{}", self.repo_url(owner, repo), encode_segment(git_ref)))?;
        self.refs.lock().unwrap().insert(key, (commit.sha.clone(), Instant::now()));
        if let Some(snap) = self.snapshots.lock().unwrap().get(&commit.sha) {
            return Ok(snap.clone());
        }
        let snap = Arc::new(Snapshot {
            owner: owner.to_owned(),
            repo: repo.to_owned(),
            modified_ms: commit.commit.committer.and_then(|c| c.date).as_deref().and_then(parse_ms),
            listing: Mutex::new(self.whole_tree(owner, repo, &commit.sha)?),
        });
        self.snapshots.lock().unwrap().insert(commit.sha, snap.clone());
        Ok(snap)
    }

    /// Lists a commit's whole tree in one request. Past GitHub's limit
    /// (100,000 entries) only the top folder is listed; the rest follows
    /// folder by folder as the compare walks down.
    fn whole_tree(&self, owner: &str, repo: &str, commit: &str) -> io::Result<Listing> {
        let base = self.repo_url(owner, repo);
        let full: Tree = self.json(&format!("{base}/git/trees/{commit}?recursive=1"))?;
        if full.truncated {
            let top: Tree = self.json(&format!("{base}/git/trees/{commit}"))?;
            let mut listing = Listing::default();
            add_level(&mut listing, "", &top.tree);
            return Ok(listing);
        }
        Ok(from_recursive(&full.tree))
    }

    /// The entries of folder `path` in a snapshot.
    fn dir(&self, snap: &Snapshot, path: &str) -> io::Result<Vec<Node>> {
        let tree_sha = {
            let l = snap.listing.lock().unwrap();
            if let Some(nodes) = l.dirs.get(path) {
                return Ok(nodes.clone());
            }
            l.unlisted.get(path).cloned()
        };
        let Some(sha) = tree_sha else {
            // Not listed yet: list the parent first, which may reveal it.
            let (parent, name) = parent_and_name(path);
            if path.is_empty() || !self.dir(snap, parent)?.iter().any(|n| n.name == name && n.kind == NodeKind::Dir) {
                return Err(io::Error::new(io::ErrorKind::NotFound, format!("no folder `{path}` in this commit")));
            }
            return self.dir(snap, path);
        };
        let tree: Tree = self.json(&format!("{}/git/trees/{sha}", self.repo_url(&snap.owner, &snap.repo)))?;
        let mut l = snap.listing.lock().unwrap();
        add_level(&mut l, path, &tree.tree);
        Ok(l.dirs.get(path).cloned().unwrap_or_default())
    }

    fn node(&self, snap: &Snapshot, path: &str) -> io::Result<Option<Node>> {
        let (parent, name) = parent_and_name(path);
        match self.dir(snap, parent) {
            Ok(nodes) => Ok(nodes.into_iter().find(|n| n.name == name)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn file(&self, path: &str) -> io::Result<(Arc<Snapshot>, Node)> {
        let Loc::Tree { owner, repo, git_ref, path } = self.locate(path) else {
            return Err(io::Error::new(io::ErrorKind::IsADirectory, "this is a folder"));
        };
        let snap = self.snapshot(&owner, &repo, &git_ref)?;
        match self.node(&snap, &path)? {
            Some(n) if n.kind == NodeKind::Dir => Err(io::Error::new(io::ErrorKind::IsADirectory, "this is a folder")),
            Some(n) => Ok((snap, n)),
            None => Err(io::Error::new(io::ErrorKind::NotFound, format!("no file `{path}` at {git_ref}"))),
        }
    }
}

/// Every folder of a recursive tree listing.
fn from_recursive(items: &[TreeItem]) -> Listing {
    let mut listing = Listing::default();
    listing.dirs.insert(String::new(), Vec::new());
    for item in items {
        let (parent, name) = parent_and_name(&item.path);
        if let Some(node) = Node::from_item(name, item) {
            if node.kind == NodeKind::Dir {
                listing.dirs.entry(item.path.clone()).or_default();
            }
            listing.dirs.entry(parent.to_owned()).or_default().push(node);
        }
    }
    listing
}

/// Adds one listed folder level (from a non-recursive tree request).
fn add_level(listing: &mut Listing, path: &str, items: &[TreeItem]) {
    let mut nodes = Vec::new();
    for item in items {
        if let Some(node) = Node::from_item(&item.path, item) {
            if node.kind == NodeKind::Dir {
                let child = if path.is_empty() { node.name.clone() } else { format!("{path}/{}", node.name) };
                listing.unlisted.insert(child, node.sha.clone());
            }
            nodes.push(node);
        }
    }
    listing.unlisted.remove(path);
    listing.dirs.insert(path.to_owned(), nodes);
}

fn dir_entry(name: String, modified_ms: Option<i64>) -> VfsEntry {
    VfsEntry {
        hidden: false,
        meta: EntryMeta { is_dir: true, modified_ms, ..Default::default() },
        name,
    }
}

impl Vfs for GithubFs {
    fn read_dir(&self, dir: &str) -> Result<Vec<VfsEntry>> {
        let listed = match self.locate(dir) {
            Loc::Owners => self.list_owners(),
            Loc::Repos { owner } => self.list_repos(&owner),
            Loc::Refs { owner, repo } => self.list_refs(&owner, &repo),
            Loc::Tree { owner, repo, git_ref, path } => self.snapshot(&owner, &repo, &git_ref).and_then(|snap| {
                Ok(self
                    .dir(&snap, &path)?
                    .into_iter()
                    .map(|n| VfsEntry { hidden: n.name.starts_with('.'), meta: n.meta(snap.modified_ms), name: n.name })
                    .collect())
            }),
        };
        listed.map_err(|e| CoreError::remote(dir, e))
    }

    fn open(&self, path: &str) -> Result<Box<dyn Read + Send>> {
        self.open_at(path, 0).map_err(|e| CoreError::remote(path, e))
    }

    fn join(&self, dir: &str, name: &str) -> String {
        let d = dir.trim_matches('/');
        if d.is_empty() {
            name.to_owned()
        } else {
            format!("{d}/{name}")
        }
    }
}

impl VfsWrite for GithubFs {
    fn caps(&self) -> WriteCaps {
        WriteCaps::default()
    }

    fn stat(&self, path: &str) -> io::Result<Option<EntryMeta>> {
        let folder = Some(EntryMeta { is_dir: true, ..Default::default() });
        match self.locate(path) {
            Loc::Tree { owner, repo, git_ref, path } => {
                let snap = match self.snapshot(&owner, &repo, &git_ref) {
                    Ok(s) => s,
                    Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
                    Err(e) => return Err(e),
                };
                if path.is_empty() {
                    return Ok(folder);
                }
                Ok(self.node(&snap, &path)?.map(|n| n.meta(snap.modified_ms)))
            }
            _ => Ok(folder),
        }
    }

    fn open_at(&self, path: &str, offset: u64) -> io::Result<Box<dyn Read + Send>> {
        if offset > 0 {
            return Err(io::Error::new(io::ErrorKind::Unsupported, "GitHub can't resume a download"));
        }
        let (snap, node) = self.file(path)?;
        if node.kind == NodeKind::Submodule {
            return Ok(Box::new(io::Cursor::new(submodule_text(&node.sha).into_bytes())));
        }
        let url = format!("{}/git/blobs/{}", self.repo_url(&snap.owner, &snap.repo), node.sha);
        let resp = rt().block_on(send(|| self.request(&url).header(ACCEPT, "application/vnd.github.raw")))?;
        Ok(Box::new(NetRead(Body::new(resp))))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn fs(owner: Option<&str>, repo: Option<&str>) -> GithubFs {
        GithubFs {
            http: reqwest::Client::new(),
            api: "https://api.github.com".into(),
            token: None,
            owner: owner.map(Into::into),
            repo: repo.map(Into::into),
            login: None,
            my_repos: Mutex::new(None),
            refs: Mutex::new(HashMap::new()),
            snapshots: Mutex::new(HashMap::new()),
        }
    }

    #[test]
    fn refs_with_slashes_round_trip() {
        for r in ["main", "feature/login", "100%/done", "v1.2.3"] {
            assert_eq!(decode_ref(&encode_ref(r)), r);
            assert!(!encode_ref(r).contains('/'));
        }
        assert_eq!(decode_ref("feature%2fx"), "feature/x");
        assert_eq!(decode_ref("50%off"), "50%off");
    }

    #[test]
    fn paths_skip_what_the_profile_pins() {
        let free = fs(None, None);
        assert_eq!(free.locate(""), Loc::Owners);
        assert_eq!(free.locate("rust-lang"), Loc::Repos { owner: "rust-lang".into() });
        assert_eq!(
            free.locate("rust-lang/rust/feature%2Fx/src/lib.rs"),
            Loc::Tree {
                owner: "rust-lang".into(),
                repo: "rust".into(),
                git_ref: "feature/x".into(),
                path: "src/lib.rs".into()
            }
        );
        let pinned = fs(Some("me"), Some("app"));
        assert_eq!(pinned.locate("/"), Loc::Refs { owner: "me".into(), repo: "app".into() });
        assert_eq!(
            pinned.locate("main"),
            Loc::Tree { owner: "me".into(), repo: "app".into(), git_ref: "main".into(), path: String::new() }
        );
    }

    #[test]
    fn recursive_listing_builds_every_folder() {
        let item = |path: &str, kind: &str, mode: &str| TreeItem {
            path: path.into(),
            mode: mode.into(),
            kind: kind.into(),
            sha: "abc".into(),
            size: (kind == "blob").then_some(5),
        };
        let items = [
            item("src", "tree", "040000"),
            item("src/main.rs", "blob", "100644"),
            item("run.sh", "blob", "100755"),
            item("vendor/lib", "commit", "160000"),
            item("vendor", "tree", "040000"),
        ];
        let listing = from_recursive(&items);
        let names = |p: &str| listing.dirs[p].iter().map(|n| n.name.clone()).collect::<Vec<_>>();
        assert_eq!(names(""), ["src", "run.sh", "vendor"]);
        assert_eq!(names("src"), ["main.rs"]);
        let sub = &listing.dirs["vendor"][0];
        assert!(sub.kind == NodeKind::Submodule && sub.size == submodule_text("abc").len() as u64);
        assert_eq!(listing.dirs[""][1].mode, 0o755);
    }

    #[test]
    fn next_link_reads_the_link_header() {
        let link = r#"<https://api.github.com/x?page=2>; rel="next", <https://api.github.com/x?page=9>; rel="last""#;
        assert_eq!(next_link(link).as_deref(), Some("https://api.github.com/x?page=2"));
        assert_eq!(next_link(r#"<https://api.github.com/x?page=1>; rel="prev""#), None);
    }
}
