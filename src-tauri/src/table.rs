//! Tauri commands for Table Compare sessions.

use crate::CmdResult;
use pcompare_table::{TableCompareOptions, TableCompareResult};
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
