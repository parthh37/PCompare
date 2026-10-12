// araxis:text-review — comparison statistics (the Statistics dialog), computed
// from what the views already hold: the text diff rows, or the folder tree.
import type { DiffRow, EntryStatus, FolderEntry, Hunk } from "./api";
import { effectiveStatus } from "./folderTree";

export interface TextSideStats {
  label: string;
  lines: number;
  size: number;
  encoding: string;
  lineEnding: string;
  /** Characters on changed or one-sided lines that differ from the other side. */
  charsChanged: number;
}

export interface TextStats {
  left: TextSideStats;
  right: TextSideStats;
  identical: number;
  changed: number;
  /** Lines only on the right. */
  inserted: number;
  /** Lines only on the left. */
  deleted: number;
  unimportant: number;
  blocks: number;
  /** Share of lines (both sides together) that are identical or unimportant, 0–1. */
  similarity: number;
}

export interface TextSideInput {
  label: string;
  lines: number;
  size: number;
  encoding: string;
  hasBom: boolean;
  lineEnding: string;
}

const segLen = (segs: { text: string; changed: boolean }[] | null, onlyChanged: boolean) =>
  segs ? segs.reduce((n, s) => n + (onlyChanged && !s.changed ? 0 : s.text.length), 0) : 0;

export function textStats(rows: DiffRow[], hunks: Hunk[], left: TextSideInput, right: TextSideInput): TextStats {
  let identical = 0;
  let changed = 0;
  let inserted = 0;
  let deleted = 0;
  let unimportant = 0;
  let charsLeft = 0;
  let charsRight = 0;
  for (const r of rows) {
    switch (r.kind) {
      case "equal":
        identical++;
        break;
      case "unimportant":
        unimportant++;
        break;
      case "changed":
        changed++;
        charsLeft += segLen(r.left, true);
        charsRight += segLen(r.right, true);
        break;
      case "leftOnly":
        deleted++;
        charsLeft += segLen(r.left, false);
        break;
      case "rightOnly":
        inserted++;
        charsRight += segLen(r.right, false);
        break;
    }
  }
  const side = (s: TextSideInput, charsChanged: number): TextSideStats => ({
    label: s.label,
    lines: s.lines,
    size: s.size,
    encoding: `${s.encoding}${s.hasBom ? " BOM" : ""}`,
    lineEnding: s.lineEnding.toUpperCase(),
    charsChanged,
  });
  // Rows pair lines, so a row counts once per side it has a line on.
  const total = left.lines + right.lines;
  const alike = rows.reduce(
    (n, r) => n + (r.kind === "equal" || r.kind === "unimportant" ? (r.leftNo != null ? 1 : 0) + (r.rightNo != null ? 1 : 0) : 0),
    0,
  );
  return {
    left: side(left, charsLeft),
    right: side(right, charsRight),
    identical,
    changed,
    inserted,
    deleted,
    unimportant,
    blocks: hunks.length,
    similarity: total ? alike / total : 1,
  };
}

export interface FolderSideStats {
  files: number;
  folders: number;
  bytes: number;
  /** Files only on this side. */
  orphanFiles: number;
  orphanFolders: number;
  /** Differing files where this side is newer. */
  newer: number;
}

export interface FolderStatsDetail {
  left: FolderSideStats;
  right: FolderSideStats;
  /** Files per status (ignored files count as same, and separately in `ignored`). */
  files: Record<EntryStatus, number>;
  folders: Record<EntryStatus, number>;
  ignored: number;
  /** Bytes of files that differ (both sides). */
  differingBytes: { left: number; right: number };
}

const emptyCounts = (): Record<EntryStatus, number> => ({
  same: 0,
  similar: 0,
  different: 0,
  leftOnly: 0,
  rightOnly: 0,
  typeMismatch: 0,
  error: 0,
});

export function folderStats(entries: FolderEntry[]): FolderStatsDetail {
  const side = (): FolderSideStats => ({ files: 0, folders: 0, bytes: 0, orphanFiles: 0, orphanFolders: 0, newer: 0 });
  const out: FolderStatsDetail = {
    left: side(),
    right: side(),
    files: emptyCounts(),
    folders: emptyCounts(),
    ignored: 0,
    differingBytes: { left: 0, right: 0 },
  };
  const visit = (e: FolderEntry) => {
    const status = effectiveStatus(e);
    // A file on one side and a folder on the other counts as a mismatch, not as either.
    (e.isDir ? out.folders : out.files)[status]++;
    if (!e.isDir && e.ignored) out.ignored++;
    for (const s of ["left", "right"] as const) {
      const m = e[s];
      if (!m) continue;
      const st = out[s];
      if (m.isDir) st.folders++;
      else {
        st.files++;
        st.bytes += m.size;
        if (status === "different" || status === "typeMismatch") out.differingBytes[s] += m.size;
      }
      const orphan = status === (s === "left" ? "leftOnly" : "rightOnly");
      if (orphan) m.isDir ? st.orphanFolders++ : st.orphanFiles++;
    }
    if (!e.isDir && !e.ignored && e.newer) out[e.newer].newer++;
    e.children.forEach(visit);
  };
  entries.forEach(visit);
  return out;
}
