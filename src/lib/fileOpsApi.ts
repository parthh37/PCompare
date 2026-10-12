// Folder Compare file operations (src-tauri/src/fileops.rs, bc5:file-ops):
// copy / move to the other side or a folder, delete, rename, new folder,
// attributes and touch. Operations run in the background; this module keeps
// the app-wide list of them (every Folder Compare tab shows the same
// Operations panel) and tells sessions when one finishes so they can rescan.
import { Channel, invoke } from "@tauri-apps/api/core";
import { useSyncExternalStore } from "react";
import { errorMessage, type EntryMeta, type FolderEntry, type Side } from "./api";
import type { FailureKind } from "./syncApi";

export type FileAction = "copy" | "move" | "delete" | "rename" | "newFolder" | "attributes" | "touch";
/** What a copy does when a file is already at its destination. */
export type ConflictMode = "ask" | "overwrite" | "skip" | "overwriteOlder";
export type ConflictAnswer = "overwrite" | "skip" | "overwriteAll" | "skipAll" | "overwriteOlderAll" | "cancel";
export type TouchTime = { kind: "now" } | { kind: "at"; ms: number } | { kind: "otherSide" };

/** `ATTR` bits to set and clear, and (Unix) permission bits. */
export interface AttrChange {
  set: number;
  clear: number;
  modeSet: number;
  modeClear: number;
}

/** Paths relative to the operation's roots, `/`-separated. */
export interface FileItem {
  src: string;
  /** Where a copy/move goes, or (touch "to other side") the counterpart. */
  dst?: string;
}

export interface FileOpOptions {
  conflicts: ConflictMode;
  /** Delete for good instead of the Recycle Bin / Trash. */
  permanent: boolean;
  /** Local files replaced by a copy go to the Recycle Bin / Trash. */
  trashReplaced: boolean;
  copyAcls: boolean;
  copyShortNames: boolean;
  newName: string;
  attributes: AttrChange;
  recursive: boolean;
  touch: TouchTime;
}

export interface FileOpRequest {
  action: FileAction;
  items: FileItem[];
  options: Partial<FileOpOptions>;
}

export type FilePhase = "queued" | "scanning" | "running" | "paused" | "asking" | "finished";

export interface ConflictQuestion {
  path: string;
  source: EntryMeta;
  target: EntryMeta;
}

export interface FileOpProgress {
  phase: FilePhase;
  filesDone: number;
  filesTotal: number;
  bytesDone: number;
  bytesTotal: number;
  current: string | null;
  failed: number;
  skipped: number;
  question: ConflictQuestion | null;
}

export interface FileOpFailure {
  path: string;
  kind: FailureKind;
  message: string;
}

export interface FileOpReport {
  succeeded: number;
  failed: number;
  skipped: number;
  bytes: number;
  cancelled: boolean;
  failures: FileOpFailure[];
  notes: string[];
  elapsedMs: number;
}

export type OpStatus = "running" | "done" | "errors" | "failed" | "cancelled";

/** One operation in the Operations panel. */
export interface FileOp {
  id: string;
  title: string;
  action: FileAction;
  srcRoot: string;
  dstRoot: string | null;
  progress: FileOpProgress | null;
  report: FileOpReport | null;
  /** The operation couldn't start (e.g. a remote didn't connect). */
  error: string | null;
  status: OpStatus;
  /** The user asked it to hold. */
  paused: boolean;
  startedMs: number;
  finishedMs: number | null;
}

// ----- the app-wide store -----

let ops: FileOp[] = [];
const listeners = new Set<() => void>();
const finished = new Set<(op: FileOp) => void>();

const notify = () => listeners.forEach((l) => l());

function patch(id: string, p: Partial<FileOp>) {
  ops = ops.map((o) => (o.id === id ? { ...o, ...p } : o));
  notify();
}

function subscribe(cb: () => void) {
  listeners.add(cb);
  return () => listeners.delete(cb);
}

export const fileOps = {
  /** Starts an operation in the background; resolves when it is done. */
  async start(title: string, srcRoot: string, dstRoot: string | null, request: FileOpRequest): Promise<FileOp> {
    const id = crypto.randomUUID();
    ops = [
      ...ops,
      {
        id,
        title,
        action: request.action,
        srcRoot,
        dstRoot,
        progress: null,
        report: null,
        error: null,
        status: "running",
        paused: false,
        startedMs: Date.now(),
        finishedMs: null,
      },
    ];
    notify();
    const channel = new Channel<FileOpProgress>();
    channel.onmessage = (progress) => patch(id, { progress });
    try {
      const report = await invoke<FileOpReport>("fileop_start", { opId: id, srcRoot, dstRoot, request, onProgress: channel });
      const status: OpStatus = report.cancelled ? "cancelled" : report.failed ? "errors" : "done";
      patch(id, { report, status, paused: false, finishedMs: Date.now() });
    } catch (e) {
      patch(id, { error: errorMessage(e), status: "failed", paused: false, finishedMs: Date.now() });
    }
    const op = ops.find((o) => o.id === id)!;
    finished.forEach((f) => f(op));
    return op;
  },
  pause(id: string, paused: boolean) {
    patch(id, { paused });
    return invoke<void>("fileop_pause", { opId: id, paused });
  },
  cancel: (id: string) => invoke<void>("fileop_cancel", { opId: id }),
  answer: (id: string, answer: ConflictAnswer) => invoke<void>("fileop_answer", { opId: id, answer }),
  dismiss(id: string) {
    ops = ops.filter((o) => o.id !== id || o.status === "running");
    notify();
  },
  clearFinished() {
    ops = ops.filter((o) => o.status === "running");
    notify();
  },
  /** Called whenever an operation finishes (any outcome). */
  onFinished(cb: (op: FileOp) => void): () => void {
    finished.add(cb);
    return () => finished.delete(cb);
  },
};

/** The current operations, oldest first; re-renders as they progress. */
export function useFileOps(): FileOp[] {
  return useSyncExternalStore(subscribe, () => ops);
}

// ----- helpers for building requests -----

export const isRemotePath = (p: string) => p.startsWith("remote://");

const norm = (p: string) => p.replace(/\\/g, "/").replace(/\/+$/, "").toLowerCase();

/** Whether two paths are the same or one contains the other. */
export function pathsOverlap(a: string, b: string): boolean {
  if (!a || !b) return false;
  const [x, y] = [norm(a), norm(b)];
  return x === y || x.startsWith(`${y}/`) || y.startsWith(`${x}/`);
}

/**
 * Every entry's relative path on the left side. Alignment can pair
 * differently named items (`leftRelPath`); right-only items below such a
 * folder belong in the left folder it was paired with.
 */
export function leftRelPaths(entries: FolderEntry[]): Map<string, string> {
  const out = new Map<string, string>();
  const walk = (list: FolderEntry[], parent: string | null) => {
    for (const e of list) {
      const left = e.leftRelPath ?? (e.left || parent === null ? e.relPath : `${parent}/${e.name}`);
      out.set(e.relPath, left);
      walk(e.children, left);
    }
  };
  walk(entries, null);
  return out;
}

/** Files, folders and bytes of `entries` (and everything inside) present on `side`. */
export function countSide(entries: FolderEntry[], side: Side): { files: number; folders: number; bytes: number } {
  const t = { files: 0, folders: 0, bytes: 0 };
  const walk = (list: FolderEntry[]) => {
    for (const e of list) {
      const m = e[side];
      if (!m) continue;
      if (e.isDir) {
        t.folders++;
        walk(e.children);
      } else {
        t.files++;
        t.bytes += m.size;
      }
    }
  };
  walk(entries);
  return t;
}

/** "3 files and 2 folders" */
export function describeCount(files: number, folders: number): string {
  const n = (k: number, one: string) => `${k.toLocaleString()} ${one}${k === 1 ? "" : "s"}`;
  if (!folders) return n(files, "file");
  if (!files) return n(folders, "folder");
  return `${n(files, "file")} and ${n(folders, "folder")}`;
}
