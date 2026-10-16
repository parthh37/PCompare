import {
  ArrowLeft,
  ArrowLeftRight,
  ArrowRight,
  Camera,
  ChevronDown,
  ChevronRight,
  CircleStop,
  Columns2,
  Equal,
  EqualNot,
  Eye,
  Feather,
  FileCheck,
  FileText,
  FoldVertical,
  FolderSync,
  FolderTree,
  Funnel,
  FunnelX,
  Link,
  ListChecks,
  PanelBottom,
  RefreshCw,
  Search,
  SlidersHorizontal,
  UnfoldVertical,
  X,
} from "lucide-react";
import {
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
  type PointerEvent as ReactPointerEvent,
} from "react";
import {
  api,
  ATTR,
  errorMessage,
  type ContentMethod,
  type ContentVerdict,
  type EntryMeta,
  type EntryStatus,
  type FolderCompareOptions,
  type FolderCompareResult,
  type FolderEntry,
  type FolderLevel,
  type ScanProgress,
  type Side,
  type TextDiffOptions,
} from "../lib/api";
import {
  activeAlignCount,
  activeFilterCount,
  COMPARE_PRESETS,
  FILTER_BOX_PRESETS,
  formatFilterBox,
  loadFolderSettings,
  parseFilterBox,
  presetOf,
  toEngineOptions,
  type ComparePreset,
  type CompareSettings,
  type FolderSettings,
} from "../lib/folderSettings";
import {
  allDirs,
  applyVerdicts,
  countsNewer,
  detailOf,
  DISPLAY_FILTERS,
  effectiveStatus,
  FOLDER_STRUCTURES,
  LiveTree,
  nameMatcher,
  replaceEntry,
  withoutFiltered,
  pairedFiles,
  selectMatching,
  tally,
  visibleSet,
  type SelectCriteria,
} from "../lib/folderTree";
import { basename, formatSize, formatTime, hex, joinPath } from "../lib/format";
import { writeClipboardText } from "../lib/clipboard";
import { useShell } from "../lib/shell";
import { autoRefreshTitle, useAutoRefresh } from "../lib/useAutoRefresh";
import { FolderSettingsDialog, type FolderSettingsPage } from "./FolderSettingsDialog";
import { FileIcon } from "./icons";
import { StackButton, StackMenuButton } from "./StackButton";
// external comparison tools.
import { openExternalIfAlways } from "../lib/convertApi";
import { externalToolItems } from "../lib/externalToolMenu";
import { SEPARATOR, type MenuItems } from "../lib/nativeMenu";
import { useContextMenu } from "./ContextMenu";
import { FileFormatsDialog } from "./FileFormatsDialog";
import { PathBar } from "./PathBar";
import { VirtualList } from "./VirtualList";
import { ReportDialog } from "./ReportDialog";
import { useFolderFileOps } from "./FolderFileOps";
import { ArchiveBanner } from "./ArchiveBanner";
import { SnapshotDialog } from "./SnapshotDialog";
import { useFolderReview } from "./FolderReview";
import { embedFocused } from "../lib/embeddedPane";
import { matches, shortcutLabel } from "../lib/keymap";
import { getPrefs, setPrefs, usePref } from "../lib/prefs";
import { sessionForFiles } from "../lib/sessions";
import { FolderSelectDialog, type SelectRequest } from "./FolderSelectDialog";
import { FolderSplitPane, type SplitPair } from "./FolderSplitPane";
import { countsSummary, DETAIL_LABEL, folderTones, LegendPopover, StatusGlyph, toneStyle } from "./FolderStatusLegend";
import "./FolderCompare.css";

export type { FolderSettings };

interface Props {
  left: string;
  right: string;
  onPathsChange: (left: string, right: string) => void;
  /** Opens a text compare; `rules` carries this session's text rules over. */
  onOpenFiles: (left: string, right: string, rules: TextDiffOptions) => void;
  onOpenHex: (left: string, right: string) => void;
  /** Opens a Folder Sync session on the same two folders (`scope` limits it to those items). */
  onOpenSync: (left: string, right: string, scope?: string[]) => void;
  /** opens one side's file in Text Edit. */
  onEditFile?: (path: string) => void;
  settings?: Record<string, unknown>;
  onSettingsChange?: (s: FolderSettings) => void;
  /** The split view's embedded file comparison holds unsaved edits. */
  onDirtyChange?: (dirty: boolean) => void;
}

const STATUS_LABEL: Record<EntryStatus, string> = {
  same: "Same",
  similar: "Similar — only unimportant differences",
  different: "Different",
  leftOnly: "Left only",
  rightOnly: "Right only",
  typeMismatch: "File on one side, folder on the other",
  error: "Error",
};

const METHOD_LABEL: Record<ContentMethod, string> = {
  off: "quick tests",
  binary: "binary",
  crc: "CRC-32",
  rules: "rules-based",
  version: "exe/dll version",
};

/** A line of the log panel. */
interface LogLine {
  at: number;
  text: string;
  error?: boolean;
}

/** `12.3 GB free on C:\` for a side's footer. */
const freeLabel = (f: { free: number; volume: string } | null) =>
  f ? `${formatSize(f.free)} free${f.volume ? ` on ${f.volume}` : ""}` : "";

/** The grid columns of one side's half of a row (the attributes column adds one). */
const sideColumns = (side: Side, attrs: boolean) => {
  const n = attrs ? 4 : 3;
  return side === "left" ? `1 / ${n + 1}` : `${n + 2} / -1`;
};

/** `3 files, 1 folder selected (12 KB)` for one side, or "" with nothing selected there. */
function selectionSummary(selected: FolderEntry[], selSide: Side | "both", side: Side): string {
  if (selSide !== "both" && selSide !== side) return "";
  const items = selected.map((e) => e[side]).filter((m): m is EntryMeta => !!m);
  if (!items.length) return "";
  const files = items.filter((m) => !m.isDir);
  const folders = items.length - files.length;
  const parts = [
    files.length && `${files.length} file${files.length === 1 ? "" : "s"}`,
    folders && `${folders} folder${folders === 1 ? "" : "s"}`,
  ].filter(Boolean);
  const bytes = files.reduce((sum, m) => sum + m.size, 0);
  return `${parts.join(", ")} selected${files.length ? ` (${formatSize(bytes)})` : ""}`;
}

/** The engine's message for a cancelled run (`folder::CANCELLED`). */
const CANCELLED = "Folder comparison cancelled";

/** `rwxr-xr-x` for Unix permission bits, `RHSA` letters for DOS attributes. */
function formatAttributes(m: EntryMeta | null): string {
  if (!m) return "";
  if (m.mode != null)
    return [6, 3, 0].map((shift) => ["r", "w", "x"].map((c, i) => ((m.mode! >> (shift + 2 - i)) & 1 ? c : "-")).join("")).join("");
  if (m.attributes == null) return "";
  const bits: [string, number][] = [["R", ATTR.readonly], ["H", ATTR.hidden], ["S", ATTR.system], ["A", ATTR.archive]];
  return bits.map(([c, b]) => (m.attributes! & b ? c : "-")).join("");
}

/** Mirrors `attributes_differ` in pcompare-core (archive doesn't count). */
function attributesDiffer(e: FolderEntry): boolean {
  const { left: l, right: r } = e;
  if (!l || !r || e.isDir) return false;
  const compared = ATTR.readonly | ATTR.hidden | ATTR.system;
  const dos = l.attributes != null && r.attributes != null && ((l.attributes ^ r.attributes) & compared) !== 0;
  return dos || (l.mode != null && r.mode != null && l.mode !== r.mode);
}

/** The entry's name on one side: alignment may pair differently named entries. */
const sideName = (e: FolderEntry, side: Side) =>
  side === "left" && e.leftRelPath ? e.leftRelPath.slice(e.leftRelPath.lastIndexOf("/") + 1) : e.name;

/** At most this many content re-checks run at once (each may read whole files). */
const RECHECK_CONCURRENCY = 4;

type Verdicts = Map<string, ContentVerdict & { method: ContentMethod }>;

/** "refresh" lists the folders afresh; "restage" re-runs only what changed options need. */
type RunMode = "refresh" | "quiet" | "restage";

interface FlatRow {
  entry: FolderEntry;
  depth: number;
}

interface MenuTarget {
  entry: FolderEntry;
  /** The half of the row that was right-clicked (or the only side the entry exists on). */
  side: Side;
}

const FILTERED_LABEL: Record<NonNullable<FolderEntry["filtered"]>, string> = {
  both: "left out by the filters (shown because filters are suppressed)",
  left: "the left side is left out by the filters",
  right: "the right side is left out by the filters",
};

function rowTitle(e: FolderEntry): string {
  if (e.pending) return "Not compared yet";
  const parts = e.ignored
    ? [`Ignored (marked as same; really ${STATUS_LABEL[e.status].toLowerCase()})`]
    : [e.isDir ? STATUS_LABEL[e.status] : DETAIL_LABEL[detailOf(e)]];
  if (e.filtered) parts.push(FILTERED_LABEL[e.filtered]);
  const inside = e.isDir && !e.ignored ? countsSummary(e) : "";
  if (inside) parts.push(`inside: ${inside}`);
  if (e.leftRelPath) parts.push(`aligned with ${e.leftRelPath} on the left`);
  if (attributesDiffer(e)) parts.push(`attributes ${formatAttributes(e.left)} ↔ ${formatAttributes(e.right)}`);
  for (const side of ["left", "right"] as const) {
    const target = e[side]?.linkTarget;
    if (target != null) parts.push(`${side} is a link → ${target || "(unreadable)"}`);
  }
  if (e.contentChecked && e.contentChecked !== "off") parts.push(`checked by ${METHOD_LABEL[e.contentChecked]}`);
  if (e.crc) parts.push(`CRC ${hex(e.crc[0], 8)} ↔ ${hex(e.crc[1], 8)}`);
  if (e.newer && !e.isDir && detailOf(e) !== "leftNewer" && detailOf(e) !== "rightNewer") parts.push(`${e.newer} is newer`);
  if (e.error) parts.push(e.error);
  return parts.join(" · ");
}

export function FolderCompare({
  left,
  right,
  onPathsChange,
  onOpenFiles,
  onOpenHex,
  onOpenSync,
  onEditFile,
  settings,
  onSettingsChange,
  onDirtyChange,
}: Props) {
  const [cfg, setCfg] = useState<FolderSettings>(() => loadFolderSettings(settings));
  const update = (patch: Partial<FolderSettings>) => setCfg((c) => ({ ...c, ...patch }));
  const reportSettings = useRef(onSettingsChange);
  reportSettings.current = onSettingsChange;
  useEffect(() => void reportSettings.current?.(cfg), [cfg]);

  /** The Filters box as typed; applied on Enter or leaving it. */
  const [filterText, setFilterText] = useState(() => formatFilterBox(cfg));
  const [nameQuery, setNameQuery] = useState("");
  /** The settings dialog's open tab, if it is open. */
  const [settingsPage, setSettingsPage] = useState<FolderSettingsPage | null>(null);
  const [log, setLog] = useState<LogLine[]>([]);
  const addLog = useCallback(
    (text: string, error = false) => setLog((l) => [...l.slice(-199), { at: Date.now(), text, error }]),
    [],
  );
  const logBox = useRef<HTMLDivElement>(null);
  useEffect(() => {
    if (logBox.current) logBox.current.scrollTop = logBox.current.scrollHeight;
  }, [log, cfg.showLog]);
  /** Free space under each side's folder (local folders only). */
  const [free, setFree] = useState<Record<Side, { free: number; volume: string } | null>>({ left: null, right: null });
  const [toolsDialog, setToolsDialog] = useState(false);
  const [snapshotting, setSnapshotting] = useState(false);
  const [reporting, setReporting] = useState(false);
  const [selecting, setSelecting] = useState(false);
  /** Where View > Legend is open, if it is. */
  const [legendAt, setLegendAt] = useState<{ x: number; y: number } | null>(null);
  // Split view (file comparison below the list): each tab toggles its own, and
  // the last choice and divider position are saved in prefs for new tabs.
  const [splitView, setSplitView] = useState(() => getPrefs().folderSplitView);
  const savedRatio = usePref("folderSplitRatio");
  const [splitRatio, setSplitRatio] = useState(savedRatio);
  useEffect(() => setSplitRatio(savedRatio), [savedRatio]);
  const splitBox = useRef<HTMLDivElement>(null);
  const splitDirty = useRef(false);
  const [result, setResult] = useState<FolderCompareResult | null>(null);
  const [verdicts, setVerdicts] = useState<Verdicts>(new Map());
  const [checking, setChecking] = useState<{ done: number; total: number } | null>(null);
  const [error, setError] = useState<string | null>(null);
  /** What the running compare is doing; the list stays usable throughout. */
  const [phase, setPhase] = useState<"scanning" | "contents" | "updating" | null>(null);
  const [progress, setProgress] = useState<ScanProgress | null>(null);
  /** The rows on screen come from the quick pass; contents weren't read yet. */
  const [quickOnly, setQuickOnly] = useState(false);
  /** Run key of the compare in flight, for cancelling it. */
  const runKey = useRef<string | null>(null);
  const hasResult = useRef(false);
  hasResult.current = !!result;
  const [expanded, setExpanded] = useState<Set<string>>(new Set());
  const root = useRef<HTMLDivElement>(null);
  const shell = useShell();

  // Only settings that affect the engine re-run it; view settings don't.
  const engineOptions = useMemo(() => toEngineOptions(cfg), [cfg]);
  const optionsKey = JSON.stringify(engineOptions);
  const requestSeq = useRef(0);
  const shownPaths = useRef("");
  /** Rows streamed for the folders on screen; refreshes refine it in place. */
  const live = useRef<LiveTree | null>(null);

  /** The listing the backend keeps for the folders on screen (`api.compareFolders`'s `scan`). */
  const scanId = useRef<string | null>(null);
  /** The folders (and archive expansion) that listing is of. */
  const scanKey = useRef("");
  /** Options of the latest pass on it, to tell what a restage will do. */
  const scanOptions = useRef<FolderCompareOptions | null>(null);
  /** Read by `run`, so toggling it doesn't re-run on its own. */
  const suppressFilters = useRef(cfg.suppressFilters);
  suppressFilters.current = cfg.suppressFilters;

  /**
   * Compares live: rows stream in folder by folder as they're listed, a quick
   * size/timestamp pass settles them, then the content pass (CRC, binary,
   * rules) refines them in place on the same listing. A "restage" (options
   * changed) re-runs filters, alignment and comparison on the listing the
   * backend kept, reading no folder again; "refresh" lists afresh. `quiet`
   * refreshes (auto-refresh) skip the quick pass; their rows replace settled
   * ones only when final, so nothing flickers.
   */
  const expandArchives = !!cfg.expandArchives;
  const run = useCallback(
    async (mode: RunMode = "refresh") => {
      // One folder is enough: it lists on its own until the other side is picked.
      if (!left && !right) return;
      const seq = ++requestSeq.current;
      const stale = () => seq !== requestSeq.current; // a newer compare superseded this one
      const options: FolderCompareOptions = JSON.parse(optionsKey);
      // New folders: don't leave the previous pair's rows up while scanning.
      const pathsKey = `${left}\n${right}`;
      if (shownPaths.current !== pathsKey || !live.current) {
        shownPaths.current = pathsKey;
        live.current = new LiveTree();
        setResult(null);
        addLog(left && right ? `Load comparison: ${left} ↔ ${right}` : `Load folder: ${left || right}`);
      } else if (mode === "refresh") addLog("Full refresh");
      const started = performance.now();
      const tree = live.current;
      const listing = `${pathsKey}\n${expandArchives}`;
      const quiet = mode === "quiet";
      const restage = mode === "restage" && !!scanId.current && scanKey.current === listing;
      if (!restage) {
        if (scanId.current) api.forgetFolderScan(scanId.current).catch(() => {});
        scanId.current = `folder-scan-${crypto.randomUUID()}`;
        scanKey.current = listing;
      }
      const scan = scanId.current!;
      const before = scanOptions.current;
      let shown = false;
      let painted = false;
      let streamed = 0;
      let timer: number | undefined;
      const paint = () => {
        timer = undefined;
        if (stale()) return;
        painted = true;
        const entries = tree.tree();
        setResult({ leftRoot: left, rightRoot: right, entries, stats: tally(entries), elapsedMs: 0 });
      };
      const onLevels = (levels: FolderLevel[]) => {
        if (stale()) return;
        tree.apply(levels);
        for (const l of levels) streamed += l.entries.length;
        // Each repaint walks the whole tree, so big trees repaint less often.
        timer ??= window.setTimeout(paint, Math.min(1000, 150 + streamed / 1000));
      };
      const finish = (res: FolderCompareResult, quick = false) => {
        window.clearTimeout(timer);
        timer = undefined;
        tree.endPass();
        shown = true;
        setQuickOnly(quick);
        setResult({ ...res, entries: tree.tree() });
        setVerdicts(new Map());
        setError(null);
      };
      // A superseded compare would only burn CPU: stop it. Each pass gets its
      // own run key so Esc / Cancel stops whichever one is in flight. Every
      // pass but a fresh listing's first works on the listing kept so far.
      const pass = (opts: FolderCompareOptions, again: boolean) => {
        if (runKey.current) api.cancelCompareFolders(runKey.current).catch(() => {});
        const key = `folder-${crypto.randomUUID()}`;
        runKey.current = key;
        if (!quiet) setProgress(null);
        scanOptions.current = opts;
        tree.beginPass();
        return api.compareFolders(left, right, opts, key, (p) => !stale() && setProgress(p), expandArchives, onLevels, {
          id: scan,
          restage: again,
          showFiltered: suppressFilters.current,
        });
      };
      const twoPass = !quiet && !restage && options.content !== "off";
      // Only reading contents anew takes long enough to name.
      const reads = options.content !== "off" && before?.content !== options.content;
      try {
        if (twoPass) {
          setPhase("scanning");
          const quick = await pass({ ...options, content: "off" }, false);
          if (stale()) return;
          finish(quick, true);
        }
        if (!quiet) setPhase(twoPass || (restage && reads) ? "contents" : restage ? "updating" : "scanning");
        const res = await pass(options, restage || twoPass);
        if (stale()) return;
        finish(res);
        const secs = ((performance.now() - started) / 1000).toFixed(2);
        addLog(
          quiet
            ? `Auto-refresh: compared again in ${secs} seconds`
            : twoPass
              ? `Background content comparison completed in ${secs} seconds`
              : restage
                ? `Comparison updated in ${secs} seconds`
                : `Comparison completed in ${secs} seconds`,
        );
      } catch (e) {
        window.clearTimeout(timer);
        if (stale()) return;
        const msg = errorMessage(e);
        if (msg === CANCELLED) {
          // Whatever streamed in before the stop stays up.
          if (timer !== undefined) paint();
          const kept = shown || painted || hasResult.current;
          setError(`Comparison cancelled${kept ? " — showing what was compared so far." : "."}`);
          addLog("Comparison cancelled", true);
        } else {
          setError(msg);
          addLog(msg, true);
          if (!quiet && !shown) setResult(null);
        }
      } finally {
        if (!stale()) {
          setPhase(null);
          runKey.current = null;
        }
      }
    },
    [left, right, optionsKey, expandArchives, addLog],
  );
  const runRef = useRef(run);
  runRef.current = run;
  const resultRef = useRef(result);
  resultRef.current = result;

  /**
   * After a file operation: compares just the rows it changed (tree paths)
   * and swaps them into the tree (a partial refresh). Rescans
   * instead while a compare runs or when a row's folder isn't in the tree.
   * `live` refreshes run while the operation is still going: they never
   * rescan (its final refresh will) and aren't logged.
   */
  const refreshRows = async (rels: string[], liveRefresh = false) => {
    const scan = scanId.current;
    const tree = live.current;
    const rescan = () => (liveRefresh ? undefined : void run("quiet"));
    if (runKey.current || !resultRef.current || !scan || !tree) return rescan();
    const seq = requestSeq.current;
    const busy = () => seq !== requestSeq.current || !!runKey.current;
    let rows: (FolderEntry | null)[];
    try {
      const options = scanOptions.current ?? (JSON.parse(optionsKey) as FolderCompareOptions);
      const ref = { id: scan, showFiltered: suppressFilters.current };
      rows = await api.compareFolderEntries(left, right, options, rels, ref, expandArchives);
    } catch {
      if (!busy()) rescan();
      return;
    }
    // A compare started meanwhile shows the change already.
    const prev = resultRef.current;
    if (busy() || !prev) return;
    let entries = prev.entries;
    for (const [i, rel] of rels.entries()) {
      const next = replaceEntry(entries, rel, rows[i] ?? null);
      if (!next) return rescan();
      entries = next;
    }
    for (const rel of rels) tree.patch(entries, rel);
    if (!liveRefresh) {
      addLog(`Refreshed ${rels.length === 1 ? rels[0] : `${rels.length} items`} after a file operation`);
    }
    const next = { ...prev, entries, stats: { ...prev.stats, filtered: 0, ...tally(entries) } };
    resultRef.current = next;
    setResult(next);
    // Content checks of the old rows no longer hold.
    const stale = (key: string) => rels.some((rel) => key === rel || key.startsWith(`${rel}/`));
    setVerdicts((v) => ([...v.keys()].some(stale) ? new Map([...v].filter(([key]) => !stale(key))) : v));
  };

  const cancel = () => {
    if (runKey.current) api.cancelCompareFolders(runKey.current).catch(() => {});
  };

  // Options changed (or first show): re-run what they need on the kept listing.
  useEffect(() => {
    run("restage");
  }, [run]);

  // Suppressing filters walks the folders they left out (listing only those);
  // turning it back off just hides the rows again.
  useEffect(() => {
    if (cfg.suppressFilters && hasResult.current) void runRef.current("restage");
  }, [cfg.suppressFilters]);

  useEffect(
    () => () => {
      if (scanId.current) void api.forgetFolderScan(scanId.current).catch(() => {});
    },
    [],
  );

  const watchState = useAutoRefresh({
    enabled: cfg.autoRefresh,
    paths: [left, right],
    exclude: engineOptions.exclude.concat(engineOptions.filters.excludeFolders),
    onChange: () => run("quiet"),
  });

  // Free space per side, read again once each compare (or file operation) settles.
  useEffect(() => {
    if (phase) return;
    let live = true;
    const read = (p: string) => (p ? api.diskFree(p).catch(() => null) : Promise.resolve(null));
    void Promise.all([read(left), read(right)]).then(([l, r]) => live && setFree({ left: l, right: r }));
    return () => {
      live = false;
    };
  }, [left, right, phase, result]);

  const ignoredSet = useMemo(() => new Set(cfg.ignored), [cfg.ignored]);
  const entries = useMemo(() => {
    if (!result) return [];
    const all = applyVerdicts(result.entries, verdicts, ignoredSet);
    // Everything downstream (selection, file operations, reports) sees what's shown.
    return cfg.suppressFilters ? all : withoutFiltered(all);
  }, [result, verdicts, ignoredSet, cfg.suppressFilters]);
  /** Files the filters leave out, shown or not. */
  const filteredCount = result?.stats.filtered ?? 0;
  const stats = useMemo(
    () => (verdicts.size || ignoredSet.size ? tally(entries) : result?.stats),
    [entries, verdicts, ignoredSet, result],
  );
  const matcher = useMemo(() => nameMatcher(nameQuery), [nameQuery]);
  /** Only one folder chosen so far: show its listing rather than a sea of orphans. */
  const oneSided = !!result && (!result.leftRoot || !result.rightRoot);
  // Everything is an "orphan" until the other side exists, so display filters wait for it.
  const visible = useMemo(
    () =>
      visibleSet(entries, oneSided ? "all" : cfg.filter, matcher, {
        structure: cfg.structure,
        ignoreMinor: cfg.ignoreMinor,
      }),
    [entries, oneSided, cfg.filter, matcher, cfg.structure, cfg.ignoreMinor],
  );

  const flat = useMemo(() => {
    const out: FlatRow[] = [];
    const walk = (list: FolderEntry[], depth: number) => {
      for (const e of list) {
        if (!visible.has(e.relPath)) continue;
        if (cfg.flatten) {
          // Ignore folder structure: files only, at one level, named by relative path.
          if (e.isDir) walk(e.children, 0);
          else out.push({ entry: e, depth: 0 });
          continue;
        }
        out.push({ entry: e, depth });
        if (e.isDir && expanded.has(e.relPath)) walk(e.children, depth + 1);
      }
    };
    walk(entries, 0);
    return out;
  }, [entries, visible, expanded, cfg.flatten]);

  const toggle = (rel: string) =>
    setExpanded((prev) => {
      const next = new Set(prev);
      next.has(rel) ? next.delete(rel) : next.add(rel);
      return next;
    });

  const sidePath = (e: FolderEntry, side: Side) =>
    result
      ? side === "left"
        ? joinPath(result.leftRoot, e.leftRelPath ?? e.relPath)
        : joinPath(result.rightRoot, e.relPath)
      : "";

  const paths = (e: FolderEntry): [string, string] | null =>
    result && e.left && e.right ? [sidePath(e, "left"), sidePath(e, "right")] : null;

  /** Marks (or unmarks) entries "ignore"; a folder's mark covers its contents. */
  const setIgnored = (list: FolderEntry[], on: boolean) => {
    let ignored = cfg.ignored;
    for (const e of list) {
      const inside = (p: string) => p === e.relPath || p.startsWith(`${e.relPath}/`);
      const covers = (p: string) => e.relPath === p || e.relPath.startsWith(`${p}/`);
      ignored = on
        ? [...ignored.filter((p) => !inside(p)), e.relPath]
        : // Unignoring also lifts a mark on a folder above it.
          ignored.filter((p) => !covers(p));
    }
    update({ ignored });
  };

  const open = (e: FolderEntry) => {
    if (e.isDir && !cfg.flatten) return toggle(e.relPath);
    if (!result || e.left?.isDir || e.right?.isDir) return;
    // An orphan (or any file while only one folder is loaded) opens with the missing side blank.
    const p: [string, string] = [e.left ? sidePath(e, "left") : "", e.right ? sidePath(e, "right") : ""];
    if (!p[0] && !p[1]) return;
    // pairs matching an "always use" external tool open there instead.
    (p[0] && p[1] ? openExternalIfAlways(...p) : Promise.resolve(false))
      .then((launched) => !launched && onOpenFiles(...p, engineOptions.rules))
      .catch((err) => setError(errorMessage(err)));
  };

  /** Re-checks the given files' contents and folds the verdicts into the tree. */
  const recheck = async (files: FolderEntry[], method: ContentMethod) => {
    if (!result || !files.length) return;
    const queue = [...files];
    let done = 0;
    setChecking({ done, total: files.length });
    const failures: string[] = [];
    const worker = async () => {
      for (let e = queue.shift(); e; e = queue.shift()) {
        const [lp, rp] = paths(e)!;
        try {
          const v = await api.compareFileContents(lp, e.left!.size, rp, e.right!.size, method, engineOptions.rules);
          setVerdicts((prev) => new Map(prev).set(e!.relPath, { ...v, method }));
        } catch (err) {
          failures.push(`${e.relPath}: ${errorMessage(err)}`);
        }
        setChecking({ done: ++done, total: files.length });
      }
    };
    await Promise.all(Array.from({ length: RECHECK_CONCURRENCY }, worker));
    setChecking(null);
    if (failures.length) setError(failures.slice(0, 3).join("\n") + (failures.length > 3 ? "\n…" : ""));
  };

  // bookmarks with comments, Statistics, Save comparison archive.
  const review = useFolderReview({
    bookmarks: cfg.bookmarks ?? [],
    onBookmarks: (bookmarks) => update({ bookmarks }),
    result,
    entries,
    stats,
    visible,
    flat,
    root,
    rowHeight: 24,
    expand: (rels) => setExpanded((prev) => new Set([...prev, ...rels])),
    showAll: () => update({ filter: "all" }),
    engineOptions,
    settings: { filter: cfg.filter, flatten: cfg.flatten, showAttributes: cfg.showAttributes },
  });

  // multi-selection, file operations and the Operations panel.
  const fops = useFolderFileOps({
    root,
    result,
    entries,
    flat,
    left,
    right,
    // Enter opens the selection: one folder toggles, file pairs open (a handful at most).
    onOpen: (list) => (list.length === 1 ? open(list[0]) : list.filter((e) => !e.isDir).slice(0, 10).forEach(open)),
    onRefresh: () => run("quiet"),
    onRefreshRows: (rels, liveRefresh) => refreshRows(rels, liveRefresh),
    onError: setError,
    compareOptions: engineOptions,
  });

  // Split view: the focused row's file pair (orphans show their one file; folders nothing).
  const focusRel = fops.sel.focus && fops.sel.rows.has(fops.sel.focus) ? fops.sel.focus : (fops.selected[0]?.relPath ?? null);
  const focusEntry = useMemo(
    () => (focusRel == null ? undefined : flat.find((r) => r.entry.relPath === focusRel)?.entry),
    [flat, focusRel],
  );
  const pairFile = focusEntry && !focusEntry.isDir && !focusEntry.left?.isDir && !focusEntry.right?.isDir;
  const pairLeft = pairFile && focusEntry.left ? sidePath(focusEntry, "left") : "";
  const pairRight = pairFile && focusEntry.right ? sidePath(focusEntry, "right") : "";
  const pairSize = pairFile ? (focusEntry.left ?? focusEntry.right)!.size : 0;
  const splitPair = useMemo<SplitPair | null>(
    () => (pairFile && focusRel ? { id: focusRel, left: pairLeft, right: pairRight, size: pairSize } : null),
    [pairFile, focusRel, pairLeft, pairRight, pairSize],
  );
  const splitHint = !focusEntry
    ? "Select a file to compare it here."
    : "Folders aren’t compared here — select a file inside.";

  /** Opens the split view's pair in its own tab; an orphan text file opens alone in Text Edit. */
  const openPairInTab = (l: string, r: string) => {
    const one = l && r ? "" : l || r;
    if (one && onEditFile && sessionForFiles(one, one) === "text") onEditFile(one);
    else onOpenFiles(l, r, engineOptions.rules);
  };

  const toggleSplit = () => {
    const discard = "The file comparison below has unsaved edits. Close it and discard them?";
    if (splitView && splitDirty.current && !window.confirm(discard)) return;
    setSplitView(!splitView);
    setPrefs({ folderSplitView: !splitView });
  };
  const toggleSplitRef = useRef(toggleSplit);
  toggleSplitRef.current = toggleSplit;

  /** Drags the divider between the folder list and the file comparison. */
  const dragDivider = (ev: ReactPointerEvent<HTMLDivElement>) => {
    const box = splitBox.current?.getBoundingClientRect();
    if (!box) return;
    ev.preventDefault();
    const bar = ev.currentTarget;
    bar.setPointerCapture(ev.pointerId);
    let ratio = splitRatio;
    const move = (m: PointerEvent) => {
      ratio = Math.min(0.85, Math.max(0.15, (m.clientY - box.top) / box.height));
      setSplitRatio(ratio);
    };
    const up = () => {
      bar.removeEventListener("pointermove", move);
      bar.removeEventListener("pointerup", up);
      setPrefs({ folderSplitRatio: ratio });
    };
    bar.addEventListener("pointermove", move);
    bar.addEventListener("pointerup", up);
  };

  const rowsMatching = (criteria: SelectCriteria, reachCollapsed: boolean) =>
    selectMatching(entries, visible, criteria, { flatten: cfg.flatten, expanded, reachCollapsed });
  const selectWhere = ({ criteria, side, add, reachCollapsed }: SelectRequest) => {
    const { rows, expand } = rowsMatching(criteria, reachCollapsed);
    if (expand.length) setExpanded((prev) => new Set([...prev, ...expand]));
    fops.selectRows(rows, side, add);
  };

  // Row context menu; built after the right-click's selection change renders.
  // View menu: display toggles and the status legend, opened under its button.
  const viewMenu = useContextMenu((at: { x: number; y: number }): MenuItems => [
    { label: "Ignore folder structure", checked: cfg.flatten, onClick: () => update({ flatten: !cfg.flatten }) },
    { label: "Show attributes", checked: !!cfg.showAttributes, onClick: () => update({ showAttributes: !cfg.showAttributes }) },
    { label: "Split view", checked: splitView, shortcut: shortcutLabel("folder.splitView"), onClick: toggleSplit },
    { label: "Log", checked: cfg.showLog, onClick: () => update({ showLog: !cfg.showLog }) },
    SEPARATOR,
    { label: "Legend…", onClick: () => setLegendAt(at) },
  ]);
  // Diffs ▾: every display filter that narrows to differences, orphans or newer files.
  const filterMenu = useContextMenu((): MenuItems =>
    DISPLAY_FILTERS.filter((f) => f.value !== "all" && f.value !== "same").map((f) => ({
      label: f.label,
      checked: cfg.filter === f.value,
      onClick: () => update({ filter: f.value }),
    })),
  );
  // Structure ▾: when folders show, and Ignore folder structure.
  const structureMenu = useContextMenu((): MenuItems => [
    ...FOLDER_STRUCTURES.map((f) => ({
      label: f.label,
      checked: !cfg.flatten && cfg.structure === f.value,
      onClick: () => update({ structure: f.value, flatten: false }),
    })),
    SEPARATOR,
    { label: "Ignore folder structure", checked: cfg.flatten, onClick: () => update({ flatten: !cfg.flatten }) },
  ]);
  // Copy with both sides selected: which way.
  const copyMenu = useContextMenu((): MenuItems => [
    { label: "Copy to Right", disabled: !right, onClick: () => fops.copyOther.run("right") },
    { label: "Copy to Left", disabled: !left, onClick: () => fops.copyOther.run("left") },
  ]);
  /** Selects every file shown, on both sides ("Select All Files"). */
  const selectAllFiles = () =>
    fops.selectRows(
      flat.filter((r) => !r.entry.isDir).map((r) => r.entry.relPath),
      "both",
      false,
    );
  const applyFilterBox = () => {
    const parsed = parseFilterBox(filterText);
    setFilterText(formatFilterBox(parsed));
    if (parsed.includeFiles !== cfg.includeFiles || parsed.exclude !== cfg.exclude) update(parsed);
  };

  const menu = useContextMenu(({ entry, side }: MenuTarget): Promise<MenuItems> | MenuItems => {
    // context-menu actions cover the whole selection.
    const list = fops.menuEntries(entry);
    const files = list.flatMap((e) => (e.isDir ? pairedFiles(e.children) : paths(e) ? [e] : []));
    const pair = !entry.isDir ? paths(entry) : null;
    const path = sidePath(entry, side);
    const items = (tools: MenuItems): MenuItems => [
      ...(pair
        ? [
            { label: "Open in Text Compare", onClick: () => onOpenFiles(...pair, engineOptions.rules) },
            { label: "Open in Hex Compare", onClick: () => onOpenHex(...pair) },
            ...tools, // third-party comparison tools
            SEPARATOR,
          ]
        : []),
      // edit the clicked side's file on its own
      onEditFile && !entry.isDir && entry[side] && { label: `Edit ${side} file`, onClick: () => onEditFile(path) },
      SEPARATOR,
      ...review.menuItems(entry),
      SEPARATOR,
      ...fops.menuItems(),
      // Folder Sync limited to the selection
      {
        label: list.length > 1 ? `Synchronize ${list.length} items…` : "Synchronize selection…",
        disabled: !left || !right,
        onClick: () => onOpenSync(left, right, list.map((e) => e.relPath)),
      },
      SEPARATOR,
      {
        label: `Compare contents${list.length > 1 || entry.isDir ? ` (${files.length} files)` : ""}`,
        disabled: !files.length || !!checking,
        submenu: (["binary", "crc", "rules", "version"] as ContentMethod[]).map((m) => ({
          label: m === "binary" ? "Binary" : m === "crc" ? "CRC-32" : m === "rules" ? "Rules-based" : "Exe/dll version",
          onClick: () => recheck(files, m),
        })),
      },
      SEPARATOR,
      entry.ignored
        ? { label: "Unignore", onClick: () => setIgnored(list, false) }
        : { label: "Ignore (mark as same)", onClick: () => setIgnored(list, true) },
      SEPARATOR,
      { label: "Select Left Side for Compare", onClick: () => shell.selectLeft(path) },
      shell.leftSelection &&
        shell.leftSelection !== path && {
          label: `Compare to “${basename(shell.leftSelection)}”`,
          onClick: () => shell.compareToLeft(path),
        },
      SEPARATOR,
      // Through the clipboard plugin: a native menu click is no user gesture for the WebView's clipboard API.
      { label: "Copy relative path", onClick: () => void writeClipboardText(entry.relPath).catch(() => {}) },
    ];
    return pair
      ? externalToolItems(pair[0], pair[1], setError, () => setToolsDialog(true)).then(items)
      : items([]);
  });

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      // Every open tab stays mounted; only the visible one handles keys.
      if (!root.current?.offsetParent) return;
      // The split view's viewer has focus: its keys (F5, Esc…) are its own.
      if (embedFocused()) return;
      if (!document.querySelector(".modal-backdrop")) {
        if (matches(e, "folder.selectWhere")) {
          e.preventDefault();
          if (hasResult.current) setSelecting(true);
          return;
        }
        if (matches(e, "folder.splitView")) {
          e.preventDefault();
          toggleSplitRef.current();
          return;
        }
      }
      if (e.key === "Escape" && runKey.current) {
        e.preventDefault();
        api.cancelCompareFolders(runKey.current).catch(() => {});
        return;
      }
      if (e.key !== "F5") return;
      e.preventDefault();
      run();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [run]);

  const renderSide = (e: FolderEntry, side: "left" | "right", depth: number) => {
    const meta = e[side];
    // The newer of two differing files in red and the older in grey;
    // a folder is bold on the side all newer files below it are on.
    const newer = e.isDir
      ? !e.ignored && countsNewer(e.counts) === side
        ? " newer"
        : ""
      : e.newer === side
        ? " newer"
        : e.newer
          ? " older"
          : "";
    const tones = e.isDir ? toneStyle(folderTones(e)) : undefined;
    // What the filters leave out is drawn in teal; a half-filtered pair only on that side.
    const filtered = e.filtered === "both" || e.filtered === side ? " filtered" : "";
    const showChevron = e.isDir && meta && !cfg.flatten;
    return (
      <>
        <div className={`fcell name${meta ? "" : " absent"}${newer}${filtered}`} style={{ paddingLeft: 6 + depth * 16 }}>
          {showChevron ? (
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
          {meta && (
            <>
              <span className="ficon">
                <FileIcon name={e.name} isDir={e.isDir} open={e.isDir && expanded.has(e.relPath) && !cfg.flatten} />
              </span>
              <span style={tones}>
                {cfg.flatten ? (side === "left" ? (e.leftRelPath ?? e.relPath) : e.relPath) : sideName(e, side)}
              </span>
              {meta.linkTarget != null && (
                <span className="fc-link" title={`Link → ${meta.linkTarget || "(unreadable)"}`}>
                  <Link size={11} />
                </span>
              )}
              {review.mark(e, side)}
            </>
          )}
        </div>
        <div className={`fcell num${newer}${filtered}`}>{meta && !meta.isDir ? formatSize(meta.size) : ""}</div>
        <div className={`fcell time${newer}${filtered}`}>{meta ? formatTime(meta.modifiedMs) : ""}</div>
        {cfg.showAttributes && (
          <div className={`fcell fc-attr${attributesDiffer(e) ? " differs" : ""}`}>{formatAttributes(meta)}</div>
        )}
      </>
    );
  };

  const renderRow = (i: number) => {
    const { entry: e, depth } = flat[i];
    const checked = e.contentChecked && e.contentChecked !== "off";
    return (
      <div
        className={`frow s-${e.pending ? "pending" : effectiveStatus(e)} d-${detailOf(e)}${e.ignored ? " ignored" : ""}${fops.rowClass(e.relPath)}${review.rowClass(e.relPath)}`}
        onClick={(ev) => fops.onRowClick(ev, e)}
        onDoubleClick={() => open(e)}
        onContextMenu={(ev) => {
          ev.preventDefault();
          const box = ev.currentTarget.getBoundingClientRect();
          const clicked: Side = ev.clientX < box.left + box.width / 2 ? "left" : "right";
          const side: Side = e[clicked] ? clicked : clicked === "left" ? "right" : "left";
          fops.onRowContext(e, side);
          menu.open(ev, { entry: e, side });
        }}
        title={review.title(e.relPath, rowTitle(e))}
      >
        {renderSide(e, "left", depth)}
        <div className={`fcell status${checked ? " checked" : ""}${e.filtered === "both" ? " filtered" : ""}`}>
          {oneSided ? null : e.pending ? (
            phase ? <span className="spinner" /> : <span className="muted">…</span>
          ) : (
            <StatusGlyph entry={e} />
          )}
        </div>
        {renderSide(e, "right", depth)}
      </div>
    );
  };

  const preset = presetOf(cfg);
  /** Filters beyond the Filters box (dialog only), and alignment overrides. */
  const dialogFilters = activeFilterCount(cfg) - (cfg.includeFiles.trim() ? 1 : 0);
  const alignCount = activeAlignCount(cfg);
  const narrowed = cfg.filter !== "all" && cfg.filter !== "same";
  const filterLabel = DISPLAY_FILTERS.find((f) => f.value === cfg.filter)?.label ?? "All";
  const copy = fops.copyOther;
  /** Status-bar stand-in for the old blocking overlay. */
  const liveStatus = phase && (
    <span className="live-progress" title={progress?.current}>
      <span className="spinner" />
      {phase === "scanning" ? "Scanning folders…" : phase === "updating" ? "Applying changes…" : "Comparing contents…"}
      {progress && (
        <span className="muted">
          {progress.scanned.toLocaleString()} items
          {progress.compared ? ` · ${progress.compared.toLocaleString()} compared` : ""}
        </span>
      )}
      <button onClick={cancel} title="Stop comparing (Esc)">
        Cancel
      </button>
    </span>
  );

  return (
    <div className={`session${cfg.showAttributes ? " fc-attrs" : ""}`} ref={root}>
      {/* Main toolbar: display filters, structure, Minor, Rules, copy, expand/collapse, selection, refresh. */}
      <div className="toolbar fc-toolbar">
        <div className="stack-group" role="group" aria-label="Show">
          <StackButton
            icon={<Columns2 size={16} />}
            label="All"
            active={cfg.filter === "all"}
            onClick={() => update({ filter: "all" })}
            title="Show all files and folders"
          />
          <StackMenuButton
            icon={<EqualNot size={16} />}
            label="Diffs"
            active={narrowed}
            onClick={() => update({ filter: "diffs" })}
            onMenu={(at) => filterMenu.open(at, null)}
            title={narrowed ? `Showing: ${filterLabel}. ▾ for orphans, newer files and more` : "Show differences. ▾ for orphans, newer files and more"}
          />
          <StackButton
            icon={<Equal size={16} />}
            label="Same"
            active={cfg.filter === "same"}
            onClick={() => update({ filter: "same" })}
            title="Show only files that are the same"
          />
        </div>
        <StackMenuButton
          icon={<FolderTree size={16} />}
          label="Structure"
          active={cfg.flatten || cfg.structure !== "structure"}
          onMenu={(at) => structureMenu.open(at, null)}
          title={`When folders show: ${cfg.flatten ? "Ignore folder structure" : FOLDER_STRUCTURES.find((f) => f.value === cfg.structure)?.label}`}
        />
        <StackButton
          icon={<Feather size={16} />}
          label="Minor"
          active={cfg.ignoreMinor}
          onClick={() => update({ ignoreMinor: !cfg.ignoreMinor })}
          title={
            cfg.ignoreMinor
              ? "Ignoring unimportant differences: files that differ only by the text rules count as same (click to count them as differences)"
              : "Unimportant differences count as differences (click to ignore them)"
          }
        />
        <StackButton
          icon={<SlidersHorizontal size={16} />}
          label={alignCount ? `Rules · ${alignCount}` : "Rules"}
          onClick={() => setSettingsPage("comparison")}
          title="Session rules: comparison criteria, alignment, filters and text rules"
        />
        <span className="sep" />
        <StackButton
          icon={copy.target === "left" ? <ArrowLeft size={16} /> : copy.target === "right" ? <ArrowRight size={16} /> : <ArrowLeftRight size={16} />}
          label="Copy"
          disabled={copy.disabled}
          onClick={(ev) => {
            if (copy.target) return copy.run(copy.target);
            ev.stopPropagation();
            const box = ev.currentTarget.getBoundingClientRect();
            copyMenu.open({ clientX: box.left, clientY: box.bottom + 2 }, null);
          }}
          title={copy.title}
        />
        <span className="sep" />
        <StackButton
          icon={<UnfoldVertical size={16} />}
          label="Expand"
          disabled={cfg.flatten}
          onClick={() => setExpanded(allDirs(entries))}
          title="Open every folder"
        />
        <StackButton
          icon={<FoldVertical size={16} />}
          label="Collapse"
          disabled={cfg.flatten}
          onClick={() => setExpanded(new Set())}
          title="Close every folder"
        />
        <StackButton
          icon={<ListChecks size={16} />}
          label="Select"
          disabled={!result}
          onClick={() => setSelecting(true)}
          title={`Select files and folders by status, name, size or date (${shortcutLabel("folder.selectWhere")})`}
        />
        <StackButton
          icon={<FileCheck size={16} />}
          label="Files"
          disabled={!flat.length}
          onClick={selectAllFiles}
          title="Select all files shown, on both sides"
        />
        <StackButton icon={<RefreshCw size={16} />} label="Refresh" onClick={() => run()} title="Refresh: list and compare both folders again (F5)" />
        <span className="spacer" />
        {/* Less frequent commands, icon only. */}
        <button className="icon-btn" title="Synchronize these folders" disabled={!left || !right} onClick={() => onOpenSync(left, right)}>
          <FolderSync size={14} />
        </button>
        <button
          className="icon-btn"
          title="Folder comparison report: summary or detailed, as HTML, text or CSV, honouring the display filter"
          disabled={!result}
          onClick={() => setReporting(true)}
        >
          <FileText size={14} />
        </button>
        <button
          className={`icon-btn${splitView ? " active" : ""}`}
          aria-pressed={splitView}
          title={`Show the selected file pair's comparison below the folders (${shortcutLabel("folder.splitView")})`}
          onClick={toggleSplit}
        >
          <PanelBottom size={14} />
        </button>
        {review.toolbar}
        <button
          className="icon-btn"
          title="Save a snapshot of a side: names, sizes, dates (and optionally CRCs) to compare against later"
          disabled={!left && !right}
          onClick={() => setSnapshotting(true)}
        >
          <Camera size={14} />
        </button>
        {fops.toolbar}
        <button
          className={`icon-btn${legendAt ? " active" : ""}`}
          title="View: ignore folder structure, attributes, split view, log, and the legend of colours and icons"
          onClick={(ev) => {
            ev.stopPropagation();
            const r = ev.currentTarget.getBoundingClientRect();
            viewMenu.open({ clientX: r.left, clientY: r.bottom }, { x: r.left, y: r.bottom + 2 });
          }}
        >
          <Eye size={14} />
        </button>
      </div>

      {/* Second row: swap, stop, the file filter and Peek (suppress filters). */}
      <div className="toolbar fc-toolbar fc-toolbar-2">
        <StackButton
          icon={<ArrowLeftRight size={16} />}
          label="Swap"
          disabled={!left && !right}
          onClick={() => onPathsChange(right, left)}
          title="Swap sides: exchange the left and right folders"
        />
        <StackButton icon={<CircleStop size={16} />} label="Stop" disabled={!phase} onClick={cancel} title="Stop comparing (Esc)" />
        <span className="sep" />
        <label className="fc-filterbox" title="Files to include, then -names to exclude (files or folders), separated by ; — e.g. *.rs;*.ts;-target;-*.tmp. Press Enter to apply.">
          Filters:
          <input
            list="fc-filter-presets"
            value={filterText}
            onChange={(e) => setFilterText(e.target.value)}
            onKeyDown={(e) => e.key === "Enter" && applyFilterBox()}
            onBlur={applyFilterBox}
            spellCheck={false}
          />
          <datalist id="fc-filter-presets">
            {FILTER_BOX_PRESETS.map((p) => (
              <option key={p} value={p} />
            ))}
          </datalist>
        </label>
        <StackButton
          icon={<Funnel size={16} />}
          label={dialogFilters ? `Filters · ${dialogFilters}` : "Filters"}
          onClick={() => setSettingsPage("filters")}
          title={`Name, size, date, attribute and content filters${dialogFilters ? ` (${dialogFilters} active)` : ""}`}
        />
        <StackButton
          icon={<FunnelX size={16} />}
          label="Peek"
          active={cfg.suppressFilters}
          onClick={() => update({ suppressFilters: !cfg.suppressFilters })}
          title="Peek: suppress the filters for a moment and show what they leave out, in teal"
        />
        <span className="spacer" />
        <label className="fc-find" title="Show only names that match: text, or wildcards like *.rs">
          <Search size={13} />
          <input value={nameQuery} onChange={(e) => setNameQuery(e.target.value)} placeholder="Find names" spellCheck={false} />
        </label>
        <label title="What decides that two files are the same">
          Compare
          <select
            value={preset}
            onChange={(e) => {
              const p = COMPARE_PRESETS[e.target.value as ComparePreset];
              if (p) update({ compareSize: p.compareSize, compareTimestamp: p.compareTimestamp, content: p.content });
            }}
          >
            {Object.entries(COMPARE_PRESETS).map(([key, p]) => (
              <option key={key} value={key}>
                {p.label}
              </option>
            ))}
            {preset === "custom" && <option value="custom">Custom</option>}
          </select>
        </label>
        <label className={`check${watchState === "watching" || watchState === "polling" ? " live" : ""}`} title={autoRefreshTitle(watchState)}>
          <input type="checkbox" checked={cfg.autoRefresh} onChange={(e) => update({ autoRefresh: e.target.checked })} />
          Auto-refresh
        </label>
      </div>

      <PathBar left={left} right={right} directory oneSided onCompare={onPathsChange} />

      <div className={`fc-split${splitView ? " on" : ""}`} ref={splitBox}>
        <div className="fc-split-top" style={splitView ? { flexGrow: splitRatio } : undefined}>
          <div className="fheader">
            <div>Name</div>
            <div className="num">Size</div>
            <div>Modified</div>
            {cfg.showAttributes && <div>Attr</div>}
            <div />
            <div>Name</div>
            <div className="num">Size</div>
            <div>Modified</div>
            {cfg.showAttributes && <div>Attr</div>}
          </div>

          <div className={`session-body${oneSided ? " one-sided" : ""}`}>
            {!left && !right && (
              <div className="empty-state">Choose a folder on either side — its contents list right away.</div>
            )}
            {error && <div className="banner error">{error}</div>}
            <ArchiveBanner left={left} right={right} content={engineOptions.content} onOpenHex={onOpenHex} />
            {result && flat.length === 0 && <div className="empty-state">Nothing to show for this filter.</div>}
            {result && <VirtualList className="folder-list" count={flat.length} rowHeight={24} renderRow={renderRow} />}
            {review.layer /* Bookmarks & comments panel */}
          </div>
          {/* Per side: what is selected there and the free space on its volume. */}
          <div className="fheader fc-sidefoot">
            {(["left", "right"] as const).map((side) => (
              <div key={side} className={`fc-sidefoot-${side}`} style={{ gridColumn: sideColumns(side, cfg.showAttributes) }}>
                <span>{selectionSummary(fops.selected, fops.sel.side, side)}</span>
                <span className="spacer" />
                <span>{freeLabel(free[side])}</span>
              </div>
            ))}
          </div>
        </div>
        {splitView && (
          <>
            <div
              className="fc-divider"
              role="separator"
              aria-orientation="horizontal"
              title="Drag to resize"
              onPointerDown={dragDivider}
            />
            <FolderSplitPane
              pair={splitPair}
              hint={splitHint}
              rules={engineOptions.rules}
              style={{ flexGrow: 1 - splitRatio }}
              onOpenInTab={openPairInTab}
              onDirtyChange={(d) => {
                splitDirty.current = d;
                onDirtyChange?.(d);
              }}
              onSaved={() => run("quiet")}
            />
          </>
        )}
      </div>
      {fops.panel /* docked Operations panel */}
      {cfg.showLog && (
        <div className="fc-log" role="log" aria-label="Log">
          <div className="fc-log-lines" ref={logBox}>
            {log.map((l, i) => (
              <div key={i} className={l.error ? "fc-log-error" : undefined}>
                <span className="fc-log-time">{new Date(l.at).toLocaleString()}</span>
                {l.text}
              </div>
            ))}
          </div>
          <button className="icon-btn" title="Hide the log (View → Log)" onClick={() => update({ showLog: false })}>
            <X size={12} />
          </button>
        </div>
      )}

      <div className="statusbar">
        {!stats && liveStatus}
        {stats && oneSided && (
          <>
            <span>
              {stats.leftOnly + stats.rightOnly} files in {basename(result!.leftRoot || result!.rightRoot)}
            </span>
            <span className="muted">Choose the other folder to compare</span>
            <span className="spacer" />
            {liveStatus}
            <span className="muted">{result!.elapsedMs} ms</span>
          </>
        )}
        {stats && !oneSided && (
          <>
            <span className="s-same-fg">
              {stats.same} same{stats.ignored ? ` (${stats.ignored} ignored)` : ""}
            </span>
            {stats.similar > 0 && <span className="s-similar-fg">{stats.similar} similar</span>}
            <span className="s-different-fg">{stats.different} different</span>
            <span className="s-leftOnly-fg">{stats.leftOnly} left only</span>
            <span className="s-rightOnly-fg">{stats.rightOnly} right only</span>
            {stats.errors > 0 && <span className="s-error-fg">{stats.errors} errors</span>}
            {cfg.filter !== "all" && <span className="muted">Showing: {filterLabel}</span>}
            {filteredCount > 0 && (
              <span
                className="s-filtered-fg"
                title={cfg.suppressFilters ? "Left out by the filters, shown in teal" : "Left out by the filters — Suppress shows them"}
              >
                {filteredCount} filtered
              </span>
            )}
            <span className="spacer" />
            {liveStatus}
            {checking && (
              <span>
                Comparing contents {checking.done}/{checking.total}…
              </span>
            )}
            <span
              className="muted"
              title={result!.needs && !result!.stats.dirsRead ? "Applied to the folders already listed; nothing was read again" : undefined}
            >
              {cfg.content !== "off" && !quickOnly ? `${METHOD_LABEL[cfg.content]} · ` : ""}
              {result!.elapsedMs} ms
            </span>
          </>
        )}
      </div>

      {menu.element}
      {viewMenu.element}
      {filterMenu.element}
      {structureMenu.element}
      {copyMenu.element}
      {legendAt && <LegendPopover {...legendAt} onClose={() => setLegendAt(null)} />}

      {/* the report lists what the view shows (re-checks and ignore marks included). */}
      {reporting && result && (
        <ReportDialog
          source={{
            kind: "folder",
            input: { leftRoot: result.leftRoot, rightRoot: result.rightRoot, entries },
            filter: oneSided ? "all" : cfg.filter,
            nameFilter: nameQuery,
            rules: engineOptions.rules,
          }}
          onClose={() => setReporting(false)}
          bookmarks={(cfg.bookmarks ?? []).map((b) => ({ path: b.relPath, comment: b.comment }))}
        />
      )}
      {fops.overlay}
      {review.dialogs}
      {selecting && result && (
        <FolderSelectDialog
          count={(c, reach) => rowsMatching(c, reach).rows.length}
          flatten={cfg.flatten}
          onSelect={selectWhere}
          onClose={() => setSelecting(false)}
        />
      )}
      {snapshotting && <SnapshotDialog folders={[left, right]} onClose={() => setSnapshotting(false)} />}
      {toolsDialog && <FileFormatsDialog initialPage="tools" onClose={() => setToolsDialog(false)} />}
      {settingsPage && (
        <FolderSettingsDialog
          value={cfg}
          initialPage={settingsPage}
          onClose={() => setSettingsPage(null)}
          onApply={(s: CompareSettings) => {
            update(s);
            setFilterText(formatFilterBox(s));
          }}
        />
      )}
    </div>
  );
}
