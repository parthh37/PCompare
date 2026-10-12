// Typed bridge to the Text Compare commands in src-tauri/src/text_compare.rs:
// loading with a chosen or detected encoding, diffing with grammars and
// per-line terminators, and saving with automatic backups.
import { invoke } from "@tauri-apps/api/core";
import type { LineEnding, TextDiffOptions, TextDiffResult, TextFileInfo } from "./api";
import type { TextDocument as BaseDocument, TextFormat } from "./mergeApi";
import type { Conversion } from "./convertApi"; // bc5:converters

export interface TextDocument extends BaseDocument {
  /** Each line's terminator as read ("none" for an unterminated last line). */
  eols: LineEnding[];
  /** The encoding was guessed statistically (no BOM, not valid UTF-8). */
  encodingGuessed: boolean;
  /** bc5:converters — the file format rule applied to this side (converted text, read-only, grammar). */
  conversion?: Conversion | null;
}

export interface TextPair {
  left: TextDocument;
  right: TextDocument;
}

/** One side of a diff: lines, their terminators when known, and the path (picks the grammar). */
export interface DiffSide {
  lines: string[];
  eols?: LineEnding[] | null;
  path?: string | null;
}

export interface EncodingChoice {
  /** Passed back to the loaders; a " BOM" suffix means "write a byte-order mark". */
  id: string;
  name: string;
  group: string;
}

export interface GrammarInfo {
  id: string;
  name: string;
  extensions: string[];
}

export type BackupMode = "off" | "bak" | "folder";

export interface BackupOptions {
  mode: BackupMode;
  /** Backup folder for mode "folder". */
  folder: string;
  /** Back up on every save, not just the first save of a file per app session. */
  everySave: boolean;
}

export const DEFAULT_BACKUPS: BackupOptions = { mode: "bak", folder: "", everySave: false };
const BACKUPS_KEY = "pcompare.text.backups";

/** The app-wide backup setting (localStorage `pcompare.text.backups`). */
export function loadBackups(): BackupOptions {
  try {
    const raw = localStorage.getItem(BACKUPS_KEY);
    return raw ? { ...DEFAULT_BACKUPS, ...JSON.parse(raw) } : DEFAULT_BACKUPS;
  } catch {
    return DEFAULT_BACKUPS;
  }
}

export function saveBackups(b: BackupOptions) {
  localStorage.setItem(BACKUPS_KEY, JSON.stringify(b));
}

export interface SavedText {
  info: TextFileInfo;
  /** Where the previous version was backed up, if a backup was made. */
  backup: string | null;
}

let encodings: Promise<EncodingChoice[]> | null = null;
let grammars: Promise<GrammarInfo[]> | null = null;

export const textApi = {
  /** `null` encodings are detected (BOM, UTF-8, then a statistical guess). */
  loadTextPair: (left: string, right: string, leftEncoding: string | null, rightEncoding: string | null) =>
    invoke<TextPair>("text_load_pair", { left, right, leftEncoding, rightEncoding }),
  loadText: (path: string, encoding: string | null) => invoke<TextDocument>("text_load", { path, encoding }),
  diffTexts: (left: DiffSide, right: DiffSide, options: TextDiffOptions) =>
    invoke<TextDiffResult>("text_diff", { left, right, options }),
  saveTextFile: (path: string, text: string, format: TextFormat, backup: BackupOptions | null) =>
    invoke<SavedText>("text_save", { path, text, format, backup }),
  /** Cached: the lists never change while the app runs. */
  encodings: () => (encodings ??= invoke<EncodingChoice[]>("text_encodings")),
  grammars: () => (grammars ??= invoke<GrammarInfo[]>("text_grammars")),
};

/**
 * The terminators a side's lines will have: as read while the side is unedited,
 * otherwise the save format's style everywhere (saving normalizes to it), with
 * none after the last line unless the file ends with a line break.
 */
export function lineEndingsFor(
  lines: string[],
  saved: string[],
  savedEols: LineEnding[] | undefined,
  format: TextFormat,
  trailingNewline: boolean,
): LineEnding[] | null {
  if (lines === saved && savedEols && savedEols.length === lines.length) return savedEols;
  if (format.lineEnding === "mixed" || format.lineEnding === "none") return null;
  return lines.map((_, i) => (i < lines.length - 1 || trailingNewline ? format.lineEnding : "none"));
}

/** Parses "1-6, 73-80, 81-" into column ranges; throws on malformed input. */
export function parseColumns(text: string): { start: number; end: number | null }[] {
  return text
    .split(/[,;\s]+/)
    .filter(Boolean)
    .map((part) => {
      const m = /^(\d+)(?:-(\d*))?$/.exec(part);
      if (!m) throw new Error(`"${part}" is not a column range (e.g. 1-6 or 73-)`);
      const start = Number(m[1]);
      const end = m[2] === undefined ? start : m[2] === "" ? null : Number(m[2]);
      if (start < 1 || (end !== null && end < start)) throw new Error(`"${part}" is not a valid column range`);
      return { start, end };
    });
}

export const formatColumns = (cols: { start: number; end: number | null }[]) =>
  cols.map((c) => (c.end === null ? `${c.start}-` : c.end === c.start ? `${c.start}` : `${c.start}-${c.end}`)).join(", ");

/** Adds the 1-based inclusive range `[start, end]` to `ranges`, merging overlaps. */
export function addRange(ranges: { start: number; end: number }[], start: number, end: number) {
  const all = [...ranges, { start, end }].sort((a, b) => a.start - b.start);
  const out: { start: number; end: number }[] = [];
  for (const r of all) {
    const last = out[out.length - 1];
    if (last && r.start <= last.end + 1) last.end = Math.max(last.end, r.end);
    else out.push({ ...r });
  }
  return out;
}
