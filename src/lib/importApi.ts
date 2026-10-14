// Typed bridge to src-tauri/src/import.rs: reading saved sessions from Beyond
// Compare, WinMerge, FreeFileSync and other tools, and saving the chosen ones.
import { invoke } from "@tauri-apps/api/core";
import type { SavedSession, SessionInput } from "./api";
import type { CompareKind } from "./sessions";

/** A sessions file found where another tool keeps it (`pcompare_import::Candidate`). */
export interface ImportCandidate {
  /** e.g. "Beyond Compare 5". */
  label: string;
  path: string;
}

/** One session read from another tool, already mapped to a PCompare session. */
export interface ImportedSession {
  name: string;
  kind: CompareKind;
  left: string;
  right: string;
  base?: string;
  output?: string;
  /** Partial session settings; viewers fill the rest with their defaults. */
  settings: Record<string, unknown>;
  /** The folder the session sits in, in tools that group them ("Work/Site"). */
  group?: string;
  /** Kept automatically by the tool rather than saved by name (BC's auto-saved sessions). */
  autoSaved: boolean;
  /** Options that couldn't be carried over. */
  notes: string[];
  /** `kind` only says "two files"; pick the viewer from the extensions. */
  fileKindByExtension: boolean;
}

export interface ImportSource {
  /** The tool and version the file came from. */
  label: string;
  path: string;
  sessions: ImportedSession[];
  /** Sessions with no PCompare equivalent, with why. */
  skipped: { name: string; reason: string }[];
}

/** File types the importer reads, for the open dialog. */
export const IMPORT_FILTERS = [
  {
    name: "Session files",
    extensions: ["xml", "bcpkg", "WinMerge", "winmerge", "ffs_gui", "ffs_batch", "meldcmp"],
  },
  { name: "All files", extensions: ["*"] },
];

export const importApi = {
  /** Session files of installed tools, in their usual locations. */
  discover: () => invoke<ImportCandidate[]>("import_discover"),
  read: (path: string) => invoke<ImportSource>("import_read", { path }),
  /** Saves the sessions; clashing names get a ` (2)` suffix like any save. */
  commit: (sessions: SessionInput[]) => invoke<SavedSession[]>("import_commit", { sessions }),
};
