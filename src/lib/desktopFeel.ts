// Stops the WebView from behaving like a browser page: no Back/Refresh/
// Print/Inspect menu on right-click and no reload/print/save-page keys.
// Text fields get a native OS edit menu instead; viewers that draw their
// own context menu or bind these keys keep working, because the guards only
// act on events nothing else has claimed.
import { Menu, MenuItem, PredefinedMenuItem } from "@tauri-apps/api/menu";
import { readClipboardText, writeClipboardText } from "./clipboard";

type TextField = HTMLInputElement | HTMLTextAreaElement;

const TEXT_INPUT_TYPES = new Set(["", "text", "search", "url", "tel", "email", "password", "number"]);

function editableTarget(target: EventTarget | null): TextField | HTMLElement | null {
  if (!(target instanceof Element)) return null;
  const el = target.closest<HTMLElement>("input, textarea, [contenteditable]");
  if (!el) return null;
  if (el instanceof HTMLTextAreaElement) return el;
  if (el instanceof HTMLInputElement) return TEXT_INPUT_TYPES.has(el.type) ? el : null;
  return el.isContentEditable ? el : null;
}

function isTextField(el: HTMLElement): el is TextField {
  return el instanceof HTMLInputElement || el instanceof HTMLTextAreaElement;
}

/** Selected text of a field; number/email inputs have no selection API. */
function fieldSelection(el: TextField): string {
  try {
    if (el.selectionStart != null && el.selectionEnd != null) {
      return el.value.slice(el.selectionStart, el.selectionEnd);
    }
  } catch {
    // fall through
  }
  return window.getSelection()?.toString() ?? "";
}

/** What the open edit menu acts on; set right before each popup. */
interface EditTarget {
  el: TextField | HTMLElement;
  field: TextField | null;
  selected: string;
  /** Caret/selection to restore, since the native menu takes focus. */
  restore: () => void;
}

let target: EditTarget | null = null;

function refocus(t: EditTarget) {
  t.el.focus();
  t.restore();
}

// execCommand edits go through the field's undo stack and fire `input`,
// so React state follows along.
function run(cmd: string, value?: string) {
  if (!target) return;
  refocus(target);
  document.execCommand(cmd, false, value);
}

type EditItem = "undo" | "redo" | "cut" | "copy" | "paste" | "delete" | "selectAll";

// Built once and reused: each Menu is a native resource that would
// otherwise pile up with every right-click.
let editMenu: Promise<{ menu: Menu; items: Record<EditItem, MenuItem> }> | null = null;
let copyMenu: Promise<Menu> | null = null;

function buildEditMenu() {
  editMenu ??= (async () => {
    const item = (text: string, accelerator: string | undefined, action: () => void) =>
      MenuItem.new({ text, accelerator, action });
    const items: Record<EditItem, MenuItem> = {
      undo: await item("Undo", "CmdOrCtrl+Z", () => run("undo")),
      redo: await item("Redo", "CmdOrCtrl+Y", () => run("redo")),
      cut: await item("Cut", "CmdOrCtrl+X", () => {
        const text = target?.selected ?? "";
        void writeClipboardText(text).then(() => run("delete"));
      }),
      copy: await item("Copy", "CmdOrCtrl+C", () => void writeClipboardText(target?.selected ?? "")),
      paste: await item("Paste", "CmdOrCtrl+V", () => {
        void readClipboardText().then((t) => t && run("insertText", t));
      }),
      delete: await item("Delete", undefined, () => run("delete")),
      selectAll: await item("Select All", "CmdOrCtrl+A", () => {
        if (!target) return;
        refocus(target);
        if (target.field) target.field.select();
        else document.execCommand("selectAll");
      }),
    };
    const sep = () => PredefinedMenuItem.new({ item: "Separator" });
    const menu = await Menu.new({
      items: [
        items.undo,
        items.redo,
        await sep(),
        items.cut,
        items.copy,
        items.paste,
        items.delete,
        await sep(),
        items.selectAll,
      ],
    });
    return { menu, items };
  })();
  return editMenu;
}

async function showEditMenu(el: TextField | HTMLElement) {
  const field = isTextField(el) ? el : null;
  const readOnly = field ? field.readOnly || field.disabled : false;
  const secret = field instanceof HTMLInputElement && field.type === "password";
  const sel = window.getSelection();
  const range = !field && sel?.rangeCount ? sel.getRangeAt(0) : null;
  const [start, end] = field ? [field.selectionStart, field.selectionEnd] : [null, null];
  target = {
    el,
    field,
    selected: field ? fieldSelection(field) : (sel?.toString() ?? ""),
    restore: () => {
      if (field && start != null && end != null) {
        try {
          field.setSelectionRange(start, end);
        } catch {
          // inputs without selection support
        }
      } else if (range) {
        window.getSelection()?.removeAllRanges();
        window.getSelection()?.addRange(range);
      }
    },
  };

  const { menu, items } = await buildEditMenu();
  const hasSel = target.selected !== "";
  await Promise.all([
    items.undo.setEnabled(!readOnly),
    items.redo.setEnabled(!readOnly),
    items.cut.setEnabled(!readOnly && !secret && hasSel),
    items.copy.setEnabled(!secret && hasSel),
    items.paste.setEnabled(!readOnly),
    items.delete.setEnabled(!readOnly && hasSel),
  ]);
  await menu.popup();
}

let copyText = "";

async function showCopyMenu(text: string) {
  copyText = text;
  copyMenu ??= Menu.new({
    items: [{ text: "Copy", accelerator: "CmdOrCtrl+C", action: () => void writeClipboardText(copyText) }],
  });
  await (await copyMenu).popup();
}

function onContextMenu(e: MouseEvent) {
  // A viewer's own menu already handled it.
  if (e.defaultPrevented) return;
  // Keep the browser menu (Inspect) reachable while developing.
  if (import.meta.env.DEV && e.shiftKey) return;
  e.preventDefault();

  const el = editableTarget(e.target);
  const show = el
    ? showEditMenu(el)
    : (() => {
        const text = window.getSelection()?.toString() ?? "";
        return text.trim() ? showCopyMenu(text) : Promise.resolve();
      })();
  show.catch(() => {
    // Outside Tauri (mock harness) there is no native menu; show nothing.
  });
}

/** Browser shortcuts that make no sense in an app: reload, print, save page,
 *  view source, the find bar, history navigation. */
function isBrowserShortcut(e: KeyboardEvent): boolean {
  const mod = e.ctrlKey || e.metaKey;
  const key = e.key.length === 1 ? e.key.toLowerCase() : e.key;
  if (key === "F5" || key === "F3" || key === "BrowserRefresh" || key === "BrowserBack" || key === "BrowserForward") {
    return true;
  }
  if (e.altKey && !mod && (key === "ArrowLeft" || key === "ArrowRight")) return true;
  if (mod && !e.altKey && ["r", "p", "s", "u", "f", "g", "h", "j", "o"].includes(key)) return true;
  return false;
}

function onKeyDownCapture(e: KeyboardEvent) {
  if (!isBrowserShortcut(e)) return;
  // Viewers listen on window after this runs and some skip already-handled
  // events, so claim the key only once every other listener has seen it:
  // a window listener added now is the last one called for this event.
  window.addEventListener(
    "keydown",
    (late) => {
      if (late === e) late.preventDefault();
    },
    { once: true },
  );
}

export function installDesktopFeel() {
  window.addEventListener("contextmenu", onContextMenu);
  window.addEventListener("keydown", onKeyDownCapture, true);
}
