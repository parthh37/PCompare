// labels, presets and rules of the sync matrix, mirroring
// crates/pcompare-sync/src/matrix.rs (the engine is the authority: every plan
// carries the matrix it followed, `SyncPlan.matrix`).
import type { SyncAction, SyncCategory, SyncMatrix, SyncMode } from "./syncApi";

export const CATEGORIES: { value: SyncCategory; label: string; hint: string }[] = [
  { value: "leftNewer", label: "Newer on left", hint: "Different, and the left copy is newer (or only it changed since the last sync)." },
  { value: "rightNewer", label: "Newer on right", hint: "Different, and the right copy is newer (or only it changed since the last sync)." },
  { value: "leftOrphan", label: "Only on left", hint: "New on the left, or no earlier sync to tell otherwise." },
  { value: "rightOrphan", label: "Only on right", hint: "New on the right, or no earlier sync to tell otherwise." },
  { value: "leftDeleted", label: "Deleted on left", hint: "Both sides had it at the last sync; the left one is gone since." },
  { value: "rightDeleted", label: "Deleted on right", hint: "Both sides had it at the last sync; the right one is gone since." },
  { value: "different", label: "Different, neither newer", hint: "Contents differ but the timestamps don't say which is newer." },
  { value: "same", label: "Same", hint: "Identical (or only unimportant differences)." },
];

export const ACTIONS: { value: SyncAction; label: string; short: string }[] = [
  { value: "leave", label: "Leave alone", short: "Leave" },
  { value: "copyLeftToRight", label: "Copy left → right", short: "Copy →" },
  { value: "copyRightToLeft", label: "Copy right → left", short: "Copy ←" },
  { value: "deleteLeft", label: "Delete left", short: "Delete left" },
  { value: "deleteRight", label: "Delete right", short: "Delete right" },
  { value: "prompt", label: "Prompt (decide in the preview)", short: "Prompt" },
];

export const actionLabel = (a: SyncAction) => ACTIONS.find((x) => x.value === a)?.label ?? a;

/** Which sides hold items of a category. */
export function categorySides(c: SyncCategory): { left: boolean; right: boolean } {
  if (c === "leftOrphan" || c === "rightDeleted") return { left: true, right: false };
  if (c === "rightOrphan" || c === "leftDeleted") return { left: false, right: true };
  return { left: true, right: true };
}

/** A copy needs its source, a delete its target. */
export function actionApplies(a: SyncAction, onLeft: boolean, onRight: boolean): boolean {
  if (a === "copyLeftToRight" || a === "deleteLeft") return onLeft;
  if (a === "copyRightToLeft" || a === "deleteRight") return onRight;
  return true;
}

const m = (
  leftNewer: SyncAction,
  rightNewer: SyncAction,
  leftOrphan: SyncAction,
  rightOrphan: SyncAction,
  leftDeleted: SyncAction,
  rightDeleted: SyncAction,
  different: SyncAction,
): SyncMatrix => ({ leftNewer, rightNewer, leftOrphan, rightOrphan, leftDeleted, rightDeleted, different, same: "leave" });

const [L, CR, CL, DL, DR, P] = ["leave", "copyLeftToRight", "copyRightToLeft", "deleteLeft", "deleteRight", "prompt"] as const;

/** The fixed modes as matrix rows (`SyncMatrix::for_mode`). */
export const PRESET_MATRICES: Record<Exclude<SyncMode, "custom">, SyncMatrix> = {
  updateLeftToRight: m(CR, L, CR, L, L, CR, P),
  updateRightToLeft: m(L, CL, L, CL, CL, L, P),
  updateBoth: m(CR, CL, CR, CL, CL, CR, P),
  mirrorLeftToRight: m(CR, CR, CR, DR, DR, CR, CR),
  mirrorRightToLeft: m(CL, CL, DL, CL, CL, DL, CL),
  twoWay: m(CR, CL, CR, CL, DR, DL, P),
};

export const ALL_LEAVE: SyncMatrix = m(L, L, L, L, L, L, L);

export const matrixFor = (mode: SyncMode, custom: SyncMatrix | undefined): SyncMatrix =>
  mode === "custom" ? { ...ALL_LEAVE, ...custom } : PRESET_MATRICES[mode];

export const matrixDeletes = (x: SyncMatrix) => Object.values(x).some((a) => a === "deleteLeft" || a === "deleteRight");

/** The fixed mode a matrix is exactly the preset of, if any. */
export function presetOf(x: SyncMatrix): Exclude<SyncMode, "custom"> | null {
  for (const [mode, p] of Object.entries(PRESET_MATRICES))
    if (CATEGORIES.every((c) => p[c.value] === x[c.value])) return mode as Exclude<SyncMode, "custom">;
  return null;
}
