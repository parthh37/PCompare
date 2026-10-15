import type { DiffRow, Hunk } from "./api";

/**
 * The "Minor" toolbar toggle (Ignore Unimportant Differences).
 *
 * On (the default), unimportant rows count as same: the engine's hunks are
 * the difference sections, and Diffs / Context hide unimportant lines.
 * Off, unimportant rows are differences too: they show in Diffs, Next/Prev
 * Section stops on them, and they can be copied like any other section.
 */
export function differenceSections(rows: DiffRow[], engineHunks: Hunk[], ignoreMinor: boolean): Hunk[] {
  if (ignoreMinor) return engineHunks;
  const out: Hunk[] = [];
  let start = -1;
  for (let i = 0; i <= rows.length; i++) {
    const differs = i < rows.length && rows[i].kind !== "equal";
    if (differs && start < 0) start = i;
    else if (!differs && start >= 0) {
      out.push({ startRow: start, endRow: i });
      start = -1;
    }
  }
  // An unimportant run touching an important hunk joins it (one section).
  // Splitting them instead would let "Copy to right" take only the
  // important part: change this loop if that suits your workflow better.
  return out;
}

/** Whether a row is a difference under the current Minor setting. */
export const isDifference = (r: DiffRow, ignoreMinor: boolean) =>
  r.kind !== "equal" && !(ignoreMinor && r.kind === "unimportant");

/** Status-bar wording for the caret line's state. */
export function rowStateLabel(r: DiffRow | undefined): string | null {
  if (!r) return null;
  switch (r.kind) {
    case "equal":
      return "Same";
    case "unimportant":
      return "Unimportant difference";
    case "leftOnly":
      return "Left orphan";
    case "rightOnly":
      return "Right orphan";
    default:
      return "Important difference";
  }
}
