// Context menus drawn by the OS (Win32 / AppKit) instead of HTML popups.
// Components describe their items as plain data; `popupNativeMenu` turns
// that into a Tauri menu, and `ContextMenu.tsx` draws the same data as HTML
// where there is no Tauri runtime (plain `vite` in a browser).
import { isTauri } from "@tauri-apps/api/core";
import { LogicalPosition } from "@tauri-apps/api/dpi";
import { Menu, type MenuOptions } from "@tauri-apps/api/menu";
import { isMac } from "./keymap";

export const SEPARATOR = "separator" as const;

export type MenuEntry =
  | {
      label: string;
      onClick: () => void;
      disabled?: boolean;
      /** A check mark; leave undefined for a plain item. */
      checked?: boolean;
      /** Shortcut text as the UI shows it ("Ctrl+C", "Alt+→", "⌘+⇧+S"). */
      shortcut?: string | null;
      /** Destructive; only the HTML fallback can colour it. */
      danger?: boolean;
    }
  | { label: string; submenu: MenuItems; disabled?: boolean }
  /** A section title: a greyed-out item in the native menu. */
  | { heading: string }
  | typeof SEPARATOR;

/** Entries as written in a component: `cond && {...}` items are skipped. */
export type MenuItems = (MenuEntry | false | null | undefined | "")[];

/** Drops skipped entries and the separators they leave doubled or at the ends. */
export function cleanEntries(items: MenuItems): MenuEntry[] {
  const out: MenuEntry[] = [];
  for (const e of items) {
    if (!e) continue;
    if (e === SEPARATOR && (!out.length || out[out.length - 1] === SEPARATOR)) continue;
    out.push(e);
  }
  while (out[out.length - 1] === SEPARATOR) out.pop();
  return out;
}

export const nativeMenusAvailable = isTauri();

const MODIFIERS: Record<string, string> = {
  ctrl: "CmdOrCtrl",
  cmd: "CmdOrCtrl",
  "⌘": "CmdOrCtrl",
  alt: "Alt",
  option: "Alt",
  "⌥": "Alt",
  shift: "Shift",
  "⇧": "Shift",
};
const KEY_ALIASES: Record<string, string> = {
  "→": "Right",
  "←": "Left",
  "↑": "Up",
  "↓": "Down",
  Del: "Delete",
  Esc: "Escape",
};
const KEY_NAME =
  /^(?:[A-Z0-9]|F(?:[1-9]|1[0-9]|2[0-4])|Up|Down|Left|Right|Enter|Tab|Space|Delete|Backspace|Escape|Home|End|PageUp|PageDown|Insert|[=\-,./;'[\]\\`])$/;
/** Keys that move or pick inside an open menu on macOS, where a menu's
 *  shortcuts are live while it is open. */
const MENU_KEYS = new Set(["Up", "Down", "Left", "Right", "Enter", "Tab", "Space", "Escape"]);

/** The UI's shortcut text as a Tauri accelerator, or undefined when it has none ("Ctrl+click"). */
export function toAccelerator(shortcut: string | null | undefined): string | undefined {
  if (!shortcut) return undefined;
  const parts = shortcut.split("+").map((p) => p.trim());
  if (parts.some((p) => !p)) return undefined;
  const raw = parts.pop()!;
  const key = KEY_ALIASES[raw] ?? (raw.length === 1 ? raw.toUpperCase() : raw);
  if (!KEY_NAME.test(key)) return undefined;
  const mods = parts.map((p) => MODIFIERS[p.toLowerCase()]);
  if (mods.some((m) => !m)) return undefined;
  if (isMac && !mods.length && MENU_KEYS.has(key)) return undefined;
  return [...new Set(mods), key].join("+");
}

/** `&` marks a mnemonic in native menu text; `&&` is a literal ampersand. */
const menuText = (s: string) => s.replace(/&/g, "&&");

type NativeItem = NonNullable<MenuOptions["items"]>[number];

function toNative(entries: MenuEntry[], withShortcuts: boolean): NativeItem[] {
  return entries.map((e): NativeItem => {
    if (e === SEPARATOR) return { item: "Separator" };
    if ("heading" in e) return { text: menuText(e.heading), enabled: false };
    if ("submenu" in e) {
      const items = cleanEntries(e.submenu);
      return { text: menuText(e.label), enabled: !e.disabled && items.length > 0, items: toNative(items, withShortcuts) };
    }
    const item = {
      text: menuText(e.label),
      enabled: !e.disabled,
      accelerator: withShortcuts ? toAccelerator(e.shortcut) : undefined,
      action: () => e.onClick(),
    };
    return e.checked === undefined ? item : { ...item, checked: e.checked };
  });
}

// The menu shown last. Closing a menu also drops its items' click handlers,
// and popup() can resolve before a picked item's event reaches us, so it is
// released when the next menu opens rather than right after popup().
let shown: Menu | null = null;

/** Shows `entries` as a native context menu at a point in the window (CSS px). */
export async function popupNativeMenu(entries: MenuEntry[], at: { x: number; y: number }): Promise<void> {
  const previous = shown;
  shown = null;
  void previous?.close().catch(() => {});
  let menu: Menu;
  try {
    menu = await Menu.new({ items: toNative(entries, true) });
  } catch {
    // A shortcut the OS couldn't parse: show the menu without them.
    menu = await Menu.new({ items: toNative(entries, false) });
  }
  shown = menu;
  await menu.popup(new LogicalPosition(at.x, at.y));
}
