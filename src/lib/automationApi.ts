// Typed bridge to the sync automation commands in src-tauri/src/automation.rs.
// Field names mirror the serde `camelCase` structs in pcompare-automation,
// pcompare-notify and pcompare-index (automation.rs).
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import type { FolderCompareOptions } from "./api";
import type { ConflictPolicy, ExecOptions, SyncMatrix, SyncMode } from "./syncApi";

// ---------- triggers ----------
export type Trigger =
  /** Every N minutes, counted from the end of the previous run. */
  | { kind: "interval"; everyMinutes: number }
  /** "HH:MM" times on weekdays (0 = Monday … 6 = Sunday; empty = every day). */
  | { kind: "daily"; times: string[]; weekdays: number[] }
  | { kind: "cron"; expr: string }
  /** Watch the source side(s); sync once changes settle. */
  | { kind: "realtime"; debounceSecs: number; maxDelaySecs: number; rescanMinutes: number }
  | { kind: "onStartup"; delaySecs: number };

export type TriggerKind = Trigger["kind"];

export type RunTrigger =
  | "manual"
  | "interval"
  | "daily"
  | "cron"
  | "realtime"
  | "rescan"
  | "startup"
  | "catchUp"
  | "retry";

export type RunStatus =
  | "running"
  | "succeeded"
  | "noChanges"
  | "completedWithErrors"
  | "failed"
  | "aborted"
  | "cancelled"
  | "interrupted";

// ---------- tasks ----------
export interface SafetyOptions {
  maxDeletions: number | null;
  maxDeletionPercent: number | null;
  abortOnEmptySide: boolean;
  abortOnMassDeletion: boolean;
  timeoutMinutes: number | null;
}

export interface RunRetry {
  attempts: number;
  delaySecs: number;
}

export interface NotifyRules {
  /** Notification channel ids. */
  channels: string[];
  desktop: boolean;
  onFailure: boolean;
  onWarning: boolean;
  onSuccess: boolean;
  onRecovery: boolean;
  /** The same failure isn't re-sent more often than this; 0 = every time. */
  repeatAfterMinutes: number;
}

export interface SyncTask {
  /** "" for a task not saved yet. */
  id: string;
  name: string;
  description: string;
  enabled: boolean;
  left: string;
  right: string;
  mode: SyncMode;
  /** bc5:sync-matrix — used in `custom` mode; mirrors the preset otherwise. */
  matrix?: SyncMatrix;
  /** bc5:sync-matrix — don't create orphan folders that hold no files. */
  skipEmptyFolders?: boolean;
  conflicts: ConflictPolicy;
  compare: FolderCompareOptions;
  exec: ExecOptions;
  triggers: Trigger[];
  catchUpMissed: boolean;
  safety: SafetyOptions;
  retry: RunRetry;
  notify: NotifyRules;
  createdMs: number;
  updatedMs: number;
}

export type IssueArea = "general" | "triggers" | "options" | "safety" | "notify";

export interface TaskIssue {
  /** Errors block saving; warnings don't. */
  error: boolean;
  area: IssueArea;
  message: string;
}

export interface AutomationSettings {
  /** Closing the window keeps PCompare running in the tray. */
  runInBackground: boolean;
  startAtLogin: boolean;
  paused: boolean;
  maxParallel: number;
  historyDays: number;
}

// ---------- live status ----------
export type TaskRunState = "idle" | "queued" | "running" | "waitingToRetry" | "disabled" | "paused";

export interface RunProgress {
  phase: "planning" | "executing";
  doneOps: number;
  totalOps: number;
  bytesDone: number;
  bytesTotal: number;
  current: string | null;
}

export interface TaskStatus {
  taskId: string;
  state: TaskRunState;
  currentRunId: string | null;
  currentTrigger: RunTrigger | null;
  attempt: number;
  progress: RunProgress | null;
  nextRunMs: number | null;
  nextTrigger: RunTrigger | null;
  retryAtMs: number | null;
  rerunPending: boolean;
  lastRunMs: number | null;
  lastStatus: RunStatus | null;
  lastSuccessMs: number | null;
  consecutiveFailures: number;
  watching: boolean;
  watchError: string | null;
}

export interface AutomationRun {
  id: string;
  taskId: string;
  taskName: string;
  trigger: RunTrigger;
  attempt: number;
  status: RunStatus;
  startedMs: number;
  finishedMs: number | null;
  /** The journaled sync job (open it in Sync History to inspect / roll back). */
  jobId: string | null;
  opsTotal: number;
  opsDone: number;
  opsFailed: number;
  conflicts: number;
  bytesCopied: number;
  message: string | null;
  /** Per-channel delivery results. */
  notifications: string[];
}

export interface UpcomingRun {
  atMs: number;
  trigger: RunTrigger;
}

// ---------- notification channels ----------
export type EmailSecurity = "tls" | "startTls" | "none";

export type ChannelSettings =
  | { kind: "teams" }
  | { kind: "slack" }
  | { kind: "webhook"; headers: [string, string][] }
  | {
      kind: "email";
      host: string;
      /** 0 = default for the security mode. */
      port: number;
      security: EmailSecurity;
      username: string;
      from: string;
      to: string[];
      cc: string[];
    };

export interface ChannelConfig {
  id: string;
  name: string;
  enabled: boolean;
  settings: ChannelSettings;
  createdMs: number;
  updatedMs: number;
}

/** When saving: undefined/null = keep the stored value, "" = remove it. */
export interface ChannelSecrets {
  url?: string | null;
  password?: string | null;
  signingKey?: string | null;
}

export interface SecretsPresent {
  url: boolean;
  password: boolean;
  signingKey: boolean;
}

export interface ChannelWithSecrets {
  channel: ChannelConfig;
  secrets: SecretsPresent;
}

// ---------- commands ----------
export const automationApi = {
  tasks: () => invoke<SyncTask[]>("automation_tasks"),
  /** A new task with defaults, optionally pre-filled from a Folder Sync session. */
  newTask: (prefill?: Partial<SyncTask>) => invoke<SyncTask>("automation_new_task", { prefill: prefill ?? null }),
  validate: (task: SyncTask) => invoke<TaskIssue[]>("automation_validate", { task }),
  /** Throws (string) when there are errors; returns the saved task + warnings. */
  save: (task: SyncTask) => invoke<[SyncTask, TaskIssue[]]>("automation_save", { task }),
  remove: (id: string) => invoke<void>("automation_delete", { id }),
  setEnabled: (id: string, enabled: boolean) => invoke<SyncTask>("automation_set_enabled", { id, enabled }),
  duplicate: (id: string) => invoke<SyncTask>("automation_duplicate", { id }),
  runNow: (id: string) => invoke<void>("automation_run_now", { id }),
  cancel: (id: string) => invoke<void>("automation_cancel", { id }),
  statuses: () => invoke<TaskStatus[]>("automation_statuses"),
  runs: (taskId: string | null, limit: number) => invoke<AutomationRun[]>("automation_runs", { taskId, limit }),
  upcoming: (triggers: Trigger[], count: number) =>
    invoke<UpcomingRun[]>("automation_upcoming", { triggers, count }),
  describeTrigger: (trigger: Trigger) => invoke<string>("automation_describe_trigger", { trigger }),
  settings: () => invoke<AutomationSettings>("automation_settings"),
  saveSettings: (settings: AutomationSettings) => invoke<void>("automation_save_settings", { settings }),
  /** Export tasks (no secrets) as JSON text; import merges by name (new ids). */
  exportTasks: (ids: string[]) => invoke<string>("automation_export", { ids }),
  importTasks: (json: string) => invoke<SyncTask[]>("automation_import", { json }),

  channels: () => invoke<ChannelWithSecrets[]>("notify_channels"),
  saveChannel: (id: string, name: string, enabled: boolean, settings: ChannelSettings, secrets: ChannelSecrets) =>
    invoke<ChannelConfig>("notify_save_channel", { id, name, enabled, settings, secrets }),
  deleteChannel: (id: string) => invoke<void>("notify_delete_channel", { id }),
  /** Sends a sample notification; throws with the delivery error. */
  testChannel: (id: string) => invoke<void>("notify_test_channel", { id }),
};

/** Emitted with a TaskStatus whenever a task's live status changes. */
export const STATUS_EVENT = "automation-status";
/** Emitted with { taskId, runId } when a run has been recorded. */
export const RUN_EVENT = "automation-run";
/** Emitted (no payload) when tasks, channels or settings change. */
export const CHANGED_EVENT = "automation-changed";

export const onStatus = (cb: (s: TaskStatus) => void): Promise<UnlistenFn> =>
  listen<TaskStatus>(STATUS_EVENT, (e) => cb(e.payload));
export const onRun = (cb: (r: { taskId: string; runId: string }) => void): Promise<UnlistenFn> =>
  listen<{ taskId: string; runId: string }>(RUN_EVENT, (e) => cb(e.payload));
export const onChanged = (cb: () => void): Promise<UnlistenFn> => listen(CHANGED_EVENT, () => cb());
/** Emitted (no payload) by the tray menu's "Sync Automation…": open/focus the Automation tab. */
export const OPEN_EVENT = "automation-open";
export const onOpenRequest = (cb: () => void): Promise<UnlistenFn> => listen(OPEN_EVENT, () => cb());
