//! Tauri commands for the Patch Viewer: reading a `.patch`/`.diff` file as
//! side-by-side hunks, and applying it against a folder to reconstruct the
//! patched files (opened in Text Compare with the patched text in memory).

use crate::CmdResult;
use pcompare_core::backups::{BackupOrigin, BackupReason};
use pcompare_core::patch::{self, ChangeKind, FileApply, Patch, PatchFormat, PatchHunk};
use pcompare_core::text::DiffRow;
use pcompare_core::{textfile, CoreError};
use pcompare_sync::fsops::{Deleter, TrashDeleter};
use serde::{Deserialize, Serialize};
use std::path::Path;
use tauri::AppHandle;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HunkView {
    old_start: usize,
    old_lines: usize,
    new_start: usize,
    new_lines: usize,
    section: String,
    rows: Vec<DiffRow>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileView {
    old_path: Option<String>,
    new_path: Option<String>,
    /// The path to show, with the patch's default prefix stripped.
    display: String,
    change: ChangeKind,
    binary: bool,
    old_mode: Option<String>,
    new_mode: Option<String>,
    similarity: Option<u32>,
    headers: Vec<String>,
    added: usize,
    removed: usize,
    hunks: Vec<HunkView>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PatchView {
    format: PatchFormat,
    preamble: String,
    strip: usize,
    warnings: Vec<String>,
    /// Encoding the patch file was read in.
    encoding: String,
    files: Vec<FileView>,
}

fn hunk_view(h: &PatchHunk) -> HunkView {
    HunkView {
        old_start: h.old_start,
        old_lines: h.old_lines,
        new_start: h.new_start,
        new_lines: h.new_lines,
        section: h.section.clone(),
        rows: patch::hunk_rows(h),
    }
}

/// Reads and parses a patch file (local or `remote://`).
fn read_patch(app: &AppHandle, path: &str) -> pcompare_core::Result<(Patch, String)> {
    let local = crate::with_remote(app, [path, ""], |r| r.local_copy(path, true))?;
    let loaded = textfile::load_text(&local)?;
    Ok((patch::parse_patch(&loaded.text)?, loaded.info.encoding))
}

fn local_base(base: &str) -> pcompare_core::Result<&Path> {
    if pcompare_remote::is_remote(base) {
        return Err(CoreError::Other(
            "patches can only be applied against a local folder".into(),
        ));
    }
    let dir = Path::new(base);
    if !dir.is_dir() {
        return Err(CoreError::Other(format!("{base}: not a folder")));
    }
    Ok(dir)
}

/// The patch's files with every hunk laid out as Text Compare rows.
#[tauri::command]
pub async fn patch_load(app: AppHandle, path: String) -> CmdResult<PatchView> {
    crate::blocking(move || {
        let (p, encoding) = read_patch(&app, &path)?;
        let files = p
            .files
            .iter()
            .map(|f| FileView {
                old_path: f.old_path.clone(),
                new_path: f.new_path.clone(),
                display: f.display_path(p.strip),
                change: f.change,
                binary: f.binary,
                old_mode: f.old_mode.clone(),
                new_mode: f.new_mode.clone(),
                similarity: f.similarity,
                headers: f.headers.clone(),
                added: f.added,
                removed: f.removed,
                hunks: f.hunks.iter().map(hunk_view).collect(),
            })
            .collect();
        Ok(PatchView {
            format: p.format,
            preamble: p.preamble,
            strip: p.strip,
            warnings: p.warnings,
            encoding,
            files,
        })
    })
    .await
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PatchCheck {
    /// The strip level used (detected when none was given).
    strip: usize,
    /// One entry per file of the patch, in order.
    files: Vec<FileApply>,
}

/// Dry-runs every file of the patch against `base`: which hunks apply, and
/// where. `strip` = `None` detects the level from the files that exist.
#[tauri::command]
pub async fn patch_check(
    app: AppHandle,
    path: String,
    base: String,
    strip: Option<usize>,
) -> CmdResult<PatchCheck> {
    crate::blocking(move || {
        let dir = local_base(&base)?;
        let (p, _) = read_patch(&app, &path)?;
        let strip = strip.unwrap_or_else(|| patch::detect_strip(&p, dir));
        let files = (0..p.files.len())
            .map(|i| {
                patch::apply_to_folder(&p, dir, strip, i, None, false).unwrap_or_else(failed_file)
            })
            .collect();
        Ok(PatchCheck { strip, files })
    })
    .await
}

/// A file whose patching failed outright.
fn failed_file(e: CoreError) -> FileApply {
    FileApply {
        original: None,
        target: None,
        missing: false,
        hunks: Vec::new(),
        error: Some(e.to_string()),
        text: None,
        written: None,
    }
}

/// Applies file `index` of the patch to its original under `base` and
/// returns the patched text (failed hunks are left out and reported).
/// `hunks` limits it to those hunk indices; absent = every hunk.
#[tauri::command]
pub async fn patch_apply_file(
    app: AppHandle,
    path: String,
    base: String,
    strip: usize,
    index: usize,
    hunks: Option<Vec<usize>>,
) -> CmdResult<FileApply> {
    crate::blocking(move || {
        let dir = local_base(&base)?;
        let (p, _) = read_patch(&app, &path)?;
        patch::apply_to_folder(&p, dir, strip, index, hunks.as_deref(), true)
    })
    .await
}

/// Which hunks of one file to write; `hunks` absent = all of them.
#[derive(Deserialize)]
pub struct FileSelection {
    index: usize,
    #[serde(default)]
    hunks: Option<Vec<usize>>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WrittenFile {
    /// The file's index in the patch.
    index: usize,
    #[serde(flatten)]
    result: FileApply,
}

/// Applies the selected hunks of the selected files to the real files under
/// `base`, in place. Old versions of deleted or renamed files go to the
/// Trash. One result per selected file, with every hunk's outcome; a file
/// that fails doesn't stop the others.
#[tauri::command]
pub async fn patch_write_selected(
    app: AppHandle,
    path: String,
    base: String,
    strip: usize,
    files: Vec<FileSelection>,
) -> CmdResult<Vec<WrittenFile>> {
    crate::blocking(move || {
        let dir = local_base(&base)?;
        let (p, _) = read_patch(&app, &path)?;
        let trash = TrashDeleter::new();
        let name = Path::new(&path).file_name().map_or_else(|| path.clone(), |n| n.to_string_lossy().into_owned());
        let batch = crate::backups::begin(
            &app,
            BackupOrigin {
                tool: "Patch".into(),
                action: format!("Apply {name}"),
                left: base.clone(),
                right: path.clone(),
            },
        );
        // With backups on, removed and rewritten files are kept there.
        let remove = |path: &Path| match &batch {
            Some(b) => b.keep_move(path, path, BackupReason::Removed).map(|_| ()),
            None => trash.delete(path),
        };
        let keep = |path: &Path| match &batch {
            Some(b) => b.keep_copy(path).map(|_| ()),
            None => Ok(()),
        };
        let written = files
            .into_iter()
            .map(|sel| WrittenFile {
                index: sel.index,
                result: patch::write_to_folder(&p, dir, strip, sel.index, sel.hunks.as_deref(), &remove, &keep)
                    .unwrap_or_else(failed_file),
            })
            .collect();
        if let Some(b) = &batch {
            crate::backups::finish(&app, b);
        }
        Ok(written)
    })
    .await
}
