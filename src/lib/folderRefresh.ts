// Which rows of a folder comparison a finished file operation changed, so
// the view can compare just those again (`api.compareFolderEntries`) and
// swap them in with `replaceEntry` instead of rescanning both folders.
import type { FolderEntry, Side } from "./api";
import type { FileOp } from "./fileOpsApi";

/** Most rows refreshed one by one; an operation touching more gets a rescan. */
export const MAX_ROW_REFRESH = 300;

const clean = (p: string) => p.replace(/\\/g, "/").replace(/\/+$/, "");

/** `path` relative to `root` ("" for the root itself), or null when it isn't inside. */
function within(path: string, root: string): string | null {
  const [p, r] = [clean(path), clean(root)];
  if (!r) return null;
  if (p.toLowerCase() === r.toLowerCase()) return "";
  return p.toLowerCase().startsWith(`${r.toLowerCase()}/`) ? p.slice(r.length + 1) : null;
}

const join = (root: string, rel: string) => {
  const r = rel.replace(/^\/+|\/+$/g, "");
  return r ? `${clean(root)}/${r}` : clean(root);
};
const parentOf = (rel: string) => (rel.includes("/") ? rel.slice(0, rel.lastIndexOf("/")) : "");

/** Every path an operation wrote to (as `lock_paths` in pcompare-sync lists them, minus what it only read). */
export function writtenPaths(op: FileOp): string[] {
  const { action, items, options } = op.request;
  const out: string[] = [];
  for (const item of items) {
    if (action !== "copy") out.push(join(op.srcRoot, item.src));
    if ((action === "copy" || action === "move") && op.dstRoot) out.push(join(op.dstRoot, item.dst ?? item.src));
    if (action === "rename" && options.newName) {
      const parent = parentOf(item.src.replace(/^\/+|\/+$/g, ""));
      out.push(join(op.srcRoot, parent ? `${parent}/${options.newName}` : options.newName));
    }
  }
  return out;
}

export interface ComparedFolders {
  left: string;
  right: string;
  /** The whole tree, rows the filters leave out included. */
  entries: FolderEntry[];
}

/**
 * The rows of `view` that `op` changed, to compare again: `[]` when it
 * wrote nowhere inside either folder, null when only a rescan will do (too
 * many rows, or one of the compared folders itself was replaced). A row
 * whose folder didn't exist on that side before is replaced by the topmost
 * such folder, since that folder's own row changes too.
 */
export function touchedRows(op: FileOp, view: ComparedFolders): string[] | null {
  const rows = new Map<string, FolderEntry>();
  /** Each tree row's path on the left; the inverse maps the left side's paths to rows. */
  const treeOfLeft = new Map<string, string>();
  const walk = (list: FolderEntry[], leftParent: string | null) => {
    for (const e of list) {
      rows.set(e.relPath, e);
      const left = e.leftRelPath ?? (e.left || leftParent === null ? e.relPath : `${leftParent}/${e.name}`);
      if (!treeOfLeft.has(left)) treeOfLeft.set(left, e.relPath);
      walk(e.children, left);
    }
  };
  walk(view.entries, null);
  // A left path not in the tree (new on the left) sits in the row its folder maps to.
  const toTree = (side: Side, rel: string): string => {
    if (side === "right" || !rel) return rel;
    const known = treeOfLeft.get(rel);
    if (known != null) return known;
    const parent = toTree(side, parentOf(rel));
    const name = rel.slice(rel.lastIndexOf("/") + 1);
    return parent ? `${parent}/${name}` : name;
  };

  const found = new Set<string>();
  for (const path of writtenPaths(op)) {
    for (const side of ["left", "right"] as const) {
      const root = view[side];
      const rel = root ? within(path, root) : null;
      if (rel == null) continue;
      // The compared folder itself: nothing above it to swap a row into.
      if (rel === "") return null;
      let at = toTree(side, rel);
      // Climb out of folders the operation created on this side.
      for (let up = parentOf(at); up && !rows.get(up)?.[side]; up = parentOf(up)) at = up;
      found.add(at);
    }
  }
  // A row compared in full covers everything below it.
  const covered = (rel: string) => {
    for (let up = parentOf(rel); up; up = parentOf(up)) if (found.has(up)) return true;
    return false;
  };
  const out = [...found].filter((rel) => !covered(rel)).sort();
  return out.length > MAX_ROW_REFRESH ? null : out;
}
