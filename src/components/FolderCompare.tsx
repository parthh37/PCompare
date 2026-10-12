import {
  ArrowLeft,
  ArrowRight,
  ChevronDown,
  ChevronRight,
  Equal,
  EqualApproximately,
  EqualNot,
  TriangleAlert,
  type LucideIcon,
} from "lucide-react";
import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import {
  api,
  errorMessage,
  type ContentMethod,
  type ContentVerdict,
  type EntryStatus,
  type FolderCompareOptions,
  type FolderCompareResult,
  type FolderEntry,
  type Side,
  type TextDiffOptions,
} from "../lib/api";
import {
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
  nameMatcher,
  pairedFiles,
  tally,
  visibleSet,
  type DisplayFilter,
} from "../lib/folderTree";
import { basename, formatSize, formatTime, hex, joinPath } from "../lib/format";
import { useShell } from "../lib/shell";
import { autoRefreshTitle, useAutoRefresh } from "../lib/useAutoRefresh";
import { FolderSettingsDialog } from "./FolderSettingsDialog";
import { FileIcon, ToolIcon, type ToolIconName } from "./icons";
import { ToolButton } from "./ToolButton";
import { PathBar } from "./PathBar";
import { VirtualList } from "./VirtualList";

export type { FolderSettings };

interface Props {
  left: string;
  right: string;
  onPathsChange: (left: string, right: string) => void;
  /** Opens a text compare; `rules` carries this session's text rules over. */
  onOpenFiles: (left: string, right: string, rules: TextDiffOptions) => void;
  onOpenHex: (left: string, right: string) => void;
  /** Opens a Folder Sync session on the same two folders. */
  onOpenSync: (left: string, right: string) => void;
  settings?: Record<string, unknown>;
  onSettingsChange?: (s: FolderSettings) => void;
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
};

/** At most this many content re-checks run at once (each may read whole files). */
const RECHECK_CONCURRENCY = 4;

type Verdicts = Map<string, ContentVerdict & { method: ContentMethod }>;

interface FlatRow {
  entry: FolderEntry;
  depth: number;
}

interface Menu {
  x: number;
  y: number;
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
  const parts = [STATUS_LABEL[e.status]];
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
  settings,
  onSettingsChange,
}: Props) {
  const [cfg, setCfg] = useState<FolderSettings>(() => loadFolderSettings(settings));
  const update = (patch: Partial<FolderSettings>) => setCfg((c) => ({ ...c, ...patch }));
  const reportSettings = useRef(onSettingsChange);
  reportSettings.current = onSettingsChange;
  useEffect(() => reportSettings.current?.(cfg), [cfg]);

  const [excludeText, setExcludeText] = useState(cfg.exclude);
  const [nameQuery, setNameQuery] = useState("");
  const [showSettings, setShowSettings] = useState(false);
  const [result, setResult] = useState<FolderCompareResult | null>(null);
  const [verdicts, setVerdicts] = useState<Verdicts>(new Map());
  const [checking, setChecking] = useState<{ done: number; total: number } | null>(null);
  const [error, setError] = useState<string | null>(null);
  /** What the running compare is doing; the list stays usable throughout. */
  const [phase, setPhase] = useState<"scanning" | "contents" | null>(null);
  const [expanded, setExpanded] = useState<Set<string>>(new Set());
  const [selected, setSelected] = useState<string | null>(null);
  const [menu, setMenu] = useState<Menu | null>(null);
  const root = useRef<HTMLDivElement>(null);
  const shell = useShell();

  // Only settings that affect the engine re-run it; view settings don't.
  const engineOptions = useMemo(() => toEngineOptions(cfg), [cfg]);
  const optionsKey = JSON.stringify(engineOptions);
  const requestSeq = useRef(0);
  const shownPaths = useRef("");

  /**
   * Compares live: a quick size/timestamp pass fills the list straight away,
   * then the content pass (CRC, binary, rules) refines it in place. `quiet`
   * refreshes (auto-refresh) skip the quick pass so settled rows don't flicker.
   */
  const run = useCallback(
    async (quiet = false) => {
      // One folder is enough: it lists on its own until the other side is picked.
      if (!left && !right) return;
      const seq = ++requestSeq.current;
      const stale = () => seq !== requestSeq.current; // a newer compare superseded this one
      const options: FolderCompareOptions = JSON.parse(optionsKey);
      const show = (res: FolderCompareResult) => {
        setResult(res);
        setVerdicts(new Map());
        setError(null);
      };
      // New folders: don't leave the previous pair's rows up while scanning.
      const pathsKey = `${left}\n${right}`;
      if (shownPaths.current !== pathsKey) {
        shownPaths.current = pathsKey;
        setResult(null);
      }
      const twoPass = !quiet && options.content !== "off";
      try {
        if (twoPass) {
          setPhase("scanning");
          const quick = await api.compareFolders(left, right, { ...options, content: "off" });
          if (stale()) return;
          show(quick);
        }
        if (!quiet) setPhase(twoPass ? "contents" : "scanning");
        const res = await api.compareFolders(left, right, options);
        if (stale()) return;
        show(res);
      } catch (e) {
        if (stale()) return;
        setError(errorMessage(e));
      } finally {
        if (!stale()) setPhase(null);
      }
    },
    [left, right, optionsKey],
  );

  useEffect(() => {
    run();
  }, [run]);

  const watchState = useAutoRefresh({
    enabled: cfg.autoRefresh,
    paths: [left, right],
    exclude: engineOptions.exclude.concat(engineOptions.filters.excludeFolders),
    onChange: () => run(true),
  });

  const entries = useMemo(() => (result ? applyVerdicts(result.entries, verdicts) : []), [result, verdicts]);
  const stats = useMemo(() => (verdicts.size ? tally(entries) : result?.stats), [entries, verdicts, result]);
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
    result ? joinPath(side === "left" ? result.leftRoot : result.rightRoot, e.relPath) : "";

  const paths = (e: FolderEntry): [string, string] | null =>
    result && e.left && e.right ? [joinPath(result.leftRoot, e.relPath), joinPath(result.rightRoot, e.relPath)] : null;

  const open = (e: FolderEntry) => {
    if (e.isDir && !cfg.flatten) return toggle(e.relPath);
    const p = paths(e);
    if (p) onOpenFiles(...p, engineOptions.rules);
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

  // Keep the context menu on screen: shift it left at the right edge and
  // open it upwards when it would run past the bottom.
  const menuRef = useRef<HTMLDivElement>(null);
  useLayoutEffect(() => {
    const el = menuRef.current;
    if (!el || !menu) return;
    const { width, height } = el.getBoundingClientRect();
    el.style.left = `${Math.max(4, Math.min(menu.x, window.innerWidth - width - 4))}px`;
    el.style.top = `${menu.y + height > window.innerHeight - 4 ? Math.max(4, menu.y - height) : menu.y}px`;
  }, [menu]);

  useEffect(() => {
    if (!menu) return;
    const close = () => setMenu(null);
    window.addEventListener("click", close);
    window.addEventListener("blur", close);
    return () => {
      window.removeEventListener("click", close);
      window.removeEventListener("blur", close);
    };
  }, [menu]);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      // Every open tab stays mounted; only the visible one handles keys.
      if (e.key !== "F5" || !root.current?.offsetParent) return;
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
              {cfg.flatten ? e.relPath : e.name}
            </>
          )}
        </div>
        <div className={`fcell num${newer}`}>{meta && !meta.isDir ? formatSize(meta.size) : ""}</div>
        <div className={`fcell time${newer}`}>{meta ? formatTime(meta.modifiedMs) : ""}</div>
      </>
    );
  };

  const renderRow = (i: number) => {
    const { entry: e, depth } = flat[i];
    const checked = e.contentChecked && e.contentChecked !== "off";
    const StatusIcon = STATUS_ICON[e.status];
    return (
      <div
        className={`frow s-${e.status}${selected === e.relPath ? " selected" : ""}`}
        onClick={() => setSelected(e.relPath)}
        onDoubleClick={() => open(e)}
        onContextMenu={(ev) => {
          ev.preventDefault();
          setSelected(e.relPath);
          const box = ev.currentTarget.getBoundingClientRect();
          const clicked: Side = ev.clientX < box.left + box.width / 2 ? "left" : "right";
          const side: Side = e[clicked] ? clicked : clicked === "left" ? "right" : "left";
          setMenu({ x: ev.clientX, y: ev.clientY, entry: e, side });
        }}
        title={rowTitle(e)}
      >
        {renderSide(e, "left", depth)}
        <div className={`fcell status${checked ? " checked" : ""}`}>{!oneSided && <StatusIcon size={13} />}</div>
        {renderSide(e, "right", depth)}
      </div>
    );
  };

  const preset = presetOf(cfg);
  const filterCount = activeFilterCount(cfg);
  const primaryFilters = DISPLAY_FILTERS.filter((f) => f.primary);
  const moreFilters = DISPLAY_FILTERS.filter((f) => !f.primary);
  const progress = phase && (
    <span className="live-progress">
      <span className="spinner" />
      {phase === "scanning" ? "Scanning folders…" : "Comparing contents…"}
    </span>
  );
  const menuFiles = menu ? (menu.entry.isDir ? pairedFiles(menu.entry.children) : paths(menu.entry) ? [menu.entry] : []) : [];

  return (
    <div className="session" ref={root}>
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
          title={`Comparison, filters and text rules${filterCount ? ` (${filterCount} filter${filterCount > 1 ? "s" : ""} active)` : ""}`}
          onClick={() => setShowSettings(true)}
        />
        <span className="tool-sep" />
        <ToolButton icon="swap" label="Swap" title="Swap sides" disabled={!left && !right} onClick={() => onPathsChange(right, left)} />
        <ToolButton icon="refresh" label="Refresh" title="Refresh (F5)" onClick={() => run()} />
        <ToolButton icon="sync" label="Sync" title="Synchronize these folders" disabled={!left || !right} onClick={() => onOpenSync(left, right)} />
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
      </div>

      <PathBar left={left} right={right} directory oneSided onCompare={onPathsChange} />

      <div className="fheader">
        <div>Name</div>
        <div className="num">Size</div>
        <div>Modified</div>
        <div />
        <div>Name</div>
        <div className="num">Size</div>
        <div>Modified</div>
      </div>

      <div className={`session-body${oneSided ? " one-sided" : ""}`}>
        {!left && !right && (
          <div className="empty-state">Choose a folder on either side — its contents list right away.</div>
        )}
        {error && <div className="banner error">{error}</div>}
        {result && flat.length === 0 && <div className="empty-state">Nothing to show for this filter.</div>}
        {result && <VirtualList className="folder-list" count={flat.length} rowHeight={24} renderRow={renderRow} />}
      </div>

      <div className="statusbar">
        {!stats && progress}
        {stats && oneSided && (
          <>
            <span>
              {stats.leftOnly + stats.rightOnly} files in {basename(result!.leftRoot || result!.rightRoot)}
            </span>
            <span className="muted">Choose the other folder to compare</span>
            <span className="spacer" />
            {progress}
            <span className="muted">{result!.elapsedMs} ms</span>
          </>
        )}
        {stats && !oneSided && (
          <>
            <span className="s-same-fg">{stats.same} same</span>
            {stats.similar > 0 && <span className="s-similar-fg">{stats.similar} similar</span>}
            <span className="s-different-fg">{stats.different} different</span>
            <span className="s-leftOnly-fg">{stats.leftOnly} left only</span>
            <span className="s-rightOnly-fg">{stats.rightOnly} right only</span>
            {stats.errors > 0 && <span className="s-error-fg">{stats.errors} errors</span>}
            <span className="spacer" />
            {progress}
            {checking && (
              <span>
                Comparing contents {checking.done}/{checking.total}…
              </span>
            )}
            <span className="muted">
              {/* During the content pass, the rows shown are still the quick pass's. */}
              {cfg.content !== "off" && phase !== "contents" ? `${METHOD_LABEL[cfg.content]} · ` : ""}
              {result!.elapsedMs} ms
            </span>
          </>
        )}
      </div>

      {menu && (
        <div className="ctx-menu" ref={menuRef} style={{ left: menu.x, top: menu.y }} onClick={(e) => e.stopPropagation()}>
          {!menu.entry.isDir && paths(menu.entry) && (
            <>
              <button className="ctx-icon" onClick={() => (setMenu(null), onOpenFiles(...paths(menu.entry)!, engineOptions.rules))}>
                <ToolIcon name="text" size={16} /> Open in Text Compare
              </button>
              <button className="ctx-icon" onClick={() => (setMenu(null), onOpenHex(...paths(menu.entry)!))}>
                <ToolIcon name="hex" size={16} /> Open in Hex Compare
              </button>
              <hr />
            </>
          )}
          <div className="ctx-label">
            Compare contents{menu.entry.isDir ? ` (${menuFiles.length} files)` : ""}
          </div>
          {(["binary", "crc", "rules"] as ContentMethod[]).map((m) => (
            <button
              key={m}
              disabled={!menuFiles.length || !!checking}
              onClick={() => {
                setMenu(null);
                recheck(menuFiles, m);
              }}
            >
              {m === "binary" ? "Binary" : m === "crc" ? "CRC-32" : "Rules-based"}
            </button>
          ))}
          <hr />
          <button className="ctx-icon" onClick={() => (setMenu(null), shell.selectLeft(sidePath(menu.entry, menu.side)))}>
            <ToolIcon name="selectLeft" size={16} /> Select Left Side for Compare
          </button>
          {shell.leftSelection && shell.leftSelection !== sidePath(menu.entry, menu.side) && (
            <button
              className="ctx-icon"
              title={`${shell.leftSelection}\n↔ ${sidePath(menu.entry, menu.side)}`}
              onClick={() => (setMenu(null), shell.compareToLeft(sidePath(menu.entry, menu.side)))}
            >
              <ToolIcon name="compareTo" size={16} /> Compare to “{basename(shell.leftSelection)}”
            </button>
          )}
          <hr />
          <button
            className="ctx-icon"
            onClick={() => {
              setMenu(null);
              navigator.clipboard.writeText(menu.entry.relPath).catch(() => {});
            }}
          >
            <ToolIcon name="copy" size={16} /> Copy relative path
          </button>
        </div>
      )}

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
