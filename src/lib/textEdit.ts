// Pure line-editing helpers for the editable Text Compare view. Everything
// here works on plain line arrays and diff rows, never on React state, so the
// structural edits (split, join, paste, delete, copy) are easy to reason about.
import type { DiffRow } from "./api";

export type Side = "left" | "right";
export const otherSide = (s: Side): Side => (s === "left" ? "right" : "left");
export const lineNoOf = (r: Pick<DiffRow, "leftNo" | "rightNo">, side: Side) =>
  side === "left" ? r.leftNo : r.rightNo;

/** 0-based, half-open line range on one side. */
export interface LineSpan {
  start: number;
  end: number;
}

/**
 * The lines one side contributes to rows `[startRow, endRow)`. When that side
 * has no lines there, the range is empty and sits right after the last line
 * shown above the rows, i.e. where copied or newly typed lines get inserted.
 */
export function lineSpan(rows: DiffRow[], startRow: number, endRow: number, side: Side): LineSpan {
  let start = -1;
  let end = -1;
  for (let i = startRow; i < endRow; i++) {
    const n = lineNoOf(rows[i], side);
    if (n == null) continue;
    if (start < 0) start = n - 1;
    end = n;
  }
  if (start >= 0) return { start, end };
  for (let i = startRow - 1; i >= 0; i--) {
    const n = lineNoOf(rows[i], side);
    if (n != null) return { start: n, end: n };
  }
  return { start: 0, end: 0 };
}

/** Maps each 0-based line of `side` to the row showing it. */
export function lineToRow(rows: DiffRow[], side: Side, lineCount: number): Int32Array {
  const map = new Int32Array(lineCount).fill(-1);
  rows.forEach((r, i) => {
    const n = lineNoOf(r, side);
    if (n != null && n <= lineCount) map[n - 1] = i;
  });
  return map;
}

/** `lines` with `[start, end)` replaced by `insert`. */
export function replaceLines(lines: string[], start: number, end: number, insert: string[]): string[] {
  return lines.slice(0, start).concat(insert, lines.slice(end));
}

/** Splits clipboard or file text into lines, accepting any line-break style. */
export function splitText(text: string): { lines: string[]; trailingNewline: boolean } {
  const lines = text.split(/\r\n|\r|\n/);
  const trailingNewline = lines.length > 1 && lines[lines.length - 1] === "";
  if (trailingNewline) lines.pop();
  return { lines, trailingNewline };
}

/** Result of a structural edit: the new lines plus where the caret ends up. */
export interface EditResult {
  lines: string[];
  line: number;
  caret: number;
  /** Lines inserted (positive) or removed (negative), counted at `at`. */
  delta: number;
  at: number;
}

/**
 * Replaces the selection `[selStart, selEnd)` of line `line` with `text`,
 * which may contain line breaks (typed Enter, multi-line paste).
 */
export function insertAt(lines: string[], line: number, selStart: number, selEnd: number, text: string): EditResult {
  const cur = lines[line] ?? "";
  const before = cur.slice(0, selStart);
  const after = cur.slice(selEnd);
  const parts = text.split(/\r\n|\r|\n/);
  if (parts.length === 1) {
    const next = lines.slice();
    next[line] = before + text + after;
    return { lines: next, line, caret: before.length + text.length, delta: 0, at: line };
  }
  const last = parts[parts.length - 1];
  const inserted = [before + parts[0], ...parts.slice(1, -1), last + after];
  return {
    lines: replaceLines(lines, line, line + 1, inserted),
    line: line + parts.length - 1,
    caret: last.length,
    delta: parts.length - 1,
    at: line + 1,
  };
}

/** Backspace at the start of `line`: joins it onto the previous line. */
export function joinWithPrevious(lines: string[], line: number): EditResult | null {
  if (line <= 0 || line >= lines.length) return null;
  const prev = lines[line - 1];
  return {
    lines: replaceLines(lines, line - 1, line + 1, [prev + lines[line]]),
    line: line - 1,
    caret: prev.length,
    delta: -1,
    at: line,
  };
}

/** Delete at the end of `line`: pulls the next line up onto it. */
export function joinWithNext(lines: string[], line: number): EditResult | null {
  if (line < 0 || line >= lines.length - 1) return null;
  const cur = lines[line];
  return {
    lines: replaceLines(lines, line, line + 2, [cur + lines[line + 1]]),
    line,
    caret: cur.length,
    delta: -1,
    at: line + 1,
  };
}

/**
 * Copies the lines `from` shows in rows `[startRow, endRow)` over the lines
 * the other side shows there. Returns the destination's new lines and the
 * replaced range, or null when nothing would change.
 */
export function copyRows(
  rows: DiffRow[],
  startRow: number,
  endRow: number,
  from: Side,
  fromLines: string[],
  toLines: string[],
): { lines: string[]; span: LineSpan; inserted: number } | null {
  const to = otherSide(from);
  const src = lineSpan(rows, startRow, endRow, from);
  const dst = lineSpan(rows, startRow, endRow, to);
  const insert = fromLines.slice(src.start, src.end);
  if (!insert.length && dst.start === dst.end) return null;
  return { lines: replaceLines(toLines, dst.start, dst.end, insert), span: dst, inserted: insert.length };
}

/** Bookmarked line (1-based) on one side. */
export interface Bookmark {
  side: Side;
  line: number;
}

/**
 * Keeps bookmarks on their lines after `side` had `removed` lines at 0-based
 * `start` replaced by `inserted` lines. Bookmarks inside a removed range move
 * to its start; duplicates are dropped.
 */
export function shiftBookmarks(
  marks: Bookmark[],
  side: Side,
  start: number,
  removed: number,
  inserted: number,
): Bookmark[] {
  if (removed === inserted) return marks;
  const seen = new Set<string>();
  const out: Bookmark[] = [];
  for (const m of marks) {
    let line = m.line;
    if (m.side === side) {
      const idx = line - 1;
      if (idx >= start + removed) line += inserted - removed;
      else if (idx >= start + inserted) line = start + Math.max(inserted, 1);
    }
    const key = `${m.side}:${line}`;
    if (line >= 1 && !seen.has(key)) {
      seen.add(key);
      out.push({ side: m.side, line });
    }
  }
  return out;
}

/** Display width of `text` up to `index`, with tabs advancing to the next stop. */
export function columnOf(text: string, index: number, tabSize: number): number {
  let col = 0;
  for (let i = 0; i < index && i < text.length; i++) col = text[i] === "\t" ? col + tabSize - (col % tabSize) : col + 1;
  return col;
}

/** The character index whose cell contains display column `col` (inverse of `columnOf`). */
export function indexAtColumn(text: string, col: number, tabSize: number): number {
  let c = 0;
  for (let i = 0; i < text.length; i++) {
    const w = text[i] === "\t" ? tabSize - (c % tabSize) : 1;
    if (col < c + w / 2) return i;
    c += w;
  }
  return text.length;
}
