import { useCallback, useEffect, useMemo, useState } from "react";
import {
  api,
  errorMessage,
  type CompareCriteria,
  type EntryStatus,
  type FolderCompareOptions,
  type FolderCompareResult,
  type FolderEntry,
} from "../lib/api";
import { formatSize, formatTime, joinPath } from "../lib/format";
import { PathBar } from "./PathBar";
import { VirtualList } from "./VirtualList";

type Filter = "all" | "diffs" | "same" | "orphans";

interface Props {
  left: string;
  right: string;
  onPathsChange: (left: string, right: string) => void;
  onOpenFiles: (left: string, right: string) => void;
}

const STATUS_ICON: Record<EntryStatus, string> = {
  same: "=",
  different: "≠",
  leftOnly: "◀",
  rightOnly: "▶",
  typeMismatch: "⚠",
  error: "⚠",
};

const DEFAULT_EXCLUDE = ".git, node_modules";

function matches(e: FolderEntry, f: Filter): boolean {
  switch (f) {
    case "all":
      return true;
    case "diffs":
      return e.status !== "same";
    case "same":
      return e.status === "same";
    case "orphans":
      return e.status === "leftOnly" || e.status === "rightOnly";
  }
}

/** Post-order pass: a folder stays visible if it or any descendant matches. */
function visibleSet(entries: FolderEntry[], f: Filter): Set<string> {
  const out = new Set<string>();
  const visit = (e: FolderEntry): boolean => {
    let childVisible = false;
    for (const c of e.children) childVisible = visit(c) || childVisible;
    const show = (e.isDir && childVisible) || matches(e, f);
    if (show) out.add(e.relPath);
    return show;
  };
  entries.forEach(visit);
  return out;
}

function allDirs(entries: FolderEntry[], out = new Set<string>()): Set<string> {
  for (const e of entries) {
    if (e.isDir) {
      out.add(e.relPath);
      allDirs(e.children, out);
    }
  }
  return out;
}

interface FlatRow {
  entry: FolderEntry;
  depth: number;
}

export function FolderCompare({ left, right, onPathsChange, onOpenFiles }: Props) {
  const [criteria, setCriteria] = useState<CompareCriteria>("sizeAndTimestamp");
  const [includeHidden, setIncludeHidden] = useState(true);
  const [excludeText, setExcludeText] = useState(DEFAULT_EXCLUDE);
  const [exclude, setExclude] = useState(DEFAULT_EXCLUDE);
  const [filter, setFilter] = useState<Filter>("all");
  const [result, setResult] = useState<FolderCompareResult | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const [expanded, setExpanded] = useState<Set<string>>(new Set());
  const [selected, setSelected] = useState<string | null>(null);

  const run = useCallback(async () => {
    if (!left || !right) return;
    const options: FolderCompareOptions = {
      criteria,
      recursive: true,
      exclude: exclude.split(",").map((s) => s.trim()).filter(Boolean),
      includeHidden,
      ignoreNameCase: navigator.userAgent.includes("Windows"),
      timeToleranceSecs: 2,
      ignoreDstShift: false,
    };
    setLoading(true);
    setError(null);
    try {
      setResult(await api.compareFolders(left, right, options));
    } catch (e) {
      setError(errorMessage(e));
      setResult(null);
    } finally {
      setLoading(false);
    }
  }, [left, right, criteria, exclude, includeHidden]);

  useEffect(() => {
    run();
  }, [run]);

  const visible = useMemo(() => (result ? visibleSet(result.entries, filter) : new Set<string>()), [result, filter]);

  const flat = useMemo(() => {
    const out: FlatRow[] = [];
    const walk = (entries: FolderEntry[], depth: number) => {
      for (const e of entries) {
        if (!visible.has(e.relPath)) continue;
        out.push({ entry: e, depth });
        if (e.isDir && expanded.has(e.relPath)) walk(e.children, depth + 1);
      }
    };
    if (result) walk(result.entries, 0);
    return out;
  }, [result, visible, expanded]);

  const toggle = (rel: string) =>
    setExpanded((prev) => {
      const next = new Set(prev);
      next.has(rel) ? next.delete(rel) : next.add(rel);
      return next;
    });

  const open = (e: FolderEntry) => {
    if (e.isDir) return toggle(e.relPath);
    if (result && e.left && e.right) {
      onOpenFiles(joinPath(result.leftRoot, e.relPath), joinPath(result.rightRoot, e.relPath));
    }
  };

  const renderSide = (e: FolderEntry, side: "left" | "right", depth: number) => {
    const meta = e[side];
    const newer = e.newer === side ? " newer" : "";
    return (
      <>
        <div className={`fcell name${meta ? "" : " absent"}${newer}`} style={{ paddingLeft: 6 + depth * 16 }}>
          {e.isDir && meta ? (
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
              {e.name}
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
    return (
      <div
        className={`frow s-${e.status}${selected === e.relPath ? " selected" : ""}`}
        onClick={() => setSelected(e.relPath)}
        onDoubleClick={() => open(e)}
        title={e.error ?? undefined}
      >
        {renderSide(e, "left", depth)}
        <div className="fcell status">{STATUS_ICON[e.status]}</div>
        {renderSide(e, "right", depth)}
      </div>
    );
  };

  const s = result?.stats;
  return (
    <div className="session">
      <div className="toolbar">
        <div className="seg-control">
          {(
            [
              ["all", "All"],
              ["diffs", "Differences"],
              ["same", "Same"],
              ["orphans", "Orphans"],
            ] as [Filter, string][]
          ).map(([f, label]) => (
            <button key={f} className={filter === f ? "active" : ""} onClick={() => setFilter(f)}>
              {label}
            </button>
          ))}
        </div>
        <span className="sep" />
        <button onClick={() => result && setExpanded(allDirs(result.entries))}>Expand all</button>
        <button onClick={() => setExpanded(new Set())}>Collapse all</button>
        <span className="sep" />
        <label>
          Compare by
          <select value={criteria} onChange={(e) => setCriteria(e.target.value as CompareCriteria)}>
            <option value="sizeAndTimestamp">Size &amp; timestamp</option>
            <option value="size">Size only</option>
            <option value="content">Binary content</option>
          </select>
        </label>
        <label className="check">
          <input type="checkbox" checked={includeHidden} onChange={(e) => setIncludeHidden(e.target.checked)} />
          Hidden files
        </label>
        <label className="grow">
          Exclude
          <input
            value={excludeText}
            onChange={(e) => setExcludeText(e.target.value)}
            onKeyDown={(e) => e.key === "Enter" && setExclude(excludeText)}
            onBlur={() => setExclude(excludeText)}
            placeholder="*.tmp, bin, obj"
            spellCheck={false}
          />
        </label>
        <button onClick={run} title="Refresh">
          ⟳ Refresh
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
        {s && (
          <>
            <span className="s-same-fg">{s.same} same</span>
            <span className="s-different-fg">{s.different} different</span>
            <span className="s-leftOnly-fg">{s.leftOnly} left only</span>
            <span className="s-rightOnly-fg">{s.rightOnly} right only</span>
            {s.errors > 0 && <span className="s-error-fg">{s.errors} errors</span>}
            <span className="spacer" />
            <span className="muted">{result!.elapsedMs} ms</span>
          </>
        )}
      </div>
    </div>
  );
}
