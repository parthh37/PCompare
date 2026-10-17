//! Reads saved sessions from other compare tools and maps each one onto a
//! PCompare session (kind, paths and the settings that have an equivalent).
//!
//! | Tool | File |
//! |---|---|
//! | Beyond Compare 3/4/5 | `BCSessions.xml`, or a `.bcpkg` settings export |
//! | WinMerge | `.WinMerge` project files |
//! | FreeFileSync | `.ffs_gui` / `.ffs_batch` configurations |
//! | Meld | `.meldcmp` comparison files |
//!
//! Files are recognised by content, not extension. Nothing here writes: the
//! app shows what was read and saves the sessions the user picks.

pub mod connections;

mod beyond_compare;
mod freefilesync;
mod meld;
mod winmerge;

use serde::Serialize;
use serde_json::{Map, Value};
use std::io::Read;
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum ImportError {
    #[error("couldn't read {path}: {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },
    #[error("{0}")]
    Format(String),
}

pub type Result<T> = std::result::Result<T, ImportError>;

pub(crate) fn format_err(msg: impl Into<String>) -> ImportError {
    ImportError::Format(msg.into())
}

/// One session read from another tool, already in PCompare's terms.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportedSession {
    pub name: String,
    /// A PCompare session kind (`pcompare_core::session::SESSION_KINDS`).
    pub kind: &'static str,
    pub left: String,
    pub right: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    /// Partial settings for the viewer; anything missing takes its default.
    pub settings: Value,
    /// The folder the session sits in, for tools that nest them ("Work/Site").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// Kept automatically rather than saved by name (BC's auto-saved sessions).
    pub auto_saved: bool,
    /// Options that had no PCompare equivalent.
    pub notes: Vec<String>,
    /// `kind` is just "two files": the app picks the viewer from the
    /// extensions (table, picture, hex…) as it does for dropped files.
    pub file_kind_by_extension: bool,
}

impl ImportedSession {
    fn new(
        name: impl Into<String>,
        kind: &'static str,
        left: impl Into<String>,
        right: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            kind,
            left: left.into(),
            right: right.into(),
            base: None,
            output: None,
            settings: Value::Object(Map::new()),
            group: None,
            auto_saved: false,
            notes: Vec::new(),
            file_kind_by_extension: false,
        }
    }

    fn set(&mut self, key: &str, value: impl Into<Value>) {
        if let Value::Object(m) = &mut self.settings {
            m.insert(key.to_owned(), value.into());
        }
    }
}

/// A session with no PCompare equivalent, and why.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Skipped {
    pub name: String,
    pub reason: String,
}

/// Everything read from one file.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportSource {
    /// The tool (and version, when the file says), e.g. "Beyond Compare 5".
    pub label: String,
    pub path: String,
    pub sessions: Vec<ImportedSession>,
    pub skipped: Vec<Skipped>,
}

/// What a single parser returns; `read` adds the path.
#[derive(Debug, Default)]
pub(crate) struct Parsed {
    pub label: String,
    pub sessions: Vec<ImportedSession>,
    pub skipped: Vec<Skipped>,
}

/// A session file found where another tool keeps it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Candidate {
    pub label: String,
    pub path: String,
}

/// Reads any supported session file, recognised by its content, or a
/// folder of Meld comparisons.
pub fn read(path: &Path) -> Result<ImportSource> {
    let parsed = if path.is_dir() {
        meld::parse_dir(path)?
    } else {
        let bytes = std::fs::read(path).map_err(|source| ImportError::Io {
            path: path.display().to_string(),
            source,
        })?;
        parse_bytes(&bytes, path)?
    };
    Ok(ImportSource {
        label: parsed.label,
        path: path.display().to_string(),
        sessions: parsed.sessions,
        skipped: parsed.skipped,
    })
}

/// `path` only resolves relative paths inside the file (WinMerge, FreeFileSync).
pub(crate) fn parse_bytes(bytes: &[u8], path: &Path) -> Result<Parsed> {
    if bytes.starts_with(b"PK\x03\x04") {
        return beyond_compare::parse_package(bytes);
    }
    let text = decode(bytes);
    let trimmed = text.trim_start();
    if trimmed.starts_with('[') {
        return meld::parse(&text);
    }
    if !trimmed.starts_with('<') {
        return Err(format_err("not a session file PCompare recognises"));
    }
    let doc = roxmltree::Document::parse_with_options(
        &text,
        roxmltree::ParsingOptions {
            allow_dtd: true,
            ..Default::default()
        },
    )
    .map_err(|e| format_err(format!("not valid XML: {e}")))?;
    let root = doc.root_element();
    let base_dir = path.parent().unwrap_or(Path::new(""));
    match root.tag_name().name() {
        "BCSessions" => beyond_compare::parse(root),
        "project" => winmerge::parse(root, base_dir),
        "FreeFileSync" => freefilesync::parse(root, path),
        other => Err(format_err(format!(
            "`<{other}>` files aren't a session format PCompare knows"
        ))),
    }
}

/// UTF-8 (with or without a BOM) or UTF-16 with a BOM; tools on Windows write both.
pub(crate) fn decode(bytes: &[u8]) -> String {
    let utf16 = |le: bool| {
        let units: Vec<u16> = bytes[2..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&c| {
                if le {
                    u16::from_le_bytes(c)
                } else {
                    u16::from_be_bytes(c)
                }
            })
            .collect();
        String::from_utf16_lossy(&units)
    };
    match bytes {
        [0xFF, 0xFE, ..] => utf16(true),
        [0xFE, 0xFF, ..] => utf16(false),
        [0xEF, 0xBB, 0xBF, rest @ ..] => String::from_utf8_lossy(rest).into_owned(),
        _ => String::from_utf8_lossy(bytes).into_owned(),
    }
}

/// Reads one file out of a zip (a `.bcpkg`), matching its name case-insensitively.
pub(crate) fn zip_entry(bytes: &[u8], name: &str) -> Result<Option<Vec<u8>>> {
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes))
        .map_err(|e| format_err(format!("not a valid package: {e}")))?;
    for i in 0..zip.len() {
        let mut file = zip.by_index(i).map_err(|e| format_err(e.to_string()))?;
        let file_name = file
            .name()
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or_default()
            .to_owned();
        if file_name.eq_ignore_ascii_case(name) {
            let mut out = Vec::new();
            file.read_to_end(&mut out)
                .map_err(|e| format_err(e.to_string()))?;
            return Ok(Some(out));
        }
    }
    Ok(None)
}

/// Session files of tools installed for this user, in their default places.
pub fn discover() -> Vec<Candidate> {
    let env = |k: &str| std::env::var_os(k).map(PathBuf::from);
    let mut out = Vec::new();
    let mut push = |label: String, path: PathBuf| {
        if path.is_file() && !out.iter().any(|c: &Candidate| Path::new(&c.path) == path) {
            out.push(Candidate {
                label,
                path: path.display().to_string(),
            });
        }
    };
    for (label, path) in beyond_compare::locations(env("APPDATA"), env("HOME")) {
        push(label, path);
    }
    for (label, path) in freefilesync::locations(env("APPDATA"), env("HOME")) {
        push(label, path);
    }
    // Meld's recent comparisons are a folder of small files; offer the folder.
    for dir in meld::location(env("LOCALAPPDATA"), env("HOME")) {
        let has_any = std::fs::read_dir(&dir)
            .into_iter()
            .flatten()
            .flatten()
            .any(|e| {
                e.path()
                    .extension()
                    .is_some_and(|x| x.eq_ignore_ascii_case("meldcmp"))
            });
        if has_any && !out.iter().any(|c| Path::new(&c.path) == dir) {
            out.push(Candidate {
                label: "Meld recent comparisons".into(),
                path: dir.display().to_string(),
            });
        }
    }
    out
}

/// Joins a relative path from a session file onto the file's folder;
/// absolute paths, URLs and empty strings pass through.
pub(crate) fn resolve(base_dir: &Path, p: &str) -> String {
    let p = p.trim();
    if p.is_empty()
        || p.contains("://")
        || is_absolute_anywhere(p)
        || base_dir.as_os_str().is_empty()
    {
        return p.to_owned();
    }
    base_dir.join(p).display().to_string()
}

/// `C:\x`, `\\server\share` and `/x` are absolute whichever OS reads the file.
fn is_absolute_anywhere(p: &str) -> bool {
    let b = p.as_bytes();
    p.starts_with('/')
        || p.starts_with("\\\\")
        || (b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_utf16_and_bom_utf8() {
        let mut le = vec![0xFF, 0xFE];
        le.extend("<a/>".encode_utf16().flat_map(|u| u.to_le_bytes()));
        assert_eq!(decode(&le), "<a/>");
        assert_eq!(decode(b"\xEF\xBB\xBF<a/>"), "<a/>");
    }

    #[test]
    fn relative_paths_resolve_against_the_file() {
        let base = Path::new("/projects");
        assert_eq!(
            resolve(base, "left"),
            Path::new("/projects").join("left").display().to_string()
        );
        assert_eq!(resolve(base, r"C:\abs"), r"C:\abs");
        assert_eq!(resolve(base, r"\\nas\share"), r"\\nas\share");
        assert_eq!(resolve(base, "sftp://host/x"), "sftp://host/x");
    }

    #[test]
    fn unknown_files_are_rejected_clearly() {
        let err = parse_bytes(b"<svg/>", Path::new("x.xml"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("<svg>"), "{err}");
        assert!(parse_bytes(b"hello", Path::new("x")).is_err());
    }
}
