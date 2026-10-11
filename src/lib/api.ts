// Typed bridge to the Rust commands in src-tauri/src/main.rs.
// Field names mirror the serde `camelCase` structs in pcompare-core.
import { invoke } from "@tauri-apps/api/core";
import type { CompareKind } from "./sessions";

export type PathKind = "file" | "directory" | "missing";

// ---------- text ----------
export type WhitespaceMode = "compare" | "ignoreTrailing" | "ignoreChanges" | "ignoreAll";
export type DiffAlgorithm = "patience" | "myers";

export interface TextDiffOptions {
  whitespace: WhitespaceMode;
  ignoreCase: boolean;
  algorithm: DiffAlgorithm;
}

export type RowKind = "equal" | "unimportant" | "changed" | "leftOnly" | "rightOnly";

export interface Segment {
  text: string;
  changed: boolean;
}

export interface DiffRow {
  kind: RowKind;
  leftNo: number | null;
  rightNo: number | null;
  left: Segment[] | null;
  right: Segment[] | null;
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
export type CompareCriteria = "sizeAndTimestamp" | "size" | "content";

export interface FolderCompareOptions {
  criteria: CompareCriteria;
  recursive: boolean;
  exclude: string[];
  includeHidden: boolean;
  ignoreNameCase: boolean;
  timeToleranceSecs: number;
  ignoreDstShift: boolean;
}

export type EntryStatus = "same" | "different" | "leftOnly" | "rightOnly" | "typeMismatch" | "error";
export type Side = "left" | "right";

export interface EntryMeta {
  size: number;
  modifiedMs: number | null;
  isDir: boolean;
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
}

export interface FolderCompareResult {
  leftRoot: string;
  rightRoot: string;
  entries: FolderEntry[];
  stats: { same: number; different: number; leftOnly: number; rightOnly: number; errors: number };
  elapsedMs: number;
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
  | "gdrive";

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
}

export interface ProviderSpec {
  kind: ProviderKind;
  label: string;
  description: string;
  fields: FieldSpec[];
  oauth: { authUrl: string; tokenUrl: string; scopes: string; consoleUrl: string } | null;
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
  launchArgs: () => invoke<string[]>("launch_args"),
  compareTextFiles: (left: string, right: string, options: TextDiffOptions) =>
    invoke<TextFileCompare>("compare_text_files", { left, right, options }),
  compareFolders: (left: string, right: string, options: FolderCompareOptions) =>
    invoke<FolderCompareResult>("compare_folders", { left, right, options }),
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
  signInProfile: (id: string) => invoke<Profile>("sign_in_profile", { id }),
  openProviderConsole: (kind: ProviderKind) => invoke<void>("open_provider_console", { kind }),
  remoteList: (path: string) => invoke<RemoteEntry[]>("remote_list", { path }),
};

export function errorMessage(e: unknown): string {
  return typeof e === "string" ? e : e instanceof Error ? e.message : JSON.stringify(e);
}
