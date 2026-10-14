//! SharePoint document libraries through Microsoft Graph.
//!
//! OpenDAL's OneDrive service only reaches the signed-in user's own drive
//! (`/me/drive`). A SharePoint library is a drive of its own, addressed as
//! `/drives/{id}`, so the site address in the profile is resolved to that
//! id once per connection and every request goes through it.

use crate::profile::ResolvedProfile;
use crate::{rt, NetRead};
use chrono::{DateTime, SecondsFormat, Utc};
use pcompare_core::folder::EntryMeta;
use pcompare_core::vfs::{Vfs, VfsEntry};
use pcompare_core::vfs_write::{unsupported, VfsWrite, WriteCaps};
use pcompare_core::{CoreError, Result};
use reqwest::header::{CONTENT_RANGE, RANGE, RETRY_AFTER};
use reqwest::{Method, RequestBuilder, StatusCode};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{json, Value};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::time::Duration;

const GRAPH: &str = "https://graph.microsoft.com/v1.0";
/// Largest file Graph accepts in a single PUT.
const SIMPLE_UPLOAD_MAX: usize = 4 << 20;
/// Upload-session chunks must be a multiple of 320 KiB; this is 10 MiB.
const UPLOAD_CHUNK: usize = 320 * 1024 * 32;
const SELECT: &str = "id,name,size,folder,file,fileSystemInfo,lastModifiedDateTime";
/// Tries per request when Graph throttles (429/503) or the link drops.
const ATTEMPTS: u32 = 4;

pub struct GraphFs {
    http: reqwest::Client,
    token: String,
    drive: String,
    /// The profile's root folder inside the library, without slashes.
    root: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Item {
    #[serde(default)]
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    size: u64,
    folder: Option<Value>,
    file: Option<Value>,
    file_system_info: Option<FsInfo>,
    last_modified_date_time: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FsInfo {
    last_modified_date_time: Option<String>,
}

impl Item {
    fn meta(&self) -> EntryMeta {
        // fileSystemInfo carries the time a sync set; lastModifiedDateTime
        // is when the service last touched the item.
        let modified = self
            .file_system_info
            .as_ref()
            .and_then(|f| f.last_modified_date_time.as_deref())
            .or(self.last_modified_date_time.as_deref());
        let is_dir = self.folder.is_some();
        EntryMeta {
            size: if is_dir { 0 } else { self.size },
            modified_ms: modified.and_then(parse_ms),
            is_dir,
            ..Default::default()
        }
    }
}

#[derive(Deserialize)]
struct Page<T> {
    value: Vec<T>,
    #[serde(rename = "@odata.nextLink")]
    next: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Drive {
    id: String,
    name: String,
    #[serde(default)]
    web_url: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UploadSession {
    upload_url: String,
}

fn parse_ms(s: &str) -> Option<i64> {
    DateTime::parse_from_rfc3339(s).ok().map(|t| t.timestamp_millis())
}

fn format_ms(ms: i64) -> Option<String> {
    DateTime::<Utc>::from_timestamp_millis(ms).map(|t| t.to_rfc3339_opts(SecondsFormat::Millis, true))
}

/// Percent-encodes one path segment (everything but RFC 3986 unreserved).
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

fn decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = b.get(i + 1..i + 3).and_then(|h| std::str::from_utf8(h).ok());
        match (b[i], hex.and_then(|h| u8::from_str_radix(h, 16).ok())) {
            (b'%', Some(v)) => {
                out.push(v);
                i += 3;
            }
            (c, _) => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `("a/b", "c")` for `"a/b/c"`, `("", "c")` for `"c"`.
fn split_parent(path: &str) -> (&str, &str) {
    path.rsplit_once('/').unwrap_or(("", path))
}

/// Where a pasted SharePoint address points.
#[derive(Debug, PartialEq)]
struct SiteAddress {
    host: String,
    /// `/sites/Name`, `/teams/Name`, or empty for the tenant's root site.
    path: String,
    /// The library a link into one names (`Shared Documents`), if any.
    library: Option<String>,
}

impl SiteAddress {
    /// Accepts what people copy from the browser: the site's home page, a
    /// library view (`…/Shared%20Documents/Forms/AllItems.aspx`) or a folder.
    fn parse(input: &str) -> Option<Self> {
        let input = input.trim();
        let url = if input.contains("://") {
            reqwest::Url::parse(input)
        } else {
            reqwest::Url::parse(&format!("https://{input}"))
        }
        .ok()?;
        let host = url.host_str()?.to_owned();
        let segs: Vec<&str> = url.path_segments().map(|s| s.filter(|x| !x.is_empty()).collect()).unwrap_or_default();
        if segs.first().is_some_and(|s| s.starts_with(':')) {
            return None; // a sharing link (`/:f:/s/…`), not an address
        }
        let (path, rest) = match segs.as_slice() {
            [kind, name, rest @ ..] if ["sites", "teams", "personal"].contains(&kind.to_ascii_lowercase().as_str()) => {
                (format!("/{kind}/{name}"), rest)
            }
            rest => (String::new(), rest),
        };
        let not_library = |s: &&&str| {
            s.starts_with('_') || s.contains('.') || ["SitePages", "Lists", "SiteAssets"].iter().any(|n| s.eq_ignore_ascii_case(n))
        };
        let library = rest.first().filter(|s| !not_library(s)).map(|s| decode(s));
        Some(Self { host, path, library })
    }

    fn graph_url(&self) -> String {
        if self.path.is_empty() {
            format!("{GRAPH}/sites/{}", self.host)
        } else {
            format!("{GRAPH}/sites/{}:{}", self.host, self.path)
        }
    }
}

/// The library a profile means by `wanted`: its display name ("Documents")
/// or the name in its address ("Shared Documents"), ignoring case.
fn pick_drive<'a>(drives: &'a [Drive], wanted: &str) -> Option<&'a Drive> {
    let wanted = wanted.trim();
    let url_name = |d: &Drive| d.web_url.trim_end_matches('/').rsplit('/').next().map(decode);
    drives
        .iter()
        .find(|d| d.name.eq_ignore_ascii_case(wanted))
        .or_else(|| drives.iter().find(|d| url_name(d).is_some_and(|n| n.eq_ignore_ascii_case(wanted))))
}

fn status_err(status: StatusCode, body: &str) -> io::Error {
    use io::ErrorKind as K;
    let message = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| v["error"]["message"].as_str().map(str::to_owned))
        .unwrap_or_else(|| body.chars().take(300).collect());
    let kind = match status.as_u16() {
        404 => K::NotFound,
        401 | 403 => K::PermissionDenied,
        409 => K::AlreadyExists,
        // Checked out, or open in Office for the web.
        423 => K::ResourceBusy,
        400 => K::InvalidInput,
        507 => K::StorageFull,
        429 | 500..=599 => K::ConnectionReset,
        _ => K::Other,
    };
    io::Error::new(kind, format!("{status}: {message}"))
}

pub(crate) fn body_err(e: reqwest::Error) -> io::Error {
    let kind = if e.is_decode() {
        io::ErrorKind::InvalidData
    } else {
        io::ErrorKind::ConnectionReset
    };
    io::Error::new(kind, e.to_string())
}

/// Sends the request `make` builds, waiting out throttling (honouring
/// Retry-After) and retrying dropped connections.
async fn send(make: impl Fn() -> RequestBuilder) -> io::Result<reqwest::Response> {
    let mut attempt = 1;
    loop {
        let wait = match make().send().await {
            Ok(resp) if resp.status().is_success() => return Ok(resp),
            Ok(resp) => {
                let status = resp.status();
                if !matches!(status.as_u16(), 429 | 500 | 502 | 503 | 504) || attempt >= ATTEMPTS {
                    let body = resp.text().await.unwrap_or_default();
                    return Err(status_err(status, &body));
                }
                let after = resp.headers().get(RETRY_AFTER).and_then(|v| v.to_str().ok()?.parse::<u64>().ok());
                after.map_or(Duration::from_millis(500 << attempt), |s| Duration::from_secs(s.min(60)))
            }
            Err(e) if attempt < ATTEMPTS && (e.is_connect() || e.is_timeout()) => Duration::from_millis(500 << attempt),
            Err(e) => return Err(io::Error::new(io::ErrorKind::ConnectionReset, e.to_string())),
        };
        tokio::time::sleep(wait).await;
        attempt += 1;
    }
}

/// A download, read synchronously chunk by chunk.
pub(crate) struct Body {
    resp: reqwest::Response,
    buf: Vec<u8>,
    pos: usize,
}

impl Body {
    pub(crate) fn new(resp: reqwest::Response) -> Self {
        Self { resp, buf: Vec::new(), pos: 0 }
    }
}

impl Read for Body {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        while self.pos == self.buf.len() {
            match rt().block_on(self.resp.chunk()).map_err(body_err)? {
                Some(chunk) => {
                    self.buf = chunk.to_vec();
                    self.pos = 0;
                }
                None => return Ok(0),
            }
        }
        let n = out.len().min(self.buf.len() - self.pos);
        out[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

impl GraphFs {
    pub fn connect(p: &ResolvedProfile, access_token: &str) -> Result<Self> {
        let name = p.profile.name.as_str();
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(30))
            .read_timeout(Duration::from_secs(120))
            .build()
            .map_err(|e| CoreError::remote(name, e))?;
        let mut fs = Self {
            http,
            token: access_token.to_owned(),
            drive: String::new(),
            root: p.get("root").unwrap_or_default().trim_matches('/').to_owned(),
        };
        let site_url = p.get("site_url").ok_or_else(|| CoreError::Other("enter the SharePoint site's address".into()))?;
        let site = SiteAddress::parse(site_url).ok_or_else(|| {
            CoreError::Other(format!("`{site_url}` isn't a SharePoint site address — open the site in a browser and copy its address"))
        })?;
        let err = |e: io::Error| CoreError::remote(name, e);
        let site_id = fs.json::<Item>(&format!("{}?$select=id", site.graph_url())).map_err(err)?.id;
        let drives_url = format!("{GRAPH}/sites/{site_id}");
        fs.drive = match p.get("library").or(site.library.as_deref()) {
            None => fs.json::<Item>(&format!("{drives_url}/drive?$select=id")).map_err(err)?.id,
            Some(wanted) => {
                let drives = fs.all::<Drive>(format!("{drives_url}/drives?$select=id,name,webUrl")).map_err(err)?;
                match pick_drive(&drives, wanted) {
                    Some(d) => d.id.clone(),
                    None => {
                        let names: Vec<&str> = drives.iter().map(|d| d.name.as_str()).collect();
                        return Err(CoreError::Other(format!(
                            "the site has no library `{wanted}` — it has: {}",
                            names.join(", ")
                        )));
                    }
                }
            }
        };
        Ok(fs)
    }

    fn request(&self, method: Method, url: &str) -> RequestBuilder {
        self.http.request(method, url).bearer_auth(&self.token)
    }

    fn send(&self, make: impl Fn() -> RequestBuilder) -> io::Result<reqwest::Response> {
        rt().block_on(send(make))
    }

    fn json<T: DeserializeOwned>(&self, url: &str) -> io::Result<T> {
        rt().block_on(async { send(|| self.request(Method::GET, url)).await?.json::<T>().await.map_err(body_err) })
    }

    /// Every item of a paged collection.
    fn all<T: DeserializeOwned>(&self, mut url: String) -> io::Result<Vec<T>> {
        let mut out = Vec::new();
        loop {
            let page: Page<T> = self.json(&url)?;
            out.extend(page.value);
            match page.next {
                Some(next) => url = next,
                None => return Ok(out),
            }
        }
    }

    /// The profile's root plus `path`: a path from the library's top.
    fn full(&self, path: &str) -> String {
        let p = path.trim_matches('/');
        match (self.root.is_empty(), p.is_empty()) {
            (true, _) => p.to_owned(),
            (false, true) => self.root.clone(),
            (false, false) => format!("{}/{p}", self.root),
        }
    }

    /// The URL of the item at `full`, optionally followed by an action such
    /// as `children` or `content`.
    fn item_url(&self, full: &str, action: &str) -> String {
        let base = format!("{GRAPH}/drives/{}/root", self.drive);
        if full.is_empty() {
            return if action.is_empty() { base } else { format!("{base}/{action}") };
        }
        let path: Vec<String> = full.split('/').map(encode_segment).collect();
        let path = path.join("/");
        if action.is_empty() {
            format!("{base}:/{path}")
        } else {
            format!("{base}:/{path}:/{action}")
        }
    }

    fn stat_full(&self, full: &str) -> io::Result<Option<EntryMeta>> {
        match self.json::<Item>(&format!("{}?$select={SELECT}", self.item_url(full, ""))) {
            Ok(item) => Ok(Some(item.meta())),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn make_dir(&self, parent: &str, name: &str) -> io::Result<()> {
        let url = self.item_url(parent, "children");
        let body = json!({ "name": name, "folder": {}, "@microsoft.graph.conflictBehavior": "fail" });
        match self.send(|| self.request(Method::POST, &url).json(&body)) {
            // Someone else created it meanwhile.
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(()),
            r => r.map(drop),
        }
    }

    fn delete(&self, full: &str) -> io::Result<()> {
        let url = self.item_url(full, "");
        match self.send(|| self.request(Method::DELETE, &url)) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            r => r.map(drop),
        }
    }

    /// Uploads `total` bytes from `data` into an upload session.
    fn upload_chunks(
        &self,
        upload_url: &str,
        data: &mut std::fs::File,
        total: u64,
        on_bytes: &mut dyn FnMut(u64) -> io::Result<()>,
    ) -> io::Result<()> {
        let mut buf = vec![0u8; UPLOAD_CHUNK];
        let mut start = 0u64;
        while start < total {
            let len = (total - start).min(UPLOAD_CHUNK as u64) as usize;
            data.read_exact(&mut buf[..len])?;
            let range = format!("bytes {start}-{}/{total}", start + len as u64 - 1);
            let chunk = &buf[..len];
            // The upload URL is pre-authorised; Graph refuses a bearer token on it.
            self.send(|| self.http.put(upload_url).header(CONTENT_RANGE, &range).body(chunk.to_vec()))?;
            on_bytes(len as u64)?;
            start += len as u64;
        }
        Ok(())
    }
}

impl Vfs for GraphFs {
    fn read_dir(&self, dir: &str) -> Result<Vec<VfsEntry>> {
        let full = self.full(dir);
        let url = format!("{}?$select={SELECT}&$top=1000", self.item_url(&full, "children"));
        let items = self.all::<Item>(url).map_err(|e| CoreError::remote(dir, e))?;
        Ok(items
            .into_iter()
            // OneNote notebooks and other packages are neither, and can't be read.
            .filter(|i| i.file.is_some() || i.folder.is_some())
            .map(|i| VfsEntry {
                hidden: i.name.starts_with('.'),
                meta: i.meta(),
                name: i.name,
            })
            .collect())
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

impl VfsWrite for GraphFs {
    fn caps(&self) -> WriteCaps {
        WriteCaps {
            rename: true,
            set_modified: true,
            resume_write: false,
            range_read: true,
            folders: true,
        }
    }

    fn stat(&self, path: &str) -> io::Result<Option<EntryMeta>> {
        self.stat_full(&self.full(path))
    }

    fn open_at(&self, path: &str, offset: u64) -> io::Result<Box<dyn Read + Send>> {
        // Graph answers with a redirect to a pre-authorised download URL;
        // reqwest follows it and drops the bearer token on the way.
        let url = self.item_url(&self.full(path), "content");
        let resp = self.send(|| {
            let req = self.request(Method::GET, &url);
            if offset > 0 {
                req.header(RANGE, format!("bytes={offset}-"))
            } else {
                req
            }
        })?;
        let ignored_range = offset > 0 && resp.status() == StatusCode::OK;
        let mut body = Body { resp, buf: Vec::new(), pos: 0 };
        if ignored_range {
            io::copy(&mut (&mut body).take(offset), &mut io::sink())?;
        }
        Ok(Box::new(NetRead(body)))
    }

    fn write(
        &self,
        path: &str,
        offset: u64,
        src: &mut dyn Read,
        on_bytes: &mut dyn FnMut(u64) -> io::Result<()>,
    ) -> io::Result<u64> {
        if offset > 0 {
            return Err(unsupported("resuming an upload"));
        }
        let full = self.full(path);
        let mut head = Vec::new();
        (&mut *src).take(SIMPLE_UPLOAD_MAX as u64 + 1).read_to_end(&mut head)?;
        if head.len() <= SIMPLE_UPLOAD_MAX {
            let url = self.item_url(&full, "content");
            self.send(|| self.request(Method::PUT, &url).body(head.clone()))?;
            on_bytes(head.len() as u64)?;
            return Ok(head.len() as u64);
        }
        // An upload session needs the total size up front, which a stream
        // doesn't tell; spool it to an anonymous temp file first.
        let mut spool = tempfile::tempfile()?;
        spool.write_all(&head)?;
        let total = head.len() as u64 + io::copy(src, &mut spool)?;
        drop(head);
        spool.seek(SeekFrom::Start(0))?;
        let url = self.item_url(&full, "createUploadSession");
        let body = json!({ "item": { "@microsoft.graph.conflictBehavior": "replace" } });
        let session: UploadSession = rt().block_on(async {
            send(|| self.request(Method::POST, &url).json(&body)).await?.json().await.map_err(body_err)
        })?;
        // Nothing appears at `path` until the last chunk lands; on failure
        // the session is cancelled so the parts don't linger.
        let result = self.upload_chunks(&session.upload_url, &mut spool, total, on_bytes);
        if result.is_err() {
            let _ = rt().block_on(self.http.delete(&session.upload_url).send());
        }
        result.map(|()| total)
    }

    fn create_dir_all(&self, path: &str) -> io::Result<()> {
        let full = self.full(path);
        if full.is_empty() || self.stat_full(&full)?.is_some_and(|m| m.is_dir) {
            return Ok(());
        }
        let mut parent = String::new();
        for name in full.split('/').filter(|s| !s.is_empty()) {
            let here = if parent.is_empty() { name.to_owned() } else { format!("{parent}/{name}") };
            match self.stat_full(&here)? {
                Some(m) if m.is_dir => {}
                Some(_) => return Err(io::Error::new(io::ErrorKind::NotADirectory, format!("{here} is a file"))),
                None => self.make_dir(&parent, name)?,
            }
            parent = here;
        }
        Ok(())
    }

    /// Deleted items go to the site's recycle bin.
    fn remove_file(&self, path: &str) -> io::Result<()> {
        self.delete(&self.full(path))
    }

    fn remove_dir_all(&self, path: &str) -> io::Result<()> {
        if path.trim_matches('/').is_empty() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "refusing to delete the root folder"));
        }
        // Graph deletes a folder with everything in it.
        self.delete(&self.full(path))
    }

    fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        let (from, to) = (self.full(from), self.full(to));
        let (to_parent, to_name) = split_parent(&to);
        let mut body = json!({ "name": to_name });
        if split_parent(&from).0 != to_parent {
            let parent: Item = self.json(&format!("{}?$select=id", self.item_url(to_parent, "")))?;
            body["parentReference"] = json!({ "id": parent.id });
        }
        let url = self.item_url(&from, "");
        self.send(|| self.request(Method::PATCH, &url).json(&body)).map(drop)
    }

    fn set_modified(&self, path: &str, modified_ms: i64) -> io::Result<()> {
        let stamp = format_ms(modified_ms)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "timestamp out of range"))?;
        let body = json!({ "fileSystemInfo": { "lastModifiedDateTime": stamp } });
        let url = self.item_url(&self.full(path), "");
        self.send(|| self.request(Method::PATCH, &url).json(&body)).map(drop)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn site(url: &str) -> Option<(String, String, Option<String>)> {
        SiteAddress::parse(url).map(|s| (s.host, s.path, s.library))
    }

    #[test]
    fn site_addresses_from_the_browser() {
        let host = || "contoso.sharepoint.com".to_owned();
        assert_eq!(
            site("https://contoso.sharepoint.com/sites/Marketing"),
            Some((host(), "/sites/Marketing".into(), None))
        );
        assert_eq!(
            site("contoso.sharepoint.com/teams/Eng/Shared%20Documents/Forms/AllItems.aspx"),
            Some((host(), "/teams/Eng".into(), Some("Shared Documents".into())))
        );
        assert_eq!(
            site("https://contoso.sharepoint.com/sites/Marketing/SitePages/Home.aspx"),
            Some((host(), "/sites/Marketing".into(), None))
        );
        assert_eq!(site("https://contoso.sharepoint.com/"), Some((host(), String::new(), None)));
        assert_eq!(
            site("https://contoso.sharepoint.com/Shared%20Documents/Specs"),
            Some((host(), String::new(), Some("Shared Documents".into())))
        );
        assert_eq!(site("https://contoso.sharepoint.com/:f:/s/Team/EkAbc"), None);
    }

    #[test]
    fn site_urls_for_graph() {
        let s = SiteAddress::parse("https://contoso.sharepoint.com/sites/Marketing/Shared%20Documents").unwrap();
        assert_eq!(s.graph_url(), format!("{GRAPH}/sites/contoso.sharepoint.com:/sites/Marketing"));
        let root = SiteAddress::parse("https://contoso.sharepoint.com").unwrap();
        assert_eq!(root.graph_url(), format!("{GRAPH}/sites/contoso.sharepoint.com"));
    }

    #[test]
    fn libraries_match_by_name_or_address() {
        let drive = |name: &str, url: &str| Drive {
            id: name.to_owned(),
            name: name.to_owned(),
            web_url: url.to_owned(),
        };
        let drives = [
            drive("Documents", "https://c.sharepoint.com/sites/x/Shared%20Documents"),
            drive("Contracts", "https://c.sharepoint.com/sites/x/Contracts"),
        ];
        assert_eq!(pick_drive(&drives, "documents").unwrap().id, "Documents");
        assert_eq!(pick_drive(&drives, "Shared Documents").unwrap().id, "Documents");
        assert_eq!(pick_drive(&drives, " Contracts ").unwrap().id, "Contracts");
        assert!(pick_drive(&drives, "Archive").is_none());
    }

    #[test]
    fn item_urls_encode_each_segment() {
        let fs = GraphFs {
            http: reqwest::Client::new(),
            token: String::new(),
            drive: "b!abc".into(),
            root: "Team Files".into(),
        };
        let base = format!("{GRAPH}/drives/b!abc/root");
        assert_eq!(fs.item_url(&fs.full(""), "children"), format!("{base}:/Team%20Files:/children"));
        assert_eq!(fs.item_url(&fs.full("/a#1/b%.txt"), ""), format!("{base}:/Team%20Files/a%231/b%25.txt"));
        assert_eq!(fs.item_url("", "children"), format!("{base}/children"));
    }

    #[test]
    fn timestamps_round_trip() {
        let ms = 1_760_000_000_123;
        assert_eq!(parse_ms(&format_ms(ms).unwrap()), Some(ms));
        assert_eq!(parse_ms("2026-10-14T09:30:00Z"), Some(1_791_970_200_000));
    }

    #[test]
    fn graph_errors_keep_their_meaning() {
        let e = status_err(StatusCode::NOT_FOUND, r#"{"error":{"code":"itemNotFound","message":"The resource could not be found."}}"#);
        assert_eq!(e.kind(), io::ErrorKind::NotFound);
        assert!(e.to_string().contains("could not be found"));
        assert_eq!(status_err(StatusCode::LOCKED, "").kind(), io::ErrorKind::ResourceBusy);
    }
}
