//! Table (CSV / TSV / Excel / HTML) comparison engine for PCompare.
//!
//! Both sides are loaded into a grid of strings ([`load`]), columns are paired
//! (by header name when both sides have headers, otherwise by position, or by
//! an explicit mapping), rows are optionally sorted, then aligned (by key
//! columns, or by a diff over whole rows) and every paired row is compared
//! cell by cell. [`sheets`] runs that over every worksheet / HTML table.

pub mod dates;
pub mod load;
pub mod sheets;

pub use load::{load_table, LoadOptions, TableBook, MAX_CELLS, MAX_ROWS};
pub use sheets::{compare_all_sheets, pair_sheets, SheetColumns, SheetComparison, SheetsCompareResult};

use pcompare_core::Result;
use serde::{Deserialize, Serialize};
use similar::{Algorithm, DiffOp};
use std::borrow::Cow;
use std::cmp::Ordering;
use std::collections::{HashMap, VecDeque};
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::time::{Duration, Instant};

/// Upper bound on row alignment; past it `similar` falls back to a coarser diff.
const DIFF_DEADLINE: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum SourceFormat {
    Delimited,
    Workbook,
    Html,
}

/// What was loaded for one side, plus the choices the UI can offer.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TableSource {
    pub format: SourceFormat,
    /// Worksheet names (workbooks) or table labels (HTML); empty for delimited text.
    pub sheets: Vec<String>,
    /// Index into `sheets` that was actually loaded.
    pub sheet: usize,
    /// Delimiter used for delimited text (detected or overridden).
    pub delimiter: Option<char>,
    /// Text encoding for delimited text and HTML.
    pub encoding: Option<String>,
    /// Rows loaded, including a header row.
    pub row_count: usize,
    pub column_count: usize,
    /// The source exceeded [`MAX_ROWS`] / [`MAX_CELLS`] and was cut short.
    pub truncated: bool,
}

/// A loaded grid of cells. Rows may be ragged.
#[derive(Debug, Clone)]
pub struct Table {
    pub rows: Vec<Vec<String>>,
    /// 1-based source row (record / spreadsheet row) for each entry in `rows`.
    pub row_numbers: Vec<usize>,
    pub source: TableSource,
}

impl Table {
    /// Builds an in-memory delimited table, mainly for tests and callers that
    /// already have parsed data.
    pub fn from_rows(rows: Vec<Vec<String>>) -> Self {
        let column_count = rows.iter().map(Vec::len).max().unwrap_or(0);
        Table {
            row_numbers: (1..=rows.len()).collect(),
            source: TableSource {
                format: SourceFormat::Delimited,
                sheets: Vec::new(),
                sheet: 0,
                delimiter: None,
                encoding: None,
                row_count: rows.len(),
                column_count,
                truncated: false,
            },
            rows,
        }
    }
}

/// One entry of a manual column mapping: 0-based source columns, either of
/// which may be `None` to leave the other side's column unmatched.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ColumnPair {
    pub left: Option<usize>,
    pub right: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TableCompareOptions {
    /// The first row of each side holds column names.
    pub has_header: bool,
    /// Per-side override of `has_header` (files with and without a header row).
    pub left_header: Option<bool>,
    pub right_header: Option<bool>,
    /// Pairs columns explicitly, in this order, instead of by header name /
    /// position. Source columns it doesn't mention are appended unmatched.
    pub column_mapping: Option<Vec<ColumnPair>>,
    /// Sort both sides before aligning rows.
    pub sort_rows: bool,
    /// Indices into the result's `columns` to sort on; empty sorts on all
    /// columns. Independent of `key_columns` (with keys it orders rows that
    /// share a key).
    pub sort_columns: Vec<usize>,
    /// Indices into the result's `columns`; rows are sorted and matched on these.
    pub key_columns: Vec<usize>,
    /// Indices into the result's `columns`; differences there are unimportant.
    pub ignored_columns: Vec<usize>,
    pub ignore_case: bool,
    pub trim_whitespace: bool,
    /// When set, cells that both parse as numbers are equal if they differ by
    /// at most this much (so `0.0` makes `1.0` equal `1`).
    pub numeric_tolerance: Option<f64>,
    /// When set, cells that both parse as dates / times ([`dates`]) are equal
    /// if they are at most this many seconds apart.
    pub date_tolerance: Option<f64>,
    /// Field delimiter for delimited text; sniffed when `None`.
    pub delimiter: Option<char>,
    /// Worksheet / HTML table index per side.
    pub left_sheet: Option<usize>,
    pub right_sheet: Option<usize>,
}

impl TableCompareOptions {
    /// Whether each side's first row holds column names.
    pub fn headers(&self) -> (bool, bool) {
        (
            self.left_header.unwrap_or(self.has_header),
            self.right_header.unwrap_or(self.has_header),
        )
    }
}

impl Default for TableCompareOptions {
    fn default() -> Self {
        TableCompareOptions {
            has_header: true,
            left_header: None,
            right_header: None,
            column_mapping: None,
            sort_rows: false,
            sort_columns: Vec::new(),
            key_columns: Vec::new(),
            ignored_columns: Vec::new(),
            ignore_case: false,
            trim_whitespace: true,
            numeric_tolerance: None,
            date_tolerance: None,
            delimiter: None,
            left_sheet: None,
            right_sheet: None,
        }
    }
}

/// A column as shown in the result: a pair of source columns, either of
/// which may be missing when one side has a column the other lacks.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ColumnInfo {
    /// 0-based source column on each side.
    pub left: Option<usize>,
    pub right: Option<usize>,
    /// Header text when `has_header` is on.
    pub left_name: Option<String>,
    pub right_name: Option<String>,
    /// Effective flags: a key must exist on both sides; a key is never ignored.
    pub key: bool,
    pub ignored: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum RowStatus {
    Same,
    Different,
    /// Only ignored columns differ.
    Unimportant,
    LeftOnly,
    RightOnly,
}

/// One aligned row. Cell vectors are laid out like `columns` (empty strings
/// where a side lacks the column or the source row is short).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TableRow {
    pub status: RowStatus,
    /// 1-based source rows.
    pub left_row: Option<usize>,
    pub right_row: Option<usize>,
    pub left: Option<Vec<String>>,
    pub right: Option<Vec<String>>,
    /// Indices into `columns` whose cells differ (ignored columns included).
    pub diffs: Vec<usize>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TableStats {
    pub same: usize,
    pub different: usize,
    pub unimportant: usize,
    pub left_only: usize,
    pub right_only: usize,
    /// Differing cells in non-ignored columns of paired rows.
    pub different_cells: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TableCompareResult {
    pub left: TableSource,
    pub right: TableSource,
    pub columns: Vec<ColumnInfo>,
    pub rows: Vec<TableRow>,
    pub stats: TableStats,
    /// No important differences (unimportant ones are allowed).
    pub identical: bool,
    /// Either side was cut short by the size caps.
    pub truncated: bool,
    /// Indices into `columns` the rows were sorted on before aligning
    /// (`None` when sorting was off).
    pub sorted_by: Option<Vec<usize>>,
}

/// Loads both files and compares them.
pub fn compare_files(left: &Path, right: &Path, options: &TableCompareOptions) -> Result<TableCompareResult> {
    let load = |path, sheet| {
        load_table(
            path,
            &LoadOptions {
                delimiter: options.delimiter,
                sheet,
            },
        )
    };
    let l = load(left, options.left_sheet)?;
    let r = load(right, options.right_sheet)?;
    Ok(compare_tables(&l, &r, options))
}

/// Compares two loaded tables.
pub fn compare_tables(left: &Table, right: &Table, options: &TableCompareOptions) -> TableCompareResult {
    let (lhead, rhead) = options.headers();
    let mut columns = pair_columns(left, right, lhead, rhead, options.column_mapping.as_deref());
    for &k in &options.key_columns {
        if let Some(c) = columns.get_mut(k) {
            c.key = c.left.is_some() && c.right.is_some();
        }
    }
    for &i in &options.ignored_columns {
        if let Some(c) = columns.get_mut(i) {
            c.ignored = !c.key;
        }
    }

    let cmp = Comparer::new(options);
    let body =
        |t: &Table, header: bool| -> Vec<usize> { (usize::from(header).min(t.rows.len())..t.rows.len()).collect() };
    let mut lrows = body(left, lhead);
    let mut rrows = body(right, rhead);

    let sorted_by = options.sort_rows.then(|| {
        let by: Vec<usize> = if options.sort_columns.is_empty() {
            (0..columns.len()).collect()
        } else {
            options
                .sort_columns
                .iter()
                .copied()
                .filter(|&i| i < columns.len())
                .collect()
        };
        let sort_cols: Vec<&ColumnInfo> = by.iter().map(|&i| &columns[i]).collect();
        sort_rows(left, &mut lrows, &sort_cols, |c| c.left, &cmp);
        sort_rows(right, &mut rrows, &sort_cols, |c| c.right, &cmp);
        by
    });

    let keys: Vec<&ColumnInfo> = columns.iter().filter(|c| c.key).collect();
    let pairs = if keys.is_empty() {
        align_by_diff(left, right, &lrows, &rrows, &columns, &cmp)
    } else {
        align_by_key(left, right, &lrows, &rrows, &keys, &cmp)
    };

    let mut stats = TableStats::default();
    let rows = pairs
        .into_iter()
        .map(|(l, r)| {
            let lcells = l.map(|i| project(&left.rows[i], &columns, |c| c.left));
            let rcells = r.map(|i| project(&right.rows[i], &columns, |c| c.right));
            let (status, diffs) = match (&lcells, &rcells) {
                (Some(a), Some(b)) => {
                    let diffs: Vec<usize> = (0..columns.len()).filter(|&c| !cmp.cells_equal(&a[c], &b[c])).collect();
                    let important = diffs.iter().filter(|&&c| !columns[c].ignored).count();
                    stats.different_cells += important;
                    let status = if important > 0 {
                        RowStatus::Different
                    } else if !diffs.is_empty() {
                        RowStatus::Unimportant
                    } else {
                        RowStatus::Same
                    };
                    (status, diffs)
                }
                (Some(_), None) => (RowStatus::LeftOnly, Vec::new()),
                _ => (RowStatus::RightOnly, Vec::new()),
            };
            match status {
                RowStatus::Same => stats.same += 1,
                RowStatus::Different => stats.different += 1,
                RowStatus::Unimportant => stats.unimportant += 1,
                RowStatus::LeftOnly => stats.left_only += 1,
                RowStatus::RightOnly => stats.right_only += 1,
            }
            TableRow {
                status,
                left_row: l.map(|i| left.row_numbers[i]),
                right_row: r.map(|i| right.row_numbers[i]),
                left: lcells,
                right: rcells,
                diffs,
            }
        })
        .collect();

    TableCompareResult {
        identical: stats.different == 0 && stats.left_only == 0 && stats.right_only == 0,
        truncated: left.source.truncated || right.source.truncated,
        left: left.source.clone(),
        right: right.source.clone(),
        columns,
        rows,
        stats,
        sorted_by,
    }
}

/// Stable sort of row indices by the given columns, comparing like keys do
/// (numbers by value, then text).
fn sort_rows(
    t: &Table,
    rows: &mut [usize],
    by: &[&ColumnInfo],
    side: fn(&ColumnInfo) -> Option<usize>,
    cmp: &Comparer,
) {
    rows.sort_by_cached_key(|&i| {
        by.iter()
            .map(|c| cmp.key_part(cell(&t.rows[i], side(c))))
            .collect::<Vec<_>>()
    });
}

fn cell(row: &[String], col: Option<usize>) -> &str {
    col.and_then(|i| row.get(i)).map_or("", String::as_str)
}

/// Reorders a source row into result-column order.
fn project(row: &[String], columns: &[ColumnInfo], side: impl Fn(&ColumnInfo) -> Option<usize>) -> Vec<String> {
    columns.iter().map(|c| cell(row, side(c)).to_owned()).collect()
}

fn width(t: &Table) -> usize {
    t.rows.iter().map(Vec::len).max().unwrap_or(0)
}

/// Builds the result columns. With a `mapping`, columns follow it; otherwise
/// they pair by header name when both sides have headers and at least one
/// name matches, else by position (see [`auto_pairs`]).
fn pair_columns(
    left: &Table,
    right: &Table,
    lhead: bool,
    rhead: bool,
    mapping: Option<&[ColumnPair]>,
) -> Vec<ColumnInfo> {
    let (lw, rw) = (width(left), width(right));
    let header = |t: &Table, w: usize, has: bool| -> Option<Vec<String>> {
        let first = t.rows.first().filter(|_| has)?;
        Some((0..w).map(|i| cell(first, Some(i)).trim().to_owned()).collect())
    };
    let lnames = header(left, lw, lhead);
    let rnames = header(right, rw, rhead);
    let pairs = match mapping {
        Some(m) => mapped_pairs(lw, rw, m),
        None => auto_pairs(lw, rw, lnames.as_deref(), rnames.as_deref()),
    };
    let name_of =
        |names: &Option<Vec<String>>, i: Option<usize>| -> Option<String> { Some(names.as_ref()?[i?].clone()) };
    pairs
        .into_iter()
        .map(|(l, r)| ColumnInfo {
            left: l,
            right: r,
            left_name: name_of(&lnames, l),
            right_name: name_of(&rnames, r),
            key: false,
            ignored: false,
        })
        .collect()
}

/// Applies a manual mapping: out-of-range or already used source columns
/// become `None`, empty pairs are dropped, and every source column the
/// mapping leaves out is appended unmatched (left ones first).
fn mapped_pairs(lw: usize, rw: usize, mapping: &[ColumnPair]) -> Vec<(Option<usize>, Option<usize>)> {
    let mut lused = vec![false; lw];
    let mut rused = vec![false; rw];
    let take = |i: Option<usize>, used: &mut [bool]| {
        let i = i.filter(|&i| i < used.len() && !used[i])?;
        used[i] = true;
        Some(i)
    };
    let mut out: Vec<_> = mapping
        .iter()
        .filter_map(|p| {
            let pair = (take(p.left, &mut lused), take(p.right, &mut rused));
            (pair != (None, None)).then_some(pair)
        })
        .collect();
    out.extend((0..lw).filter(|&i| !lused[i]).map(|i| (Some(i), None)));
    out.extend((0..rw).filter(|&i| !rused[i]).map(|i| (None, Some(i))));
    out
}

/// Pairs by header name when both sides have names and at least one matches;
/// otherwise by position. Duplicate names pair in order. Right-only columns
/// are placed after the column holding their nearest preceding right column.
fn auto_pairs(
    lw: usize,
    rw: usize,
    lnames: Option<&[String]>,
    rnames: Option<&[String]>,
) -> Vec<(Option<usize>, Option<usize>)> {
    let by_name = match (lnames, rnames) {
        (Some(ln), Some(rn)) => match_names(ln, rn),
        _ => None,
    };
    let Some(matched) = by_name else {
        return (0..lw.max(rw))
            .map(|i| ((i < lw).then_some(i), (i < rw).then_some(i)))
            .collect();
    };

    let mut cols: Vec<(Option<usize>, Option<usize>)> = (0..lw).map(|l| (Some(l), matched[l])).collect();
    let mut used = vec![false; rw];
    matched.iter().flatten().for_each(|&r| used[r] = true);
    for r in (0..rw).filter(|&r| !used[r]) {
        let nearest = |before: bool| {
            let side = cols.iter().enumerate().filter_map(|(p, c)| c.1.map(|x| (p, x)));
            if before {
                side.filter(|&(_, x)| x < r).max_by_key(|&(_, x)| x).map(|(p, _)| p + 1)
            } else {
                side.filter(|&(_, x)| x > r).min_by_key(|&(_, x)| x).map(|(p, _)| p)
            }
        };
        let pos = nearest(true).or_else(|| nearest(false)).unwrap_or(cols.len());
        cols.insert(pos, (None, Some(r)));
    }
    cols
}

/// For each left header, the right column with the same name (exact match
/// first, then case-insensitive). `None` when no names match at all.
fn match_names(left: &[String], right: &[String]) -> Option<Vec<Option<usize>>> {
    let mut result = vec![None; left.len()];
    let mut taken = vec![false; right.len()];
    fn norm(s: &str, fold: bool) -> Cow<'_, str> {
        if fold {
            Cow::Owned(s.to_lowercase())
        } else {
            Cow::Borrowed(s)
        }
    }
    for fold in [false, true] {
        let mut queues: HashMap<Cow<'_, str>, VecDeque<usize>> = HashMap::new();
        for (r, name) in right.iter().enumerate().filter(|&(r, _)| !taken[r]) {
            queues.entry(norm(name, fold)).or_default().push_back(r);
        }
        for (l, name) in left.iter().enumerate() {
            if result[l].is_some() {
                continue;
            }
            if let Some(r) = queues.get_mut(&norm(name, fold)).and_then(VecDeque::pop_front) {
                result[l] = Some(r);
                taken[r] = true;
            }
        }
    }
    result.iter().any(Option::is_some).then_some(result)
}

/// Cell comparison honouring case, whitespace and numeric options.
struct Comparer {
    ignore_case: bool,
    trim: bool,
    tolerance: Option<f64>,
    date_tolerance: Option<f64>,
}

impl Comparer {
    fn new(o: &TableCompareOptions) -> Self {
        Comparer {
            ignore_case: o.ignore_case,
            trim: o.trim_whitespace,
            tolerance: o.numeric_tolerance.map(sanitize_tolerance),
            date_tolerance: o.date_tolerance.map(sanitize_tolerance),
        }
    }

    fn norm<'a>(&self, s: &'a str) -> Cow<'a, str> {
        let s = if self.trim { s.trim() } else { s };
        if self.ignore_case && s.chars().any(char::is_uppercase) {
            Cow::Owned(s.to_lowercase())
        } else {
            Cow::Borrowed(s)
        }
    }

    fn cells_equal(&self, a: &str, b: &str) -> bool {
        let (a, b) = (self.norm(a), self.norm(b));
        if a == b {
            return true;
        }
        if let Some(tol) = self.tolerance {
            if let (Some(x), Some(y)) = (parse_number(&a), parse_number(&b)) {
                return within(x, y, tol);
            }
        }
        match (
            self.date_tolerance,
            dates::parse_datetime(&a),
            dates::parse_datetime(&b),
        ) {
            (Some(tol), Some(x), Some(y)) => within(x, y, tol),
            _ => false,
        }
    }

    /// Canonical text used for row hashing and keys: numbers collapse to one
    /// spelling when numeric comparison is on.
    /// Dates (with a date tolerance) become their epoch seconds behind a
    /// marker, so they never collide with plain numbers.
    fn canonical<'a>(&self, s: &'a str) -> Cow<'a, str> {
        let n = self.norm(s);
        if let Some(v) = self.tolerance.and(parse_number(&n)) {
            return Cow::Owned(format!("{}", v + 0.0)); // `+ 0.0` folds -0 into 0
        }
        match self.date_tolerance.and_then(|_| dates::parse_datetime(&n)) {
            Some(t) => Cow::Owned(format!("\u{1}date:{}", t + 0.0)),
            None => n,
        }
    }

    fn key_part(&self, s: &str) -> KeyPart {
        // Dates never parse as numbers, so checking them first is safe.
        if let Some(t) = self.date_tolerance.and_then(|_| dates::parse_datetime(&self.norm(s))) {
            return KeyPart::Date(t, String::new()); // any spelling of the same moment
        }
        let c = self.canonical(s);
        match parse_number(&c) {
            Some(v) => KeyPart::Num(v, c.into_owned()),
            None => KeyPart::Text(c.into_owned()),
        }
    }
}

fn sanitize_tolerance(t: f64) -> f64 {
    if t.is_finite() {
        t.abs()
    } else {
        0.0
    }
}

fn within(x: f64, y: f64, tol: f64) -> bool {
    // Slack for binary rounding, so 0.1 + 0.2 still equals 0.3.
    let slack = f64::EPSILON * 4.0 * x.abs().max(y.abs());
    (x - y).abs() <= tol + slack
}

fn parse_number(s: &str) -> Option<f64> {
    let t = s.trim();
    if t.is_empty() {
        return None;
    }
    t.parse::<f64>().ok().filter(|v| v.is_finite())
}

/// A key component with a total order: numbers (by value, then spelling)
/// sort before dates (by time, then spelling), which sort before text, so
/// keys like 2 < 10 and 9/1/2024 < 10/1/2024 sort naturally.
#[derive(Debug, Clone, PartialEq)]
enum KeyPart {
    Num(f64, String),
    Date(f64, String),
    Text(String),
}

impl KeyPart {
    fn rank(&self) -> u8 {
        match self {
            KeyPart::Num(..) => 0,
            KeyPart::Date(..) => 1,
            KeyPart::Text(_) => 2,
        }
    }
}

impl Eq for KeyPart {}

impl Ord for KeyPart {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (KeyPart::Num(a, sa), KeyPart::Num(b, sb)) | (KeyPart::Date(a, sa), KeyPart::Date(b, sb)) => {
                a.total_cmp(b).then_with(|| sa.cmp(sb))
            }
            (KeyPart::Text(a), KeyPart::Text(b)) => a.cmp(b),
            _ => self.rank().cmp(&other.rank()),
        }
    }
}

impl PartialOrd for KeyPart {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

type RowPair = (Option<usize>, Option<usize>);

/// Sorts both sides by key and merges them. Rows sharing a key pair up in
/// source order; surplus duplicates become orphans.
fn align_by_key(
    left: &Table,
    right: &Table,
    lrows: &[usize],
    rrows: &[usize],
    keys: &[&ColumnInfo],
    cmp: &Comparer,
) -> Vec<RowPair> {
    let sorted = |t: &Table, rows: &[usize], side: fn(&ColumnInfo) -> Option<usize>| {
        let mut keyed: Vec<(Vec<KeyPart>, usize)> = rows
            .iter()
            .map(|&i| {
                (
                    keys.iter().map(|c| cmp.key_part(cell(&t.rows[i], side(c)))).collect(),
                    i,
                )
            })
            .collect();
        keyed.sort_by(|a, b| a.0.cmp(&b.0)); // stable: duplicates keep source order
        keyed
    };
    let l = sorted(left, lrows, |c| c.left);
    let r = sorted(right, rrows, |c| c.right);

    let mut out = Vec::with_capacity(l.len().max(r.len()));
    let (mut i, mut j) = (0, 0);
    while i < l.len() && j < r.len() {
        match l[i].0.cmp(&r[j].0) {
            Ordering::Less => {
                out.push((Some(l[i].1), None));
                i += 1;
            }
            Ordering::Greater => {
                out.push((None, Some(r[j].1)));
                j += 1;
            }
            Ordering::Equal => {
                let li = (i..l.len()).find(|&k| l[k].0 != l[i].0).unwrap_or(l.len());
                let rj = (j..r.len()).find(|&k| r[k].0 != r[j].0).unwrap_or(r.len());
                for k in 0..(li - i).max(rj - j) {
                    out.push(((i + k < li).then(|| l[i + k].1), (j + k < rj).then(|| r[j + k].1)));
                }
                (i, j) = (li, rj);
            }
        }
    }
    out.extend(l[i..].iter().map(|x| (Some(x.1), None)));
    out.extend(r[j..].iter().map(|x| (None, Some(x.1))));
    out
}

/// Keeps source order and aligns rows with a diff over row hashes (ignored
/// columns excluded). Replaced runs pair up positionally as changed rows.
fn align_by_diff(
    left: &Table,
    right: &Table,
    lrows: &[usize],
    rrows: &[usize],
    columns: &[ColumnInfo],
    cmp: &Comparer,
) -> Vec<RowPair> {
    let compared: Vec<&ColumnInfo> = columns.iter().filter(|c| !c.ignored).collect();
    let hashes = |t: &Table, rows: &[usize], side: fn(&ColumnInfo) -> Option<usize>| -> Vec<u64> {
        rows.iter()
            .map(|&i| {
                let mut h = std::collections::hash_map::DefaultHasher::new();
                for c in &compared {
                    cmp.canonical(cell(&t.rows[i], side(c))).hash(&mut h);
                }
                h.finish()
            })
            .collect()
    };
    let lh = hashes(left, lrows, |c| c.left);
    let rh = hashes(right, rrows, |c| c.right);
    let ops =
        similar::capture_diff_slices_deadline(Algorithm::Patience, &lh, &rh, Some(Instant::now() + DIFF_DEADLINE));

    let mut out = Vec::with_capacity(lrows.len().max(rrows.len()));
    for op in ops {
        match op {
            DiffOp::Equal {
                old_index,
                new_index,
                len,
            } => {
                out.extend((0..len).map(|k| (Some(lrows[old_index + k]), Some(rrows[new_index + k]))));
            }
            DiffOp::Delete { old_index, old_len, .. } => {
                out.extend((0..old_len).map(|k| (Some(lrows[old_index + k]), None)));
            }
            DiffOp::Insert { new_index, new_len, .. } => {
                out.extend((0..new_len).map(|k| (None, Some(rrows[new_index + k]))));
            }
            DiffOp::Replace {
                old_index,
                old_len,
                new_index,
                new_len,
            } => {
                out.extend((0..old_len.max(new_len)).map(|k| {
                    (
                        (k < old_len).then(|| lrows[old_index + k]),
                        (k < new_len).then(|| rrows[new_index + k]),
                    )
                }));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(text: &str) -> Table {
        Table::from_rows(
            text.lines()
                .map(|l| l.split(',').map(str::to_owned).collect())
                .collect(),
        )
    }

    fn statuses(r: &TableCompareResult) -> Vec<RowStatus> {
        r.rows.iter().map(|r| r.status).collect()
    }

    use RowStatus::*;

    #[test]
    fn identical_tables() {
        let t = table("a,b\n1,2\n3,4");
        let r = compare_tables(&t, &t, &TableCompareOptions::default());
        assert!(r.identical);
        assert_eq!(r.stats.same, 2);
        assert_eq!(r.columns[0].left_name.as_deref(), Some("a"));
        assert_eq!(r.rows[0].left_row, Some(2), "header is row 1");
    }

    #[test]
    fn pairs_reordered_columns_by_header() {
        let l = table("id,name,age\n1,Ann,30\n2,Bob,40");
        let r = table("age,id,city,name\n30,1,Oslo,Ann\n41,2,Rome,Bob");
        let res = compare_tables(&l, &r, &TableCompareOptions::default());
        let pairs: Vec<_> = res.columns.iter().map(|c| (c.left, c.right)).collect();
        assert_eq!(
            pairs,
            vec![
                (Some(0), Some(1)),
                (None, Some(2)),
                (Some(1), Some(3)),
                (Some(2), Some(0))
            ]
        );
        assert_eq!(res.rows[0].left.as_ref().unwrap(), &vec!["1", "", "Ann", "30"]);
        assert_eq!(res.rows[0].right.as_ref().unwrap(), &vec!["1", "Oslo", "Ann", "30"]);
        // The right-only "city" column differs (it is empty on the left).
        assert_eq!(res.rows[0].diffs, vec![1]);
        assert_eq!(res.rows[1].diffs, vec![1, 3]);

        let ignore_city = TableCompareOptions {
            ignored_columns: vec![1],
            ..Default::default()
        };
        let res = compare_tables(&l, &r, &ignore_city);
        assert_eq!(statuses(&res), vec![Unimportant, Different]);
        assert_eq!(res.stats.different_cells, 1);
    }

    #[test]
    fn falls_back_to_position_without_headers_or_matches() {
        let l = table("a,b\n1,2");
        let r = table("x,y,z\n1,2,3");
        let res = compare_tables(&l, &r, &TableCompareOptions::default());
        assert_eq!(res.columns.len(), 3);
        assert_eq!(res.columns[2].left, None);
        assert_eq!(res.columns[1].right_name.as_deref(), Some("y"));

        let no_header = TableCompareOptions {
            has_header: false,
            ..Default::default()
        };
        let res = compare_tables(&l, &r, &no_header);
        assert_eq!(res.rows.len(), 2);
        assert_eq!(res.columns[0].left_name, None);
    }

    #[test]
    fn aligns_by_key_with_inserted_and_deleted_rows() {
        let l = table("id,v\n3,c\n1,a\n2,b\n5,e\n5,e2");
        let r = table("id,v\n1,a\n4,d\n2,B\n5,e\n3,c");
        let opts = TableCompareOptions {
            key_columns: vec![0],
            ..Default::default()
        };
        let res = compare_tables(&l, &r, &opts);
        let keys: Vec<_> = res
            .rows
            .iter()
            .map(|r| r.left.as_ref().or(r.right.as_ref()).unwrap()[0].clone())
            .collect();
        assert_eq!(keys, vec!["1", "2", "3", "4", "5", "5"]);
        assert_eq!(statuses(&res), vec![Same, Different, Same, RightOnly, Same, LeftOnly]);
        assert_eq!(res.rows[2].left_row, Some(2));
        assert_eq!(res.rows[2].right_row, Some(6));
        assert_eq!(
            res.rows[5].left.as_ref().unwrap()[1],
            "e2",
            "surplus duplicate is the later one"
        );
        assert!(res.columns[0].key);
        assert_eq!(res.stats.different_cells, 1);
    }

    #[test]
    fn numeric_keys_sort_naturally_and_text_after() {
        let l = table("k\n10\nb\n2\na");
        let res = compare_tables(
            &l,
            &l,
            &TableCompareOptions {
                key_columns: vec![0],
                ..Default::default()
            },
        );
        let keys: Vec<_> = res.rows.iter().map(|r| r.left.as_ref().unwrap()[0].as_str()).collect();
        assert_eq!(keys, vec!["2", "10", "a", "b"]);
    }

    #[test]
    fn aligns_by_diff_without_keys() {
        let l = table("id,v\n1,a\n2,b\n3,c\n4,d");
        let r = table("id,v\n1,a\n2,x\n3,c\n9,new\n4,d");
        let res = compare_tables(&l, &r, &TableCompareOptions::default());
        assert_eq!(statuses(&res), vec![Same, Different, Same, RightOnly, Same]);
        assert_eq!(res.rows[1].diffs, vec![1]);
        assert_eq!(res.rows[3].right_row, Some(5));
        assert_eq!(res.stats.right_only, 1);
        assert!(!res.identical);
    }

    #[test]
    fn ignored_columns_make_rows_unimportant() {
        let l = table("id,stamp\n1,2024\n2,2024");
        let r = table("id,stamp\n1,2025\n2,2024");
        let opts = TableCompareOptions {
            ignored_columns: vec![1],
            ..Default::default()
        };
        let res = compare_tables(&l, &r, &opts);
        assert_eq!(statuses(&res), vec![Unimportant, Same]);
        assert!(res.identical);
        assert_eq!(res.stats.different_cells, 0);
        assert!(res.columns[1].ignored);
    }

    #[test]
    fn key_columns_must_exist_on_both_sides_and_win_over_ignore() {
        let l = table("a,b\n1,2");
        let r = table("a\n1");
        let opts = TableCompareOptions {
            key_columns: vec![0, 1],
            ignored_columns: vec![0],
            has_header: false,
            ..Default::default()
        };
        let res = compare_tables(&l, &r, &opts);
        assert!(res.columns[0].key && !res.columns[0].ignored);
        assert!(!res.columns[1].key);
    }

    #[test]
    fn numeric_tolerance_and_text_options() {
        let l = table("v\n1.0\n0.30\n100\n ABC \n10");
        let r = table("v\n1\n0.3\n100.4\nabc\n11");
        let res = compare_tables(&l, &r, &TableCompareOptions::default());
        assert_eq!(statuses(&res), vec![Different; 5]);

        let opts = TableCompareOptions {
            numeric_tolerance: Some(0.5),
            ignore_case: true,
            ..Default::default()
        };
        let res = compare_tables(&l, &r, &opts);
        assert_eq!(statuses(&res), vec![Same, Same, Same, Same, Different]);

        let exact = TableCompareOptions {
            numeric_tolerance: Some(0.0),
            trim_whitespace: false,
            ..Default::default()
        };
        let res = compare_tables(&l, &r, &exact);
        assert_eq!(statuses(&res), vec![Same, Same, Different, Different, Different]);
    }

    #[test]
    fn numeric_keys_match_across_spellings() {
        let l = table("id,v\n1.0,a");
        let r = table("id,v\n1,a");
        let mut opts = TableCompareOptions {
            key_columns: vec![0],
            ..Default::default()
        };
        assert_eq!(statuses(&compare_tables(&l, &r, &opts)), vec![RightOnly, LeftOnly]);
        opts.numeric_tolerance = Some(0.0);
        assert_eq!(statuses(&compare_tables(&l, &r, &opts)), vec![Same]);
    }

    #[test]
    fn duplicate_header_names_pair_in_order() {
        let l = table("x,x,y\n1,2,3");
        let r = table("y,X,x\n3,2,1");
        let res = compare_tables(&l, &r, &TableCompareOptions::default());
        let pairs: Vec<_> = res.columns.iter().map(|c| (c.left, c.right)).collect();
        assert_eq!(pairs, vec![(Some(0), Some(2)), (Some(1), Some(1)), (Some(2), Some(0))]);
        assert!(res.identical);
    }

    #[test]
    fn compares_files_end_to_end() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.csv");
        let b = dir.path().join("b.csv");
        std::fs::write(&a, "id;name\n1;\"Ann; Jr\"\n2;Bob\n").unwrap();
        std::fs::write(&b, "id;name\n1;\"Ann; Jr\"\n2;Rob\n").unwrap();
        let res = compare_files(&a, &b, &TableCompareOptions::default()).unwrap();
        assert_eq!(res.left.delimiter, Some(';'));
        assert_eq!(statuses(&res), vec![Same, Different]);
        let json = serde_json::to_value(&res).unwrap();
        assert_eq!(json["rows"][1]["status"], "different");
        assert_eq!(json["stats"]["differentCells"], 1);
        assert!(compare_files(&a, &dir.path().join("missing.csv"), &TableCompareOptions::default()).is_err());
    }

    #[test]
    fn date_tolerance_compares_parsed_dates() {
        let l = table("when\n2024-03-05\n2024-03-05 10:00:00\n05.03.2024\n2024-03-05\nsoon\n12");
        let r = table("when\n3/5/2024\n2024-03-05T10:00:40Z\n2024-03-06\n5 Mar 2024 00:02\nsoon\n13");
        let res = compare_tables(&l, &r, &TableCompareOptions::default());
        assert_eq!(
            statuses(&res),
            vec![Different, Different, Different, Different, Same, Different]
        );

        let exact = TableCompareOptions {
            date_tolerance: Some(0.0),
            ..Default::default()
        };
        let res = compare_tables(&l, &r, &exact);
        assert_eq!(
            statuses(&res),
            vec![Same, Different, Different, Different, Same, Different]
        );

        let minute = TableCompareOptions {
            date_tolerance: Some(60.0),
            ..Default::default()
        };
        let res = compare_tables(&l, &r, &minute);
        assert_eq!(
            statuses(&res),
            vec![Same, Same, Different, Different, Same, Different],
            "40 s apart is within a minute, 2 min and 1 day are not; numbers are not dates"
        );

        let day = TableCompareOptions {
            date_tolerance: Some(86_400.0),
            ..Default::default()
        };
        let res = compare_tables(&l, &r, &day);
        assert_eq!(statuses(&res), vec![Same, Same, Same, Same, Same, Different]);
    }

    #[test]
    fn date_keys_match_across_spellings_and_sort_chronologically() {
        let l = table("d,v\n10/1/2024,b\n9/1/2024,a");
        let r = table("d,v\n2024-09-01,a\n2024-10-01,b");
        let opts = TableCompareOptions {
            key_columns: vec![0],
            date_tolerance: Some(0.0),
            ..Default::default()
        };
        let res = compare_tables(&l, &r, &opts);
        assert_eq!(statuses(&res), vec![Same, Same]);
        assert_eq!(res.rows[0].left.as_ref().unwrap()[0], "9/1/2024");
    }

    #[test]
    fn manual_column_mapping_overrides_header_pairing() {
        let l = table("id,first,last\n1,Ann,Lee");
        let r = table("key,surname,given,extra\n1,Lee,Ann,x");
        // Auto pairing finds no common names, so it goes by position.
        let res = compare_tables(&l, &r, &TableCompareOptions::default());
        assert_eq!(statuses(&res), vec![Different]);

        let pair = |l, r| ColumnPair { left: l, right: r };
        let opts = TableCompareOptions {
            column_mapping: Some(vec![
                pair(Some(0), Some(0)),
                pair(Some(1), Some(2)),
                pair(Some(2), Some(1)),
                pair(Some(9), None),    // out of range: dropped
                pair(Some(1), Some(3)), // left 1 already used: right 3 stays alone
            ]),
            ..Default::default()
        };
        let res = compare_tables(&l, &r, &opts);
        let pairs: Vec<_> = res.columns.iter().map(|c| (c.left, c.right)).collect();
        assert_eq!(
            pairs,
            vec![
                (Some(0), Some(0)),
                (Some(1), Some(2)),
                (Some(2), Some(1)),
                (None, Some(3))
            ]
        );
        assert_eq!(res.columns[1].left_name.as_deref(), Some("first"));
        assert_eq!(res.columns[1].right_name.as_deref(), Some("given"));
        assert_eq!(res.rows[0].diffs, vec![3]);

        // Columns the mapping leaves out are appended unmatched.
        let partial = TableCompareOptions {
            column_mapping: Some(vec![pair(Some(2), Some(1))]),
            ..Default::default()
        };
        let res = compare_tables(&l, &r, &partial);
        let pairs: Vec<_> = res.columns.iter().map(|c| (c.left, c.right)).collect();
        assert_eq!(
            pairs,
            vec![
                (Some(2), Some(1)),
                (Some(0), None),
                (Some(1), None),
                (None, Some(0)),
                (None, Some(2)),
                (None, Some(3))
            ]
        );
    }

    #[test]
    fn per_side_header_rows() {
        let l = table("id,name\n1,Ann\n2,Bob");
        let r = table("1,Ann\n2,Bob");
        let opts = TableCompareOptions {
            right_header: Some(false),
            column_mapping: Some(vec![
                ColumnPair {
                    left: Some(0),
                    right: Some(0),
                },
                ColumnPair {
                    left: Some(1),
                    right: Some(1),
                },
            ]),
            ..Default::default()
        };
        let res = compare_tables(&l, &r, &opts);
        assert!(res.identical);
        assert_eq!(res.rows.len(), 2);
        assert_eq!(res.columns[0].left_name.as_deref(), Some("id"));
        assert_eq!(res.columns[0].right_name, None);
        assert_eq!((res.rows[0].left_row, res.rows[0].right_row), (Some(2), Some(1)));
    }

    #[test]
    fn sorts_rows_before_aligning() {
        let l = table("id,v\n3,c\n1,a\n2,b\n10,j");
        let r = table("id,v\n1,a\n10,j\n2,B\n3,c");
        // In file order the shuffled rows don't line up.
        let res = compare_tables(&l, &r, &TableCompareOptions::default());
        assert!(res.stats.different + res.stats.left_only > 1);
        assert_eq!(res.sorted_by, None);

        // Sorting on all columns lines them up; 2 < 10 numerically.
        let all = TableCompareOptions {
            sort_rows: true,
            ..Default::default()
        };
        let res = compare_tables(&l, &r, &all);
        assert_eq!(res.sorted_by, Some(vec![0, 1]));
        let ids: Vec<_> = res.rows.iter().map(|r| r.left.as_ref().unwrap()[0].as_str()).collect();
        assert_eq!(ids, vec!["1", "2", "3", "10"]);
        assert_eq!(statuses(&res), vec![Same, Different, Same, Same]);
        assert_eq!(res.rows[1].diffs, vec![1]);

        // Sorting by the second column only.
        let by_v = TableCompareOptions {
            sort_rows: true,
            sort_columns: vec![1, 7],
            ignore_case: true,
            ..Default::default()
        };
        let res = compare_tables(&l, &r, &by_v);
        assert_eq!(res.sorted_by, Some(vec![1]));
        assert!(res.identical);
    }

    #[test]
    fn sort_orders_duplicate_keys() {
        let l = table("k,v\n1,b\n1,a");
        let r = table("k,v\n1,a\n1,b");
        let keyed = TableCompareOptions {
            key_columns: vec![0],
            ..Default::default()
        };
        assert_eq!(statuses(&compare_tables(&l, &r, &keyed)), vec![Different, Different]);
        let sorted = TableCompareOptions {
            sort_rows: true,
            sort_columns: vec![1],
            ..keyed
        };
        assert_eq!(statuses(&compare_tables(&l, &r, &sorted)), vec![Same, Same]);
    }

    #[test]
    fn options_deserialize_from_partial_json() {
        let o: TableCompareOptions = serde_json::from_str(r#"{"keyColumns":[1],"delimiter":"\t"}"#).unwrap();
        assert!(o.has_header);
        assert_eq!(o.key_columns, vec![1]);
        assert_eq!(o.delimiter, Some('\t'));
    }
}
