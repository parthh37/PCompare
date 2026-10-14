// Typed bridge to src-tauri/src/explorer.rs: the Explorer right-click menu
// PCompare registers for itself on Windows.
import { invoke } from "@tauri-apps/api/core";

/** Mirrors `pcompare_shell::menu::MenuStatus`. */
export type MenuStatus =
  | { state: "installed" }
  /** The menu runs another copy of PCompare (moved or reinstalled elsewhere). */
  | { state: "stale"; exe: string }
  | { state: "missing" }
  | { state: "disabled" }
  /** Not Windows. */
  | { state: "unsupported" };

export const explorerApi = {
  status: () => invoke<MenuStatus>("shell_menu_status"),
  /** Adds the menu for this copy (and forgets an earlier "off"), or removes it. */
  set: (enabled: boolean) => invoke<MenuStatus>("shell_menu_set", { enabled }),
};
