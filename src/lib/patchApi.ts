// Typed bridge to the Patch Viewer commands in src-tauri/src/patch.rs:
// parsing a .patch/.diff into side-by-side hunks and applying it against a
// folder. Mirrors the structs in crates/pcompare-core/src/patch.rs.
import { invoke } from "@tauri-apps/api/core";
import type { DiffRow } from "./api";

export type ChangeKind = "modified" | "added" | "deleted" | "renamed" | "copied";
export type PatchFormat = "unified" | "git" | "context";

export interface PatchHunkView {
  oldStart: number;
  oldLines: number;
  newStart: number;
  newLines: number;
  /** Text after the closing `@@` (often the enclosing function). */
  section: string;
  rows: DiffRow[];
}

export interface PatchFileView {
  /** Paths as written in the patch; null for /dev/null. */
  oldPath: string | null;
  newPath: string | null;
  /** The path to show (default prefix stripped). */
  display: string;
  change: ChangeKind;
  binary: boolean;
  oldMode: string | null;
  newMode: string | null;
  similarity: number | null;
  headers: string[];
  added: number;
  removed: number;
  hunks: PatchHunkView[];
}

export interface PatchView {
  format: PatchFormat;
  /** Commit message or mail headers before the first file. */
  preamble: string;
  strip: number;
  warnings: string[];
  encoding: string;
  files: PatchFileView[];
}

export interface HunkOutcome {
  applied: boolean;
  /** 1-based line in the original where the hunk applied (or was expected). */
  line: number;
  offset: number;
  fuzz: number;
  alreadyApplied: boolean;
  /** Left out on purpose (the hunk wasn't selected). */
  skipped?: boolean;
  /** Why a failed hunk didn't apply. */
  reason?: string;
}

/** What writing a file did on disk (patchWriteSelected). */
export type WriteAction = "modified" | "created" | "deleted" | "renamed" | "copied";

export interface FileApply {
  original: string | null;
  target: string | null;
  missing: boolean;
  hunks: HunkOutcome[];
  error: string | null;
  /** The patched text (patchApplyFile only). */
  text?: string;
  /** What was written (patchWriteSelected only); absent = nothing changed on disk. */
  written?: WriteAction;
}

/** Which hunks of a file to apply; `hunks` absent = all of them. */
export interface FileSelection {
  index: number;
  hunks?: number[];
}

export interface WrittenFile extends FileApply {
  /** The file's index in the patch. */
  index: number;
}

/** A Text Compare side given as text rather than a file (`TextSettings.memory`). */
export interface MemorySide {
  text: string;
  label: string;
}

export interface PatchCheck {
  strip: number;
  files: FileApply[];
}

export const patchApi = {
  load: (path: string) => invoke<PatchView>("patch_load", { path }),
  /** `strip` null = detect from the files that exist under `base`. */
  check: (path: string, base: string, strip: number | null) =>
    invoke<PatchCheck>("patch_check", { path, base, strip }),
  /** `hunks` limits it to those hunk indices; absent = all. */
  applyFile: (path: string, base: string, strip: number, index: number, hunks?: number[]) =>
    invoke<FileApply>("patch_apply_file", { path, base, strip, index, hunks: hunks ?? null }),
  /** Writes the selected hunks into the real files under `base` (deleted/renamed originals go to the Trash). */
  writeSelected: (path: string, base: string, strip: number, files: FileSelection[]) =>
    invoke<WrittenFile[]>("patch_write_selected", { path, base, strip, files }),
};

/** One-line summary of how a file's hunks applied. */
export function applySummary(a: FileApply): string {
  if (a.error) return a.error;
  const tried = a.hunks.filter((h) => !h.skipped);
  const skipped = a.hunks.length - tried.length;
  const skipNote = skipped ? `, ${skipped} skipped` : "";
  const failed = tried.filter((h) => !h.applied);
  const moved = tried.filter((h) => h.applied && (h.offset || h.fuzz)).length;
  if (!failed.length) {
    const n = tried.length;
    return `${n} hunk${n === 1 ? "" : "s"} apply cleanly${moved ? ` (${moved} with offset or fuzz)` : ""}${skipNote}`;
  }
  const already = failed.every((h) => h.alreadyApplied);
  return `${failed.length} of ${tried.length} hunk${tried.length === 1 ? "" : "s"} fail${
    already ? " (already applied?)" : ""
  }${skipNote}`;
}

/** Describes one hunk's outcome, e.g. "applied at line 120 (offset +3, fuzz 1)". */
export function hunkOutcomeText(h: HunkOutcome): string {
  if (h.skipped) return "skipped (not selected)";
  if (!h.applied)
    return h.alreadyApplied
      ? `already applied (near line ${h.line})`
      : `failed: ${h.reason ?? `expected near line ${h.line}`}`;
  const extra = [h.offset ? `offset ${h.offset > 0 ? "+" : ""}${h.offset}` : "", h.fuzz ? `fuzz ${h.fuzz}` : ""]
    .filter(Boolean)
    .join(", ");
  return `applied at line ${h.line}${extra ? ` (${extra})` : ""}`;
}
