// Typed bridge to the Table Compare command in src-tauri/src/table.rs.
// Field names mirror the serde `camelCase` structs in pcompare-table.
import { invoke } from "@tauri-apps/api/core";

export type SourceFormat = "delimited" | "workbook" | "html";

/** One manual column-mapping entry: 0-based source columns (`null` = unmatched). */
export interface ColumnPair {
  left: number | null;
  right: number | null;
}

export interface TableCompareOptions {
  hasHeader: boolean;
  /** Per-side override of `hasHeader`; `null` follows it. */
  leftHeader: boolean | null;
  rightHeader: boolean | null;
  /** Explicit column pairing in display order; `null` pairs by header name / position. */
  columnMapping: ColumnPair[] | null;
  /** Sort both sides before aligning rows. */
  sortRows: boolean;
  /** Indices into `TableCompareResult.columns`; empty sorts on all columns. */
  sortColumns: number[];
  /** Indices into `TableCompareResult.columns`. */
  keyColumns: number[];
  /** Indices into `TableCompareResult.columns`. */
  ignoredColumns: number[];
  ignoreCase: boolean;
  trimWhitespace: boolean;
  /** `null` compares cells as text; a number compares numeric cells within that tolerance. */
  numericTolerance: number | null;
  /** `null` compares dates as text; seconds of slack for cells that both parse as dates / times. */
  dateTolerance: number | null;
  /** Single character; `null` sniffs among `, ; Tab |`. */
  delimiter: string | null;
  leftSheet: number | null;
  rightSheet: number | null;
}

export interface TableSource {
  format: SourceFormat;
  /** Worksheet names or HTML table labels; empty for delimited text. */
  sheets: string[];
  sheet: number;
  delimiter: string | null;
  encoding: string | null;
  rowCount: number;
  columnCount: number;
  truncated: boolean;
}

export interface ColumnInfo {
  left: number | null;
  right: number | null;
  leftName: string | null;
  rightName: string | null;
  key: boolean;
  ignored: boolean;
}

export type RowStatus = "same" | "different" | "unimportant" | "leftOnly" | "rightOnly";

export interface TableRow {
  status: RowStatus;
  leftRow: number | null;
  rightRow: number | null;
  /** Cells in `columns` order; `null` when the row exists only on the other side. */
  left: string[] | null;
  right: string[] | null;
  /** Column indices whose cells differ (ignored columns included). */
  diffs: number[];
}

export interface TableStats {
  same: number;
  different: number;
  unimportant: number;
  leftOnly: number;
  rightOnly: number;
  differentCells: number;
}

export interface TableCompareResult {
  left: TableSource;
  right: TableSource;
  columns: ColumnInfo[];
  rows: TableRow[];
  stats: TableStats;
  identical: boolean;
  truncated: boolean;
  /** Column indices rows were sorted on before aligning; `null` when not sorted. */
  sortedBy: number[] | null;
}

/** Column choices that belong to one sheet pair (indices into that pair's columns). */
export interface SheetColumns {
  leftSheet: number | null;
  rightSheet: number | null;
  keyColumns: number[];
  ignoredColumns: number[];
  columnMapping: ColumnPair[] | null;
  sortColumns: number[];
}

export interface SheetComparison {
  left: number | null;
  right: number | null;
  leftLabel: string | null;
  rightLabel: string | null;
  /** Paired by name rather than by position / not at all. */
  byName: boolean;
  stats: TableStats | null;
  /** Important row differences, or every row of an unpaired sheet. */
  differences: number;
  identical: boolean;
  truncated: boolean;
  error: string | null;
}

export interface SheetsCompareResult {
  leftFormat: SourceFormat;
  rightFormat: SourceFormat;
  sheets: SheetComparison[];
  identical: boolean;
}

export const tableApi = {
  compareTables: (left: string, right: string, options: TableCompareOptions) =>
    invoke<TableCompareResult>("compare_tables", { left, right, options }),
  compareSheets: (left: string, right: string, options: TableCompareOptions, columns: SheetColumns[]) =>
    invoke<SheetsCompareResult>("compare_table_sheets", { left, right, options, columns }),
};
