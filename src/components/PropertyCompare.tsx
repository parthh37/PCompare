import { ArrowLeft, ArrowRight, ChevronDown, ChevronRight, Equal, EqualNot, RefreshCw, type LucideIcon } from "lucide-react";
import { useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { matches as matchesKey, shortcutLabel } from "../lib/keymap";
import type { DiffStatus, PropertyCompareResult, PropertyRow } from "../lib/metaApi";
import { BusyOverlay } from "./ProgressBar";
import "./PropertyCompare.css";

export type PropertyFilter = "all" | "diffs" | "same";

export const STATUS_ICON: Record<DiffStatus, LucideIcon> = {
  same: Equal,
  different: EqualNot,
  leftOnly: ArrowLeft,
  rightOnly: ArrowRight,
};

interface Props {
  result: PropertyCompareResult | null;
  loading: boolean;
  /** Overlay text while `loading`, e.g. "Reading version information…". */
  loadingText: string;
  error: string | null;
  filter: PropertyFilter;
  onFilterChange: (f: PropertyFilter) => void;
  onRefresh: () => void;
  /** The session's path inputs, shown under the toolbar. */
  pathBar: ReactNode;
  /** Optional strip between the path bar and the table (headline facts, thumbnails). */
  header?: ReactNode;
  leftTitle: string;
  rightTitle: string;
  /** Shown when there is nothing to compare yet. */
  emptyText: string;
}

function matches(r: PropertyRow, f: PropertyFilter): boolean {
  switch (f) {
    case "all":
      return true;
    case "diffs":
      return r.status !== "same";
    case "same":
      return r.status === "same";
  }
}

type Item = { group: string; rows: PropertyRow[]; diffs: number };

/**
 * Grouped "Property | Left | Right" table shared by the Version and MP3
 * Compare sessions, with toolbar filter and status-bar counts.
 */
export function PropertyCompare({
  result,
  loading,
  loadingText,
  error,
  filter,
  onFilterChange,
  onRefresh,
  pathBar,
  header,
  leftTitle,
  rightTitle,
  emptyText,
}: Props) {
  const root = useRef<HTMLDivElement>(null);
  const [collapsed, setCollapsed] = useState<Set<string>>(new Set());

  const groups = useMemo(() => {
    const out: Item[] = [];
    for (const r of result?.rows ?? []) {
      if (!matches(r, filter)) continue;
      let g = out[out.length - 1];
      if (!g || g.group !== r.group) out.push((g = { group: r.group, rows: [], diffs: 0 }));
      g.rows.push(r);
      if (r.status !== "same") g.diffs++;
    }
    return out;
  }, [result, filter]);

  // The refresh key (F5 by default) only acts on the visible tab (hidden tab panels have no layout).
  const refresh = useRef(onRefresh);
  refresh.current = onRefresh;
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (!matchesKey(e, "property.refresh") || !root.current?.offsetParent) return;
      e.preventDefault();
      refresh.current();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  const toggle = (group: string) =>
    setCollapsed((prev) => {
      const next = new Set(prev);
      next.has(group) ? next.delete(group) : next.add(group);
      return next;
    });

  const s = result?.stats;
  return (
    <div className="session" ref={root}>
      <div className="toolbar">
        <div className="seg-control">
          {(
            [
              ["all", "All"],
              ["diffs", "Differences"],
              ["same", "Same"],
            ] as [PropertyFilter, string][]
          ).map(([f, label]) => (
            <button key={f} className={filter === f ? "active" : ""} onClick={() => onFilterChange(f)}>
              {label}
            </button>
          ))}
        </div>
        <span className="spacer" />
        <button onClick={onRefresh} title={`Refresh (${shortcutLabel("property.refresh")})`}>
          <RefreshCw size={14} /> Refresh
        </button>
      </div>

      {pathBar}
      {header}

      <div className="session-body">
        {loading && <BusyOverlay label={loadingText} />}
        {error && <div className="banner error">{error}</div>}
        {!result && !error && !loading && <div className="empty-state">{emptyText}</div>}
        {result && (
          <div className="prop-table">
            <div className="prop-row prop-head">
              <div>Property</div>
              <div title={leftTitle}>{leftTitle}</div>
              <div title={rightTitle}>{rightTitle}</div>
            </div>
            {groups.length === 0 && <div className="empty-state prop-empty">Nothing to show for this filter.</div>}
            {groups.map((g) => {
              const open = !collapsed.has(g.group);
              return (
                <div key={g.group} className="prop-group">
                  <div className="prop-group-head" onClick={() => toggle(g.group)}>
                    <span className="chevron">{open ? <ChevronDown size={13} /> : <ChevronRight size={13} />}</span>
                    {g.group}
                    <span className="muted prop-group-count">
                      {g.rows.length} {g.rows.length === 1 ? "property" : "properties"}
                      {g.diffs > 0 && <span className="s-different-fg">, {g.diffs} differ</span>}
                    </span>
                  </div>
                  {open &&
                    g.rows.map((r) => {
                      const Icon = STATUS_ICON[r.status];
                      return (
                        <div key={r.name} className={`prop-row p-${r.status}`}>
                          <div className="prop-name" title={r.name}>
                            <Icon size={12} className="prop-icon" />
                            {r.name}
                          </div>
                          <div className={`prop-val${r.left === null ? " absent" : ""}`}>{r.left}</div>
                          <div className={`prop-val${r.right === null ? " absent" : ""}`}>{r.right}</div>
                        </div>
                      );
                    })}
                </div>
              );
            })}
          </div>
        )}
      </div>

      <div className="statusbar">
        {s && (
          <>
            <span className="s-same-fg">{s.same} same</span>
            <span className="s-different-fg">{s.different} different</span>
            <span className="s-leftOnly-fg">{s.leftOnly} left only</span>
            <span className="s-rightOnly-fg">{s.rightOnly} right only</span>
            <span className="spacer" />
            {s.different + s.leftOnly + s.rightOnly === 0 && <span className="ok">Identical properties</span>}
          </>
        )}
      </div>
    </div>
  );
}
