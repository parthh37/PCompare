import {
  ArrowDown,
  ArrowLeft,
  ArrowRight,
  ArrowUp,
  Binary,
  ChevronLeft,
  ChevronRight,
  ListFilter,
  Lock,
  Redo2,
  RefreshCw,
  Save,
  Undo2,
} from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState, type WheelEvent } from "react";
import {
  DEFAULT_TEXT_OPTIONS,
  errorMessage,
  isRemote,
  type DiffRow,
  type Hunk,
  type Segment,
  type TextDiffOptions,
  type TextDiffResult,
  type TextFileInfo,
  type WhitespaceMode,
} from "../lib/api";
import { formatSize } from "../lib/format";
import { joinLines, mergeApi, type TextDocument, type TextFormat } from "../lib/mergeApi";
import { autoRefreshTitle, useAutoRefresh } from "../lib/useAutoRefresh";
import { Modal } from "./Modal";
import { PathBar } from "./PathBar";
import { VirtualList, type VirtualListHandle } from "./VirtualList";
import "./TextCompare.css";

const ROW_HEIGHT = 20;
const CHAR_WIDTH = 7.8;
const CONTEXT_LINES = 3;
const MAX_UNDO = 200;
const READ_ONLY_HINT = "Remote files are read-only; copy the file locally to edit it";

type ViewMode = "all" | "context" | "diffs";
type Side = "left" | "right";
const other = (s: Side): Side => (s === "left" ? "right" : "left");

/** What a saved text session remembers besides its paths. */
export interface TextSettings {
  options: TextDiffOptions;
  mode: ViewMode;
  autoRefresh: boolean;
}
type DisplayItem = { row: number } | { gap: number };

interface Props {
  left: string;
  right: string;
  onPathsChange: (left: string, right: string) => void;
  onOpenHex: () => void;
  settings?: Partial<TextSettings>;
  onSettingsChange?: (s: TextSettings) => void;
  onDirtyChange?: (dirty: boolean) => void;
}

/** One side's file as edited in memory. */
interface SideDoc {
  info: TextFileInfo;
  format: TextFormat;
  trailingNewline: boolean;
  binary: boolean;
  lines: string[];
  /** The `lines` array last loaded or saved; the side is dirty when `lines` is a different array. */
  saved: string[];
}
type Docs = Record<Side, SideDoc>;
type Snapshot = Record<Side, string[]>;
interface Editing {
  row: number;
  side: Side;
  value: string;
}

const toSide = (d: TextDocument): SideDoc => ({
  info: d.info,
  format: d.format,
  trailingNewline: d.trailingNewline,
  binary: d.binary,
  lines: d.lines,
  saved: d.lines,
});
const isDirty = (d: SideDoc) => d.lines !== d.saved;
const sameLines = (a: string[], b: string[]) => a.length === b.length && a.every((l, i) => l === b[i]);

/**
 * Folds a re-read file into an open side: unsaved edits are kept, unchanged
 * content keeps its array (so undo history stays valid), anything else is
 * replaced. Returns `null` when the side is unchanged.
 */
function reloadSide(cur: SideDoc, fresh: TextDocument): SideDoc | null {
  if (isDirty(cur)) return null;
  if (cur.binary === fresh.binary && sameLines(cur.lines, fresh.lines)) {
    return { ...cur, info: fresh.info, format: fresh.format, trailingNewline: fresh.trailingNewline };
  }
  return toSide(fresh);
}
const lineNo = (r: DiffRow, side: Side) => (side === "left" ? r.leftNo : r.rightNo);

/**
 * The 0-based line range a hunk covers on one side. When that side has no
 * lines in the hunk, the range is empty and sits right after the last line
 * shown above the hunk, i.e. where copied lines get inserted.
 */
function hunkSpan(rows: DiffRow[], h: Hunk, side: Side): { start: number; end: number } {
  let start = -1;
  let end = -1;
  for (let i = h.startRow; i < h.endRow; i++) {
    const n = lineNo(rows[i], side);
    if (n == null) continue;
    if (start < 0) start = n - 1;
    end = n;
  }
  if (start >= 0) return { start, end };
  for (let i = h.startRow - 1; i >= 0; i--) {
    const n = lineNo(rows[i], side);
    if (n != null) return { start: n, end: n };
  }
  return { start: 0, end: 0 };
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

function FileInfo({ doc, readOnly }: { doc: SideDoc; readOnly: boolean }) {
  const { info } = doc;
  return (
    <span className="fileinfo">
      {readOnly && (
        <span className="tc-readonly" title={READ_ONLY_HINT}>
          <Lock size={11} /> read-only ·{" "}
        </span>
      )}
      {isDirty(doc) && (
        <span className="tc-dirty" title="Unsaved changes (Ctrl+S to save)">
          <span className="tc-dirty-dot" /> modified ·{" "}
        </span>
      )}
      {doc.binary ? (
        <>binary · {formatSize(info.size)}</>
      ) : (
        <>
          {info.encoding}
          {info.hasBom ? " BOM" : ""} · {info.lineEnding.toUpperCase()} · {formatSize(info.size)}
        </>
      )}
    </span>
  );
}

export function TextCompare({ left, right, onPathsChange, onOpenHex, settings, onSettingsChange, onDirtyChange }: Props) {
  const [options, setOptions] = useState<TextDiffOptions>({ ...DEFAULT_TEXT_OPTIONS, ...settings?.options });
  const [mode, setMode] = useState<ViewMode>(settings?.mode ?? "all");
  const [autoRefresh, setAutoRefresh] = useState(settings?.autoRefresh ?? false);
  const [editingRules, setEditingRules] = useState(false);
  const reportSettings = useRef(onSettingsChange);
  reportSettings.current = onSettingsChange;
  useEffect(() => reportSettings.current?.({ options, mode, autoRefresh }), [options, mode, autoRefresh]);

  const [docs, setDocs] = useState<Docs | null>(null);
  const docsRef = useRef(docs);
  docsRef.current = docs;
  /** An auto-refresh found a file changed on disk but kept the unsaved edits of that side. */
  const [staleOnDisk, setStaleOnDisk] = useState(false);
  const [history, setHistory] = useState<{ undo: Snapshot[]; redo: Snapshot[] }>({ undo: [], redo: [] });
  /** The diff plus the exact line arrays it was computed from. */
  const [diffed, setDiffed] = useState<{ result: TextDiffResult; for: Snapshot } | null>(null);
  const [error, setError] = useState<string | null>(null);
  /** Diffing failed, e.g. because an ignore rule is not a valid regex. */
  const [diffError, setDiffError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const [saving, setSaving] = useState(false);
  const [hunkIdx, setHunkIdx] = useState(-1);
  const [hx, setHx] = useState(0);
  const [editing, setEditingState] = useState<Editing | null>(null);
  // Mirrors `editing` so blur/keyboard handlers see the latest value synchronously.
  const editingRef = useRef<Editing | null>(null);
  const setEditing = (e: Editing | null) => {
    editingRef.current = e;
    setEditingState(e);
  };
  const list = useRef<VirtualListHandle>(null);
  const root = useRef<HTMLDivElement>(null);

  const readOnly: Record<Side, boolean> = { left: isRemote(left), right: isRemote(right) };
  const dirty = !!docs && (isDirty(docs.left) || isDirty(docs.right));
  const reportDirty = useRef(onDirtyChange);
  reportDirty.current = onDirtyChange;
  useEffect(() => reportDirty.current?.(dirty), [dirty]);

  const requestSeq = useRef(0);
  /**
   * Reads both files. `quiet` reloads (auto-refresh) keep the view, the
   * current difference and any unsaved edits instead of resetting.
   */
  const load = useCallback(
    async (quiet = false) => {
      if (!left || !right) return;
      const seq = ++requestSeq.current;
      if (!quiet) setLoading(true);
      try {
        const pair = await mergeApi.loadTextPair(left, right);
        if (seq !== requestSeq.current) return;
        setError(null);
        const cur = docsRef.current;
        if (quiet && cur) {
          const l = reloadSide(cur.left, pair.left);
          const r = reloadSide(cur.right, pair.right);
          const kept = (side: SideDoc, fresh: TextDocument) => isDirty(side) && !sameLines(side.saved, fresh.lines);
          setStaleOnDisk(kept(cur.left, pair.left) || kept(cur.right, pair.right));
          if ((l && l.lines !== cur.left.lines) || (r && r.lines !== cur.right.lines)) {
            setHistory({ undo: [], redo: [] });
          }
          setDocs({ left: l ?? cur.left, right: r ?? cur.right });
          return;
        }
        setDocs({ left: toSide(pair.left), right: toSide(pair.right) });
        setHistory({ undo: [], redo: [] });
        setDiffed(null);
        setHunkIdx(-1);
        setStaleOnDisk(false);
        editingRef.current = null;
        setEditingState(null);
      } catch (e) {
        if (seq !== requestSeq.current) return;
        setError(errorMessage(e));
        if (!quiet) setDocs(null);
      } finally {
        if (seq === requestSeq.current) setLoading(false);
      }
    },
    [left, right],
  );

  useEffect(() => {
    load();
  }, [load]);

  const watchState = useAutoRefresh({ enabled: autoRefresh, paths: [left, right], onChange: () => load(true) });

  const binary = !!docs && (docs.left.binary || docs.right.binary);
  const leftLines = docs?.left.lines;
  const rightLines = docs?.right.lines;
  useEffect(() => {
    if (!leftLines || !rightLines || binary) return;
    let stale = false;
    mergeApi
      .diffTexts(leftLines, rightLines, options)
      .then((result) => {
        if (stale) return;
        setDiffed({ result, for: { left: leftLines, right: rightLines } });
        setDiffError(null);
      })
      .catch((e) => !stale && setDiffError(errorMessage(e)));
    return () => {
      stale = true;
    };
  }, [leftLines, rightLines, binary, options]);

  const diff = binary ? null : diffed?.result ?? null;
  /** Row line numbers match the current text, so edits can be mapped onto it. */
  const fresh = !!diffed && !!docs && diffed.for.left === docs.left.lines && diffed.for.right === docs.right.lines;
  const rows = diff?.rows ?? [];
  const hunks = diff?.hunks ?? [];
  const display = useMemo(() => buildDisplay(rows, mode), [rows, mode]);
  const rowToDisplay = useMemo(() => {
    const map = new Int32Array(rows.length).fill(-1);
    display.forEach((d, i) => "row" in d && (map[d.row] = i));
    return map;
  }, [display, rows.length]);
  const rowHunk = useMemo(() => {
    const map = new Int32Array(rows.length).fill(-1);
    hunks.forEach((h, i) => map.fill(i, h.startRow, h.endRow));
    return map;
  }, [hunks, rows.length]);
  const maxChars = useMemo(() => {
    let m = 0;
    for (const r of rows)
      for (const side of [r.left, r.right])
        if (side) m = Math.max(m, side.reduce((n, s) => n + s.text.length, 0));
    return m;
  }, [rows]);

  // Copying a hunk removes it; keep the selection on the hunk that took its place.
  useEffect(() => {
    if (hunkIdx >= hunks.length) setHunkIdx(hunks.length - 1);
  }, [hunks.length, hunkIdx]);

  const goHunk = useCallback(
    (idx: number) => {
      if (!hunks.length) return;
      const clamped = Math.max(0, Math.min(hunks.length - 1, idx));
      setHunkIdx(clamped);
      list.current?.scrollToRow(rowToDisplay[hunks[clamped].startRow]);
    },
    [hunks, rowToDisplay],
  );

  /** Replaces the text of both sides as one undoable step; returns the new docs. */
  const commit = (next: Snapshot): Docs | null => {
    if (!docs) return null;
    if (next.left === docs.left.lines && next.right === docs.right.lines) return docs;
    const updated: Docs = {
      left: { ...docs.left, lines: next.left },
      right: { ...docs.right, lines: next.right },
    };
    setHistory((h) => ({
      undo: [...h.undo.slice(-(MAX_UNDO - 1)), { left: docs.left.lines, right: docs.right.lines }],
      redo: [],
    }));
    setDocs(updated);
    return updated;
  };

  const snapshot = (): Snapshot | null => docs && { left: docs.left.lines, right: docs.right.lines };

  const undo = () => {
    const cur = snapshot();
    const prev = history.undo[history.undo.length - 1];
    if (!cur || !prev) return;
    setHistory({ undo: history.undo.slice(0, -1), redo: [...history.redo, cur] });
    setDocs((d) => d && { left: { ...d.left, lines: prev.left }, right: { ...d.right, lines: prev.right } });
  };

  const redo = () => {
    const cur = snapshot();
    const next = history.redo[history.redo.length - 1];
    if (!cur || !next) return;
    setHistory({ undo: [...history.undo, cur], redo: history.redo.slice(0, -1) });
    setDocs((d) => d && { left: { ...d.left, lines: next.left }, right: { ...d.right, lines: next.right } });
  };

  /** Replaces the destination side of hunk `idx` with the lines from `from`. */
  const copyHunk = (idx: number, from: Side) => {
    const to = other(from);
    if (!docs || !fresh || readOnly[to] || idx < 0 || idx >= hunks.length) return;
    const src = hunkSpan(rows, hunks[idx], from);
    const dst = hunkSpan(rows, hunks[idx], to);
    const dest = docs[to].lines;
    const lines = dest.slice(0, dst.start).concat(docs[from].lines.slice(src.start, src.end), dest.slice(dst.end));
    setHunkIdx(idx);
    commit({ ...snapshot()!, [to]: lines });
  };

  const startEdit = (row: number, side: Side) => {
    const n = rows[row] && lineNo(rows[row], side);
    if (!docs || !fresh || readOnly[side] || n == null) return;
    setEditing({ row, side, value: docs[side].lines[n - 1] });
  };

  /** Applies the inline edit, if any; returns the resulting docs. */
  const commitEdit = (): Docs | null => {
    const e = editingRef.current;
    setEditing(null);
    if (!e || !docs || !fresh) return docs;
    const n = lineNo(rows[e.row], e.side);
    const lines = docs[e.side].lines;
    if (n == null || lines[n - 1] === e.value) return docs;
    const next = lines.slice();
    next[n - 1] = e.value;
    return commit({ ...snapshot()!, [e.side]: next });
  };

  const save = async () => {
    const current = editingRef.current ? commitEdit() : docs;
    if (!current || saving) return;
    const sides = (["left", "right"] as Side[]).filter((s) => isDirty(current[s]) && !readOnly[s]);
    if (!sides.length) return;
    setSaving(true);
    setError(null);
    try {
      for (const s of sides) {
        const doc = current[s];
        const info = await mergeApi.saveTextFile(
          doc.info.path,
          joinLines(doc.lines, doc.trailingNewline),
          doc.format,
        );
        // Edits made while saving stay dirty: only the saved array is marked clean.
        setDocs((d) => d && { ...d, [s]: { ...d[s], info: { ...info, path: doc.info.path }, saved: doc.lines } });
      }
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setSaving(false);
    }
  };

  const confirmDiscard = (action: string) =>
    !dirty || window.confirm(`You have unsaved changes. Discard them and ${action}?`);

  const refresh = () => {
    if (confirmDiscard("reload both files")) load();
  };
  const patternCount = options.ignorePatterns.filter((p) => p.trim()).length;

  const changePaths = (l: string, r: string) => {
    if (confirmDiscard("open the new files")) onPathsChange(l, r);
  };

  // Re-registered every render so the handler always sees current state.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      // Inactive tabs stay mounted but hidden; only the visible session reacts.
      if (e.defaultPrevented || !root.current?.offsetParent) return;
      const mod = e.ctrlKey || e.metaKey;
      const key = e.key.toLowerCase();
      const t = e.target as HTMLElement | null;
      const inField = !!t && (t.tagName === "INPUT" || t.tagName === "TEXTAREA" || t.tagName === "SELECT");
      if (mod && !e.shiftKey && !e.altKey && key === "s") save();
      else if (inField) return;
      else if (mod && !e.shiftKey && key === "z") undo();
      else if (mod && (key === "y" || (e.shiftKey && key === "z"))) redo();
      else if (e.altKey && e.key === "ArrowDown") goHunk(hunkIdx + 1);
      else if (e.altKey && e.key === "ArrowUp") goHunk(hunkIdx - 1);
      else if (e.altKey && e.key === "ArrowRight") copyHunk(hunkIdx, "left");
      else if (e.altKey && e.key === "ArrowLeft") copyHunk(hunkIdx, "right");
      else if (e.key === "F5") refresh();
      else return;
      e.preventDefault();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  });

  const onWheel = (e: WheelEvent<HTMLDivElement>) => {
    const dx = e.shiftKey ? e.deltaY : e.deltaX;
    if (!dx || (!e.shiftKey && Math.abs(e.deltaX) < Math.abs(e.deltaY))) return;
    setHx((x) => Math.max(0, Math.min(maxChars * CHAR_WIDTH, x + dx)));
  };

  const shift = { transform: `translateX(${-hx}px)` };
  const copyTitle = (to: Side, hotkey: string) =>
    readOnly[to] ? READ_ONLY_HINT : `Copy this difference to the ${to} (${hotkey})`;

  const renderCell = (rowIdx: number, row: DiffRow, side: Side) => {
    const segs = side === "left" ? row.left : row.right;
    const edit = editing && editing.row === rowIdx && editing.side === side ? editing : null;
    return (
      <div
        className={`tcell${segs ? "" : " absent"}`}
        onDoubleClick={() => startEdit(rowIdx, side)}
        title={segs && readOnly[side] ? READ_ONLY_HINT : undefined}
      >
        {edit ? (
          <input
            className="tc-edit-input"
            autoFocus
            spellCheck={false}
            value={edit.value}
            onChange={(e) => setEditing({ ...edit, value: e.target.value })}
            onBlur={commitEdit}
            onKeyDown={(e) => {
              if (e.key === "Enter") commitEdit();
              else if (e.key === "Escape") setEditing(null);
              else return;
              e.preventDefault();
            }}
          />
        ) : (
          <div className="tcontent" style={shift}>
            {segs && <Segments segs={segs} />}
          </div>
        )}
      </div>
    );
  };

  const renderRow = (i: number) => {
    const item = display[i];
    if ("gap" in item) {
      return <div className="tgap">⋯ {item.gap.toLocaleString()} identical lines hidden ⋯</div>;
    }
    const row = rows[item.row];
    const h = rowHunk[item.row];
    const inHunk = h >= 0 && h === hunkIdx;
    const first = h >= 0 && hunks[h].startRow === item.row;
    return (
      <div
        className={`trow k-${row.kind}${inHunk ? " current" : ""}`}
        onClick={() => h >= 0 && h !== hunkIdx && setHunkIdx(h)}
      >
        <span className="ln">{row.leftNo ?? ""}</span>
        {renderCell(item.row, row, "left")}
        <span className="mid">
          {first && (
            <>
              <button
                className="tc-gutter-btn"
                disabled={!fresh || readOnly.right}
                title={copyTitle("right", "Alt+→")}
                onClick={(e) => {
                  e.stopPropagation();
                  copyHunk(h, "left");
                }}
              >
                <ChevronRight size={13} />
              </button>
              <button
                className="tc-gutter-btn"
                disabled={!fresh || readOnly.left}
                title={copyTitle("left", "Alt+←")}
                onClick={(e) => {
                  e.stopPropagation();
                  copyHunk(h, "right");
                }}
              >
                <ChevronLeft size={13} />
              </button>
            </>
          )}
        </span>
        <span className="ln">{row.rightNo ?? ""}</span>
        {renderCell(item.row, row, "right")}
      </div>
    );
  };

  const stats = diff?.stats;
  const canSave = !!docs && (["left", "right"] as Side[]).some((s) => isDirty(docs[s]) && !readOnly[s]);
  const allReadOnly = readOnly.left && readOnly.right;
  const hasHunk = fresh && hunkIdx >= 0 && hunkIdx < hunks.length;
  return (
    <div className="session tc-editable" ref={root}>
      <div className="toolbar">
        <button onClick={() => goHunk(hunkIdx - 1)} disabled={!hunks.length} title="Previous difference (Alt+↑)">
          <ArrowUp size={14} /> Prev
        </button>
        <button onClick={() => goHunk(hunkIdx + 1)} disabled={!hunks.length} title="Next difference (Alt+↓)">
          <ArrowDown size={14} /> Next
        </button>
        <button
          className="icon-btn"
          onClick={() => copyHunk(hunkIdx, "left")}
          disabled={!hasHunk || readOnly.right}
          title={readOnly.right ? READ_ONLY_HINT : "Copy current difference to the right (Alt+→)"}
        >
          <ArrowRight size={14} />
        </button>
        <button
          className="icon-btn"
          onClick={() => copyHunk(hunkIdx, "right")}
          disabled={!hasHunk || readOnly.left}
          title={readOnly.left ? READ_ONLY_HINT : "Copy current difference to the left (Alt+←)"}
        >
          <ArrowLeft size={14} />
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
        <button onClick={() => setEditingRules(true)} title="Regexes for text that never matters">
          <ListFilter size={14} /> Ignore rules{patternCount ? ` · ${patternCount}` : ""}
        </button>
        <span className="spacer" />
        <button className="icon-btn" onClick={undo} disabled={!history.undo.length} title="Undo (Ctrl+Z)">
          <Undo2 size={14} />
        </button>
        <button
          className="icon-btn"
          onClick={redo}
          disabled={!history.redo.length}
          title="Redo (Ctrl+Y or Ctrl+Shift+Z)"
        >
          <Redo2 size={14} />
        </button>
        <button
          onClick={save}
          disabled={!canSave || saving}
          title={allReadOnly ? READ_ONLY_HINT : "Save modified files (Ctrl+S)"}
        >
          <Save size={14} /> {saving ? "Saving…" : "Save"}
        </button>
        <label
          className={`check${watchState === "watching" || watchState === "polling" ? " live" : ""}`}
          title={`${autoRefreshTitle(watchState)}; unsaved edits are never overwritten`}
        >
          <input type="checkbox" checked={autoRefresh} onChange={(e) => setAutoRefresh(e.target.checked)} />
          Auto-refresh
        </label>
        <button onClick={refresh} title="Refresh (F5)">
          <RefreshCw size={14} /> Refresh
        </button>
      </div>
      {editingRules && (
        <IgnoreRulesDialog
          patterns={options.ignorePatterns}
          onClose={() => setEditingRules(false)}
          onApply={(ignorePatterns) => setOptions({ ...options, ignorePatterns })}
        />
      )}

      <PathBar left={left} right={right} directory={false} onCompare={changePaths} />

      {docs && (
        <div className="infobar">
          <FileInfo doc={docs.left} readOnly={readOnly.left} />
          <FileInfo doc={docs.right} readOnly={readOnly.right} />
        </div>
      )}

      <div className="session-body">
        {(loading || (docs && !binary && !diffed && !error && !diffError)) && (
          <div className="overlay">Comparing…</div>
        )}
        {error && <div className="banner error">{error}</div>}
        {diffError && <div className="banner error">{diffError}</div>}
        {binary && (
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
            <span className="ok">{dirty ? "Texts are identical" : "Files are identical"}</span>
          ) : (
            <>
              <span>
                {hunks.length} difference{hunks.length === 1 ? "" : "s"}
                {hunkIdx >= 0 && hunkIdx < hunks.length ? ` (at ${hunkIdx + 1})` : ""}
              </span>
              <span className="k-changed-fg">{stats!.changed} changed</span>
              <span className="k-leftOnly-fg">{stats!.leftOnly} left only</span>
              <span className="k-rightOnly-fg">{stats!.rightOnly} right only</span>
              {stats!.unimportant > 0 && <span className="k-unimportant-fg">{stats!.unimportant} unimportant</span>}
            </>
          ))}
        {staleOnDisk && (
          <span className="tc-stale" title="Your unsaved edits were kept; Refresh discards them and reloads">
            Changed on disk
          </span>
        )}
        {docs && !binary && (
          <span className="tc-hint">
            {allReadOnly ? "Read-only" : "Double-click a line to edit"}
          </span>
        )}
      </div>
    </div>
  );
}

/** Edits the regexes whose matches are ignored when comparing lines. */
export function IgnoreRulesDialog({
  patterns,
  onApply,
  onClose,
}: {
  patterns: string[];
  onApply: (p: string[]) => void;
  onClose: () => void;
}) {
  const [text, setText] = useState(patterns.join("\n"));
  return (
    <Modal
      title="Ignore rules"
      onClose={onClose}
      footer={
        <>
          <button onClick={onClose}>Cancel</button>
          <button
            className="primary"
            onClick={() => {
              onApply(text.split("\n").map((p) => p.trim()).filter(Boolean));
              onClose();
            }}
          >
            Apply
          </button>
        </>
      }
    >
      <label className="field">
        <span>Ignore text matching (one regex per line)</span>
        <textarea
          rows={6}
          spellCheck={false}
          autoFocus
          value={text}
          placeholder={"\\$Id:[^$]*\\$\n^\\s*//.*$"}
          onChange={(e) => setText(e.target.value)}
        />
        <small className="muted">
          Matching text is removed before lines are compared — lines that differ only there are shown as unimportant.
        </small>
      </label>
    </Modal>
  );
}

/** Thumbnail strip showing where differences sit in the whole file; click to jump. */
function Overview({ rows, hunks, onJump }: { rows: DiffRow[]; hunks: Hunk[]; onJump: (row: number) => void }) {
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
