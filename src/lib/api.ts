// Typed bridge to the Rust commands in src-tauri/src/main.rs.
// Field names mirror the serde `camelCase` structs in pcompare-core.
import { invoke } from "@tauri-apps/api/core";

export type PathKind = "file" | "directory" | "missing";

// ---------- text ----------
export type WhitespaceMode = "compare" | "ignoreTrailing" | "ignoreChanges" | "ignoreAll";
export type DiffAlgorithm = "patience" | "myers";

export interface TextDiffOptions {
  whitespace: WhitespaceMode;
  ignoreCase: boolean;
  algorithm: DiffAlgorithm;
}

export type RowKind = "equal" | "unimportant" | "changed" | "leftOnly" | "rightOnly";

export interface Segment {
  text: string;
  changed: boolean;
}

export interface DiffRow {
  kind: RowKind;
  leftNo: number | null;
  rightNo: number | null;
  left: Segment[] | null;
  right: Segment[] | null;
}

export interface Hunk {
  startRow: number;
  endRow: number;
}

export interface TextDiffResult {
  rows: DiffRow[];
  hunks: Hunk[];
  stats: { equal: number; unimportant: number; changed: number; leftOnly: number; rightOnly: number };
  identical: boolean;
}

export type LineEnding = "lf" | "crlf" | "cr" | "mixed" | "none";

export interface TextFileInfo {
  path: string;
  encoding: string;
  hasBom: boolean;
  lineEnding: LineEnding;
  size: number;
}

export interface TextFileCompare {
  left: TextFileInfo;
  right: TextFileInfo;
  diff: TextDiffResult | null;
}

// ---------- folder ----------
export type CompareCriteria = "sizeAndTimestamp" | "size" | "content";

export interface FolderCompareOptions {
  criteria: CompareCriteria;
  recursive: boolean;
  exclude: string[];
  includeHidden: boolean;
  ignoreNameCase: boolean;
  timeToleranceSecs: number;
  ignoreDstShift: boolean;
}

export type EntryStatus = "same" | "different" | "leftOnly" | "rightOnly" | "typeMismatch" | "error";
export type Side = "left" | "right";

export interface EntryMeta {
  size: number;
  modifiedMs: number | null;
  isDir: boolean;
}

export interface FolderEntry {
  name: string;
  relPath: string;
  isDir: boolean;
  status: EntryStatus;
  newer: Side | null;
  left: EntryMeta | null;
  right: EntryMeta | null;
  children: FolderEntry[];
  error: string | null;
}

export interface FolderCompareResult {
  leftRoot: string;
  rightRoot: string;
  entries: FolderEntry[];
  stats: { same: number; different: number; leftOnly: number; rightOnly: number; errors: number };
  elapsedMs: number;
}

// ---------- hex ----------
export interface HexSummary {
  leftSize: number;
  rightSize: number;
  firstDifference: number | null;
  differingBytes: number;
  identical: boolean;
}

export interface HexChunk {
  offset: number;
  left: number[];
  right: number[];
}

export const api = {
  pathKind: (path: string) => invoke<PathKind>("path_kind", { path }),
  launchArgs: () => invoke<string[]>("launch_args"),
  compareTextFiles: (left: string, right: string, options: TextDiffOptions) =>
    invoke<TextFileCompare>("compare_text_files", { left, right, options }),
  compareFolders: (left: string, right: string, options: FolderCompareOptions) =>
    invoke<FolderCompareResult>("compare_folders", { left, right, options }),
  hexSummary: (left: string, right: string) => invoke<HexSummary>("hex_summary", { left, right }),
  hexChunk: (left: string, right: string, offset: number, len: number) =>
    invoke<HexChunk>("hex_chunk", { left, right, offset, len }),
  hexNextDifference: (left: string, right: string, from: number) =>
    invoke<number | null>("hex_next_difference", { left, right, from }),
};

export function errorMessage(e: unknown): string {
  return typeof e === "string" ? e : e instanceof Error ? e.message : JSON.stringify(e);
}
