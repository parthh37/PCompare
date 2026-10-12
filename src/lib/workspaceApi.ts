// Typed bridge to src-tauri/src/workspace.rs: workspaces, the last-open tab
// set, recent (auto-saved) sessions, auto-saving linked sessions on close,
// and paths forwarded from a second launch.
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import type { SavedSession, SessionInput } from "./api";
import type { CompareKind, SessionKind } from "./sessions";

/** One tab of a workspace (mirrors `pcompare_core::workspace::WorkspaceTab`). */
export interface WorkspaceTab {
  kind: SessionKind;
  left: string;
  right: string;
  base?: string;
  output?: string;
  settings?: Record<string, unknown> | null;
  sessionId?: string;
  sessionName?: string;
}

export interface Workspace {
  id: string;
  name: string;
  tabs: WorkspaceTab[];
  /** Index of the tab that was in front. */
  active: number;
  createdMs: number;
  modifiedMs: number;
}

/** A compare closed without being saved (BC's auto-saved sessions). */
export interface RecentSession {
  id: string;
  kind: CompareKind;
  left: string;
  right: string;
  base?: string;
  output?: string;
  settings: Record<string, unknown> | null;
  closedMs: number;
}

export type RecentInput = Omit<RecentSession, "id" | "closedMs">;

export const workspaceApi = {
  list: () => invoke<Workspace[]>("workspace_list"),
  save: (input: { id?: string; name: string; tabs: WorkspaceTab[]; active: number }) =>
    invoke<Workspace>("workspace_save", { input }),
  rename: (id: string, name: string) => invoke<Workspace>("workspace_rename", { id, name }),
  remove: (id: string) => invoke<void>("workspace_delete", { id }),
  last: () => invoke<Workspace | null>("workspace_last"),
  setLast: (tabs: WorkspaceTab[], active: number) => invoke<void>("workspace_set_last", { tabs, active }),

  recent: () => invoke<RecentSession[]>("recent_list"),
  addRecent: (input: RecentInput) => invoke<RecentSession>("recent_add", { input }),
  removeRecent: (id: string) => invoke<void>("recent_remove", { id }),
  clearRecent: () => invoke<void>("recent_clear"),

  /** Writes a closing tab back into its saved session; null if it was deleted meanwhile. */
  autosaveSession: (id: string, input: SessionInput) =>
    invoke<SavedSession | null>("session_autosave", { id, input }),

  /** `pcompare <paths…>` launched while this window was already open. */
  // bc5:reports-script — the payload is the structured launch options (cliApi.ts).
  onLaunchPaths: (handler: (launch: import("./cliApi").LaunchOptions) => void): Promise<UnlistenFn> =>
    listen<import("./cliApi").LaunchOptions>("open-launch-paths", (e) => handler(e.payload)),
};
