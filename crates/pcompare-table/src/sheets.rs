//! Comparing every worksheet (workbooks) or `<table>` (HTML) of two files at
//! once: sheets pair up by name, leftovers by position, and each pair gets a
//! difference summary. The UI then opens one pair at a time in the grid.

use crate::load::TableBook;
use crate::{compare_tables, ColumnPair, SourceFormat, TableCompareOptions, TableStats};
use pcompare_core::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

/// Column-specific options for one sheet pair. Key, ignored, mapped and sort
/// columns are indices into that pair's columns, so they can't carry over to
/// other sheets; pairs without an entry compare with none of them.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SheetColumns {
    pub left_sheet: Option<usize>,
    pub right_sheet: Option<usize>,
    pub key_columns: Vec<usize>,
    pub ignored_columns: Vec<usize>,
    pub column_mapping: Option<Vec<ColumnPair>>,
    pub sort_columns: Vec<usize>,
}

/// One row of the sheet list.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SheetComparison {
    /// Sheet / table index on each side (`None` when only the other side has it).
    pub left: Option<usize>,
    pub right: Option<usize>,
    /// Display labels.
    pub left_label: Option<String>,
    pub right_label: Option<String>,
    /// Paired by name (rather than by position or not at all).
    pub by_name: bool,
    /// Row statistics for paired sheets that loaded.
    pub stats: Option<TableStats>,
    /// Rows that differ importantly, or every row of an unpaired sheet.
    pub differences: usize,
    pub identical: bool,
    pub truncated: bool,
    /// Why the sheet pair couldn't be compared.
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SheetsCompareResult {
    pub left_format: SourceFormat,
    pub right_format: SourceFormat,
    pub sheets: Vec<SheetComparison>,
    /// Every sheet paired and without important differences.
    pub identical: bool,
}

/// Pairs sheets by exact name, then case-insensitively, then whatever is left
/// by position. Unnamed sheets (`None`) only pair by position. Pairs come in
/// left order; right-only sheets follow.
pub fn pair_sheets(left: &[Option<String>], right: &[Option<String>]) -> Vec<(Option<usize>, Option<usize>, bool)> {
    let mut matched: Vec<Option<usize>> = vec![None; left.len()];
    let mut taken = vec![false; right.len()];
    for fold in [false, true] {
        let norm = |s: &str| if fold { s.to_lowercase() } else { s.to_owned() };
        let mut by_name: HashMap<String, Vec<usize>> = HashMap::new();
        for (r, name) in right.iter().enumerate().rev() {
            if let (Some(name), false) = (name, taken[r]) {
                by_name.entry(norm(name)).or_default().push(r); // reversed, so `pop` yields the first
            }
        }
        for (l, name) in left.iter().enumerate() {
            let Some(name) = name.as_deref().filter(|_| matched[l].is_none()) else {
                continue;
            };
            if let Some(r) = by_name.get_mut(&norm(name)).and_then(Vec::pop) {
                matched[l] = Some(r);
                taken[r] = true;
            }
        }
    }
    let by_name: Vec<bool> = matched.iter().map(Option::is_some).collect();
    let mut spare = (0..right.len()).filter(|&r| !taken[r]);
    for m in matched.iter_mut().filter(|m| m.is_none()) {
        *m = spare.next();
    }
    let mut out: Vec<_> = matched
        .iter()
        .enumerate()
        .map(|(l, &r)| (Some(l), r, by_name[l]))
        .collect();
    out.extend(spare.map(|r| (None, Some(r), false)));
    out
}

/// Loads every sheet of both files and summarizes each pair. `columns`
/// supplies per-pair column options (matched on both sheet indices).
pub fn compare_all_sheets(
    left: &Path,
    right: &Path,
    options: &TableCompareOptions,
    columns: &[SheetColumns],
) -> Result<SheetsCompareResult> {
    let mut lbook = TableBook::open(left, options.delimiter)?;
    let mut rbook = TableBook::open(right, options.delimiter)?;
    let label = |book: &TableBook, i: Option<usize>| i.and_then(|i| book.labels.get(i).cloned());

    let mut sheets = Vec::new();
    for (l, r, by_name) in pair_sheets(&lbook.names, &rbook.names) {
        let mut row = SheetComparison {
            left: l,
            right: r,
            left_label: label(&lbook, l),
            right_label: label(&rbook, r),
            by_name,
            stats: None,
            differences: 0,
            identical: false,
            truncated: false,
            error: None,
        };
        match (l, r) {
            (Some(l), Some(r)) => match (lbook.load(l), rbook.load(r)) {
                (Ok(lt), Ok(rt)) => {
                    let opts = sheet_options(options, columns, l, r);
                    let res = compare_tables(&lt, &rt, &opts);
                    row.differences = res.stats.different + res.stats.left_only + res.stats.right_only;
                    row.identical = res.identical;
                    row.truncated = res.truncated;
                    row.stats = Some(res.stats);
                }
                (Err(e), _) | (_, Err(e)) => row.error = Some(e.to_string()),
            },
            // An unpaired sheet: every data row is an orphan.
            (Some(i), None) | (None, Some(i)) => {
                let (book, header) = if l.is_some() {
                    (&mut lbook, options.headers().0)
                } else {
                    (&mut rbook, options.headers().1)
                };
                match book.load(i) {
                    Ok(t) => {
                        row.differences = t.rows.len().saturating_sub(usize::from(header));
                        row.truncated = t.source.truncated;
                    }
                    Err(e) => row.error = Some(e.to_string()),
                }
            }
            (None, None) => unreachable!("pair_sheets never yields an empty pair"),
        }
        sheets.push(row);
    }
    Ok(SheetsCompareResult {
        left_format: lbook.format,
        right_format: rbook.format,
        identical: sheets.iter().all(|s| s.identical),
        sheets,
    })
}

/// The options for one pair: the shared ones plus that pair's column choices.
fn sheet_options(options: &TableCompareOptions, columns: &[SheetColumns], l: usize, r: usize) -> TableCompareOptions {
    let own = columns
        .iter()
        .find(|c| c.left_sheet.unwrap_or(0) == l && c.right_sheet.unwrap_or(0) == r)
        .cloned()
        .unwrap_or_default();
    TableCompareOptions {
        key_columns: own.key_columns,
        ignored_columns: own.ignored_columns,
        column_mapping: own.column_mapping,
        sort_columns: own.sort_columns,
        left_sheet: Some(l),
        right_sheet: Some(r),
        ..options.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(list: &[&str]) -> Vec<Option<String>> {
        list.iter().map(|s| (!s.is_empty()).then(|| s.to_string())).collect()
    }

    #[test]
    fn pairs_by_name_then_case_then_position() {
        let l = names(&["Summary", "Data", "notes", "Old"]);
        let r = names(&["data", "Data", "NOTES", "Extra", "More"]);
        let pairs = pair_sheets(&l, &r);
        assert_eq!(
            pairs,
            vec![
                (Some(0), Some(0), false), // Summary ↔ data: leftovers pair by position
                (Some(1), Some(1), true),  // exact beats case-insensitive
                (Some(2), Some(2), true),  // case-insensitive
                (Some(3), Some(3), false), // Old ↔ Extra
                (None, Some(4), false),
            ]
        );
    }

    #[test]
    fn unnamed_tables_pair_by_position() {
        let l = names(&["", "People", ""]);
        let r = names(&["People", "", "", ""]);
        assert_eq!(
            pair_sheets(&l, &r),
            vec![
                (Some(0), Some(1), false),
                (Some(1), Some(0), true),
                (Some(2), Some(2), false),
                (None, Some(3), false),
            ]
        );
        assert_eq!(
            pair_sheets(&names(&["a", "b"]), &[]),
            vec![(Some(0), None, false), (Some(1), None, false)]
        );
    }

    #[test]
    fn compares_every_workbook_sheet() {
        use rust_xlsxwriter::Workbook;
        let dir = tempfile::tempdir().unwrap();
        let write = |name: &str, sheets: &[(&str, &[(&str, &str)])]| {
            let path = dir.path().join(name);
            let mut wb = Workbook::new();
            for (sheet, rows) in sheets {
                let ws = wb.add_worksheet().set_name(*sheet).unwrap();
                for (i, (a, b)) in rows.iter().enumerate() {
                    ws.write_string(i as u32, 0, *a).unwrap();
                    ws.write_string(i as u32, 1, *b).unwrap();
                }
            }
            wb.save(&path).unwrap();
            path
        };
        let a = write(
            "a.xlsx",
            &[
                ("People", &[("id", "name"), ("1", "Ann"), ("2", "Bob")]),
                ("Prices", &[("sku", "price"), ("x", "1")]),
                ("Gone", &[("h", "v"), ("1", "2"), ("3", "4")]),
            ],
        );
        let b = write(
            "b.xlsx",
            &[
                ("prices", &[("sku", "price"), ("x", "1")]),
                ("People", &[("id", "name"), ("2", "Rob"), ("1", "Ann")]),
            ],
        );
        let res = compare_all_sheets(&a, &b, &TableCompareOptions::default(), &[]).unwrap();
        assert_eq!(res.left_format, SourceFormat::Workbook);
        assert_eq!(res.sheets.len(), 3);
        let people = &res.sheets[0];
        assert_eq!((people.left, people.right, people.by_name), (Some(0), Some(1), true));
        // Without keys the reordered rows don't line up.
        assert!(people.differences >= 2);
        let prices = &res.sheets[1];
        assert_eq!((prices.right, prices.identical, prices.differences), (Some(0), true, 0));
        assert_eq!(prices.right_label.as_deref(), Some("prices"));
        let gone = &res.sheets[2];
        assert_eq!((gone.right, gone.differences, gone.stats.is_none()), (None, 2, true));
        assert!(!res.identical);

        // Keys for the People pair only: Bob/Rob is then the one difference.
        let cols = [SheetColumns {
            left_sheet: Some(0),
            right_sheet: Some(1),
            key_columns: vec![0],
            ..Default::default()
        }];
        let res = compare_all_sheets(&a, &b, &TableCompareOptions::default(), &cols).unwrap();
        assert_eq!(res.sheets[0].differences, 1);
        assert_eq!(res.sheets[0].stats.as_ref().unwrap().same, 1);
    }

    #[test]
    fn compares_html_tables_by_caption() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.html");
        let b = dir.path().join("b.htm");
        std::fs::write(
            &a,
            "<table><caption>Totals</caption><tr><td>t</td></tr><tr><td>1</td></tr></table>\
             <table><caption>Items</caption><tr><td>i</td></tr><tr><td>a</td></tr></table>",
        )
        .unwrap();
        std::fs::write(
            &b,
            "<table><caption>Items</caption><tr><td>i</td></tr><tr><td>b</td></tr></table>\
             <table><caption>Totals</caption><tr><td>t</td></tr><tr><td>1</td></tr></table>",
        )
        .unwrap();
        let res = compare_all_sheets(&a, &b, &TableCompareOptions::default(), &[]).unwrap();
        let summary: Vec<_> = res.sheets.iter().map(|s| (s.left, s.right, s.differences)).collect();
        assert_eq!(summary, vec![(Some(0), Some(1), 0), (Some(1), Some(0), 1)]);
        assert_eq!(res.sheets[0].left_label.as_deref(), Some("Table 1: Totals (2 rows)"));
    }

    #[test]
    fn delimited_files_are_one_sheet() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.csv");
        let b = dir.path().join("b.csv");
        std::fs::write(&a, "x,y\n1,2\n").unwrap();
        std::fs::write(&b, "x,y\n1,3\n").unwrap();
        let res = compare_all_sheets(&a, &b, &TableCompareOptions::default(), &[]).unwrap();
        assert_eq!(res.sheets.len(), 1);
        assert_eq!(res.sheets[0].left_label.as_deref(), Some("a.csv"));
        assert_eq!(res.sheets[0].differences, 1);
    }
}
