//! Reading tables from delimited text, spreadsheet workbooks and HTML.

use crate::{SourceFormat, Table, TableSource};
use calamine::{Data, Reader};
use dom_query::{Document, NodeRef};
use pcompare_core::textfile;
use pcompare_core::{CoreError, Result};
use std::path::Path;

/// Rows kept per side; anything beyond is dropped and flagged as truncated.
pub const MAX_ROWS: usize = 200_000;
/// Cells kept per side, so very wide sheets are capped as well as long ones.
pub const MAX_CELLS: usize = 4_000_000;
/// Files larger than this are refused outright rather than read into memory.
pub const MAX_FILE_BYTES: u64 = 256 << 20;

/// Delimiters considered by [`sniff_delimiter`], in tie-break order.
const CANDIDATES: [u8; 4] = *b",;\t|";
const SNIFF_BYTES: usize = 64 << 10;
const SNIFF_RECORDS: usize = 100;

const WORKBOOK_EXTS: &[&str] = &["xlsx", "xlsm", "xlsb", "xls", "xla", "xlam", "ods"];
const HTML_EXTS: &[&str] = &["html", "htm", "xhtml"];

/// Per-side loading choices.
#[derive(Debug, Clone, Copy, Default)]
pub struct LoadOptions {
    /// Field delimiter for delimited text; sniffed when `None`.
    pub delimiter: Option<char>,
    /// Worksheet (workbooks) or `<table>` (HTML) index; out-of-range picks the first.
    pub sheet: Option<usize>,
}

/// Picks the reader from the file extension.
pub fn detect_format(path: &Path) -> SourceFormat {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    if WORKBOOK_EXTS.contains(&ext.as_str()) {
        SourceFormat::Workbook
    } else if HTML_EXTS.contains(&ext.as_str()) {
        SourceFormat::Html
    } else {
        SourceFormat::Delimited
    }
}

/// Loads one side of a table comparison.
pub fn load_table(path: &Path, opts: &LoadOptions) -> Result<Table> {
    let size = std::fs::metadata(path).map_err(|e| CoreError::io(path, e))?.len();
    if size > MAX_FILE_BYTES {
        return Err(CoreError::Other(format!(
            "{}: file is too large for Table Compare ({} MiB limit)",
            path.display(),
            MAX_FILE_BYTES >> 20
        )));
    }
    let sheet = opts.sheet.unwrap_or(0);
    let format = detect_format(path);
    if format == SourceFormat::Workbook {
        return load_workbook(path, sheet);
    }

    let bytes = std::fs::read(path).map_err(|e| CoreError::io(path, e))?;
    if textfile::is_probably_binary(&bytes) {
        return Err(in_file(path, "file looks binary, not delimited text or HTML"));
    }
    let (text, encoding, _) = textfile::decode(&bytes);
    let mut table = if format == SourceFormat::Html || looks_like_html(&text) {
        parse_html(&text, sheet)
    } else {
        let delimiter = match opts.delimiter {
            Some(c) if c.is_ascii() => Some(c as u8),
            Some(c) => return Err(CoreError::Other(format!("delimiter {c:?} must be an ASCII character"))),
            None => None,
        };
        parse_delimited(&text, delimiter, default_delimiter(path))
    }
    .map_err(|e| in_file(path, e))?;
    table.source.encoding = Some(encoding.name().to_string());
    Ok(table)
}

fn in_file(path: &Path, message: impl std::fmt::Display) -> CoreError {
    CoreError::Other(format!("{}: {message}", path.display()))
}

/// The delimiter assumed when sniffing finds no evidence of any candidate.
fn default_delimiter(path: &Path) -> u8 {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("tsv" | "tab") => b'\t',
        _ => b',',
    }
}

/// HTML saved with a generic extension (`.txt`, `.xls` exports from web apps).
fn looks_like_html(text: &str) -> bool {
    let head: String = text
        .trim_start()
        .chars()
        .take(1024)
        .collect::<String>()
        .to_ascii_lowercase();
    (head.starts_with("<!doctype html") || head.starts_with("<html") || head.starts_with("<table"))
        && text.to_ascii_lowercase().contains("<table")
}

/// Guesses the field delimiter among `, ; \t |` by finding the candidate that
/// occurs the same non-zero number of times (outside quotes) on most records.
pub fn sniff_delimiter(text: &str) -> Option<u8> {
    let mut records: Vec<[usize; CANDIDATES.len()]> = Vec::new();
    let mut current = [0usize; CANDIDATES.len()];
    let mut in_quotes = false;
    let mut has_content = false;
    let bytes = text.as_bytes();
    let sample = &bytes[..bytes.len().min(SNIFF_BYTES)];
    for &b in sample {
        match b {
            b'"' => in_quotes = !in_quotes,
            b'\n' if !in_quotes => {
                if has_content {
                    records.push(current);
                    if records.len() == SNIFF_RECORDS {
                        break;
                    }
                }
                current = [0; CANDIDATES.len()];
                has_content = false;
                continue;
            }
            b'\r' if !in_quotes => continue,
            _ if !in_quotes => {
                if let Some(i) = CANDIDATES.iter().position(|&c| c == b) {
                    current[i] += 1;
                }
            }
            _ => {}
        }
        has_content = true;
    }
    // A final record cut off by the sample limit would skew the counts.
    if has_content && (sample.len() == bytes.len() || records.is_empty()) {
        records.push(current);
    }

    let mut best: Option<(usize, usize, u8)> = None; // (consistent records, per-record count, delimiter)
    for (i, &delim) in CANDIDATES.iter().enumerate() {
        let mut freq = std::collections::HashMap::<usize, usize>::new();
        for r in records.iter().filter(|r| r[i] > 0) {
            *freq.entry(r[i]).or_default() += 1;
        }
        // Most common count; ties favour more fields per record.
        let Some((&count, &hits)) = freq.iter().max_by_key(|&(&count, &hits)| (hits, count)) else {
            continue;
        };
        if best.is_none_or(|(h, c, _)| (hits, count) > (h, c)) {
            best = Some((hits, count, delim));
        }
    }
    best.map(|(_, _, d)| d)
}

/// Collects rows while enforcing [`MAX_ROWS`] / [`MAX_CELLS`].
struct Builder {
    rows: Vec<Vec<String>>,
    row_numbers: Vec<usize>,
    cells: usize,
    truncated: bool,
}

impl Builder {
    fn new() -> Self {
        Builder {
            rows: Vec::new(),
            row_numbers: Vec::new(),
            cells: 0,
            truncated: false,
        }
    }

    /// Adds a row; returns `false` once the caps are hit and loading should stop.
    fn push(&mut self, row_number: usize, cells: Vec<String>) -> bool {
        if self.rows.len() >= MAX_ROWS || self.cells + cells.len() > MAX_CELLS {
            self.truncated = true;
            return false;
        }
        self.cells += cells.len();
        self.rows.push(cells);
        self.row_numbers.push(row_number);
        true
    }

    fn finish(self, format: SourceFormat, sheets: Vec<String>, sheet: usize, delimiter: Option<char>) -> Table {
        let column_count = self.rows.iter().map(Vec::len).max().unwrap_or(0);
        Table {
            source: TableSource {
                format,
                sheets,
                sheet,
                delimiter,
                encoding: None,
                row_count: self.rows.len(),
                column_count,
                truncated: self.truncated,
            },
            rows: self.rows,
            row_numbers: self.row_numbers,
        }
    }
}

/// Parses delimited text. Quoted fields may contain delimiters, doubled quotes
/// and newlines; rows may have differing field counts. Blank lines are skipped.
pub fn parse_delimited(text: &str, delimiter: Option<u8>, fallback: u8) -> Result<Table> {
    let delimiter = delimiter.or_else(|| sniff_delimiter(text)).unwrap_or(fallback);
    let mut reader = csv::ReaderBuilder::new()
        .has_headers(false)
        .flexible(true)
        .delimiter(delimiter)
        .from_reader(text.as_bytes());
    let mut builder = Builder::new();
    for (i, record) in reader.records().enumerate() {
        let record = record.map_err(|e| CoreError::Other(format!("cannot parse delimited text: {e}")))?;
        if !builder.push(i + 1, record.iter().map(str::to_owned).collect()) {
            break;
        }
    }
    Ok(builder.finish(SourceFormat::Delimited, Vec::new(), 0, Some(delimiter as char)))
}

/// Reads one worksheet. Cells left of / above the used range are padded so
/// columns and row numbers line up with what the spreadsheet app shows.
pub fn load_workbook(path: &Path, sheet: usize) -> Result<Table> {
    let mut workbook = calamine::open_workbook_auto(path).map_err(|e| in_file(path, e))?;
    let names = workbook.sheet_names();
    if names.is_empty() {
        return Err(in_file(path, "workbook has no worksheets"));
    }
    let sheet = if sheet < names.len() { sheet } else { 0 };
    let range = workbook
        .worksheet_range(&names[sheet])
        .map_err(|e| in_file(path, format!("sheet \"{}\": {e}", names[sheet])))?;
    let (row0, col0) = range.start().unwrap_or((0, 0));
    let mut builder = Builder::new();
    for (i, row) in range.rows().enumerate() {
        let mut cells = vec![String::new(); col0 as usize];
        cells.extend(row.iter().map(cell_text));
        if !builder.push(row0 as usize + i + 1, cells) {
            break;
        }
    }
    Ok(builder.finish(SourceFormat::Workbook, names, sheet, None))
}

/// Renders a spreadsheet cell the way a user would read it.
fn cell_text(cell: &Data) -> String {
    match cell {
        Data::Float(f) if f.fract() == 0.0 && f.abs() < 1e15 => format!("{}", *f as i64),
        Data::DateTime(dt) if dt.is_duration() => {
            let total = (dt.as_f64() * 86_400.0).round() as i64;
            let sign = if total < 0 { "-" } else { "" };
            let t = total.abs();
            format!("{sign}{}:{:02}:{:02}", t / 3600, t / 60 % 60, t % 60)
        }
        Data::DateTime(dt) => {
            let (y, mo, d, h, mi, s, ms) = dt.to_ymd_hms_milli();
            let has_time = dt.as_f64().fract() != 0.0;
            let time = if ms > 0 {
                format!("{h:02}:{mi:02}:{s:02}.{ms:03}")
            } else {
                format!("{h:02}:{mi:02}:{s:02}")
            };
            if dt.as_f64().trunc() == 0.0 && has_time {
                time
            } else if has_time {
                format!("{y:04}-{mo:02}-{d:02} {time}")
            } else {
                format!("{y:04}-{mo:02}-{d:02}")
            }
        }
        other => other.to_string(),
    }
}

/// Extracts the `index`-th `<table>` in document order. Each table's own rows
/// are read (directly or via `thead`/`tbody`/`tfoot`), so nested tables do not
/// leak rows into their parent; `colspan` cells are followed by empty cells.
pub fn parse_html(html: &str, index: usize) -> Result<Table> {
    let doc = Document::from(html);
    let selection = doc.select("table");
    let tables = selection.nodes();
    if tables.is_empty() {
        return Err(CoreError::Other("no <table> element found".into()));
    }
    let labels = tables.iter().enumerate().map(|(i, t)| table_label(i, t)).collect();
    let index = if index < tables.len() { index } else { 0 };
    let mut builder = Builder::new();
    for (i, tr) in table_rows(&tables[index]).iter().enumerate() {
        let mut cells = Vec::new();
        for cell in tr
            .element_children()
            .into_iter()
            .filter(|c| is_tag(c, "td") || is_tag(c, "th"))
        {
            cells.push(collapse_whitespace(&cell.text()));
            let span = cell
                .attr("colspan")
                .and_then(|s| s.trim().parse::<usize>().ok())
                .unwrap_or(1)
                .clamp(1, 1000);
            cells.extend(std::iter::repeat_n(String::new(), span - 1));
        }
        if !builder.push(i + 1, cells) {
            break;
        }
    }
    Ok(builder.finish(SourceFormat::Html, labels, index, None))
}

fn is_tag(node: &NodeRef, tag: &str) -> bool {
    node.node_name().is_some_and(|n| n.eq_ignore_ascii_case(tag))
}

fn table_rows<'a>(table: &NodeRef<'a>) -> Vec<NodeRef<'a>> {
    let mut rows = Vec::new();
    for child in table.element_children() {
        if is_tag(&child, "tr") {
            rows.push(child);
        } else if ["thead", "tbody", "tfoot"].iter().any(|t| is_tag(&child, t)) {
            rows.extend(child.element_children().into_iter().filter(|c| is_tag(c, "tr")));
        }
    }
    rows
}

/// "Table 2: Caption (14 rows)" — caption, else `id`, else just the number.
fn table_label(i: usize, table: &NodeRef) -> String {
    let caption = table
        .element_children()
        .into_iter()
        .find(|c| is_tag(c, "caption"))
        .map(|c| collapse_whitespace(&c.text()))
        .filter(|s| !s.is_empty())
        .or_else(|| table.attr("id").map(|s| s.to_string()).filter(|s| !s.is_empty()));
    let rows = table_rows(table).len();
    let plural = if rows == 1 { "" } else { "s" };
    match caption {
        Some(c) => format!("Table {}: {c} ({rows} row{plural})", i + 1),
        None => format!("Table {} ({rows} row{plural})", i + 1),
    }
}

fn collapse_whitespace(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniffs_common_delimiters() {
        assert_eq!(sniff_delimiter("a,b,c\n1,2,3\n"), Some(b','));
        assert_eq!(sniff_delimiter("a;b;c\n1,5;2;3\n4;5,5;6\n"), Some(b';'));
        assert_eq!(sniff_delimiter("a\tb\n1\t2\n"), Some(b'\t'));
        assert_eq!(sniff_delimiter("a|b|c\n1|2|3"), Some(b'|'));
        assert_eq!(sniff_delimiter("just one column\nanother"), None);
    }

    #[test]
    fn sniff_ignores_delimiters_inside_quotes() {
        let text = "name;note\n\"Smith, J\";\"a, b, c\"\n\"Doe, K\";\"x, y\"\n";
        assert_eq!(sniff_delimiter(text), Some(b';'));
    }

    #[test]
    fn parses_quoted_fields_and_ragged_rows() {
        let text = "id,text\n1,\"hello, world\"\n2,\"line one\nline two\"\n3,\"say \"\"hi\"\"\",extra\n\n4\n";
        let t = parse_delimited(text, None, b',').unwrap();
        assert_eq!(t.source.delimiter, Some(','));
        assert_eq!(t.rows.len(), 5);
        assert_eq!(t.rows[1], vec!["1", "hello, world"]);
        assert_eq!(t.rows[2][1], "line one\nline two");
        assert_eq!(t.rows[3], vec!["3", "say \"hi\"", "extra"]);
        assert_eq!(t.rows[4], vec!["4"]);
        assert_eq!(t.source.column_count, 3);
        assert_eq!(t.row_numbers, vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn explicit_delimiter_overrides_sniffing() {
        let t = parse_delimited("a,b;c\n1,2;3\n", Some(b';'), b',').unwrap();
        assert_eq!(t.rows[0], vec!["a,b", "c"]);
    }

    #[test]
    fn extracts_html_tables() {
        let html = r#"<html><body>
            <table id="first"><tr><td>x</td></tr></table>
            <table><caption>People</caption>
              <thead><tr><th>Name</th><th>Age</th></tr></thead>
              <tbody>
                <tr><td> Ann
                    Lee </td><td>31</td></tr>
                <tr><td colspan="2">merged</td></tr>
                <tr><td>Bob</td><td><table><tr><td>nested</td></tr></table></td></tr>
              </tbody>
            </table></body></html>"#;
        let t = parse_html(html, 1).unwrap();
        assert_eq!(t.source.sheets.len(), 3, "nested table is listed too");
        assert_eq!(t.source.sheets[0], "Table 1: first (1 row)");
        assert_eq!(t.source.sheets[1], "Table 2: People (4 rows)");
        assert_eq!(t.rows[0], vec!["Name", "Age"]);
        assert_eq!(t.rows[1], vec!["Ann Lee", "31"]);
        assert_eq!(t.rows[2], vec!["merged", ""]);
        assert_eq!(t.rows.len(), 4);
        assert!(parse_html("<p>no tables</p>", 0).is_err());
        // Unclosed cells are fine for an HTML5 parser.
        let loose = parse_html("<table><tr><td>a<td>b<tr><td>c</table>", 0).unwrap();
        assert_eq!(loose.rows, vec![vec!["a", "b"], vec!["c"]]);
    }

    #[test]
    fn loads_by_extension_and_sniffs_html_content() {
        let dir = tempfile::tempdir().unwrap();
        let tsv = dir.path().join("data.tsv");
        std::fs::write(&tsv, "a\tb\n1\t2\n").unwrap();
        let t = load_table(&tsv, &LoadOptions::default()).unwrap();
        assert_eq!(t.source.delimiter, Some('\t'));
        assert_eq!(t.source.encoding.as_deref(), Some("UTF-8"));

        let disguised = dir.path().join("export.txt");
        std::fs::write(&disguised, "<!DOCTYPE html><table><tr><td>v</td></tr></table>").unwrap();
        assert_eq!(
            load_table(&disguised, &LoadOptions::default()).unwrap().source.format,
            SourceFormat::Html
        );

        let bin = dir.path().join("blob.csv");
        std::fs::write(&bin, b"a,b\0\x01").unwrap();
        assert!(load_table(&bin, &LoadOptions::default()).is_err());
    }

    #[test]
    fn reads_generated_xlsx() {
        use rust_xlsxwriter::Workbook;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("book.xlsx");
        let mut wb = Workbook::new();
        let first = wb.add_worksheet().set_name("Summary").unwrap();
        first.write_string(0, 0, "only").unwrap();
        let data = wb.add_worksheet().set_name("Data").unwrap();
        data.write_string(1, 1, "id").unwrap();
        data.write_string(1, 2, "price").unwrap();
        data.write_number(2, 1, 1).unwrap();
        data.write_number(2, 2, 2.5).unwrap();
        data.write_boolean(3, 1, true).unwrap();
        wb.save(&path).unwrap();

        let t = load_table(
            &path,
            &LoadOptions {
                delimiter: None,
                sheet: Some(1),
            },
        )
        .unwrap();
        assert_eq!(t.source.format, SourceFormat::Workbook);
        assert_eq!(t.source.sheets, vec!["Summary", "Data"]);
        assert_eq!(t.source.sheet, 1);
        assert_eq!(t.row_numbers, vec![2, 3, 4]);
        assert_eq!(t.rows[0], vec!["", "id", "price"]);
        assert_eq!(t.rows[1], vec!["", "1", "2.5"]);
        assert_eq!(t.rows[2][1], "true");

        // An out-of-range sheet index falls back to the first sheet.
        let t = load_table(
            &path,
            &LoadOptions {
                delimiter: None,
                sheet: Some(9),
            },
        )
        .unwrap();
        assert_eq!(t.source.sheet, 0);
        assert_eq!(t.rows, vec![vec!["only"]]);
    }
}
