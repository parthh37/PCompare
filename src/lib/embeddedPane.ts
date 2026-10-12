// Folder Compare's split view embeds a whole viewer (Text, Hex, Table or
// Picture Compare) under the folder list. Each viewer handles keys with a
// window listener gated on its tab being visible, and inside the split both
// the list and the viewer are visible — so keyboard focus decides who gets a
// key. The pane is focusable (tabIndex -1), so a click anywhere in it focuses
// it, and a click in the folder list moves focus back out.

export const EMBED_CLASS = "fc-embed";

/** True when `el` sits in a split pane that doesn't have focus: its keys belong to the folder list. */
export function embedInactive(el: Element | null | undefined): boolean {
  const pane = el?.closest(`.${EMBED_CLASS}`);
  return !!pane && !pane.contains(document.activeElement);
}

/** True when focus is inside a split pane: the folder list leaves keys to the embedded viewer. */
export function embedFocused(): boolean {
  return !!document.activeElement?.closest(`.${EMBED_CLASS}`);
}
