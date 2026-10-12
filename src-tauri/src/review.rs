//! Comparison archives (`.pcmpz`, araxis:text-review): saving a text or
//! folder comparison with its comments into one file, and opening one
//! read-only. The format lives in `pcompare-review`.

use std::path::PathBuf;

use pcompare_core::CoreError;
use pcompare_review::{
    self as review, Comparison, EmbedOptions, FolderBookmark, Manifest, ReviewArchive, ReviewKind,
    TextBookmark,
};
use serde::Serialize;
use serde_json::Value;

use crate::{blocking, CmdResult};

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewSaved {
    pub path: String,
    /// Folder comparisons: how many rows had their contents embedded.
    pub embedded: usize,
}

/// Writes `comparison` to `path`. For a folder comparison, `embed` reads the
/// differing text files from disk into the archive (within its limits).
#[tauri::command]
pub async fn review_save(
    path: String,
    title: String,
    comparison: Comparison,
    embed: Option<EmbedOptions>,
) -> CmdResult<ReviewSaved> {
    blocking(move || {
        let contents = match (&comparison, embed) {
            (Comparison::Folder(f), Some(opts)) => review::collect_embedded(f, &opts),
            _ => Vec::new(),
        };
        let archive = ReviewArchive {
            manifest: Manifest::new(comparison.kind(), title, env!("CARGO_PKG_VERSION")),
            comparison,
        };
        review::write(&PathBuf::from(&path), &archive, &contents)?;
        Ok(ReviewSaved {
            path,
            embedded: contents.len(),
        })
    })
    .await
}

#[tauri::command]
pub async fn review_open(path: String) -> CmdResult<ReviewArchive> {
    blocking(move || review::read(&PathBuf::from(path))).await
}

/// Writes a reviewer's bookmarks and comments back into an archive. The shape
/// follows the archive's kind: `{side, line, comment?}` rows for text,
/// `{relPath, comment?}` rows for folders.
#[tauri::command]
pub async fn review_save_bookmarks(path: String, bookmarks: Value) -> CmdResult<()> {
    blocking(move || {
        let path = PathBuf::from(path);
        let bad = |e: serde_json::Error| CoreError::Other(format!("bookmarks: {e}"));
        // Checked before rewriting, so a bad request leaves the archive alone.
        match review::read(&path)?.comparison.kind() {
            ReviewKind::Text => {
                let marks: Vec<TextBookmark> = serde_json::from_value(bookmarks).map_err(bad)?;
                review::update(&path, |c| {
                    if let Comparison::Text(t) = c {
                        t.bookmarks = marks;
                    }
                })?;
            }
            ReviewKind::Folder => {
                let marks: Vec<FolderBookmark> = serde_json::from_value(bookmarks).map_err(bad)?;
                review::update(&path, |c| {
                    if let Comparison::Folder(f) = c {
                        f.bookmarks = marks;
                    }
                })?;
            }
        }
        Ok(())
    })
    .await
}

/// One embedded side, decoded for display.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EmbeddedText {
    pub text: String,
    pub encoding: String,
    pub size: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EmbeddedPair {
    pub left: Option<EmbeddedText>,
    pub right: Option<EmbeddedText>,
}

/// The embedded contents of one folder row of an archive.
#[tauri::command]
pub async fn review_read_file(path: String, rel_path: String) -> CmdResult<EmbeddedPair> {
    blocking(move || {
        let c = review::read_embedded(&PathBuf::from(path), &rel_path)?;
        let decode = |bytes: Option<Vec<u8>>| {
            bytes.map(|b| {
                let (text, encoding) = review::decode_embedded(&b);
                EmbeddedText {
                    text,
                    encoding: encoding.to_owned(),
                    size: b.len() as u64,
                }
            })
        };
        Ok(EmbeddedPair {
            left: decode(c.left),
            right: decode(c.right),
        })
    })
    .await
}
