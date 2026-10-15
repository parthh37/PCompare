// Multi-selection and file operations for Folder Compare (bc5:file-ops).
//
// Selection works like Beyond Compare: a click selects the row on the side
// that was clicked (the middle status column selects both sides), Ctrl/⌘
// toggles rows, Shift extends a range, Ctrl+A selects everything shown, and
// ↑/↓ (with Shift) move through the list. Operations — copy / move to the
// other side or a folder, delete, rename, new folder, attributes, touch —
// work on the selected side's items, run in the background Operations queue
// and, when they finish, compare the rows they changed again (or rescan).
import { ArrowLeftToLine, ArrowRightToLine, ListTodo, Trash } from "lucide-react";
import { useEffect, useMemo, useRef, useState, type MouseEvent, type ReactNode, type RefObject } from "react";
import type { FolderCompareOptions, FolderCompareResult, FolderEntry, Side } from "../lib/api";
import { touchedRows } from "../lib/folderRefresh";
import {
  countSide,
  fileOps,
  isRemotePath,
  itemFilterFor,
  leftRelPaths,
  pathsOverlap,
  useFileOps,
  type FileAction,
  type FileOpOptions,
} from "../lib/fileOpsApi";
import { eventBinding, isMac, matches, shortcutLabel } from "../lib/keymap";
import { SEPARATOR, type MenuItems } from "../lib/nativeMenu";
import {
  AttributesDialog,
  DeleteDialog,
  NewFolderDialog,
  RenameDialog,
  TouchDialog,
  TransferDialog,
  type OpPart,
  type ReadyPart,
} from "./FileOpDialogs";
import { FileOpsPanel } from "./FileOpsPanel";

export type SelSide = Side | "both";

interface Selection {
  side: SelSide;
  rows: Set<string>;
  /** Where a Shift range starts. */
  anchor: string | null;
  /** The keyboard cursor. */
  focus: string | null;
}

type Dialog =
  | { kind: "transfer"; action: "copy" | "move"; parts: OpPart[]; toFolder: boolean }
  | { kind: "delete"; parts: OpPart[]; permanent: boolean }
  | { kind: "rename"; side: Side; root: string; rel: string }
  | { kind: "newFolder"; side: Side; root: string; folder: string }
  | { kind: "attributes"; parts: OpPart[]; attributes: number[]; modes: number[]; anyFolder: boolean }
  | { kind: "touch"; parts: OpPart[]; anyFolder: boolean; otherSide: boolean; initialMs: number | null };

interface Args {
  root: RefObject<HTMLDivElement | null>;
  result: FolderCompareResult | null;
  entries: FolderEntry[];
  /** The rows as shown, in order. */
  flat: { entry: FolderEntry }[];
  left: string;
  right: string;
  /** Opens the given entries (Enter). */
  onOpen: (entries: FolderEntry[]) => void;
  /** Rescans after an operation changed too much to refresh row by row. */
  onRefresh: () => void;
  /** Compares these rows (tree paths) again after an operation changed them. */
  onRefreshRows: (rels: string[]) => void;
  onError: (message: string) => void;
  /** The comparison's options: copies and moves leave out what its filters hide. */
  compareOptions?: FolderCompareOptions;
}

const ROW_HEIGHT = 24;
const otherSide = (s: Side): Side => (s === "left" ? "right" : "left");
const parentOf = (rel: string) => (rel.includes("/") ? rel.slice(0, rel.lastIndexOf("/")) : "");
const lastName = (rel: string) => rel.slice(rel.lastIndexOf("/") + 1);
const sidesOf = (s: SelSide): Side[] => (s === "both" ? ["left", "right"] : [s]);

export function useFolderFileOps({
  root,
  result,
  entries,
  flat,
  left,
  right,
  onOpen,
  onRefresh,
  onRefreshRows,
  onError,
  compareOptions,
}: Args) {
  const [sel, setSel] = useState<Selection>({ side: "both", rows: new Set(), anchor: null, focus: null });
  const [dialog, setDialog] = useState<Dialog | null>(null);
  const [panelOpen, setPanelOpen] = useState(false);
  const ops = useFileOps();
  const running = ops.filter((o) => o.status === "running").length;

  const leftRels = useMemo(() => leftRelPaths(entries), [entries]);
  const relOn = (side: Side, e: FolderEntry) => (side === "right" ? e.relPath : (leftRels.get(e.relPath) ?? e.relPath));
  const rootOf = (side: Side) => (result ? (side === "left" ? result.leftRoot : result.rightRoot) : "");

  const index = useMemo(() => new Map(flat.map((r, i) => [r.entry.relPath, i])), [flat]);
  const selected = useMemo(() => flat.map((r) => r.entry).filter((e) => sel.rows.has(e.relPath)), [flat, sel.rows]);
  const focusEntry = (sel.focus != null ? flat[index.get(sel.focus) ?? -1]?.entry : undefined) ?? selected[0];

  // When an operation that touched our folders finishes (in any tab), compare
  // the rows it changed again; rescan when that can't be worked out.
  const refresh = useRef({ onRefresh, onRefreshRows, left, right, result });
  refresh.current = { onRefresh, onRefreshRows, left, right, result };
  useEffect(
    () =>
      fileOps.onFinished((op) => {
        const { left: l, right: r, result: res, onRefresh: rescan, onRefreshRows: compareRows } = refresh.current;
        const rows = res ? touchedRows(op, { left: l, right: r, entries: res.entries }) : null;
        if (rows) {
          if (rows.length) compareRows(rows);
          return;
        }
        const touched = [op.srcRoot, op.dstRoot ?? ""];
        if (touched.some((t) => pathsOverlap(t, l) || pathsOverlap(t, r))) rescan();
      }),
    [],
  );

  // ----- selection -----

  const scrollIntoView = (row: number) => {
    const list = root.current?.querySelector<HTMLElement>(".folder-list");
    if (!list) return;
    const top = row * ROW_HEIGHT;
    if (top < list.scrollTop) list.scrollTop = top;
    else if (top + ROW_HEIGHT > list.scrollTop + list.clientHeight) list.scrollTop = top + ROW_HEIGHT - list.clientHeight;
  };

  const range = (from: string, to: string): string[] => {
    const [a, b] = [index.get(from) ?? 0, index.get(to) ?? 0];
    return flat.slice(Math.min(a, b), Math.max(a, b) + 1).map((r) => r.entry.relPath);
  };

  const onRowClick = (ev: MouseEvent<HTMLElement>, e: FolderEntry) => {
    const box = ev.currentTarget.getBoundingClientRect();
    let side: SelSide = (ev.target as HTMLElement).closest(".fcell.status")
      ? "both"
      : ev.clientX < box.left + box.width / 2
        ? "left"
        : "right";
    const rel = e.relPath;
    const toggleKey = ev.ctrlKey || ev.metaKey;
    if (ev.shiftKey && sel.anchor && index.has(sel.anchor)) {
      const rows = new Set(toggleKey ? sel.rows : []);
      range(sel.anchor, rel).forEach((r) => rows.add(r));
      setSel({ side, rows, anchor: sel.anchor, focus: rel });
    } else if (toggleKey) {
      const rows = new Set(sel.rows);
      rows.has(rel) ? rows.delete(rel) : rows.add(rel);
      setSel({ side, rows, anchor: rel, focus: rel });
    } else {
      // A click on the empty half of an orphan row means the item itself.
      if (side !== "both" && !e[side] && e[otherSide(side)]) side = otherSide(side);
      setSel({ side, rows: new Set([rel]), anchor: rel, focus: rel });
    }
  };

  /** Right-click: keeps a selection that includes the row, else selects just it. */
  const onRowContext = (e: FolderEntry, side: Side) => {
    if (!sel.rows.has(e.relPath)) setSel({ side, rows: new Set([e.relPath]), anchor: e.relPath, focus: e.relPath });
    else setSel((s) => ({ ...s, focus: e.relPath }));
  };

  /** The entries a context menu on `e` acts on: the selection when `e` is part of it. */
  const menuEntries = (e: FolderEntry): FolderEntry[] => (sel.rows.has(e.relPath) && selected.length ? selected : [e]);

  const moveFocus = (delta: number, extend: boolean) => {
    if (!flat.length) return;
    const at = sel.focus != null ? (index.get(sel.focus) ?? -1) : -1;
    const next = Math.max(0, Math.min(flat.length - 1, at < 0 ? (delta > 0 ? 0 : flat.length - 1) : at + delta));
    const rel = flat[next].entry.relPath;
    if (extend && sel.anchor && index.has(sel.anchor)) {
      setSel({ ...sel, rows: new Set(range(sel.anchor, rel)), focus: rel });
    } else {
      setSel({ ...sel, rows: new Set([rel]), anchor: rel, focus: rel });
    }
    scrollIntoView(next);
  };

  // araxis:folder — "Select…" by criteria: replace the selection, or add to it.
  const selectRows = (rows: string[], side: SelSide, add: boolean) =>
    setSel((s) => {
      const keep = add && s.rows.size > 0;
      const next = new Set(keep ? s.rows : []);
      rows.forEach((r) => next.add(r));
      const first = rows[0] ?? null;
      return {
        side: keep && s.side !== side ? "both" : side,
        rows: next,
        anchor: keep ? s.anchor : first,
        focus: first ?? s.focus,
      };
    });
  // araxis:folder end

  const selectAll = () =>
    setSel((s) => ({ ...s, rows: new Set(flat.map((r) => r.entry.relPath)), anchor: s.anchor ?? flat[0]?.entry.relPath ?? null }));

  const rowClass = (rel: string): string => {
    let c = "";
    if (sel.rows.has(rel)) c += sel.side === "both" ? " selected" : ` fc-sel-${sel.side}`;
    if (sel.focus === rel && sel.rows.size > 1) c += " fc-focus";
    return c;
  };

  // ----- building operations -----

  const part = (side: Side, list: FolderEntry[]): OpPart => {
    const present = list.filter((e) => e[side]);
    const other = otherSide(side);
    return {
      side,
      srcRoot: rootOf(side),
      dstRoot: rootOf(other) || null,
      items: present.map((e) => ({ src: relOn(side, e), dst: relOn(other, e) })),
      ...countSide(present, side),
    };
  };

  /** One part per selected side; "both" covers each side's own items. */
  const sideParts = (list: FolderEntry[]): OpPart[] =>
    sidesOf(sel.side)
      .filter((s) => rootOf(s))
      .map((s) => part(s, list))
      .filter((p) => p.items.length);

  const start = (ready: ReadyPart[], action: FileAction, options: Partial<FileOpOptions>) => {
    setDialog(null);
    setPanelOpen(true);
    for (const p of ready) {
      if (!p.items.length) continue;
      fileOps.start(p.title, p.srcRoot, p.dstRoot, { action, items: p.items, options });
    }
  };

  const nothing = () => onError("Select files or folders first.");

  const copyTo = (action: "copy" | "move", target: Side) => {
    if (!selected.length) return nothing();
    if (!rootOf(target)) return onError(`Choose a folder on the ${target} first.`);
    const p = part(otherSide(target), selected);
    if (!p.items.length) return onError(`Nothing selected exists on the ${otherSide(target)}.`);
    setDialog({ kind: "transfer", action, parts: [p], toFolder: false });
  };

  const toFolder = (action: "copy" | "move") => {
    if (!selected.length) return nothing();
    // Both sides selected: each row's left item, or its right item if it is right-only.
    const parts =
      sel.side === "both"
        ? [part("left", selected.filter((e) => e.left)), part("right", selected.filter((e) => !e.left))]
        : [part(sel.side, selected)];
    const usable = parts.filter((p) => p.items.length && p.srcRoot);
    if (!usable.length) return nothing();
    setDialog({ kind: "transfer", action, parts: usable, toFolder: true });
  };

  const remove = (permanent: boolean) => {
    if (!selected.length) return nothing();
    const parts = sideParts(selected);
    if (!parts.length) return nothing();
    setDialog({ kind: "delete", parts, permanent });
  };

  const rename = () => {
    const e = focusEntry;
    if (!e) return nothing();
    const side: Side = sel.side === "both" ? (e.left ? "left" : "right") : sel.side;
    if (!e[side]) return onError(`${e.name} isn't on the ${side}.`);
    setDialog({ kind: "rename", side, root: rootOf(side), rel: relOn(side, e) });
  };

  const newFolder = () => {
    const side: Side = sel.side === "both" ? (rootOf("left") ? "left" : "right") : sel.side;
    if (!rootOf(side)) return onError(`Choose a folder on the ${side} first.`);
    const e = focusEntry;
    // Inside the focused folder, else next to the focused item.
    const folder = !e ? "" : e.isDir && e[side] ? relOn(side, e) : parentOf(relOn(side, e));
    setDialog({ kind: "newFolder", side, root: rootOf(side), folder });
  };

  const attributes = () => {
    if (!selected.length) return nothing();
    const parts = sideParts(selected).filter((p) => !isRemotePath(p.srcRoot));
    if (!parts.length) return onError("Attributes can only be changed on local folders.");
    const metas = parts.flatMap((p) => selected.map((e) => e[p.side]).filter((m) => m != null));
    const modes = metas.filter((m) => m.mode != null).map((m) => m.mode!);
    setDialog({
      kind: "attributes",
      parts,
      attributes: metas.map((m) => m.attributes ?? 0),
      modes: modes.length === metas.length ? modes : [],
      anyFolder: parts.some((p) => p.folders > 0),
    });
  };

  const touch = () => {
    if (!selected.length) return nothing();
    const parts = sideParts(selected);
    if (!parts.length) return nothing();
    const first = selected[0][parts[0].side];
    setDialog({
      kind: "touch",
      parts,
      anyFolder: parts.some((p) => p.folders > 0),
      otherSide: parts.every((p) => p.dstRoot),
      initialMs: first?.modifiedMs ?? null,
    });
  };

  // ----- keyboard -----

  const keys = useRef<(e: KeyboardEvent) => boolean>(() => false);
  keys.current = (e: KeyboardEvent): boolean => {
    const plainArrow = !e.ctrlKey && !e.metaKey && !e.altKey;
    if (plainArrow && (e.key === "ArrowDown" || e.key === "ArrowUp")) {
      moveFocus(e.key === "ArrowDown" ? 1 : -1, e.shiftKey);
      return true;
    }
    if (plainArrow && (e.key === "Home" || e.key === "End")) {
      moveFocus(e.key === "Home" ? -flat.length : flat.length, e.shiftKey);
      return true;
    }
    if (e.key === "Enter" && !e.ctrlKey && !e.metaKey && !e.altKey && !e.shiftKey && selected.length) {
      onOpen(selected);
      return true;
    }
    const table: [string, () => void][] = [
      ["folder.selectAll", selectAll],
      ["folder.copyToRight", () => copyTo("copy", "right")],
      ["folder.copyToLeft", () => copyTo("copy", "left")],
      ["folder.moveToRight", () => copyTo("move", "right")],
      ["folder.moveToLeft", () => copyTo("move", "left")],
      ["folder.copyToFolder", () => toFolder("copy")],
      ["folder.moveToFolder", () => toFolder("move")],
      ["folder.delete", () => remove(false)],
      ["folder.deletePermanently", () => remove(true)],
      ["folder.rename", rename],
      ["folder.newFolder", newFolder],
      ["folder.attributes", attributes],
      ["folder.touch", touch],
      ["folder.operations", () => setPanelOpen((o) => !o)],
    ];
    for (const [id, run] of table) {
      if (matches(e, id)) {
        run();
        return true;
      }
    }
    // macOS keyboards call Delete "Backspace"; ⌘⌫ moves to the Trash as in Finder.
    if (isMac && eventBinding(e) === "Ctrl+Backspace") {
      remove(false);
      return true;
    }
    return false;
  };

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      // Every open tab stays mounted; only the visible one handles keys.
      if (!root.current?.offsetParent) return;
      const target = e.target as HTMLElement | null;
      if (target?.closest("input, textarea, select, [contenteditable='true']")) return;
      if (document.querySelector(".modal-backdrop")) return;
      if (target?.closest(".fc-embed")) return; // araxis:folder — keys belong to the split view's viewer
      if (keys.current(e)) e.preventDefault();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [root]);

  // ----- UI pieces -----

  const tip = (label: string, id: string) => {
    const k = shortcutLabel(id);
    return k ? `${label} (${k})` : label;
  };
  const has = (side: Side) => selected.some((e) => e[side]);

  const toolbar = (
    <>
      <span className="tool-sep" />
      <button
        className="tool-btn"
        title={tip("Copy the selection to the right", "folder.copyToRight")}
        disabled={!has("left") || !right}
        onClick={() => copyTo("copy", "right")}
      >
        <ArrowRightToLine size={20} className="fop-tool" />
        <span>Copy →</span>
      </button>
      <button
        className="tool-btn"
        title={tip("Copy the selection to the left", "folder.copyToLeft")}
        disabled={!has("right") || !left}
        onClick={() => copyTo("copy", "left")}
      >
        <ArrowLeftToLine size={20} className="fop-tool" />
        <span>← Copy</span>
      </button>
      <button
        className="tool-btn"
        title={tip("Delete the selection (Recycle Bin / Trash)", "folder.delete")}
        disabled={!selected.length}
        onClick={() => remove(false)}
      >
        <Trash size={20} className="fop-tool" />
        <span>Delete</span>
      </button>
      <button
        className={`tool-btn${panelOpen ? " active" : ""}`}
        title={tip("Show background file operations", "folder.operations")}
        onClick={() => setPanelOpen(!panelOpen)}
      >
        <ListTodo size={20} className="fop-tool" />
        <span className="fop-badge">{running ? `Ops · ${running}` : "Ops"}</span>
      </button>
    </>
  );

  /** File-operation items for the context menu; they act on the selection. */
  const menuItems = (): MenuItems => {
    const sideLabel = sel.side === "both" ? "both sides" : `the ${sel.side}`;
    const item = (label: string, id: string, disabled: boolean, onClick: () => void) => ({
      label,
      shortcut: shortcutLabel(id),
      disabled,
      onClick,
    });
    return [
      { heading: selected.length > 1 ? `${selected.length} items on ${sideLabel}` : `File operations · ${sideLabel}` },
      item("Copy to Right", "folder.copyToRight", !has("left") || !right, () => copyTo("copy", "right")),
      item("Copy to Left", "folder.copyToLeft", !has("right") || !left, () => copyTo("copy", "left")),
      item("Move to Right", "folder.moveToRight", !has("left") || !right, () => copyTo("move", "right")),
      item("Move to Left", "folder.moveToLeft", !has("right") || !left, () => copyTo("move", "left")),
      item("Copy to Folder…", "folder.copyToFolder", !selected.length, () => toFolder("copy")),
      item("Move to Folder…", "folder.moveToFolder", !selected.length, () => toFolder("move")),
      SEPARATOR,
      item("Rename…", "folder.rename", selected.length !== 1, rename),
      item("New Folder…", "folder.newFolder", false, newFolder),
      item("Delete", "folder.delete", !selected.length, () => remove(false)),
      item("Delete Permanently…", "folder.deletePermanently", !selected.length, () => remove(true)),
      SEPARATOR,
      item("Change Attributes…", "folder.attributes", !selected.length, attributes),
      item("Touch (Set Modified Time)…", "folder.touch", !selected.length, touch),
    ];
  };

  const panel = panelOpen ? <FileOpsPanel ops={ops} onClose={() => setPanelOpen(false)} /> : null;

  const close = () => setDialog(null);
  let overlay: ReactNode = null;
  if (dialog?.kind === "transfer") {
    overlay = (
      <TransferDialog
        action={dialog.action}
        parts={dialog.parts}
        toFolder={dialog.toFolder}
        filter={compareOptions ? itemFilterFor(compareOptions) : null}
        onCancel={close}
        onConfirm={(ready, options) => start(ready, dialog.action, options)}
      />
    );
  } else if (dialog?.kind === "delete") {
    const verb = (p: OpPart) => `Delete ${p.items.length === 1 ? lastName(p.items[0].src) : `${p.items.length} items`} on the ${p.side}`;
    overlay = (
      <DeleteDialog
        parts={dialog.parts}
        permanent={dialog.permanent}
        onCancel={close}
        onConfirm={(permanent) =>
          start(
            dialog.parts.map((p) => ({ title: verb(p), srcRoot: p.srcRoot, dstRoot: null, items: p.items.map((i) => ({ src: i.src })) })),
            "delete",
            { permanent },
          )
        }
      />
    );
  } else if (dialog?.kind === "rename") {
    overlay = (
      <RenameDialog
        name={lastName(dialog.rel)}
        side={dialog.side}
        remote={isRemotePath(dialog.root)}
        onCancel={close}
        onConfirm={(newName) =>
          start(
            [{ title: `Rename ${lastName(dialog.rel)} → ${newName}`, srcRoot: dialog.root, dstRoot: null, items: [{ src: dialog.rel }] }],
            "rename",
            { newName },
          )
        }
      />
    );
  } else if (dialog?.kind === "newFolder") {
    overlay = (
      <NewFolderDialog
        side={dialog.side}
        where={dialog.folder}
        onCancel={close}
        onConfirm={(name) =>
          start(
            [
              {
                title: `New folder ${name} on the ${dialog.side}`,
                srcRoot: dialog.root,
                dstRoot: null,
                items: [{ src: dialog.folder ? `${dialog.folder}/${name}` : name }],
              },
            ],
            "newFolder",
            {},
          )
        }
      />
    );
  } else if (dialog?.kind === "attributes") {
    overlay = (
      <AttributesDialog
        attributes={dialog.attributes}
        modes={dialog.modes}
        count={dialog.parts.reduce((n, p) => n + p.items.length, 0)}
        anyFolder={dialog.anyFolder}
        onCancel={close}
        onConfirm={(change, recursive) =>
          start(
            dialog.parts.map((p) => ({
              title: `Attributes of ${p.items.length} item${p.items.length === 1 ? "" : "s"} on the ${p.side}`,
              srcRoot: p.srcRoot,
              dstRoot: null,
              items: p.items.map((i) => ({ src: i.src })),
            })),
            "attributes",
            { attributes: change, recursive },
          )
        }
      />
    );
  } else if (dialog?.kind === "touch") {
    overlay = (
      <TouchDialog
        count={dialog.parts.reduce((n, p) => n + p.items.length, 0)}
        anyFolder={dialog.anyFolder}
        otherSide={dialog.otherSide}
        initialMs={dialog.initialMs}
        onCancel={close}
        onConfirm={(time, recursive) =>
          start(
            dialog.parts.map((p) => ({
              title: `Touch ${p.items.length} item${p.items.length === 1 ? "" : "s"} on the ${p.side}`,
              srcRoot: p.srcRoot,
              dstRoot: time.kind === "otherSide" ? p.dstRoot : null,
              items: p.items,
            })),
            "touch",
            { touch: time, recursive },
          )
        }
      />
    );
  }

  // araxis:folder — selectRows added
  return { sel, selected, onRowClick, onRowContext, menuEntries, rowClass, toolbar, menuItems, panel, overlay, selectRows };
}
