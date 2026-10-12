import type { DialogFilter } from "@tauri-apps/plugin-dialog";
import { ArrowDown, ArrowUp, Columns3, EyeOff, KeyRound, RefreshCw, TriangleAlert } from "lucide-react";
import {
  useCallback,
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
  type MouseEvent as ReactMouseEvent,
  type WheelEvent,
} from "react";
import { errorMessage } from "../lib/api";
import type { CompareProps } from "../lib/sessionProps";
import {
  tableApi,
  type ColumnInfo,
  type TableCompareOptions,
  type TableCompareResult,
  type TableRow,
  type TableSource,
} from "../lib/tableApi";
import { Modal } from "./Modal";
import { PathBar } from "./PathBar";
import { VirtualList, type VirtualListHandle } from "./VirtualList";
import "./TableCompare.css";

const ROW_HEIGHT = 22;
const CHAR_WIDTH = 7.4;
const MIN_COL = 56;
const MAX_COL = 360;
/** Rows sampled when sizing columns, so huge tables don't stall rendering. */
const WIDTH_SAMPLE = 400;

type Filter = "all" | "diffs" | "same" | "orphans";
type Side = "left" | "right";

/** What a saved table session remembers besides its paths. */
export interface TableSettings {
  options: TableCompareOptions;
  filter: Filter;
}

const DEFAULT_OPTIONS: TableCompareOptions = {
  hasHeader: true,
  keyColumns: [],
  ignoredColumns: [],
  ignoreCase: false,
  trimWhitespace: true,
  numericTolerance: null,
  delimiter: null,
  leftSheet: null,
  rightSheet: null,
};

const FILE_FILTERS: DialogFilter[] = [
  { name: "Delimited text", extensions: ["csv", "tsv", "tab", "txt"] },
  { name: "Excel / OpenDocument", extensions: ["xlsx", "xlsm", "xlsb", "xls", "ods"] },
  { name: "HTML", extensions: ["html", "htm", "xhtml"] },
  { name: "All files", extensions: ["*"] },
];

const DELIMITERS: [string, string][] = [
  ["", "Auto"],
  [",", "Comma"],
  [";", "Semicolon"],
  ["\t", "Tab"],
  ["|", "Pipe"],
];

const isImportant = (r: TableRow) => r.status === "different" || r.status === "leftOnly" || r.status === "rightOnly";

function matches(r: TableRow, f: Filter): boolean {
  switch (f) {
    case "all":
      return true;
    case "diffs":
      return isImportant(r);
    case "same":
      return r.status === "same" || r.status === "unimportant";
    case "orphans":
      return r.status === "leftOnly" || r.status === "rightOnly";
  }
}

/** Spreadsheet-style column letters: 0 → A, 26 → AA. */
function columnLetter(i: number): string {
  let s = "";
  for (let n = i + 1; n > 0; n = Math.floor((n - 1) / 26)) s = String.fromCharCode(65 + ((n - 1) % 26)) + s;
  return s;
}

function columnLabel(c: ColumnInfo, side: Side): string {
  const name = side === "left" ? c.leftName : c.rightName;
  const index = side === "left" ? c.left : c.right;
  if (index === null) return "";
  return name || columnLetter(index);
}

const cellText = (s: string) => s.replace(/\r?\n/g, " ↵ ");

function sourceSummary(s: TableSource): string {
  const kind =
    s.format === "workbook"
      ? "Workbook"
      : s.format === "html"
        ? "HTML table"
        : `Delimited (${s.delimiter === "\t" ? "Tab" : s.delimiter ?? "?"})`;
  const enc = s.encoding ? ` · ${s.encoding}` : "";
  return `${kind}${enc} · ${s.rowCount.toLocaleString()} × ${s.columnCount} cells`;
}

/** Index of the last offset not greater than `x` (offsets ascending, offsets[0] = 0). */
function lastAtOrBefore(offsets: number[], x: number): number {
  let lo = 0;
  let hi = offsets.length - 1;
  while (lo < hi) {
    const mid = (lo + hi + 1) >> 1;
    if (offsets[mid] <= x) lo = mid;
    else hi = mid - 1;
  }
  return lo;
}

interface MenuState {
  col: number;
  x: number;
  y: number;
}

export function TableCompare({ left, right, onPathsChange, settings, onSettingsChange }: CompareProps<TableSettings>) {
  const [options, setOptions] = useState<TableCompareOptions>({ ...DEFAULT_OPTIONS, ...settings?.options });
  const [filter, setFilter] = useState<Filter>(settings?.filter ?? "all");
  const reportSettings = useRef(onSettingsChange);
  reportSettings.current = onSettingsChange;
  useEffect(() => reportSettings.current?.({ options, filter }), [options, filter]);

  const [result, setResult] = useState<TableCompareResult | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const [diffIdx, setDiffIdx] = useState(-1);
  const [hx, setHx] = useState(0);
  const [viewW, setViewW] = useState(400);
  const [menu, setMenu] = useState<MenuState | null>(null);
  const [showColumns, setShowColumns] = useState(false);
  const [tolText, setTolText] = useState(String(options.numericTolerance ?? 0));
  const list = useRef<VirtualListHandle>(null);
  const leftBar = useRef<HTMLDivElement>(null);
  const rightBar = useRef<HTMLDivElement>(null);

  // Column indices and sheet choices belong to the previous files; drop them
  // when the user points the session at different files (not on first mount,
  // which restores a saved session).
  const prevPaths = useRef(`${left}\n${right}`);
  useEffect(() => {
    const key = `${left}\n${right}`;
    if (prevPaths.current === key) return;
    prevPaths.current = key;
    setOptions((o) => ({ ...o, keyColumns: [], ignoredColumns: [], leftSheet: null, rightSheet: null }));
  }, [left, right]);

  // Changing paths also resets options, so two requests can overlap; only the
  // latest one may update state.
  const requestId = useRef(0);
  const run = useCallback(async () => {
    if (!left || !right) return;
    const id = ++requestId.current;
    setLoading(true);
    setError(null);
    try {
      const res = await tableApi.compareTables(left, right, options);
      if (id !== requestId.current) return;
      setResult(res);
      setDiffIdx(-1);
    } catch (e) {
      if (id !== requestId.current) return;
      setError(errorMessage(e));
      setResult(null);
    } finally {
      if (id === requestId.current) setLoading(false);
    }
  }, [left, right, options]);

  useEffect(() => {
    run();
  }, [run]);

  const rows = result?.rows ?? [];
  const columns = result?.columns ?? [];
  const display = useMemo(() => {
    const out: number[] = [];
    rows.forEach((r, i) => matches(r, filter) && out.push(i));
    return out;
  }, [rows, filter]);

  const blocks = useMemo(() => {
    const out: { start: number; end: number }[] = [];
    display.forEach((ri, i) => {
      if (!isImportant(rows[ri])) return;
      const last = out[out.length - 1];
      if (last && last.end === i) last.end = i + 1;
      else out.push({ start: i, end: i + 1 });
    });
    return out;
  }, [display, rows]);

  const widths = useMemo(() => {
    const chars = columns.map((c) =>
      Math.max(columnLabel(c, "left").length, columnLabel(c, "right").length) + (c.key ? 3 : 0),
    );
    const step = Math.max(1, Math.floor(rows.length / WIDTH_SAMPLE));
    for (let i = 0; i < rows.length; i += step) {
      for (const cells of [rows[i].left, rows[i].right]) {
        cells?.forEach((t, c) => (chars[c] = Math.max(chars[c], t.length)));
      }
    }
    return chars.map((n) => Math.round(Math.min(MAX_COL, Math.max(MIN_COL, n * CHAR_WIDTH + 14))));
  }, [columns, rows]);

  const offsets = useMemo(() => {
    const out = [0];
    for (const w of widths) out.push(out[out.length - 1] + w);
    return out;
  }, [widths]);
  const totalWidth = offsets[offsets.length - 1];
  const maxHx = Math.max(0, totalWidth - viewW);

  useEffect(() => setHx((x) => Math.min(x, maxHx)), [maxHx]);

  const hasGrid = result !== null;
  useLayoutEffect(() => {
    const el = leftBar.current;
    if (!el) return;
    const ro = new ResizeObserver(() => setViewW(el.clientWidth));
    ro.observe(el);
    setViewW(el.clientWidth);
    return () => ro.disconnect();
  }, [hasGrid]);

  useLayoutEffect(() => {
    for (const el of [leftBar.current, rightBar.current]) {
      if (el && Math.abs(el.scrollLeft - hx) > 0.5) el.scrollLeft = hx;
    }
  }, [hx]);

  const goDiff = useCallback(
    (idx: number) => {
      if (!blocks.length) return;
      const clamped = Math.max(0, Math.min(blocks.length - 1, idx));
      setDiffIdx(clamped);
      list.current?.scrollToRow(blocks[clamped].start);
    },
    [blocks],
  );

  useEffect(() => setDiffIdx(-1), [filter]);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.altKey && e.key === "ArrowDown") goDiff(diffIdx + 1);
      else if (e.altKey && e.key === "ArrowUp") goDiff(diffIdx - 1);
      else if (e.key === "F5") run();
      else return;
      e.preventDefault();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [goDiff, diffIdx, run]);

  useEffect(() => {
    if (!menu) return;
    const close = () => setMenu(null);
    const onKey = (e: KeyboardEvent) => e.key === "Escape" && close();
    window.addEventListener("mousedown", close);
    window.addEventListener("keydown", onKey);
    window.addEventListener("resize", close);
    return () => {
      window.removeEventListener("mousedown", close);
      window.removeEventListener("keydown", onKey);
      window.removeEventListener("resize", close);
    };
  }, [menu]);

  const set = (patch: Partial<TableCompareOptions>) => setOptions((o) => ({ ...o, ...patch }));

  const toggleKey = (col: number) =>
    setOptions((o) =>
      o.keyColumns.includes(col)
        ? { ...o, keyColumns: o.keyColumns.filter((c) => c !== col) }
        : { ...o, keyColumns: [...o.keyColumns, col], ignoredColumns: o.ignoredColumns.filter((c) => c !== col) },
    );

  const toggleIgnored = (col: number) =>
    setOptions((o) =>
      o.ignoredColumns.includes(col)
        ? { ...o, ignoredColumns: o.ignoredColumns.filter((c) => c !== col) }
        : { ...o, ignoredColumns: [...o.ignoredColumns, col], keyColumns: o.keyColumns.filter((c) => c !== col) },
    );

  const commitTolerance = () => {
    const v = Number(tolText);
    if (Number.isFinite(v) && v >= 0) {
      if (options.numericTolerance !== null && v !== options.numericTolerance) set({ numericTolerance: v });
    } else {
      setTolText(String(options.numericTolerance ?? 0));
    }
  };

  const onWheel = (e: WheelEvent<HTMLDivElement>) => {
    const dx = e.shiftKey ? e.deltaY : e.deltaX;
    if (!dx || (!e.shiftKey && Math.abs(e.deltaX) < Math.abs(e.deltaY))) return;
    setHx((x) => Math.max(0, Math.min(maxHx, x + dx)));
  };

  const openMenu = (e: ReactMouseEvent, col: number) => {
    e.preventDefault();
    e.stopPropagation();
    setMenu({ col, x: e.clientX, y: e.clientY });
  };

  // Only the columns intersecting the viewport are rendered.
  const firstCol = columns.length ? lastAtOrBefore(offsets, hx) : 0;
  let lastCol = firstCol;
  while (lastCol < columns.length && offsets[lastCol] < hx + viewW) lastCol++;
  const visibleCols: number[] = [];
  for (let c = firstCol; c < lastCol; c++) visibleCols.push(c);
  const shift = { transform: `translateX(${offsets[firstCol] - hx}px)` };

  const renderHeader = (side: Side) => (
    <div className="tbl-half">
      <div className="tbl-strip" style={shift}>
        {visibleCols.map((c) => {
          const col = columns[c];
          const present = (side === "left" ? col.left : col.right) !== null;
          const renamed = col.leftName !== null && col.rightName !== null && col.leftName !== col.rightName;
          const cls = `tbl-hcell${present ? "" : " na"}${col.key ? " key" : ""}${col.ignored ? " ign" : ""}${
            renamed ? " renamed" : ""
          }`;
          const label = columnLabel(col, side);
          return (
            <div
              key={c}
              className={cls}
              style={{ width: widths[c] }}
              title={`${label || "(missing)"} — click for column options`}
              onClick={(e) => openMenu(e, c)}
              onContextMenu={(e) => openMenu(e, c)}
            >
              {col.key && <KeyRound size={11} className="tbl-key-icon" />}
              {col.ignored && <EyeOff size={11} />}
              <span className="tbl-hlabel">{label}</span>
            </div>
          );
        })}
      </div>
    </div>
  );

  const renderCells = (row: TableRow, side: Side) => {
    const cells = side === "left" ? row.left : row.right;
    return (
      <div className={`tbl-half${cells ? "" : " absent"}`}>
        {cells && (
          <div className="tbl-strip" style={shift}>
            {visibleCols.map((c) => {
              const col = columns[c];
              const present = (side === "left" ? col.left : col.right) !== null;
              const differs = row.diffs.includes(c);
              const cls = `tbl-cell${present ? "" : " na"}${differs ? " diff" : ""}${col.ignored ? " ign" : ""}${
                col.key ? " key" : ""
              }`;
              const text = cells[c];
              return (
                <div key={c} className={cls} style={{ width: widths[c] }} title={text.length > 8 ? text : undefined}>
                  {cellText(text)}
                </div>
              );
            })}
          </div>
        )}
      </div>
    );
  };

  const renderRow = (i: number) => {
    const row = rows[display[i]];
    const block = diffIdx >= 0 ? blocks[diffIdx] : null;
    const current = block !== null && i >= block.start && i < block.end;
    return (
      <div className={`tbl-row s-${row.status}${current ? " current" : ""}`}>
        <span className="ln">{row.leftRow ?? ""}</span>
        {renderCells(row, "left")}
        <span className="mid" />
        <span className="ln">{row.rightRow ?? ""}</span>
        {renderCells(row, "right")}
      </div>
    );
  };

  const sheetPicker = (side: Side) => {
    const src = result?.[side];
    if (!src || src.sheets.length < 2) return null;
    return (
      <select
        className="tbl-sheet"
        value={src.sheet}
        onChange={(e) =>
          set(side === "left" ? { leftSheet: Number(e.target.value) } : { rightSheet: Number(e.target.value) })
        }
        title={src.format === "html" ? "Table" : "Worksheet"}
      >
        {src.sheets.map((name, i) => (
          <option key={i} value={i}>
            {name}
          </option>
        ))}
      </select>
    );
  };

  const s = result?.stats;
  const menuCol = menu ? columns[menu.col] : null;
  return (
    <div className="session">
      <div className="toolbar">
        <button onClick={() => goDiff(diffIdx - 1)} disabled={!blocks.length} title="Previous difference (Alt+↑)">
          <ArrowUp size={14} /> Prev
        </button>
        <button onClick={() => goDiff(diffIdx + 1)} disabled={!blocks.length} title="Next difference (Alt+↓)">
          <ArrowDown size={14} /> Next
        </button>
        <span className="sep" />
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
        <label className="check">
          <input
            type="checkbox"
            checked={options.hasHeader}
            onChange={(e) => set({ hasHeader: e.target.checked, keyColumns: [], ignoredColumns: [] })}
          />
          Has header
        </label>
        <label className="check">
          <input type="checkbox" checked={options.ignoreCase} onChange={(e) => set({ ignoreCase: e.target.checked })} />
          Ignore case
        </label>
        <label className="check">
          <input
            type="checkbox"
            checked={options.trimWhitespace}
            onChange={(e) => set({ trimWhitespace: e.target.checked })}
          />
          Trim whitespace
        </label>
        <label className="check" title="Compare cells that are both numbers by value, within the tolerance">
          <input
            type="checkbox"
            checked={options.numericTolerance !== null}
            onChange={(e) => {
              const v = Number(tolText);
              set({ numericTolerance: e.target.checked ? (Number.isFinite(v) && v >= 0 ? v : 0) : null });
            }}
          />
          Numeric ±
        </label>
        <input
          className="tbl-tolerance"
          type="number"
          min={0}
          step="any"
          value={tolText}
          disabled={options.numericTolerance === null}
          onChange={(e) => setTolText(e.target.value)}
          onBlur={commitTolerance}
          onKeyDown={(e) => e.key === "Enter" && commitTolerance()}
          aria-label="Numeric tolerance"
        />
        <label>
          Delimiter
          <select value={options.delimiter ?? ""} onChange={(e) => set({ delimiter: e.target.value || null })}>
            {DELIMITERS.map(([v, label]) => (
              <option key={label} value={v}>
                {label}
              </option>
            ))}
          </select>
        </label>
        <span className="sep" />
        <button onClick={() => setShowColumns(true)} disabled={!columns.length} title="Key and ignored columns">
          <Columns3 size={14} /> Columns
        </button>
        <span className="spacer" />
        <button onClick={run} title="Refresh (F5)">
          <RefreshCw size={14} /> Refresh
        </button>
      </div>

      <PathBar left={left} right={right} directory={false} onCompare={onPathsChange} filters={FILE_FILTERS} />

      {result && (
        <div className="infobar tbl-info">
          {(["left", "right"] as Side[]).map((side) => (
            <span key={side} className="tbl-info-side">
              {sheetPicker(side)}
              <span>{sourceSummary(result[side])}</span>
              {result[side].truncated && (
                <span className="tbl-warn" title="Only the first rows were loaded">
                  <TriangleAlert size={12} /> truncated
                </span>
              )}
            </span>
          ))}
        </div>
      )}

      <div className="session-body">
        {loading && <div className="overlay">Comparing…</div>}
        {error && <div className="banner error">{error}</div>}
        {result && (
          <div className="tbl-grid" onWheel={onWheel}>
            <div className="tbl-row tbl-head">
              <span className="ln" />
              {renderHeader("left")}
              <span className="mid" />
              <span className="ln" />
              {renderHeader("right")}
            </div>
            {display.length === 0 ? (
              <div className="empty-state">
                {rows.length === 0 ? "Both tables are empty." : "Nothing to show for this filter."}
              </div>
            ) : (
              <VirtualList
                ref={list}
                className="tbl-list"
                count={display.length}
                rowHeight={ROW_HEIGHT}
                renderRow={renderRow}
              />
            )}
            <div className="tbl-row tbl-hbar">
              <span />
              <div ref={leftBar} className="tbl-hscroll" onScroll={(e) => setHx(e.currentTarget.scrollLeft)}>
                <div style={{ width: totalWidth }} />
              </div>
              <span />
              <span />
              <div ref={rightBar} className="tbl-hscroll" onScroll={(e) => setHx(e.currentTarget.scrollLeft)}>
                <div style={{ width: totalWidth }} />
              </div>
            </div>
          </div>
        )}
      </div>

      <div className="statusbar">
        {s &&
          (result!.identical && s.unimportant === 0 ? (
            <span className="ok">Tables are identical ({s.same.toLocaleString()} rows)</span>
          ) : (
            <>
              <span>
                {blocks.length} difference{blocks.length === 1 ? "" : "s"}
                {diffIdx >= 0 ? ` (at ${diffIdx + 1})` : ""}
              </span>
              <span className="tbl-same-fg">{s.same.toLocaleString()} same</span>
              <span className="k-changed-fg">{s.different.toLocaleString()} different</span>
              <span className="k-leftOnly-fg">{s.leftOnly.toLocaleString()} left only</span>
              <span className="k-rightOnly-fg">{s.rightOnly.toLocaleString()} right only</span>
              {s.unimportant > 0 && (
                <span className="k-unimportant-fg">{s.unimportant.toLocaleString()} unimportant</span>
              )}
              <span className="muted">
                {s.differentCells.toLocaleString()} differing cell{s.differentCells === 1 ? "" : "s"}
              </span>
            </>
          ))}
        {result?.truncated && (
          <>
            <span className="spacer" />
            <span className="tbl-warn">
              <TriangleAlert size={12} /> Large table truncated
            </span>
          </>
        )}
      </div>

      {menu && menuCol && (
        <div
          className="tbl-menu"
          style={{ left: Math.min(menu.x, window.innerWidth - 200), top: Math.min(menu.y, window.innerHeight - 130) }}
          onMouseDown={(e) => e.stopPropagation()}
          role="menu"
        >
          <div className="tbl-menu-title">{columnLabel(menuCol, "left") || columnLabel(menuCol, "right")}</div>
          <button
            role="menuitemcheckbox"
            aria-checked={menuCol.key}
            disabled={menuCol.left === null || menuCol.right === null}
            title={menuCol.left === null || menuCol.right === null ? "Key columns must exist on both sides" : undefined}
            onClick={() => {
              toggleKey(menu.col);
              setMenu(null);
            }}
          >
            <KeyRound size={13} /> {menuCol.key ? "Remove key" : "Use as key"}
          </button>
          <button
            role="menuitemcheckbox"
            aria-checked={menuCol.ignored}
            onClick={() => {
              toggleIgnored(menu.col);
              setMenu(null);
            }}
          >
            <EyeOff size={13} /> {menuCol.ignored ? "Compare column" : "Ignore column"}
          </button>
          <button
            onClick={() => {
              setShowColumns(true);
              setMenu(null);
            }}
          >
            <Columns3 size={13} /> All columns…
          </button>
        </div>
      )}

      {showColumns && (
        <Modal
          title="Columns"
          wide
          onClose={() => setShowColumns(false)}
          footer={
            <>
              <button onClick={() => set({ keyColumns: [] })} disabled={!options.keyColumns.length}>
                Clear keys
              </button>
              <button onClick={() => set({ ignoredColumns: [] })} disabled={!options.ignoredColumns.length}>
                Clear ignored
              </button>
              <span className="spacer" />
              <button className="primary" onClick={() => setShowColumns(false)}>
                Done
              </button>
            </>
          }
        >
          <p className="muted tbl-columns-hint">
            Rows are sorted and matched on key columns; without keys they are aligned in file order. Differences in
            ignored columns are shown but count as unimportant.
          </p>
          <div className="tbl-columns">
            <div className="tbl-columns-head">
              <span>Left</span>
              <span>Right</span>
              <span>Key</span>
              <span>Ignore</span>
            </div>
            {columns.map((c, i) => (
              <div key={i} className={`tbl-columns-row${c.ignored ? " ign" : ""}`}>
                <span className={c.left === null ? "muted" : ""}>{columnLabel(c, "left") || "(missing)"}</span>
                <span className={c.right === null ? "muted" : ""}>{columnLabel(c, "right") || "(missing)"}</span>
                <input
                  type="checkbox"
                  checked={options.keyColumns.includes(i)}
                  disabled={c.left === null || c.right === null}
                  onChange={() => toggleKey(i)}
                  aria-label="Key column"
                />
                <input
                  type="checkbox"
                  checked={options.ignoredColumns.includes(i)}
                  onChange={() => toggleIgnored(i)}
                  aria-label="Ignore column"
                />
              </div>
            ))}
          </div>
        </Modal>
      )}
    </div>
  );
}
