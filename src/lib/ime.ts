// IME (input method editor) support for keydown handlers. While a composition
// is open (Chinese, Japanese, Korean input, dead keys…), Enter, Tab, Escape and
// the arrows belong to the IME: they pick or cancel a candidate, so shortcut
// handlers must leave them alone.
import type { KeyboardEvent as ReactKeyboardEvent } from "react";

/** True while an IME composition is in progress (keyCode 229 covers WebKit's keydown before compositionstart). */
export function isComposing(e: KeyboardEvent | ReactKeyboardEvent): boolean {
  const n: KeyboardEvent = "nativeEvent" in e ? e.nativeEvent : e;
  return n.isComposing || n.keyCode === 229;
}
