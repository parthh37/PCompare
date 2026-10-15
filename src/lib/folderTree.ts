// Pure helpers over the folder-compare result tree: display filters, name
// search, flattening, "ignore" marks, and re-tallying after per-file content
// re-checks.
import type {
  ContentVerdict,
  EntryStatus,
  FolderEntry,
  FolderLevel,
  FolderStats,
  Side,
  StatusCounts,
  StatusDetail,
} from "./api";

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

/** Whether the filters leave the whole row out (shown only with Suppress filters). */
export const filteredOut = (e: FolderEntry) => e.filtered === "both";

/**
 * The tree without rows the filters leave out, as the view shows it unless
 * filters are suppressed. Shares untouched folders with the input.
 */
export function withoutFiltered(entries: FolderEntry[]): FolderEntry[] {
  let changed = false;
  const out: FolderEntry[] = [];
  for (const e of entries) {
    if (filteredOut(e)) {
      changed = true;
      continue;
    }
    const children = e.children.length ? withoutFiltered(e.children) : e.children;
    if (children !== e.children) changed = true;
    out.push(children === e.children ? e : { ...e, children });
  }
  return changed ? out : entries;
}

/** The status filters and totals go by: entries marked "ignore" count as same. */
export const effectiveStatus = (e: FolderEntry): EntryStatus => (e.ignored ? "same" : e.status);

// ---- Status detail and folder counts (mirrors pcompare-core's folder/status.rs) ----

export const STATUS_DETAILS: StatusDetail[] = [
  "unknown",
  "same",
  "sameDiff",
  "similar",
  "leftNewer",
  "rightNewer",
  "different",
  "leftOnly",
  "rightOnly",
  "typeMismatch",
  "error",
];

/** Details a folder's status treats as same (`StatusDetail::is_same`). */
const SAME_LIKE: ReadonlySet<StatusDetail> = new Set(["unknown", "same", "sameDiff"]);

/**
 * A row's {@link StatusDetail} as filters and colours use it: ignored rows
 * are same, streamed pairs unknown, and results saved before details
 * existed derive one from status and newer side.
 */
export function detailOf(e: FolderEntry): StatusDetail {
  if (e.ignored) return "same";
  if (e.pending) return "unknown";
  if (e.detail) return e.detail;
  if (e.status === "different" && !e.isDir && e.newer) return e.newer === "left" ? "leftNewer" : "rightNewer";
  return e.status;
}

/** A paired folder whose status comes from its counts. */
const isDerived = (e: FolderEntry) => e.isDir && !!e.left && !!e.right && !!e.counts;

/** What `e` adds to its parent's counts: its own detail (unless derived) and everything below it. */
export function contribution(e: FolderEntry): StatusCounts {
  // Rows the filters leave out count nowhere, like everything below them.
  if (filteredOut(e)) return {};
  const c: StatusCounts = { ...e.counts };
  if (!isDerived(e)) {
    const d = detailOf(e);
    c[d] = (c[d] ?? 0) + 1;
  }
  return c;
}

/** `a + b - minus`, dropping zeros like the engine's JSON does. */
function combine(a: StatusCounts, b: StatusCounts, minus: StatusCounts = {}): StatusCounts {
  const out: StatusCounts = {};
  for (const d of STATUS_DETAILS) {
    const n = Math.max(0, (a[d] ?? 0) - (minus[d] ?? 0)) + (b[d] ?? 0);
    if (n) out[d] = n;
  }
  return out;
}

/** The counts of a folder with these children. */
export const aggregate = (children: FolderEntry[]): StatusCounts =>
  children.reduce<StatusCounts>((c, e) => combine(c, contribution(e)), {});

/** The side every newer file below is on; null when none or both sides have some. */
export function countsNewer(c: StatusCounts | undefined): Side | null {
  const [l, r] = [(c?.leftNewer ?? 0) > 0, (c?.rightNewer ?? 0) > 0];
  return l && !r ? "left" : r && !l ? "right" : null;
}

/** The status of a paired folder with these counts (`StatusCounts::folder_status`). */
export function folderStatusOf(c: StatusCounts): EntryStatus {
  let same = 0;
  let total = 0;
  for (const d of STATUS_DETAILS) {
    total += c[d] ?? 0;
    if (SAME_LIKE.has(d)) same += c[d] ?? 0;
  }
  if (total === same) return "same";
  return total === same + (c.similar ?? 0) ? "similar" : "different";
}

/** Whether the row has `d`, or (a folder) holds a row that does. */
function hasDetail(e: FolderEntry, d: StatusDetail): boolean {
  return detailOf(e) === d || (e.isDir && !e.ignored && (e.counts?.[d] ?? 0) > 0);
}

/**
 * Mirrors `replace_entry` in pcompare-core: swaps the row at `relPath`
 * (with everything below it) for `next`, removes it (`null`), or inserts it
 * into its parent folder, and updates each folder above it from the change
 * alone: no rescan, no walk over siblings. Returns the new tree (unchanged
 * rows shared), or null when the path's parent folder isn't in it.
 */
export function replaceEntry(entries: FolderEntry[], relPath: string, next: FolderEntry | null): FolderEntry[] | null {
  type Change = { list: FolderEntry[]; before: StatusCounts; after: StatusCounts };
  const go = (list: FolderEntry[], listRel: string): Change | null => {
    const i = list.findIndex((e) => e.relPath === relPath);
    if (i >= 0) {
      const out = [...list];
      const before = contribution(list[i]);
      if (next) out[i] = next;
      else out.splice(i, 1);
      return { list: out, before, after: next ? contribution(next) : {} };
    }
    const j = list.findIndex((e) => e.isDir && relPath.startsWith(`${e.relPath}/`));
    if (j >= 0) {
      const dir = list[j];
      const inner = go(dir.children, dir.relPath);
      if (!inner) return null;
      const counts = dir.counts ? combine(dir.counts, inner.after, inner.before) : aggregate(inner.list);
      let moved: FolderEntry = { ...dir, children: inner.list, counts };
      if (isDerived(moved) && !moved.error) {
        const status = folderStatusOf(counts);
        moved = { ...moved, status, detail: status };
      }
      const out = [...list];
      out[j] = moved;
      return { list: out, before: contribution(dir), after: contribution(moved) };
    }
    const parent = relPath.includes("/") ? relPath.slice(0, relPath.lastIndexOf("/")) : "";
    if (!next || parent !== listRel) return null;
    // The engine's order: folders first, then by case-folded name.
    const key = (e: FolderEntry) => `${e.isDir ? 0 : 1}${e.name.toLowerCase()}`;
    const at = list.findIndex((e) => key(e) > key(next));
    const out = [...list];
    out.splice(at < 0 ? out.length : at, 0, next);
    return { list: out, before: {}, after: contribution(next) };
  };
  return go(entries, "")?.list ?? null;
}

/**
 * "Similar" files (only unimportant differences under rules-based compare)
 * and ignored ones count as same, so "Differences" shows exactly what needs
 * attention.
 */
export function matchesFilter(entry: FolderEntry, f: DisplayFilter): boolean {
  const e = { status: effectiveStatus(entry) };
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
    // Files by their own detail; folders when they hold such a file.
    case "leftNewer":
      return hasDetail(entry, "leftNewer");
    case "rightNewer":
      return hasDetail(entry, "rightNewer");
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
  const st = children.filter((c) => !filteredOut(c)).map(effectiveStatus);
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
        // Folders have counts only once their contents are known.
        const counts = e.counts && aggregate(children);
        next = { ...e, children, counts };
        const keep = !e.children.length || e.status === "leftOnly" || e.status === "rightOnly" || e.status === "error";
        if (!keep) {
          const status = counts ? folderStatusOf(counts) : folderStatus(children);
          next = { ...next, status, detail: e.detail && status };
        }
      } else {
        const v = verdicts.get(e.relPath);
        if (v) next = { ...e, status: v.status, crc: v.crc ?? e.crc, contentChecked: v.method, ...recheckedDetail(e, v.status) };
      }
      return ignore ? { ...next, ignored: true } : next;
    });
  return walk(entries, false);
}

/**
 * A re-checked file's detail and newer side, as the engine would set them
 * (`StatusDetail::for_file`): the engine only records a newer side when
 * the timestamps differ, and a `sameDiff` verdict means a quick test did.
 */
function recheckedDetail(e: FolderEntry, status: EntryStatus): Pick<FolderEntry, "detail" | "newer"> {
  const stampsMayDiffer = !!e.newer || e.detail === "sameDiff";
  const quickDiffers = stampsMayDiffer || e.left?.size !== e.right?.size;
  if (status === "same") return { detail: quickDiffers ? "sameDiff" : "same", newer: null };
  const [l, r] = [e.left?.modifiedMs, e.right?.modifiedMs];
  const newer = e.newer ?? (stampsMayDiffer && l != null && r != null && l !== r ? (l > r ? "left" : "right") : null);
  if (status === "different" && newer) return { detail: newer === "left" ? "leftNewer" : "rightNewer", newer };
  return { detail: status, newer };
}

/** Mirrors `tally` in pcompare-core so stats stay right after re-checks and ignore marks. */
export function tally(entries: FolderEntry[], stats: FolderStats = emptyStats()): FolderStats {
  countFiles(entries, stats);
  stats.counts = aggregate(entries);
  return stats;
}

function countFiles(entries: FolderEntry[], stats: FolderStats) {
  for (const e of entries) {
    if (e.pending && !e.isDir) continue; // not compared yet
    if (filteredOut(e)) {
      // Counted apart, like everything below it.
      if (!e.isDir) stats.filtered = (stats.filtered ?? 0) + 1;
    } else if (!e.isDir) {
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
    countFiles(e.children, stats);
  }
}

const emptyStats = (): FolderStats => ({ same: 0, similar: 0, different: 0, leftOnly: 0, rightOnly: 0, errors: 0 });

// ---- "Select…": rows matching criteria (Araxis Merge's Select Files) ----

/** Status buckets the Select dialog offers; differing pairs split by which side is newer. */
export type SelectStatus =
  | "diffLeftNewer"
  | "diffRightNewer"
  | "diffOther"
  | "similar"
  | "same"
  | "leftOnly"
  | "rightOnly"
  | "error";

export interface SelectCriteria {
  statuses: SelectStatus[];
  /** `*.cs;*.xml` (`;` or `,` separated). Matched against the name, or the relative path for patterns with `/`. Empty = any. */
  include: string;
  exclude: string;
  files: boolean;
  folders: boolean;
  /** Size and date ranges apply to files; a pair matches if either side is in range. */
  minSizeKb: number | null;
  maxSizeKb: number | null;
  /** `yyyy-mm-dd`, or "". */
  modifiedAfter: string;
  modifiedBefore: string;
}

export const ALL_SELECT_STATUSES: SelectStatus[] = [
  "diffLeftNewer",
  "diffRightNewer",
  "diffOther",
  "similar",
  "same",
  "leftOnly",
  "rightOnly",
  "error",
];

export function selectStatusOf(e: FolderEntry): SelectStatus {
  switch (effectiveStatus(e)) {
    case "same":
      return "same";
    case "similar":
      return "similar";
    case "leftOnly":
      return "leftOnly";
    case "rightOnly":
      return "rightOnly";
    case "error":
      return "error";
    default:
      return e.newer === "left" ? "diffLeftNewer" : e.newer === "right" ? "diffRightNewer" : "diffOther";
  }
}

/** `*.cs;*.xml` → one predicate over (name, relPath); null when the list is empty. */
export function wildcardList(text: string): ((name: string, relPath: string) => boolean) | null {
  const parts = text
    .split(/[;,]/)
    .map((p) => p.trim())
    .filter(Boolean);
  if (!parts.length) return null;
  const tests = parts.map((p) => {
    const re = new RegExp(
      "^" +
        p
          .split("")
          .map((c) => (c === "*" ? ".*" : c === "?" ? "." : c.replace(/[.+^${}()|[\]\\]/g, "\\$&")))
          .join("") +
        "$",
      "i",
    );
    const onPath = p.includes("/");
    return (name: string, rel: string) => re.test(onPath ? rel : name);
  });
  return (name, rel) => tests.some((t) => t(name, rel));
}

const dayMs = (value: string, endOfDay: boolean): number | null => {
  if (!value) return null;
  const t = new Date(`${value}T00:00:00`).getTime();
  return Number.isNaN(t) ? null : t + (endOfDay ? 86_400_000 - 1 : 0);
};

/** A predicate for one entry, compiled once per criteria. */
export function selectMatcher(c: SelectCriteria): (e: FolderEntry) => boolean {
  const statuses = new Set(c.statuses);
  const include = wildcardList(c.include);
  const exclude = wildcardList(c.exclude);
  const [min, max] = [c.minSizeKb, c.maxSizeKb].map((v) => (v == null ? null : v * 1024));
  const [after, before] = [dayMs(c.modifiedAfter, false), dayMs(c.modifiedBefore, true)];
  const inRange = (m: FolderEntry["left"]) =>
    !!m &&
    (min == null || m.size >= min) &&
    (max == null || m.size <= max) &&
    (after == null || m.modifiedMs == null || m.modifiedMs >= after) &&
    (before == null || m.modifiedMs == null || m.modifiedMs <= before);
  return (e) => {
    if (e.isDir ? !c.folders : !c.files) return false;
    if (!statuses.has(selectStatusOf(e))) return false;
    if (include && !include(e.name, e.relPath)) return false;
    if (exclude && exclude(e.name, e.relPath)) return false;
    return e.isDir || inRange(e.left) || inRange(e.right);
  };
}

/**
 * The rows to select for `criteria`, in display order, among entries the
 * display filter shows (`visible`). Matches inside collapsed folders are
 * included when `reachCollapsed` is on, with the folders to expand to show
 * them; flattened views list files only.
 */
export function selectMatching(
  entries: FolderEntry[],
  visible: ReadonlySet<string>,
  criteria: SelectCriteria,
  view: { flatten: boolean; expanded: ReadonlySet<string>; reachCollapsed: boolean },
): { rows: string[]; expand: string[] } {
  const match = selectMatcher(criteria);
  const rows: string[] = [];
  const expand = new Set<string>();
  const walk = (list: FolderEntry[], closed: string[]) => {
    for (const e of list) {
      if (!visible.has(e.relPath)) continue;
      // Flattened views have no folder rows.
      if ((!view.flatten || !e.isDir) && match(e)) {
        rows.push(e.relPath);
        if (!view.flatten) closed.forEach((d) => expand.add(d));
      }
      if (!e.isDir) continue;
      const open = view.flatten || view.expanded.has(e.relPath);
      if (open || view.reachCollapsed) walk(e.children, open ? closed : [...closed, e.relPath]);
    }
  };
  walk(entries, []);
  return { rows, expand: [...expand] };
}

// ---- Live trees: rows streamed folder by folder while a compare runs ----

/**
 * Assembles the tree from streamed {@link FolderLevel}s. Each folder arrives
 * twice: listed (two-sided rows not compared yet, shown as pending) and then
 * complete, which replaces its rows. A tree carried over from an earlier pass
 * keeps its settled rows until the new pass completes their folder, so a
 * refresh refines the view in place instead of blanking it.
 */
export class LiveTree {
  /** Rows per folder relPath ("" is the root). */
  private levels = new Map<string, FolderEntry[]>();
  /** Folders reported by the current pass. */
  private seen = new Set<string>();
  private settled = new Set<string>();

  /** Starts a new pass over the same folders. */
  beginPass() {
    this.seen.clear();
  }

  apply(levels: FolderLevel[]) {
    for (const { rel, entries, complete } of levels) {
      this.seen.add(rel);
      if (complete) {
        this.levels.set(rel, entries);
        this.settled.add(rel);
      } else if (!this.settled.has(rel)) {
        // One-sided rows are already what they'll be; pairs (files and folders) await a verdict.
        const pending = (e: FolderEntry) =>
          e.left && e.right && e.status !== "typeMismatch" ? { ...e, pending: true } : e;
        this.levels.set(rel, entries.map(pending));
      }
    }
  }

  /** Drops folders the finished pass no longer has (deleted, filtered out, now unreadable). */
  endPass() {
    for (const rel of this.levels.keys()) {
      if (!this.seen.has(rel)) {
        this.levels.delete(rel);
        this.settled.delete(rel);
      }
    }
  }

  /**
   * Takes `tree`, which {@link replaceEntry} made by swapping the row at
   * `relPath`, as settled: the folders on the way down to it get their new
   * rows, and its own folders replace whatever was kept below it.
   */
  patch(tree: FolderEntry[], relPath: string) {
    for (const rel of [...this.levels.keys()]) {
      if (rel === relPath || rel.startsWith(`${relPath}/`)) {
        this.levels.delete(rel);
        this.settled.delete(rel);
      }
    }
    const keep = (rel: string, rows: FolderEntry[]) => {
      this.levels.set(rel, rows);
      this.settled.add(rel);
    };
    const below = (e: FolderEntry) => {
      if (!e.isDir) return;
      keep(e.relPath, e.children);
      e.children.forEach(below);
    };
    let rows = tree;
    let rel = "";
    for (;;) {
      keep(rel, rows);
      const next = rows.find((e) => e.isDir && (relPath === e.relPath || relPath.startsWith(`${e.relPath}/`)));
      if (!next) return;
      if (next.relPath === relPath) return below(next);
      [rows, rel] = [next.children, next.relPath];
    }
  }

  /** The tree so far. Rows are shared with the previous call, children re-linked. */
  tree(): FolderEntry[] {
    const link = (rel: string): FolderEntry[] => {
      const rows = this.levels.get(rel) ?? [];
      for (const e of rows) {
        // An "<unreadable>" row stands in for its own folder (same relPath): no recursion.
        if (e.isDir && e.relPath !== rel) e.children = link(e.relPath);
      }
      return rows;
    };
    return [...link("")];
  }
}
