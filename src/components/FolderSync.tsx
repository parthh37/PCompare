import {
  ArrowDown,
  ArrowLeft,
  ArrowLeftRight,
  ArrowLeftToLine,
  ArrowRight,
  ArrowRightToLine,
  ArrowUp,
  CircleCheck,
  CircleX,
  FolderPlus,
  FolderSync as FolderSyncIcon,
  ListChecks,
  ListX,
  RefreshCw,
  Trash,
  TriangleAlert,
  X,
  type LucideIcon,
} from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { errorMessage, isRemote } from "../lib/api";
import { COMPARE_PRESETS, DEFAULT_FOLDER_SETTINGS, toEngineOptions, type ComparePreset } from "../lib/folderSettings";
import { formatSize, joinPath } from "../lib/format";
import type { CompareProps } from "../lib/sessionProps";
import {
  syncApi,
  type SyncMode,
  type SyncOp,
  type SyncPlan,
  type SyncProgress,
  type SyncReport,
  type SyncSkip,
} from "../lib/syncApi";
import { Modal } from "./Modal";
import { PathBar } from "./PathBar";
import { VirtualList } from "./VirtualList";
import "./FolderSync.css";
import { FileIcon } from "./icons";

/** "1 file", "3 files". */
const count = (n: number, noun: string) => `${n.toLocaleString()} ${noun}${n === 1 ? "" : "s"}`;

/** What a saved sync session remembers besides its paths. */
export interface FolderSyncSettings {
  mode: SyncMode;
  compare: ComparePreset;
  exclude: string;
  includeHidden: boolean;
}

interface Props extends CompareProps<FolderSyncSettings> {
  /** Opens a two-way compare of one file pair; wired by the shell when available. */
  onOpenFiles?: (left: string, right: string) => void;
}

const MODES: { value: SyncMode; label: string; icon: LucideIcon; hint: string }[] = [
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
    value: "mirrorLeftToRight",
    label: "Mirror left → right",
    icon: ArrowRightToLine,
    hint: "Make the right side identical to the left, deleting right-only items.",
  },
  {
    value: "mirrorRightToLeft",
    label: "Mirror right → left",
    icon: ArrowLeftToLine,
    hint: "Make the left side identical to the right, deleting left-only items.",
  },
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
};

const TRASH_NAME = navigator.userAgent.includes("Windows") ? "Recycle Bin" : "Trash";

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

function opLabel(op: SyncOp): { icon: LucideIcon; text: string } {
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

/** Per-kind totals for a set of operations (the confirmation dialog and status bar). */
function totals(ops: SyncOp[]) {
  const t = { copyRight: 0, copyLeft: 0, deleteRight: 0, deleteLeft: 0, mkdir: 0, bytes: 0 };
  for (const op of ops) {
    if (op.kind === "copy") {
      op.direction === "toRight" ? t.copyRight++ : t.copyLeft++;
      t.bytes += op.size;
    } else if (op.kind === "delete") {
      op.direction === "toRight" ? t.deleteRight++ : t.deleteLeft++;
    } else t.mkdir++;
  }
  return t;
}

type Row = { op: SyncOp } | { skip: SyncSkip };

export function FolderSync({ left, right, onPathsChange, settings, onSettingsChange, onOpenFiles }: Props) {
  const [cfg, setCfg] = useState<FolderSyncSettings>(() => ({
    ...DEFAULTS,
    ...settings,
    compare: settings?.compare && settings.compare in COMPARE_PRESETS ? settings.compare : DEFAULTS.compare,
  }));
  const [excludeText, setExcludeText] = useState(cfg.exclude);
  const update = (patch: Partial<FolderSyncSettings>) => setCfg((c) => ({ ...c, ...patch }));
  const reportSettings = useRef(onSettingsChange);
  reportSettings.current = onSettingsChange;
  useEffect(() => reportSettings.current?.(cfg), [cfg]);

  const [plan, setPlan] = useState<SyncPlan | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const [excluded, setExcluded] = useState<Set<number>>(new Set());
  const [filter, setFilter] = useState<OpFilter>("all");
  const [sort, setSort] = useState<{ key: SortKey; desc: boolean }>({ key: "plan", desc: false });
  const [selected, setSelected] = useState<string | null>(null);
  const [confirming, setConfirming] = useState(false);
  const [job, setJob] = useState<{ id: string; progress: SyncProgress | null; cancelling: boolean } | null>(null);
  const [report, setReport] = useState<SyncReport | null>(null);
  const [showFailures, setShowFailures] = useState(false);
  const root = useRef<HTMLDivElement>(null);

  const remote = isRemote(left) || isRemote(right);
  const planSeq = useRef(0);

  const replan = useCallback(async () => {
    if (!left || !right) return;
    const p = COMPARE_PRESETS[cfg.compare];
    const options = toEngineOptions({
      ...DEFAULT_FOLDER_SETTINGS,
      compareSize: p.compareSize,
      compareTimestamp: p.compareTimestamp,
      content: p.content,
      exclude: cfg.exclude,
      includeHidden: cfg.includeHidden,
    });
    // Ignore answers to superseded requests (settings changed mid-plan).
    const seq = ++planSeq.current;
    setLoading(true);
    setError(null);
    try {
      const next = await syncApi.plan(left, right, cfg.mode, options);
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
  }, [left, right, cfg.mode, cfg.compare, cfg.exclude, cfg.includeHidden]);

  useEffect(() => {
    replan();
  }, [replan]);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      // Every open tab registers this; only the visible one reacts.
      if (e.key !== "F5" || !root.current?.offsetParent || job) return;
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

  const execute = async () => {
    setConfirming(false);
    if (!plan || chosen.length === 0) return;
    const id = crypto.randomUUID();
    setJob({ id, progress: null, cancelling: false });
    setReport(null);
    setShowFailures(false);
    try {
      const r = await syncApi.execute(plan.leftRoot, plan.rightRoot, chosen, id, (progress) =>
        setJob((j) => (j && j.id === id ? { ...j, progress } : j)),
      );
      setReport(r);
      setShowFailures(r.failed > 0);
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setJob(null);
    }
    await replan(); // show what remains — ideally nothing
  };

  const cancel = () => {
    if (!job) return;
    setJob({ ...job, cancelling: true });
    syncApi.cancel(job.id).catch((e) => setError(errorMessage(e)));
  };

  const openPair = (op: SyncOp) => {
    if (onOpenFiles && plan && op.overwrite) onOpenFiles(joinPath(plan.leftRoot, op.relPath), joinPath(plan.rightRoot, op.relPath));
  };

  const renderRow = (i: number) => {
    const row = rows[i];
    if ("skip" in row) {
      const s = row.skip;
      return (
        <div className={`sync-row skipped${s.conflict ? " conflict" : ""}`} title={s.reason}>
          <div className="sync-cell check" />
          <div className="sync-cell op">
            <TriangleAlert size={13} />
            {s.conflict ? "Conflict" : "Skipped"}
          </div>
          <div className="sync-cell path">
            <span className="ficon">
              <FileIcon name={s.relPath} isDir={s.isDir} />
            </span>
            {s.relPath}
          </div>
          <div className="sync-cell num" />
          <div className="sync-cell reason">{s.reason}</div>
        </div>
      );
    }
    const op = row.op;
    const on = !excluded.has(op.id);
    const { icon: Icon, text } = opLabel(op);
    return (
      <div
        className={`sync-row k-${op.kind}${on ? "" : " off"}${selected === op.relPath ? " selected" : ""}`}
        onClick={() => setSelected(op.relPath)}
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
        </div>
        <div className="sync-cell path">
          <span className="ficon">
            <FileIcon name={op.relPath} isDir={op.isDir} />
          </span>
          {op.relPath}
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

  const mode = MODES.find((m) => m.value === cfg.mode) ?? MODES[0];
  const ModeIcon = mode.icon;
  const sortMark = (key: SortKey) =>
    sort.key === key ? sort.desc ? <ArrowDown size={11} /> : <ArrowUp size={11} /> : null;
  const deletions = chosenTotals.deleteLeft + chosenTotals.deleteRight;
  const failures = report?.results.filter((r) => !r.ok) ?? [];
  const pct = (p: SyncProgress) =>
    p.bytesTotal > 0 ? (p.bytesDone / p.bytesTotal) * 100 : p.total > 0 ? (p.done / p.total) * 100 : 0;

  return (
    <div className="session folder-sync" ref={root}>
      <div className="toolbar">
        <label title={mode.hint}>
          <span className={`sync-mode-icon${cfg.mode.startsWith("mirror") ? " mirror" : ""}`}>
            <ModeIcon size={15} />
          </span>
          <select value={cfg.mode} onChange={(e) => update({ mode: e.target.value as SyncMode })} disabled={!!job}>
            {MODES.map((m) => (
              <option key={m.value} value={m.value}>
                {m.label}
              </option>
            ))}
          </select>
        </label>
        <span className="sep" />
        <label>
          Compare
          <select
            value={cfg.compare}
            onChange={(e) => update({ compare: e.target.value as ComparePreset })}
            disabled={!!job}
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
            disabled={!!job}
          />
          Hidden files
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
            disabled={!!job}
          />
        </label>
        <button onClick={replan} disabled={!left || !right || loading || !!job} title="Re-scan both folders (F5)">
          <RefreshCw size={14} /> Preview
        </button>
        <button
          className="primary"
          onClick={() => setConfirming(true)}
          disabled={remote || !plan || chosen.length === 0 || loading || !!job}
          title={remote ? "Syncing to remote locations isn't supported yet" : "Run the selected operations"}
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
        <span className="spacer" />
        <span className="muted sync-hint">{mode.hint}</span>
      </div>

      {remote && (
        <div className="banner info sync-banner">
          Previewing a remote folder. Syncing to or from remote locations isn't supported yet.
        </div>
      )}

      {report && (
        <div className={`sync-report${report.failed ? " has-failures" : ""}`}>
          <div className="sync-report-head">
            {report.failed ? <CircleX size={15} /> : <CircleCheck size={15} />}
            <span>
              {report.cancelled ? "Sync cancelled" : "Sync finished"}: {report.succeeded} succeeded
              {report.failed > 0 && `, ${report.failed} failed`} · {formatSize(report.bytesCopied)} copied ·{" "}
              {(report.elapsedMs / 1000).toFixed(1)} s
            </span>
            {failures.length > 0 && (
              <button className="link" onClick={() => setShowFailures((v) => !v)}>
                {showFailures ? "Hide failures" : "Show failures"}
              </button>
            )}
            <span className="spacer" />
            <button className="tab-close" onClick={() => setReport(null)} aria-label="Dismiss">
              <X size={14} />
            </button>
          </div>
          {showFailures && (
            <ul className="sync-failures">
              {failures.map((f) => (
                <li key={f.id}>
                  <span className="path">{f.relPath}</span>
                  <span className="err">{f.error}</span>
                </li>
              ))}
            </ul>
          )}
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
            <div className="sync-progress">
              <div className="sync-progress-title">
                {job.cancelling ? "Cancelling…" : "Synchronizing…"}
                <span className="muted">
                  {job.progress ? `${job.progress.done} / ${job.progress.total}` : ""}
                </span>
              </div>
              <div className="sync-bar">
                <div style={{ width: `${job.progress ? pct(job.progress) : 0}%` }} />
              </div>
              <div className="sync-progress-file" title={job.progress?.current ?? undefined}>
                {job.progress?.current ?? "Starting…"}
              </div>
              <div className="sync-progress-foot">
                <span className="muted">
                  {job.progress
                    ? `${formatSize(job.progress.bytesDone)} of ${formatSize(job.progress.bytesTotal)}`
                    : ""}
                </span>
                <button onClick={cancel} disabled={job.cancelling}>
                  Cancel
                </button>
              </div>
            </div>
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
        <Modal
          title="Synchronize folders"
          onClose={() => setConfirming(false)}
          footer={
            <>
              <button onClick={() => setConfirming(false)}>Cancel</button>
              <button className={deletions ? "danger" : "primary"} onClick={execute} autoFocus>
                <FolderSyncIcon size={14} /> Sync {chosen.length} item{chosen.length === 1 ? "" : "s"}
              </button>
            </>
          }
        >
          <p className="sync-confirm-mode">
            <ModeIcon size={15} /> {mode.label}
          </p>
          <ul className="sync-confirm-list">
            {chosenTotals.copyRight > 0 && (
              <li>
                <ArrowRight size={13} className="op-copy-fg" /> {count(chosenTotals.copyRight, "file")} copied to the right
              </li>
            )}
            {chosenTotals.copyLeft > 0 && (
              <li>
                <ArrowLeft size={13} className="op-copy-fg" /> {count(chosenTotals.copyLeft, "file")} copied to the left
              </li>
            )}
            {chosenTotals.mkdir > 0 && (
              <li>
                <FolderPlus size={13} className="op-mkdir-fg" /> {count(chosenTotals.mkdir, "folder")} created
              </li>
            )}
            {chosenTotals.deleteRight > 0 && (
              <li>
                <Trash size={13} className="op-delete-fg" /> {count(chosenTotals.deleteRight, "item")} deleted on the right
              </li>
            )}
            {chosenTotals.deleteLeft > 0 && (
              <li>
                <Trash size={13} className="op-delete-fg" /> {count(chosenTotals.deleteLeft, "item")} deleted on the left
              </li>
            )}
          </ul>
          <p className="muted">{formatSize(chosenTotals.bytes)} to copy. Existing files are replaced.</p>
          {deletions > 0 && <p className="sync-confirm-warn">Deleted items are moved to the {TRASH_NAME}.</p>}
        </Modal>
      )}
    </div>
  );
}
