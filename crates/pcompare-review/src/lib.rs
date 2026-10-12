//! Comparison archives (`.pcmpz`): one zip file holding a whole comparison —
//! the compared texts (or a folder comparison's result tree plus the contents
//! of its differing text files), the session options, and the bookmarks and
//! review comments — so a reviewer on another machine, without either
//! original, can open it read-only.
//!
//! Layout of the zip:
//! - `manifest.json`: [`Manifest`] (format id, version, kind, title, author's app version)
//! - `comparison.json`: [`Comparison`] without the texts
//! - `text/left.txt`, `text/right.txt`, `text/base.txt`: the text sides, UTF-8
//! - `files/left/<relPath>`, `files/right/<relPath>`: embedded folder files, raw bytes
//!
//! Options and view settings are kept as plain JSON so a reader never drops
//! fields that a newer writer added.

use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use pcompare_core::textfile;
use pcompare_core::{CoreError, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use zip::result::ZipError;
use zip::write::SimpleFileOptions;

/// `manifest.json`'s `format`.
pub const FORMAT: &str = "pcompare-review";
/// The newest layout this crate writes and reads.
pub const VERSION: u32 = 1;
/// File extension, without the dot.
pub const EXTENSION: &str = "pcmpz";

const MANIFEST: &str = "manifest.json";
const COMPARISON: &str = "comparison.json";
/// No entry is read past this size: a damaged or hostile archive can't make
/// the reader allocate without bound.
const MAX_ENTRY: u64 = 512 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ReviewKind {
    Text,
    Folder,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Manifest {
    pub format: String,
    pub version: u32,
    pub kind: ReviewKind,
    #[serde(default)]
    pub title: String,
    /// When the archive was written (ms since the Unix epoch).
    #[serde(default)]
    pub created_ms: i64,
    /// Version of the PCompare that wrote it.
    #[serde(default)]
    pub app_version: String,
}

impl Manifest {
    /// A manifest for an archive written now.
    pub fn new(kind: ReviewKind, title: impl Into<String>, app_version: impl Into<String>) -> Self {
        let created_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as i64);
        Manifest {
            format: FORMAT.into(),
            version: VERSION,
            kind,
            title: title.into(),
            created_ms,
            app_version: app_version.into(),
        }
    }
}

/// One side of a text comparison.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TextSide {
    /// What the viewer calls the side (file name, "Clipboard"…).
    pub label: String,
    /// Where the text came from on the author's machine ("" for in-memory text).
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub encoding: String,
    #[serde(default)]
    pub has_bom: bool,
    /// `lf`, `crlf`, `cr`, `mixed` or `none`, as the author's viewer reported it.
    #[serde(default)]
    pub line_ending: String,
    /// Size of the original in bytes.
    #[serde(default)]
    pub size: u64,
    /// The text itself; stored in its own zip entry rather than in `comparison.json`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum BookmarkSide {
    Left,
    Right,
}

/// A bookmarked line (1-based) with an optional review comment.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TextBookmark {
    pub side: BookmarkSide,
    pub line: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TextComparison {
    pub left: TextSide,
    pub right: TextSide,
    /// Common ancestor of a 3-way comparison.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<TextSide>,
    /// Text rules (`TextDiffOptions`).
    #[serde(default)]
    pub options: Value,
    /// View settings of the session (display mode, word wrap…).
    #[serde(default)]
    pub settings: Value,
    #[serde(default)]
    pub bookmarks: Vec<TextBookmark>,
}

/// A bookmarked folder-compare row (by relative path) with an optional comment.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FolderBookmark {
    pub rel_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
}

/// A row whose file contents are in the archive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EmbeddedFile {
    pub rel_path: String,
    pub left: bool,
    pub right: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FolderComparison {
    pub left_root: String,
    pub right_root: String,
    /// The result as the view showed it (`FolderCompareResult`: entry tree with
    /// sizes, dates and statuses, plus totals), re-checks and ignore marks included.
    pub result: Value,
    /// Engine options (`FolderCompareOptions`).
    #[serde(default)]
    pub options: Value,
    /// View settings (display filter, flatten…).
    #[serde(default)]
    pub settings: Value,
    #[serde(default)]
    pub bookmarks: Vec<FolderBookmark>,
    /// Rows whose contents were embedded; set by [`write`].
    #[serde(default)]
    pub embedded: Vec<EmbeddedFile>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
// One value per archive: boxing the text variant would buy nothing.
#[allow(clippy::large_enum_variant)]
pub enum Comparison {
    Text(TextComparison),
    Folder(FolderComparison),
}

impl Comparison {
    pub fn kind(&self) -> ReviewKind {
        match self {
            Comparison::Text(_) => ReviewKind::Text,
            Comparison::Folder(_) => ReviewKind::Folder,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewArchive {
    pub manifest: Manifest,
    pub comparison: Comparison,
}

/// One folder row's file contents to embed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbeddedContent {
    pub rel_path: String,
    pub left: Option<Vec<u8>>,
    pub right: Option<Vec<u8>>,
}

/// Limits for embedding the files of a folder comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct EmbedOptions {
    /// Files larger than this (on either side) are left out.
    pub max_file_bytes: u64,
    /// Embedding stops once this many bytes are in.
    pub max_total_bytes: u64,
}

impl Default for EmbedOptions {
    fn default() -> Self {
        EmbedOptions {
            max_file_bytes: 1024 * 1024,
            max_total_bytes: 64 * 1024 * 1024,
        }
    }
}

/// Whether `path` names a comparison archive (by extension).
pub fn is_review_archive_path(path: &str) -> bool {
    Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case(EXTENSION))
}

fn zip_err(path: &Path, e: ZipError) -> CoreError {
    match e {
        ZipError::Io(source) => CoreError::io(path, source),
        e => CoreError::Other(format!("{}: {e}", path.display())),
    }
}

fn json_err(path: &Path, what: &str, e: serde_json::Error) -> CoreError {
    CoreError::Other(format!("{}: damaged {what}: {e}", path.display()))
}

fn text_entry(side: &str) -> String {
    format!("text/{side}.txt")
}

fn file_entry(side: &str, rel_path: &str) -> String {
    format!("files/{side}/{}", rel_path.trim_start_matches('/'))
}

/// Writes `archive` to `path`, embedding `contents` for a folder comparison
/// (ignored for text). The archive is written beside `path` first and moved
/// over it, so an existing archive is never left half-written.
pub fn write(path: &Path, archive: &ReviewArchive, contents: &[EmbeddedContent]) -> Result<()> {
    let tmp = temp_path(path);
    let res = write_to(&tmp, archive, contents)
        .and_then(|()| std::fs::rename(&tmp, path).map_err(|e| CoreError::io(path, e)));
    if res.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    res
}

fn temp_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{}.tmp", std::process::id()));
    path.with_file_name(name)
}

fn write_to(path: &Path, archive: &ReviewArchive, contents: &[EmbeddedContent]) -> Result<()> {
    let file = File::create(path).map_err(|e| CoreError::io(path, e))?;
    let mut zip = zip::ZipWriter::new(file);
    let opts = SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .large_file(false);
    let put = |zip: &mut zip::ZipWriter<File>, name: &str, bytes: &[u8]| -> Result<()> {
        let big = bytes.len() as u64 >= u32::MAX as u64;
        zip.start_file(name, opts.large_file(big))
            .map_err(|e| zip_err(path, e))?;
        zip.write_all(bytes).map_err(|e| CoreError::io(path, e))
    };

    let manifest = Manifest {
        format: FORMAT.into(),
        version: VERSION,
        kind: archive.comparison.kind(),
        ..archive.manifest.clone()
    };
    put(&mut zip, MANIFEST, &to_json(&manifest)?)?;

    match &archive.comparison {
        Comparison::Text(t) => {
            let strip = |s: &TextSide| TextSide {
                text: String::new(),
                ..s.clone()
            };
            let meta = Comparison::Text(TextComparison {
                left: strip(&t.left),
                right: strip(&t.right),
                base: t.base.as_ref().map(strip),
                ..t.clone()
            });
            put(&mut zip, COMPARISON, &to_json(&meta)?)?;
            put(&mut zip, &text_entry("left"), t.left.text.as_bytes())?;
            put(&mut zip, &text_entry("right"), t.right.text.as_bytes())?;
            if let Some(base) = &t.base {
                put(&mut zip, &text_entry("base"), base.text.as_bytes())?;
            }
        }
        Comparison::Folder(f) => {
            let embedded = contents
                .iter()
                .filter(|c| c.left.is_some() || c.right.is_some())
                .map(|c| EmbeddedFile {
                    rel_path: c.rel_path.clone(),
                    left: c.left.is_some(),
                    right: c.right.is_some(),
                })
                .collect();
            let meta = Comparison::Folder(FolderComparison {
                embedded,
                ..f.clone()
            });
            put(&mut zip, COMPARISON, &to_json(&meta)?)?;
            for c in contents {
                for (side, bytes) in [("left", &c.left), ("right", &c.right)] {
                    if let Some(bytes) = bytes {
                        put(&mut zip, &file_entry(side, &c.rel_path), bytes)?;
                    }
                }
            }
        }
    }
    let file = zip.finish().map_err(|e| zip_err(path, e))?;
    file.sync_all().map_err(|e| CoreError::io(path, e))
}

fn to_json<T: Serialize>(v: &T) -> Result<Vec<u8>> {
    serde_json::to_vec_pretty(v).map_err(|e| CoreError::Other(e.to_string()))
}

fn open_zip(path: &Path) -> Result<zip::ZipArchive<File>> {
    let file = File::open(path).map_err(|e| CoreError::io(path, e))?;
    zip::ZipArchive::new(file).map_err(|e| match e {
        ZipError::InvalidArchive(_) | ZipError::UnsupportedArchive(_) => {
            CoreError::Other(format!("{} is not a comparison archive", path.display()))
        }
        e => zip_err(path, e),
    })
}

/// An entry's bytes, or `None` when the archive doesn't have it.
fn read_entry(zip: &mut zip::ZipArchive<File>, path: &Path, name: &str) -> Result<Option<Vec<u8>>> {
    let entry = match zip.by_name(name) {
        Ok(f) => f,
        Err(ZipError::FileNotFound) => return Ok(None),
        Err(e) => return Err(zip_err(path, e)),
    };
    if entry.size() > MAX_ENTRY {
        return Err(CoreError::Other(format!(
            "{}: `{name}` is too large to open",
            path.display()
        )));
    }
    let mut out = Vec::with_capacity(entry.size() as usize);
    entry
        .take(MAX_ENTRY + 1)
        .read_to_end(&mut out)
        .map_err(|e| CoreError::io(path, e))?;
    Ok(Some(out))
}

fn read_text(zip: &mut zip::ZipArchive<File>, path: &Path, side: &str) -> Result<String> {
    let bytes = read_entry(zip, path, &text_entry(side))?.unwrap_or_default();
    String::from_utf8(bytes).map_err(|_| {
        CoreError::Other(format!(
            "{}: damaged text of the {side} side",
            path.display()
        ))
    })
}

/// Reads an archive: manifest, comparison and, for text, the texts. Folder
/// file contents stay in the archive; fetch them with [`read_embedded`].
pub fn read(path: &Path) -> Result<ReviewArchive> {
    let mut zip = open_zip(path)?;
    let not_ours = || CoreError::Other(format!("{} is not a comparison archive", path.display()));
    let manifest: Manifest =
        serde_json::from_slice(&read_entry(&mut zip, path, MANIFEST)?.ok_or_else(not_ours)?)
            .map_err(|e| json_err(path, "manifest", e))?;
    if manifest.format != FORMAT {
        return Err(not_ours());
    }
    if manifest.version > VERSION {
        return Err(CoreError::Other(format!(
            "{} was made by a newer PCompare (archive format {}, this version reads up to {VERSION}); update PCompare to open it",
            path.display(),
            manifest.version
        )));
    }
    let bytes = read_entry(&mut zip, path, COMPARISON)?.ok_or_else(not_ours)?;
    let mut comparison: Comparison =
        serde_json::from_slice(&bytes).map_err(|e| json_err(path, "comparison", e))?;
    if comparison.kind() != manifest.kind {
        return Err(CoreError::Other(format!(
            "{}: damaged archive (manifest and comparison disagree on the kind)",
            path.display()
        )));
    }
    if let Comparison::Text(t) = &mut comparison {
        t.left.text = read_text(&mut zip, path, "left")?;
        t.right.text = read_text(&mut zip, path, "right")?;
        if let Some(base) = &mut t.base {
            base.text = read_text(&mut zip, path, "base")?;
        }
    }
    Ok(ReviewArchive {
        manifest,
        comparison,
    })
}

/// The embedded contents of one folder row; a side that wasn't embedded is `None`.
pub fn read_embedded(path: &Path, rel_path: &str) -> Result<EmbeddedContent> {
    embedded_from(&mut open_zip(path)?, path, rel_path)
}

fn embedded_from(
    zip: &mut zip::ZipArchive<File>,
    path: &Path,
    rel_path: &str,
) -> Result<EmbeddedContent> {
    Ok(EmbeddedContent {
        rel_path: rel_path.to_owned(),
        left: read_entry(zip, path, &file_entry("left", rel_path))?,
        right: read_entry(zip, path, &file_entry("right", rel_path))?,
    })
}

/// Rewrites an archive after `edit` changed its comparison (new review
/// comments, say), keeping its manifest and embedded files.
pub fn update(path: &Path, edit: impl FnOnce(&mut Comparison)) -> Result<ReviewArchive> {
    let mut archive = read(path)?;
    let contents = match &archive.comparison {
        Comparison::Folder(f) => {
            let mut zip = open_zip(path)?;
            f.embedded
                .iter()
                .map(|e| embedded_from(&mut zip, path, &e.rel_path))
                .collect::<Result<Vec<_>>>()?
        }
        Comparison::Text(_) => Vec::new(),
    };
    edit(&mut archive.comparison);
    write(path, &archive, &contents)?;
    Ok(archive)
}

/// Embedded bytes decoded for display: the text and the detected encoding.
pub fn decode_embedded(bytes: &[u8]) -> (String, &'static str) {
    let d = textfile::decode_auto(bytes);
    (d.text, d.codec.name())
}

/// A folder entry as far as embedding needs it (a `FolderEntry` in JSON).
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct EntryView {
    rel_path: String,
    #[serde(default)]
    is_dir: bool,
    #[serde(default)]
    status: String,
    #[serde(default)]
    ignored: bool,
    left: Option<MetaView>,
    right: Option<MetaView>,
    #[serde(default)]
    children: Vec<EntryView>,
    left_rel_path: Option<String>,
}

#[derive(Deserialize)]
struct MetaView {
    #[serde(default)]
    size: u64,
}

/// Reads the differing text files of a folder comparison from disk, within
/// `opts`' limits: files on both sides whose status is different (or only
/// unimportantly different), skipping ignored rows, binary files, and sides
/// that can't be read locally (remote or archive roots).
pub fn collect_embedded(folder: &FolderComparison, opts: &EmbedOptions) -> Vec<EmbeddedContent> {
    let entries: Vec<EntryView> = folder
        .result
        .get("entries")
        .cloned()
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default();
    let mut out = Vec::new();
    let mut total = 0u64;
    let mut stack: Vec<&EntryView> = entries.iter().rev().collect();
    while let Some(e) = stack.pop() {
        stack.extend(e.children.iter().rev());
        if e.is_dir || e.ignored || !matches!(e.status.as_str(), "different" | "similar") {
            continue;
        }
        let (Some(l), Some(r)) = (&e.left, &e.right) else {
            continue;
        };
        if l.size > opts.max_file_bytes || r.size > opts.max_file_bytes {
            continue;
        }
        if total + l.size + r.size > opts.max_total_bytes {
            continue;
        }
        let left_rel = e.left_rel_path.as_deref().unwrap_or(&e.rel_path);
        let read = |root: &str, rel: &str| -> Option<Vec<u8>> {
            if root.is_empty() {
                return None;
            }
            let bytes = read_capped(&Path::new(root).join(rel), opts.max_file_bytes).ok()?;
            (!textfile::is_probably_binary(&bytes)).then_some(bytes)
        };
        let (Some(lb), Some(rb)) = (
            read(&folder.left_root, left_rel),
            read(&folder.right_root, &e.rel_path),
        ) else {
            continue;
        };
        total += (lb.len() + rb.len()) as u64;
        out.push(EmbeddedContent {
            rel_path: e.rel_path.clone(),
            left: Some(lb),
            right: Some(rb),
        });
    }
    out
}

/// Reads a file, failing when it grew past `max` since it was listed.
fn read_capped(path: &Path, max: u64) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    File::open(path)?.take(max + 1).read_to_end(&mut out)?;
    if out.len() as u64 > max {
        return Err(io::Error::other("file grew past the embedding limit"));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn side(label: &str, text: &str) -> TextSide {
        TextSide {
            label: label.into(),
            path: format!("/src/{label}"),
            encoding: "UTF-8".into(),
            has_bom: false,
            line_ending: "lf".into(),
            size: text.len() as u64,
            text: text.into(),
        }
    }

    fn text_archive() -> ReviewArchive {
        ReviewArchive {
            manifest: Manifest::new(ReviewKind::Text, "a.c ↔ b.c", "0.2.0"),
            comparison: Comparison::Text(TextComparison {
                left: side("a.c", "int main() {\n  return 0;\n}\n"),
                right: side("b.c", "int main() {\n  return 1; // ü\n}\n"),
                base: Some(side("base.c", "")),
                options: json!({ "whitespace": "ignoreAll", "futureOption": [1, 2] }),
                settings: json!({ "mode": "context" }),
                bookmarks: vec![
                    TextBookmark {
                        side: BookmarkSide::Right,
                        line: 2,
                        comment: Some("Why 1?\nShould stay 0.".into()),
                    },
                    TextBookmark {
                        side: BookmarkSide::Left,
                        line: 1,
                        comment: None,
                    },
                ],
            }),
        }
    }

    #[test]
    fn text_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("review.pcmpz");
        let archive = text_archive();
        write(&path, &archive, &[]).unwrap();
        assert_eq!(read(&path).unwrap(), archive);
        // Overwriting goes through a temp file and leaves nothing behind.
        write(&path, &archive, &[]).unwrap();
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn texts_live_outside_the_comparison_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("r.pcmpz");
        write(&path, &text_archive(), &[]).unwrap();
        let mut zip = open_zip(&path).unwrap();
        let meta =
            String::from_utf8(read_entry(&mut zip, &path, COMPARISON).unwrap().unwrap()).unwrap();
        assert!(!meta.contains("return 1"));
        assert!(meta.contains("futureOption"));
        let right = read_entry(&mut zip, &path, "text/right.txt")
            .unwrap()
            .unwrap();
        assert_eq!(right, "int main() {\n  return 1; // ü\n}\n".as_bytes());
    }

    #[test]
    fn folder_round_trip_with_embedded_files() {
        let dir = tempfile::tempdir().unwrap();
        let (l, r) = (dir.path().join("l"), dir.path().join("r"));
        for d in [&l, &r] {
            std::fs::create_dir_all(d.join("src")).unwrap();
        }
        std::fs::write(l.join("src/main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(r.join("src/main.rs"), "fn main() { run(); }\n").unwrap();
        std::fs::write(l.join("bin.dat"), [0u8, 1, 2]).unwrap();
        std::fs::write(r.join("bin.dat"), [0u8, 1, 3]).unwrap();
        std::fs::write(l.join("big.txt"), "x".repeat(50)).unwrap();
        std::fs::write(r.join("big.txt"), "y".repeat(50)).unwrap();
        std::fs::write(l.join("same.txt"), "same").unwrap();
        std::fs::write(r.join("same.txt"), "same").unwrap();
        let meta =
            |n: u64| json!({ "size": n, "modifiedMs": 1_700_000_000_000i64, "isDir": false });
        let file = |rel: &str, status: &str, ls: u64, rs: u64| {
            json!({ "name": rel.rsplit('/').next().unwrap(), "relPath": rel, "isDir": false, "status": status,
                    "newer": null, "left": meta(ls), "right": meta(rs), "children": [], "error": null,
                    "contentChecked": null, "crc": null })
        };
        let result = json!({
            "leftRoot": l.to_str().unwrap(),
            "rightRoot": r.to_str().unwrap(),
            "entries": [
                { "name": "src", "relPath": "src", "isDir": true, "status": "different", "newer": null,
                  "left": { "size": 0, "modifiedMs": null, "isDir": true }, "right": { "size": 0, "modifiedMs": null, "isDir": true },
                  "children": [file("src/main.rs", "different", 13, 21)], "error": null, "contentChecked": null, "crc": null },
                file("bin.dat", "different", 3, 3),
                file("big.txt", "different", 50, 50),
                file("same.txt", "same", 4, 4),
            ],
            "stats": { "same": 1, "similar": 0, "different": 3, "leftOnly": 0, "rightOnly": 0, "errors": 0 },
            "elapsedMs": 3
        });
        let folder = FolderComparison {
            left_root: l.to_string_lossy().into(),
            right_root: r.to_string_lossy().into(),
            result,
            options: json!({ "content": "binary" }),
            settings: json!({ "filter": "diffs" }),
            bookmarks: vec![FolderBookmark {
                rel_path: "src/main.rs".into(),
                comment: Some("run() is new".into()),
            }],
            embedded: vec![],
        };
        let contents = collect_embedded(
            &folder,
            &EmbedOptions {
                max_file_bytes: 40,
                max_total_bytes: 1000,
            },
        );
        // Binary, oversized and identical files stay out.
        assert_eq!(contents.len(), 1);
        assert_eq!(contents[0].rel_path, "src/main.rs");

        let path = dir.path().join("folders.pcmpz");
        let archive = ReviewArchive {
            manifest: Manifest::new(ReviewKind::Folder, "l ↔ r", "0.2.0"),
            comparison: Comparison::Folder(folder.clone()),
        };
        write(&path, &archive, &contents).unwrap();
        let back = read(&path).unwrap();
        let Comparison::Folder(f) = back.comparison else {
            panic!("kind")
        };
        assert_eq!(f.result, folder.result);
        assert_eq!(f.bookmarks, folder.bookmarks);
        assert_eq!(
            f.embedded,
            vec![EmbeddedFile {
                rel_path: "src/main.rs".into(),
                left: true,
                right: true
            }]
        );
        let EmbeddedContent {
            left: lb,
            right: rb,
            ..
        } = read_embedded(&path, "src/main.rs").unwrap();
        assert_eq!(decode_embedded(&lb.unwrap()).0, "fn main() {}\n");
        assert_eq!(rb.unwrap(), b"fn main() { run(); }\n");
        let missing = read_embedded(&path, "bin.dat").unwrap();
        assert_eq!((missing.left, missing.right), (None, None));

        // New comments are written back without losing the embedded files.
        update(&path, |c| {
            if let Comparison::Folder(f) = c {
                f.bookmarks[0].comment = Some("checked".into());
            }
        })
        .unwrap();
        let Comparison::Folder(f) = read(&path).unwrap().comparison else {
            panic!("kind")
        };
        assert_eq!(f.bookmarks[0].comment.as_deref(), Some("checked"));
        assert_eq!(f.embedded.len(), 1);
        assert!(read_embedded(&path, "src/main.rs").unwrap().right.is_some());
    }

    #[test]
    fn total_limit_stops_embedding() {
        let dir = tempfile::tempdir().unwrap();
        for side in ["l", "r"] {
            std::fs::create_dir_all(dir.path().join(side)).unwrap();
            for n in 0..3 {
                std::fs::write(
                    dir.path().join(side).join(format!("{n}.txt")),
                    format!("{side}{n}"),
                )
                .unwrap();
            }
        }
        let entries: Vec<Value> = (0..3)
            .map(|n| json!({ "relPath": format!("{n}.txt"), "status": "different", "left": { "size": 2 }, "right": { "size": 2 } }))
            .collect();
        let folder = FolderComparison {
            left_root: dir.path().join("l").to_string_lossy().into(),
            right_root: dir.path().join("r").to_string_lossy().into(),
            result: json!({ "entries": entries }),
            options: Value::Null,
            settings: Value::Null,
            bookmarks: vec![],
            embedded: vec![],
        };
        let opts = EmbedOptions {
            max_file_bytes: 10,
            max_total_bytes: 9,
        };
        assert_eq!(collect_embedded(&folder, &opts).len(), 2);
    }

    #[test]
    fn rejects_foreign_and_newer_archives() {
        let dir = tempfile::tempdir().unwrap();
        let plain = dir.path().join("plain.txt");
        std::fs::write(&plain, "hello").unwrap();
        assert!(read(&plain)
            .unwrap_err()
            .to_string()
            .contains("not a comparison archive"));

        let other = dir.path().join("other.zip");
        let mut zip = zip::ZipWriter::new(File::create(&other).unwrap());
        zip.start_file("readme.txt", SimpleFileOptions::default())
            .unwrap();
        zip.write_all(b"hi").unwrap();
        zip.finish().unwrap();
        assert!(read(&other)
            .unwrap_err()
            .to_string()
            .contains("not a comparison archive"));

        let newer = dir.path().join("newer.pcmpz");
        let mut zip = zip::ZipWriter::new(File::create(&newer).unwrap());
        zip.start_file(MANIFEST, SimpleFileOptions::default())
            .unwrap();
        zip.write_all(br#"{"format":"pcompare-review","version":99,"kind":"text"}"#)
            .unwrap();
        zip.finish().unwrap();
        assert!(read(&newer)
            .unwrap_err()
            .to_string()
            .contains("newer PCompare"));
    }

    #[test]
    fn older_bookmarks_without_comments_still_read() {
        let b: TextBookmark = serde_json::from_str(r#"{"side":"left","line":4}"#).unwrap();
        assert_eq!(b.comment, None);
        assert!(is_review_archive_path("C:\\x\\Review.PCMPZ"));
        assert!(!is_review_archive_path("/x/review.zip"));
    }
}
