// Dropping a file or folder onto one path field fills that field. Tauri
// reports drag-and-drop for the whole webview with a position in physical
// pixels, so the field under the cursor is found with elementFromPoint and
// told through a DOM event (PathField listens for it).

export const PATH_DROP_EVENT = "pcompare:path-drop";
const HOVER_CLASS = "drop-target";

interface Position {
  x: number;
  y: number;
}

/** The visible path field under a drag position (physical pixels from the webview's top-left). */
function fieldAt(pos: Position): HTMLElement | null {
  const scale = window.devicePixelRatio || 1;
  // The topmost element: a field covered by a dialog or hidden in another tab isn't hit.
  return document.elementFromPoint(pos.x / scale, pos.y / scale)?.closest<HTMLElement>(".pathfield") ?? null;
}

/** Marks the field under a drag (or clears every mark for `null`). */
export function highlightPathField(pos: Position | null) {
  const target = pos ? fieldAt(pos) : null;
  document.querySelectorAll(`.pathfield.${HOVER_CLASS}`).forEach((el) => el !== target && el.classList.remove(HOVER_CLASS));
  target?.classList.add(HOVER_CLASS);
}

/** Hands `path` to the field under `pos`; false when the drop missed every field. */
export function dropOnPathField(path: string, pos: Position): boolean {
  highlightPathField(null);
  const field = fieldAt(pos);
  if (!field) return false;
  field.dispatchEvent(new CustomEvent<string>(PATH_DROP_EVENT, { detail: path }));
  return true;
}
