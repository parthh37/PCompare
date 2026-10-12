//! Tauri commands for comparison reports (Text, Folder and Table Compare
//! "Report…"): rendering to a string for the clipboard or printing, saving
//! to a file (folder reports with linked per-file reports), and opening a
//! report in the system browser.

use crate::CmdResult;
use pcompare_core::text::{self, SideInput, TextDiffOptions};
use pcompare_core::textfile::LineEnding;
use pcompare_core::CoreError;
use pcompare_report::folder::{
    FolderReportInput, FolderReportOptions, LinkedReports, WrittenReport,
};
use pcompare_report::table::TableReportOptions;
use pcompare_report::text::{TextReportOptions, TextSection};
use pcompare_table::TableCompareOptions;
use serde::Deserialize;
use std::path::PathBuf;
use tauri::AppHandle;
use tauri_plugin_opener::OpenerExt;

/// One side of a text report: the text as the view holds it (edits
/// included), its line terminators when known, and the label to print.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReportSide {
    lines: Vec<String>,
    #[serde(default)]
    eols: Option<Vec<LineEnding>>,
    /// Picks the grammar (comments/strings rules); not shown.
    #[serde(default)]
    path: Option<String>,
    /// Shown in the report header (path or tab title).
    title: String,
}

/// Diffs both sides with the view's rules and renders the report.
#[tauri::command]
pub async fn report_text(
    left: ReportSide,
    right: ReportSide,
    rules: TextDiffOptions,
    options: TextReportOptions,
) -> CmdResult<String> {
    crate::blocking(move || {
        let l: Vec<&str> = left.lines.iter().map(String::as_str).collect();
        let r: Vec<&str> = right.lines.iter().map(String::as_str).collect();
        let diff = text::diff_sides(
            SideInput {
                lines: &l,
                eols: left.eols.as_deref(),
                path: left.path.as_deref(),
            },
            SideInput {
                lines: &r,
                eols: right.eols.as_deref(),
                path: right.path.as_deref(),
            },
            &rules,
        )?;
        let section = TextSection {
            name: "",
            left: &left.title,
            right: &right.title,
            diff: &diff,
        };
        Ok(pcompare_report::text::render(
            &[section],
            &options,
            &pcompare_report::now_stamp(),
        ))
    })
    .await
}

/// Renders a folder report of what the view shows (for copy and print).
#[tauri::command]
pub async fn report_folder(
    input: FolderReportInput,
    options: FolderReportOptions,
) -> CmdResult<String> {
    crate::blocking(move || {
        Ok(pcompare_report::folder::render(
            &input,
            &options,
            &pcompare_report::now_stamp(),
            &Default::default(),
        ))
    })
    .await
}

/// Per-file reports to link from a saved folder report.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LinkOptions {
    text: TextReportOptions,
    rules: TextDiffOptions,
}

/// Writes a folder report to `path`; with `links`, first writes a text
/// report for each differing text file into `<name>_files/` and links them.
/// Remote sides are downloaded to the cache for those reports.
#[tauri::command]
pub async fn report_folder_save(
    app: AppHandle,
    input: FolderReportInput,
    options: FolderReportOptions,
    path: String,
    links: Option<LinkOptions>,
) -> CmdResult<WrittenReport> {
    crate::blocking(move || {
        let roots = [input.left_root.clone(), input.right_root.clone()];
        let local = |p: &str| -> pcompare_core::Result<PathBuf> {
            crate::with_remote(&app, [&roots[0], &roots[1]], |remote| {
                remote.local_copy(p, true)
            })
        };
        let linked = links.as_ref().map(|l| LinkedReports {
            text: &l.text,
            rules: &l.rules,
            local: &local,
        });
        pcompare_report::folder::write_folder_report(
            &input,
            &options,
            &pcompare_report::now_stamp(),
            &PathBuf::from(&path),
            linked.as_ref(),
        )
    })
    .await
}

/// Compares the two tables again with the view's options and renders the
/// report (the view doesn't keep anything the engine can't recompute).
#[tauri::command]
pub async fn report_table(
    app: AppHandle,
    left: String,
    right: String,
    compare: TableCompareOptions,
    mut options: TableReportOptions,
) -> CmdResult<String> {
    crate::blocking(move || {
        let (lp, rp) = crate::with_remote(&app, [&left, &right], |remote| {
            Ok((
                remote.local_copy(&left, true)?,
                remote.local_copy(&right, true)?,
            ))
        })?;
        let result = pcompare_table::compare_files(&lp, &rp, &compare)?;
        if options.left.is_empty() {
            options.left = left;
        }
        if options.right.is_empty() {
            options.right = right;
        }
        Ok(pcompare_report::table::render(
            &result,
            &options,
            &pcompare_report::now_stamp(),
        ))
    })
    .await
}

/// Saves a rendered report (UTF-8) where the user chose.
#[tauri::command]
pub async fn report_write(path: String, content: String) -> CmdResult<()> {
    crate::blocking(move || std::fs::write(&path, content).map_err(|e| CoreError::io(&path, e)))
        .await
}

/// Writes an HTML report to a temporary file and opens it in the default
/// browser; with `print`, the page opens the print dialog once loaded.
/// Used for printing where the webview can't print (macOS WKWebView ignores
/// `window.print()`). Returns the file's path.
#[tauri::command]
pub async fn report_open_in_browser(
    app: AppHandle,
    html: String,
    print: bool,
) -> CmdResult<String> {
    let path = crate::blocking(move || {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or_default();
        let path = std::env::temp_dir().join(format!("pcompare-report-{stamp}.html"));
        let html = if print {
            let script = "<script>window.addEventListener(\"load\", () => setTimeout(() => window.print(), 200));</script>\n";
            match html.rfind("</body>") {
                Some(i) => format!("{}{script}{}", &html[..i], &html[i..]),
                None => format!("{html}{script}"),
            }
        } else {
            html
        };
        std::fs::write(&path, html).map_err(|e| CoreError::io(&path, e))?;
        Ok(path)
    })
    .await?;
    let shown = path.to_string_lossy().into_owned();
    app.opener()
        .open_path(&shown, None::<&str>)
        .map_err(|e| format!("cannot open the browser: {e}"))?;
    Ok(shown)
}
