import type { DialogFilter } from "@tauri-apps/plugin-dialog";
import { embedInactive } from "../lib/embeddedPane"; // araxis:folder
import {
  ArrowDown,
  ArrowDownWideNarrow,
  ArrowLeftRight,
  ArrowUp,
  Columns3,
  Eye,
  EyeOff,
  KeyRound,
  Layers,
  RefreshCw,
  TriangleAlert,
} from "lucide-react";
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
  type ColumnPair,
  type SheetColumns,
  type SheetComparison,
  type SheetsCompareResult,
  type TableCompareOptions,
  type TableCompareResult,
  type TableRow,
  type TableSource,
} from "../lib/tableApi";
import { Modal } from "./Modal";
import { columnLetter, TableMappingDialog } from "./TableMappingDialog";
import { PathBar } from "./PathBar";
import { VirtualList, type VirtualListHandle } from "./VirtualList";
// bc5:reports-script — Report… (HTML grid or CSV)
import { FileText } from "lucide-react";
import { ReportDialog } from "./ReportDialog";
import "./TableCompare.css";

const ROW_HEIGHT = 22;
const CHAR_WIDTH = 7.4;
const MIN_COL = 56;
const MAX_COL = 360;
/** Rows sampled when sizing columns, so huge tables don't stall rendering. */
const WIDTH_SAMPLE = 400;

type Filter = "all" | "diffs" | "same" | "orphans";
type Side = "left" | "right";
type DateUnit = "s" | "min" | "h" | "d";

const DATE_UNITS: [DateUnit, string, number][] = [
  ["s", "seconds", 1],
  ["min", "minutes", 60],
  ["h", "hours", 3600],
  ["d", "days", 86400],
];
const unitSeconds = (u: DateUnit) => DATE_UNITS.find(([k]) => k === u)?.[2] ?? 1;

/** One sheet pair's column choices, plus its display-only hidden columns. */
type PairColumns = SheetColumns & { hiddenColumns: number[] };

/** What a saved table session remembers besides its paths. */
export interface TableSettings {
  options: TableCompareOptions;
  filter: Filter;
  /** Indices into the result columns that are not displayed (they are still compared). */
  hiddenColumns: number[];
  /** Unit the date tolerance is entered in. */
  dateUnit: DateUnit;
  /** Compare every worksheet / HTML table and list them with their difference counts. */
  allSheets: boolean;
  /** Column choices of the sheet pairs not currently shown (all-sheets mode). */
  sheetColumns: PairColumns[];
}

const DEFAULT_OPTIONS: TableCompareOptions = {
  hasHeader: true,
  leftHeader: null,
  rightHeader: null,
  columnMapping: null,
  sortRows: false,
  sortColumns: [],
  keyColumns: [],
  ignoredColumns: [],
  ignoreCase: false,
  trimWhitespace: true,
  numericTolerance: null,
  dateTolerance: null,
  delimiter: null,
  leftSheet: null,
  rightSheet: null,
};

const TABLE_EXTENSIONS = ["csv", "tsv", "tab", "txt", "xlsx", "xlsm", "xlsb", "xls", "ods", "html", "htm", "xhtml"];
const FILE_FILTERS: DialogFilter[] = [
  { name: "All tables", extensions: TABLE_EXTENSIONS },
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

function columnLabel(c: ColumnInfo, side: Side): string {
  const name = side === "left" ? c.leftName : c.rightName;
  const index = side === "left" ? c.left : c.right;
  if (index === null) return "";
  return name || columnLetter(index);
}

const cellText = (s: string) => s.replace(/\r?\n/g, " ↵ ");

const columnName = (c: ColumnInfo) => columnLabel(c, "left") || columnLabel(c, "right");

/** Toggles `v` in a list of indices. */
const toggled = (list: number[], v: number) => (list.includes(v) ? list.filter((x) => x !== v) : [...list, v]);

/** Whether a pair has any column choices worth remembering. */
const hasChoices = (c: PairColumns) =>
  c.keyColumns.length > 0 ||
  c.ignoredColumns.length > 0 ||
  c.sortColumns.length > 0 ||
  c.hiddenColumns.length > 0 ||
  c.columnMapping !== null;

/** The engine's view of a pair's column choices (hidden columns are UI-only). */
const engineColumns = ({ hiddenColumns: _, ...rest }: PairColumns): SheetColumns => rest;

function sheetStatus(s: SheetComparison): string {
  if (s.error) return "error";
  if (s.left === null) return "rightOnly";
  if (s.right === null) return "leftOnly";
  return s.identical ? "same" : "different";
}

function sheetBadge(s: SheetComparison): string {
  if (s.error) return "error";
  if (s.left === null) return "right only";
  if (s.right === null) return "left only";
  if (s.identical) return s.stats?.unimportant ? "unimportant" : "same";
  return s.differences.toLocaleString();
}

function sourceSummary(s: TableSource): string {
  const kind =
    s.format === "workbook"
      ? "Workbook"
      : s.format === "html"
        ? "HTML table"
        : `Delimited (${s.delimiter === "\t" ? "Tab" : (s.delimiter ?? "?")})`;
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
  const [hidden, setHidden] = useState<number[]>(settings?.hiddenColumns ?? []);
  const [dateUnit, setDateUnit] = useState<DateUnit>(settings?.dateUnit ?? "s");
  const [allSheets, setAllSheets] = useState(settings?.allSheets ?? false);
  const [sheetColumns, setSheetColumns] = useState<PairColumns[]>(settings?.sheetColumns ?? []);
  const reportSettings = useRef(onSettingsChange);
  reportSettings.current = onSettingsChange;
  useEffect(
    () => reportSettings.current?.({ options, filter, hiddenColumns: hidden, dateUnit, allSheets, sheetColumns }),
    [options, filter, hidden, dateUnit, allSheets, sheetColumns],
  );

  const [result, setResult] = useState<TableCompareResult | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const [diffIdx, setDiffIdx] = useState(-1);
  const [hx, setHx] = useState(0);
  const [viewW, setViewW] = useState(400);
  const [menu, setMenu] = useState<MenuState | null>(null);
  const [showColumns, setShowColumns] = useState(false);
  const [showMapping, setShowMapping] = useState(false);
  const [reporting, setReporting] = useState(false); // bc5:reports-script
  const [tolText, setTolText] = useState(String(options.numericTolerance ?? 0));
  const [dateText, setDateText] = useState(
    String(options.dateTolerance === null ? 0 : options.dateTolerance / unitSeconds(dateUnit)),
  );
  const [sheetsResult, setSheetsResult] = useState<SheetsCompareResult | null>(null);
  const [sheetsError, setSheetsError] = useState<string | null>(null);
  const [sheetsLoading, setSheetsLoading] = useState(false);
  const root = useRef<HTMLDivElement>(null);
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
    setOptions((o) => ({
      ...o,
      keyColumns: [],
      ignoredColumns: [],
      sortColumns: [],
      columnMapping: null,
      leftSheet: null,
      rightSheet: null,
    }));
    setHidden([]);
    setSheetColumns([]);
    setSheetsResult(null);
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

  // All-sheets mode: the current pair is whatever the options point at; its
  // column choices live in `options` / `hidden`, every other pair's in `sheetColumns`.
  const multiSheet = !!result && (result.left.format !== "delimited" || result.right.format !== "delimited");
  const curL = options.leftSheet ?? 0;
  const curR = options.rightSheet ?? 0;
  const isCurrent = (c: { leftSheet: number | null; rightSheet: number | null }) =>
    (c.leftSheet ?? 0) === curL && (c.rightSheet ?? 0) === curR;
  const currentColumns: PairColumns = {
    leftSheet: curL,
    rightSheet: curR,
    keyColumns: options.keyColumns,
    ignoredColumns: options.ignoredColumns,
    columnMapping: options.columnMapping,
    sortColumns: options.sortColumns,
    hiddenColumns: hidden,
  };

  const sheetsRequest = useRef(0);
  const runSheets = useCallback(async () => {
    if (!allSheets || !multiSheet || !left || !right) return;
    const id = ++sheetsRequest.current;
    setSheetsLoading(true);
    setSheetsError(null);
    const cur: SheetColumns = {
      leftSheet: options.leftSheet ?? 0,
      rightSheet: options.rightSheet ?? 0,
      keyColumns: options.keyColumns,
      ignoredColumns: options.ignoredColumns,
      columnMapping: options.columnMapping,
      sortColumns: options.sortColumns,
    };
    const others = sheetColumns
      .filter((c) => !(c.leftSheet === cur.leftSheet && c.rightSheet === cur.rightSheet))
      .map(engineColumns);
    try {
      const res = await tableApi.compareSheets(left, right, options, [...others, cur]);
      if (id === sheetsRequest.current) setSheetsResult(res);
    } catch (e) {
      if (id !== sheetsRequest.current) return;
      setSheetsError(errorMessage(e));
      setSheetsResult(null);
    } finally {
      if (id === sheetsRequest.current) setSheetsLoading(false);
    }
  }, [allSheets, multiSheet, left, right, options, sheetColumns]);

  useEffect(() => {
    runSheets();
  }, [runSheets]);

  /** Shows another sheet pair, parking the current pair's column choices. */
  const selectPair = (l: number, r: number) => {
    if (l === curL && r === curR) return;
    const others = sheetColumns.filter((c) => !isCurrent(c));
    const parked = hasChoices(currentColumns) ? [...others, currentColumns] : others;
    const mine = parked.find((c) => c.leftSheet === l && c.rightSheet === r);
    setSheetColumns(parked.filter((c) => c !== mine));
    setOptions((o) => ({
      ...o,
      leftSheet: l,
      rightSheet: r,
      keyColumns: mine?.keyColumns ?? [],
      ignoredColumns: mine?.ignoredColumns ?? [],
      columnMapping: mine?.columnMapping ?? null,
      sortColumns: mine?.sortColumns ?? [],
    }));
    setHidden(mine?.hiddenColumns ?? []);
  };

  // When the shown sheets aren't one of the pairs (e.g. All sheets was just
  // switched on and sheets pair by name), jump to the first pair.
  const selectPairRef = useRef(selectPair);
  selectPairRef.current = selectPair;
  useEffect(() => {
    if (!allSheets || !multiSheet || !sheetsResult) return;
    const paired = sheetsResult.sheets.filter((s) => s.left !== null && s.right !== null);
    if (!paired.length || paired.some((s) => s.left === curL && s.right === curR)) return;
    selectPairRef.current(paired[0].left!, paired[0].right!);
  }, [allSheets, multiSheet, sheetsResult, curL, curR]);

  const rows = result?.rows ?? [];
  const columns = result?.columns ?? [];
  /** Result-column indices that are displayed, in order. */
  const shown = useMemo(() => columns.map((_, i) => i).filter((i) => !hidden.includes(i)), [columns, hidden]);
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
    const chars = columns.map(
      (c) => Math.max(columnLabel(c, "left").length, columnLabel(c, "right").length) + (c.key ? 3 : 0),
    );
    const step = Math.max(1, Math.floor(rows.length / WIDTH_SAMPLE));
    for (let i = 0; i < rows.length; i += step) {
      for (const cells of [rows[i].left, rows[i].right]) {
        cells?.forEach((t, c) => (chars[c] = Math.max(chars[c], t.length)));
      }
    }
    return chars.map((n) => Math.round(Math.min(MAX_COL, Math.max(MIN_COL, n * CHAR_WIDTH + 14))));
  }, [columns, rows]);

  /** Left edge of each displayed column (by display position), plus the total width. */
  const offsets = useMemo(() => {
    const out = [0];
    for (const c of shown) out.push(out[out.length - 1] + widths[c]);
    return out;
  }, [widths, shown]);
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
      if (!root.current?.offsetParent) return; // hidden tab
      if (embedInactive(root.current)) return; // araxis:folder — split view: the folder list has the keys
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

  const toggleHidden = (col: number) => setHidden((h) => toggled(h, col));

  const toggleSort = (col: number) =>
    setOptions((o) => {
      // Ticking a column while sorting is off starts a sort on just that column;
      // unticking the last one stops sorting (rather than sorting on all columns).
      const sortColumns = o.sortRows ? toggled(o.sortColumns, col) : [col];
      return { ...o, sortRows: sortColumns.length > 0, sortColumns };
    });

  const { headers, lhead, rhead } = useMemo(() => {
    const lhead = options.leftHeader ?? options.hasHeader;
    const rhead = options.rightHeader ?? options.hasHeader;
    return { headers: lhead && rhead ? "on" : !lhead && !rhead ? "off" : "mixed", lhead, rhead };
  }, [options.hasHeader, options.leftHeader, options.rightHeader]);

  /** Header changes re-pair columns unless a manual mapping pins them. */
  const setHeaders = (patch: Partial<TableCompareOptions>) => {
    if (options.columnMapping) return set(patch);
    set({ ...patch, keyColumns: [], ignoredColumns: [], sortColumns: [] });
    setHidden([]);
  };

  const applyMapping = (mapping: ColumnPair[] | null) => {
    setShowMapping(false);
    if (mapping === null) {
      set({ columnMapping: null, keyColumns: [], ignoredColumns: [], sortColumns: [] });
      setHidden([]);
      return;
    }
    // Carry key / ignored / sort / hidden choices over to wherever their column lands.
    const translate = (idx: number[]) =>
      idx.flatMap((i) => {
        const c = columns[i];
        if (!c) return [];
        const j = mapping.findIndex((p) => (c.left !== null ? p.left === c.left : p.right === c.right));
        return j >= 0 ? [j] : [];
      });
    setOptions((o) => ({
      ...o,
      columnMapping: mapping,
      keyColumns: translate(o.keyColumns),
      ignoredColumns: translate(o.ignoredColumns),
      sortColumns: translate(o.sortColumns),
    }));
    setHidden((h) => translate(h));
  };

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

  const commitDate = (unit: DateUnit = dateUnit) => {
    const v = Number(dateText);
    if (Number.isFinite(v) && v >= 0) {
      const secs = v * unitSeconds(unit);
      if (options.dateTolerance !== null && secs !== options.dateTolerance) set({ dateTolerance: secs });
    } else {
      setDateText(String((options.dateTolerance ?? 0) / unitSeconds(unit)));
    }
  };

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

  // Only the displayed columns intersecting the viewport are rendered;
  // `visibleCols` holds their result-column indices.
  const firstCol = shown.length ? lastAtOrBefore(offsets, hx) : 0;
  let lastCol = firstCol;
  while (lastCol < shown.length && offsets[lastCol] < hx + viewW) lastCol++;
  const visibleCols: number[] = [];
  for (let d = firstCol; d < lastCol; d++) visibleCols.push(shown[d]);
  const shift = { transform: `translateX(${offsets[firstCol] - hx}px)` };
  const sortSet = new Set(result?.sortedBy ?? []);
  const sortAll = options.sortRows && options.sortColumns.length === 0;

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
              {sortSet.has(c) && !sortAll && <ArrowDownWideNarrow size={11} className="tbl-sort-icon" />}
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

  // Spell out how rows were lined up, since it decides what counts as a change.
  const keyCols = columns.flatMap((c, i) => (c.key ? [i] : []));
  const names = (idx: number[]) => idx.map((i) => columnName(columns[i])).join(", ");
  const sortedText = result?.sortedBy ? (sortAll ? "all columns" : names(result.sortedBy)) : null;
  const alignment = !result
    ? null
    : keyCols.length
      ? `Aligned by key: ${names(keyCols)}${sortedText ? ` · equal keys ordered by ${sortedText}` : ""}`
      : sortedText
        ? `Sorted by ${sortedText} · aligned by all columns`
        : "Aligned by all columns, in file order";

  const sheetWord = result?.left.format === "html" && result.right.format !== "workbook" ? "tables" : "sheets";
  const sheetDiffs = sheetsResult?.sheets.filter((x) => !x.identical).length ?? 0;

  return (
    <div className="session" ref={root}>
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
        <label
          className="check"
          title={
            headers === "mixed"
              ? `First row is a header on the ${lhead ? "left" : "right"} only (set per side in Column mapping)`
              : "First row holds column names"
          }
        >
          <input
            type="checkbox"
            checked={headers === "on"}
            ref={(el) => {
              if (el) el.indeterminate = headers === "mixed";
            }}
            onChange={(e) => setHeaders({ hasHeader: e.target.checked, leftHeader: null, rightHeader: null })}
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
        <label
          className="check"
          title="Compare cells that are both dates or times (2024-03-05 14:30, 3/5/2024, 05.03.2024, 5 Mar 2024, …) as moments, equal within the tolerance"
        >
          <input
            type="checkbox"
            checked={options.dateTolerance !== null}
            onChange={(e) => {
              const v = Number(dateText);
              set({
                dateTolerance: e.target.checked ? (Number.isFinite(v) && v >= 0 ? v : 0) * unitSeconds(dateUnit) : null,
              });
            }}
          />
          Date ±
        </label>
        <input
          className="tbl-tolerance"
          type="number"
          min={0}
          step="any"
          value={dateText}
          disabled={options.dateTolerance === null}
          onChange={(e) => setDateText(e.target.value)}
          onBlur={() => commitDate()}
          onKeyDown={(e) => e.key === "Enter" && commitDate()}
          aria-label="Date tolerance"
        />
        <select
          value={dateUnit}
          disabled={options.dateTolerance === null}
          onChange={(e) => {
            const unit = e.target.value as DateUnit;
            setDateUnit(unit);
            commitDate(unit);
          }}
          aria-label="Date tolerance unit"
        >
          {DATE_UNITS.map(([u, label]) => (
            <option key={u} value={u}>
              {label}
            </option>
          ))}
        </select>
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
        <label
          className="check"
          title="Sort both sides before aligning rows — on the columns ticked under Sort in Columns, or on all columns"
        >
          <input type="checkbox" checked={options.sortRows} onChange={(e) => set({ sortRows: e.target.checked })} />
          Sort rows
        </label>
        <button
          onClick={() => setShowColumns(true)}
          disabled={!columns.length}
          title="Key, ignored, sort and hidden columns"
        >
          <Columns3 size={14} /> Columns
        </button>
        <button
          className={options.columnMapping ? "active" : ""}
          onClick={() => setShowMapping(true)}
          disabled={!columns.length}
          title={`Pair, rearrange or unmatch columns${options.columnMapping ? " (manual mapping active)" : ""}`}
        >
          <ArrowLeftRight size={14} /> Mapping
        </button>
        {multiSheet && (
          <label
            className="check"
            title={`Compare every ${sheetWord === "tables" ? "table" : "worksheet"}, paired by name then position`}
          >
            <input type="checkbox" checked={allSheets} onChange={(e) => setAllSheets(e.target.checked)} />
            <Layers size={13} /> All {sheetWord}
          </label>
        )}
        <span className="spacer" />
        {/* bc5:reports-script */}
        <button
          onClick={() => setReporting(true)}
          disabled={!result}
          title="Report: an HTML grid with differing cells highlighted, or CSV; save, copy or print"
        >
          <FileText size={14} /> Report…
        </button>
        <button onClick={run} title="Refresh (F5)">
          <RefreshCw size={14} /> Refresh
        </button>
      </div>
      {/* bc5:reports-script — the engine recomputes the comparison with the view's options. */}
      {reporting && result && (
        <ReportDialog
          source={{ kind: "table", left, right, compare: options, filter, hiddenColumns: hidden }}
          onClose={() => setReporting(false)}
        />
      )}

      <PathBar left={left} right={right} directory={false} onCompare={onPathsChange} filters={FILE_FILTERS} />

      {result && (
        <div className="infobar tbl-info">
          {(["left", "right"] as Side[]).map((side) => (
            <span key={side} className="tbl-info-side">
              {!allSheets && sheetPicker(side)}
              {allSheets && result[side].sheets.length > 0 && (
                <span className="tbl-info-sheet">{result[side].sheets[result[side].sheet]}</span>
              )}
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
        <div className="tbl-main">
          {allSheets && multiSheet && (
            <div className="tbl-sheets" role="listbox" aria-label={`Compared ${sheetWord}`}>
              <div className="tbl-sheets-head">
                {sheetsResult
                  ? `${sheetsResult.sheets.length} pair${sheetsResult.sheets.length === 1 ? "" : "s"} · ${sheetDiffs} differ${
                      sheetDiffs === 1 ? "s" : ""
                    }`
                  : sheetsLoading
                    ? `Comparing ${sheetWord}…`
                    : ""}
              </div>
              {sheetsError && <div className="tbl-sheets-error">{sheetsError}</div>}
              {sheetsResult?.sheets.map((x, i) => {
                const paired = x.left !== null && x.right !== null;
                const active = paired && x.left === curL && x.right === curR;
                const label =
                  x.leftLabel !== null && x.rightLabel !== null && x.leftLabel !== x.rightLabel
                    ? `${x.leftLabel} ↔ ${x.rightLabel}`
                    : (x.leftLabel ?? x.rightLabel ?? "");
                return (
                  <button
                    key={i}
                    role="option"
                    aria-selected={active}
                    className={`tbl-sheet-item s-${sheetStatus(x)}${active ? " active" : ""}`}
                    disabled={!paired || !!x.error}
                    onClick={() => selectPair(x.left!, x.right!)}
                    title={
                      x.error ??
                      `${label}${paired && !x.byName ? " (paired by position)" : ""}${
                        x.stats
                          ? ` — ${x.stats.different} different, ${x.stats.leftOnly} left only, ${x.stats.rightOnly} right only`
                          : ""
                      }`
                    }
                  >
                    <span className="tbl-sheet-name">{label}</span>
                    <span className="tbl-sheet-count">{sheetBadge(x)}</span>
                  </button>
                );
              })}
            </div>
          )}
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
      </div>

      <div className="statusbar">
        {alignment && (
          <span className="tbl-align" title="How rows are lined up — set key columns or Sort rows to change it">
            {keyCols.length ? <KeyRound size={12} /> : options.sortRows ? <ArrowDownWideNarrow size={12} /> : null}
            {alignment}
          </span>
        )}
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
        {(hidden.length > 0 || (allSheets && multiSheet && sheetsResult)) && <span className="spacer" />}
        {allSheets && multiSheet && sheetsResult && (
          <span className={sheetsResult.identical ? "ok" : "muted"}>
            {sheetsResult.identical
              ? `All ${sheetWord} identical`
              : `${sheetDiffs} of ${sheetsResult.sheets.length} ${sheetWord} differ`}
          </span>
        )}
        {hidden.length > 0 && (
          <button className="link tbl-link" onClick={() => setHidden([])} title="Show hidden columns">
            <Eye size={12} /> {hidden.length} hidden column{hidden.length === 1 ? "" : "s"} · Show all
          </button>
        )}
        {result?.truncated && (
          <>
            {hidden.length === 0 && !(allSheets && multiSheet && sheetsResult) && <span className="spacer" />}
            <span className="tbl-warn">
              <TriangleAlert size={12} /> Large table truncated
            </span>
          </>
        )}
      </div>

      {menu && menuCol && (
        <div
          className="tbl-menu"
          style={{ left: Math.min(menu.x, window.innerWidth - 200), top: Math.min(menu.y, window.innerHeight - 230) }}
          onMouseDown={(e) => e.stopPropagation()}
          role="menu"
        >
          <div className="tbl-menu-title">{columnName(menuCol)}</div>
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
            role="menuitemcheckbox"
            aria-checked={options.sortRows && options.sortColumns.includes(menu.col)}
            onClick={() => {
              toggleSort(menu.col);
              setMenu(null);
            }}
          >
            <ArrowDownWideNarrow size={13} />{" "}
            {options.sortRows && options.sortColumns.includes(menu.col) ? "Stop sorting on column" : "Sort on column"}
          </button>
          <button
            onClick={() => {
              toggleHidden(menu.col);
              setMenu(null);
            }}
            title="Hide from view; the column is still compared"
          >
            <Eye size={13} /> Hide column
          </button>
          <div className="tbl-menu-sep" />
          <button
            onClick={() => {
              setShowMapping(true);
              setMenu(null);
            }}
          >
            <ArrowLeftRight size={13} /> Column mapping…
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
              <button onClick={() => setHidden([])} disabled={!hidden.length}>
                Show all
              </button>
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
            Rows are sorted and matched on key columns; without keys they are aligned on all columns, in file order or —
            with Sort ticked — after sorting both sides on the Sort columns (none ticked sorts on all). Differences in
            ignored columns are shown but count as unimportant. Hidden columns are only left out of the view.
          </p>
          <div className="tbl-columns">
            <div className="tbl-columns-head">
              <span>Left</span>
              <span>Right</span>
              <span>Key</span>
              <span>Ignore</span>
              <span>Sort</span>
              <span>Show</span>
            </div>
            {columns.map((c, i) => (
              <div
                key={i}
                className={`tbl-columns-row${c.ignored ? " ign" : ""}${hidden.includes(i) ? " hidden" : ""}`}
              >
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
                <input
                  type="checkbox"
                  checked={options.sortRows && options.sortColumns.includes(i)}
                  onChange={() => toggleSort(i)}
                  aria-label="Sort on column"
                />
                <input
                  type="checkbox"
                  checked={!hidden.includes(i)}
                  onChange={() => toggleHidden(i)}
                  aria-label="Show column"
                />
              </div>
            ))}
          </div>
        </Modal>
      )}

      {showMapping && (
        <TableMappingDialog
          columns={columns}
          manual={options.columnMapping !== null}
          leftHeader={lhead}
          rightHeader={rhead}
          onHeadersChange={(l, r) => setHeaders({ leftHeader: l, rightHeader: r })}
          onApply={applyMapping}
          onClose={() => setShowMapping(false)}
        />
      )}
    </div>
  );
}
