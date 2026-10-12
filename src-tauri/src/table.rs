//! Tauri commands for Table Compare sessions.

use crate::CmdResult;
use pcompare_table::{SheetColumns, SheetsCompareResult, TableCompareOptions, TableCompareResult};
use tauri::AppHandle;

/// Loads both tables (CSV/TSV, Excel/ODS or HTML; remote paths are
/// downloaded first) and compares them cell by cell.
#[tauri::command]
pub async fn compare_tables(
    app: AppHandle,
    left: String,
    right: String,
    options: TableCompareOptions,
) -> CmdResult<TableCompareResult> {
    crate::blocking(move || {
        let (lp, rp) = crate::with_remote(&app, [&left, &right], |remote| {
            Ok((remote.local_copy(&left, true)?, remote.local_copy(&right, true)?))
        })?;
        pcompare_table::compare_files(&lp, &rp, &options)
    })
    .await
}

/// Compares every worksheet / HTML table of the two files, pairing them by
/// name then position, and returns a per-pair difference summary.
/// `columns` holds each pair's own key / ignored / mapped / sort columns.
#[tauri::command]
pub async fn compare_table_sheets(
    app: AppHandle,
    left: String,
    right: String,
    options: TableCompareOptions,
    columns: Vec<SheetColumns>,
) -> CmdResult<SheetsCompareResult> {
    crate::blocking(move || {
        let (lp, rp) = crate::with_remote(&app, [&left, &right], |remote| {
            Ok((remote.local_copy(&left, true)?, remote.local_copy(&right, true)?))
        })?;
        pcompare_table::compare_all_sheets(&lp, &rp, &options, &columns)
    })
    .await
}
