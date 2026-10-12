// araxis:text-review — comparison archives (.pcmpz) and review comments.
// Mirrors src-tauri/src/review.rs and pcompare-review (serde camelCase).
import { invoke } from "@tauri-apps/api/core";
import { open, save } from "@tauri-apps/plugin-dialog";
import type { FolderCompareResult, LineEnding, TextDiffOptions, TextFileInfo } from "./api";
import { basename } from "./format";
import { joinLines } from "./mergeApi";
import type { Bookmark } from "./textEdit";

export const REVIEW_EXTENSION = "pcmpz";
const FILTERS = [{ name: "PCompare comparison archive", extensions: [REVIEW_EXTENSION] }];

export const isReviewArchivePath = (path: string) => path.toLowerCase().endsWith(`.${REVIEW_EXTENSION}`);

export interface ReviewManifest {
  format: string;
  version: number;
  kind: "text" | "folder";
  title: string;
  createdMs: number;
  appVersion: string;
}

export interface ReviewTextSide {
  label: string;
  path: string;
  encoding: string;
  hasBom: boolean;
  lineEnding: LineEnding | string;
  size: number;
  /** Absent when the side is empty. */
  text?: string;
}

/** A bookmarked folder-compare row with an optional comment. */
export interface FolderBookmark {
  relPath: string;
  comment?: string;
}

export interface EmbeddedFile {
  relPath: string;
  left: boolean;
  right: boolean;
}

export interface TextReview {
  kind: "text";
  left: ReviewTextSide;
  right: ReviewTextSide;
  base?: ReviewTextSide;
  options: Partial<TextDiffOptions>;
  settings: Record<string, unknown>;
  bookmarks: Bookmark[];
}

export interface FolderReview {
  kind: "folder";
  leftRoot: string;
  rightRoot: string;
  result: FolderCompareResult;
  options: unknown;
  settings: Record<string, unknown>;
  bookmarks: FolderBookmark[];
  /** Filled in when saving. */
  embedded: EmbeddedFile[];
}

export type ReviewComparison = TextReview | FolderReview;

export interface ReviewArchive {
  manifest: ReviewManifest;
  comparison: ReviewComparison;
}

export interface EmbedOptions {
  maxFileBytes: number;
  maxTotalBytes: number;
}

export interface EmbeddedText {
  text: string;
  encoding: string;
  size: number;
}

export const reviewApi = {
  save: (path: string, title: string, comparison: ReviewComparison, embed: EmbedOptions | null) =>
    invoke<{ path: string; embedded: number }>("review_save", { path, title, comparison, embed }),
  open: (path: string) => invoke<ReviewArchive>("review_open", { path }),
  saveBookmarks: (path: string, bookmarks: Bookmark[] | FolderBookmark[]) =>
    invoke<void>("review_save_bookmarks", { path, bookmarks }),
  readFile: (path: string, relPath: string) =>
    invoke<{ left: EmbeddedText | null; right: EmbeddedText | null }>("review_read_file", { path, relPath }),
};

/** Asks where to save an archive; null when cancelled. */
export async function pickSavePath(title: string): Promise<string | null> {
  const stem = title.replace(/[^\w.-]+/g, "_").replace(/^_+|_+$/g, "") || "comparison";
  const path = await save({ title: "Save comparison archive", defaultPath: `${stem}.${REVIEW_EXTENSION}`, filters: FILTERS });
  return path || null;
}

/** Asks for an archive to open; null when cancelled. */
export async function pickArchive(): Promise<string | null> {
  const picked = await open({ title: "Open comparison archive", multiple: false, directory: false, filters: FILTERS });
  return typeof picked === "string" ? picked : null;
}

/** What Text Compare knows about one side, enough to archive it. */
export interface TextSideSource {
  info: TextFileInfo;
  lines: string[];
  trailingNewline: boolean;
  /** Clipboard / in-memory text: `info.path` is a label, not a path. */
  memory: boolean;
}

export function reviewTextSide(s: TextSideSource): ReviewTextSide {
  return {
    label: s.memory ? s.info.path : basename(s.info.path) || s.info.path,
    path: s.memory ? "" : s.info.path,
    encoding: s.info.encoding,
    hasBom: s.info.hasBom,
    lineEnding: s.info.lineEnding,
    size: s.info.size,
    text: joinLines(s.lines, s.trailingNewline),
  };
}

export const archiveTitle = (left: string, right: string) => (left === right ? left : `${left} ↔ ${right}`);

// ---------- bookmark helpers ----------

/** An empty comment is dropped (kept as typed otherwise: this runs on every keystroke). */
const cleanComment = (c: string) => (c.trim() ? c : undefined);

export function setTextComment(marks: Bookmark[], target: Bookmark, comment: string): Bookmark[] {
  return marks.map((b) => (b.side === target.side && b.line === target.line ? { ...b, comment: cleanComment(comment) } : b));
}

/** Adds a bookmark on the line if there is none (keeping an existing one and its comment). */
export function ensureTextBookmark(marks: Bookmark[], side: Bookmark["side"], line: number): Bookmark[] {
  return marks.some((b) => b.side === side && b.line === line) ? marks : [...marks, { side, line }];
}

export function toggleFolderBookmark(marks: FolderBookmark[], relPath: string): FolderBookmark[] {
  return marks.some((b) => b.relPath === relPath)
    ? marks.filter((b) => b.relPath !== relPath)
    : [...marks, { relPath }];
}

export function ensureFolderBookmark(marks: FolderBookmark[], relPath: string): FolderBookmark[] {
  return marks.some((b) => b.relPath === relPath) ? marks : [...marks, { relPath }];
}

export function setFolderComment(marks: FolderBookmark[], relPath: string, comment: string): FolderBookmark[] {
  return marks.map((b) => (b.relPath === relPath ? { ...b, comment: cleanComment(comment) } : b));
}

/** Text bookmarks by `side:line`, for the gutter. */
export const textBookmarkMap = (marks: Bookmark[]) => new Map(marks.map((b) => [`${b.side}:${b.line}`, b]));

/** Same bookmarks and comments, ignoring order. */
export function sameBookmarks<T extends { comment?: string }>(a: T[], b: T[], key: (m: T) => string): boolean {
  if (a.length !== b.length) return false;
  const m = new Map(a.map((x) => [key(x), x.comment ?? ""]));
  return b.every((x) => m.get(key(x)) === (x.comment ?? ""));
}
