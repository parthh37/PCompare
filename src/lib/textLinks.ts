// pure helpers for Text Compare's linked (unpadded) layout,
// synchronization points, edit markers and line-ending glyphs. No React here:
// everything works on diff rows, display items and line arrays.
import type { DiffRow, LineEnding, LinePair, LineRange, RowKind } from "./api";
import type { Side } from "./textEdit";

/** A displayed row, or a run of identical rows hidden by the Context/Diffs view. */
export type DisplayItem = { row: number } | { gap: number };

/**
 * The display split into blocks for the linked layout. A block is a run of
 * display items both sides scroll through together: an equal stretch (one
 * line on each side per item), one difference, or one orphan row. Offsets are
 * in rows; `u` is the padded (side-by-side) position, `l` / `r` each side's.
 */
export interface LinkedLayout {
  /** Display indices shown on each side, in order (rows with a line there, and gaps). */
  items: Record<Side, number[]>;
  /** Display index → index in `items[side]`, or -1 when that side has no line there. */
  pos: Record<Side, Int32Array>;
  /** Block starts (blocks + 1 entries, the last is the total). */
  u: Int32Array;
  l: Int32Array;
  r: Int32Array;
  /** Per block: the kind a connector is drawn in, or null for equal stretches. */
  kind: (RowKind | null)[];
  /** Per block: the difference (hunk index) it shows, or -1. */
  hunk: number[];
}

export function linkedLayout(display: DisplayItem[], rows: DiffRow[], rowHunk: Int32Array): LinkedLayout {
  const items: Record<Side, number[]> = { left: [], right: [] };
  const pos = { left: new Int32Array(display.length).fill(-1), right: new Int32Array(display.length).fill(-1) };
  const u = [0];
  const l = [0];
  const r = [0];
  const kind: (RowKind | null)[] = [];
  const hunk: number[] = [];
  let key = "";
  let allUnimportant = true;
  const close = () => {
    const b = kind.length - 1;
    if (b < 0) return;
    const nl = l[b + 1] - l[b];
    const nr = r[b + 1] - r[b];
    if (key === "eq") kind[b] = null;
    else if (allUnimportant) kind[b] = "unimportant";
    else kind[b] = nl && nr ? "changed" : nl ? "leftOnly" : "rightOnly";
  };
  display.forEach((d, i) => {
    const row = "row" in d ? rows[d.row] : null;
    const hasL = !row || row.leftNo != null;
    const hasR = !row || row.rightNo != null;
    const h = row ? rowHunk[(d as { row: number }).row] : -1;
    const k = h >= 0 ? `h${h}` : hasL && hasR ? "eq" : `o${i}`;
    if (k !== key || !kind.length) {
      close();
      key = k;
      allUnimportant = true;
      kind.push(null);
      hunk.push(h);
      u.push(u[u.length - 1]);
      l.push(l[l.length - 1]);
      r.push(r[r.length - 1]);
    }
    const b = kind.length;
    u[b]++;
    if (row && row.kind !== "unimportant") allUnimportant = false;
    if (hasL) {
      pos.left[i] = items.left.length;
      items.left.push(i);
      l[b]++;
    }
    if (hasR) {
      pos.right[i] = items.right.length;
      items.right.push(i);
      r[b]++;
    }
  });
  close();
  return { items, pos, u: Int32Array.from(u), l: Int32Array.from(l), r: Int32Array.from(r), kind, hunk };
}

/** The last block starting at or before `at` in the prefix array `starts`. */
export function blockAt(starts: Int32Array, at: number): number {
  let lo = 0;
  let hi = starts.length - 2;
  if (hi < 0) return 0;
  while (lo < hi) {
    const mid = (lo + hi + 1) >> 1;
    if (starts[mid] <= at) lo = mid;
    else hi = mid - 1;
  }
  return lo;
}

/** A side's offset (rows) for padded position `u` (rows): each block is crossed proportionally on both sides. */
export function sideOffsetAt(layout: LinkedLayout, side: Side, u: number): number {
  const s = side === "left" ? layout.l : layout.r;
  const n = layout.u.length - 1;
  if (n <= 0) return 0;
  if (u >= layout.u[n]) return s[n] + (u - layout.u[n]);
  const b = blockAt(layout.u, Math.max(0, u));
  const span = layout.u[b + 1] - layout.u[b];
  const t = span ? (Math.max(0, u) - layout.u[b]) / span : 0;
  return s[b] + t * (s[b + 1] - s[b]);
}

/**
 * The one region where `next` differs from `prev` (common prefix and suffix
 * removed): `removed` lines at `start` became `inserted` lines. Null when equal.
 */
export function changedRegion(prev: string[], next: string[]): { start: number; removed: number; inserted: number } | null {
  if (prev === next) return null;
  const max = Math.min(prev.length, next.length);
  let start = 0;
  while (start < max && prev[start] === next[start]) start++;
  let tail = 0;
  while (tail < max - start && prev[prev.length - 1 - tail] === next[next.length - 1 - tail]) tail++;
  const removed = prev.length - start - tail;
  const inserted = next.length - start - tail;
  return removed || inserted ? { start, removed, inserted } : null;
}

/**
 * Keeps sync points on their lines after `side` had `removed` lines at
 * 0-based `start` replaced by `inserted`: later lines move with the text, a
 * pair whose line was deleted is dropped. Returns `pairs` when nothing moved.
 */
export function shiftLinePairs(pairs: LinePair[], side: Side, start: number, removed: number, inserted: number): LinePair[] {
  if (removed === inserted || !pairs.length) return pairs;
  const out: LinePair[] = [];
  for (const p of pairs) {
    const idx = p[side] - 1;
    if (idx >= start + removed) out.push({ ...p, [side]: p[side] + inserted - removed });
    else if (idx < start + inserted) out.push(p);
  }
  return out;
}

/**
 * Line ranges (1-based, inclusive) of `side` after `removed` lines at 0-based
 * `start` were replaced by `inserted` lines: later ranges move, a range ending
 * inside the replaced lines is cut to the new ones, and one that lay wholly
 * inside lines that are now gone is dropped.
 */
export function shiftLineRanges(ranges: LineRange[], start: number, removed: number, inserted: number): LineRange[] {
  if (removed === inserted || !ranges.length) return ranges;
  const delta = inserted - removed;
  const out: LineRange[] = [];
  for (const r of ranges) {
    const [s0, e0] = [r.start - 1, r.end - 1];
    if (e0 < start) out.push(r);
    else if (s0 >= start + removed) out.push({ start: r.start + delta, end: r.end + delta });
    else {
      // Overlaps the replaced lines: keep what lies outside them plus the new lines it covers.
      const first = Math.min(s0, start);
      const last = e0 >= start + removed ? e0 + delta : Math.min(e0, start + inserted - 1);
      if (last >= first && last >= 0) out.push({ start: first + 1, end: last + 1 });
    }
  }
  return out;
}

/** Glyphs shown at line ends with "Show line endings" (dimmed). */
export const EOL_GLYPH: Record<LineEnding, string> = { lf: "↵", crlf: "␍␊", cr: "␍", mixed: "", none: "" };

/** Each line's terminator in `text` (as `splitText` splits it), "none" for an unterminated last line. */
export function eolsOfText(text: string): LineEnding[] {
  const out: LineEnding[] = [];
  const re = /\r\n|\r|\n/g;
  let last = 0;
  for (let m = re.exec(text); m; m = re.exec(text)) {
    out.push(m[0] === "\r\n" ? "crlf" : m[0] === "\r" ? "cr" : "lf");
    last = re.lastIndex;
  }
  if (last < text.length || !out.length) out.push("none");
  return out;
}
