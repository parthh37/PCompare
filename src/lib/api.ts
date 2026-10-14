// Typed bridge to the Rust commands in src-tauri/src/main.rs.
// Field names mirror the serde `camelCase` structs in pcompare-core.
import { Channel, invoke } from "@tauri-apps/api/core";
import type { CompareKind } from "./sessions";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

export type PathKind = "file" | "directory" | "missing";

// ---------- text ----------
export type WhitespaceMode = "compare" | "ignoreTrailing" | "ignoreChanges" | "ignoreAll";
export type DiffAlgorithm = "patience" | "myers";

// bc5:text-engine — alignment, grammar, column, page, replacement and manual rules
/** How lines inside a block of differences are paired: most similar first, by position, or not at all. */
export type Alignment = "similar" | "positional" | "unaligned";
/** 1-based inclusive character columns; `end: null` runs to the end of the line. */
export interface ColumnRange {
  start: number;
  end: number | null;
}
/** 1-based inclusive line range. */
export interface LineRange {
  start: number;
  end: number;
}
/** A left and a right line number (1-based). */
export interface LinePair {
  left: number;
  right: number;
}
/** "Ignore replaced text": `left` on the left side corresponds to `right` on the right side. */
export interface Replacement {
  left: string;
  right: string;
  /** `left` is a regex; `right` may use `$1`. */
  regex: boolean;
}

export interface TextDiffOptions {
  whitespace: WhitespaceMode;
  ignoreCase: boolean;
  algorithm: DiffAlgorithm;
  /** Regexes for text that never matters; matches are removed before comparing. */
  ignorePatterns: string[];
  /** Grammar id (see `textApi.grammars`); null/"" = detect from file extension, "text" = none. */
  grammar: string | null;
  /** Comments (per grammar) are unimportant. */
  ignoreComments: boolean;
  /** String literal contents (per grammar) are unimportant. */
  ignoreStrings: boolean;
  /** Ask for `leftSpans` / `rightSpans` grammar tokens on every row (syntax highlighting). */
  syntaxSpans: boolean;
  ignoreColumns: ColumnRange[];
  /** Lines matching these regexes are page headers/footers (unimportant). */
  pageHeaderPatterns: string[];
  /** First / last N lines of every form-feed-delimited page are headers / footers. */
  pageHeaderLines: number;
  pageFooterLines: number;
  /** CRLF vs LF vs CR is a difference (needs per-line `eols`). */
  compareLineEndings: boolean;
  replacements: Replacement[];
  alignment: Alignment;
  /** Lines marked unimportant by the user, per side. */
  unimportantLines: { left: LineRange[]; right: LineRange[] };
  /** Forced alignment: each left line is shown next to its right line. */
  anchors: LinePair[];
  /** Forced separation: these line pairs are never shown side by side. */
  breaks: LinePair[];
  // araxis:text-engine
  /** Numbers compare by value, within a tolerance (absent = off). */
  numericTolerance?: import("./textApi").NumericTolerance;
  /** Begin/end regex pairs bounding wholly unimportant blocks of lines. */
  ignoreBlocks?: import("./textApi").BlockPattern[];
  // araxis:text-engine end
}

export const DEFAULT_TEXT_OPTIONS: TextDiffOptions = {
  whitespace: "compare",
  ignoreCase: false,
  algorithm: "patience",
  ignorePatterns: [],
  grammar: null,
  ignoreComments: false,
  ignoreStrings: false,
  syntaxSpans: false,
  ignoreColumns: [],
  pageHeaderPatterns: [],
  pageHeaderLines: 0,
  pageFooterLines: 0,
  compareLineEndings: false,
  replacements: [],
  alignment: "similar",
  unimportantLines: { left: [], right: [] },
  anchors: [],
  breaks: [],
};

export type RowKind = "equal" | "unimportant" | "changed" | "leftOnly" | "rightOnly";

export interface Segment {
  text: string;
  changed: boolean;
}

/**
 * A grammar token of one line (bc5:text-engine). `start`/`end` are UTF-16 offsets into the
 * line's full text (its segments joined), sorted and non-overlapping.
 */
export interface TokenSpan {
  start: number;
  end: number;
  kind: "comment" | "string" | "keyword" | "number";
}

export interface DiffRow {
  /** "unimportant" rows may have one side null: an orphan line that is wholly unimportant. */
  kind: RowKind;
  leftNo: number | null;
  rightNo: number | null;
  left: Segment[] | null;
  right: Segment[] | null;
  /** Grammar tokens; only present with `options.syntaxSpans` and a grammar. */
  leftSpans?: TokenSpan[];
  rightSpans?: TokenSpan[];
  /** Line terminators; only present with `options.compareLineEndings`. */
  leftEol?: LineEnding;
  rightEol?: LineEnding;
}

export interface Hunk {
  startRow: number;
  endRow: number;
}

export interface TextDiffResult {
  rows: DiffRow[];
  hunks: Hunk[];
  stats: { equal: number; unimportant: number; changed: number; leftOnly: number; rightOnly: number };
  identical: boolean;
  /** Grammar id used per side (bc5:text-engine); absent from older commands. */
  leftGrammar?: string | null;
  rightGrammar?: string | null;
}

export type LineEnding = "lf" | "crlf" | "cr" | "mixed" | "none";

export interface TextFileInfo {
  path: string;
  encoding: string;
  hasBom: boolean;
  lineEnding: LineEnding;
  size: number;
}

export interface TextFileCompare {
  left: TextFileInfo;
  right: TextFileInfo;
  diff: TextDiffResult | null;
}

// ---------- folder ----------
/** How file contents are read after the quick (size/timestamp) tests. */
export type ContentMethod = "off" | "binary" | "crc" | "rules" | "version";

/** DOS attribute bits of `EntryMeta.attributes` (mirrors `folder::attr`). */
export const ATTR = { readonly: 0x1, hidden: 0x2, system: 0x4, archive: 0x20 } as const;

export interface FolderFilters {
  includeFiles: string[];
  excludeFiles: string[];
  excludeFolders: string[];
  minSize: number | null;
  maxSize: number | null;
  modifiedAfterMs: number | null;
  modifiedBeforeMs: number | null;
  /** `ATTR` bits; entries carrying any are skipped. */
  excludeAttributes: number;
  excludeExecutable: boolean;
  includeContaining: string;
  excludeContaining: string;
  containingRegex: boolean;
  containingMatchCase: boolean;
}

export interface AlignRule {
  left: string;
  right: string;
}

export interface AlignOptions {
  unicode: boolean;
  ignoreExtensions: boolean;
  extensionGroups: string[];
  rules: AlignRule[];
}

export interface FolderCompareOptions {
  compareSize: boolean;
  compareTimestamp: boolean;
  content: ContentMethod;
  overrideQuickTests: boolean;
  rules: TextDiffOptions;
  recursive: boolean;
  exclude: string[];
  filters: FolderFilters;
  includeHidden: boolean;
  ignoreNameCase: boolean;
  align: AlignOptions;
  compareAttributes: boolean;
  followLinks: boolean;
  timeToleranceSecs: number;
  ignoreDstShift: boolean;
  // araxis:folder — files above the threshold (bytes) use `largeFileMethod` instead of `content`.
  largeFileThreshold?: number | null;
  largeFileMethod?: LargeFileMethod;
  // araxis:folder end
}

/** araxis:folder — how content comparison treats files above the size threshold. */
export type LargeFileMethod = "binary" | "size" | "sizeTimestamp";

export type EntryStatus = "same" | "similar" | "different" | "leftOnly" | "rightOnly" | "typeMismatch" | "error";
export type Side = "left" | "right";

export interface EntryMeta {
  size: number;
  modifiedMs: number | null;
  isDir: boolean;
  /** `ATTR` bits, where the file system reports them. */
  attributes?: number;
  /** Unix permission bits. */
  mode?: number;
  /** Set for symbolic links / junctions: where they point. */
  linkTarget?: string;
}

export interface FolderEntry {
  name: string;
  relPath: string;
  isDir: boolean;
  status: EntryStatus;
  newer: Side | null;
  left: EntryMeta | null;
  right: EntryMeta | null;
  children: FolderEntry[];
  error: string | null;
  contentChecked: ContentMethod | null;
  crc: [number, number] | null;
  /** The left side's own relative path when alignment paired differently named entries. */
  leftRelPath?: string;
  /** UI only: the user marked it "ignore", so it counts as same. */
  ignored?: boolean;
  /** UI only: a streamed pair whose contents aren't compared yet. */
  pending?: boolean;
}

/** One folder's rows streamed by a running compare (`FolderLevel` in pcompare-core); no `children`. */
export interface FolderLevel {
  rel: string;
  entries: FolderEntry[];
  /** False: just listed, two-sided rows not compared yet. True: final rows. */
  complete: boolean;
}

/** One message on a folder compare's channel (`ScanUpdate` in src-tauri). */
interface ScanUpdate {
  progress: ScanProgress | null;
  levels: FolderLevel[];
  end: boolean;
}

/** Progress of a running folder compare. */
export interface ScanProgress {
  scanned: number;
  folders: number;
  compared: number;
  current: string;
}

export interface FolderStats {
  same: number;
  similar: number;
  different: number;
  leftOnly: number;
  rightOnly: number;
  errors: number;
  /** UI only: files marked "ignore" (included in `same`). */
  ignored?: number;
}

export interface FolderCompareResult {
  leftRoot: string;
  rightRoot: string;
  entries: FolderEntry[];
  stats: FolderStats;
  elapsedMs: number;
}

export interface ContentVerdict {
  status: EntryStatus;
  crc: [number, number] | null;
}

// ---------- hex ----------
export interface HexSummary {
  leftSize: number;
  rightSize: number;
  firstDifference: number | null;
  differingBytes: number;
  identical: boolean;
}

export interface HexChunk {
  offset: number;
  left: number[];
  right: number[];
}

// bc5:hex — editable, aligned hex sessions (src-tauri/src/hex.rs; wrappers in hexApi.ts)
export type HexRowFilter = "all" | "differences" | "context";
export type HexAlignMode = "aligned" | "offset";

export interface HexView {
  bytesPerRow: number;
  filter: HexRowFilter;
  /** Rows shown around each difference with the "context" filter. */
  context: number;
}

export interface HexState {
  /** Bumped whenever content or layout changes; rows from older versions are stale. */
  version: number;
  sizes: [number, number];
  dirty: [boolean, boolean];
  canUndo: boolean;
  canRedo: boolean;
  mode: HexAlignMode;
  /** Why the files were compared offset by offset, if they were. */
  note: string | null;
  /** Edited since the last full compare; differences are approximate until re-aligned. */
  stale: boolean;
  displayLen: number;
  diffBlocks: number;
  differingBytes: number;
  identical: boolean;
  /** Rows in the requested view. */
  rowCount: number;
}

export interface HexRow {
  /** Visible row index in the view. */
  index: number;
  /** Row on the unfiltered display axis. */
  row: number;
  /** Each side's offset of the first byte at or after the row start. */
  start: [number, number];
  /** `null` cells are gaps where the other side has inserted bytes. */
  left: (number | null)[];
  right: (number | null)[];
  /** The view hides rows above this one. */
  breakBefore: boolean;
}

export interface HexTarget {
  display: number;
  /** Visible row in the view, or null if the view hides it. */
  row: number | null;
  start: [number, number];
}

export interface HexEditAt {
  side: 0 | 1;
  offset: number;
}

// ---------- saved sessions ----------
export type SavedKind = CompareKind;

export interface SavedSession {
  id: string;
  name: string;
  kind: SavedKind;
  left: string;
  right: string;
  /** 3-way sessions only. */
  base?: string;
  output?: string;
  settings: Record<string, unknown> | null;
  createdMs: number;
  modifiedMs: number;
}

export interface SessionInput {
  id?: string;
  name: string;
  kind: SavedKind;
  left: string;
  right: string;
  base?: string;
  output?: string;
  settings?: unknown;
}

// ---------- connection profiles ----------
export type ProviderKind =
  | "s3"
  | "azblob"
  | "gcs"
  | "b2"
  | "ftp"
  | "sftp"
  | "webdav"
  | "onedrive"
  | "dropbox"
  | "gdrive"
  | "sharepoint"
  | "github";

export type FieldKind = "text" | "secret" | "number" | "checkbox" | "select" | "file";

export interface FieldSpec {
  key: string;
  label: string;
  kind: FieldKind;
  required: boolean;
  placeholder: string;
  help: string;
  default: string;
  options: string[];
  showIf: [string, string] | null;
  /** Shown under "Advanced" in the form. */
  advanced: boolean;
}

export interface ProviderSpec {
  kind: ProviderKind;
  label: string;
  description: string;
  fields: FieldSpec[];
  oauth: {
    authUrl: string;
    tokenUrl: string;
    scopes: string;
    consoleUrl: string;
    /** This build ships its own app registration, so a Client ID is optional. */
    hasBuiltinClient: boolean;
  } | null;
}

export interface Profile {
  id: string;
  name: string;
  kind: ProviderKind;
  settings: Record<string, string>;
  /** Which secrets are stored in the OS keychain — values never reach the UI. */
  storedSecrets: string[];
  createdMs: number;
  modifiedMs: number;
}

export interface ProfileInput {
  id?: string;
  name: string;
  kind: ProviderKind;
  settings: Record<string, string>;
  /** Only changed secrets: a value replaces, "" deletes, absent keeps. */
  secrets: Record<string, string>;
}

export interface RemoteEntry {
  name: string;
  isDir: boolean;
  size: number;
  modifiedMs: number | null;
}

export const REMOTE_SCHEME = "remote://";
export const isRemote = (path: string) => path.toLowerCase().startsWith(REMOTE_SCHEME);

export const api = {
  pathKind: (path: string) => invoke<PathKind>("path_kind", { path }),
  /** bc5:reports-script — paths plus viewer / title / read-only switches (see cliApi.ts). */
  launchArgs: () => invoke<import("./cliApi").LaunchOptions>("launch_args"),
  /** "Select Left Side for Compare" — shared with the Explorer context menu. */
  getLeftSelection: () => invoke<string | null>("get_left_selection"),
  setLeftSelection: (path: string | null) => invoke<void>("set_left_selection", { path }),
  compareTextFiles: (left: string, right: string, options: TextDiffOptions) =>
    invoke<TextFileCompare>("compare_text_files", { left, right, options }),
  /**
   * `runKey` makes the run cancellable with `cancelCompareFolders`. With
   * `onLevels`, rows stream in folder by folder and the result has no
   * `entries`; it resolves once every streamed level has been delivered.
   */
  compareFolders: async (
    left: string,
    right: string,
    options: FolderCompareOptions,
    runKey?: string,
    onProgress?: (p: ScanProgress) => void,
    expandArchives?: boolean, // bc5:archives — list archives in the trees as folders
    onLevels?: (levels: FolderLevel[]) => void,
  ) => {
    const channel = new Channel<ScanUpdate>();
    let ended = () => {};
    const end = new Promise<void>((resolve) => (ended = resolve));
    channel.onmessage = (m) => {
      if (m.progress) onProgress?.(m.progress);
      if (m.levels.length) onLevels?.(m.levels);
      if (m.end) ended();
    };
    const res = await invoke<FolderCompareResult>("compare_folders", {
      left,
      right,
      options,
      runKey,
      onProgress: channel,
      expandArchives,
      stream: !!onLevels,
    });
    // The reply can overtake the channel's last messages.
    if (onLevels) await end;
    return res;
  },
  cancelCompareFolders: (runKey: string) => invoke<void>("compare_folders_cancel", { runKey }),
  compareFileContents: (
    left: string,
    leftSize: number,
    right: string,
    rightSize: number,
    method: ContentMethod,
    rules: TextDiffOptions,
  ) => invoke<ContentVerdict>("compare_file_contents", { left, leftSize, right, rightSize, method, rules }),

  /** Watches local paths; resolves to how many could be watched (remote ones can't). */
  watchPaths: (id: string, paths: string[], exclude: string[]) =>
    invoke<number>("watch_paths", { id, paths, exclude }),
  unwatchPaths: (id: string) => invoke<void>("unwatch_paths", { id }),
  onFsChanged: (handler: (id: string) => void): Promise<UnlistenFn> =>
    listen<{ id: string }>("fs-changed", (e) => handler(e.payload.id)),
  hexSummary: (left: string, right: string) => invoke<HexSummary>("hex_summary", { left, right }),
  hexChunk: (left: string, right: string, offset: number, len: number) =>
    invoke<HexChunk>("hex_chunk", { left, right, offset, len }),
  hexNextDifference: (left: string, right: string, from: number) =>
    invoke<number | null>("hex_next_difference", { left, right, from }),

  listSessions: () => invoke<SavedSession[]>("list_sessions"),
  saveSession: (input: SessionInput) => invoke<SavedSession>("save_session", { input }),
  renameSession: (id: string, name: string) => invoke<SavedSession>("rename_session", { id, name }),
  deleteSession: (id: string) => invoke<void>("delete_session", { id }),

  listProviders: () => invoke<ProviderSpec[]>("list_providers"),
  oauthRedirectUri: () => invoke<string>("oauth_redirect_uri"),
  listProfiles: () => invoke<Profile[]>("list_profiles"),
  saveProfile: (input: ProfileInput) => invoke<Profile>("save_profile", { input }),
  deleteProfile: (id: string) => invoke<void>("delete_profile", { id }),
  testProfile: (input: ProfileInput) => invoke<string>("test_profile", { input }),
  /** Folders under `path` (relative to the account's top level) for the Root folder picker. */
  browseProfile: (input: ProfileInput, path: string) => invoke<RemoteEntry[]>("browse_profile", { input, path }),
  signInProfile: (id: string) => invoke<Profile>("sign_in_profile", { id }),
  /** Stops a pending sign-in; `signInProfile` then rejects with SIGN_IN_CANCELLED. */
  cancelSignIn: () => invoke<void>("cancel_sign_in"),
  openProviderConsole: (kind: ProviderKind) => invoke<void>("open_provider_console", { kind }),
  remoteList: (path: string) => invoke<RemoteEntry[]>("remote_list", { path }),
};

/** The error a cancelled sign-in rejects with (oauth.rs CANCELLED). */
export const SIGN_IN_CANCELLED = "sign-in cancelled";

export function errorMessage(e: unknown): string {
  return typeof e === "string" ? e : e instanceof Error ? e.message : JSON.stringify(e);
}
