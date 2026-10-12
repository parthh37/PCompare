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
  | "mirrorRightToLeft"
  /** True two-way sync against the state of the last sync (deletions propagate). */
  | "twoWay";

/** How two-way sync settles paths changed on both sides. */
export type ConflictPolicy = "skip" | "newerWins" | "leftWins" | "rightWins";

export type OpKind = "copy" | "delete" | "mkdir";
/** The side an operation writes to (or deletes from). */
export type Direction = "toLeft" | "toRight";

/** Size + modification time of one side of a path. */
export interface Snap {
  size: number;
  modifiedMs: number | null;
}

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
  /** The target as the preview saw it; the executor refuses to touch it if it changed since. */
  dstBefore: Snap | null;
  /** bc5:file-ops — the left side's own path when alignment paired differently named items. */
  leftRelPath?: string;
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

export type WarningKind = "emptySide" | "massDeletion" | "remotePermanent"; // bc5:remote-write

/** Something about a plan the user must acknowledge before running it. */
export interface PlanWarning {
  kind: WarningKind;
  message: string;
}

export interface SyncPlan {
  leftRoot: string;
  rightRoot: string;
  mode: SyncMode;
  ops: SyncOp[];
  skipped: SyncSkip[];
  summary: SyncSummary;
  warnings: PlanWarning[];
  /** A baseline from an earlier sync informed this (two-way) plan. */
  hasBaseline: boolean;
  elapsedMs: number;
}

// ----- execution options -----
/** What to do with a file still locked after all retries. */
export type LockedPolicy = "skip" | "defer" | "stop";
export type ErrorPolicy = "continue" | "stop";
/** Where deleted items and replaced files' old versions go. */
export type Disposal = { kind: "trash" } | { kind: "versioning"; folder: string } | { kind: "permanent" };

export interface ExecOptions {
  /** Extra attempts after the first for transient failures (locked, network…). */
  retryAttempts: number;
  /** First retry delay; doubles each time up to retryMaxDelayMs. */
  retryDelayMs: number;
  retryMaxDelayMs: number;
  onLocked: LockedPolicy;
  onError: ErrorPolicy;
  /** Read every copy back and compare checksums before it replaces anything. */
  verify: boolean;
  /** Refuse to overwrite/delete targets that changed since the preview. */
  checkDrift: boolean;
  disposal: Disposal;
  /** bc5:file-ops — Windows: copy NTFS owners and ACLs along (best effort). */
  copyAcls?: boolean;
  /** bc5:file-ops — Windows: carry DOS 8.3 short names over (needs administrator rights). */
  copyShortNames?: boolean;
}

export const DEFAULT_EXEC_OPTIONS: ExecOptions = {
  retryAttempts: 3,
  retryDelayMs: 500,
  retryMaxDelayMs: 8000,
  onLocked: "defer",
  onError: "continue",
  verify: false,
  checkDrift: true,
  disposal: { kind: "trash" },
};

export interface PreflightIssue {
  /** The sync can't run until this is fixed. */
  blocking: boolean;
  message: string;
}

// ----- progress and results -----
export type FailureKind =
  | "locked"
  | "accessDenied"
  | "notFound"
  | "diskFull"
  | "readOnly"
  | "badName"
  | "network"
  | "changedSincePreview"
  | "sourceChanged"
  | "verifyFailed"
  | "typeClash"
  | "invalid"
  | "cancelled"
  | "other";

export const FAILURE_LABELS: Record<FailureKind, string> = {
  locked: "File in use",
  accessDenied: "Access denied",
  notFound: "Not found",
  diskFull: "Disk full",
  readOnly: "Read-only",
  badName: "Invalid name or path too long",
  network: "Network error",
  changedSincePreview: "Changed since preview",
  sourceChanged: "Source changed during copy",
  verifyFailed: "Verification failed",
  typeClash: "File/folder clash",
  invalid: "Invalid operation",
  cancelled: "Cancelled",
  other: "Error",
};

export type ExecPhase = "running" | "retrying" | "deferred" | "paused" | "finished"; // bc5:file-ops — paused

export interface SyncProgress {
  done: number;
  total: number;
  bytesDone: number;
  bytesTotal: number;
  current: string | null;
  phase: ExecPhase;
  /** Attempt number of the current operation (1 = first try). */
  attempt: number;
  /** Why the current operation is being retried. */
  waitingOn: FailureKind | null;
  failed: number;
  /** Locked operations postponed to the end of the run. */
  deferred: number;
}

export interface LockHolder {
  pid: number;
  name: string;
}

export interface OpResult {
  id: number;
  kind: OpKind;
  direction: Direction;
  relPath: string;
  ok: boolean;
  /** The failure message, or a caveat on a successful op. */
  error: string | null;
  failure: FailureKind | null;
  attempts: number;
  bytes: number;
  /** Processes holding the file when it was locked (Windows). */
  lockedBy: LockHolder[];
  /** Where the replaced/deleted item can be restored from (versioning folder). */
  backup: string | null;
  written: Snap | null;
  /** Postponed because the file was locked, then retried at the end. */
  deferred: boolean;
}

export interface SyncReport {
  results: OpResult[];
  succeeded: number;
  failed: number;
  bytesCopied: number;
  cancelled: boolean;
  /** Why the run stopped itself (stop-on-error, disk full…). */
  stopped: string | null;
  /** Ids of operations that never ran (cancelled or stopped). */
  notRun: number[];
  elapsedMs: number;
}

/** What a sync run returns: its journal id, the report, and a fresh plan of what remains. */
export interface SyncRun {
  jobId: string | null;
  report: SyncReport;
  /** Re-planned after the run with the same settings; null if re-planning failed. */
  plan: SyncPlan | null;
}

// ----- job history (the app database) -----
export type JobStatus =
  | "running"
  | "completed"
  | "completedWithErrors"
  | "cancelled"
  | "stopped"
  | "interrupted"
  | "rolledBack";

export type OpState = "pending" | "running" | "done" | "failed" | "rolledBack";

export interface JobSummary {
  id: string;
  left: string;
  right: string;
  mode: SyncMode;
  status: JobStatus;
  startedMs: number;
  finishedMs: number | null;
  totalOps: number;
  doneOps: number;
  failedOps: number;
  /** Pending + running (never finished). */
  pendingOps: number;
  bytesCopied: number;
}

export interface JobOp {
  op: SyncOp;
  state: OpState;
  attempts: number;
  staged: string | null;
  result: OpResult | null;
  updatedMs: number;
}

export interface JobDetail {
  summary: JobSummary;
  options: ExecOptions;
  ops: JobOp[];
}

export type Recovery =
  | { outcome: "clean" }
  | { outcome: "restored" }
  | { outcome: "completed"; backup: string | null }
  | { outcome: "manual"; message: string };

export interface RollbackResult {
  id: number;
  relPath: string;
  ok: boolean;
  message: string | null;
}

export interface IndexStats {
  dbBytes: number;
  checksums: number;
  baselines: number;
  baselineEntries: number;
  jobs: number;
}

// ---------- folder merge (3-way, or 2-way without a base) ----------
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
  | "error"
  // Two-way merges (no base).
  | "leftOnly"
  | "rightOnly"
  | "leftNewer"
  | "rightNewer"
  | "different"
  /** A text file changed on both sides that merged line by line without conflicts. */
  | "autoMerged";

/** `mergeText` writes the automatic line-by-line merge ("Merged automatically"). */
export type MergeAction = "takeLeft" | "takeRight" | "takeBase" | "delete" | "skip" | "mergeText";

/** Settings of a folder merge beyond the comparison options. */
export interface FolderMergeOptions {
  /** Three-way: merge text files changed on both sides line by line. */
  autoMergeText: boolean;
  /** Two-way: propose the newer file where the sides differ (else they conflict). */
  takeNewer: boolean;
}

/** How a line-by-line merge of a text file changed on both sides went. */
export interface TextMergeSummary {
  changes: number;
  conflicts: number;
}

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
  /** Set for text files changed on both sides that were merged line by line. */
  textMerge: TextMergeSummary | null;
  conflicts: number;
  changes: number;
}

export interface FolderMergeResult {
  /** Empty in a two-way merge. */
  baseRoot: string;
  leftRoot: string;
  rightRoot: string;
  entries: FolderMergeEntry[];
  counts: Partial<Record<MergeClass, number>>;
  conflicts: number;
  changes: number;
  /** No base folder: items were classified by left vs right alone. */
  twoWay: boolean;
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
  plan: (left: string, right: string, mode: SyncMode, options: FolderCompareOptions, conflicts: ConflictPolicy) =>
    invoke<SyncPlan>("sync_plan", { left, right, mode, options, conflicts }),
  /** Checks folders, write access, free space and (with `exec`) the versioning folder. */
  preflight: (left: string, right: string, ops: SyncOp[], exec?: ExecOptions) =>
    invoke<PreflightIssue[]>("sync_preflight", { left, right, ops, exec: exec ?? null }),
  /**
   * Runs `ops` as a journaled job; progress arrives on `onProgress` until the promise settles.
   * `runKey` identifies the run for `cancel`. `compare`/`mode`/`conflicts` are used to re-plan
   * afterwards and to record the new two-way baseline.
   */
  execute: (args: {
    left: string;
    right: string;
    mode: SyncMode;
    ops: SyncOp[];
    exec: ExecOptions;
    compare: FolderCompareOptions;
    conflicts: ConflictPolicy;
    runKey: string;
    onProgress: (p: SyncProgress) => void;
  }) => {
    const { onProgress, ...rest } = args;
    const channel = new Channel<SyncProgress>();
    channel.onmessage = onProgress;
    return invoke<SyncRun>("sync_execute", { ...rest, onProgress: channel });
  },
  cancel: (runKey: string) => invoke<void>("sync_cancel", { runKey }),
  /** bc5:file-ops — holds (or resumes) a running sync between files and inside large copies. */
  pause: (runKey: string, paused: boolean) => invoke<void>("sync_pause", { runKey, paused }),

  /** The latest interrupted job for this pair (the app crashed or was killed mid-sync). */
  interrupted: (left: string, right: string) => invoke<JobSummary | null>("sync_interrupted", { left, right }),
  /** Repairs half-finished operations and runs everything that never completed. */
  resume: (args: {
    jobId: string;
    compare: FolderCompareOptions;
    conflicts: ConflictPolicy;
    runKey: string;
    onProgress: (p: SyncProgress) => void;
  }) => {
    const { onProgress, ...rest } = args;
    const channel = new Channel<SyncProgress>();
    channel.onmessage = onProgress;
    return invoke<SyncRun>("sync_resume", { ...rest, onProgress: channel });
  },
  /** Repairs half-finished operations (puts staged old versions back) and closes the job. */
  discard: (jobId: string) => invoke<Recovery[]>("sync_discard", { jobId }),
  /** Undoes a job's completed operations, newest first. */
  rollback: (jobId: string) => invoke<RollbackResult[]>("sync_rollback", { jobId }),
  jobs: (left: string | null, right: string | null, limit = 50) =>
    invoke<JobSummary[]>("sync_jobs", { left, right, limit }),
  jobDetail: (jobId: string) => invoke<JobDetail>("sync_job_detail", { jobId }),
  /** Drops the two-way baseline of a pair (the next two-way sync starts fresh). */
  forgetHistory: (left: string, right: string) => invoke<void>("sync_forget_history", { left, right }),
  defaultVersioningFolder: () => invoke<string>("sync_default_versioning_folder"),
  indexStats: () => invoke<IndexStats>("index_stats"),
  clearChecksums: () => invoke<number>("index_clear_checksums"),

  /** An empty `base` runs a two-way merge. */
  mergeCompare: (
    base: string,
    left: string,
    right: string,
    options: FolderCompareOptions,
    mergeOptions: FolderMergeOptions,
  ) => invoke<FolderMergeResult>("folder_merge_compare", { base, left, right, options, mergeOptions }),
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
  "different",
]);
