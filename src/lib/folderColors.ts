// Folder Compare's colour cues, worked out per side. A file takes the colour of
// what it is on that side (the newer copy red, the older grey, an orphan
// purple); a folder's icon shows the two most important colours found below it
// on that side, so the two halves of a row can read differently.
import type { FolderEntry, Side, StatusCounts, StatusDetail } from "./api";
import { detailOf } from "./folderTree";

/** A colour role; each is a `--fc-<tone>` CSS variable (overridable in Settings → Colors). */
export type Tone = "same" | "newer" | "different" | "older" | "orphan" | "unknown" | "error";

/** Every tone, in the order Settings and the legend list them. */
export const TONES: Tone[] = ["same", "newer", "different", "older", "orphan", "unknown", "error"];

/** The CSS colour of a tone. */
export const toneColor = (t: Tone) => (t === "error" ? "var(--warn)" : `var(--fc-${t})`);

/**
 * One side of a row with this detail: `newer` is the side the newer file is on.
 * Unimportant differences are same while Minor is on (they count as
 * differences otherwise). A file facing a folder is an orphan on both sides,
 * as two unrelated items would be.
 */
export function sideTone(detail: StatusDetail, side: Side, newer: Side | null, ignoreMinor: boolean): Tone {
  switch (detail) {
    case "same":
    case "sameDiff":
      return "same";
    case "similar":
      if (ignoreMinor) return "same";
      return newer ? (newer === side ? "newer" : "older") : "different";
    case "leftNewer":
      return side === "left" ? "newer" : "older";
    case "rightNewer":
      return side === "right" ? "newer" : "older";
    case "different":
      return "different";
    case "leftOnly":
    case "rightOnly":
    case "typeMismatch":
      return "orphan";
    case "unknown":
      return "unknown";
    case "error":
      return "error";
  }
}

/** The status column's colour for a file row: the pair as a whole, so no newer/older split. */
export function pairTone(detail: StatusDetail, ignoreMinor: boolean): Tone {
  if (detail === "similar") return ignoreMinor ? "older" : "different";
  const t = sideTone(detail, "left", null, ignoreMinor);
  return t === "newer" || t === "older" ? "different" : t;
}

/** Whether rows with `detail` have an item on `side` (orphans leave the other side blank). */
const presentOn = (detail: StatusDetail, side: Side) =>
  side === "left" ? detail !== "rightOnly" : detail !== "leftOnly";

/**
 * Which tones a folder icon shows when several are present below it. Tones in
 * one group share a slot (by default they share a colour too), and the icon
 * shows the first two groups present: top-left triangle, then bottom-right.
 */
// TODO(you): where errors rank, and whether newer/different should take
// separate slots. The default puts errors after orphans and before the greys.
const ICON_GROUPS: Tone[][] = [["newer", "different"], ["orphan"], ["error"], ["older", "unknown"]];

/** Up to two tones for a folder icon from the tones present below it; `["same"]` when there are none. */
export function iconTones(present: ReadonlySet<Tone>): Tone[] {
  const picked: Tone[] = [];
  for (const group of ICON_GROUPS) {
    const hit = group.find((t) => present.has(t));
    if (hit) picked.push(hit);
    if (picked.length === 2) break;
  }
  return picked.length ? picked : ["same"];
}

/** The tones of everything counted below a folder, on one side. */
function countedTones(counts: StatusCounts, side: Side, ignoreMinor: boolean): Set<Tone> {
  const present = new Set<Tone>();
  for (const [d, n] of Object.entries(counts) as [StatusDetail, number][]) {
    if (n > 0 && presentOn(d, side)) present.add(sideTone(d, side, null, ignoreMinor));
  }
  return present;
}

/**
 * How a folder's icon is drawn on one side: `tones` fill it; `hollow` while
 * its contents are still being compared; `unopened` when a non-recursive
 * compare never listed it (drawn as a plain folder).
 */
export interface FolderLook {
  tones: Tone[];
  hollow?: boolean;
  unopened?: boolean;
  error?: boolean;
}

export function folderLook(e: FolderEntry, side: Side, ignoreMinor: boolean): FolderLook {
  if (e.ignored) return { tones: ["same"] };
  const error = !!e.error || e.status === "error";
  if (e.pending) return { tones: ["unknown"], hollow: true, error };
  // One-sided (or facing a file): the whole folder is an orphan here.
  if (!e.left || !e.right || !e.left.isDir || !e.right.isDir) return { tones: ["orphan"], error };
  if (!e.counts) return { tones: ["unknown"], unopened: detailOf(e) === "unknown", error };
  return { tones: iconTones(countedTones(e.counts, side, ignoreMinor)), error };
}
