// Hide the console window on Windows release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use pcompare_core::folder::{self, FolderCompareOptions, FolderCompareResult};
use pcompare_core::hex::{self, HexChunk, HexSummary};
use pcompare_core::text::{self, TextDiffOptions, TextDiffResult};
use pcompare_core::textfile::{self, TextFileInfo};
use pcompare_core::PathKind;
use serde::Serialize;
use std::path::PathBuf;

type CmdResult<T> = Result<T, String>;

/// Runs blocking engine work off the IPC thread so the UI stays responsive.
async fn blocking<T, F>(f: F) -> CmdResult<T>
where
    T: Send + 'static,
    F: FnOnce() -> pcompare_core::Result<T> + Send + 'static,
{
    tauri::async_runtime::spawn_blocking(f)
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TextFileCompare {
    left: TextFileInfo,
    right: TextFileInfo,
    /// `None` when either side looks binary; the UI offers the hex view instead.
    diff: Option<TextDiffResult>,
}

#[tauri::command]
fn path_kind(path: String) -> PathKind {
    pcompare_core::path_kind(path)
}

/// Paths passed on the command line, e.g. `pcompare.exe left right`.
#[tauri::command]
fn launch_args() -> Vec<String> {
    std::env::args()
        .skip(1)
        .filter(|a| !a.starts_with('-'))
        .collect()
}

#[tauri::command]
async fn compare_text_files(
    left: String,
    right: String,
    options: TextDiffOptions,
) -> CmdResult<TextFileCompare> {
    blocking(move || {
        let binary = textfile::file_is_binary(&left)? || textfile::file_is_binary(&right)?;
        let l = textfile::load_text(&left)?;
        let r = textfile::load_text(&right)?;
        let diff = (!binary).then(|| text::diff_text(&l.text, &r.text, &options));
        Ok(TextFileCompare {
            left: l.info,
            right: r.info,
            diff,
        })
    })
    .await
}

#[tauri::command]
async fn compare_folders(
    left: String,
    right: String,
    options: FolderCompareOptions,
) -> CmdResult<FolderCompareResult> {
    blocking(move || folder::compare_folders(&left, &right, &options)).await
}

#[tauri::command]
async fn hex_summary(left: String, right: String) -> CmdResult<HexSummary> {
    blocking(move || hex::summarize(&PathBuf::from(left), &PathBuf::from(right))).await
}

#[tauri::command]
async fn hex_chunk(left: String, right: String, offset: u64, len: usize) -> CmdResult<HexChunk> {
    let len = len.min(1 << 20); // never ship more than 1 MiB per IPC call
    blocking(move || hex::read_chunk(&PathBuf::from(left), &PathBuf::from(right), offset, len)).await
}

#[tauri::command]
async fn hex_next_difference(left: String, right: String, from: u64) -> CmdResult<Option<u64>> {
    blocking(move || hex::next_difference(&PathBuf::from(left), &PathBuf::from(right), from)).await
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![
            path_kind,
            launch_args,
            compare_text_files,
            compare_folders,
            hex_summary,
            hex_chunk,
            hex_next_difference,
        ])
        .run(tauri::generate_context!())
        .expect("error while running PCompare");
}
