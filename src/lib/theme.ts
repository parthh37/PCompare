// Applies the appearance preferences (theme, fonts, diff colours) to the
// document as CSS variables, live: imported once by App, it follows every
// prefs change and, for the "system" theme, the OS light/dark setting.
import { getPrefs, onPrefsChange, type DiffColorKey, type Prefs } from "./prefs";

/** CSS variable each overridable diff colour drives (defaults live in styles.css). */
export const DIFF_COLOR_VARS: Record<DiffColorKey, string> = {
  changed: "--chg-bg",
  inserted: "--add-bg",
  deleted: "--del-bg",
  unimportant: "--unimportant",
  intralineDeleted: "--del-mark",
  intralineInserted: "--add-mark",
};

const darkQuery = window.matchMedia("(prefers-color-scheme: dark)");

export function resolvedTheme(p: Prefs = getPrefs()): "dark" | "light" {
  if (p.theme === "system") return darkQuery.matches ? "dark" : "light";
  return p.theme;
}

const quote = (font: string) => (/^[\w-]+$/.test(font) ? font : `"${font.replace(/["\\]/g, "")}"`);

function apply(p: Prefs) {
  const root = document.documentElement;
  root.dataset.theme = resolvedTheme(p);
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
