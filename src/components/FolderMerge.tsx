import {
  ChevronDown,
  ChevronRight,
  ChevronsDownUp,
  ChevronsUpDown,
  ChevronUp,
  CircleCheck,
  CircleX,
  Equal,
  EqualNot,
  GitMerge,
  Minus,
  Plus,
  RefreshCw,
  TriangleAlert,
  type LucideIcon,
} from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { errorMessage, isRemote, type EntryMeta } from "../lib/api";
import { COMPARE_PRESETS, DEFAULT_FOLDER_SETTINGS, toEngineOptions, type ComparePreset } from "../lib/folderSettings";
import { formatSize, formatTime, joinPath } from "../lib/format";
import { LAUNCH_READ_ONLY_HINT, type MergePaths, type MergeProps } from "../lib/sessionProps";
import {
  CONFLICT_CLASSES,
  syncApi,
  type FolderMergeEntry,
  type FolderMergeResult,
  type MergeAction,
  type MergeClass,
  type MergeDecision,
  type MergeReport,
} from "../lib/syncApi";
import { Modal } from "./Modal";
import { PathField } from "./PathField";
import { VirtualList, type VirtualListHandle } from "./VirtualList";
import "./FolderMerge.css";
import { FileIcon } from "./icons";

type Filter = "all" | "changes" | "conflicts";

/** What a saved folder-merge session remembers besides its paths. */
export interface FolderMergeSettings {
  filter: Filter;
  compare: ComparePreset;
  exclude: string;
  includeHidden: boolean;
  /** Three-way: merge text files changed on both sides line by line. */
  autoMergeText: boolean;
  /** Two-way: propose the newer of two differing files instead of a conflict. */
  takeNewer: boolean;
}

interface Props extends MergeProps<FolderMergeSettings> {
  /** Resolve one conflicting file in a Text Merge session; wired by the shell. */
  onOpenTextMerge?: (paths: MergePaths) => void;
  /** Two-way compare of one file pair; wired by the shell. */
  onOpenFiles?: (left: string, right: string) => void;
}

const DEFAULTS: FolderMergeSettings = {
  filter: "all",
  // Copies rarely keep timestamps across three trees, so compare bytes.
  compare: "binary",
  exclude: DEFAULT_FOLDER_SETTINGS.exclude,
  includeHidden: true,
  autoMergeText: true,
  takeNewer: true,
};

const CLASS_LABEL: Record<MergeClass, string> = {
  unchanged: "Unchanged",
  leftChanged: "Changed on left",
  rightChanged: "Changed on right",
  bothChangedSame: "Same change on both",
  conflict: "Conflict",
  leftAdded: "Added on left",
  rightAdded: "Added on right",
  bothAddedSame: "Added on both",
  bothAddedConflict: "Added differently",
  leftDeleted: "Deleted on left",
  rightDeleted: "Deleted on right",
  bothDeleted: "Deleted on both",
  deleteModifyConflict: "Deleted vs modified",
  typeConflict: "File vs folder",
  merged: "Changes on both",
  error: "Error",
  leftOnly: "Left only",
  rightOnly: "Right only",
  leftNewer: "Left newer",
  rightNewer: "Right newer",
  different: "Different",
  autoMerged: "Text merged",
};

/** Two-way merges have no base, so a few classes read differently. */
const TWO_WAY_LABEL: Partial<Record<MergeClass, string>> = {
  unchanged: "Same",
  merged: "Differences",
};

const ACTION_LABEL: Record<MergeAction, string> = {
  takeLeft: "Take left",
  takeRight: "Take right",
  takeBase: "Take base",
  delete: "Delete",
  skip: "Skip",
  mergeText: "Merged automatically",
};

const TRASH_NAME = navigator.userAgent.includes("Windows") ? "Recycle Bin" : "Trash";

const isConflict = (e: FolderMergeEntry) => CONFLICT_CLASSES.has(e.class);

function available(e: FolderMergeEntry): MergeAction[] {
  const out: MergeAction[] = [];
  if (e.class === "autoMerged") out.push("mergeText");
  if (e.left) out.push("takeLeft");
  if (e.right) out.push("takeRight");
  if (e.base) out.push("takeBase");
  out.push("delete", "skip");
  return out;
}

function matches(e: FolderMergeEntry, f: Filter): boolean {
  switch (f) {
    case "all":
      return true;
    case "changes":
      return e.class !== "unchanged";
    case "conflicts":
      return isConflict(e);
  }
}

/** Post-order: a folder stays visible if it or any descendant matches. */
function visibleSet(entries: FolderMergeEntry[], f: Filter): Set<string> {
  const out = new Set<string>();
  const visit = (e: FolderMergeEntry): boolean => {
    let childVisible = false;
    for (const c of e.children) childVisible = visit(c) || childVisible;
    const show = (e.isDir && childVisible) || (matches(e, f) && (!e.isDir || f === "all" || !e.children.length));
    if (show) out.add(e.relPath);
    return show;
  };
  entries.forEach(visit);
  return out;
}

function walkAll(entries: FolderMergeEntry[], fn: (e: FolderMergeEntry) => void) {
  for (const e of entries) {
    fn(e);
    walkAll(e.children, fn);
  }
}

function ancestors(rel: string): string[] {
  const parts = rel.split("/");
  return parts.slice(0, -1).map((_, i) => parts.slice(0, i + 1).join("/"));
}

/** How one side's file relates to the base version, for the side columns. */
type SideState = "same" | "changed" | "added" | "deleted" | "absent";

function sideState(e: FolderMergeEntry, side: "left" | "right", twoWay: boolean): SideState {
  if (twoWay) {
    // Without a base: an orphan counts as added, a differing file as changed.
    if (!e[side]) return "absent";
    if (!e[side === "left" ? "right" : "left"]) return "added";
    return e.isDir || e.class === "unchanged" ? "same" : "changed";
  }
  if (!e[side]) return e.base ? "deleted" : "absent";
  if (!e.base) return "added";
  if (e.isDir) return "same";
  const own = side === "left" ? "leftChanged" : "rightChanged";
  const changed: MergeClass[] = [
    own,
    "bothChangedSame",
    "conflict",
    "autoMerged",
    "deleteModifyConflict",
    "typeConflict",
  ];
  return changed.includes(e.class) ? "changed" : "same";
}

const STATE_ICON: Record<SideState, LucideIcon | null> = {
  same: Equal,
  changed: EqualNot,
  added: Plus,
  deleted: Minus,
  absent: null,
};

interface FlatRow {
  entry: FolderMergeEntry;
  depth: number;
}

export function FolderMerge({
  left,
  base,
  right,
  output,
  onPathsChange,
  settings,
  onSettingsChange,
  onOpenTextMerge,
  onOpenFiles,
  readOnlySides, // bc5:reports-script
}: Props) {
  const [cfg, setCfg] = useState<FolderMergeSettings>(() => ({
    ...DEFAULTS,
    ...settings,
    compare: settings?.compare && settings.compare in COMPARE_PRESETS ? settings.compare : DEFAULTS.compare,
  }));
  const [excludeText, setExcludeText] = useState(cfg.exclude);
  const update = (patch: Partial<FolderMergeSettings>) => setCfg((c) => ({ ...c, ...patch }));
  const reportSettings = useRef(onSettingsChange);
  reportSettings.current = onSettingsChange;
  useEffect(() => void reportSettings.current?.(cfg), [cfg]);

  // Path fields are edited locally and committed on Enter or browse.
  const [draft, setDraft] = useState<MergePaths>({ left, base, right, output });
  useEffect(() => setDraft({ left, base, right, output }), [left, base, right, output]);
  const commit = (patch: Partial<MergePaths> = {}) => {
    const next = { ...draft, ...patch };
    setDraft(next);
    onPathsChange(next);
  };

  const [result, setResult] = useState<FolderMergeResult | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const [expanded, setExpanded] = useState<Set<string>>(new Set());
  const [selected, setSelected] = useState<string | null>(null);
  const [overrides, setOverrides] = useState<Map<string, MergeAction>>(new Map());
  const [confirming, setConfirming] = useState(false);
  const [merging, setMerging] = useState(false);
  const [report, setReport] = useState<MergeReport | null>(null);
  const list = useRef<VirtualListHandle>(null);
  const root = useRef<HTMLDivElement>(null);
  const pendingScroll = useRef<string | null>(null);
  const seq = useRef(0);

  const run = useCallback(async () => {
    // The base is optional: without it the merge is two-way.
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
    const mine = ++seq.current;
    setLoading(true);
    setError(null);
    try {
      const res = await syncApi.mergeCompare(base, left, right, options, {
        autoMergeText: cfg.autoMergeText,
        takeNewer: cfg.takeNewer,
      });
      if (mine !== seq.current) return;
      setResult(res);
      setOverrides(new Map());
      // Open folders that hold conflicts so they are visible straight away.
      const open = new Set<string>();
      walkAll(res.entries, (e) => e.isDir && e.conflicts > 0 && open.add(e.relPath));
      setExpanded(open);
    } catch (e) {
      if (mine !== seq.current) return;
      setError(errorMessage(e));
      setResult(null);
    } finally {
      if (mine === seq.current) setLoading(false);
    }
  }, [left, base, right, cfg.compare, cfg.exclude, cfg.includeHidden, cfg.autoMergeText, cfg.takeNewer]);

  useEffect(() => {
    run();
  }, [run]);

  const twoWay = result ? result.twoWay : !base;
  const classLabel = (c: MergeClass) => (twoWay && TWO_WAY_LABEL[c]) || CLASS_LABEL[c];

  const effective = useCallback((e: FolderMergeEntry) => overrides.get(e.relPath) ?? e.action, [overrides]);

  const visible = useMemo(
    () => (result ? visibleSet(result.entries, cfg.filter) : new Set<string>()),
    [result, cfg.filter],
  );

  const flat = useMemo(() => {
    const out: FlatRow[] = [];
    const walk = (entries: FolderMergeEntry[], depth: number) => {
      for (const e of entries) {
        if (!visible.has(e.relPath)) continue;
        out.push({ entry: e, depth });
        if (e.isDir && expanded.has(e.relPath)) walk(e.children, depth + 1);
      }
    };
    if (result) walk(result.entries, 0);
    return out;
  }, [result, visible, expanded]);

  useEffect(() => {
    const target = pendingScroll.current;
    if (!target) return;
    const idx = flat.findIndex((r) => r.entry.relPath === target);
    if (idx >= 0) {
      list.current?.scrollToRow(idx);
      pendingScroll.current = null;
    }
  }, [flat]);

  /**
   * Items needing a decision, in tree order, for prev/next navigation. Folder
   * rows only count when unreadable; otherwise their conflicting children do.
   */
  const conflictOrder = useMemo(() => {
    const out: string[] = [];
    if (result)
      walkAll(result.entries, (e) => isConflict(e) && (!e.isDir || e.class === "error") && out.push(e.relPath));
    return out;
  }, [result]);
  const treeOrder = useMemo(() => {
    const m = new Map<string, number>();
    if (result) walkAll(result.entries, (e) => m.set(e.relPath, m.size));
    return m;
  }, [result]);

  const goConflict = useCallback(
    (dir: 1 | -1) => {
      if (!conflictOrder.length) return;
      const here = selected != null ? (treeOrder.get(selected) ?? -1) : -1;
      const order = conflictOrder;
      const target =
        dir > 0
          ? (order.find((p) => treeOrder.get(p)! > here) ?? order[0])
          : ([...order].reverse().find((p) => treeOrder.get(p)! < here) ?? order[order.length - 1]);
      setExpanded((prev) => new Set([...prev, ...ancestors(target)]));
      if (cfg.filter !== "all" && cfg.filter !== "conflicts") update({ filter: "conflicts" });
      setSelected(target);
      pendingScroll.current = target;
    },
    [conflictOrder, treeOrder, selected, cfg.filter],
  );

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      // Every open tab registers this; only the visible one reacts.
      if (!root.current?.offsetParent || merging) return;
      if (e.altKey && e.key === "ArrowDown") goConflict(1);
      else if (e.altKey && e.key === "ArrowUp") goConflict(-1);
      else if (e.key === "F5") run();
      else return;
      e.preventDefault();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [goConflict, run, merging]);

  const toggle = (rel: string) =>
    setExpanded((prev) => {
      const next = new Set(prev);
      next.has(rel) ? next.delete(rel) : next.add(rel);
      return next;
    });

  /**
   * Sets an action on one item. On a folder it applies to the whole subtree:
   * "Take left" makes the folder match the left side, so items the left
   * lacks are deleted.
   */
  const choose = (e: FolderMergeEntry, action: MergeAction) =>
    setOverrides((prev) => {
      const next = new Map(prev);
      const set = (x: FolderMergeEntry, a: MergeAction) => (a === x.action ? next.delete(x.relPath) : next.set(x.relPath, a));
      if (!e.isDir) {
        set(e, action);
        return next;
      }
      const side = action === "takeLeft" ? "left" : action === "takeRight" ? "right" : action === "takeBase" ? "base" : null;
      const pick = (x: FolderMergeEntry) => (side && !x[side] ? "delete" : action);
      walkAll([e], (x) => set(x, pick(x)));
      // Record the folder's own choice even when it equals its default, so the
      // row shows what was applied instead of "Per item".
      next.set(e.relPath, pick(e));
      return next;
    });

  const paths = (e: FolderMergeEntry) =>
    result && {
      left: joinPath(result.leftRoot, e.relPath),
      base: result.twoWay ? "" : joinPath(result.baseRoot, e.relPath),
      right: joinPath(result.rightRoot, e.relPath),
      output: output ? joinPath(output, e.relPath) : "",
    };

  /** Files worth a Text Merge session: conflicts, automatic merges to review, two-way differences. */
  const canTextMerge = (e: FolderMergeEntry) =>
    !!onOpenTextMerge &&
    !e.isDir &&
    !!e.left &&
    !!e.right &&
    ((!!e.base && (e.class === "conflict" || e.class === "autoMerged")) ||
      (twoWay && (e.class === "different" || e.class === "leftNewer" || e.class === "rightNewer")));

  const open = (e: FolderMergeEntry) => {
    if (e.isDir) return toggle(e.relPath);
    const p = paths(e);
    if (!p) return;
    if (canTextMerge(e)) onOpenTextMerge!(p);
    else if (onOpenFiles && e.left && e.right) onOpenFiles(p.left, p.right);
  };

  const renderSide = (e: FolderMergeEntry, side: "left" | "base" | "right") => {
    const meta: EntryMeta | null = e[side];
    const state: SideState = side === "base" ? (meta ? "same" : "absent") : sideState(e, side, twoWay);
    const Icon = side === "base" ? null : STATE_ICON[state];
    const tip = meta
      ? `${meta.isDir ? "Folder" : formatSize(meta.size)}${meta.modifiedMs ? ` · ${formatTime(meta.modifiedMs)}` : ""}`
      : state === "deleted"
        ? "Deleted"
        : "Not present";
    return (
      <div className={`fm-cell side st-${state}${meta ? "" : " absent"}`} title={tip}>
        {Icon && <Icon size={12} />}
        {meta && (meta.isDir ? <FileIcon name="" isDir size={14} /> : <span>{formatSize(meta.size)}</span>)}
      </div>
    );
  };

  const textMergeTip = (e: FolderMergeEntry) => {
    const t = e.textMerge;
    if (!t) return undefined;
    const changes = `${t.changes} change${t.changes === 1 ? "" : "s"} merged line by line`;
    return t.conflicts
      ? `${changes}; ${t.conflicts} conflicting region${t.conflicts === 1 ? "" : "s"} left. Open in Text Merge to resolve.`
      : `${changes} without conflicts.`;
  };

  const renderRow = (i: number) => {
    const { entry: e, depth } = flat[i];
    const action = effective(e);
    const overridden = overrides.has(e.relPath);
    const conflict = isConflict(e) || (e.isDir && e.conflicts > 0);
    const unresolved = !e.isDir && isConflict(e) && action === "skip";
    return (
      <div
        className={`fm-row c-${e.class}${conflict ? " conflict" : ""}${selected === e.relPath ? " selected" : ""}`}
        onClick={() => setSelected(e.relPath)}
        onDoubleClick={() => open(e)}
        title={e.error ?? undefined}
      >
        <div className="fm-cell name" style={{ paddingLeft: 6 + depth * 16 }}>
          {e.isDir ? (
            <span
              className="chevron"
              onClick={(ev) => {
                ev.stopPropagation();
                toggle(e.relPath);
              }}
            >
              {expanded.has(e.relPath) ? <ChevronDown size={13} /> : <ChevronRight size={13} />}
            </span>
          ) : (
            <span className="chevron" />
          )}
          <span className="ficon">
            <FileIcon name={e.name} isDir={e.isDir} open={expanded.has(e.relPath)} />
          </span>
          {e.name}
        </div>
        {renderSide(e, "left")}
        {!twoWay && renderSide(e, "base")}
        {renderSide(e, "right")}
        <div className="fm-cell status">
          <span className="fm-badge" title={textMergeTip(e)}>
            {conflict && <TriangleAlert size={11} />}
            {e.class === "autoMerged" && <GitMerge size={11} />}
            {classLabel(e.class)}
            {e.textMerge && e.textMerge.conflicts > 0 && <span className="fm-textconf">· {e.textMerge.conflicts} in text</span>}
          </span>
          {e.isDir && e.conflicts > 0 && <span className="fm-count">{e.conflicts}</span>}
          {canTextMerge(e) && (
            <button
              className="icon-btn fm-resolve"
              title={e.class === "autoMerged" ? "Review the automatic merge in Text Merge" : "Open in Text Merge"}
              onClick={(ev) => {
                ev.stopPropagation();
                open(e);
              }}
            >
              <GitMerge size={12} />
            </button>
          )}
        </div>
        <div className="fm-cell action">
          <select
            className={`${unresolved ? "unresolved" : ""}${overridden ? " overridden" : ""}`}
            value={e.isDir && !overridden ? "" : action}
            onChange={(ev) => ev.target.value && choose(e, ev.target.value as MergeAction)}
            onClick={(ev) => ev.stopPropagation()}
            title={e.isDir ? "Choosing an action applies it to everything in this folder" : undefined}
          >
            {/* Until a folder-wide choice is made, each item keeps its own proposed action. */}
            {e.isDir && !overridden && <option value="">Per item</option>}
            {available(e).map((a) => (
              <option key={a} value={a}>
                {ACTION_LABEL[a]}
              </option>
            ))}
          </select>
        </div>
      </div>
    );
  };

  /** Effective decisions for the whole tree, parents before children. */
  const decisions = useMemo<MergeDecision[]>(() => {
    const out: MergeDecision[] = [];
    if (result) walkAll(result.entries, (e) => out.push({ relPath: e.relPath, isDir: e.isDir, action: effective(e) }));
    return out;
  }, [result, effective]);

  const plan = useMemo(() => {
    const t: Record<MergeAction, number> & { unresolved: number } = {
      takeLeft: 0,
      takeRight: 0,
      takeBase: 0,
      delete: 0,
      skip: 0,
      mergeText: 0,
      unresolved: 0,
    };
    if (result)
      walkAll(result.entries, (e) => {
        if (e.isDir) return;
        const a = effective(e);
        t[a]++;
        if (a === "skip" && isConflict(e)) t.unresolved++;
      });
    return t;
  }, [result, effective]);

  const norm = (p: string) => p.replace(/[\\/]+$/, "").toLowerCase();
  const inPlace = output
    ? ([
        ["left", left],
        ["base", base],
        ["right", right],
      ] as const).find(([, p]) => p && norm(p) === norm(output))?.[0]
    : undefined;
  const remote = [left, base, right, output].some((p) => p && isRemote(p));
  // bc5:reports-script — `/lro`, `/rro`: never merge into a read-only side.
  const intoLocked = (inPlace === "left" || inPlace === "right") && !!readOnlySides?.[inPlace];

  const execute = async () => {
    setConfirming(false);
    setMerging(true);
    setError(null);
    try {
      setReport(await syncApi.mergeExecute(base, left, right, output, decisions));
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setMerging(false);
    }
    await run();
  };

  const counts = result?.counts ?? {};
  const n = (...classes: MergeClass[]) => classes.reduce((sum, c) => sum + (counts[c] ?? 0), 0);
  const ready = !!left && !!right;
  const mergeBlocked = !result || !output || remote || intoLocked || loading || merging;

  return (
    <div className={`session folder-merge${twoWay ? " fm-two" : ""}`} ref={root}>
      <div className="toolbar">
        <div className="seg-control">
          {(
            [
              ["all", "All"],
              ["changes", "Changes"],
              ["conflicts", "Conflicts"],
            ] as [Filter, string][]
          ).map(([f, label]) => (
            <button key={f} className={cfg.filter === f ? "active" : ""} onClick={() => update({ filter: f })}>
              {label}
            </button>
          ))}
        </div>
        <button onClick={() => goConflict(-1)} disabled={!conflictOrder.length} title="Previous conflict (Alt+Up)">
          <ChevronUp size={14} />
        </button>
        <button onClick={() => goConflict(1)} disabled={!conflictOrder.length} title="Next conflict (Alt+Down)">
          <ChevronDown size={14} />
        </button>
        <span className="sep" />
        <button
          onClick={() => {
            const all = new Set<string>();
            if (result) walkAll(result.entries, (e) => e.isDir && all.add(e.relPath));
            setExpanded(all);
          }}
          title="Expand all"
        >
          <ChevronsUpDown size={14} /> Expand
        </button>
        <button onClick={() => setExpanded(new Set())} title="Collapse all">
          <ChevronsDownUp size={14} /> Collapse
        </button>
        <span className="sep" />
        <label>
          Compare
          <select value={cfg.compare} onChange={(e) => update({ compare: e.target.value as ComparePreset })}>
            {Object.entries(COMPARE_PRESETS).map(([key, p]) => (
              <option key={key} value={key}>
                {p.label}
              </option>
            ))}
          </select>
        </label>
        {twoWay ? (
          <label className="check" title="Propose the newer file where the two sides differ; otherwise every difference is a conflict">
            <input type="checkbox" checked={cfg.takeNewer} onChange={(e) => update({ takeNewer: e.target.checked })} />
            Take newer
          </label>
        ) : (
          <label
            className="check"
            title="Merge text files changed on both sides line by line; files without conflicting lines are merged automatically"
          >
            <input
              type="checkbox"
              checked={cfg.autoMergeText}
              onChange={(e) => update({ autoMergeText: e.target.checked })}
            />
            Automatically merge text files
          </label>
        )}
        <label className="check">
          <input
            type="checkbox"
            checked={cfg.includeHidden}
            onChange={(e) => update({ includeHidden: e.target.checked })}
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
          />
        </label>
        <button onClick={run} disabled={!ready || loading || merging} title="Re-scan the folders (F5)">
          <RefreshCw size={14} /> Refresh
        </button>
        <button
          className="primary"
          onClick={() => setConfirming(true)}
          disabled={mergeBlocked}
          title={
            remote
              ? "Merging remote folders isn't supported yet"
              : intoLocked
                ? `The output is the ${inPlace} folder. ${LAUNCH_READ_ONLY_HINT}`
                : !output
                ? "Choose an output folder first"
                : "Write the merged result to the output folder"
          }
        >
          <GitMerge size={14} /> Merge
        </button>
      </div>

      <div className="fm-paths">
        {(
          [
            ["left", "Left"],
            ["base", "Base"],
            ["right", "Right"],
          ] as const
        ).map(([key, label]) => (
          <PathField
            key={key}
            label={label}
            value={draft[key]}
            onChange={(v) => setDraft((d) => ({ ...d, [key]: v }))}
            onSubmit={() => commit()}
            onPick={(p) => commit({ [key]: p })}
            directory
            placeholder={
              key === "base"
                ? "Common ancestor folder (optional: leave empty for a two-way merge)"
                : `${label} folder… or remote://profile/path`
            }
          />
        ))}
        <div className="fm-output">
          <PathField
            label="Output"
            value={draft.output}
            onChange={(v) => setDraft((d) => ({ ...d, output: v }))}
            onSubmit={() => commit()}
            onPick={(p) => commit({ output: p })}
            directory
            localOnly
            placeholder="Folder that receives the merged result"
          />
          <span className="muted">Merge into</span>
          <button
            onClick={() => commit({ output: draft.left })}
            disabled={!draft.left || !!readOnlySides?.left}
            title={readOnlySides?.left ? LAUNCH_READ_ONLY_HINT : "Write the result into the left folder"}
          >
            Left
          </button>
          <button
            onClick={() => commit({ output: draft.right })}
            disabled={!draft.right || !!readOnlySides?.right}
            title={readOnlySides?.right ? LAUNCH_READ_ONLY_HINT : "Write the result into the right folder"}
          >
            Right
          </button>
        </div>
      </div>

      <div className="fm-header">
        <div>Name</div>
        <div>Left</div>
        {!twoWay && <div>Base</div>}
        <div>Right</div>
        <div>Status</div>
        <div>Action</div>
      </div>

      <div className="session-body">
        {loading && <div className="overlay">{base ? "Comparing three folders…" : "Comparing folders…"}</div>}
        {merging && <div className="overlay">Merging…</div>}
        {error && <div className="banner error">{error}</div>}
        {!ready && !result && (
          <div className="empty-state">
            Choose the left and right folders. Add their common ancestor (base) for a 3-way merge,
            <br />
            or leave it empty to merge the two folders directly.
            <br />
            Pick an output folder, or merge into left or right, then press Merge.
          </div>
        )}
        {result && flat.length === 0 && (
          <div className="empty-state">
            {cfg.filter === "conflicts" ? "No conflicts." : cfg.filter === "changes" ? "No changes." : "All folders are empty."}
          </div>
        )}
        {result && <VirtualList ref={list} className="fm-list" count={flat.length} rowHeight={24} renderRow={renderRow} />}
      </div>

      <div className="statusbar">
        {result && (
          <>
            <span className={result.conflicts ? "s-different-fg" : "muted"}>
              {result.conflicts} conflict{result.conflicts === 1 ? "" : "s"}
            </span>
            {twoWay ? (
              <>
                <span className="fm-left-fg">{n("leftOnly")} left only</span>
                <span className="fm-right-fg">{n("rightOnly")} right only</span>
                <span className="muted">
                  {n("leftNewer")} left newer · {n("rightNewer")} right newer
                </span>
                <span className="muted">{n("unchanged")} same</span>
              </>
            ) : (
              <>
                <span className="fm-left-fg">{n("leftChanged", "leftAdded", "leftDeleted")} left changes</span>
                <span className="fm-right-fg">{n("rightChanged", "rightAdded", "rightDeleted")} right changes</span>
                {n("autoMerged") > 0 && <span className="fm-auto-fg">{n("autoMerged")} merged automatically</span>}
                <span className="muted">{n("bothChangedSame", "bothAddedSame", "bothDeleted")} identical changes</span>
                <span className="muted">{n("unchanged")} unchanged</span>
              </>
            )}
            <span className="spacer" />
            {plan.unresolved > 0 && <span className="s-error-fg">{plan.unresolved} unresolved</span>}
            <span className="muted">{result.elapsedMs} ms</span>
          </>
        )}
      </div>

      {confirming && result && (
        <Modal
          title="Merge folders"
          onClose={() => setConfirming(false)}
          footer={
            <>
              <button onClick={() => setConfirming(false)}>Cancel</button>
              <button className="primary" onClick={execute} autoFocus>
                <GitMerge size={14} /> Merge
              </button>
            </>
          }
        >
          <div className="save-summary fm-confirm-out">
            <div>Output: {output}</div>
          </div>
          {inPlace && (
            <p className="banner info">
              Merging in place into the {inPlace} folder: only files that change are written.
            </p>
          )}
          <ul className="fm-confirm-list">
            <li>{plan.takeLeft} files from the left</li>
            <li>{plan.takeRight} files from the right</li>
            {plan.mergeText > 0 && <li>{plan.mergeText} text files merged automatically</li>}
            {plan.takeBase > 0 && <li>{plan.takeBase} files restored from the base</li>}
            <li>{plan.delete} files removed (if present)</li>
            <li>{plan.skip} files left untouched</li>
          </ul>
          {plan.unresolved > 0 && (
            <p className="fm-warn">
              <TriangleAlert size={13} /> {plan.unresolved} conflict{plan.unresolved === 1 ? " is" : "s are"} unresolved
              and will be skipped.
            </p>
          )}
          {plan.delete > 0 && <p className="muted">Removed items are moved to the {TRASH_NAME}.</p>}
        </Modal>
      )}

      {report && (
        <Modal
          title={report.failed ? "Merge finished with errors" : "Merge complete"}
          onClose={() => setReport(null)}
          wide
          footer={<button onClick={() => setReport(null)}>Close</button>}
        >
          <p className={`fm-report-head${report.failed ? " bad" : ""}`}>
            {report.failed ? <CircleX size={15} /> : <CircleCheck size={15} />}
            {report.written} written · {report.created} folder{report.created === 1 ? "" : "s"} created · {report.deleted} removed · {report.unchanged}{" "}
            already up to date · {report.skipped} skipped
            {report.failed > 0 && ` · ${report.failed} failed`}
          </p>
          <p className="muted">
            {formatSize(report.bytesWritten)} written in {(report.elapsedMs / 1000).toFixed(1)} s
          </p>
          {report.results.some((r) => r.outcome === "failed" || r.outcome === "skipped") && (
            <ul className="fm-report-list">
              {report.results
                .filter((r) => r.outcome === "failed" || r.outcome === "skipped")
                .map((r) => (
                  <li key={r.relPath} className={`o-${r.outcome}`}>
                    <span className="what">{r.outcome === "failed" ? "Failed" : "Skipped"}</span>
                    <span className="path">{r.relPath}</span>
                    {r.error && <span className="err">{r.error}</span>}
                  </li>
                ))}
            </ul>
          )}
        </Modal>
      )}
    </div>
  );
}
