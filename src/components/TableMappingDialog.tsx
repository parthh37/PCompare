import { ArrowLeftRight, ChevronDown, ChevronUp, Plus, X } from "lucide-react";
import { useMemo, useState } from "react";
import type { ColumnInfo, ColumnPair } from "../lib/tableApi";
import { Modal } from "./Modal";

/** Spreadsheet-style column letters: 0 → A, 26 → AA. */
export function columnLetter(i: number): string {
  let s = "";
  for (let n = i + 1; n > 0; n = Math.floor((n - 1) / 26)) s = String.fromCharCode(65 + ((n - 1) % 26)) + s;
  return s;
}

/** Source columns of one side as `[index, label]`, read off the result columns. */
function sourceColumns(columns: ColumnInfo[], side: "left" | "right"): [number, string][] {
  const out: [number, string][] = [];
  for (const c of columns) {
    const i = side === "left" ? c.left : c.right;
    const name = side === "left" ? c.leftName : c.rightName;
    if (i !== null) out.push([i, name ? `${columnLetter(i)}: ${name}` : columnLetter(i)]);
  }
  return out.sort((a, b) => a[0] - b[0]);
}

/**
 * The column layout the engine builds from a manual mapping (mirrors
 * `mapped_pairs` in pcompare-table): out-of-range / reused columns drop out,
 * empty pairs vanish, and unmentioned columns are appended unmatched.
 */
export function mappedLayout(leftWidth: number, rightWidth: number, mapping: ColumnPair[]): ColumnPair[] {
  const lused = new Array<boolean>(leftWidth).fill(false);
  const rused = new Array<boolean>(rightWidth).fill(false);
  const take = (i: number | null, used: boolean[]) => {
    if (i === null || i < 0 || i >= used.length || used[i]) return null;
    used[i] = true;
    return i;
  };
  const out: ColumnPair[] = [];
  for (const p of mapping) {
    const pair = { left: take(p.left, lused), right: take(p.right, rused) };
    if (pair.left !== null || pair.right !== null) out.push(pair);
  }
  lused.forEach((u, i) => u || out.push({ left: i, right: null }));
  rused.forEach((u, i) => u || out.push({ left: null, right: i }));
  return out;
}

/** Number of source columns per side (one past the highest index seen). */
export function sideWidths(columns: ColumnInfo[]): [number, number] {
  let l = 0;
  let r = 0;
  for (const c of columns) {
    if (c.left !== null) l = Math.max(l, c.left + 1);
    if (c.right !== null) r = Math.max(r, c.right + 1);
  }
  return [l, r];
}

interface Props {
  columns: ColumnInfo[];
  /** Whether a manual mapping is in force (vs. automatic pairing). */
  manual: boolean;
  leftHeader: boolean;
  rightHeader: boolean;
  onHeadersChange: (left: boolean, right: boolean) => void;
  /** `null` returns to automatic pairing by header name / position. */
  onApply: (mapping: ColumnPair[] | null) => void;
  onClose: () => void;
}

/** Column mapping: pair any left column with any right one, or leave it unmatched. */
export function TableMappingDialog({
  columns,
  manual,
  leftHeader,
  rightHeader,
  onHeadersChange,
  onApply,
  onClose,
}: Props) {
  const [pairs, setPairs] = useState<ColumnPair[]>(() => columns.map((c) => ({ left: c.left, right: c.right })));
  const leftCols = useMemo(() => sourceColumns(columns, "left"), [columns]);
  const rightCols = useMemo(() => sourceColumns(columns, "right"), [columns]);
  const [lw, rw] = sideWidths(columns);

  // Picking a column already used in another row swaps the two rows' choices,
  // so every source column stays in exactly one place.
  const choose = (row: number, side: "left" | "right", value: number | null) =>
    setPairs((ps) =>
      ps.map((p, i) => {
        if (i === row) return { ...p, [side]: value };
        if (value !== null && p[side] === value) return { ...p, [side]: ps[row][side] };
        return p;
      }),
    );

  const move = (row: number, by: number) =>
    setPairs((ps) => {
      const to = row + by;
      if (to < 0 || to >= ps.length) return ps;
      const next = ps.slice();
      [next[row], next[to]] = [next[to], next[row]];
      return next;
    });

  const byPosition = () =>
    setPairs(
      Array.from({ length: Math.max(lw, rw) }, (_, i) => ({ left: i < lw ? i : null, right: i < rw ? i : null })),
    );

  const select = (row: number, side: "left" | "right", options: [number, string][]) => (
    <select
      value={pairs[row][side] ?? ""}
      onChange={(e) => choose(row, side, e.target.value === "" ? null : Number(e.target.value))}
      aria-label={`${side === "left" ? "Left" : "Right"} column`}
    >
      <option value="">(unmatched)</option>
      {options.map(([i, label]) => (
        <option key={i} value={i}>
          {label}
        </option>
      ))}
    </select>
  );

  return (
    <Modal
      title="Column mapping"
      wide
      onClose={onClose}
      footer={
        <>
          <button
            onClick={() => onApply(null)}
            title="Pair columns by header name (or by position when names don't match)"
            disabled={!manual}
          >
            Automatic
          </button>
          <button onClick={byPosition} title="Pair column A with A, B with B, …">
            By position
          </button>
          <span className="spacer" />
          <button onClick={onClose}>Cancel</button>
          <button className="primary" onClick={() => onApply(mappedLayout(lw, rw, pairs))}>
            Apply
          </button>
        </>
      }
    >
      <p className="muted tbl-columns-hint">
        {manual ? "Columns are paired manually." : "Columns are currently paired automatically."} Choose which left
        column lines up with which right column, reorder rows to rearrange the display, or leave a side unmatched.
        Columns not listed are added at the end, unmatched.
      </p>
      <div className="tbl-map-headers">
        <label className="check">
          <input
            type="checkbox"
            checked={leftHeader}
            onChange={(e) => onHeadersChange(e.target.checked, rightHeader)}
          />
          Left: first row is header
        </label>
        <label className="check">
          <input
            type="checkbox"
            checked={rightHeader}
            onChange={(e) => onHeadersChange(leftHeader, e.target.checked)}
          />
          Right: first row is header
        </label>
      </div>
      <div className="tbl-columns tbl-map">
        <div className="tbl-map-row tbl-columns-head">
          <span>#</span>
          <span>Left column</span>
          <span />
          <span>Right column</span>
          <span />
        </div>
        {pairs.map((p, i) => (
          <div key={i} className={`tbl-map-row${p.left === null || p.right === null ? " unmatched" : ""}`}>
            <span className="muted">{i + 1}</span>
            {select(i, "left", leftCols)}
            <ArrowLeftRight size={13} className="muted" />
            {select(i, "right", rightCols)}
            <span className="tbl-map-actions">
              <button className="icon-btn" onClick={() => move(i, -1)} disabled={i === 0} title="Move up">
                <ChevronUp size={13} />
              </button>
              <button
                className="icon-btn"
                onClick={() => move(i, 1)}
                disabled={i === pairs.length - 1}
                title="Move down"
              >
                <ChevronDown size={13} />
              </button>
              <button
                className="icon-btn"
                onClick={() => setPairs((ps) => ps.filter((_, j) => j !== i))}
                title="Remove row"
              >
                <X size={13} />
              </button>
            </span>
          </div>
        ))}
      </div>
      <button className="tbl-map-add" onClick={() => setPairs((ps) => [...ps, { left: null, right: null }])}>
        <Plus size={13} /> Add row
      </button>
    </Modal>
  );
}
