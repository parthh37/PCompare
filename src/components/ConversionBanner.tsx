import { FileCog } from "lucide-react";
import type { Conversion } from "../lib/convertApi";
import "./ConversionBanner.css";

type Side = "left" | "right";

/** One side's note: "Left: Converted from PDF — read-only". */
function note(side: Side, c: Conversion | null | undefined, both: boolean): string | null {
  if (!c || (!c.convertedFrom && !c.readOnly)) return null;
  const who = both ? "" : side === "left" ? "Left: " : "Right: ";
  const what = c.convertedFrom ? `Converted from ${c.convertedFrom}` : `“${c.rule}” file format`;
  return `${who}${what}${c.readOnly ? " — read-only" : ""}`;
}

/**
 * Text Compare banner for sides a file format rule converted to text (or
 * made read-only), with shortcuts to the original bytes and the File
 * formats settings.
 */
export function ConversionBanner({
  conversions,
  onConfigure,
  onOpenHex,
}: {
  conversions: Record<Side, Conversion | null | undefined>;
  onConfigure: () => void;
  /** Compares the original files instead (Hex Compare). */
  onOpenHex?: () => void;
}) {
  const l = conversions.left;
  const r = conversions.right;
  // The same effect on both sides reads as one note.
  const same = !!l && !!r && l.convertedFrom === r.convertedFrom && l.readOnly === r.readOnly && l.rule === r.rule;
  const notes = same ? [note("left", l, true)] : [note("left", l, false), note("right", r, false)];
  const shown = notes.filter(Boolean);
  if (!shown.length) return null;
  return (
    <div className="banner info conversion-banner">
      <FileCog size={14} />
      <span>{shown.join(" · ")}</span>
      {onOpenHex && (l?.convertedFrom || r?.convertedFrom) && (
        <button className="link" onClick={onOpenHex} title="Compare the original files byte by byte">
          Open in Hex
        </button>
      )}
      <button className="link" onClick={onConfigure} title="File format rules: converters, read-only, grammar">
        File formats…
      </button>
    </div>
  );
}
