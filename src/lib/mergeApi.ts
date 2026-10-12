// Typed bridge to the Text Compare editing and Text Merge commands in
// src-tauri/src/merge.rs. Field names mirror the serde `camelCase` structs in
// pcompare-core (textfile) and pcompare-merge.
import { invoke } from "@tauri-apps/api/core";
import type { LineEnding, TextDiffOptions, TextDiffResult, TextFileInfo } from "./api";

/** How a file is written back: encoding, BOM and the line-ending style to normalize to. */
export interface TextFormat {
  encoding: string;
  hasBom: boolean;
  lineEnding: LineEnding;
}

export interface TextDocument {
  info: TextFileInfo;
  format: TextFormat;
  /** Empty when `binary`. */
  lines: string[];
  trailingNewline: boolean;
  binary: boolean;
}

export interface TextPair {
  left: TextDocument;
  right: TextDocument;
}

export type ChunkKind = "stable" | "leftChanged" | "rightChanged" | "bothSame" | "conflict";

/** 0-based, half-open line range. */
export interface LineRange {
  start: number;
  end: number;
}

export interface MergeChunk {
  kind: ChunkKind;
  base: LineRange;
  left: LineRange;
  right: LineRange;
  /** 1-based conflict number (matches the output's conflict markers). */
  conflict: number | null;
}

export interface MergeStats {
  conflicts: number;
  leftChanges: number;
  rightChanges: number;
  bothSame: number;
}

export interface MergeLoad {
  left: TextDocument;
  /** `null` in a two-way merge (no base file): every difference is then a conflict. */
  base: TextDocument | null;
  right: TextDocument;
  merge: { chunks: MergeChunk[]; stats: MergeStats };
  /** Auto-merged output; unresolved conflicts are wrapped in markers (see TextMerge). */
  output: string;
  /** The left file's format, used when saving the output. */
  outputFormat: TextFormat;
}

export const mergeApi = {
  loadTextPair: (left: string, right: string) => invoke<TextPair>("load_text_pair", { left, right }),
  diffTexts: (left: string[], right: string[], options: TextDiffOptions) =>
    invoke<TextDiffResult>("diff_texts", { left, right, options }),
  saveTextFile: (path: string, text: string, format: TextFormat) =>
    invoke<TextFileInfo>("save_text_file", { path, text, format }),
  /** An empty `base` runs a two-way merge. */
  loadMerge: (left: string, base: string, right: string, options: TextDiffOptions) =>
    invoke<MergeLoad>("load_merge", { left, base, right, options }),
};

/** Lines joined back into file text (the backend normalizes line breaks on save). */
export const joinLines = (lines: string[], trailingNewline: boolean) =>
  lines.join("\n") + (trailingNewline && lines.length ? "\n" : "");
