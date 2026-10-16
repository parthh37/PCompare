// Applies the appearance preferences (theme, fonts, diff colours) to the
// document as CSS variables, live: imported once by App, it follows every
// prefs change and, for the "system" theme, the OS light/dark setting.
import { getCurrentWindow } from "@tauri-apps/api/window";
import { getPrefs, onPrefsChange, type DiffColorKey, type Prefs } from "./prefs";

/** CSS variable each overridable diff colour drives (defaults live in styles.css). */
export const DIFF_COLOR_VARS: Record<DiffColorKey, string> = {
  changed: "--chg-bg",
  inserted: "--add-bg",
  deleted: "--del-bg",
  unimportant: "--unimportant",
  intralineDeleted: "--del-mark",
  intralineInserted: "--add-mark",
  folderSame: "--fc-same",
  folderNewer: "--fc-newer",
  folderDifferent: "--fc-different",
  folderOlder: "--fc-older",
  folderOrphan: "--fc-orphan",
  folderUnknown: "--fc-unknown",
};

const darkQuery = window.matchMedia("(prefers-color-scheme: dark)");

export function resolvedTheme(p: Prefs = getPrefs()): "dark" | "light" {
  if (p.theme === "system") return darkQuery.matches ? "dark" : "light";
  return p.theme;
}

const quote = (font: string) => (/^[\w-]+$/.test(font) ? font : `"${font.replace(/["\\]/g, "")}"`);

let windowTheme: Prefs["theme"] | undefined;

/** Native title bar and menus follow the app's theme, not just the OS's:
 *  otherwise the dark app sits under a white Windows title bar. */
function applyWindowTheme(theme: Prefs["theme"]) {
  if (theme === windowTheme) return;
  windowTheme = theme;
  // Outside Tauri (plain vite, mock harness) there is no native window.
  try {
    getCurrentWindow()
      .setTheme(theme === "system" ? null : theme)
      .catch(() => {});
  } catch {
    // no window
  }
}

function apply(p: Prefs) {
  const root = document.documentElement;
  root.dataset.theme = resolvedTheme(p);
  applyWindowTheme(p.theme);
  const set = (name: string, value: string | null) =>
    value ? root.style.setProperty(name, value) : root.style.removeProperty(name);
  set("--ui", p.uiFont ? `${quote(p.uiFont)}, system-ui, sans-serif` : null);
  set("--mono", p.editorFont ? `${quote(p.editorFont)}, ui-monospace, monospace` : null);
  set("--ui-font-size", `${p.uiFontSize}px`);
  set("--editor-font-size", `${p.editorFontSize}px`);
  for (const [key, cssVar] of Object.entries(DIFF_COLOR_VARS)) {
    set(cssVar, p.diffColors[key as DiffColorKey] ?? null);
  }
}

apply(getPrefs());
onPrefsChange(apply);
darkQuery.addEventListener("change", () => getPrefs().theme === "system" && apply(getPrefs()));

/** Reads a theme colour as currently computed (for canvas drawing and colour pickers). */
export function cssVar(name: string): string {
  return getComputedStyle(document.documentElement).getPropertyValue(name).trim();
}
