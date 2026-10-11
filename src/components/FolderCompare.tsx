import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  api,
  errorMessage,
  type ContentMethod,
  type ContentVerdict,
  type EntryStatus,
  type FolderCompareResult,
  type FolderEntry,
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
import { formatSize, formatTime, hex, joinPath } from "../lib/format";
import { autoRefreshTitle, useAutoRefresh } from "../lib/useAutoRefresh";
import { FolderSettingsDialog } from "./FolderSettingsDialog";
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
  settings?: Record<string, unknown>;
  onSettingsChange?: (s: FolderSettings) => void;
}

const STATUS_ICON: Record<EntryStatus, string> = {
  same: "=",
  similar: "≈",
  different: "≠",
  leftOnly: "◀",
  rightOnly: "▶",
  typeMismatch: "⚠",
  error: "⚠",
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
}

function rowTitle(e: FolderEntry): string {
  const parts = [STATUS_LABEL[e.status]];
  if (e.contentChecked && e.contentChecked !== "off") parts.push(`checked by ${METHOD_LABEL[e.contentChecked]}`);
  if (e.crc) parts.push(`CRC ${hex(e.crc[0], 8)} ↔ ${hex(e.crc[1], 8)}`);
  if (e.newer) parts.push(`${e.newer} is newer`);
  if (e.error) parts.push(e.error);
  return parts.join(" · ");
}

export function FolderCompare({ left, right, onPathsChange, onOpenFiles, onOpenHex, settings, onSettingsChange }: Props) {
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
  const [loading, setLoading] = useState(false);
  const [expanded, setExpanded] = useState<Set<string>>(new Set());
  const [selected, setSelected] = useState<string | null>(null);
  const [menu, setMenu] = useState<Menu | null>(null);
  const root = useRef<HTMLDivElement>(null);

  // Only settings that affect the engine re-run it; view settings don't.
  const engineOptions = useMemo(() => toEngineOptions(cfg), [cfg]);
  const optionsKey = JSON.stringify(engineOptions);
  const requestSeq = useRef(0);

  /** `quiet` refreshes (auto-refresh) keep the current view instead of flashing an overlay. */
  const run = useCallback(
    async (quiet = false) => {
      if (!left || !right) return;
      const seq = ++requestSeq.current;
      if (!quiet) setLoading(true);
      try {
        const res = await api.compareFolders(left, right, JSON.parse(optionsKey));
        if (seq !== requestSeq.current) return; // a newer compare superseded this one
        setResult(res);
        setVerdicts(new Map());
        setError(null);
      } catch (e) {
        if (seq !== requestSeq.current) return;
        setError(errorMessage(e));
        if (!quiet) setResult(null);
      } finally {
        if (seq === requestSeq.current) setLoading(false);
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
  const visible = useMemo(() => visibleSet(entries, cfg.filter, matcher), [entries, cfg.filter, matcher]);

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
    const newer = e.newer === side ? " newer" : "";
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
              {expanded.has(e.relPath) ? "▾" : "▸"}
            </span>
          ) : (
            <span className="chevron" />
          )}
          {meta && (
            <>
              <span className="ficon">{e.isDir ? "📁" : "📄"}</span>
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
    return (
      <div
        className={`frow s-${e.status}${selected === e.relPath ? " selected" : ""}`}
        onClick={() => setSelected(e.relPath)}
        onDoubleClick={() => open(e)}
        onContextMenu={(ev) => {
          ev.preventDefault();
          setSelected(e.relPath);
          setMenu({ x: ev.clientX, y: ev.clientY, entry: e });
        }}
        title={rowTitle(e)}
      >
        {renderSide(e, "left", depth)}
        <div className={`fcell status${checked ? " checked" : ""}`}>{STATUS_ICON[e.status]}</div>
        {renderSide(e, "right", depth)}
      </div>
    );
  };

  const preset = presetOf(cfg);
  const filterCount = activeFilterCount(cfg);
  const primaryFilters = DISPLAY_FILTERS.filter((f) => f.primary);
  const moreFilters = DISPLAY_FILTERS.filter((f) => !f.primary);
  const menuFiles = menu ? (menu.entry.isDir ? pairedFiles(menu.entry.children) : paths(menu.entry) ? [menu.entry] : []) : [];

  return (
    <div className="session" ref={root}>
      <div className="toolbar">
        <div className="seg-control">
          {primaryFilters.map((f) => (
            <button key={f.value} className={cfg.filter === f.value ? "active" : ""} onClick={() => update({ filter: f.value })}>
              {f.label}
            </button>
          ))}
        </div>
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
        <input
          className="name-filter"
          value={nameQuery}
          onChange={(e) => setNameQuery(e.target.value)}
          placeholder="Filter names (*.rs)"
          spellCheck={false}
        />
        <span className="sep" />
        <button onClick={() => setExpanded(allDirs(entries))} disabled={cfg.flatten} title="Expand all folders">
          Expand
        </button>
        <button onClick={() => setExpanded(new Set())} disabled={cfg.flatten} title="Collapse all folders">
          Collapse
        </button>
        <label className="check" title="Ignore folder structure: list every file at one level">
          <input type="checkbox" checked={cfg.flatten} onChange={(e) => update({ flatten: e.target.checked })} />
          Flatten
        </label>
        <span className="sep" />
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
        <button onClick={() => setShowSettings(true)} title="Comparison, filters and text rules">
          ⚙ Settings{filterCount ? ` · ${filterCount} filter${filterCount > 1 ? "s" : ""}` : ""}
        </button>
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
        <button onClick={() => onPathsChange(right, left)} title="Swap sides" disabled={!left || !right}>
          ⇄
        </button>
        <button onClick={() => run()} title="Refresh (F5)">
          ⟳
        </button>
      </div>

      <PathBar left={left} right={right} directory onCompare={onPathsChange} />

      <div className="fheader">
        <div>Name</div>
        <div className="num">Size</div>
        <div>Modified</div>
        <div />
        <div>Name</div>
        <div className="num">Size</div>
        <div>Modified</div>
      </div>

      <div className="session-body">
        {loading && <div className="overlay">Comparing folders…</div>}
        {error && <div className="banner error">{error}</div>}
        {result && flat.length === 0 && <div className="empty-state">Nothing to show for this filter.</div>}
        {result && <VirtualList className="folder-list" count={flat.length} rowHeight={24} renderRow={renderRow} />}
      </div>

      <div className="statusbar">
        {stats && (
          <>
            <span className="s-same-fg">{stats.same} same</span>
            {stats.similar > 0 && <span className="s-similar-fg">{stats.similar} similar</span>}
            <span className="s-different-fg">{stats.different} different</span>
            <span className="s-leftOnly-fg">{stats.leftOnly} left only</span>
            <span className="s-rightOnly-fg">{stats.rightOnly} right only</span>
            {stats.errors > 0 && <span className="s-error-fg">{stats.errors} errors</span>}
            <span className="spacer" />
            {checking && (
              <span>
                Comparing contents {checking.done}/{checking.total}…
              </span>
            )}
            <span className="muted">
              {cfg.content !== "off" ? `${METHOD_LABEL[cfg.content]} · ` : ""}
              {result!.elapsedMs} ms
            </span>
          </>
        )}
      </div>

      {menu && (
        <div className="ctx-menu" style={{ left: menu.x, top: menu.y }} onClick={(e) => e.stopPropagation()}>
          {!menu.entry.isDir && paths(menu.entry) && (
            <>
              <button onClick={() => (setMenu(null), onOpenFiles(...paths(menu.entry)!, engineOptions.rules))}>Open in Text Compare</button>
              <button onClick={() => (setMenu(null), onOpenHex(...paths(menu.entry)!))}>Open in Hex Compare</button>
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
          <button
            onClick={() => {
              setMenu(null);
              navigator.clipboard.writeText(menu.entry.relPath).catch(() => {});
            }}
          >
            Copy relative path
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
