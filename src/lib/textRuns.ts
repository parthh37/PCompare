// Composes the highlight layers of one Text Compare line (diff segments,
// syntax tokens, find matches) into flat styled runs, and wraps runs into
// fixed-width visual lines for word-wrap mode.
import type { Segment } from "./api";

/** A class applied to `[start, end)` of a line. */
export interface StyledRange {
  start: number;
  end: number;
  cls: string;
}

/** A piece of text with all classes that apply to it (space-separated). */
export interface Run {
  text: string;
  cls: string;
}

/** Changed diff segments as ranges of the joined line text. */
export function segmentRanges(segs: Segment[], cls: string): StyledRange[] {
  const out: StyledRange[] = [];
  let pos = 0;
  for (const s of segs) {
    if (s.changed && s.text) out.push({ start: pos, end: pos + s.text.length, cls });
    pos += s.text.length;
  }
  return out;
}

/** Splits `text` at every range boundary and tags each piece with the classes covering it. */
export function composeRuns(text: string, layers: StyledRange[][]): Run[] {
  const ranges = layers.flat().filter((r) => r.end > r.start && r.start < text.length);
  if (!ranges.length) return text ? [{ text, cls: "" }] : [];
  const cuts = new Set<number>([0, text.length]);
  for (const r of ranges) {
    cuts.add(Math.max(0, r.start));
    cuts.add(Math.min(text.length, r.end));
  }
  const points = [...cuts].sort((a, b) => a - b);
  const out: Run[] = [];
  for (let k = 0; k + 1 < points.length; k++) {
    const a = points[k];
    const b = points[k + 1];
    const cls = ranges
      .filter((r) => r.start <= a && r.end >= b)
      .map((r) => r.cls)
      .join(" ");
    const last = out[out.length - 1];
    if (last && last.cls === cls) last.text += text.slice(a, b);
    else out.push({ text: text.slice(a, b), cls });
  }
  return out;
}

/**
 * Display width of a whole line with tab stops every `tabSize` columns, one
 * column per code point (as `wrapRuns` lays it out).
 */
export function displayWidth(text: string, tabSize: number): number {
  let col = 0;
  for (const ch of text) col = ch === "\t" ? col + tabSize - (col % tabSize) : col + 1;
  return col;
}

/** How many visual lines `text` takes when wrapped at `cols` columns (at least 1). */
export const visualLines = (text: string, cols: number, tabSize: number) =>
  Math.max(1, Math.ceil(displayWidth(text, tabSize) / Math.max(1, cols)));

/**
 * Breaks runs into visual lines of exactly `cols` columns (the last one may be
 * shorter). Tabs are expanded to spaces so every line has a predictable width,
 * which is what keeps wrapped left and right rows aligned.
 */
export function wrapRuns(runs: Run[], cols: number, tabSize: number): Run[][] {
  const width = Math.max(1, cols);
  const lines: Run[][] = [[]];
  let col = 0;
  const emit = (ch: string, cls: string) => {
    if (col === width) {
      lines.push([]);
      col = 0;
    }
    const line = lines[lines.length - 1];
    const last = line[line.length - 1];
    if (last && last.cls === cls) last.text += ch;
    else line.push({ text: ch, cls });
    col++;
  };
  let abs = 0;
  for (const run of runs) {
    for (const ch of run.text) {
      if (ch === "\t") {
        const n = tabSize - (abs % tabSize);
        for (let k = 0; k < n; k++) emit(" ", run.cls);
        abs += n;
      } else {
        emit(ch, run.cls);
        abs++;
      }
    }
  }
  return lines;
}
