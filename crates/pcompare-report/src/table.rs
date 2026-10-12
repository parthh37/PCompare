//! Table comparison reports: an HTML grid with differing cells highlighted
//! (left value over right value), or CSV with both values of each column.

use crate::csv_row;
use crate::html::{self, escape, Style};
use pcompare_table::{ColumnInfo, RowStatus, TableCompareResult, TableRow};
use serde::{Deserialize, Serialize};
use std::fmt::Write;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TableReportFormat {
    #[default]
    Html,
    Csv,
}

/// The Table Compare view filters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TableFilter {
    #[default]
    All,
    /// Different rows and orphans (not rows where only ignored columns differ).
    Diffs,
    Same,
    Orphans,
}

impl TableFilter {
    fn matches(self, r: &TableRow) -> bool {
        match self {
            TableFilter::All => true,
            TableFilter::Diffs => matches!(
                r.status,
                RowStatus::Different | RowStatus::LeftOnly | RowStatus::RightOnly
            ),
            TableFilter::Same => matches!(r.status, RowStatus::Same | RowStatus::Unimportant),
            TableFilter::Orphans => matches!(r.status, RowStatus::LeftOnly | RowStatus::RightOnly),
        }
    }

    fn label(self) -> &'static str {
        match self {
            TableFilter::All => "All rows",
            TableFilter::Diffs => "Differences",
            TableFilter::Same => "Same rows",
            TableFilter::Orphans => "Orphan rows",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TableReportOptions {
    pub format: TableReportFormat,
    pub filter: TableFilter,
    /// Indices into the result's columns left out of the report (the view's
    /// hidden columns).
    pub hidden_columns: Vec<usize>,
    pub mono: bool,
    /// Report heading; empty for "Table Compare".
    pub title: String,
    /// The sides as the reader should see them (default: the file paths).
    pub left: String,
    pub right: String,
}

impl Default for TableReportOptions {
    fn default() -> Self {
        Self {
            format: TableReportFormat::Html,
            filter: TableFilter::All,
            hidden_columns: Vec::new(),
            mono: false,
            title: String::new(),
            left: String::new(),
            right: String::new(),
        }
    }
}

/// Spreadsheet-style column letter (0 → A, 26 → AA).
fn column_letter(i: usize) -> String {
    let mut s = Vec::new();
    let mut n = i + 1;
    while n > 0 {
        s.push(b'A' + ((n - 1) % 26) as u8);
        n = (n - 1) / 26;
    }
    s.reverse();
    String::from_utf8(s).unwrap_or_default()
}

/// The header text the view shows for a column (mirrors `columnName`).
fn column_name(c: &ColumnInfo) -> String {
    let side = |name: &Option<String>, index: Option<usize>| -> Option<String> {
        let index = index?;
        Some(
            name.clone()
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| column_letter(index)),
        )
    };
    side(&c.left_name, c.left)
        .or_else(|| side(&c.right_name, c.right))
        .unwrap_or_default()
}

fn status_label(s: RowStatus) -> &'static str {
    match s {
        RowStatus::Same => "Same",
        RowStatus::Different => "Different",
        RowStatus::Unimportant => "Unimportant",
        RowStatus::LeftOnly => "Left orphan",
        RowStatus::RightOnly => "Right orphan",
    }
}

fn row_class(s: RowStatus) -> &'static str {
    match s {
        RowStatus::Same => "eq",
        RowStatus::Different => "chg",
        RowStatus::Unimportant => "uni",
        RowStatus::LeftOnly => "lo",
        RowStatus::RightOnly => "ro",
    }
}

fn cell(side: &Option<Vec<String>>, col: usize) -> &str {
    side.as_ref()
        .and_then(|v| v.get(col))
        .map(String::as_str)
        .unwrap_or("")
}

fn row_number(r: &TableRow) -> String {
    match (r.left_row, r.right_row) {
        (Some(l), Some(r)) if l == r => l.to_string(),
        (Some(l), Some(r)) => format!("{l} / {r}"),
        (Some(l), None) => l.to_string(),
        (None, Some(r)) => r.to_string(),
        (None, None) => String::new(),
    }
}

/// Renders `result` in the format `opts` asks for. `generated` is the
/// "produced" stamp (empty to leave it out).
pub fn render(result: &TableCompareResult, opts: &TableReportOptions, generated: &str) -> String {
    let columns: Vec<usize> = (0..result.columns.len())
        .filter(|i| !opts.hidden_columns.contains(i))
        .collect();
    let rows: Vec<&TableRow> = result
        .rows
        .iter()
        .filter(|r| opts.filter.matches(r))
        .collect();
    match opts.format {
        TableReportFormat::Html => render_html(result, opts, generated, &columns, &rows),
        TableReportFormat::Csv => render_csv(result, &columns, &rows),
    }
}

fn render_html(
    result: &TableCompareResult,
    opts: &TableReportOptions,
    generated: &str,
    columns: &[usize],
    rows: &[&TableRow],
) -> String {
    let title = if opts.title.trim().is_empty() {
        "Table Compare"
    } else {
        opts.title.trim()
    };
    let mut body = html::header(title, generated, &opts.left, &opts.right);
    let s = &result.stats;
    let _ = writeln!(
        body,
        "<div class=\"stats\">Display: {} · Rows: {} same, {} different, {} unimportant, {} left orphans, {} right orphans · {} differing cells{}</div>",
        opts.filter.label(),
        s.same,
        s.different,
        s.unimportant,
        s.left_only,
        s.right_only,
        s.different_cells,
        if result.truncated { " · tables were truncated to the size limit" } else { "" }
    );
    if rows.is_empty() {
        body.push_str("<div class=\"note\">Nothing to show for this filter.</div>\n");
    } else {
        body.push_str("<table class=\"grid\">\n<tr><th>Row</th>");
        for &i in columns {
            let c = &result.columns[i];
            let class = if c.key {
                " class=\"key\""
            } else if c.ignored {
                " class=\"ign\""
            } else {
                ""
            };
            let _ = write!(body, "<th{class}>{}</th>", escape(&column_name(c)));
        }
        body.push_str("</tr>\n");
        for r in rows {
            let _ = write!(
                body,
                "<tr class=\"{}\" title=\"{}\"><td class=\"num\">{}</td>",
                row_class(r.status),
                status_label(r.status),
                row_number(r)
            );
            for &i in columns {
                let ign = if result.columns[i].ignored {
                    " ign"
                } else {
                    ""
                };
                match r.status {
                    RowStatus::LeftOnly => {
                        let _ = write!(
                            body,
                            "<td class=\"cell{ign}\">{}</td>",
                            escape(cell(&r.left, i))
                        );
                    }
                    RowStatus::RightOnly => {
                        let _ = write!(
                            body,
                            "<td class=\"cell{ign}\">{}</td>",
                            escape(cell(&r.right, i))
                        );
                    }
                    _ if r.diffs.contains(&i) => {
                        let _ = write!(
                            body,
                            "<td class=\"cd{ign}\"><div class=\"l\">{}</div><div class=\"r\">{}</div></td>",
                            escape(cell(&r.left, i)),
                            escape(cell(&r.right, i))
                        );
                    }
                    _ if ign.is_empty() => {
                        let _ = write!(body, "<td>{}</td>", escape(cell(&r.left, i)));
                    }
                    _ => {
                        let _ = write!(body, "<td class=\"ign\">{}</td>", escape(cell(&r.left, i)));
                    }
                }
            }
            body.push_str("</tr>\n");
        }
        body.push_str("</table>\n");
    }
    html::page(
        title,
        Style {
            mono: opts.mono,
            wrap: true,
        },
        &body,
    )
}

fn render_csv(result: &TableCompareResult, columns: &[usize], rows: &[&TableRow]) -> String {
    let mut head = vec![
        "Status".to_owned(),
        "Left Row".to_owned(),
        "Right Row".to_owned(),
    ];
    for &i in columns {
        let name = column_name(&result.columns[i]);
        head.push(format!("{name} (left)"));
        head.push(format!("{name} (right)"));
    }
    let mut out = csv_row(&head);
    for r in rows {
        let mut fields = vec![
            status_label(r.status).to_owned(),
            r.left_row.map(|n| n.to_string()).unwrap_or_default(),
            r.right_row.map(|n| n.to_string()).unwrap_or_default(),
        ];
        for &i in columns {
            fields.push(cell(&r.left, i).to_owned());
            fields.push(cell(&r.right, i).to_owned());
        }
        out.push_str(&csv_row(&fields));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use pcompare_table::{compare_tables, Table, TableCompareOptions};

    fn table(rows: &[&[&str]]) -> Table {
        Table::from_rows(
            rows.iter()
                .map(|r| r.iter().map(|c| c.to_string()).collect())
                .collect(),
        )
    }

    fn result() -> TableCompareResult {
        let left = table(&[
            &["id", "name", "qty"],
            &["1", "apple", "3"],
            &["2", "pear", "5"],
            &["3", "fig", "1"],
        ]);
        let right = table(&[
            &["id", "name", "qty"],
            &["1", "apple", "4"],
            &["2", "pear", "5"],
            &["4", "kiwi", "9"],
        ]);
        let opts = TableCompareOptions {
            key_columns: vec![0],
            ..Default::default()
        };
        compare_tables(&left, &right, &opts)
    }

    #[test]
    fn column_letters_roll_over() {
        assert_eq!(column_letter(0), "A");
        assert_eq!(column_letter(25), "Z");
        assert_eq!(column_letter(26), "AA");
        assert_eq!(column_letter(701), "ZZ");
    }

    #[test]
    fn csv_has_both_values_per_column_and_honours_the_filter() {
        let csv = render(
            &result(),
            &TableReportOptions {
                format: TableReportFormat::Csv,
                filter: TableFilter::Diffs,
                hidden_columns: vec![1],
                ..Default::default()
            },
            "",
        );
        let lines: Vec<&str> = csv.lines().collect();
        assert_eq!(
            lines[0],
            "Status,Left Row,Right Row,id (left),id (right),qty (left),qty (right)"
        );
        assert!(lines.contains(&"Different,2,2,1,1,3,4"), "{csv}");
        assert!(lines.contains(&"Left orphan,4,,3,,1,"), "{csv}");
        assert!(lines.contains(&"Right orphan,,4,,4,,9"), "{csv}");
        assert_eq!(lines.len(), 4);
    }

    #[test]
    fn html_grid_highlights_differing_cells() {
        let h = render(
            &result(),
            &TableReportOptions {
                left: "a.csv".into(),
                right: "b.csv".into(),
                ..Default::default()
            },
            "",
        );
        assert!(
            h.contains("<th class=\"key\">id</th><th>name</th><th>qty</th>"),
            "{h}"
        );
        assert!(
            h.contains("<td class=\"cd\"><div class=\"l\">3</div><div class=\"r\">4</div></td>"),
            "{h}"
        );
        assert!(h.contains("<tr class=\"lo\" title=\"Left orphan\">"));
        assert!(h.contains("<td>a.csv</td>"));
    }
}
