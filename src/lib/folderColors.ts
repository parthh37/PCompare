// The folder views' colour cues, worked out per side. A file takes the colour
// of what it is on that side (the newer copy red, the older grey, an orphan
// purple); a folder's icon shows the two most important colours found below it
// on that side, so the two halves of a row can read differently. Folder Sync
// reuses Folder Compare's tones; Folder Merge adds its own roles.
import type { FolderEntry, Side, StatusCounts, StatusDetail } from "./api";
import { detailOf } from "./folderTree";
import type { FolderMergeEntry, MergeClass, SyncCategory } from "./syncApi";

/** A colour role; each is a `--fc-<tone>` CSS variable (overridable in Settings → Colors). */
export type Tone = "same" | "newer" | "different" | "older" | "orphan" | "unknown" | "error";

/** Every tone, in the order Settings and the legend list them. */
export const TONES: Tone[] = ["same", "newer", "different", "older", "orphan", "unknown", "error"];

/**
 * Folder Merge's own roles, each a `--fm-<tone>` CSS variable: a change made on
 * the left, one made on the right, changes on both sides that merge cleanly,
 * and a conflict.
 */
export type MergeTone = "left" | "right" | "mergeable" | "conflict";

/** Any tone a folder view draws. */
export type AnyTone = Tone | MergeTone;

const MERGE_TONES: ReadonlySet<AnyTone> = new Set<MergeTone>(["left", "right", "mergeable", "conflict"]);

/** Whether a tone is one of Folder Merge's roles (rows in those get a wash of the colour). */
export const isMergeTone = (t: AnyTone): t is MergeTone => MERGE_TONES.has(t);

/** The CSS colour of a tone. */
export const toneColor = (t: AnyTone) =>
  t === "error" ? "var(--warn)" : isMergeTone(t) ? `var(--fm-${t})` : `var(--fc-${t})`;

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

/**
 * How one side of a Folder Compare row is coloured: a file in its tone (name,
 * size and date), a folder by its icon with a plain name and dim details.
 * Ignored rows keep their own muted look, and a lone folder is a plain
 * listing. File or folder is per side, not per row: a file can face a folder.
 */
export function sideLook(e: FolderEntry, side: Side, ignoreMinor: boolean, oneSided: boolean) {
  const meta = e[side];
  const dir = meta ? meta.isDir : e.isDir;
  const toned = !!meta && !e.ignored && !oneSided;
  const tone = toned && !dir ? sideTone(detailOf(e), side, e.newer, ignoreMinor) : null;
  return {
    dir,
    toned,
    tone,
    nameClass: !toned ? "" : dir ? " fct-folder" : ` fct-${tone}`,
    infoClass: !toned ? "" : dir ? " fct-info" : ` fct-${tone}`,
  };
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
export function iconTones(present: ReadonlySet<AnyTone>, groups: readonly AnyTone[][] = ICON_GROUPS): AnyTone[] {
  const picked: AnyTone[] = [];
  for (const group of groups) {
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
  tones: AnyTone[];
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

// ---------- Folder Sync ----------

/** A sync category read as the Folder Compare detail it amounts to. */
const SYNC_DETAIL: Record<SyncCategory, StatusDetail> = {
  leftNewer: "leftNewer",
  rightNewer: "rightNewer",
  leftOrphan: "leftOnly",
  rightOrphan: "rightOnly",
  // Deleted on one side since the last sync: the item is left on the other only.
  leftDeleted: "rightOnly",
  rightDeleted: "leftOnly",
  different: "different",
  same: "same",
};

/**
 * Each side's tone for a sync item, `null` where that side has nothing. The
 * category decides both; without one (a clash, an error, a change on both
 * sides), `has` says which sides hold the item: both read as different, one
 * as an orphan.
 */
export function syncTones(category: SyncCategory | null | undefined, has: Record<Side, boolean>): Record<Side, Tone | null> {
  const detail: StatusDetail = category
    ? SYNC_DETAIL[category]
    : has.left && has.right
      ? "different"
      : has.left
        ? "leftOnly"
        : "rightOnly";
  const tone = (side: Side) =>
    (category ? presentOn(detail, side) : has[side]) ? sideTone(detail, side, null, false) : null;
  return { left: tone("left"), right: tone("right") };
}

// ---------- Folder Merge ----------

export type MergeSide = "left" | "base" | "right";

/**
 * One side's tone for a merge item of this class. A change takes the colour of
 * the side it came from (a change made the same way on both sides counts as
 * the left's), and the copies it replaces are older; a conflict or a clean
 * line-by-line merge colours every side. Two-way merges have no base, so they
 * read like Folder Compare. A side without the item still gets a tone, for its
 * deletion mark and for the folder icons above it.
 */
export function mergeSideTone(cls: MergeClass, side: MergeSide): AnyTone {
  switch (cls) {
    case "unchanged":
    case "merged":
      return "same";
    case "leftChanged":
    case "leftAdded":
    case "leftDeleted":
      return side === "left" ? "left" : "older";
    case "rightChanged":
    case "rightAdded":
    case "rightDeleted":
      return side === "right" ? "right" : "older";
    case "bothChangedSame":
    case "bothAddedSame":
    case "bothDeleted":
      return "left";
    case "conflict":
    case "bothAddedConflict":
    case "deleteModifyConflict":
    case "typeConflict":
    case "different":
      return "conflict";
    case "autoMerged":
      return "mergeable";
    case "error":
      return "error";
    case "leftOnly":
    case "rightOnly":
      return "orphan";
    case "leftNewer":
      return side === "left" ? "newer" : "older";
    case "rightNewer":
      return side === "right" ? "newer" : "older";
  }
}

/**
 * Which tones a Folder Merge folder icon (and a row as a whole) shows first,
 * in the same slot scheme as `ICON_GROUPS`.
 */
// TODO(you): whether a mergeable change outranks a one-sided one, and whether
// left and right changes should share a slot. The default ranks what needs a
// decision first (conflicts, then clean merges), then left, then right.
const MERGE_ICON_GROUPS: AnyTone[][] = [
  ["conflict"],
  ["mergeable"],
  ["left"],
  ["right"],
  ["newer", "different"],
  ["orphan"],
  ["error"],
  ["older", "unknown"],
];

const MERGE_SIDES: MergeSide[] = ["left", "base", "right"];

/** The tone a merge row carries as a whole: its name colour and background tint. */
export const mergeRowTone = (cls: MergeClass): AnyTone =>
  iconTones(new Set(MERGE_SIDES.map((s) => mergeSideTone(cls, s))), MERGE_ICON_GROUPS)[0];

/**
 * Whether an item counts towards the folder icons above it on `side`: where it
 * is, and where it was deleted (the base had it), since a deletion is a change
 * made on that side.
 */
const countsOn = (e: FolderMergeEntry, side: MergeSide) => !!e[side] || (side !== "base" && !!e.base);

/**
 * Each folder's icon on each side, from the tones of everything below it there
 * (a folder with nothing in it shows its own). Keyed by relPath; built once per
 * result.
 */
export function mergeFolderLooks(entries: FolderMergeEntry[]): Map<string, Record<MergeSide, FolderLook>> {
  const out = new Map<string, Record<MergeSide, FolderLook>>();
  /** Adds `e`'s own tones and everything below it to `into`, side by side. */
  const visit = (e: FolderMergeEntry, into?: Record<MergeSide, Set<AnyTone>>) => {
    const below: Record<MergeSide, Set<AnyTone>> = { left: new Set(), base: new Set(), right: new Set() };
    for (const c of e.children) visit(c, below);
    for (const s of MERGE_SIDES) {
      if (!countsOn(e, s)) continue;
      const own = mergeSideTone(e.class, s);
      if (!e.children.length) below[s].add(own);
      into?.[s].add(own);
      below[s].forEach((t) => into?.[s].add(t));
    }
    if (e.isDir) {
      const error = !!e.error;
      const look = (s: MergeSide): FolderLook => {
        below[s].delete("same");
        return { tones: iconTones(below[s], MERGE_ICON_GROUPS), error };
      };
      out.set(e.relPath, { left: look("left"), base: look("base"), right: look("right") });
    }
  };
  entries.forEach((e) => visit(e));
  return out;
}
