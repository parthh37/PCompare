// Recently compared paths, offered in a dropdown under every path field.
// Stored in prefs (prefs.json) so the history survives updates; folders and
// files are kept apart so a file field never suggests a folder.
import { getPrefs, RECENT_PATHS_MAX, setPrefs, usePref } from "./prefs";

export type PathKind = "folder" | "file";

const key = (kind: PathKind) => (kind === "folder" ? "recentFolders" : "recentFiles");

/** Windows drive and UNC paths compare case-insensitively; everything else exactly. */
const windowsish = (p: string) => /^[a-z]:[\\/]/i.test(p) || p.startsWith("\\\\");

/** Identity used for de-duplication: trailing separators and (on Windows paths) case ignored. */
export function samePath(a: string, b: string): boolean {
  const norm = (p: string) => {
    const t = p.trim().replace(/(?<=.)[\\/]+$/, "");
    return windowsish(t) ? t.toLowerCase() : t;
  };
  return norm(a) === norm(b);
}

/** Moves `path` to the front of its history (adding it if new). */
export function rememberPath(kind: PathKind, path: string) {
  const p = path.trim();
  if (!p) return;
  const list = getPrefs()[key(kind)];
  if (list[0] !== undefined && samePath(list[0], p)) return; // already newest: skip the write
  setPrefs({ [key(kind)]: [p, ...list.filter((x) => !samePath(x, p))].slice(0, RECENT_PATHS_MAX) });
}

export function forgetPath(kind: PathKind, path: string) {
  setPrefs((prefs) => ({ [key(kind)]: prefs[key(kind)].filter((x) => !samePath(x, path)) }));
}

export const useRecentPaths = (kind: PathKind): string[] => usePref(key(kind));

/**
 * Which history entries to offer for what's typed in the field, best first.
 * `history` is newest first; `query` is the field's current text.
 */
export function matchRecent(history: string[], query: string): string[] {
  const q = query.trim().toLowerCase();
  if (!q) return history;
  // Prefix matches (continuing a path) rank above matches elsewhere (a folder name
  // typed from memory); within each group recency order is kept.
  const prefix: string[] = [];
  const inside: string[] = [];
  for (const p of history) {
    const l = p.toLowerCase();
    if (l === q) continue; // the field already holds it
    if (l.startsWith(q)) prefix.push(p);
    else if (l.includes(q)) inside.push(p);
  }
  return [...prefix, ...inside];
}
