//! Tauri commands for editable Text Compare sessions and 3-way Text Merge.

use crate::CmdResult;
use pcompare_core::text::{self, TextDiffOptions, TextDiffResult};
use pcompare_core::textfile::{self, LineEnding, TextFileInfo, TextFormat};
use pcompare_core::CoreError;
use pcompare_merge::MergeResult;
use serde::Serialize;
use std::path::Path;
use tauri::AppHandle;

/// A text file split into lines, plus what is needed to save it back as-is.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TextDocument {
    info: TextFileInfo,
    /// Encoding, BOM and line-ending style used when saving; mixed or missing
    /// line endings resolve to the dominant or native style.
    format: TextFormat,
    /// Empty for binary files.
    lines: Vec<String>,
    trailing_newline: bool,
    /// The file looks binary, so `lines` were not loaded.
    binary: bool,
}

/// Loads `local` (possibly a download-cache copy) and reports `display` as its path.
fn load_document(local: &Path, display: String) -> pcompare_core::Result<TextDocument> {
    if textfile::file_is_binary(local)? {
        // Only the metadata is needed; avoid decoding a large binary.
        let size = std::fs::metadata(local).map_err(|e| CoreError::io(local, e))?.len();
        return Ok(TextDocument {
            info: TextFileInfo {
                path: display,
                encoding: String::new(),
                has_bom: false,
                line_ending: LineEnding::None,
                size,
            },
            format: TextFormat {
                encoding: "UTF-8".into(),
                has_bom: false,
                line_ending: textfile::NATIVE_LINE_ENDING,
            },
            lines: Vec::new(),
            trailing_newline: false,
            binary: true,
        });
    }
    let loaded = textfile::load_text(local)?;
    let format = loaded.format();
    let (lines, trailing_newline) = pcompare_merge::split_lines(&loaded.text);
    let mut info = loaded.info;
    info.path = display;
    Ok(TextDocument {
        info,
        format,
        lines,
        trailing_newline,
        binary: false,
    })
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TextPair {
    left: TextDocument,
    right: TextDocument,
}

/// Loads both sides of a Text Compare session for in-memory editing.
#[tauri::command]
pub async fn load_text_pair(app: AppHandle, left: String, right: String) -> CmdResult<TextPair> {
    crate::blocking(move || {
        let (lp, rp) = crate::with_remote(&app, [&left, &right], |r| {
            Ok((r.local_copy(&left, true)?, r.local_copy(&right, true)?))
        })?;
        Ok(TextPair {
            left: load_document(&lp, left)?,
            right: load_document(&rp, right)?,
        })
    })
    .await
}

/// Re-diffs edited text held by the UI.
#[tauri::command]
pub async fn diff_texts(left: Vec<String>, right: Vec<String>, options: TextDiffOptions) -> CmdResult<TextDiffResult> {
    crate::blocking(move || {
        let (l, r): (Vec<&str>, Vec<&str>) = (
            left.iter().map(String::as_str).collect(),
            right.iter().map(String::as_str).collect(),
        );
        text::diff_lines(&l, &r, &options)
    })
    .await
}

/// Writes `text` to a local file in `format` (line breaks are normalized to
/// the format's style). Remote paths are rejected: they are read-only.
#[tauri::command]
pub async fn save_text_file(path: String, text: String, format: TextFormat) -> CmdResult<TextFileInfo> {
    if pcompare_remote::is_remote(&path) {
        return Err(format!("{path}: remote files are read-only"));
    }
    crate::blocking(move || textfile::save_text(&path, &text, &format)).await
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MergeLoad {
    left: TextDocument,
    base: TextDocument,
    right: TextDocument,
    merge: MergeResult,
    /// Auto-merged output with conflict markers around unresolved conflicts.
    output: String,
    /// Format for saving the output: the left file's (the version being
    /// merged into, "ours" in mergetool terms).
    output_format: TextFormat,
}

/// Loads the three inputs of a Text Merge session and merges them.
#[tauri::command]
pub async fn load_merge(
    app: AppHandle,
    left: String,
    base: String,
    right: String,
    options: TextDiffOptions,
) -> CmdResult<MergeLoad> {
    crate::blocking(move || {
        let (lp, rp) = crate::with_remote(&app, [&left, &right], |r| {
            Ok((r.local_copy(&left, true)?, r.local_copy(&right, true)?))
        })?;
        let bp = crate::with_remote(&app, [&base, ""], |r| r.local_copy(&base, true))?;
        let (left, base, right) = (load_document(&lp, left)?, load_document(&bp, base)?, load_document(&rp, right)?);
        if let Some(doc) = [&left, &base, &right].into_iter().find(|d| d.binary) {
            return Err(CoreError::Other(format!(
                "{} looks binary; Text Merge needs text files",
                doc.info.path
            )));
        }

        let merge = pcompare_merge::merge3(&base.lines, &left.lines, &right.lines, &options)?;
        let lines = pcompare_merge::render_output(&merge, &left.lines, &right.lines);
        let trailing =
            pcompare_merge::merge_trailing_newline(base.trailing_newline, left.trailing_newline, right.trailing_newline);
        Ok(MergeLoad {
            output: pcompare_merge::join_lines(&lines, trailing),
            output_format: left.format.clone(),
            left,
            base,
            right,
            merge,
        })
    })
    .await
}
