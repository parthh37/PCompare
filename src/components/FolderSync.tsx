import {
  ArrowDown,
  ArrowLeft,
  ArrowLeftRight,
  ArrowLeftToLine,
  ArrowRight,
  ArrowRightToLine,
  ArrowUp,
  CalendarClock, // automation:ui
  CircleCheck,
  CircleX,
  ClipboardCopy,
  Clock,
  FolderPlus,
  FolderSync as FolderSyncIcon,
  Grid3x3,
  Info,
  ListChecks,
  ListX,
  LoaderCircle,
  Lock,
  Play,
  RefreshCw,
  Repeat2,
  RotateCcw,
  Settings2,
  ShieldCheck,
  StepForward,
  Trash,
  TriangleAlert,
  Undo2,
  X,
  type LucideIcon,
} from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState, type MouseEvent as ReactMouseEvent } from "react";
import { errorMessage, isRemote, type Side } from "../lib/api";
import { syncTones, toneColor, type Tone } from "../lib/folderColors";
import { COMPARE_PRESETS, DEFAULT_FOLDER_SETTINGS, toEngineOptions, type ComparePreset } from "../lib/folderSettings";
import { basename, formatSize, formatTime, joinPath } from "../lib/format";
import type { SyncTask } from "../lib/automationApi"; // automation:ui
import { matches, shortcutLabel } from "../lib/keymap";
import type { CompareProps } from "../lib/sessionProps";
import { SEPARATOR, type MenuItems } from "../lib/nativeMenu";
import { ACTIONS, PRESET_MATRICES, actionApplies, matrixDeletes, presetOf } from "../lib/syncMatrix";
import {
  DEFAULT_EXEC_OPTIONS,
  FAILURE_LABELS,
  syncApi,
  type ConflictPolicy,
  type ExecOptions,
  type FailureKind,
  type JobDetail,
  type JobSummary,
  type PlanRules,
  type OpResult,
  type PreflightIssue,
  type Recovery,
  type RollbackResult,
  type SyncAction,
  type SyncMatrix,
  type SyncMode,
  type SyncOp,
  type SyncPlan,
  type SyncProgress,
  type SyncReport,
  type SyncRun,
  type SyncSkip,
} from "../lib/syncApi";
import { useContextMenu } from "./ContextMenu";
import { Modal } from "./Modal";
import { SyncMatrixEditor } from "./SyncMatrixEditor";
import { PathBar } from "./PathBar";
import { SyncHistoryDialog } from "./SyncHistoryDialog";
import { SyncOptionsDialog, TRASH_NAME, safetySummary } from "./SyncOptionsDialog";
import { VirtualList } from "./VirtualList";
import { FolderToneIcon, ToneSpot } from "./FolderStatusLegend";
import "./FolderSync.css";

/** "1 file", "3 files". */
const count = (n: number, noun: string) => `${n.toLocaleString()} ${noun}${n === 1 ? "" : "s"}`;
const lowerFirst = (s: string) => s.charAt(0).toLowerCase() + s.slice(1);
const seconds = (ms: number) => `${(ms / 1000).toFixed(1)} s`;

/** What a saved sync session remembers besides its paths. */
export interface FolderSyncSettings {
  mode: SyncMode;
  compare: ComparePreset;
  exclude: string;
  includeHidden: boolean;
  /** How two-way mode settles paths changed on both sides. */
  conflicts: ConflictPolicy;
  /** Retries, locked-file policy, verification and disposal for runs. */
  exec: ExecOptions;
  /** One action per category; planning uses it in Custom mode. */
  matrix: SyncMatrix;
  /** Don't create orphan folders that hold no files. */
  skipEmptyFolders: boolean;
  /** Only these items (Folder Compare's "Synchronize selection"); absent = everything. */
  scope?: string[];
}

interface Props extends CompareProps<FolderSyncSettings> {
  /** Opens a two-way compare of one file pair; wired by the shell when available. */
  onOpenFiles?: (left: string, right: string) => void;
  // automation:ui
  /** Opens the Sync Automation editor pre-filled with this pair and its settings. */
  onAutomate?: (prefill: Partial<SyncTask>) => void;
  // automation:ui end
}

export interface ModeInfo { // automation:ui — exported for the Sync Automation editor
  value: SyncMode;
  label: string;
  icon: LucideIcon;
  hint: string;
  /** The mode can delete items (shown with a warning tint). */
  deletes?: boolean;
}

export const MODES: ModeInfo[] = [ // automation:ui — exported
  {
    value: "updateLeftToRight",
    label: "Update left → right",
    icon: ArrowRight,
    hint: "Copy newer and left-only items to the right. Nothing is deleted.",
  },
  {
    value: "updateRightToLeft",
    label: "Update right → left",
    icon: ArrowLeft,
    hint: "Copy newer and right-only items to the left. Nothing is deleted.",
  },
  {
    value: "updateBoth",
    label: "Update both",
    icon: ArrowLeftRight,
    hint: "The newer file wins in each direction; orphans are copied both ways. Nothing is deleted.",
  },
  {
    value: "twoWay",
    label: "Two-way (with history)",
    icon: Repeat2,
    hint: "Carries changes and deletions both ways using the state of the last sync; paths changed on both sides are conflicts.",
    deletes: true,
  },
  {
    value: "mirrorLeftToRight",
    label: "Mirror left → right",
    icon: ArrowRightToLine,
    hint: "Make the right side identical to the left, deleting right-only items.",
    deletes: true,
  },
  {
    value: "mirrorRightToLeft",
    label: "Mirror right → left",
    icon: ArrowLeftToLine,
    hint: "Make the left side identical to the right, deleting left-only items.",
    deletes: true,
  },
  {
    value: "custom",
    label: "Custom…",
    icon: Grid3x3,
    hint: "Your own action for each kind of difference: newer, orphan, deleted, different, same.",
  },
];

export const modeLabel = (m: SyncMode) => // automation:ui — exported
  MODES.find((x) => x.value === m)?.label ?? m;

export const CONFLICT_POLICIES: { value: ConflictPolicy; label: string }[] = [ // automation:ui — exported
  { value: "skip", label: "Skip (list them)" },
  { value: "newerWins", label: "Newer wins" },
  { value: "leftWins", label: "Left wins" },
  { value: "rightWins", label: "Right wins" },
];

type OpFilter = "all" | "copyRight" | "copyLeft" | "delete" | "mkdir" | "skipped";
type SortKey = "plan" | "path" | "size";

const FILTERS: { value: OpFilter; label: string }[] = [
  { value: "all", label: "All" },
  { value: "copyRight", label: "Copy →" },
  { value: "copyLeft", label: "Copy ←" },
  { value: "delete", label: "Delete" },
  { value: "mkdir", label: "New folder" },
  { value: "skipped", label: "Skipped" },
];

const DEFAULTS: FolderSyncSettings = {
  mode: "updateLeftToRight",
  compare: "sizeAndTimestamp",
  exclude: DEFAULT_FOLDER_SETTINGS.exclude,
  includeHidden: true,
  conflicts: "skip",
  exec: DEFAULT_EXEC_OPTIONS,
  matrix: PRESET_MATRICES.updateLeftToRight,
  skipEmptyFolders: false,
};

/** Saved sessions may be partial or predate newer fields; anything unknown falls back to the defaults. */
function restoreSettings(s: Partial<FolderSyncSettings> | undefined): FolderSyncSettings {
  return {
    ...DEFAULTS,
    ...s,
    mode: MODES.some((m) => m.value === s?.mode) ? s!.mode! : DEFAULTS.mode,
    compare: s?.compare && s.compare in COMPARE_PRESETS ? s.compare : DEFAULTS.compare,
    conflicts: CONFLICT_POLICIES.some((c) => c.value === s?.conflicts) ? s!.conflicts! : DEFAULTS.conflicts,
    exec: { ...DEFAULT_EXEC_OPTIONS, ...s?.exec },
    // sessions saved before the matrix start from their mode's preset.
    matrix: s?.matrix
      ? { ...PRESET_MATRICES.updateLeftToRight, ...s.matrix }
      : PRESET_MATRICES[s?.mode && s.mode !== "custom" && s.mode in PRESET_MATRICES ? s.mode : DEFAULTS.mode as Exclude<SyncMode, "custom">],
    skipEmptyFolders: !!s?.skipEmptyFolders,
    scope: Array.isArray(s?.scope) && s.scope.length ? s.scope : undefined,
  };
}

// ---------- per-row actions ----------

/** Which sides hold a preview row's item. */
function rowSides(row: Row): { left: boolean; right: boolean } {
  if ("skip" in row) return { left: row.skip.onLeft, right: row.skip.onRight };
  const op = row.op;
  const toRight = op.direction === "toRight";
  const both = { left: true, right: true };
  switch (op.kind) {
    case "mkdir":
      return { left: toRight, right: !toRight };
    case "copy":
      return op.overwrite ? both : { left: toRight, right: !toRight };
    case "delete": {
      const oneSided = op.category && ["leftOrphan", "rightOrphan", "leftDeleted", "rightDeleted"].includes(op.category);
      return oneSided ? { left: !toRight, right: toRight } : both;
    }
  }
}

const rowPath = (row: Row) => ("skip" in row ? row.skip.relPath : row.op.relPath);
/** Identifies an operation across re-plans (ids are renumbered). */
const opKey = (op: SyncOp) => `${op.kind}|${op.direction}|${op.relPath}`;

/** The actions a row can be set to (Prompt is for the matrix, not for rows). */
const ROW_ACTIONS = ACTIONS.filter((a) => a.value !== "prompt");

function opMatches(op: SyncOp, f: OpFilter): boolean {
  switch (f) {
    case "all":
      return true;
    case "copyRight":
      return op.kind === "copy" && op.direction === "toRight";
    case "copyLeft":
      return op.kind === "copy" && op.direction === "toLeft";
    case "delete":
      return op.kind === "delete";
    case "mkdir":
      return op.kind === "mkdir";
    case "skipped":
      return false;
  }
}

function opLabel(op: Pick<SyncOp, "kind" | "direction">): { icon: LucideIcon; text: string } {
  const side = op.direction === "toRight" ? "right" : "left";
  switch (op.kind) {
    case "copy":
      return { icon: op.direction === "toRight" ? ArrowRight : ArrowLeft, text: `Copy to ${side}` };
    case "delete":
      return { icon: Trash, text: `Delete on ${side}` };
    case "mkdir":
      return { icon: FolderPlus, text: `Create on ${side}` };
  }
}

/** The side an op works from: where a copy (or a new folder) comes from, or where a delete removes the item. */
function actingSide(op: Pick<SyncOp, "kind" | "direction">): Side {
  const target: Side = op.direction === "toRight" ? "right" : "left";
  return op.kind === "delete" ? target : target === "right" ? "left" : "right";
}

/** Each side's tone for an op: by its category, else an overwrite has both sides and anything else only the side it works from. */
function opTones(op: SyncOp): Record<Side, Tone | null> {
  const at = actingSide(op);
  return syncTones(op.category, { left: op.overwrite || at === "left", right: op.overwrite || at === "right" });
}

const TONE_WORD: Record<Tone, string> = {
  same: "same",
  newer: "newer",
  different: "different",
  older: "older",
  orphan: "only on this side",
  unknown: "not compared",
  error: "error",
};

/**
 * A row's two sides before its path, left then right, coloured as Folder
 * Compare colours them: a spot for a file, a toned icon for a folder, a gap
 * where that side has nothing.
 */
function SideTones({ tones, isDir }: { tones: Record<Side, Tone | null>; isDir: boolean }) {
  const word = (t: Tone | null) => (t ? TONE_WORD[t] : "nothing there");
  return (
    <span className="sync-sides" title={`Left: ${word(tones.left)} · Right: ${word(tones.right)}`}>
      {(["left", "right"] as const).map((side) => {
        const t = tones[side];
        return (
          <span key={side} className="sync-side">
            {t && (isDir ? <FolderToneIcon look={{ tones: [t] }} /> : <ToneSpot tone={t} />)}
          </span>
        );
      })}
    </span>
  );
}

/** Per-kind totals for a set of operations (the confirmation dialog and status bar). */
function totals(ops: SyncOp[]) {
  const t = { copyRight: 0, copyLeft: 0, deleteRight: 0, deleteLeft: 0, mkdir: 0, overwrites: 0, bytes: 0 };
  for (const op of ops) {
    if (op.kind === "copy") {
      op.direction === "toRight" ? t.copyRight++ : t.copyLeft++;
      if (op.overwrite) t.overwrites++;
      t.bytes += op.size;
    } else if (op.kind === "delete") {
      op.direction === "toRight" ? t.deleteRight++ : t.deleteLeft++;
    } else t.mkdir++;
  }
  return t;
}

const lockers = (r: OpResult) => r.lockedBy.map((h) => `${h.name} (pid ${h.pid})`).join(", ");

type Row = { op: SyncOp } | { skip: SyncSkip };

/** Starts a run (execute or resume) under `runKey`, streaming progress. */
type Starter = (runKey: string, onProgress: (p: SyncProgress) => void) => Promise<SyncRun>;

/** What a run was given — kept so its report can retry failed or not-run operations. */
interface RunInput {
  left: string;
  right: string;
  mode: SyncMode;
  ops: SyncOp[];
  exec: ExecOptions;
}

interface RunOutcome extends RunInput {
  key: string;
  jobId: string | null;
  report: SyncReport;
  finishedMs: number;
}

interface RunningJob {
  key: string;
  exec: ExecOptions;
  progress: SyncProgress | null;
  cancelling: boolean;
  /** held by the user. */
  paused?: boolean;
}

export function FolderSync({ left, right, onPathsChange, settings, onSettingsChange, onOpenFiles, onAutomate }: Props) {
  const [cfg, setCfg] = useState<FolderSyncSettings>(() => restoreSettings(settings));
  const [excludeText, setExcludeText] = useState(cfg.exclude);
  const update = (patch: Partial<FolderSyncSettings>) => setCfg((c) => ({ ...c, ...patch }));
  const reportSettings = useRef(onSettingsChange);
  reportSettings.current = onSettingsChange;
  useEffect(() => void reportSettings.current?.(cfg), [cfg]);

  const [plan, setPlan] = useState<SyncPlan | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<{ text: string; items: string[] } | null>(null);
  const [loading, setLoading] = useState(false);
  const [excluded, setExcluded] = useState<Set<number>>(new Set());
  const [filter, setFilter] = useState<OpFilter>("all");
  const [sort, setSort] = useState<{ key: SortKey; desc: boolean }>({ key: "plan", desc: false });
  // multi-selection (by relPath) and the actions picked for rows.
  const [selection, setSelection] = useState<Set<string>>(new Set());
  const anchor = useRef<number | null>(null);
  const [overrides, setOverrides] = useState<Record<string, SyncAction>>({});
  const [matrixOpen, setMatrixOpen] = useState(false);
  const [confirming, setConfirming] = useState(false);
  const [optionsOpen, setOptionsOpen] = useState(false);
  const [historyOpen, setHistoryOpen] = useState(false);
  const [rollbackJob, setRollbackJob] = useState<string | null>(null);
  const [job, setJob] = useState<RunningJob | null>(null);
  const [outcome, setOutcome] = useState<RunOutcome | null>(null);
  const [interrupted, setInterrupted] = useState<JobSummary | null>(null);
  const [discarding, setDiscarding] = useState(false);
  const root = useRef<HTMLDivElement>(null);

  const remote = isRemote(left) || isRemote(right);
  const running = !!job;
  const planSeq = useRef(0);
  const interruptSeq = useRef(0);

  // Planning, execution and resume all use the same options so the post-run re-plan matches the preview.
  const compare = useMemo(() => {
    const p = COMPARE_PRESETS[cfg.compare];
    return toEngineOptions({
      ...DEFAULT_FOLDER_SETTINGS,
      compareSize: p.compareSize,
      compareTimestamp: p.compareTimestamp,
      content: p.content,
      exclude: cfg.exclude,
      includeHidden: cfg.includeHidden,
    });
  }, [cfg.compare, cfg.exclude, cfg.includeHidden]);

  // what planning needs besides the mode, compare options and conflicts.
  const baseRules = useMemo<PlanRules>(
    () => ({
      matrix: cfg.mode === "custom" ? cfg.matrix : undefined,
      skipEmptyFolders: cfg.skipEmptyFolders,
      scope: cfg.scope,
    }),
    [cfg.mode, cfg.matrix, cfg.skipEmptyFolders, cfg.scope],
  );
  const rules = useMemo<PlanRules>(() => ({ ...baseRules, overrides }), [baseRules, overrides]);

  /** Shows a plan that arrived outside `replan` (a run re-plans itself), superseding any in-flight one. */
  const showPlan = (next: SyncPlan) => {
    planSeq.current++;
    setPlan(next);
    setExcluded(new Set());
    setLoading(false);
  };

  const replan = useCallback(async () => {
    if (!left || !right) return;
    // Ignore answers to superseded requests (settings changed mid-plan).
    const seq = ++planSeq.current;
    setLoading(true);
    setError(null);
    // A rescan starts from the matrix again, like a refresh.
    setOverrides({});
    try {
      const next = await syncApi.plan(left, right, cfg.mode, compare, cfg.conflicts, baseRules);
      if (seq !== planSeq.current) return;
      setPlan(next);
      setExcluded(new Set());
    } catch (e) {
      if (seq !== planSeq.current) return;
      setError(errorMessage(e));
      setPlan(null);
    } finally {
      if (seq === planSeq.current) setLoading(false);
    }
  }, [left, right, cfg.mode, cfg.conflicts, compare, baseRules]);

  /**
   * sets (or, with null, resets) the action of `paths` and re-plans from the
   * comparison already made: only those rows change, unticked operations stay unticked.
   */
  const setRowAction = async (paths: string[], action: SyncAction | null) => {
    if (!left || !right || !paths.length) return;
    const next = { ...overrides };
    for (const p of paths) {
      if (action) next[p] = action;
      else delete next[p];
    }
    setOverrides(next);
    const before = plan;
    const off = new Set(before?.ops.filter((op) => excluded.has(op.id)).map(opKey));
    const seq = ++planSeq.current;
    setLoading(true);
    setError(null);
    try {
      const fresh = await syncApi.plan(left, right, cfg.mode, compare, cfg.conflicts, { ...baseRules, overrides: next }, true);
      if (seq !== planSeq.current) return;
      setPlan(fresh);
      setExcluded(new Set(fresh.ops.filter((op) => off.has(opKey(op))).map((op) => op.id)));
    } catch (e) {
      if (seq !== planSeq.current) return;
      setError(errorMessage(e));
    } finally {
      if (seq === planSeq.current) setLoading(false);
    }
  };

  useEffect(() => {
    replan();
  }, [replan]);

  const checkInterrupted = useCallback(() => {
    const seq = ++interruptSeq.current;
    if (!left || !right) return setInterrupted(null);
    syncApi.interrupted(left, right).then(
      (j) => seq === interruptSeq.current && setInterrupted(j),
      // The banner is a convenience; a broken journal shows up when a sync actually runs.
      () => seq === interruptSeq.current && setInterrupted(null),
    );
  }, [left, right]);

  useEffect(() => {
    if (!running) checkInterrupted();
  }, [checkInterrupted, running]);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      // Every open tab registers this; only the visible one reacts.
      if (!matches(e, "sync.refresh") || !root.current?.offsetParent || job) return;
      e.preventDefault();
      replan();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [replan, job]);

  const rows = useMemo<Row[]>(() => {
    if (!plan) return [];
    const ops = plan.ops.filter((op) => opMatches(op, filter));
    if (sort.key !== "plan") {
      const dir = sort.desc ? -1 : 1;
      ops.sort((a, b) =>
        sort.key === "size"
          ? (a.size - b.size) * dir
          : a.relPath.localeCompare(b.relPath, undefined, { sensitivity: "base" }) * dir,
      );
    } else if (sort.desc) ops.reverse();
    const out: Row[] = ops.map((op) => ({ op }));
    if (filter === "all" || filter === "skipped") out.push(...plan.skipped.map((skip) => ({ skip })));
    return out;
  }, [plan, filter, sort]);

  const chosen = useMemo(() => plan?.ops.filter((op) => !excluded.has(op.id)) ?? [], [plan, excluded]);
  const chosenTotals = useMemo(() => totals(chosen), [chosen]);

  const toggle = (id: number) =>
    setExcluded((prev) => {
      const next = new Set(prev);
      next.has(id) ? next.delete(id) : next.add(id);
      return next;
    });

  /** Select all / none applies to the operations the current filter shows. */
  const setVisible = (include: boolean) =>
    setExcluded((prev) => {
      const next = new Set(prev);
      for (const r of rows) if ("op" in r) include ? next.delete(r.op.id) : next.add(r.op.id);
      return next;
    });

  const sortBy = (key: SortKey) =>
    setSort((s) => (s.key === key ? (s.desc ? { key: "plan", desc: false } : { key, desc: true }) : { key, desc: false }));

  /** Runs a job behind the progress overlay, then shows its report and what remains. */
  const runJob = async (input: RunInput, start: Starter) => {
    const key = crypto.randomUUID();
    setJob({ key, exec: input.exec, progress: null, cancelling: false });
    setOutcome(null);
    setNotice(null);
    setError(null);
    let fresh: SyncPlan | null = null;
    try {
      const run = await start(key, (progress) => setJob((j) => (j && j.key === key ? { ...j, progress } : j)));
      setOutcome({ ...input, key, jobId: run.jobId, report: run.report, finishedMs: Date.now() });
      fresh = run.plan;
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setJob(null);
    }
    // Show what remains — ideally nothing. The run re-planned with the same settings unless that failed.
    if (fresh) showPlan(fresh);
    else await replan();
  };

  const executeOps = (l: string, r: string, mode: SyncMode, ops: SyncOp[]) => {
    const exec = cfg.exec;
    return runJob({ left: l, right: r, mode, ops, exec }, (runKey, onProgress) =>
      syncApi.execute({ left: l, right: r, mode, ops, exec, compare, conflicts: cfg.conflicts, rules, runKey, onProgress }),
    );
  };

  const execute = () => {
    setConfirming(false);
    if (plan && chosen.length > 0) executeOps(plan.leftRoot, plan.rightRoot, plan.mode, chosen);
  };

  const resume = async (jobId: string) => {
    setHistoryOpen(false);
    // The journal knows the job's operations and options; the report needs them for "Retry failed".
    const detail = await syncApi.jobDetail(jobId).catch(() => null);
    const input: RunInput = {
      left: detail?.summary.left ?? left,
      right: detail?.summary.right ?? right,
      mode: detail?.summary.mode ?? cfg.mode,
      ops: detail?.ops.map((o) => o.op) ?? [],
      exec: detail?.options ?? cfg.exec,
    };
    await runJob(input, (runKey, onProgress) =>
      syncApi.resume({ jobId, compare, conflicts: cfg.conflicts, rules, runKey, onProgress }),
    );
  };

  const discard = async (jobId: string) => {
    setDiscarding(true);
    try {
      const recoveries = await syncApi.discard(jobId);
      const manual = recoveries.filter((r): r is Extract<Recovery, { outcome: "manual" }> => r.outcome === "manual");
      setNotice(
        manual.length
          ? {
              text: `${count(manual.length, "item")} from the interrupted sync ${manual.length === 1 ? "needs" : "need"} a look:`,
              items: manual.map((m) => m.message),
            }
          : null,
      );
      setInterrupted(null);
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setDiscarding(false);
    }
    checkInterrupted();
    replan();
  };

  const rolledBack = (jobId: string) => {
    // The report describes changes that no longer exist.
    setOutcome((o) => (o?.jobId === jobId ? null : o));
    checkInterrupted();
    replan();
  };

  const cancel = () => {
    if (!job) return;
    setJob({ ...job, cancelling: true });
    syncApi.cancel(job.key).catch((e) => setError(errorMessage(e)));
  };

  // Pause / Resume a running sync.
  const togglePause = () => {
    if (!job) return;
    const paused = !job.paused;
    setJob({ ...job, paused });
    syncApi.pause(job.key, paused).catch((e) => setError(errorMessage(e)));
  };

  const openPair = (op: SyncOp) => {
    // aligned pairs have their own name on the left.
    if (onOpenFiles && plan && op.overwrite)
      onOpenFiles(joinPath(plan.leftRoot, op.leftRelPath ?? op.relPath), joinPath(plan.rightRoot, op.relPath));
  };

  // click selects, Ctrl/Cmd+click toggles, Shift+click extends.
  const clickRow = (i: number, e: { shiftKey: boolean; metaKey: boolean; ctrlKey: boolean }) => {
    const path = rowPath(rows[i]);
    if (e.shiftKey && anchor.current != null) {
      const [a, b] = [Math.min(anchor.current, i), Math.max(anchor.current, i)];
      setSelection(new Set(rows.slice(a, b + 1).map(rowPath)));
      return;
    }
    anchor.current = i;
    if (e.metaKey || e.ctrlKey)
      setSelection((prev) => {
        const next = new Set(prev);
        next.has(path) ? next.delete(path) : next.add(path);
        return next;
      });
    else setSelection(new Set([path]));
  };

  const selectedRows = useMemo(() => rows.filter((r) => selection.has(rowPath(r))), [rows, selection]);

  /** The row actions for `targets`, each applied where it can be. */
  const actionItems = (targets: Row[]): MenuItems => {
    const paths = targets.map(rowPath);
    const anySet = paths.some((p) => p in overrides);
    return [
      { heading: targets.length === 1 ? "Action" : `Action for ${targets.length} items` },
      ...ROW_ACTIONS.map((a) => {
        const fits = targets.filter((r) => {
          const sides = rowSides(r);
          return actionApplies(a.value, sides.left, sides.right);
        });
        return {
          label: a.label,
          disabled: running || fits.length === 0,
          danger: a.value === "deleteLeft" || a.value === "deleteRight",
          checked: targets.length > 0 && paths.every((p) => overrides[p] === a.value) ? true : undefined,
          onClick: () => void setRowAction(fits.map(rowPath), a.value),
        };
      }),
      SEPARATOR,
      { label: "Reset to plan", disabled: running || !anySet, onClick: () => void setRowAction(paths, null) },
    ];
  };

  const rowMenu = useContextMenu((targets: Row[]) => actionItems(targets));

  const onRowMenu = (i: number, e: ReactMouseEvent) => {
    e.preventDefault();
    const path = rowPath(rows[i]);
    // Right-clicking outside the selection selects just that row, like a file manager.
    const targets = selection.has(path) ? selectedRows : [rows[i]];
    if (!selection.has(path)) {
      anchor.current = i;
      setSelection(new Set([path]));
    }
    rowMenu.open(e, targets);
  };

  /** "Set by you" marker for overridden rows. */
  const overrideMark = (path: string) =>
    overrides[path] ? (
      <span className="sync-override" title={`Set by you: ${ROW_ACTIONS.find((a) => a.value === overrides[path])?.label}`}>
        ✎
      </span>
    ) : null;

  const renderRow = (i: number) => {
    const row = rows[i];
    if ("skip" in row) {
      const s = row.skip;
      const prompt = s.conflict && s.category != null;
      return (
        <div
          className={`sync-row skipped${s.conflict ? " conflict" : ""}${selection.has(s.relPath) ? " selected" : ""}`}
          title={s.reason}
          onClick={(e) => clickRow(i, e)}
          onContextMenu={(e) => onRowMenu(i, e)}
        >
          <div className="sync-cell check" />
          <div className="sync-cell op">
            <TriangleAlert size={13} />
            {prompt ? "Choose" : s.conflict ? "Conflict" : "Left alone"}
            {overrideMark(s.relPath)}
          </div>
          <div className="sync-cell path">
            <SideTones tones={syncTones(s.category, { left: s.onLeft, right: s.onRight })} isDir={s.isDir} />
            <span className="sync-path">{s.relPath}</span>
          </div>
          <div className="sync-cell num" />
          <div className="sync-cell reason">{s.reason}</div>
        </div>
      );
    }
    const op = row.op;
    const on = !excluded.has(op.id);
    const { icon: Icon, text } = opLabel(op);
    // The path takes the tone of the side the op works from (what's copied, or what's deleted).
    const tones = opTones(op);
    const pathTone = tones[actingSide(op)] ?? tones.left ?? tones.right;
    return (
      <div
        className={`sync-row k-${op.kind}${on ? "" : " off"}${selection.has(op.relPath) ? " selected" : ""}`}
        onClick={(e) => clickRow(i, e)}
        onContextMenu={(e) => onRowMenu(i, e)}
        onDoubleClick={() => openPair(op)}
        title={op.overwrite && onOpenFiles ? "Double-click to compare the two versions" : undefined}
      >
        <div className="sync-cell check">
          <input
            type="checkbox"
            checked={on}
            onChange={() => toggle(op.id)}
            onClick={(e) => e.stopPropagation()}
            aria-label={`Include ${op.relPath}`}
          />
        </div>
        <div className="sync-cell op">
          <Icon size={13} />
          {text}
          {overrideMark(op.relPath)}
        </div>
        <div className="sync-cell path">
          <SideTones tones={tones} isDir={op.isDir} />
          <span className="sync-path" style={pathTone ? { color: toneColor(pathTone) } : undefined}>
            {op.relPath}
          </span>
        </div>
        <div className="sync-cell num">{op.kind === "mkdir" ? "" : formatSize(op.size)}</div>
        <div className="sync-cell reason">{op.reason}</div>
      </div>
    );
  };

  const counts: Record<OpFilter, number> = useMemo(() => {
    const s = plan?.summary;
    return {
      all: (plan?.ops.length ?? 0) + (plan?.skipped.length ?? 0),
      copyRight: s?.copyToRight ?? 0,
      copyLeft: s?.copyToLeft ?? 0,
      delete: (s?.deleteLeft ?? 0) + (s?.deleteRight ?? 0),
      mkdir: (s?.mkdirLeft ?? 0) + (s?.mkdirRight ?? 0),
      skipped: s?.skipped ?? 0,
    };
  }, [plan]);

  const baseMode = MODES.find((m) => m.value === cfg.mode) ?? MODES[0];
  // a custom matrix deletes if any of its cells do.
  const mode = cfg.mode === "custom" ? { ...baseMode, deletes: matrixDeletes(cfg.matrix) } : baseMode;
  const ModeIcon = mode.icon;

  /** switching to Custom starts from the mode in use unless the matrix was already customised. */
  const pickMode = (next: SyncMode) => {
    if (next !== "custom") return update({ mode: next });
    const seed = cfg.mode !== "custom" && presetOf(cfg.matrix) ? PRESET_MATRICES[cfg.mode] : cfg.matrix;
    update({ mode: next, matrix: seed });
    setMatrixOpen(true);
  };
  const sortMark = (key: SortKey) =>
    sort.key === key ? sort.desc ? <ArrowDown size={11} /> : <ArrowUp size={11} /> : null;

  return (
    <div className="session folder-sync" ref={root}>
      <div className="toolbar">
        <label title={mode.hint}>
          <span className={`sync-mode-icon${mode.deletes ? " mirror" : ""}`}>
            <ModeIcon size={15} />
          </span>
          <select value={cfg.mode} onChange={(e) => pickMode(e.target.value as SyncMode)} disabled={running}>
            {MODES.map((m) => (
              <option key={m.value} value={m.value}>
                {m.label}
              </option>
            ))}
          </select>
        </label>
        {cfg.mode === "custom" && (
          <button onClick={() => setMatrixOpen(true)} disabled={running} title="Choose the action for each kind of difference">
            <Grid3x3 size={14} /> Matrix…
          </button>
        )}
        {(cfg.mode === "twoWay" || cfg.mode === "custom") && (
          <label title="How paths changed on both sides since the last sync are settled">
            Conflicts
            <select
              value={cfg.conflicts}
              onChange={(e) => update({ conflicts: e.target.value as ConflictPolicy })}
              disabled={running}
            >
              {CONFLICT_POLICIES.map((c) => (
                <option key={c.value} value={c.value}>
                  {c.label}
                </option>
              ))}
            </select>
          </label>
        )}
        <span className="sep" />
        <label>
          Compare
          <select
            value={cfg.compare}
            onChange={(e) => update({ compare: e.target.value as ComparePreset })}
            disabled={running}
          >
            {Object.entries(COMPARE_PRESETS).map(([key, p]) => (
              <option key={key} value={key}>
                {p.label}
              </option>
            ))}
          </select>
        </label>
        <label className="check">
          <input
            type="checkbox"
            checked={cfg.includeHidden}
            onChange={(e) => update({ includeHidden: e.target.checked })}
            disabled={running}
          />
          Hidden files
        </label>
        {/* "Sync empty folders" */}
        <label className="check" title="Create folders that hold no files (or only filtered-out ones) on the other side">
          <input
            type="checkbox"
            checked={!cfg.skipEmptyFolders}
            onChange={(e) => update({ skipEmptyFolders: !e.target.checked })}
            disabled={running}
          />
          Empty folders
        </label>
        <label className="grow">
          Exclude
          <input
            value={excludeText}
            onChange={(e) => setExcludeText(e.target.value)}
            onKeyDown={(e) => e.key === "Enter" && update({ exclude: excludeText })}
            onBlur={() => update({ exclude: excludeText })}
            placeholder="*.tmp, bin, obj"
            spellCheck={false}
            disabled={running}
          />
        </label>
        <button onClick={() => setOptionsOpen(true)} disabled={running} title={`Sync options: ${safetySummary(cfg.exec)}`}>
          <Settings2 size={14} /> Options
        </button>
        {/* automation:ui — save this pair and its settings as a scheduled / real-time task */}
        {onAutomate && (
          <button
            onClick={() =>
              onAutomate({
                name: `${basename(left)} → ${basename(right)}`,
                left,
                right,
                mode: cfg.mode,
                matrix: cfg.mode === "custom" ? cfg.matrix : PRESET_MATRICES[cfg.mode],
                skipEmptyFolders: cfg.skipEmptyFolders,
                conflicts: cfg.conflicts,
                compare,
                exec: cfg.exec,
              })
            }
            disabled={!left || !right}
            title="Run this sync automatically: on a schedule, when files change, or at startup"
          >
            <CalendarClock size={14} /> Automate…
          </button>
        )}
        {/* automation:ui end */}
        <button onClick={replan} disabled={!left || !right || loading || running} title={`Re-scan both folders (${shortcutLabel("sync.refresh")})`}>
          <RefreshCw size={14} /> Preview
        </button>
        <button
          className="primary"
          onClick={() => setConfirming(true)}
          disabled={!plan || chosen.length === 0 || loading || running}
          title={remote ? "Run the selected operations (deletions on a remote side are permanent)" : "Run the selected operations"}
        >
          <FolderSyncIcon size={14} /> Sync
        </button>
      </div>

      <PathBar left={left} right={right} directory onCompare={onPathsChange} />

      <div className="toolbar sync-filterbar">
        <div className="seg-control">
          {FILTERS.map((f) => (
            <button
              key={f.value}
              className={filter === f.value ? "active" : ""}
              onClick={() => setFilter(f.value)}
              disabled={f.value !== "all" && counts[f.value] === 0}
            >
              {f.label}
              <span className="sync-count">{counts[f.value]}</span>
            </button>
          ))}
        </div>
        <span className="sep" />
        <button onClick={() => setVisible(true)} disabled={!plan?.ops.length} title="Include every operation shown">
          <ListChecks size={14} /> Select all
        </button>
        <button onClick={() => setVisible(false)} disabled={!plan?.ops.length} title="Exclude every operation shown">
          <ListX size={14} /> Select none
        </button>
        {/* the selected rows' action (also on the right-click menu) */}
        <span className="sep" />
        <div className="seg-control sync-actions" role="group" aria-label="Action for the selected rows">
          {ROW_ACTIONS.map((a) => {
            const fits = selectedRows.filter((r) => {
              const sides = rowSides(r);
              return actionApplies(a.value, sides.left, sides.right);
            });
            return (
              <button
                key={a.value}
                className={selectedRows.length > 0 && selectedRows.every((r) => overrides[rowPath(r)] === a.value) ? "active" : ""}
                disabled={running || fits.length === 0}
                onClick={() => void setRowAction(fits.map(rowPath), a.value)}
                title={`${a.label} for the selected rows${fits.length < selectedRows.length ? ` (${fits.length} of ${selectedRows.length} can)` : ""}`}
              >
                {a.short}
              </button>
            );
          })}
          <button
            disabled={running || !selectedRows.some((r) => rowPath(r) in overrides)}
            onClick={() => void setRowAction(selectedRows.map(rowPath), null)}
            title="Back to what the sync mode plans for the selected rows"
          >
            <RotateCcw size={13} />
          </button>
        </div>
        <span className="sep" />
        <button onClick={() => setHistoryOpen(true)} disabled={!left || !right} title="Past syncs of these folders">
          <Clock size={14} /> History
        </button>
        <span className="spacer" />
        <span className="muted sync-hint">{mode.hint}</span>
      </div>

      {/* remote sides sync too, but without a Recycle Bin. */}
      {remote && (
        <div className="banner info sync-banner">
          A side is on a remote profile. Files deleted or overwritten there are removed permanently (remote folders have
          no Recycle Bin), so those changes can't be rolled back.
        </div>
      )}

      {interrupted && !running && (
        <div className="sync-interrupted">
          <TriangleAlert size={15} />
          <span>
            A sync of these folders was interrupted on {formatTime(interrupted.startedMs)} ({interrupted.doneOps} of{" "}
            {interrupted.totalOps} done).
          </span>
          <span className="spacer" />
          <button
            className="primary"
            onClick={() => resume(interrupted.id)}
            disabled={discarding}
            title="Repair half-finished operations and run the rest"
          >
            <Play size={14} /> Resume
          </button>
          {interrupted.doneOps > 0 && (
            <button onClick={() => setRollbackJob(interrupted.id)} disabled={discarding} title="Undo what it already did">
              <Undo2 size={14} /> Roll back
            </button>
          )}
          <button
            onClick={() => discard(interrupted.id)}
            disabled={discarding}
            title="Repair half-finished operations and close the job without running the rest"
          >
            Dismiss
          </button>
        </div>
      )}

      {notice && (
        <div className="banner info sync-banner sync-notice">
          <div className="sync-notice-head">
            <Info size={14} />
            <span>{notice.text}</span>
            <span className="spacer" />
            <button className="tab-close" onClick={() => setNotice(null)} aria-label="Dismiss">
              <X size={14} />
            </button>
          </div>
          {notice.items.length > 0 && (
            <ul>
              {notice.items.map((t, i) => (
                <li key={i}>{t}</li>
              ))}
            </ul>
          )}
        </div>
      )}

      {outcome && (
        <SyncReportPanel
          key={outcome.key}
          outcome={outcome}
          busy={running}
          onRun={(ops) => executeOps(outcome.left, outcome.right, outcome.mode, ops)}
          onRollback={() => outcome.jobId && setRollbackJob(outcome.jobId)}
          onDismiss={() => setOutcome(null)}
        />
      )}

      {plan && plan.warnings.length > 0 && (
        <div className="sync-warnings">
          {plan.warnings.map((w, i) => (
            <div key={i} className={`sync-warning w-${w.kind}`}>
              <TriangleAlert size={14} /> {w.message}
            </div>
          ))}
        </div>
      )}

      {/* "Synchronize selection" from Folder Compare */}
      {cfg.scope && (
        <div className="banner info sync-banner sync-subtle">
          <Info size={13} /> Only the {count(cfg.scope.length, "item")} selected in Folder Compare (and their contents) are
          synchronized.
          <span className="spacer" />
          <button onClick={() => update({ scope: undefined })} disabled={running}>
            Sync whole folders
          </button>
        </div>
      )}

      {plan?.mode === "custom" &&
        !plan.hasBaseline &&
        (cfg.matrix.leftDeleted !== cfg.matrix.rightOrphan || cfg.matrix.rightDeleted !== cfg.matrix.leftOrphan) && (
          <div className="banner info sync-banner sync-subtle">
            <Info size={13} /> No earlier sync of these folders yet, so “Deleted on left/right” can't be told apart from
            orphans: they follow the “Only on …” rows this time.
          </div>
        )}

      {plan?.mode === "twoWay" && !plan.hasBaseline && (
        <div className="banner info sync-banner sync-subtle">
          <Info size={13} /> First two-way sync of these folders: nothing will be deleted; history starts after this sync.
        </div>
      )}

      <div className="sync-header">
        <div />
        <div>Operation</div>
        <div className="sortable" onClick={() => sortBy("path")}>
          Path {sortMark("path")}
        </div>
        <div className="sortable num" onClick={() => sortBy("size")}>
          {sortMark("size")} Size
        </div>
        <div>Reason</div>
      </div>

      <div className="session-body">
        {loading && !job && <div className="overlay">Planning sync…</div>}
        {job && (
          <div className="overlay">
            <SyncProgressPanel job={job} onCancel={cancel} onPause={togglePause} />
          </div>
        )}
        {error && <div className="banner error">{error}</div>}
        {plan && rows.length === 0 && (
          <div className="empty-state">
            {plan.ops.length === 0 && plan.skipped.length === 0
              ? "Folders are in sync. Nothing to do."
              : "Nothing to show for this filter."}
          </div>
        )}
        {plan && rows.length > 0 && (
          <VirtualList className="sync-list" count={rows.length} rowHeight={24} renderRow={renderRow} />
        )}
      </div>

      <div className="statusbar">
        {plan && (
          <>
            <span className="op-copy-fg">{plan.summary.copyToRight} to copy →</span>
            <span className="op-copy-fg">{plan.summary.copyToLeft} to copy ←</span>
            <span className="op-delete-fg">{counts.delete} to delete</span>
            <span className="op-mkdir-fg">{counts.mkdir} new folders</span>
            {plan.summary.skipped > 0 && (
              <span className={plan.summary.conflicts ? "s-error-fg" : "muted"}>
                {plan.summary.skipped} skipped{plan.summary.conflicts ? ` (${plan.summary.conflicts} conflicts)` : ""}
              </span>
            )}
            <span className="spacer" />
            <span>
              {chosen.length} of {plan.ops.length} selected · {formatSize(chosenTotals.bytes)}
            </span>
            <span className="muted">{plan.elapsedMs} ms</span>
          </>
        )}
      </div>

      {confirming && plan && (
        <SyncConfirmDialog
          plan={plan}
          ops={chosen}
          mode={mode}
          exec={cfg.exec}
          onRun={execute}
          onClose={() => setConfirming(false)}
        />
      )}

      {matrixOpen && (
        <SyncMatrixDialog
          value={cfg.matrix}
          onApply={(matrix) => update({ mode: "custom", matrix })}
          onClose={() => setMatrixOpen(false)}
        />
      )}
      {rowMenu.element}

      {optionsOpen && (
        <SyncOptionsDialog value={cfg.exec} onApply={(exec) => update({ exec })} onClose={() => setOptionsOpen(false)} />
      )}

      {historyOpen && (
        <SyncHistoryDialog
          left={left}
          right={right}
          modeLabel={modeLabel}
          busy={running}
          onRollback={(id) => {
            setHistoryOpen(false);
            setRollbackJob(id);
          }}
          onResume={resume}
          onHistoryForgotten={replan}
          onClose={() => setHistoryOpen(false)}
        />
      )}

      {rollbackJob && (
        <RollbackDialog jobId={rollbackJob} onDone={rolledBack} onClose={() => setRollbackJob(null)} />
      )}
    </div>
  );
}

/** edits Custom mode's matrix. */
function SyncMatrixDialog({
  value,
  onApply,
  onClose,
}: {
  value: SyncMatrix;
  onApply: (m: SyncMatrix) => void;
  onClose: () => void;
}) {
  const [draft, setDraft] = useState(value);
  return (
    <Modal
      title="Custom sync"
      onClose={onClose}
      footer={
        <>
          <button onClick={onClose}>Cancel</button>
          <button
            className="primary"
            onClick={() => {
              onApply(draft);
              onClose();
            }}
          >
            Apply
          </button>
        </>
      }
    >
      <SyncMatrixEditor value={draft} onChange={setDraft} presetLabel={modeLabel} />
    </Modal>
  );
}

/** The confirmation before a run: what will happen, plan warnings, pre-flight checks and the safety settings. */
function SyncConfirmDialog({
  plan,
  ops,
  mode,
  exec,
  onRun,
  onClose,
}: {
  plan: SyncPlan;
  ops: SyncOp[];
  mode: ModeInfo;
  exec: ExecOptions;
  onRun: () => void;
  onClose: () => void;
}) {
  const [issues, setIssues] = useState<PreflightIssue[] | null>(null);
  const [ack, setAck] = useState(false);
  const t = useMemo(() => totals(ops), [ops]);

  useEffect(() => {
    let live = true;
    syncApi.preflight(plan.leftRoot, plan.rightRoot, ops, exec).then(
      (found) => live && setIssues(found),
      // A failed check shouldn't block the sync; the executor still guards every operation.
      (e) => live && setIssues([{ blocking: false, message: `The pre-flight checks couldn't run: ${errorMessage(e)}` }]),
    );
    return () => {
      live = false;
    };
  }, [plan, ops, exec]);

  const Icon = mode.icon;
  const deletions = t.deleteLeft + t.deleteRight;
  const blocked = issues?.some((i) => i.blocking) ?? false;
  const needsAck = plan.warnings.length > 0;
  const permanent = exec.disposal.kind === "permanent";
  const ready = issues !== null && !blocked && (!needsAck || ack);

  return (
    <Modal
      title="Synchronize folders"
      onClose={onClose}
      footer={
        <>
          <button onClick={onClose}>Cancel</button>
          <button
            className={deletions || needsAck || (permanent && t.overwrites) ? "danger" : "primary"}
            onClick={onRun}
            disabled={!ready}
            autoFocus
          >
            <FolderSyncIcon size={14} /> Sync {count(ops.length, "item")}
          </button>
        </>
      }
    >
      <p className="sync-confirm-mode">
        <Icon size={15} /> {mode.label}
      </p>
      <ul className="sync-confirm-list">
        {t.copyRight > 0 && (
          <li>
            <ArrowRight size={13} className="op-copy-fg" /> {count(t.copyRight, "file")} copied to the right
          </li>
        )}
        {t.copyLeft > 0 && (
          <li>
            <ArrowLeft size={13} className="op-copy-fg" /> {count(t.copyLeft, "file")} copied to the left
          </li>
        )}
        {t.mkdir > 0 && (
          <li>
            <FolderPlus size={13} className="op-mkdir-fg" /> {count(t.mkdir, "folder")} created
          </li>
        )}
        {t.deleteRight > 0 && (
          <li>
            <Trash size={13} className="op-delete-fg" /> {count(t.deleteRight, "item")} deleted on the right
          </li>
        )}
        {t.deleteLeft > 0 && (
          <li>
            <Trash size={13} className="op-delete-fg" /> {count(t.deleteLeft, "item")} deleted on the left
          </li>
        )}
      </ul>
      <p className="muted">
        {formatSize(t.bytes)} to copy.{t.overwrites > 0 && ` ${count(t.overwrites, "existing file")} replaced.`}
      </p>
      {(deletions > 0 || t.overwrites > 0) &&
        (permanent ? (
          <p className="sync-confirm-warn sync-danger">
            Deleted and replaced files are removed permanently. This can't be undone or rolled back.
          </p>
        ) : (
          <p className={deletions > 0 ? "sync-confirm-warn" : "muted"}>
            Deleted and replaced files are moved to{" "}
            {exec.disposal.kind === "trash" ? `the ${TRASH_NAME}` : "the versioning folder"}.
          </p>
        ))}

      {needsAck && (
        <div className="sync-confirm-warnings">
          {plan.warnings.map((w, i) => (
            <div key={i}>
              <TriangleAlert size={14} /> {w.message}
            </div>
          ))}
          <label className="field check sync-ack">
            <input type="checkbox" checked={ack} onChange={(e) => setAck(e.target.checked)} />I understand — sync anyway
          </label>
        </div>
      )}

      <div className="sync-preflight">
        {issues === null ? (
          <div className="muted">
            <LoaderCircle size={13} className="sync-spin" /> Checking…
          </div>
        ) : (
          issues.map((i, n) => (
            <div key={n} className={i.blocking ? "blocking" : "note"}>
              {i.blocking ? <CircleX size={13} /> : <TriangleAlert size={13} />} {i.message}
            </div>
          ))
        )}
      </div>

      <p className="sync-safety muted">
        <ShieldCheck size={13} /> {safetySummary(exec)}
      </p>
    </Modal>
  );
}

/** The progress card shown over the list while a job runs. */
function SyncProgressPanel({ job, onCancel, onPause }: { job: RunningJob; onCancel: () => void; onPause: () => void }) {
  const p = job.progress;
  const pct = p ? (p.bytesTotal > 0 ? (p.bytesDone / p.bytesTotal) * 100 : p.total > 0 ? (p.done / p.total) * 100 : 0) : 0;
  let phase = "Synchronizing…";
  if (job.cancelling) phase = "Cancelling…";
  else if (p?.phase === "paused" || job.paused) phase = p?.phase === "paused" ? "Paused" : "Pausing…";
  else if (p?.phase === "retrying")
    // `attempt` is the try that just failed; the wait is for the next one.
    phase = `Waiting to retry — ${lowerFirst(FAILURE_LABELS[p.waitingOn ?? "other"])} (attempt ${p.attempt + 1} of ${
      job.exec.retryAttempts + 1
    })…`;
  else if (p?.phase === "deferred") phase = "Retrying files that were in use…";

  return (
    <div className="sync-progress">
      <div className="sync-progress-title">
        <span className={p?.phase === "retrying" ? "sync-retrying" : undefined}>{phase}</span>
        <span className="muted">{p ? `${p.done} / ${p.total}` : ""}</span>
      </div>
      <div className="sync-bar">
        <div style={{ width: `${pct}%` }} />
      </div>
      <div className="sync-progress-file" title={p?.current ?? undefined}>
        {p?.current ?? "Starting…"}
      </div>
      <div className="sync-progress-foot">
        <span className="muted">{p ? `${formatSize(p.bytesDone)} of ${formatSize(p.bytesTotal)}` : ""}</span>
        {p && p.failed > 0 && <span className="s-error-fg">{p.failed} failed</span>}
        {p && p.deferred > 0 && <span className="sync-deferred">{p.deferred} deferred</span>}
        <span className="spacer" />
        <button onClick={onPause} disabled={job.cancelling} title="Hold the sync between files; resume continues where it stopped">
          {job.paused ? "Resume" : "Pause"}
        </button>
        <button onClick={onCancel} disabled={job.cancelling}>
          Cancel
        </button>
      </div>
    </div>
  );
}

/** Plain-text version of a report for the clipboard (support tickets, chat). */
function reportText(o: RunOutcome): string {
  const r = o.report;
  const lines = [
    `PCompare folder sync — ${modeLabel(o.mode)} — ${formatTime(o.finishedMs)}`,
    `Left:  ${o.left}`,
    `Right: ${o.right}`,
    `${r.stopped ? `Stopped: ${r.stopped}` : r.cancelled ? "Cancelled" : "Finished"} · ${r.succeeded} succeeded, ${
      r.failed
    } failed, ${r.notRun.length} not run · ${formatSize(r.bytesCopied)} copied · ${seconds(r.elapsedMs)}`,
  ];
  const failures = r.results.filter((x) => !x.ok);
  if (failures.length) {
    lines.push("", "Failures:");
    for (const f of failures) {
      const who = f.lockedBy.length ? ` — in use by ${lockers(f)}` : "";
      lines.push(`  [${FAILURE_LABELS[f.failure ?? "other"]}] ${f.relPath}: ${f.error ?? ""}${who}`);
    }
  }
  const notes = r.results.filter((x) => x.ok && x.error);
  if (notes.length) {
    lines.push("", "Notes:");
    for (const n of notes) lines.push(`  ${n.relPath}: ${n.error}`);
  }
  return lines.join("\n");
}

/** The report banner after a run: outcome, failures grouped by kind, caveats and follow-up actions. */
function SyncReportPanel({
  outcome,
  busy,
  onRun,
  onRollback,
  onDismiss,
}: {
  outcome: RunOutcome;
  busy: boolean;
  onRun: (ops: SyncOp[]) => void;
  onRollback: () => void;
  onDismiss: () => void;
}) {
  const { report } = outcome;
  const [open, setOpen] = useState(report.failed > 0);
  const [kind, setKind] = useState<FailureKind | null>(null);
  const [copied, setCopied] = useState<"ok" | "failed" | null>(null);

  const failures = useMemo(() => report.results.filter((r) => !r.ok), [report]);
  const notes = useMemo(() => report.results.filter((r) => r.ok && r.error), [report]);
  const groups = useMemo(() => {
    const m = new Map<FailureKind, number>();
    for (const f of failures) m.set(f.failure ?? "other", (m.get(f.failure ?? "other") ?? 0) + 1);
    return [...m].sort((a, b) => b[1] - a[1]);
  }, [failures]);

  // Follow-up runs reuse the exact operations (with their preview snapshots) the run was given.
  const byId = useMemo(() => new Map(outcome.ops.map((op) => [op.id, op])), [outcome.ops]);
  const lookup = (ids: number[]) => ids.flatMap((id) => byId.get(id) ?? []);
  const drifted = failures.filter((f) => f.failure === "changedSincePreview").length;
  const retryable = lookup(failures.filter((f) => f.failure !== "changedSincePreview").map((f) => f.id));
  const remaining = lookup(report.notRun);

  const halted = report.cancelled || !!report.stopped;
  const title = report.stopped ? "Sync stopped" : report.cancelled ? "Sync cancelled" : "Sync finished";
  const shown = kind ? failures.filter((f) => (f.failure ?? "other") === kind) : failures;

  const copy = () => {
    const flash = (state: "ok" | "failed") => {
      setCopied(state);
      setTimeout(() => setCopied(null), 1500);
    };
    navigator.clipboard.writeText(reportText(outcome)).then(() => flash("ok"), () => flash("failed"));
  };

  const retryTitle =
    drifted > 0
      ? `${count(drifted, "file")} changed since the preview and won't be retried — click Preview to plan ${drifted === 1 ? "it" : "them"} again`
      : "Run the failed operations again";

  return (
    <div className={`sync-report${report.failed ? " has-failures" : halted ? " halted" : ""}`}>
      <div className="sync-report-head">
        {report.failed ? <CircleX size={15} /> : halted ? <TriangleAlert size={15} /> : <CircleCheck size={15} />}
        <span className="sync-report-title">
          <b>{title}</b>
          {report.stopped && `: ${report.stopped}`} · {report.succeeded} succeeded
          {report.failed > 0 && `, ${report.failed} failed`}
          {report.notRun.length > 0 && `, ${report.notRun.length} not run`} · {formatSize(report.bytesCopied)} copied ·{" "}
          {seconds(report.elapsedMs)}
        </span>
        {(failures.length > 0 || notes.length > 0) && (
          <button className="link" onClick={() => setOpen((v) => !v)}>
            {open ? "Hide details" : "Show details"}
          </button>
        )}
        <span className="spacer" />
        {failures.length > 0 && (
          <button onClick={() => onRun(retryable)} disabled={busy || retryable.length === 0} title={retryTitle}>
            <RotateCcw size={14} /> Retry failed{retryable.length > 0 && ` (${retryable.length})`}
          </button>
        )}
        {remaining.length > 0 && (
          <button onClick={() => onRun(remaining)} disabled={busy} title="Run the operations that never started">
            <StepForward size={14} /> Run remaining ({remaining.length})
          </button>
        )}
        {outcome.jobId && report.succeeded > 0 && (
          <button onClick={onRollback} disabled={busy} title="Undo what this sync changed">
            <Undo2 size={14} /> Roll back
          </button>
        )}
        <button onClick={copy} title="Copy a plain-text report to the clipboard">
          <ClipboardCopy size={14} /> {copied === "ok" ? "Copied" : copied === "failed" ? "Couldn't copy" : "Copy report"}
        </button>
        <button className="tab-close" onClick={onDismiss} aria-label="Dismiss">
          <X size={14} />
        </button>
      </div>

      {open && (
        <div className="sync-report-body">
          {(groups.length > 1 || notes.length > 0) && (
            <div className="sync-chips">
              {groups.map(([k, n]) => (
                <button
                  key={k}
                  className={`sync-chip${kind === k ? " active" : ""}`}
                  onClick={() => setKind(kind === k ? null : k)}
                  title={kind === k ? "Show all failures" : "Show only these"}
                >
                  {FAILURE_LABELS[k]} <span className="sync-count">{n}</span>
                </button>
              ))}
              {notes.length > 0 && (
                <span className="sync-chip note">
                  Notes <span className="sync-count">{notes.length}</span>
                </span>
              )}
            </div>
          )}
          {groups.length === 1 && (
            <div className="sync-group-title">
              {FAILURE_LABELS[groups[0][0]]} <span className="sync-count">{groups[0][1]}</span>
            </div>
          )}
          {shown.length > 0 && (
            <ul className="sync-failures">
              {shown.map((f) => {
                const { text } = opLabel(f);
                return (
                  <li key={f.id}>
                    <div className="sync-failure-line">
                      <span className="sync-kind">{FAILURE_LABELS[f.failure ?? "other"]}</span>
                      <span className="path" title={f.relPath}>
                        {f.relPath}
                      </span>
                      <span className="muted">{text}</span>
                      {f.attempts > 1 && <span className="muted">· {f.attempts} attempts</span>}
                      {f.deferred && <span className="muted">· deferred</span>}
                    </div>
                    {f.error && <div className="err">{f.error}</div>}
                    {f.lockedBy.length > 0 && (
                      <div className="sync-lockers">
                        <Lock size={11} /> In use by: {lockers(f)}
                      </div>
                    )}
                  </li>
                );
              })}
            </ul>
          )}
          {notes.length > 0 && !kind && (
            <>
              <div className="sync-group-title">Notes</div>
              <ul className="sync-failures sync-notes">
                {notes.map((n) => (
                  <li key={n.id}>
                    <div className="sync-failure-line">
                      <Info size={12} />
                      <span className="path" title={n.relPath}>
                        {n.relPath}
                      </span>
                    </div>
                    <div className="note">{n.error}</div>
                  </li>
                ))}
              </ul>
            </>
          )}
        </div>
      )}
    </div>
  );
}

/** Confirms, runs and reports a rollback of one job. */
export function RollbackDialog({ // automation:ui — exported for Sync Automation's run history
  jobId,
  onDone,
  onClose,
}: {
  jobId: string;
  onDone: (jobId: string) => void;
  onClose: () => void;
}) {
  const [detail, setDetail] = useState<JobDetail | null>(null);
  const [results, setResults] = useState<RollbackResult[] | null>(null);
  const [rolling, setRolling] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let live = true;
    syncApi.jobDetail(jobId).then(
      (d) => live && setDetail(d),
      (e) => live && setError(errorMessage(e)),
    );
    return () => {
      live = false;
    };
  }, [jobId]);

  const run = async () => {
    setRolling(true);
    setError(null);
    try {
      setResults(await syncApi.rollback(jobId));
      onDone(jobId);
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setRolling(false);
    }
  };

  const done = detail?.ops.filter((o) => o.state === "done").length ?? 0;
  const disposal = detail?.options.disposal.kind;
  const failed = results?.filter((r) => !r.ok) ?? [];
  // Closing mid-rollback would hide its result.
  const close = rolling ? () => {} : onClose;

  return (
    <Modal
      title="Roll back sync"
      onClose={close}
      footer={
        results ? (
          <button className="primary" onClick={onClose} autoFocus>
            Close
          </button>
        ) : (
          <>
            <button onClick={onClose} disabled={rolling}>
              Cancel
            </button>
            <button className="danger" onClick={run} disabled={!detail || rolling}>
              <Undo2 size={14} /> {rolling ? "Rolling back…" : "Roll back"}
            </button>
          </>
        )
      }
    >
      {error && <div className="banner error sync-dialog-banner">{error}</div>}
      {!results && (
        <>
          {detail && (
            <p className="sync-confirm-mode">
              {modeLabel(detail.summary.mode)} · {formatTime(detail.summary.startedMs)} · {count(done, "completed operation")}
            </p>
          )}
          <p>
            Restores replaced and deleted files from the versioning folder, removes files and folders this sync created.
            Files changed since the sync are left alone.
          </p>
          {disposal === "trash" && (
            <p className="sync-confirm-warn">Items moved to the {TRASH_NAME} can't be restored automatically.</p>
          )}
          {disposal === "permanent" && (
            <p className="sync-confirm-warn sync-danger">This sync deleted files permanently; those can't be restored.</p>
          )}
        </>
      )}
      {results && (
        <>
          <p className={failed.length ? undefined : "banner ok"}>
            {count(results.length - failed.length, "item")} restored
            {failed.length > 0 && `, ${failed.length} couldn't be`}.
          </p>
          {failed.length > 0 && (
            <ul className="sync-failures sync-rollback-failures">
              {failed.map((r) => (
                <li key={r.id}>
                  <div className="sync-failure-line">
                    <span className="path" title={r.relPath}>
                      {r.relPath}
                    </span>
                  </div>
                  {r.message && <div className="err">{r.message}</div>}
                </li>
              ))}
            </ul>
          )}
        </>
      )}
    </Modal>
  );
}
