// Typed bridge to the Folder Sync / Folder Merge commands in src-tauri/src/sync.rs.
// Field names mirror the serde `camelCase` structs in pcompare-sync.
import { Channel, invoke } from "@tauri-apps/api/core";
import type { EntryMeta, FolderCompareOptions } from "./api";

// ---------- folder sync ----------
export type SyncMode =
  | "updateLeftToRight"
  | "updateRightToLeft"
  | "updateBoth"
  | "mirrorLeftToRight"
  | "mirrorRightToLeft";

export type OpKind = "copy" | "delete" | "mkdir";
/** The side an operation writes to (or deletes from). */
export type Direction = "toLeft" | "toRight";

export interface SyncOp {
  id: number;
  kind: OpKind;
  direction: Direction;
  relPath: string;
  isDir: boolean;
  /** Bytes copied for a copy; total bytes removed for a delete. */
  size: number;
  /** A copy that replaces an existing file (both sides have it). */
  overwrite: boolean;
  reason: string;
}

export interface SyncSkip {
  relPath: string;
  isDir: boolean;
  reason: string;
  /** Needs the user's attention (type clash, error, ambiguous change). */
  conflict: boolean;
}

export interface SyncSummary {
  copyToRight: number;
  copyToLeft: number;
  deleteRight: number;
  deleteLeft: number;
  mkdirRight: number;
  mkdirLeft: number;
  bytesToCopy: number;
  conflicts: number;
  skipped: number;
}

export interface SyncPlan {
  leftRoot: string;
  rightRoot: string;
  mode: SyncMode;
  ops: SyncOp[];
  skipped: SyncSkip[];
  summary: SyncSummary;
  elapsedMs: number;
}

export interface SyncProgress {
  done: number;
  total: number;
  bytesDone: number;
  bytesTotal: number;
  current: string | null;
}

export interface OpResult {
  id: number;
  kind: OpKind;
  direction: Direction;
  relPath: string;
  ok: boolean;
  error: string | null;
}

export interface SyncReport {
  results: OpResult[];
  succeeded: number;
  failed: number;
  bytesCopied: number;
  cancelled: boolean;
  elapsedMs: number;
}

// ---------- 3-way folder merge ----------
export type MergeClass =
  | "unchanged"
  | "leftChanged"
  | "rightChanged"
  | "bothChangedSame"
  | "conflict"
  | "leftAdded"
  | "rightAdded"
  | "bothAddedSame"
  | "bothAddedConflict"
  | "leftDeleted"
  | "rightDeleted"
  | "bothDeleted"
  | "deleteModifyConflict"
  | "typeConflict"
  | "merged"
  | "error";

export type MergeAction = "takeLeft" | "takeRight" | "takeBase" | "delete" | "skip";

export interface FolderMergeEntry {
  name: string;
  relPath: string;
  isDir: boolean;
  class: MergeClass;
  /** Proposed default; for folders it governs only the folder itself. */
  action: MergeAction;
  base: EntryMeta | null;
  left: EntryMeta | null;
  right: EntryMeta | null;
  children: FolderMergeEntry[];
  error: string | null;
  conflicts: number;
  changes: number;
}

export interface FolderMergeResult {
  baseRoot: string;
  leftRoot: string;
  rightRoot: string;
  entries: FolderMergeEntry[];
  counts: Partial<Record<MergeClass, number>>;
  conflicts: number;
  changes: number;
  elapsedMs: number;
}

export interface MergeDecision {
  relPath: string;
  isDir: boolean;
  action: MergeAction;
}

export type MergeOutcome = "written" | "created" | "deleted" | "skipped" | "failed";

export interface MergeItemResult {
  relPath: string;
  isDir: boolean;
  outcome: MergeOutcome;
  error: string | null;
}

export interface MergeReport {
  results: MergeItemResult[];
  written: number;
  created: number;
  deleted: number;
  unchanged: number;
  skipped: number;
  failed: number;
  bytesWritten: number;
  elapsedMs: number;
}

export const syncApi = {
  plan: (left: string, right: string, mode: SyncMode, options: FolderCompareOptions) =>
    invoke<SyncPlan>("sync_plan", { left, right, mode, options }),
  /** Runs `ops`; progress arrives on `onProgress` until the promise settles. */
  execute: (left: string, right: string, ops: SyncOp[], jobId: string, onProgress: (p: SyncProgress) => void) => {
    const channel = new Channel<SyncProgress>();
    channel.onmessage = onProgress;
    return invoke<SyncReport>("sync_execute", { left, right, ops, jobId, onProgress: channel });
  },
  cancel: (jobId: string) => invoke<void>("sync_cancel", { jobId }),

  mergeCompare: (base: string, left: string, right: string, options: FolderCompareOptions) =>
    invoke<FolderMergeResult>("folder_merge_compare", { base, left, right, options }),
  mergeExecute: (base: string, left: string, right: string, output: string, decisions: MergeDecision[]) =>
    invoke<MergeReport>("folder_merge_execute", { base, left, right, output, decisions }),
};

/** Merge classes that need a decision from the user. */
export const CONFLICT_CLASSES: ReadonlySet<MergeClass> = new Set([
  "conflict",
  "bothAddedConflict",
  "deleteModifyConflict",
  "typeConflict",
  "error",
]);
