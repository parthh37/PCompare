import { ArrowDown, ArrowUp, Binary, RefreshCw } from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState, type WheelEvent } from "react";
import {
  api,
  errorMessage,
  type DiffRow,
  type Segment,
  type TextDiffOptions,
  type TextFileCompare,
  type TextFileInfo,
  type WhitespaceMode,
} from "../lib/api";
import { formatSize } from "../lib/format";
import { PathBar } from "./PathBar";
import { VirtualList, type VirtualListHandle } from "./VirtualList";

const ROW_HEIGHT = 20;
const CHAR_WIDTH = 7.8;
const CONTEXT_LINES = 3;

type ViewMode = "all" | "context" | "diffs";

/** What a saved text session remembers besides its paths. */
export interface TextSettings {
  options: TextDiffOptions;
  mode: ViewMode;
}
type DisplayItem = { row: number } | { gap: number };

interface Props {
  left: string;
  right: string;
  onPathsChange: (left: string, right: string) => void;
  onOpenHex: () => void;
  settings?: Partial<TextSettings>;
  onSettingsChange?: (s: TextSettings) => void;
}

const isImportant = (r: DiffRow) => r.kind !== "equal" && r.kind !== "unimportant";

function buildDisplay(rows: DiffRow[], mode: ViewMode): DisplayItem[] {
  if (mode === "all") return rows.map((_, row) => ({ row }));
  const ctx = mode === "context" ? CONTEXT_LINES : 0;
  const keep = new Uint8Array(rows.length);
  rows.forEach((r, i) => {
    if (!isImportant(r)) return;
    for (let j = Math.max(0, i - ctx); j <= Math.min(rows.length - 1, i + ctx); j++) keep[j] = 1;
  });
  const out: DisplayItem[] = [];
  let hidden = 0;
  for (let i = 0; i < rows.length; i++) {
    if (keep[i]) {
      if (hidden) out.push({ gap: hidden });
      hidden = 0;
      out.push({ row: i });
    } else hidden++;
  }
  if (hidden) out.push({ gap: hidden });
  return out;
}

function Segments({ segs }: { segs: Segment[] }) {
  return (
    <>
      {segs.map((s, i) =>
        s.changed ? (
          <mark key={i} className="seg-changed">
            {s.text}
          </mark>
        ) : (
          <span key={i}>{s.text}</span>
        ),
      )}
    </>
  );
}

function FileInfo({ info }: { info: TextFileInfo }) {
  return (
    <span className="fileinfo">
      {info.encoding}
      {info.hasBom ? " BOM" : ""} · {info.lineEnding.toUpperCase()} · {formatSize(info.size)}
    </span>
  );
}

export function TextCompare({ left, right, onPathsChange, onOpenHex, settings, onSettingsChange }: Props) {
  const [options, setOptions] = useState<TextDiffOptions>({
    whitespace: "compare",
    ignoreCase: false,
    algorithm: "patience",
    ...settings?.options,
  });
  const [mode, setMode] = useState<ViewMode>(settings?.mode ?? "all");
  const reportSettings = useRef(onSettingsChange);
  reportSettings.current = onSettingsChange;
  useEffect(() => reportSettings.current?.({ options, mode }), [options, mode]);
  const [result, setResult] = useState<TextFileCompare | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const [hunkIdx, setHunkIdx] = useState(-1);
  const [hx, setHx] = useState(0);
  const list = useRef<VirtualListHandle>(null);

  const run = useCallback(async () => {
    if (!left || !right) return;
    setLoading(true);
    setError(null);
    try {
      setResult(await api.compareTextFiles(left, right, options));
      setHunkIdx(-1);
    } catch (e) {
      setError(errorMessage(e));
      setResult(null);
    } finally {
      setLoading(false);
    }
  }, [left, right, options]);

  useEffect(() => {
    run();
  }, [run]);

  const diff = result?.diff ?? null;
  const rows = diff?.rows ?? [];
  const display = useMemo(() => buildDisplay(rows, mode), [rows, mode]);
  const rowToDisplay = useMemo(() => {
    const map = new Int32Array(rows.length).fill(-1);
    display.forEach((d, i) => "row" in d && (map[d.row] = i));
    return map;
  }, [display, rows.length]);
  const maxChars = useMemo(() => {
    let m = 0;
    for (const r of rows)
      for (const side of [r.left, r.right])
        if (side) m = Math.max(m, side.reduce((n, s) => n + s.text.length, 0));
    return m;
  }, [rows]);

  const hunks = diff?.hunks ?? [];
  const goHunk = useCallback(
    (idx: number) => {
      if (!hunks.length) return;
      const clamped = Math.max(0, Math.min(hunks.length - 1, idx));
      setHunkIdx(clamped);
      list.current?.scrollToRow(rowToDisplay[hunks[clamped].startRow]);
    },
    [hunks, rowToDisplay],
  );

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.altKey && e.key === "ArrowDown") goHunk(hunkIdx + 1);
      else if (e.altKey && e.key === "ArrowUp") goHunk(hunkIdx - 1);
      else if (e.key === "F5") run();
      else return;
      e.preventDefault();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [goHunk, hunkIdx, run]);

  const onWheel = (e: WheelEvent<HTMLDivElement>) => {
    const dx = e.shiftKey ? e.deltaY : e.deltaX;
    if (!dx || (!e.shiftKey && Math.abs(e.deltaX) < Math.abs(e.deltaY))) return;
    setHx((x) => Math.max(0, Math.min(maxChars * CHAR_WIDTH, x + dx)));
  };

  const shift = { transform: `translateX(${-hx}px)` };
  const renderRow = (i: number) => {
    const item = display[i];
    if ("gap" in item) {
      return <div className="tgap">⋯ {item.gap.toLocaleString()} identical lines hidden ⋯</div>;
    }
    const row = rows[item.row];
    const inHunk = hunkIdx >= 0 && item.row >= hunks[hunkIdx].startRow && item.row < hunks[hunkIdx].endRow;
    return (
      <div className={`trow k-${row.kind}${inHunk ? " current" : ""}`}>
        <span className="ln">{row.leftNo ?? ""}</span>
        <div className={`tcell${row.left ? "" : " absent"}`}>
          <div className="tcontent" style={shift}>
            {row.left && <Segments segs={row.left} />}
          </div>
        </div>
        <span className="mid" />
        <span className="ln">{row.rightNo ?? ""}</span>
        <div className={`tcell${row.right ? "" : " absent"}`}>
          <div className="tcontent" style={shift}>
            {row.right && <Segments segs={row.right} />}
          </div>
        </div>
      </div>
    );
  };

  const stats = diff?.stats;
  return (
    <div className="session">
      <div className="toolbar">
        <button onClick={() => goHunk(hunkIdx - 1)} disabled={!hunks.length} title="Previous difference (Alt+↑)">
          <ArrowUp size={14} /> Prev
        </button>
        <button onClick={() => goHunk(hunkIdx + 1)} disabled={!hunks.length} title="Next difference (Alt+↓)">
          <ArrowDown size={14} /> Next
        </button>
        <span className="sep" />
        <div className="seg-control">
          {(["all", "context", "diffs"] as ViewMode[]).map((m) => (
            <button key={m} className={mode === m ? "active" : ""} onClick={() => setMode(m)}>
              {m === "all" ? "All" : m === "context" ? "Context" : "Diffs"}
            </button>
          ))}
        </div>
        <span className="sep" />
        <label>
          Whitespace
          <select
            value={options.whitespace}
            onChange={(e) => setOptions({ ...options, whitespace: e.target.value as WhitespaceMode })}
          >
            <option value="compare">Compare</option>
            <option value="ignoreTrailing">Ignore trailing</option>
            <option value="ignoreChanges">Ignore changes</option>
            <option value="ignoreAll">Ignore all</option>
          </select>
        </label>
        <label className="check">
          <input
            type="checkbox"
            checked={options.ignoreCase}
            onChange={(e) => setOptions({ ...options, ignoreCase: e.target.checked })}
          />
          Ignore case
        </label>
        <label>
          Algorithm
          <select
            value={options.algorithm}
            onChange={(e) => setOptions({ ...options, algorithm: e.target.value as TextDiffOptions["algorithm"] })}
          >
            <option value="patience">Patience</option>
            <option value="myers">Myers</option>
          </select>
        </label>
        <span className="spacer" />
        <button onClick={run} title="Refresh (F5)">
          <RefreshCw size={14} /> Refresh
        </button>
      </div>

      <PathBar left={left} right={right} directory={false} onCompare={onPathsChange} />

      {result && (
        <div className="infobar">
          <FileInfo info={result.left} />
          <FileInfo info={result.right} />
        </div>
      )}

      <div className="session-body">
        {loading && <div className="overlay">Comparing…</div>}
        {error && <div className="banner error">{error}</div>}
        {result && !diff && (
          <div className="empty-state">
            <p>One or both files look binary.</p>
            <button className="primary" onClick={onOpenHex}>
              <Binary size={14} /> Open in Hex Compare
            </button>
          </div>
        )}
        {diff && (
          <div className="text-grid">
            <Overview rows={rows} hunks={hunks} onJump={(row) => list.current?.scrollToRow(rowToDisplay[row])} />
            <VirtualList
              ref={list}
              className="text-list"
              count={display.length}
              rowHeight={ROW_HEIGHT}
              renderRow={renderRow}
              onWheel={onWheel}
            />
          </div>
        )}
      </div>

      <div className="statusbar">
        {diff &&
          (diff.identical ? (
            <span className="ok">Files are identical</span>
          ) : (
            <>
              <span>
                {hunks.length} difference{hunks.length === 1 ? "" : "s"}
                {hunkIdx >= 0 ? ` (at ${hunkIdx + 1})` : ""}
              </span>
              <span className="k-changed-fg">{stats!.changed} changed</span>
              <span className="k-leftOnly-fg">{stats!.leftOnly} left only</span>
              <span className="k-rightOnly-fg">{stats!.rightOnly} right only</span>
              {stats!.unimportant > 0 && <span className="k-unimportant-fg">{stats!.unimportant} unimportant</span>}
            </>
          ))}
      </div>
    </div>
  );
}

/** Thumbnail strip showing where differences sit in the whole file; click to jump. */
function Overview({
  rows,
  hunks,
  onJump,
}: {
  rows: DiffRow[];
  hunks: { startRow: number; endRow: number }[];
  onJump: (row: number) => void;
}) {
  const total = Math.max(rows.length, 1);
  return (
    <div
      className="overview"
      onClick={(e) => {
        const rect = e.currentTarget.getBoundingClientRect();
        onJump(Math.floor(((e.clientY - rect.top) / rect.height) * total));
      }}
    >
      {hunks.map((h, i) => (
        <div
          key={i}
          className={`ov-mark k-${rows[h.startRow].kind}`}
          style={{
            top: `${(h.startRow / total) * 100}%`,
            height: `max(2px, ${((h.endRow - h.startRow) / total) * 100}%)`,
          }}
        />
      ))}
    </div>
  );
}
