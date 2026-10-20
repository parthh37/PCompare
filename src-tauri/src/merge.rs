//! Tauri commands for editable Text Compare sessions and Text Merge (3-way,
//! or 2-way when no base file is given).

use crate::CmdResult;
use pcompare_core::backups::BackupOrigin;
use pcompare_core::text::{self, TextDiffOptions, TextDiffResult};
use pcompare_core::textfile::{self, LineEnding, TextFileInfo, TextFormat};
use pcompare_core::CoreError;
use pcompare_merge::{MergeResult, MergeRules};
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
/// the format's style). Remote paths are rejected: they are read-only. With
/// an `origin` (a merge's output, not an export), the file it replaces is
/// kept in the app's backups when they are on.
#[tauri::command]
pub async fn save_text_file(
    app: AppHandle,
    path: String,
    text: String,
    format: TextFormat,
    origin: Option<BackupOrigin>,
) -> CmdResult<TextFileInfo> {
    if pcompare_remote::is_remote(&path) {
        return Err(format!("{path}: remote files are read-only"));
    }
    if pcompare_remote::is_inside_container(&path) {
        return Err(pcompare_remote::read_only_inside(&path));
    }
    crate::blocking(move || {
        if let Some(origin) = origin {
            textfile::encode_text(&text, &format)?;
            crate::backups::keep_before_save(&app, Path::new(&path), origin)?;
        }
        textfile::save_text(&path, &text, &format)
    })
    .await
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MergeLoad {
    left: TextDocument,
    /// `None` in a two-way merge, where every difference is a conflict.
    base: Option<TextDocument>,
    right: TextDocument,
    merge: MergeResult,
    /// Auto-merged output with conflict markers around unresolved conflicts.
    output: String,
    /// Format for saving the output: the left file's (the version being
    /// merged into, "ours" in mergetool terms).
    output_format: TextFormat,
}

/// Loads the inputs of a Text Merge session and merges them: three-way
/// against `base`, or two-way when `base` is empty. `rules` defaults to
/// 2 conflict scan lines.
#[tauri::command]
pub async fn load_merge(
    app: AppHandle,
    left: String,
    base: String,
    right: String,
    options: TextDiffOptions,
    rules: Option<MergeRules>,
) -> CmdResult<MergeLoad> {
    let rules = rules.unwrap_or_default();
    // Without a grammar picked in the rules, the files' type decides (as in Text Compare).
    let mut options = options;
    if options.grammar.as_deref().is_none_or(|g| g.trim().is_empty()) {
        options.grammar = [&left, &right, &base]
            .into_iter()
            .find_map(|p| pcompare_core::grammar::for_path(p))
            .map(|g| g.id.to_owned());
    }
    crate::blocking(move || {
        let (lp, rp) = crate::with_remote(&app, [&left, &right], |r| {
            Ok((r.local_copy(&left, true)?, r.local_copy(&right, true)?))
        })?;
        let base = if base.is_empty() {
            None
        } else {
            let bp = crate::with_remote(&app, [&base, ""], |r| r.local_copy(&base, true))?;
            Some(load_document(&bp, base)?)
        };
        let (left, right) = (load_document(&lp, left)?, load_document(&rp, right)?);
        if let Some(doc) = [Some(&left), base.as_ref(), Some(&right)].into_iter().flatten().find(|d| d.binary) {
            return Err(CoreError::Other(format!(
                "{} looks binary; Text Merge needs text files",
                doc.info.path
            )));
        }

        // without a base every difference is a conflict to resolve.
        let (merge, trailing) = match &base {
            Some(base) => (
                pcompare_merge::merge3(&base.lines, &left.lines, &right.lines, &options, &rules)?,
                pcompare_merge::merge_trailing_newline(
                    base.trailing_newline,
                    left.trailing_newline,
                    right.trailing_newline,
                ),
            ),
            None => (pcompare_merge::merge2(&left.lines, &right.lines, &options)?, left.trailing_newline),
        };
        let lines = pcompare_merge::render_output(&merge, &left.lines, &right.lines);
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
