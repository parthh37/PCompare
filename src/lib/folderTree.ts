// Pure helpers over the folder-compare result tree: display filters, name
// search, flattening, "ignore" marks, and re-tallying after per-file content
// re-checks.
import type { ContentVerdict, EntryStatus, FolderEntry, FolderStats } from "./api";

export type DisplayFilter =
  | "all"
  | "diffs"
  | "diffsNoOrphans"
  | "same"
  | "orphans"
  | "leftOrphans"
  | "rightOrphans"
  | "leftNewer"
  | "rightNewer";

export const DISPLAY_FILTERS: { value: DisplayFilter; label: string; primary?: boolean }[] = [
  { value: "all", label: "All", primary: true },
  { value: "diffs", label: "Differences", primary: true },
  { value: "same", label: "Same", primary: true },
  { value: "orphans", label: "Orphans", primary: true },
  { value: "diffsNoOrphans", label: "Differences (no orphans)" },
  { value: "leftOrphans", label: "Left orphans" },
  { value: "rightOrphans", label: "Right orphans" },
  { value: "leftNewer", label: "Left newer" },
  { value: "rightNewer", label: "Right newer" },
];

const isOrphan = (s: EntryStatus) => s === "leftOnly" || s === "rightOnly";

/** The status filters and totals go by: entries marked "ignore" count as same. */
export const effectiveStatus = (e: FolderEntry): EntryStatus => (e.ignored ? "same" : e.status);

/**
 * "Similar" files (only unimportant differences under rules-based compare)
 * and ignored ones count as same, so "Differences" shows exactly what needs
 * attention.
 */
export function matchesFilter(entry: FolderEntry, f: DisplayFilter): boolean {
  const e = entry.ignored ? { status: "same" as EntryStatus, newer: null } : entry;
  switch (f) {
    case "all":
      return true;
    case "diffs":
      return e.status !== "same" && e.status !== "similar";
    case "diffsNoOrphans":
      return e.status !== "same" && e.status !== "similar" && !isOrphan(e.status);
    case "same":
      return e.status === "same" || e.status === "similar";
    case "orphans":
      return isOrphan(e.status);
    case "leftOrphans":
      return e.status === "leftOnly";
    case "rightOrphans":
      return e.status === "rightOnly";
    case "leftNewer":
      return e.newer === "left";
    case "rightNewer":
      return e.newer === "right";
  }
}

/** Turns a quick-filter string into a predicate: `*`/`?` wildcards, otherwise substring. */
export function nameMatcher(query: string): ((name: string) => boolean) | null {
  const q = query.trim().toLowerCase();
  if (!q) return null;
  if (!/[*?]/.test(q)) return (name) => name.toLowerCase().includes(q);
  const re = new RegExp(
    "^" +
      q
        .split("")
        .map((c) => (c === "*" ? ".*" : c === "?" ? "." : c.replace(/[.+^${}()|[\]\\]/g, "\\$&")))
        .join("") +
      "$",
  );
  return (name) => re.test(name.toLowerCase());
}

/**
 * Post-order pass: a file is visible if it passes both filters; a folder is
 * visible if any descendant is (or, with no name query, if it matches itself).
 */
export function visibleSet(
  entries: FolderEntry[],
  f: DisplayFilter,
  name: ((name: string) => boolean) | null,
): Set<string> {
  const out = new Set<string>();
  const visit = (e: FolderEntry): boolean => {
    let childVisible = false;
    for (const c of e.children) childVisible = visit(c) || childVisible;
    const self = matchesFilter(e, f) && (!name || (!e.isDir && name(e.name)));
    const show = (e.isDir && childVisible) || (self && (!e.isDir || !name));
    if (show) out.add(e.relPath);
    return show;
  };
  entries.forEach(visit);
  return out;
}

export function allDirs(entries: FolderEntry[], out = new Set<string>()): Set<string> {
  for (const e of entries) {
    if (e.isDir) {
      out.add(e.relPath);
      allDirs(e.children, out);
    }
  }
  return out;
}

/** Every file below `entries` that exists on both sides. */
export function pairedFiles(entries: FolderEntry[], out: FolderEntry[] = []): FolderEntry[] {
  for (const e of entries) {
    if (e.isDir) pairedFiles(e.children, out);
    else if (e.left && e.right) out.push(e);
  }
  return out;
}

/** Mirrors `folder_status` in pcompare-core, with ignored entries as same. */
function folderStatus(children: FolderEntry[]): EntryStatus {
  const st = children.map(effectiveStatus);
  if (st.every((s) => s === "same")) return "same";
  if (st.every((s) => s === "same" || s === "similar")) return "similar";
  return "different";
}

/**
 * Applies per-file content verdicts (from "Compare contents" on selected
 * rows) and "ignore" marks (relative paths; a folder's mark covers
 * everything below it), then recomputes the status of every ancestor folder.
 */
export function applyVerdicts(
  entries: FolderEntry[],
  verdicts: Map<string, ContentVerdict & { method: FolderEntry["contentChecked"] }>,
  ignored: ReadonlySet<string> = new Set(),
): FolderEntry[] {
  if (verdicts.size === 0 && ignored.size === 0) return entries;
  const walk = (list: FolderEntry[], inherited: boolean): FolderEntry[] =>
    list.map((e) => {
      const ignore = inherited || ignored.has(e.relPath);
      let next = e;
      if (e.isDir) {
        const children = walk(e.children, ignore);
        const keep = !e.children.length || e.status === "leftOnly" || e.status === "rightOnly" || e.status === "error";
        next = { ...e, children, status: keep ? e.status : folderStatus(children) };
      } else {
        const v = verdicts.get(e.relPath);
        if (v) next = { ...e, status: v.status, crc: v.crc ?? e.crc, contentChecked: v.method };
      }
      return ignore ? { ...next, ignored: true } : next;
    });
  return walk(entries, false);
}

/** Mirrors `tally` in pcompare-core so stats stay right after re-checks and ignore marks. */
export function tally(entries: FolderEntry[], stats: FolderStats = emptyStats()): FolderStats {
  for (const e of entries) {
    if (!e.isDir) {
      if (e.ignored) stats.ignored = (stats.ignored ?? 0) + 1;
      switch (effectiveStatus(e)) {
        case "same":
          stats.same++;
          break;
        case "similar":
          stats.similar++;
          break;
        case "different":
        case "typeMismatch":
          stats.different++;
          break;
        case "leftOnly":
          stats.leftOnly++;
          break;
        case "rightOnly":
          stats.rightOnly++;
          break;
        case "error":
          stats.errors++;
          break;
      }
    } else if (effectiveStatus(e) === "error") stats.errors++;
    tally(e.children, stats);
  }
  return stats;
}

const emptyStats = (): FolderStats => ({ same: 0, similar: 0, different: 0, leftOnly: 0, rightOnly: 0, errors: 0 });
