// Typed wrappers for the editable hex compare commands (src-tauri/src/hex.rs).
import { invoke } from "@tauri-apps/api/core";
import type { HexEditAt, HexRow, HexState, HexTarget, HexView } from "./api";

export type HexSide = 0 | 1;

export interface HexUndo {
  state: HexState;
  /** Where the change happened, or null if there was nothing to undo/redo. */
  at: HexEditAt | null;
}

export const hexApi = {
  /** Opens (or reopens) both files; `limitMb` caps the size aligned byte by byte. */
  open: (id: string, left: string, right: string, limitMb: number, view: HexView) =>
    invoke<HexState>("hex_open", { id, left, right, limitMb, view }),
  close: (id: string) => invoke<void>("hex_close", { id }),
  state: (id: string, view: HexView) => invoke<HexState>("hex_state", { id, view }),
  rows: (id: string, view: HexView, first: number, count: number) =>
    invoke<HexRow[]>("hex_rows", { id, view, first, count }),
  /** Replaces `remove` bytes at `offset` with `insert`. */
  edit: (id: string, side: HexSide, offset: number, remove: number, insert: number[], view: HexView) =>
    invoke<HexState>("hex_edit", { id, side, offset, remove, insert, view }),
  undo: (id: string, view: HexView) => invoke<HexUndo>("hex_undo", { id, view }),
  redo: (id: string, view: HexView) => invoke<HexUndo>("hex_redo", { id, view }),
  realign: (id: string, limitMb: number, view: HexView) => invoke<HexState>("hex_realign", { id, limitMb, view }),
  /** Next/previous difference from display position `from` (null: before the start). */
  navigate: (id: string, view: HexView, from: number | null, forward: boolean) =>
    invoke<HexTarget | null>("hex_navigate", { id, view, from, forward }),
  locate: (id: string, view: HexView, side: HexSide, offset: number) =>
    invoke<HexTarget>("hex_locate", { id, view, side, offset }),
  find: (id: string, side: HexSide, pattern: number[], from: number, forward: boolean, foldCase: boolean) =>
    invoke<number | null>("hex_find", { id, side, pattern, from, forward, foldCase }),
  read: (id: string, side: HexSide, offset: number, len: number) =>
    invoke<number[]>("hex_read", { id, side, offset, len }),
  replaceAll: (id: string, side: HexSide, pattern: number[], replacement: number[], foldCase: boolean, view: HexView) =>
    invoke<{ state: HexState; count: number }>("hex_replace_all", { id, side, pattern, replacement, foldCase, view }),
  save: (id: string, side: HexSide, backup: boolean, view: HexView) =>
    invoke<HexState>("hex_save", { id, side, backup, view }),
  diskChanged: (id: string) => invoke<[boolean, boolean]>("hex_disk_changed", { id }),
};

export type FindEncoding = "hex" | "ascii" | "utf8" | "utf16le" | "utf16be";

/**
 * Encodes a Find/Replace field as bytes. Hex accepts digits with optional
 * spaces (`DE AD be ef`); throws a readable message for bad input.
 */
export function encodePattern(text: string, encoding: FindEncoding): number[] {
  switch (encoding) {
    case "hex": {
      const digits = text.replace(/\s+/g, "");
      if (!/^[0-9a-fA-F]*$/.test(digits)) throw new Error("Hex patterns may only contain 0-9, A-F and spaces");
      if (digits.length % 2) throw new Error("Hex patterns need an even number of digits");
      return Array.from({ length: digits.length / 2 }, (_, i) => parseInt(digits.slice(i * 2, i * 2 + 2), 16));
    }
    case "ascii": {
      const out = Array.from(text, (c) => c.charCodeAt(0));
      if (out.some((b) => b > 0x7f)) throw new Error("ASCII patterns can't contain non-ASCII characters; use UTF-8");
      return out;
    }
    case "utf8":
      return Array.from(new TextEncoder().encode(text));
    case "utf16le":
    case "utf16be": {
      const out: number[] = [];
      for (let i = 0; i < text.length; i++) {
        const u = text.charCodeAt(i);
        const [lo, hi] = [u & 0xff, u >> 8];
        out.push(...(encoding === "utf16le" ? [lo, hi] : [hi, lo]));
      }
      return out;
    }
  }
}
