// Typed bridge to the Table Compare command in src-tauri/src/table.rs.
// Field names mirror the serde `camelCase` structs in pcompare-table.
import { invoke } from "@tauri-apps/api/core";

export type SourceFormat = "delimited" | "workbook" | "html";

export interface TableCompareOptions {
  hasHeader: boolean;
  /** Indices into `TableCompareResult.columns`. */
  keyColumns: number[];
  /** Indices into `TableCompareResult.columns`. */
  ignoredColumns: number[];
  ignoreCase: boolean;
  trimWhitespace: boolean;
  /** `null` compares cells as text; a number compares numeric cells within that tolerance. */
  numericTolerance: number | null;
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
}

export const tableApi = {
  compareTables: (left: string, right: string, options: TableCompareOptions) =>
    invoke<TableCompareResult>("compare_tables", { left, right, options }),
};
