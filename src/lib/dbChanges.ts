// The changes a user lines up in Database Compare with the ← / → arrows, and
// their plain-English descriptions for the review dialog. SQL is generated
// and run only when the user applies the plan (DbReviewDialog).
import { objectName, type Cell, type ObjectDiff, type ObjectKind, type RowStatus, type SyncDirection, type TableDataSummary } from "./dbApi";

export type Side = "left" | "right";
/** `toRight` = copy from the left database into the right one. */
export type MoveDir = "toRight" | "toLeft";

export const syncDirection = (d: MoveDir): SyncDirection => (d === "toRight" ? "leftToRight" : "rightToLeft");
export const targetSide = (d: MoveDir): Side => (d === "toRight" ? "right" : "left");
export const sourceSide = (d: MoveDir): Side => (d === "toRight" ? "left" : "right");

export interface RowPick {
  dir: MoveDir;
  key: Cell[];
  status: RowStatus;
}

export interface ChangePlan {
  /** Schema object id → direction. */
  objects: Map<string, MoveDir>;
  /** Table id → direction for every differing row of the table. */
  tables: Map<string, MoveDir>;
  /** Table id → row key id → pick, for tables not picked as a whole. */
  rows: Map<string, Map<string, RowPick>>;
}

export const emptyPlan = (): ChangePlan => ({ objects: new Map(), tables: new Map(), rows: new Map() });

export const rowKeyId = (key: Cell[]) => JSON.stringify(key);

export function planSize(p: ChangePlan): Record<MoveDir, number> {
  const n = { toRight: 0, toLeft: 0 };
  p.objects.forEach((d) => n[d]++);
  p.tables.forEach((d) => n[d]++);
  p.rows.forEach((rows) => rows.forEach((r) => n[r.dir]++));
  return n;
}

/** Sets (or with `undefined` clears) the direction of every id in `ids`. */
export function pickAll(map: Map<string, MoveDir>, ids: string[], dir: MoveDir | undefined): Map<string, MoveDir> {
  const next = new Map(map);
  ids.forEach((id) => (dir ? next.set(id, dir) : next.delete(id)));
  return next;
}

/** The shared direction of `ids`, or `undefined` when they differ or aren't all picked. */
export function commonDir(map: Map<string, MoveDir>, ids: string[]): MoveDir | undefined {
  const first = ids.length ? map.get(ids[0]) : undefined;
  return first && ids.every((id) => map.get(id) === first) ? first : undefined;
}

// ---------- plain-English descriptions ----------

export type Tone = "add" | "change" | "remove";

export interface PlainChange {
  /** Identifies the pick so the review dialog can drop it. */
  ref: { kind: "object"; id: string } | { kind: "table"; id: string } | { kind: "rows"; id: string; dir: MoveDir };
  target: Side;
  tone: Tone;
  title: string;
  details: string[];
}

const KIND_WORD: Record<ObjectKind, string> = {
  table: "table",
  view: "view",
  procedure: "stored procedure",
  function: "function",
  trigger: "trigger",
  sequence: "sequence",
};

const plural = (n: number, one: string, many = `${one}s`) => `${n.toLocaleString()} ${n === 1 ? one : many}`;

/** Arrow tooltips for an object, in the words the review dialog uses. */
export function objectHints(o: ObjectDiff, labels: Record<Side, string>): Record<MoveDir, string> {
  const what = KIND_WORD[o.kind];
  if (o.status === "leftOnly") return { toRight: `Add this ${what} to ${labels.right}`, toLeft: `Delete this ${what} from ${labels.left}` };
  if (o.status === "rightOnly") return { toRight: `Delete this ${what} from ${labels.right}`, toLeft: `Add this ${what} to ${labels.left}` };
  return { toRight: `Make ${labels.right} match ${labels.left}`, toLeft: `Make ${labels.left} match ${labels.right}` };
}

/**
 * Rewrites one schema-diff line ("Column Email: NULL → NOT NULL", where the
 * left value comes first) as what will happen to the target.
 */
export function explainChangeLine(line: string, dir: MoveDir, source: string): string {
  if (line === "Definition differs") return `Its code is replaced with the version from ${source}`;
  if (line === "Column order differs") return `Columns are reordered to match ${source}`;
  const colon = line.indexOf(": ");
  if (colon < 0) return line;
  const subject = line.slice(0, colon);
  const what = line.slice(colon + 2);
  // "only in left" means the source has it when copying to the right.
  if (what === "only in left" || what === "only in right") {
    const inSource = (what === "only in left") === (dir === "toRight");
    return `${subject} will be ${inSource ? "added" : "removed"}`;
  }
  const arrow = what.split(" → ");
  if (arrow.length !== 2) return line;
  let [leftVal, rightVal] = arrow;
  // A leading facet word ("default 0 → 1") names what changes.
  const facet = /^(default|computed|collation|method|filter|include|columns|references) (.*)$/.exec(leftVal);
  const label = facet ? `${subject} ${facet[1]}` : subject;
  if (facet) leftVal = facet[2];
  const [from, to] = dir === "toRight" ? [rightVal, leftVal] : [leftVal, rightVal];
  return `${label} changes from ${from} to ${to}`;
}

export function describeObject(o: ObjectDiff, dir: MoveDir, labels: Record<Side, string>): PlainChange {
  const src = labels[sourceSide(dir)];
  const what = KIND_WORD[o.kind];
  const name = objectName(o.name);
  const base = { ref: { kind: "object" as const, id: o.id }, target: targetSide(dir) };
  const onlyInSource = (o.status === "leftOnly") === (dir === "toRight") && o.status !== "different";
  if (o.status !== "different" && onlyInSource) {
    return { ...base, tone: "add", title: `Add the ${what} ${name}`, details: [`Copied from ${src}.`] };
  }
  if (o.status !== "different") {
    const details = [`It doesn't exist in ${src}.`];
    if (o.kind === "table") details.push("Every row stored in this table is deleted with it.");
    return { ...base, tone: "remove", title: `Delete the ${what} ${name}`, details };
  }
  const details = o.changes.length
    ? o.changes.map((c) => explainChangeLine(c, dir, src))
    : [`Its definition is replaced with the one from ${src}.`];
  return { ...base, tone: "change", title: `Update the ${what} ${name} to match ${src}`, details };
}

/** Row counts a direction would add / update / delete in the target. */
function rowWork(dir: MoveDir, counts: { different: number; leftOnly: number; rightOnly: number }) {
  return {
    add: dir === "toRight" ? counts.leftOnly : counts.rightOnly,
    update: counts.different,
    remove: dir === "toRight" ? counts.rightOnly : counts.leftOnly,
  };
}

function rowDetails(work: ReturnType<typeof rowWork>, src: string): string[] {
  return [
    work.add ? `Add ${plural(work.add, "row")} that only ${src} has` : "",
    work.update ? `Update ${plural(work.update, "row")} to ${src}’s values` : "",
    work.remove ? `Delete ${plural(work.remove, "row")} that ${src} doesn’t have` : "",
  ].filter(Boolean);
}

export function tableHints(labels: Record<Side, string>): Record<MoveDir, string> {
  return {
    toRight: `Make the rows in ${labels.right} match ${labels.left}`,
    toLeft: `Make the rows in ${labels.left} match ${labels.right}`,
  };
}

export function describeTable(t: TableDataSummary, dir: MoveDir, labels: Record<Side, string>): PlainChange {
  const work = rowWork(dir, t);
  return {
    ref: { kind: "table", id: t.id },
    target: targetSide(dir),
    tone: work.remove ? "remove" : work.add && !work.update ? "add" : "change",
    title: `Copy the data of ${objectName(t.left ?? t.right)} from ${labels[sourceSide(dir)]}`,
    details: rowDetails(work, labels[sourceSide(dir)]),
  };
}

/** One entry per direction used among a table's picked rows. */
export function describeRows(t: TableDataSummary, picks: RowPick[], labels: Record<Side, string>): PlainChange[] {
  return (["toRight", "toLeft"] as MoveDir[]).flatMap((dir) => {
    const mine = picks.filter((p) => p.dir === dir);
    if (!mine.length) return [];
    const count = (s: RowStatus) => mine.filter((p) => p.status === s).length;
    const work = rowWork(dir, { different: count("different"), leftOnly: count("leftOnly"), rightOnly: count("rightOnly") });
    return [
      {
        ref: { kind: "rows", id: t.id, dir },
        target: targetSide(dir),
        tone: work.remove ? "remove" : work.add && !work.update ? "add" : "change",
        title: `Copy ${plural(mine.length, "selected row")} of ${objectName(t.left ?? t.right)} from ${labels[sourceSide(dir)]}`,
        details: rowDetails(work, labels[sourceSide(dir)]),
      } satisfies PlainChange,
    ];
  });
}

export function rowHints(status: RowStatus, labels: Record<Side, string>): Record<MoveDir, string> {
  if (status === "leftOnly") return { toRight: `Add this row to ${labels.right}`, toLeft: `Delete this row from ${labels.left}` };
  if (status === "rightOnly") return { toRight: `Delete this row from ${labels.right}`, toLeft: `Add this row to ${labels.left}` };
  return { toRight: `Copy ${labels.left}’s values to ${labels.right}`, toLeft: `Copy ${labels.right}’s values to ${labels.left}` };
}

/** A plain-English reason for a failed apply, or `null` when the raw error is all we know. */
export function explainApplyError(raw: string): string | null {
  if (/foreign key|FOREIGN KEY|REFERENCE constraint|violates foreign/i.test(raw))
    return "A row being copied refers to a row the other database doesn't have (or a deleted row is still referred to). Copy the related rows too, or copy the whole table.";
  if (/unique|duplicate|PRIMARY KEY constraint/i.test(raw)) return "A row with the same key already exists there.";
  if (/permission|denied|not authorized|insufficient privilege/i.test(raw))
    return "The saved login isn't allowed to make this change on that database.";
  if (/locked|busy|deadlock|timeout/i.test(raw)) return "The database was busy or locked. Try again in a moment.";
  return null;
}
