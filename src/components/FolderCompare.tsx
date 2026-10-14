import {
  ArrowLeft,
  ArrowRight,
  ChevronDown,
  ChevronRight,
  Equal,
  EqualApproximately,
  EqualNot,
  EyeOff,
  Link,
  TriangleAlert,
  type LucideIcon,
} from "lucide-react";
import { ListChecks, PanelBottom } from "lucide-react";
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
  loadFolderSettings,
  presetOf,
  toEngineOptions,
  type ComparePreset,
  type CompareSettings,
  type FolderSettings,
} from "../lib/folderSettings";
import {
  allDirs,
  applyVerdicts,
  DISPLAY_FILTERS,
  effectiveStatus,
  LiveTree,
  nameMatcher,
  pairedFiles,
  selectMatching,
  tally,
  visibleSet,
  type DisplayFilter,
  type SelectCriteria,
} from "../lib/folderTree";
import { basename, formatSize, formatTime, hex, joinPath } from "../lib/format";
import { writeClipboardText } from "../lib/clipboard";
import { useShell } from "../lib/shell";
import { autoRefreshTitle, useAutoRefresh } from "../lib/useAutoRefresh";
import { FolderSettingsDialog } from "./FolderSettingsDialog";
import { FileIcon, type ToolIconName } from "./icons";
import { ToolButton } from "./ToolButton";
// bc5:converters — external comparison tools.
import { openExternalIfAlways } from "../lib/convertApi";
import { externalToolItems } from "../lib/externalToolMenu";
import { SEPARATOR, type MenuItems } from "../lib/nativeMenu";
import { useContextMenu } from "./ContextMenu";
import { FileFormatsDialog } from "./FileFormatsDialog";
import { PathBar } from "./PathBar";
import { VirtualList } from "./VirtualList";
import { ReportDialog } from "./ReportDialog"; // bc5:reports-script
import { useFolderFileOps } from "./FolderFileOps"; // bc5:file-ops
import { ArchiveBanner } from "./ArchiveBanner"; // bc5:archives
import { SnapshotDialog } from "./SnapshotDialog"; // bc5:archives
import { useFolderReview } from "./FolderReview"; // araxis:text-review
import { embedFocused } from "../lib/embeddedPane";
import { matches, shortcutLabel } from "../lib/keymap";
import { getPrefs, setPrefs, usePref } from "../lib/prefs";
import { sessionForFiles } from "../lib/sessions";
import { FolderSelectDialog, type SelectRequest } from "./FolderSelectDialog";
import { FolderSplitPane, type SplitPair } from "./FolderSplitPane";
import "./FolderCompare.css";

export type { FolderSettings };

interface Props {
  left: string;
  right: string;
  onPathsChange: (left: string, right: string) => void;
  /** Opens a text compare; `rules` carries this session's text rules over. */
  onOpenFiles: (left: string, right: string, rules: TextDiffOptions) => void;
  onOpenHex: (left: string, right: string) => void;
  /** Opens a Folder Sync session on the same two folders (bc5:sync-matrix: `scope` limits it to those items). */
  onOpenSync: (left: string, right: string, scope?: string[]) => void;
  /** bc5:editor-patch — opens one side's file in Text Edit. */
  onEditFile?: (path: string) => void;
  settings?: Record<string, unknown>;
  onSettingsChange?: (s: FolderSettings) => void;
  /** The split view's embedded file comparison holds unsaved edits. */
  onDirtyChange?: (dirty: boolean) => void;
}

const STATUS_ICON: Record<EntryStatus, LucideIcon> = {
  same: Equal,
  similar: EqualApproximately,
  different: EqualNot,
  leftOnly: ArrowLeft,
  rightOnly: ArrowRight,
  typeMismatch: TriangleAlert,
  error: TriangleAlert,
};

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

interface FlatRow {
  entry: FolderEntry;
  depth: number;
}

interface MenuTarget {
  entry: FolderEntry;
  /** The half of the row that was right-clicked (or the only side the entry exists on). */
  side: Side;
}

const FILTER_ICON: Partial<Record<DisplayFilter, ToolIconName>> = {
  all: "all",
  diffs: "diffs",
  same: "same",
  orphans: "orphans",
};

function rowTitle(e: FolderEntry): string {
  if (e.pending) return "Not compared yet";
  const parts = e.ignored ? [`Ignored (marked as same; really ${STATUS_LABEL[e.status].toLowerCase()})`] : [STATUS_LABEL[e.status]];
  if (e.leftRelPath) parts.push(`aligned with ${e.leftRelPath} on the left`);
  if (attributesDiffer(e)) parts.push(`attributes ${formatAttributes(e.left)} ↔ ${formatAttributes(e.right)}`);
  for (const side of ["left", "right"] as const) {
    const target = e[side]?.linkTarget;
    if (target != null) parts.push(`${side} is a link → ${target || "(unreadable)"}`);
  }
  if (e.contentChecked && e.contentChecked !== "off") parts.push(`checked by ${METHOD_LABEL[e.contentChecked]}`);
  if (e.crc) parts.push(`CRC ${hex(e.crc[0], 8)} ↔ ${hex(e.crc[1], 8)}`);
  if (e.newer) parts.push(`${e.newer} is newer`);
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
  onEditFile, // bc5:editor-patch
  settings,
  onSettingsChange,
  onDirtyChange,
}: Props) {
  const [cfg, setCfg] = useState<FolderSettings>(() => loadFolderSettings(settings));
  const update = (patch: Partial<FolderSettings>) => setCfg((c) => ({ ...c, ...patch }));
  const reportSettings = useRef(onSettingsChange);
  reportSettings.current = onSettingsChange;
  useEffect(() => void reportSettings.current?.(cfg), [cfg]);

  const [excludeText, setExcludeText] = useState(cfg.exclude);
  const [nameQuery, setNameQuery] = useState("");
  const [showSettings, setShowSettings] = useState(false);
  const [toolsDialog, setToolsDialog] = useState(false); // bc5:converters
  const [snapshotting, setSnapshotting] = useState(false); // bc5:archives
  const [reporting, setReporting] = useState(false); // bc5:reports-script
  const [selecting, setSelecting] = useState(false);
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
  const [phase, setPhase] = useState<"scanning" | "contents" | null>(null);
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

  /**
   * Compares live: rows stream in folder by folder as they're listed, a quick
   * size/timestamp pass settles them, then the content pass (CRC, binary,
   * rules) refines them in place. `quiet` refreshes (auto-refresh) skip the
   * quick pass; their rows replace settled ones only when final, so nothing
   * flickers.
   */
  const expandArchives = !!cfg.expandArchives; // bc5:archives
  const run = useCallback(
    async (quiet = false) => {
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
      }
      const tree = live.current;
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
      // own run key so Esc / Cancel stops whichever one is in flight.
      const pass = (opts: FolderCompareOptions) => {
        if (runKey.current) api.cancelCompareFolders(runKey.current).catch(() => {});
        const key = `folder-${crypto.randomUUID()}`;
        runKey.current = key;
        if (!quiet) setProgress(null);
        tree.beginPass();
        return api.compareFolders(left, right, opts, key, (p) => !stale() && setProgress(p), expandArchives, onLevels);
      };
      const twoPass = !quiet && options.content !== "off";
      try {
        if (twoPass) {
          setPhase("scanning");
          const quick = await pass({ ...options, content: "off" });
          if (stale()) return;
          finish(quick, true);
        }
        if (!quiet) setPhase(twoPass ? "contents" : "scanning");
        const res = await pass(options);
        if (stale()) return;
        finish(res);
      } catch (e) {
        window.clearTimeout(timer);
        if (stale()) return;
        const msg = errorMessage(e);
        if (msg === CANCELLED) {
          // Whatever streamed in before the stop stays up.
          if (timer !== undefined) paint();
          const kept = shown || painted || hasResult.current;
          setError(`Comparison cancelled${kept ? " — showing what was compared so far." : "."}`);
        } else {
          setError(msg);
          if (!quiet && !shown) setResult(null);
        }
      } finally {
        if (!stale()) {
          setPhase(null);
          runKey.current = null;
        }
      }
    },
    [left, right, optionsKey, expandArchives],
  );

  const cancel = () => {
    if (runKey.current) api.cancelCompareFolders(runKey.current).catch(() => {});
  };

  useEffect(() => {
    run();
  }, [run]);

  const watchState = useAutoRefresh({
    enabled: cfg.autoRefresh,
    paths: [left, right],
    exclude: engineOptions.exclude.concat(engineOptions.filters.excludeFolders),
    onChange: () => run(true),
  });

  const ignoredSet = useMemo(() => new Set(cfg.ignored), [cfg.ignored]);
  const entries = useMemo(
    () => (result ? applyVerdicts(result.entries, verdicts, ignoredSet) : []),
    [result, verdicts, ignoredSet],
  );
  const stats = useMemo(
    () => (verdicts.size || ignoredSet.size ? tally(entries) : result?.stats),
    [entries, verdicts, ignoredSet, result],
  );
  const matcher = useMemo(() => nameMatcher(nameQuery), [nameQuery]);
  /** Only one folder chosen so far: show its listing rather than a sea of orphans. */
  const oneSided = !!result && (!result.leftRoot || !result.rightRoot);
  // Everything is an "orphan" until the other side exists, so display filters wait for it.
  const visible = useMemo(
    () => visibleSet(entries, oneSided ? "all" : cfg.filter, matcher),
    [entries, oneSided, cfg.filter, matcher],
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
    // bc5:converters — pairs matching an "always use" external tool open there instead.
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

  // araxis:text-review — bookmarks with comments, Statistics, Save comparison archive.
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
  // araxis:text-review end

  // bc5:file-ops — multi-selection, file operations and the Operations panel.
  const fops = useFolderFileOps({
    root,
    result,
    entries,
    flat,
    left,
    right,
    // Enter opens the selection: one folder toggles, file pairs open (a handful at most).
    onOpen: (list) => (list.length === 1 ? open(list[0]) : list.filter((e) => !e.isDir).slice(0, 10).forEach(open)),
    onRefresh: () => run(true),
    onError: setError,
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
  const menu = useContextMenu(({ entry, side }: MenuTarget): Promise<MenuItems> | MenuItems => {
    // bc5:file-ops — context-menu actions cover the whole selection.
    const list = fops.menuEntries(entry);
    const files = list.flatMap((e) => (e.isDir ? pairedFiles(e.children) : paths(e) ? [e] : []));
    const pair = !entry.isDir ? paths(entry) : null;
    const path = sidePath(entry, side);
    const items = (tools: MenuItems): MenuItems => [
      ...(pair
        ? [
            { label: "Open in Text Compare", onClick: () => onOpenFiles(...pair, engineOptions.rules) },
            { label: "Open in Hex Compare", onClick: () => onOpenHex(...pair) },
            ...tools, // bc5:converters — third-party comparison tools
            SEPARATOR,
          ]
        : []),
      // bc5:editor-patch — edit the clicked side's file on its own
      onEditFile && !entry.isDir && entry[side] && { label: `Edit ${side} file`, onClick: () => onEditFile(path) },
      SEPARATOR,
      ...review.menuItems(entry), // araxis:text-review
      SEPARATOR,
      ...fops.menuItems(), // bc5:file-ops
      // bc5:sync-matrix — Folder Sync limited to the selection
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
    // Beyond Compare draws the newer of two differing files in red and the older in grey.
    const newer = e.newer === side ? " newer" : e.newer ? " older" : "";
    const showChevron = e.isDir && meta && !cfg.flatten;
    return (
      <>
        <div className={`fcell name${meta ? "" : " absent"}${newer}`} style={{ paddingLeft: 6 + depth * 16 }}>
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
              {cfg.flatten ? (side === "left" ? (e.leftRelPath ?? e.relPath) : e.relPath) : sideName(e, side)}
              {meta.linkTarget != null && (
                <span className="fc-link" title={`Link → ${meta.linkTarget || "(unreadable)"}`}>
                  <Link size={11} />
                </span>
              )}
              {review.mark(e, side) /* araxis:text-review */}
            </>
          )}
        </div>
        <div className={`fcell num${newer}`}>{meta && !meta.isDir ? formatSize(meta.size) : ""}</div>
        <div className={`fcell time${newer}`}>{meta ? formatTime(meta.modifiedMs) : ""}</div>
        {cfg.showAttributes && (
          <div className={`fcell fc-attr${attributesDiffer(e) ? " differs" : ""}`}>{formatAttributes(meta)}</div>
        )}
      </>
    );
  };

  const renderRow = (i: number) => {
    const { entry: e, depth } = flat[i];
    const checked = e.contentChecked && e.contentChecked !== "off";
    const StatusIcon = e.ignored ? EyeOff : STATUS_ICON[e.status];
    return (
      <div
        className={`frow s-${e.pending ? "pending" : effectiveStatus(e)}${e.ignored ? " ignored" : ""}${fops.rowClass(e.relPath)}${review.rowClass(e.relPath) /* araxis:text-review */}`}
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
        title={review.title(e.relPath, rowTitle(e)) /* araxis:text-review */}
      >
        {renderSide(e, "left", depth)}
        <div className={`fcell status${checked ? " checked" : ""}`}>
          {oneSided ? null : e.pending ? (
            phase ? <span className="spinner" /> : <span className="muted">…</span>
          ) : (
            <StatusIcon size={13} />
          )}
        </div>
        {renderSide(e, "right", depth)}
      </div>
    );
  };

  const preset = presetOf(cfg);
  const filterCount = activeFilterCount(cfg) + activeAlignCount(cfg);
  const primaryFilters = DISPLAY_FILTERS.filter((f) => f.primary);
  const moreFilters = DISPLAY_FILTERS.filter((f) => !f.primary);
  /** Status-bar stand-in for the old blocking overlay. */
  const liveStatus = phase && (
    <span className="live-progress" title={progress?.current}>
      <span className="spinner" />
      {phase === "scanning" ? "Scanning folders…" : "Comparing contents…"}
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
      <div className="bc-toolbar">
        {primaryFilters.map((f) => (
          <ToolButton
            key={f.value}
            icon={FILTER_ICON[f.value]!}
            label={f.label === "Differences" ? "Diffs" : f.label}
            title={`Show ${f.label.toLowerCase()}`}
            active={cfg.filter === f.value}
            onClick={() => update({ filter: f.value })}
          />
        ))}
        <select
          className={`more-filters${moreFilters.some((f) => f.value === cfg.filter) ? " active" : ""}`}
          value={moreFilters.some((f) => f.value === cfg.filter) ? cfg.filter : ""}
          onChange={(e) => e.target.value && update({ filter: e.target.value as DisplayFilter })}
          title="More display filters"
        >
          <option value="">More…</option>
          {moreFilters.map((f) => (
            <option key={f.value} value={f.value}>
              {f.label}
            </option>
          ))}
        </select>
        <span className="tool-sep" />
        <ToolButton icon="expand" label="Expand" title="Expand all folders" disabled={cfg.flatten} onClick={() => setExpanded(allDirs(entries))} />
        <ToolButton icon="collapse" label="Collapse" title="Collapse all folders" disabled={cfg.flatten} onClick={() => setExpanded(new Set())} />
        <ToolButton
          icon="flatten"
          label="Flatten"
          title="Ignore folder structure: list every file at one level"
          active={cfg.flatten}
          onClick={() => update({ flatten: !cfg.flatten })}
        />
        <span className="tool-sep" />
        <ToolButton
          icon="rules"
          label={filterCount ? `Rules · ${filterCount}` : "Rules"}
          title={`Comparison, alignment, filters and text rules${filterCount ? ` (${filterCount} filter${filterCount > 1 ? "s" : ""} or override${filterCount > 1 ? "s" : ""} active)` : ""}`}
          onClick={() => setShowSettings(true)}
        />
        <span className="tool-sep" />
        <ToolButton icon="swap" label="Swap" title="Swap sides" disabled={!left && !right} onClick={() => onPathsChange(right, left)} />
        <ToolButton icon="refresh" label="Refresh" title="Refresh (F5)" onClick={() => run()} />
        <ToolButton icon="sync" label="Sync" title="Synchronize these folders" disabled={!left || !right} onClick={() => onOpenSync(left, right)} />
        {/* bc5:reports-script */}
        <ToolButton
          icon="report"
          label="Report"
          title="Folder comparison report: summary or detailed, as HTML, text or CSV, honouring the display filter"
          disabled={!result}
          onClick={() => setReporting(true)}
        />
        <button
          className="tool-btn"
          title={`Select files and folders by status, name, size or date (${shortcutLabel("folder.selectWhere")})`}
          disabled={!result}
          onClick={() => setSelecting(true)}
        >
          <ListChecks size={20} className="fop-tool" />
          <span>Select</span>
        </button>
        <button
          className={`tool-btn${splitView ? " active" : ""}`}
          aria-pressed={splitView}
          title={`Show the selected file pair's comparison below the folders (${shortcutLabel("folder.splitView")})`}
          onClick={toggleSplit}
        >
          <PanelBottom size={20} className="fop-tool" />
          <span>Split</span>
        </button>
        {review.toolbar /* araxis:text-review */}
        {fops.toolbar /* bc5:file-ops */}
        {/* bc5:archives */}
        <ToolButton
          icon="snapshot"
          label="Snapshot"
          title="Save a snapshot of a side: names, sizes, dates (and optionally CRCs) to compare against later"
          disabled={!left && !right}
          onClick={() => setSnapshotting(true)}
        />
      </div>

      <div className="toolbar filterbar">
        <input
          className="name-filter"
          value={nameQuery}
          onChange={(e) => setNameQuery(e.target.value)}
          placeholder="Filter names (*.rs)"
          spellCheck={false}
        />
        <label>
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
        <label className={`check${watchState === "watching" || watchState === "polling" ? " live" : ""}`} title={autoRefreshTitle(watchState)}>
          <input type="checkbox" checked={cfg.autoRefresh} onChange={(e) => update({ autoRefresh: e.target.checked })} />
          Auto-refresh
        </label>
        <label className="check" title="Show file attributes (DOS RHSA / Unix permissions)">
          <input
            type="checkbox"
            checked={cfg.showAttributes}
            onChange={(e) => update({ showAttributes: e.target.checked })}
          />
          Attributes
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
            {/* bc5:archives */}
            <ArchiveBanner left={left} right={right} content={engineOptions.content} onOpenHex={onOpenHex} />
            {result && flat.length === 0 && <div className="empty-state">Nothing to show for this filter.</div>}
            {result && <VirtualList className="folder-list" count={flat.length} rowHeight={24} renderRow={renderRow} />}
            {review.layer /* araxis:text-review — Bookmarks & comments panel */}
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
              onSaved={() => run(true)}
            />
          </>
        )}
      </div>
      {fops.panel /* bc5:file-ops — docked Operations panel */}

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
            <span className="spacer" />
            {liveStatus}
            {checking && (
              <span>
                Comparing contents {checking.done}/{checking.total}…
              </span>
            )}
            <span className="muted">
              {cfg.content !== "off" && !quickOnly ? `${METHOD_LABEL[cfg.content]} · ` : ""}
              {result!.elapsedMs} ms
            </span>
          </>
        )}
      </div>

      {menu.element}

      {/* bc5:reports-script — the report lists what the view shows (re-checks and ignore marks included). */}
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
      {fops.overlay /* bc5:file-ops */}
      {review.dialogs /* araxis:text-review */}
      {selecting && result && (
        <FolderSelectDialog
          count={(c, reach) => rowsMatching(c, reach).rows.length}
          flatten={cfg.flatten}
          onSelect={selectWhere}
          onClose={() => setSelecting(false)}
        />
      )}
      {snapshotting && <SnapshotDialog folders={[left, right]} onClose={() => setSnapshotting(false)} /> /* bc5:archives */}
      {/* bc5:converters */}
      {toolsDialog && <FileFormatsDialog initialPage="tools" onClose={() => setToolsDialog(false)} />}
      {showSettings && (
        <FolderSettingsDialog
          value={cfg}
          onClose={() => setShowSettings(false)}
          onApply={(s: CompareSettings) => {
            update(s);
            setExcludeText(s.exclude);
          }}
        />
      )}
    </div>
  );
}
