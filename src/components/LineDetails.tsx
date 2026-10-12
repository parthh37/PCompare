import { X } from "lucide-react";
import { useMemo } from "react";
import { alignChars, utf8Hex } from "../lib/charAlign";
import "./LineDetails.css";

interface Props {
  left: string | null;
  right: string | null;
  leftNo: number | null;
  rightNo: number | null;
  ignoreCase: boolean;
  onClose: () => void;
}

/** Columns past this are not drawn (very long minified lines). */
const MAX_COLUMNS = 3000;

const visible = (ch: string) => (ch === "\t" ? "→" : ch === " " ? "·" : ch);

/**
 * The Line Details pane: the current line pair aligned character by
 * character, as text and as UTF-8 hex bytes, with differing columns marked.
 */
export function LineDetails({ left, right, leftNo, rightNo, ignoreCase, onClose }: Props) {
  const cols = useMemo(() => alignChars(left ?? "", right ?? "", ignoreCase), [left, right, ignoreCase]);
  const shown = cols.length > MAX_COLUMNS ? cols.slice(0, MAX_COLUMNS) : cols;
  const differs = (l: string | null, r: string | null) =>
    l !== r && !(ignoreCase && l !== null && r !== null && l.toLowerCase() === r.toLowerCase());
  const diffCount = cols.reduce((n, c) => n + (differs(c.left, c.right) ? 1 : 0), 0);

  const charRow = (side: "left" | "right") => (
    <div className="ld-row">
      {shown.map((c, i) => {
        const ch = c[side];
        const cls = `ld-ch${ch === null ? " gap" : ""}${differs(c.left, c.right) ? " diff" : ""}${ch === " " || ch === "\t" ? " ws" : ""}`;
        return (
          <span key={i} className={cls}>
            {ch === null ? "" : visible(ch)}
          </span>
        );
      })}
    </div>
  );

  const hexRow = (side: "left" | "right") => (
    <div className="ld-row">
      {shown.map((c, i) => {
        const ch = c[side];
        const width = Math.max(c.left ? utf8Hex(c.left).length : 1, c.right ? utf8Hex(c.right).length : 1);
        return (
          <span
            key={i}
            className={`ld-hex${ch === null ? " gap" : ""}${differs(c.left, c.right) ? " diff" : ""}`}
            style={{ width: `${width * 3}ch` }}
          >
            {ch === null ? "" : utf8Hex(ch).join(" ")}
          </span>
        );
      })}
    </div>
  );

  const label = (side: "left" | "right", no: number | null, text: string | null) =>
    text === null ? `${side === "left" ? "Left" : "Right"}: no line` : `${side === "left" ? "Left" : "Right"} ${no}`;

  return (
    <div className="line-details">
      <div className="ld-head">
        <span>Line details</span>
        <span className="muted">
          {left === null && right === null
            ? "Select a line to see its details"
            : `${label("left", leftNo, left)} · ${label("right", rightNo, right)} · ${
                diffCount ? `${diffCount} differing column${diffCount === 1 ? "" : "s"}` : "identical"
              }${cols.length > MAX_COLUMNS ? ` · first ${MAX_COLUMNS.toLocaleString()} columns shown` : ""}`}
        </span>
        <span className="spacer" />
        <button className="icon-btn" onClick={onClose} title="Hide line details">
          <X size={14} />
        </button>
      </div>
      {(left !== null || right !== null) && (
        <div className="ld-body">
          <div className="ld-labels">
            <span>L</span>
            <span>R</span>
            <span className="ld-sep" />
            <span>L hex</span>
            <span>R hex</span>
          </div>
          <div className="ld-scroll">
            {charRow("left")}
            {charRow("right")}
            <div className="ld-sep" />
            {hexRow("left")}
            {hexRow("right")}
          </div>
        </div>
      )}
    </div>
  );
}
