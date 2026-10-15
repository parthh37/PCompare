// Keyboard shortcuts: one registry of commands with default bindings, user
// overrides from prefs (`prefs.keys`), and `matches(e, id)` for handlers.
//
// Binding strings look like "Ctrl+Shift+S", "Alt+Down", "F5". "Ctrl" matches
// Ctrl or ⌘ (the app has always treated them alike). Use `eventBinding(e)` to
// turn a keydown into the same format.
//
// Converting a session component: replace a hard-coded check such as
// `e.altKey && e.key === "ArrowDown"` with `matches(e, "text.nextDiff")` and
// drop `fixed: true` from that command below; it then becomes rebindable in
// Settings → Keyboard and its help page shows the user's binding.
import { getPrefs, onPrefsChange } from "./prefs";
import type { SessionKind } from "./sessions";

/** Where a command applies: everywhere, or while a tab of that kind is in front. */
export type KeyScope = "global" | SessionKind;

export interface CommandDef {
  id: string;
  label: string;
  scope: KeyScope[];
  /** Default binding; "" = none. */
  binding: string;
  /**
   * The owning component still checks keys itself, so the shortcut is listed
   * (help, conflict checks) but can't be changed yet.
   */
  fixed?: boolean;
}

const G: KeyScope[] = ["global"];

export const COMMANDS: CommandDef[] = [
  // ---- app shell ----
  { id: "app.newTab", label: "New tab", scope: G, binding: "Ctrl+T" },
  { id: "app.closeTab", label: "Close tab", scope: G, binding: "Ctrl+W" },
  { id: "app.nextTab", label: "Next tab", scope: G, binding: "Ctrl+Tab" },
  { id: "app.prevTab", label: "Previous tab", scope: G, binding: "Ctrl+Shift+Tab" },
  { id: "app.saveSession", label: "Save session", scope: G, binding: "Ctrl+Shift+S" },
  { id: "app.saveWorkspace", label: "Save workspace…", scope: G, binding: "Ctrl+Alt+S" },
  { id: "app.settings", label: "Settings", scope: G, binding: "Ctrl+," },
  { id: "app.help", label: "Help for this view", scope: G, binding: "F1" },

  // ---- Text Compare ----
  { id: "text.save", label: "Save edited files", scope: ["text"], binding: "Ctrl+S", fixed: true },
  { id: "text.undo", label: "Undo", scope: ["text"], binding: "Ctrl+Z", fixed: true },
  { id: "text.redo", label: "Redo", scope: ["text"], binding: "Ctrl+Y", fixed: true },
  { id: "text.nextDiff", label: "Next difference", scope: ["text"], binding: "Alt+Down", fixed: true },
  { id: "text.prevDiff", label: "Previous difference", scope: ["text"], binding: "Alt+Up", fixed: true },
  { id: "text.copyToRight", label: "Copy change to right", scope: ["text"], binding: "Alt+Right", fixed: true },
  { id: "text.copyToLeft", label: "Copy change to left", scope: ["text"], binding: "Alt+Left", fixed: true },
  { id: "text.refresh", label: "Reload files", scope: ["text"], binding: "F5", fixed: true },
  // araxis:text-view — linking, sync points, go to line, block indent, line endings, replace file, resolved
  { id: "text.goToLine", label: "Go to line…", scope: ["text"], binding: "Ctrl+G" },
  { id: "text.indent", label: "Indent selected lines", scope: ["text"], binding: "Tab", fixed: true },
  { id: "text.outdent", label: "Un-indent selected lines", scope: ["text"], binding: "Shift+Tab", fixed: true },
  { id: "text.togglePadding", label: "Toggle vertical padding (linked lines)", scope: ["text"], binding: "" },
  { id: "text.lineEndings", label: "Show / hide line endings", scope: ["text"], binding: "" },
  { id: "text.setSyncPoint", label: "Set sync point on the current line", scope: ["text"], binding: "" },
  { id: "text.copyAllToRight", label: "Replace right file with left", scope: ["text"], binding: "" },
  { id: "text.copyAllToLeft", label: "Replace left file with right", scope: ["text"], binding: "" },
  { id: "text.markResolved", label: "Mark / unmark selected lines resolved", scope: ["text"], binding: "" },
  // araxis:text-view end

  // ---- Text Merge ----
  { id: "merge.save", label: "Save output", scope: ["merge"], binding: "Ctrl+S", fixed: true },
  { id: "merge.nextConflict", label: "Next conflict", scope: ["merge"], binding: "Alt+Down", fixed: true },
  { id: "merge.prevConflict", label: "Previous conflict", scope: ["merge"], binding: "Alt+Up", fixed: true },
  { id: "merge.refresh", label: "Reload files", scope: ["merge"], binding: "F5", fixed: true },
  // araxis:merge
  { id: "merge.undo", label: "Undo merge step (outside the output pane)", scope: ["merge"], binding: "Ctrl+Z", fixed: true },
  { id: "merge.redo", label: "Redo merge step (outside the output pane)", scope: ["merge"], binding: "Ctrl+Y", fixed: true },
  { id: "merge.autoMerge", label: "Automatic merge", scope: ["merge"], binding: "Ctrl+Shift+M" },
  { id: "merge.markResolved", label: "Mark selection resolved / unresolved", scope: ["merge"], binding: "Ctrl+Shift+R" },
  // araxis:merge end

  // ---- Table Compare ----
  { id: "table.nextDiff", label: "Next difference", scope: ["table"], binding: "Alt+Down", fixed: true },
  { id: "table.prevDiff", label: "Previous difference", scope: ["table"], binding: "Alt+Up", fixed: true },
  { id: "table.refresh", label: "Recompare", scope: ["table"], binding: "F5", fixed: true },

  // ---- Picture Compare ----
  { id: "picture.zoomIn", label: "Zoom in", scope: ["picture"], binding: "Ctrl+=", fixed: true },
  { id: "picture.zoomOut", label: "Zoom out", scope: ["picture"], binding: "Ctrl+-", fixed: true },
  { id: "picture.zoomFit", label: "Zoom to fit", scope: ["picture"], binding: "Ctrl+0", fixed: true },
  { id: "picture.refresh", label: "Reload pictures", scope: ["picture"], binding: "F5", fixed: true },

  // ---- Folder Compare / Merge ----
  { id: "folder.refresh", label: "Rescan folders", scope: ["folder"], binding: "F5", fixed: true },
  // bc5:file-ops — selection and file operations
  { id: "folder.selectAll", label: "Select all", scope: ["folder"], binding: "Ctrl+A" },
  { id: "folder.copyToRight", label: "Copy to right", scope: ["folder"], binding: "Ctrl+R" },
  { id: "folder.copyToLeft", label: "Copy to left", scope: ["folder"], binding: "Ctrl+L" },
  { id: "folder.moveToRight", label: "Move to right", scope: ["folder"], binding: "Ctrl+Shift+R" },
  { id: "folder.moveToLeft", label: "Move to left", scope: ["folder"], binding: "Ctrl+Shift+L" },
  { id: "folder.copyToFolder", label: "Copy to folder…", scope: ["folder"], binding: "Ctrl+Shift+C" },
  { id: "folder.moveToFolder", label: "Move to folder…", scope: ["folder"], binding: "Ctrl+Shift+M" },
  { id: "folder.delete", label: "Delete (Recycle Bin / Trash)", scope: ["folder"], binding: "Delete" },
  { id: "folder.deletePermanently", label: "Delete permanently", scope: ["folder"], binding: "Shift+Delete" },
  { id: "folder.rename", label: "Rename", scope: ["folder"], binding: "F2" },
  { id: "folder.newFolder", label: "New folder", scope: ["folder"], binding: "Ctrl+Shift+N" },
  { id: "folder.attributes", label: "Change attributes…", scope: ["folder"], binding: "Ctrl+Shift+A" },
  { id: "folder.touch", label: "Touch (set modified time)…", scope: ["folder"], binding: "Ctrl+Shift+T" },
  { id: "folder.operations", label: "Show / hide the Operations panel", scope: ["folder"], binding: "Ctrl+Shift+O" },
  // araxis:folder — Select… by criteria and the split view (file comparison below)
  { id: "folder.selectWhere", label: "Select files by status, name, size or date…", scope: ["folder"], binding: "Ctrl+Shift+E" },
  { id: "folder.splitView", label: "Show / hide the file comparison below", scope: ["folder"], binding: "Ctrl+Shift+V" },
  // araxis:folder end
  { id: "folderMerge.nextConflict", label: "Next conflict", scope: ["folderMerge"], binding: "Alt+Down", fixed: true },
  { id: "folderMerge.prevConflict", label: "Previous conflict", scope: ["folderMerge"], binding: "Alt+Up", fixed: true },
  { id: "folderMerge.refresh", label: "Rescan folders", scope: ["folderMerge"], binding: "F5", fixed: true },

  // ---- Registry ----
  { id: "registry.nextDiff", label: "Next difference", scope: ["registry"], binding: "Alt+Down", fixed: true },
  { id: "registry.prevDiff", label: "Previous difference", scope: ["registry"], binding: "Alt+Up", fixed: true },
  { id: "registry.refresh", label: "Recompare", scope: ["registry"], binding: "F5", fixed: true },
  // bc5:picture-registry — registry editing
  { id: "registry.save", label: "Save edited .reg files", scope: ["registry"], binding: "Ctrl+S", fixed: true },
  { id: "registry.undo", label: "Undo unsaved edit", scope: ["registry"], binding: "Ctrl+Z", fixed: true },
  { id: "registry.copyToRight", label: "Copy key/value to right", scope: ["registry"], binding: "Alt+Right", fixed: true },
  { id: "registry.copyToLeft", label: "Copy key/value to left", scope: ["registry"], binding: "Alt+Left", fixed: true },

  // ---- converted to matches() ----
  { id: "sync.refresh", label: "Rebuild sync preview", scope: ["sync"], binding: "F5" },
  { id: "database.refresh", label: "Recompare", scope: ["database"], binding: "F5" },
  { id: "database.copyToRight", label: "Copy selected difference to right", scope: ["database"], binding: "Alt+Right" },
  { id: "database.copyToLeft", label: "Copy selected difference to left", scope: ["database"], binding: "Alt+Left" },
  { id: "property.refresh", label: "Recompare", scope: ["version", "mp3"], binding: "F5" },

  // ---- bc5:editor-patch — Text Edit ----
  { id: "edit.save", label: "Save", scope: ["edit"], binding: "Ctrl+S" },
  { id: "edit.saveAs", label: "Save as…", scope: ["edit"], binding: "F12" },
  { id: "edit.undo", label: "Undo", scope: ["edit"], binding: "Ctrl+Z" },
  { id: "edit.redo", label: "Redo (also Ctrl+Shift+Z)", scope: ["edit"], binding: "Ctrl+Y" },
  { id: "edit.find", label: "Find", scope: ["edit"], binding: "Ctrl+F" },
  { id: "edit.replace", label: "Replace", scope: ["edit"], binding: "Ctrl+H" },
  { id: "edit.findNext", label: "Find next", scope: ["edit"], binding: "F3" },
  { id: "edit.findPrev", label: "Find previous", scope: ["edit"], binding: "Shift+F3" },
  { id: "edit.goToLine", label: "Go to line…", scope: ["edit"], binding: "Ctrl+G" },
  { id: "edit.toggleBookmark", label: "Toggle bookmark", scope: ["edit"], binding: "Ctrl+F2" },
  { id: "edit.nextBookmark", label: "Next bookmark", scope: ["edit"], binding: "F2" },
  { id: "edit.prevBookmark", label: "Previous bookmark", scope: ["edit"], binding: "Shift+F2" },
  { id: "edit.clearBookmarks", label: "Clear bookmarks", scope: ["edit"], binding: "Ctrl+Shift+F2" },
  { id: "edit.wordWrap", label: "Toggle word wrap", scope: ["edit"], binding: "Alt+Z" },
  { id: "edit.duplicateLines", label: "Duplicate lines", scope: ["edit"], binding: "Ctrl+D" },
  { id: "edit.moveLinesUp", label: "Move lines up", scope: ["edit"], binding: "Alt+Up" },
  { id: "edit.moveLinesDown", label: "Move lines down", scope: ["edit"], binding: "Alt+Down" },
  { id: "edit.reload", label: "Reload file", scope: ["edit"], binding: "F5" },
  // ---- bc5:editor-patch — Patch Viewer ----
  { id: "patch.nextHunk", label: "Next hunk", scope: ["patch"], binding: "Alt+Down" },
  { id: "patch.prevHunk", label: "Previous hunk", scope: ["patch"], binding: "Alt+Up" },
  { id: "patch.nextFile", label: "Next file", scope: ["patch"], binding: "Ctrl+Down" },
  { id: "patch.prevFile", label: "Previous file", scope: ["patch"], binding: "Ctrl+Up" },
  { id: "patch.applyFolder", label: "Apply against folder…", scope: ["patch"], binding: "Ctrl+Shift+A" },
  { id: "patch.openCompare", label: "Open patched file in Text Compare", scope: ["patch"], binding: "Ctrl+Enter" },
  { id: "patch.reload", label: "Reload patch", scope: ["patch"], binding: "F5" },
  // araxis:merge
  { id: "patch.toggleHunk", label: "Tick / untick the current hunk", scope: ["patch"], binding: "Space" },
  { id: "patch.applySelected", label: "Apply selected hunks to the folder", scope: ["patch"], binding: "Ctrl+Shift+Enter" },
  // araxis:merge end
];

const BY_ID = new Map(COMMANDS.map((c) => [c.id, c]));

export const isMac = typeof navigator !== "undefined" && /Mac|iPhone|iPad/.test(navigator.platform);

const NAMED_KEYS: Record<string, string> = {
  ArrowDown: "Down",
  ArrowUp: "Up",
  ArrowLeft: "Left",
  ArrowRight: "Right",
  " ": "Space",
  Escape: "Esc",
  "+": "=",
};
const MODIFIER_KEYS = new Set(["Control", "Shift", "Alt", "Meta", "AltGraph", "CapsLock", "OS"]);

/** The key part of a binding for this event, or null for a bare modifier press. */
function keyName(e: KeyboardEvent): string | null {
  if (MODIFIER_KEYS.has(e.key)) return null;
  // Physical letter/digit keys, so Alt/Option combos (which type symbols on
  // macOS) and keyboard layouts still give "Alt+N".
  const code = /^(?:Key([A-Z])|Digit([0-9]))$/.exec(e.code);
  if (code) return code[1] ?? code[2];
  const named = NAMED_KEYS[e.key];
  if (named) return named;
  if (e.key.length === 1) return e.key.toUpperCase();
  return e.key; // F5, Enter, Tab, Home, PageDown, Delete…
}

/** The keydown as a binding string ("Ctrl+Shift+S"), or null for a bare modifier. */
export function eventBinding(e: KeyboardEvent): string | null {
  const key = keyName(e);
  if (!key) return null;
  const mods = [e.ctrlKey || e.metaKey ? "Ctrl" : "", e.altKey ? "Alt" : "", e.shiftKey ? "Shift" : ""];
  return [...mods.filter(Boolean), key].join("+");
}

/** Normalises user/default text ("shift+ctrl+s") to the canonical order and case. */
export function normalizeBinding(binding: string): string {
  const parts = binding.split("+").map((p) => p.trim());
  // A trailing "+" key ("Ctrl++") splits into an empty last part.
  const key = parts.length > 1 && parts[parts.length - 1] === "" ? "=" : parts.pop() ?? "";
  if (!key) return "";
  const mods = new Set(parts.map((p) => p.toLowerCase()));
  const has = (...names: string[]) => names.some((n) => mods.has(n));
  const k = key.length === 1 ? key.toUpperCase() : key[0].toUpperCase() + key.slice(1);
  return [has("ctrl", "cmd", "meta", "mod") ? "Ctrl" : "", has("alt", "option") ? "Alt" : "", has("shift") ? "Shift" : "", k]
    .filter(Boolean)
    .join("+");
}

// Effective bindings, rebuilt when the overrides change.
let effective = bindingsWith(getPrefs().keys);
onPrefsChange((p) => (effective = bindingsWith(p.keys)));

/** The binding currently assigned to `id` ("" when unbound). */
export function bindingFor(id: string): string {
  return effective.get(id) ?? "";
}

/** True when the keydown triggers command `id`. */
export function matches(e: KeyboardEvent, id: string): boolean {
  const b = effective.get(id);
  return !!b && eventBinding(e) === b;
}

/** A binding for display, with macOS symbols where they're expected. */
export function formatBinding(binding: string): string {
  if (!binding) return "";
  if (!isMac) return binding;
  return binding.replace(/\bCtrl\b/, "⌘").replace(/\bAlt\b/, "⌥").replace(/\bShift\b/, "⇧");
}

/** Display text for a command's current shortcut, e.g. for `title` attributes. */
export const shortcutLabel = (id: string) => formatBinding(bindingFor(id));

const overlaps = (a: KeyScope[], b: KeyScope[]) =>
  a.includes("global") || b.includes("global") || a.some((s) => b.includes(s));

/**
 * Commands that would fire on the same key in the same place, given the
 * bindings in `bindings` (id → binding; defaults to the current ones).
 */
export function findConflicts(bindings: Map<string, string> = effective): Map<string, string[]> {
  const out = new Map<string, string[]>();
  for (const a of COMMANDS) {
    const ba = bindings.get(a.id);
    if (!ba) continue;
    for (const b of COMMANDS) {
      if (a.id === b.id || bindings.get(b.id) !== ba || !overlaps(a.scope, b.scope)) continue;
      out.set(a.id, [...(out.get(a.id) ?? []), b.id]);
    }
  }
  return out;
}

/** Effective bindings if `overrides` were the user's key prefs. */
export function bindingsWith(overrides: Record<string, string>): Map<string, string> {
  return new Map(
    COMMANDS.map((c) => [c.id, normalizeBinding(!c.fixed && c.id in overrides ? overrides[c.id] : c.binding)]),
  );
}

export const commandById = (id: string) => BY_ID.get(id);

/** Commands that apply in a tab of `kind` (global ones excluded). */
export const commandsFor = (kind: SessionKind) => COMMANDS.filter((c) => c.scope.includes(kind));
