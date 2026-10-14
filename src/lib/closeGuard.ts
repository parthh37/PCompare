// Close/quit guard (src-tauri/src/close_guard.rs): the shell reports whether any
// tab holds unsaved edits; closing the window or quitting then stops and asks.
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

const ASK_EVENT = "close-guard-ask";

export interface CloseAsk {
  /** The app is quitting, not just the window closing. */
  quit: boolean;
}

export const setUnsaved = (unsaved: boolean) => invoke<void>("close_guard_set_unsaved", { unsaved });
/** Closes the window or quits after the user saved everything or chose Discard. */
export const proceed = (quit: boolean) => invoke<void>("close_guard_proceed", { quit });
export const onAsk = (cb: (ask: CloseAsk) => void): Promise<UnlistenFn> =>
  listen<CloseAsk>(ASK_EVENT, (e) => cb(e.payload));

/** Saves a tab's edits; resolves once the save was attempted (failures show in the tab). */
export type SaveTab = () => unknown;

/**
 * Saves every dirty tab in turn (showing each first, since a save may ask
 * something), then lets React report the results. Returns the ids still
 * dirty afterwards: tabs that can't save from here, failed, or were cancelled.
 */
export async function saveAll(
  dirty: Iterable<number>,
  savers: Map<number, SaveTab>,
  show: (id: number) => void,
  stillDirty: () => Set<number>,
): Promise<number[]> {
  for (const id of [...dirty]) {
    const save = savers.get(id);
    if (!save) continue;
    show(id);
    try {
      await save();
    } catch {
      // The tab shows its own error and stays dirty.
    }
  }
  // Sessions report "clean" from an effect after the save's state update renders.
  await new Promise((r) => setTimeout(r, 50));
  return [...stillDirty()];
}
