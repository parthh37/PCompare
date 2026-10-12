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
}

export interface FileApply {
  original: string | null;
  target: string | null;
  missing: boolean;
  hunks: HunkOutcome[];
  error: string | null;
  /** The patched text (patchApplyFile only). */
  text?: string;
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
  applyFile: (path: string, base: string, strip: number, index: number) =>
    invoke<FileApply>("patch_apply_file", { path, base, strip, index }),
};

/** One-line summary of how a file's hunks applied. */
export function applySummary(a: FileApply): string {
  if (a.error) return a.error;
  const failed = a.hunks.filter((h) => !h.applied);
  const moved = a.hunks.filter((h) => h.applied && (h.offset || h.fuzz)).length;
  if (!failed.length) {
    const n = a.hunks.length;
    return `${n} hunk${n === 1 ? "" : "s"} apply cleanly${moved ? ` (${moved} with offset or fuzz)` : ""}`;
  }
  const already = failed.every((h) => h.alreadyApplied);
  return `${failed.length} of ${a.hunks.length} hunk${a.hunks.length === 1 ? "" : "s"} fail${
    already ? " (already applied?)" : ""
  }`;
}

/** Describes one hunk's outcome, e.g. "applied at line 120 (offset +3, fuzz 1)". */
export function hunkOutcomeText(h: HunkOutcome): string {
  if (!h.applied) return h.alreadyApplied ? `already applied (near line ${h.line})` : `failed (expected near line ${h.line})`;
  const extra = [h.offset ? `offset ${h.offset > 0 ? "+" : ""}${h.offset}` : "", h.fuzz ? `fuzz ${h.fuzz}` : ""]
    .filter(Boolean)
    .join(", ");
  return `applied at line ${h.line}${extra ? ` (${extra})` : ""}`;
}
