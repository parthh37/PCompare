// App-wide preferences (per-session view options live in each session's own
// settings). The source of truth is `prefs.json` in the app config directory
// (Rust `prefs_load`/`prefs_save`), so preferences survive updates and
// reinstalls; a localStorage copy gives the first paint the right theme and
// fonts before that file has been read.
//
// Usage: `const [prefs, setPrefs] = usePrefs()` in components, or
// `getPrefs()` / `onPrefsChange()` outside React. Every change is broadcast,
// so open sessions update live.
import { invoke } from "@tauri-apps/api/core";
import { useSyncExternalStore } from "react";

export type ThemePref = "dark" | "light" | "system";

/** Diff colours a user can override (each maps to one CSS variable). */
export type DiffColorKey =
  | "changed"
  | "inserted"
  | "deleted"
  | "unimportant"
  | "intralineDeleted"
  | "intralineInserted";

export interface Prefs {
  /** Check GitHub Releases for updates in the background. */
  autoUpdate: boolean;
  /** Pop up the "new version available" dialog (again on every hourly check until installed). */
  updateReminders: boolean;
  /** Reopen the tabs from the previous run at startup. */
  restoreWorkspace: boolean;
  /** Closing a tab updates its saved session, or adds it to Recent sessions. */
  autoSaveSessions: boolean;
  theme: ThemePref;
  /** Font family names; "" = the built-in default stack. */
  uiFont: string;
  uiFontSize: number;
  editorFont: string;
  editorFontSize: number;
  /** CSS colours overriding the theme's diff palette. */
  diffColors: Partial<Record<DiffColorKey, string>>;
  /** Shortcut overrides by command id (see keymap.ts); "" = unbound. */
  keys: Record<string, string>;
  /** The Getting started overview was closed (it shows once on Home). */
  gettingStartedDismissed: boolean; // araxis:vcs araxis:vcs end
  // araxis:text-view
  /** Text Compare pads the shorter side of a difference with blank rows (off = linked lines). */
  textPadding: boolean;
  // araxis:text-view end
  /** Text Compare draws the left pane's gutter (copy arrow + line number) at its right edge, by the right pane's. */
  textLeftGutterRight: boolean;
  // araxis:folder — Folder Compare's split view (file comparison below the folder list).
  folderSplitView: boolean;
  /** Share of the height the folder list keeps (0.15–0.85). */
  folderSplitRatio: number;
  // araxis:folder end
  // araxis:reports — printed reports' running header/footer templates (see reportApi.ts) and paper size.
  printHeader: string;
  printFooter: string;
  printPaper: "a4" | "letter";
  // araxis:reports end
  /** Paths recently compared, newest first (offered under every path field). */
  recentFolders: string[];
  recentFiles: string[];
}

export const DEFAULT_PREFS: Prefs = {
  autoUpdate: true,
  updateReminders: true,
  restoreWorkspace: false,
  autoSaveSessions: true,
  theme: "dark",
  uiFont: "",
  uiFontSize: 13,
  editorFont: "",
  editorFontSize: 13,
  diffColors: {},
  keys: {},
  gettingStartedDismissed: false, // araxis:vcs araxis:vcs end
  textPadding: true, // araxis:text-view araxis:text-view end
  textLeftGutterRight: false,
  folderSplitView: false, // araxis:folder
  folderSplitRatio: 0.5, // araxis:folder
  // araxis:reports
  printHeader: "{title}|{date}",
  printFooter: "{left} ↔ {right}|Page {page} of {pages}",
  printPaper: "a4",
  // araxis:reports end
  recentFolders: [],
  recentFiles: [],
};

/** Size limits keep fixed-height diff rows (20px) readable. */
export const UI_FONT_SIZES = { min: 11, max: 16 };
export const EDITOR_FONT_SIZES = { min: 9, max: 16 };

const CACHE_KEY = "pcompare.prefs";
/** Pre-prefs localStorage key from useUpdater ("off" disabled update checks). */
const LEGACY_AUTO_UPDATE_KEY = "pcompare.autoUpdate";

const clamp = (n: unknown, { min, max }: { min: number; max: number }, fallback: number) =>
  typeof n === "number" && Number.isFinite(n) ? Math.min(max, Math.max(min, Math.round(n * 2) / 2)) : fallback;

const stringRecord = (v: unknown): Record<string, string> =>
  v && typeof v === "object" && !Array.isArray(v)
    ? Object.fromEntries(Object.entries(v).filter((e): e is [string, string] => typeof e[1] === "string"))
    : {};

/** Most recent-path lists keep; older entries fall off the end. */
export const RECENT_PATHS_MAX = 20;

const stringList = (v: unknown): string[] =>
  Array.isArray(v)
    ? v.filter((x): x is string => typeof x === "string" && x.trim() !== "").slice(0, RECENT_PATHS_MAX)
    : [];

/** Fills gaps and drops invalid values, so a hand-edited or older file can't break the UI. */
export function sanitizePrefs(raw: unknown): Prefs {
  const r = (raw && typeof raw === "object" ? raw : {}) as Partial<Record<keyof Prefs, unknown>>;
  const d = DEFAULT_PREFS;
  const bool = (v: unknown, fallback: boolean) => (typeof v === "boolean" ? v : fallback);
  const str = (v: unknown) => (typeof v === "string" ? v.trim() : "");
  return {
    autoUpdate: bool(r.autoUpdate, d.autoUpdate),
    updateReminders: bool(r.updateReminders, d.updateReminders),
    restoreWorkspace: bool(r.restoreWorkspace, d.restoreWorkspace),
    autoSaveSessions: bool(r.autoSaveSessions, d.autoSaveSessions),
    theme: r.theme === "light" || r.theme === "system" || r.theme === "dark" ? r.theme : d.theme,
    uiFont: str(r.uiFont),
    uiFontSize: clamp(r.uiFontSize, UI_FONT_SIZES, d.uiFontSize),
    editorFont: str(r.editorFont),
    editorFontSize: clamp(r.editorFontSize, EDITOR_FONT_SIZES, d.editorFontSize),
    diffColors: stringRecord(r.diffColors) as Prefs["diffColors"],
    keys: stringRecord(r.keys),
    gettingStartedDismissed: bool(r.gettingStartedDismissed, d.gettingStartedDismissed), // araxis:vcs araxis:vcs end
    textPadding: bool(r.textPadding, d.textPadding), // araxis:text-view araxis:text-view end
    textLeftGutterRight: bool(r.textLeftGutterRight, d.textLeftGutterRight),
    // araxis:folder
    folderSplitView: bool(r.folderSplitView, d.folderSplitView),
    folderSplitRatio:
      typeof r.folderSplitRatio === "number" && Number.isFinite(r.folderSplitRatio)
        ? Math.min(0.85, Math.max(0.15, r.folderSplitRatio))
        : d.folderSplitRatio,
    // araxis:folder end
    // araxis:reports — templates keep their spacing; empty means no header/footer.
    printHeader: typeof r.printHeader === "string" ? r.printHeader : d.printHeader,
    printFooter: typeof r.printFooter === "string" ? r.printFooter : d.printFooter,
    printPaper: r.printPaper === "letter" ? "letter" : "a4",
    // araxis:reports end
    recentFolders: stringList(r.recentFolders),
    recentFiles: stringList(r.recentFiles),
  };
}

function readCache(): Prefs {
  try {
    const text = localStorage.getItem(CACHE_KEY);
    if (text) return sanitizePrefs(JSON.parse(text));
  } catch {
    // Unreadable cache: fall through to defaults.
  }
  const prefs = { ...DEFAULT_PREFS };
  if (localStorage.getItem(LEGACY_AUTO_UPDATE_KEY) === "off") prefs.autoUpdate = false;
  return prefs;
}

let current: Prefs = readCache();
const listeners = new Set<(p: Prefs) => void>();

function publish(next: Prefs) {
  current = next;
  try {
    localStorage.setItem(CACHE_KEY, JSON.stringify(next));
  } catch {
    // Storage full or disabled: the config file is still authoritative.
  }
  listeners.forEach((l) => l(next));
}

let pendingSave: number | undefined;
function saveNow(p: Prefs) {
  window.clearTimeout(pendingSave);
  pendingSave = undefined;
  invoke("prefs_save", { prefs: p }).catch((e) => console.error("saving preferences failed", e));
}
/** Coalesces bursts (dragging a colour picker) into one write. */
function persist(p: Prefs) {
  window.clearTimeout(pendingSave);
  pendingSave = window.setTimeout(() => saveNow(p), 250);
}
window.addEventListener("pagehide", () => pendingSave !== undefined && saveNow(current));

/**
 * Resolves once preferences have been read from disk (or immediately with the
 * cached copy if the backend isn't reachable), for startup decisions such as
 * "restore last workspace".
 */
export const prefsReady: Promise<Prefs> = invoke<unknown>("prefs_load")
  .then((stored) => {
    const empty = !stored || (typeof stored === "object" && Object.keys(stored).length === 0);
    if (empty) {
      // First run with the prefs file: carry over what localStorage knew.
      persist(current);
    } else {
      const next = sanitizePrefs(stored);
      if (JSON.stringify(next) !== JSON.stringify(current)) publish(next);
    }
    localStorage.removeItem(LEGACY_AUTO_UPDATE_KEY);
    return current;
  })
  .catch(() => current);

export const getPrefs = (): Prefs => current;

/** Applies a partial update, broadcasts it and saves it. */
export function setPrefs(patch: Partial<Prefs> | ((p: Prefs) => Partial<Prefs>)) {
  const delta = typeof patch === "function" ? patch(current) : patch;
  const next = sanitizePrefs({ ...current, ...delta });
  publish(next);
  persist(next);
}

export function onPrefsChange(listener: (p: Prefs) => void): () => void {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

const subscribe = (cb: () => void) => onPrefsChange(cb);

/** The live preferences plus a setter; re-renders on every change. */
export function usePrefs(): [Prefs, typeof setPrefs] {
  return [useSyncExternalStore(subscribe, getPrefs), setPrefs];
}

/** One preference value; re-renders only when that value changes identity. */
export function usePref<K extends keyof Prefs>(key: K): Prefs[K] {
  return useSyncExternalStore(subscribe, () => current[key]);
}
