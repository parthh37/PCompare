import { save as saveDialog } from "@tauri-apps/plugin-dialog";
import { ArrowDown, ArrowDownToLine, ArrowUp, GitMerge, ListFilter, RefreshCw, Save } from "lucide-react";
import {
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
  type MouseEvent,
  type PointerEvent,
  type ReactNode,
  type WheelEvent,
} from "react";
import { DEFAULT_TEXT_OPTIONS, errorMessage, isRemote, type TextDiffOptions, type WhitespaceMode } from "../lib/api";
import { formatSize } from "../lib/format";
import { mergeApi, type ChunkKind, type MergeChunk, type MergeLoad, type TextDocument } from "../lib/mergeApi";
import type { MergePaths, MergeProps } from "../lib/sessionProps";
import { PathField } from "./PathField";
import { IgnoreRulesDialog } from "./TextCompare";
import { VirtualList, type VirtualListHandle } from "./VirtualList";
import "./TextMerge.css";

const ROW_HEIGHT = 20;
const CHAR_WIDTH = 7.8;
/** Output textarea metrics; the highlight layer relies on them matching TextMerge.css. */
const OUT_LINE_HEIGHT = 20;
const OUT_PAD_TOP = 4;
const MIN_SPLIT = 0.15;
const MAX_SPLIT = 0.85;

/** What a saved merge session remembers besides its paths. */
export interface MergeSettings {
  options: TextDiffOptions;
  /** Fraction of the body height given to the three input panes. */
  split: number;
}

type Resolution = "left" | "right" | "leftRight" | "rightLeft" | "base" | "conflict";
const RESOLUTIONS: { id: Resolution; label: string; title: string }[] = [
  { id: "left", label: "Left", title: "Take the left version" },
  { id: "right", label: "Right", title: "Take the right version" },
  { id: "leftRight", label: "Left + Right", title: "Take the left version, then the right version" },
  { id: "rightLeft", label: "Right + Left", title: "Take the right version, then the left version" },
  { id: "base", label: "Base", title: "Take the base (ancestor) version, discarding both changes" },
  { id: "conflict", label: "Conflict", title: "Put both versions back between conflict markers (re-open it)" },
];

type Pane = "left" | "base" | "right";

/** Output lines `[start, end)` that a chunk currently occupies. */
interface Region {
  start: number;
  end: number;
}

/** A programmatic output edit: lines `[start, oldEnd)` became `[start, newEnd)`, inside chunk `chunk`'s region. */
interface RegionEdit {
  chunk: number;
  start: number;
  oldEnd: number;
  newEnd: number;
}

/** Lines picked in one input pane (pane line indices; `anchor` is where the pick started). */
interface LineSelection {
  pane: Pane;
  anchor: number;
  focus: number;
}

const rangeLen = (r: { start: number; end: number }) => r.end - r.start;

/** Where each chunk lands in the freshly rendered output (mirrors pcompare-merge's `render_output`). */
function initialRegions(chunks: MergeChunk[]): Region[] {
  let pos = 0;
  return chunks.map((c) => {
    const len =
      c.kind === "conflict"
        ? rangeLen(c.left) + rangeLen(c.right) + 3
        : c.kind === "rightChanged"
          ? rangeLen(c.right)
          : rangeLen(c.left);
    const r = { start: pos, end: pos + len };
    pos += len;
    return r;
  });
}

/**
 * Moves the chunk regions along with an edit of the output text. Programmatic
 * edits say exactly what changed; hand edits are inferred from the common
 * leading and trailing lines, and new text goes to the region it was typed in.
 */
function shiftRegions(prev: string, next: string, regions: Region[], edit: RegionEdit | null): Region[] {
  if (edit) {
    const delta = edit.newEnd - edit.oldEnd;
    return regions.map((r, k) =>
      k < edit.chunk
        ? r
        : k === edit.chunk
          ? { start: Math.min(r.start, edit.start), end: r.end + delta }
          : { start: r.start + delta, end: r.end + delta },
    );
  }
  if (prev === next) return regions;
  const a = prev.split("\n");
  const b = next.split("\n");
  const min = Math.min(a.length, b.length);
  let p = 0;
  while (p < min && a[p] === b[p]) p++;
  let s = 0;
  while (s < min - p && a[a.length - 1 - s] === b[b.length - 1 - s]) s++;
  const q = a.length - s; // old lines [p, q) became new lines [p, q2)
  const q2 = b.length - s;
  const delta = q2 - q;
  let floor = 0;
  return regions.map((r) => {
    const start = Math.max(floor, r.start < p ? r.start : r.start >= q ? r.start + delta : p);
    const end = Math.max(start, r.end <= p ? r.end : r.end >= q ? r.end + delta : q2);
    floor = end;
    return { start, end };
  });
}

/** Character range and replacement text that turn output lines `[start, end)` into `insert`. */
function lineEdit(lines: string[], text: string, start: number, end: number, insert: string[]) {
  if (end < lines.length) {
    // A line follows the range: replace through its start, newline included.
    return { from: lineOffset(lines, start), to: lineOffset(lines, end), value: insert.map((l) => l + "\n").join("") };
  }
  // The range reaches the last line, which has no newline after it.
  if (start < lines.length) {
    const from = lineOffset(lines, start);
    // Removing the last lines also removes the newline that preceded them.
    return insert.length || start === 0
      ? { from, to: text.length, value: insert.join("\n") }
      : { from: from - 1, to: text.length, value: "" };
  }
  return { from: text.length, to: text.length, value: insert.length ? "\n" + insert.join("\n") : "" };
}

/** Cell classes (left, base, right) per chunk kind in the input panes. */
const CELL_CLASSES: Record<ChunkKind, [string, string, string]> = {
  stable: ["", "", ""],
  leftChanged: ["chg", "base", ""],
  rightChanged: ["", "base", "chg"],
  bothSame: ["same", "base", "same"],
  conflict: ["conf", "conf", "conf"],
};

const CHUNK_LABEL: Record<ChunkKind, string> = {
  stable: "Unchanged",
  leftChanged: "Changed on the left (merged automatically)",
  rightChanged: "Changed on the right (merged automatically)",
  bothSame: "Same change on both sides (merged automatically)",
  conflict: "Conflict",
};

/** One aligned row of the three input panes; `null` pads a shorter side. */
interface MergeRow {
  chunk: number;
  left: number | null;
  base: number | null;
  right: number | null;
  first: boolean;
}

function buildRows(chunks: MergeChunk[]): { rows: MergeRow[]; chunkRow: number[] } {
  const rows: MergeRow[] = [];
  const chunkRow: number[] = [];
  chunks.forEach((c, chunk) => {
    chunkRow.push(rows.length);
    const ll = c.left.end - c.left.start;
    const bl = c.base.end - c.base.start;
    const rl = c.right.end - c.right.start;
    const n = Math.max(ll, bl, rl);
    for (let k = 0; k < n; k++) {
      rows.push({
        chunk,
        left: k < ll ? c.left.start + k : null,
        base: k < bl ? c.base.start + k : null,
        right: k < rl ? c.right.start + k : null,
        first: k === 0,
      });
    }
  });
  return { rows, chunkRow };
}

// Conflict markers written by pcompare-merge's `render_output`.
const START_RE = /^<{7} Left #(\d+)$/;
const END_RE = /^>{7} Right #(\d+)$/;
const SEPARATOR = "=======";

/** An unresolved conflict block in the output, as line indices (markers included). */
interface Block {
  n: number;
  start: number;
  sep: number;
  end: number;
}

function findBlocks(lines: string[]): Block[] {
  const out: Block[] = [];
  let open: { n: number; start: number; sep: number } | null = null;
  lines.forEach((line, i) => {
    const c = line.charCodeAt(0);
    if (c === 60 /* < */) {
      const m = START_RE.exec(line);
      if (m) open = { n: Number(m[1]), start: i, sep: -1 };
    } else if (!open) {
      return;
    } else if (line === SEPARATOR && open.sep < 0) {
      open.sep = i;
    } else if (c === 62 /* > */) {
      const m = END_RE.exec(line);
      if (m && Number(m[1]) === open.n) {
        out.push({ ...open, end: i });
        open = null;
      }
    }
  });
  return out;
}

/** Character offset where line `i` starts (`text.length` past the last line). */
function lineOffset(lines: string[], i: number): number {
  let off = 0;
  for (let k = 0; k < i && k < lines.length; k++) off += lines[k].length + 1;
  return off;
}

function DocInfo({ label, doc }: { label: string; doc: TextDocument | undefined }) {
  return (
    <div className="tm-colhead">
      <span className="tm-colname">{label}</span>
      {doc && (
        <span className="fileinfo">
          {doc.info.encoding}
          {doc.info.hasBom ? " BOM" : ""} · {doc.info.lineEnding.toUpperCase()} · {formatSize(doc.info.size)}
        </span>
      )}
    </div>
  );
}

export function TextMerge({
  left,
  base,
  right,
  output,
  onPathsChange,
  settings,
  onSettingsChange,
  onDirtyChange,
}: MergeProps<MergeSettings>) {
  const [options, setOptionsState] = useState<TextDiffOptions>({ ...DEFAULT_TEXT_OPTIONS, ...settings?.options });
  const [editingRules, setEditingRules] = useState(false);
  const [split, setSplit] = useState(() => Math.min(MAX_SPLIT, Math.max(MIN_SPLIT, settings?.split ?? 0.5)));
  const reportSettings = useRef(onSettingsChange);
  reportSettings.current = onSettingsChange;
  useEffect(() => void reportSettings.current?.({ options, split }), [options, split]);

  const [draft, setDraft] = useState<MergePaths>({ left, base, right, output });
  useEffect(() => setDraft({ left, base, right, output }), [left, base, right, output]);

  const [data, setData] = useState<MergeLoad | null>(null);
  /** The output text and where each chunk sits in it. */
  const [out, setOut] = useState<{ text: string; regions: Region[] }>({ text: "", regions: [] });
  const text = out.text;
  const regions = out.regions;
  /** Output text as last written to disk; `null` until the first save. */
  const [savedText, setSavedText] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const [saving, setSaving] = useState(false);
  /** Index of the selected (changed) chunk, or -1. */
  const [sel, setSel] = useState(-1);
  const [lineSel, setLineSel] = useState<LineSelection | null>(null);
  const [hx, setHx] = useState(0);
  const [outScroll, setOutScroll] = useState({ top: 0, left: 0 });

  const root = useRef<HTMLDivElement>(null);
  const body = useRef<HTMLDivElement>(null);
  const list = useRef<VirtualListHandle>(null);
  const textarea = useRef<HTMLTextAreaElement>(null);
  /** Set just before a programmatic edit so the change handler knows which chunk it belongs to. */
  const pendingEdit = useRef<RegionEdit | null>(null);
  /** Pane being drag-selected in, while the mouse button is down. */
  const dragPane = useRef<Pane | null>(null);

  /** The base is optional: without it the merge is two-way and every difference is a conflict. */
  const ready = !!(left && right);
  const twoWay = data ? !data.base : !base;
  const requestSeq = useRef(0);
  const load = useCallback(async () => {
    if (!left || !right) return;
    const seq = ++requestSeq.current;
    setLoading(true);
    setError(null);
    try {
      const res = await mergeApi.loadMerge(left, base, right, options);
      if (seq !== requestSeq.current) return;
      setData(res);
      setOut({ text: res.output, regions: initialRegions(res.merge.chunks) });
      setSavedText(null);
      setSel(-1);
      setLineSel(null);
    } catch (e) {
      if (seq !== requestSeq.current) return;
      setError(errorMessage(e));
      setData(null);
      setOut({ text: "", regions: [] });
    } finally {
      if (seq === requestSeq.current) setLoading(false);
    }
  }, [left, base, right, options]);

  useEffect(() => {
    load();
  }, [load]);

  /** The output differs from what is on disk (or from the fresh auto-merge if never saved). */
  const edited = !!data && text !== (savedText ?? data.output);
  const unsaved = !!data && text !== savedText;
  const reportDirty = useRef(onDirtyChange);
  reportDirty.current = onDirtyChange;
  // Only hand edits count: an untouched auto-merge can simply be recomputed.
  useEffect(() => void reportDirty.current?.(edited), [edited]);
  const confirmDiscard = (action: string) =>
    !edited || window.confirm(`The merged output has unsaved edits. Discard them and ${action}?`);

  const setOptions = (next: TextDiffOptions) => {
    if (confirmDiscard("merge again with the new options")) setOptionsState(next);
  };

  const applyPaths = (next: MergePaths) => {
    const inputsChanged = next.left !== left || next.base !== base || next.right !== right;
    if (inputsChanged && !confirmDiscard("merge the new files")) {
      setDraft({ left, base, right, output });
      return;
    }
    onPathsChange(next);
  };
  const pick = (key: keyof MergePaths, value: string) => {
    const next = { ...draft, [key]: value };
    setDraft(next);
    applyPaths(next);
  };

  const chunks = useMemo(() => data?.merge.chunks ?? [], [data]);
  const { rows, chunkRow } = useMemo(() => buildRows(chunks), [chunks]);
  /** Chunk indices of all conflicts, in order. */
  const conflicts = useMemo(
    () => chunks.flatMap((c, i) => (c.kind === "conflict" ? [i] : [])),
    [chunks],
  );
  /** Chunk indices of every change (auto-merged or conflicting), in order. */
  const changes = useMemo(() => chunks.flatMap((c, i) => (c.kind !== "stable" ? [i] : [])), [chunks]);
  /** Number used in the markers when a chunk is (re-)opened as a conflict. */
  const markerNo = useMemo(() => {
    let next = data?.merge.stats.conflicts ?? 0;
    return chunks.map((c) => c.conflict ?? ++next);
  }, [chunks, data]);
  const outLines = useMemo(() => text.split("\n"), [text]);
  const blocks = useMemo(() => findBlocks(outLines), [outLines]);
  /** The unresolved conflict block inside each chunk's output region, if any. */
  const blockOf = useMemo(() => {
    const m = new Map<number, Block>();
    let k = 0;
    for (const b of blocks) {
      while (k < regions.length && regions[k].end <= b.start) k++;
      if (k < regions.length && regions[k].start <= b.start) m.set(k, b);
    }
    return m;
  }, [blocks, regions]);
  /** Chunks whose output no longer matches the automatic merge (resolved or overridden). */
  const overridden = useMemo(() => {
    const set = new Set<number>();
    if (!data) return set;
    const initial = initialRegions(chunks);
    const fresh = data.output.split("\n");
    regions.forEach((r, k) => {
      const i = initial[k];
      if (rangeLen(r) !== rangeLen(i) || outLines.slice(r.start, r.end).some((l, j) => l !== fresh[i.start + j]))
        set.add(k);
    });
    return set;
  }, [data, chunks, regions, outLines]);
  const unresolved = blocks.length;

  const maxChars = useMemo(() => {
    if (!data) return 0;
    let m = 0;
    for (const doc of [data.left, data.base, data.right]) if (doc) for (const l of doc.lines) m = Math.max(m, l.length);
    return m;
  }, [data]);

  const curChunk = sel >= 0 && sel < chunks.length && chunks[sel].kind !== "stable" ? chunks[sel] : null;
  const curBlock = curChunk ? blockOf.get(sel) : undefined;
  const curConflict = curChunk ? conflicts.indexOf(sel) : -1;
  const docOf = (pane: Pane) => (data ? data[pane] : null);

  const scrollOutputTo = (line: number) => {
    const ta = textarea.current;
    if (ta) ta.scrollTop = Math.max(0, line * OUT_LINE_HEIGHT - ta.clientHeight / 3);
  };

  /** Selects a chunk and scrolls the output (and optionally the input panes) to it. */
  const goChunk = (k: number, scrollInputs = true) => {
    if (k < 0 || k >= chunks.length) return;
    setSel(k);
    if (scrollInputs) list.current?.scrollToRow(chunkRow[k]);
    if (regions[k]) scrollOutputTo(regions[k].start);
  };

  /** Moves to the next/previous chunk in `order` (conflicts, or all changes). */
  const step = (order: number[], dir: 1 | -1) => {
    if (!order.length) return;
    const target =
      dir > 0
        ? (order.find((k) => k > sel) ?? (sel < 0 ? order[0] : undefined))
        : [...order].reverse().find((k) => k < sel);
    if (target !== undefined) goChunk(target);
  };

  /** Takes the new output text, moving the chunk regions along with the edit. */
  const changeText = (next: string) => {
    const edit = pendingEdit.current;
    pendingEdit.current = null;
    setOut((prev) => ({ text: next, regions: shiftRegions(prev.text, next, prev.regions, edit) }));
  };

  /** Replaces `[from, to)` of the output, keeping the textarea's native undo history. */
  const replaceOutput = (from: number, to: number, insert: string) => {
    if (from === to && !insert) return;
    const ta = textarea.current;
    if (ta) {
      ta.focus();
      ta.setSelectionRange(from, to);
      // Fires the textarea's change handler synchronously.
      const ok = insert ? document.execCommand("insertText", false, insert) : document.execCommand("delete");
      if (ok) return;
    }
    changeText(text.slice(0, from) + insert + text.slice(to));
  };

  /** Replaces output lines `[start, end)`, which lie in chunk `k`'s region, with `lines`. */
  const editLines = (k: number, start: number, end: number, lines: string[]) => {
    const { from, to, value } = lineEdit(outLines, text, start, end, lines);
    pendingEdit.current = { chunk: k, start, oldEnd: end, newEnd: start + lines.length };
    replaceOutput(from, to, value);
    pendingEdit.current = null;
  };

  /** Sets the selected chunk's output: one side, both, the base, or the conflict markers again. */
  const resolve = (how: Resolution) => {
    if (!data || !curChunk) return;
    const take = (doc: TextDocument | null, r: { start: number; end: number }) =>
      doc ? doc.lines.slice(r.start, r.end) : [];
    const l = take(data.left, curChunk.left);
    const r = take(data.right, curChunk.right);
    const n = markerNo[sel];
    const lines = {
      left: l,
      right: r,
      leftRight: [...l, ...r],
      rightLeft: [...r, ...l],
      base: take(data.base, curChunk.base),
      conflict: [`<<<<<<< Left #${n}`, ...l, SEPARATOR, ...r, `>>>>>>> Right #${n}`],
    }[how];
    const reg = regions[sel];
    editLines(sel, reg.start, reg.end, lines);
  };

  /** The chunk holding line `line` of `pane`, or -1. */
  const chunkAt = (pane: Pane, line: number) => chunks.findIndex((c) => c[pane].start <= line && line < c[pane].end);

  /**
   * Copies the lines picked in an input pane to the output: they replace the
   * conflict they belong to while it is unresolved, otherwise they are
   * inserted after the matching output line (unchanged text) or after the
   * chunk's current output (changes).
   */
  const copySelection = () => {
    const doc = lineSel && docOf(lineSel.pane);
    if (!lineSel || !doc) return;
    const lo = Math.min(lineSel.anchor, lineSel.focus);
    const hi = Math.max(lineSel.anchor, lineSel.focus);
    const lines = doc.lines.slice(lo, hi + 1);
    const k = chunkAt(lineSel.pane, hi);
    if (k < 0 || !lines.length) return;
    const c = chunks[k];
    const reg = regions[k];
    if (blockOf.has(k) && chunkAt(lineSel.pane, lo) === k) {
      editLines(k, reg.start, reg.end, lines);
    } else {
      const at = c.kind === "stable" ? Math.min(reg.end, reg.start + (hi - c[lineSel.pane].start) + 1) : reg.end;
      editLines(k, at, at, lines);
    }
    if (c.kind !== "stable") setSel(k);
    scrollOutputTo(reg.start);
  };

  /** Starts or extends (Shift) a line pick in an input pane. */
  const pickLine = (pane: Pane, line: number | null, extend: boolean) => {
    if (line == null) return;
    dragPane.current = pane;
    setLineSel((cur) =>
      extend && cur && cur.pane === pane ? { ...cur, focus: line } : { pane, anchor: line, focus: line },
    );
  };
  useEffect(() => {
    const up = () => (dragPane.current = null);
    window.addEventListener("mouseup", up);
    return () => window.removeEventListener("mouseup", up);
  }, []);
  const picked = (pane: Pane, line: number | null) =>
    line != null &&
    !!lineSel &&
    lineSel.pane === pane &&
    line >= Math.min(lineSel.anchor, lineSel.focus) &&
    line <= Math.max(lineSel.anchor, lineSel.focus);

  const save = async () => {
    if (!data || saving) return;
    if (
      unresolved > 0 &&
      !window.confirm(
        `${unresolved} unresolved conflict${unresolved === 1 ? "" : "s"} remain${unresolved === 1 ? "s" : ""} ` +
          "(marked in the output). Save anyway?",
      )
    )
      return;
    let target = output;
    if (!target) {
      const picked = await saveDialog({ title: "Save merged output", defaultPath: isRemote(left) ? undefined : left });
      if (typeof picked !== "string") return;
      target = picked;
      onPathsChange({ left, base, right, output: picked });
    }
    if (isRemote(target)) {
      setError("The merged output must be saved to a local file.");
      return;
    }
    const written = text;
    setSaving(true);
    setError(null);
    try {
      await mergeApi.saveTextFile(target, written, data.outputFormat);
      setSavedText(written);
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setSaving(false);
    }
  };

  const refresh = () => {
    if (confirmDiscard("merge the files again")) load();
  };

  // Re-registered every render so the handler always sees current state.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      // Inactive tabs stay mounted but hidden; only the visible session reacts.
      if (e.defaultPrevented || !root.current?.offsetParent) return;
      const mod = e.ctrlKey || e.metaKey;
      const inOutput = e.target === textarea.current;
      if (mod && !e.shiftKey && !e.altKey && e.key.toLowerCase() === "s") save();
      else if (e.altKey && e.shiftKey && e.key === "ArrowDown") step(changes, 1);
      else if (e.altKey && e.shiftKey && e.key === "ArrowUp") step(changes, -1);
      else if (e.altKey && e.key === "ArrowDown") step(conflicts, 1);
      else if (e.altKey && e.key === "ArrowUp") step(conflicts, -1);
      else if (mod && e.key === "Enter" && !inOutput && lineSel) copySelection();
      else if (e.key === "Escape" && !inOutput && lineSel) setLineSel(null);
      else if (e.key === "F5") refresh();
      else return;
      e.preventDefault();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  });

  const onSplitterDown = (e: PointerEvent<HTMLDivElement>) => {
    const el = body.current;
    if (!el) return;
    e.currentTarget.setPointerCapture(e.pointerId);
    const rect = el.getBoundingClientRect();
    const move = (ev: globalThis.PointerEvent) =>
      setSplit(Math.min(MAX_SPLIT, Math.max(MIN_SPLIT, (ev.clientY - rect.top) / rect.height)));
    const target = e.currentTarget;
    const up = () => {
      target.removeEventListener("pointermove", move);
      target.removeEventListener("pointerup", up);
    };
    target.addEventListener("pointermove", move);
    target.addEventListener("pointerup", up);
  };

  const onWheel = (e: WheelEvent<HTMLDivElement>) => {
    const dx = e.shiftKey ? e.deltaY : e.deltaX;
    if (!dx || (!e.shiftKey && Math.abs(e.deltaX) < Math.abs(e.deltaY))) return;
    setHx((x) => Math.max(0, Math.min(maxChars * CHAR_WIDTH, x + dx)));
  };

  const shift = { transform: `translateX(${-hx}px)` };
  const cell = (pane: Pane, line: number | null, cls: string) => {
    const doc = docOf(pane);
    const pick = picked(pane, line) ? " picked" : "";
    const handlers = {
      onMouseDown: (e: MouseEvent) => e.button === 0 && pickLine(pane, line, e.shiftKey),
      onMouseEnter: (e: MouseEvent) =>
        dragPane.current === pane && e.buttons & 1 && line != null && setLineSel((s) => s && { ...s, focus: line }),
    };
    return (
      <>
        <span className={`ln${pick}`} {...handlers}>
          {line == null ? "" : line + 1}
        </span>
        <div className={`tm-cell ${line == null ? "absent" : cls}${pick}`} {...handlers}>
          <div className="tcontent" style={shift}>
            {line != null && doc?.lines[line]}
          </div>
        </div>
      </>
    );
  };

  const renderRow = (i: number) => {
    const row = rows[i];
    const chunk = chunks[row.chunk];
    const isConflict = chunk.kind === "conflict";
    const open = blockOf.has(row.chunk);
    const changed = overridden.has(row.chunk);
    const resolved = isConflict && !open;
    const current = sel === row.chunk && chunk.kind !== "stable";
    const [lc, bc, rc] = CELL_CLASSES[chunk.kind];
    const title =
      chunk.kind === "stable"
        ? changed
          ? "Unchanged text (edited in the output)"
          : undefined
        : isConflict
          ? `Conflict ${chunk.conflict}${resolved ? " (resolved)" : ""}`
          : `${CHUNK_LABEL[chunk.kind]}${open ? " (re-opened as a conflict)" : changed ? " (overridden)" : ""}; click to choose a different side`;
    return (
      <div
        className={`mrow m-${chunk.kind}${row.first && chunk.kind !== "stable" ? " first" : ""}${
          resolved ? " resolved" : ""
        }${!isConflict && changed ? " edited" : ""}${!isConflict && open ? " reopened" : ""}${current ? " current" : ""}`}
        onClick={() => chunk.kind !== "stable" && goChunk(row.chunk, false)}
        title={title}
      >
        {cell("left", row.left, lc)}
        {!twoWay && cell("base", row.base, bc)}
        {cell("right", row.right, rc)}
      </div>
    );
  };

  /** Background bars behind the output text: unresolved conflict blocks and the selected chunk. */
  const highlights = () => {
    const ta = textarea.current;
    const viewTop = outScroll.top;
    const viewBottom = viewTop + (ta?.clientHeight ?? 2000);
    const bars: ReactNode[] = [];
    const bar = (line: number, cls: string, key: string, height = OUT_LINE_HEIGHT) => {
      const top = OUT_PAD_TOP + line * OUT_LINE_HEIGHT;
      if (top + OUT_LINE_HEIGHT < viewTop || top > viewBottom) return;
      bars.push(<div key={key} className={`tm-hl ${cls}`} style={{ top, height }} />);
    };
    for (const b of blocks) {
      const blockTop = OUT_PAD_TOP + b.start * OUT_LINE_HEIGHT;
      const blockBottom = OUT_PAD_TOP + (b.end + 1) * OUT_LINE_HEIGHT;
      if (blockBottom < viewTop || blockTop > viewBottom) continue;
      const isCur = curBlock?.n === b.n ? " current" : "";
      for (let line = b.start; line <= b.end; line++) {
        const marker = line === b.start || line === b.end || line === b.sep;
        const part = marker ? "marker" : b.sep < 0 || line < b.sep ? "left" : "right";
        bar(line, `${part}${isCur}`, `${b.n}:${line}`);
      }
    }
    const reg = curChunk && !curBlock ? regions[sel] : null;
    if (reg) {
      // A chunk that currently contributes no lines: a thin bar where its output would go.
      if (reg.start === reg.end) bar(reg.start, "region empty", "sel", 2);
      for (let line = reg.start; line < reg.end; line++) bar(line, "region", `sel:${line}`);
    }
    return bars;
  };

  const stats = data?.merge.stats;
  const autoMerged = stats ? stats.leftChanges + stats.rightChanges + stats.bothSame : 0;
  const canResolve = !!curChunk;
  const resolutions = RESOLUTIONS.filter((r) => !(twoWay && r.id === "base"));
  const lineCount = lineSel ? Math.abs(lineSel.focus - lineSel.anchor) + 1 : 0;
  const patternCount = options.ignorePatterns.filter((p) => p.trim()).length;
  return (
    <div className="session tm-session" ref={root}>
      <div className="toolbar">
        <button
          onClick={() => step(conflicts, -1)}
          disabled={!conflicts.length}
          title="Previous conflict (Alt+↑; Alt+Shift+↑ for any change)"
        >
          <ArrowUp size={14} /> Prev
        </button>
        <button
          onClick={() => step(conflicts, 1)}
          disabled={!conflicts.length}
          title="Next conflict (Alt+↓; Alt+Shift+↓ for any change)"
        >
          <ArrowDown size={14} /> Next
        </button>
        <span className="sep" />
        <span className="tm-take-label">Take</span>
        <div className="seg-control">
          {resolutions.map((r) => (
            <button
              key={r.id}
              disabled={!canResolve}
              onClick={() => resolve(r.id)}
              title={
                !curChunk
                  ? "Select a conflict or change first (click it, or Alt+↓)"
                  : curBlock
                    ? `${r.title} for ${curChunk.conflict ? `conflict ${curChunk.conflict}` : "this change"}`
                    : `${r.title}, replacing this ${curChunk.kind === "conflict" ? "resolved conflict" : "change"}'s current output`
              }
            >
              {r.label}
            </button>
          ))}
        </div>
        <button
          onClick={copySelection}
          disabled={!lineSel}
          title={
            lineSel
              ? `Copy the ${lineCount} picked ${lineSel.pane} line${lineCount === 1 ? "" : "s"} to the output (Ctrl+Enter)`
              : "Pick lines in an input pane first (click, Shift+click or drag)"
          }
        >
          <ArrowDownToLine size={14} /> Copy to output
        </button>
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
        <button onClick={() => setEditingRules(true)} title="Regexes for text that never matters">
          <ListFilter size={14} /> Ignore rules{patternCount ? ` · ${patternCount}` : ""}
        </button>
        <span className="spacer" />
        <button
          onClick={save}
          disabled={!data || saving}
          title={output ? `Save the merged output to ${output} (Ctrl+S)` : "Choose where to save the merged output (Ctrl+S)"}
        >
          <Save size={14} /> {saving ? "Saving…" : "Save"}
        </button>
        <button onClick={refresh} disabled={!ready} title="Merge again from the files on disk (F5)">
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

      <div className="tm-paths">
        {(["left", "base", "right"] as const).map((key) => (
          <PathField
            key={key}
            label={key[0].toUpperCase() + key.slice(1)}
            value={draft[key]}
            onChange={(v) => setDraft({ ...draft, [key]: v })}
            onSubmit={() => applyPaths(draft)}
            onPick={(p) => pick(key, p)}
            directory={false}
            placeholder={
              key === "base"
                ? "Common ancestor (optional: leave empty for a two-way merge)"
                : `${key[0].toUpperCase() + key.slice(1)} file… or remote://profile/path`
            }
          />
        ))}
        <div className="tm-output-path">
          <PathField
            label="Output"
            value={draft.output}
            onChange={(v) => setDraft({ ...draft, output: v })}
            onSubmit={() => applyPaths(draft)}
            onPick={(p) => pick("output", p)}
            directory={false}
            saveDialog
            localOnly
            placeholder="Merged output file (chosen on first save if empty)"
          />
          <button className="primary" onClick={() => applyPaths(draft)} disabled={!draft.left || !draft.right}>
            <GitMerge size={14} /> Merge
          </button>
        </div>
      </div>

      <div className="session-body tm-body" ref={body}>
        {loading && <div className="overlay">Merging…</div>}
        {error && <div className="banner error">{error}</div>}
        {!ready && !data && (
          <div className="empty-state">
            Choose the left and right files to merge. Add their common ancestor (base) for a 3-way merge; without
            it every difference is a conflict to resolve.
          </div>
        )}
        {data && (
          <>
            <div className={`tm-inputs${twoWay ? " tm-two" : ""}`} style={{ flexBasis: `${split * 100}%` }}>
              <div className="tm-heads">
                <DocInfo label="Left" doc={data.left} />
                {data.base && <DocInfo label="Base" doc={data.base} />}
                <DocInfo label="Right" doc={data.right} />
              </div>
              <VirtualList
                ref={list}
                className="tm-list"
                count={rows.length}
                rowHeight={ROW_HEIGHT}
                renderRow={renderRow}
                onWheel={onWheel}
              />
            </div>
            <div
              className="tm-splitter"
              onPointerDown={onSplitterDown}
              role="separator"
              aria-orientation="horizontal"
              title="Drag to resize"
            />
            <div className="tm-outpane">
              <div className="tm-colhead tm-outhead">
                <span className="tm-colname">Output</span>
                <span className="fileinfo">{output || "not saved yet"}</span>
                {unsaved && (
                  <span className="tm-dirty" title="The merged output has not been saved (Ctrl+S)">
                    <span className="tm-dirty-dot" /> unsaved
                  </span>
                )}
              </div>
              <div className="tm-editor">
                <div className="tm-highlights" aria-hidden>
                  <div style={{ transform: `translate(${-outScroll.left}px, ${-outScroll.top}px)` }}>
                    {highlights()}
                  </div>
                </div>
                <textarea
                  ref={textarea}
                  className="tm-textarea"
                  value={text}
                  wrap="off"
                  spellCheck={false}
                  onChange={(e) => changeText(e.target.value)}
                  onScroll={(e) =>
                    setOutScroll({ top: e.currentTarget.scrollTop, left: e.currentTarget.scrollLeft })
                  }
                  onKeyDown={(e) => {
                    if (e.key === "Tab" && !e.shiftKey && !e.ctrlKey && !e.altKey && !e.metaKey) {
                      e.preventDefault();
                      document.execCommand("insertText", false, "\t");
                    }
                  }}
                />
              </div>
            </div>
          </>
        )}
      </div>

      <div className="statusbar">
        {stats && (
          <>
            {stats.conflicts === 0 ? (
              <span className="ok">No conflicts</span>
            ) : (
              <span className="tm-conflicts">
                {stats.conflicts} conflict{stats.conflicts === 1 ? "" : "s"}
                {curConflict >= 0 ? ` (at ${curConflict + 1})` : ""}
              </span>
            )}
            {(stats.conflicts > 0 || unresolved > 0) &&
              (unresolved ? (
                <span className="tm-unresolved">{unresolved} unresolved</span>
              ) : (
                <span className="ok">All conflicts resolved</span>
              ))}
            {twoWay ? (
              <span className="muted">Two-way merge (no base): every difference is a conflict</span>
            ) : (
              <>
                <span>
                  {autoMerged} auto-merged change{autoMerged === 1 ? "" : "s"}
                </span>
                <span className="muted">
                  {stats.leftChanges} left · {stats.rightChanges} right · {stats.bothSame} both
                </span>
              </>
            )}
            {lineSel && (
              <span className="muted">
                {lineCount} {lineSel.pane} line{lineCount === 1 ? "" : "s"} picked · Ctrl+Enter copies to the output
              </span>
            )}
          </>
        )}
      </div>
    </div>
  );
}
