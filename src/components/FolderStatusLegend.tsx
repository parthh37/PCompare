// Folder-compare status glyphs: per-detail icons for files, each side's
// folder icon coloured by what lies below it there (see folderColors.ts), the
// colour spot before file names, and the legend popovers (Folder Compare's
// View > Legend, Folder Merge's toolbar Legend).
import {
  ArrowLeft,
  ArrowRight,
  CircleDashed,
  Clock,
  Equal,
  EqualApproximately,
  EqualNot,
  EyeOff,
  Minus,
  TriangleAlert,
  type LucideIcon,
} from "lucide-react";
import { useEffect, useId, useLayoutEffect, useRef, type ReactNode } from "react";
import type { FolderEntry, StatusDetail } from "../lib/api";
import { isMergeTone, toneColor, type AnyTone, type FolderLook } from "../lib/folderColors";
import { countsNewer, detailOf } from "../lib/folderTree";
import { FileIcon } from "./icons";
// The glyphs' own styles (.fc-folder, .fc-spot), for every view that borrows them.
import "./FolderCompare.css";

export const DETAIL_ICON: Record<StatusDetail, LucideIcon> = {
  unknown: CircleDashed,
  same: Equal,
  sameDiff: Clock,
  similar: EqualApproximately,
  leftNewer: EqualNot,
  rightNewer: EqualNot,
  different: EqualNot,
  leftOnly: ArrowLeft,
  rightOnly: ArrowRight,
  typeMismatch: TriangleAlert,
  error: TriangleAlert,
};

export const DETAIL_LABEL: Record<StatusDetail, string> = {
  unknown: "Not compared yet",
  same: "Same",
  sameDiff: "Same contents, but size or timestamp differ",
  similar: "Similar — only unimportant differences",
  leftNewer: "Different — left is newer",
  rightNewer: "Different — right is newer",
  different: "Different",
  leftOnly: "Left only",
  rightOnly: "Right only",
  typeMismatch: "File on one side, folder on the other",
  error: "Error",
};

/** "3 right newer · 2 left only" for a folder's tooltip. */
export function countsSummary(e: FolderEntry): string {
  if (!e.counts) return "";
  const parts = (Object.entries(e.counts) as [StatusDetail, number][])
    .filter(([d, n]) => n > 0 && d !== "same")
    .map(
      ([d, n]) =>
        `${n} ${DETAIL_LABEL[d].replace(/^Different — /, "").toLowerCase()}`,
    );
  const newer = countsNewer(e.counts);
  if (newer) parts.push(`newer files only on the ${newer}`);
  return parts.join(" · ");
}

const FOLDER_BACK = "M1.5 3.5a1 1 0 0 1 1-1h3.7l1.5 1.6h5.8a1 1 0 0 1 1 1v8.4H1.5z";
const FOLDER_FRONT = "M1.5 5.8h13v7.7h-13z";
const FOLDER_FRONT_OPEN = "M3.4 6.6h11.4l-1.9 6.9H1.4z";

/**
 * One side's folder icon: filled with its tone, or split corner to corner
 * when it shows two (the first top-left). Hollow while the folder is still
 * being compared, a plain folder when it was never opened.
 */
export function FolderToneIcon({ look, open, size = 16 }: { look: FolderLook; open?: boolean; size?: number }) {
  const id = useId();
  if (look.unopened) return <FileIcon name="" isDir open={open} size={size} />;
  const [a, b = a] = look.tones;
  const fill = look.hollow ? "none" : a === b ? toneColor(a) : `url(#${id}-split)`;
  const outline = look.hollow ? "var(--muted)" : "rgba(0, 0, 0, 0.5)";
  return (
    <svg className="fc-folder" width={size} height={size} viewBox="0 0 16 16" aria-hidden>
      <defs>
        {/* objectBoundingBox: the split runs from each shape's bottom-left to its top-right corner. */}
        <linearGradient id={`${id}-split`} x1="0" y1="0" x2="1" y2="1">
          <stop offset="0.5" style={{ stopColor: toneColor(a) }} />
          <stop offset="0.5" style={{ stopColor: toneColor(b) }} />
        </linearGradient>
        <linearGradient id={`${id}-sheen`} x1="0" y1="0" x2="0" y2="1">
          <stop offset="0" stopColor="#fff" stopOpacity="0.4" />
          <stop offset="1" stopColor="#fff" stopOpacity="0" />
        </linearGradient>
      </defs>
      <path d={FOLDER_BACK} fill={fill} stroke={outline} strokeWidth="0.8" strokeLinejoin="round" />
      {!look.hollow && <path d={FOLDER_BACK} fill="rgba(0, 0, 0, 0.22)" />}
      <path d={open ? FOLDER_FRONT_OPEN : FOLDER_FRONT} fill={fill} stroke={outline} strokeWidth="0.8" strokeLinejoin="round" />
      {!look.hollow && <path d={open ? FOLDER_FRONT_OPEN : FOLDER_FRONT} fill={`url(#${id}-sheen)`} />}
      {look.error && (
        <g>
          <circle cx="12.3" cy="12.3" r="3.4" fill="var(--warn)" stroke="var(--panel)" strokeWidth="0.8" />
          <path d="M10.9 10.9l2.8 2.8M13.7 10.9l-2.8 2.8" stroke="#fff" strokeWidth="1.1" strokeLinecap="round" />
        </g>
      )}
    </svg>
  );
}

/** The colour spot before a file name: keeps the row's cue visible under a selection. */
export const ToneSpot = ({ tone }: { tone: AnyTone }) => (
  <span className="fc-spot" style={{ background: toneColor(tone) }} aria-hidden />
);

/** The status column's glyph for a row: files only (each side's folder icon carries a folder's state). */
export function StatusGlyph({ entry }: { entry: FolderEntry }) {
  if (entry.ignored) return <EyeOff size={13} />;
  if (entry.isDir && entry.left?.isDir && entry.right?.isDir && detailOf(entry) !== "error") return null;
  const Icon = DETAIL_ICON[detailOf(entry)];
  return <Icon size={13} />;
}

/** A legend's file rows (a colour and what it means) and folder rows (an icon and what it holds). */
type FileRows = [AnyTone, string][];
type FolderRows = [FolderLook, string][];

const FILE_ROWS: FileRows = [
  ["same", "Same"],
  ["newer", "Newer than the other side"],
  ["different", "Different, neither side newer"],
  ["older", "Older than the other side"],
  ["orphan", "Orphan: only on this side"],
  ["unknown", "Not compared yet"],
  ["error", "Error"],
];

const FOLDER_ROWS: FolderRows = [
  [{ tones: ["newer"] }, "Holds newer or different files"],
  [{ tones: ["orphan"] }, "Holds orphans, or is one"],
  [{ tones: ["older"] }, "Holds only older files"],
  [{ tones: ["newer", "orphan"] }, "Newer files and orphans"],
  [{ tones: ["newer", "older"] }, "Newer and older files"],
  [{ tones: ["orphan", "older"] }, "Orphans and older files"],
  [{ tones: ["same"] }, "Everything inside is the same"],
  [{ tones: ["unknown"], hollow: true }, "Still being compared"],
  [{ tones: ["unknown"], unopened: true }, "Not opened yet"],
];

// Folder Merge, three-way: each side by what its copy is in the merge (mergeSideTone).
const MERGE_FILE_ROWS: FileRows = [
  ["same", "Unchanged"],
  ["left", "Changed on the left, or the same on both"],
  ["right", "Changed on the right"],
  ["mergeable", "Changed on both sides, merges cleanly"],
  ["conflict", "Conflict: changed differently on both sides"],
  ["older", "Older: the copy a change replaces"],
  ["error", "Error"],
];

const MERGE_FOLDER_ROWS: FolderRows = [
  [{ tones: ["conflict"] }, "Holds conflicts"],
  [{ tones: ["conflict", "mergeable"] }, "Conflicts and clean merges"],
  [{ tones: ["mergeable"] }, "Changes that merge cleanly"],
  [{ tones: ["left"] }, "Holds left changes"],
  [{ tones: ["left", "older"] }, "Left changes and older copies"],
  [{ tones: ["right"] }, "Holds right changes"],
  [{ tones: ["right", "older"] }, "Right changes and older copies"],
  [{ tones: ["older"] }, "Holds only older copies"],
  [{ tones: ["same"] }, "Nothing inside changed"],
];

// Folder Merge, two-way: no base, so it reads like Folder Compare, with conflicts for
// differences it can't settle by date. Conflicts and newer files share a colour by
// default, so their folder icons share a row.
const MERGE_TWO_WAY_FILE_ROWS: FileRows = [
  ["same", "Same"],
  ["newer", "Newer than the other side"],
  ["older", "Older than the other side"],
  ["orphan", "Orphan: only on this side"],
  ["conflict", "Conflict: different, neither side newer"],
  ["error", "Error"],
];

const MERGE_TWO_WAY_FOLDER_ROWS: FolderRows = [
  [{ tones: ["conflict"] }, "Holds conflicts or newer files"],
  [{ tones: ["orphan"] }, "Holds orphans, or is one"],
  [{ tones: ["newer", "orphan"] }, "Newer files and orphans"],
  [{ tones: ["newer", "older"] }, "Newer and older files"],
  [{ tones: ["orphan", "older"] }, "Orphans and older files"],
  [{ tones: ["older"] }, "Holds only older files"],
  [{ tones: ["same"] }, "Everything inside is the same"],
];

interface PopoverProps {
  x: number;
  y: number;
  onClose: () => void;
}

/**
 * A legend popover at (x, y), kept inside the window; a click outside it or
 * Escape closes it. File rows in a Folder Merge role carry that role's row wash,
 * as they do in the view.
 */
function Legend({
  x,
  y,
  onClose,
  files,
  folders,
  children,
}: PopoverProps & { files: FileRows; folders: FolderRows; children: ReactNode }) {
  const ref = useRef<HTMLDivElement>(null);
  useLayoutEffect(() => {
    const el = ref.current;
    if (!el) return;
    const { width } = el.getBoundingClientRect();
    el.style.left = `${Math.max(4, Math.min(x, window.innerWidth - width - 4))}px`;
    el.style.top = `${y}px`;
  }, [x, y]);
  useEffect(() => {
    const away = (e: MouseEvent) =>
      !ref.current?.contains(e.target as Node) && onClose();
    const key = (e: KeyboardEvent) => e.key === "Escape" && onClose();
    window.addEventListener("mousedown", away);
    window.addEventListener("keydown", key);
    return () => {
      window.removeEventListener("mousedown", away);
      window.removeEventListener("keydown", key);
    };
  }, [onClose]);

  return (
    <div className="fc-legend" ref={ref} role="dialog" aria-label="Legend">
      <h4>Files</h4>
      {files.map(([tone, text]) => (
        <div key={tone} className={`fc-legend-row${isMergeTone(tone) ? ` fmr-${tone}` : ""}`}>
          <span className="fcell status">
            <ToneSpot tone={tone} />
          </span>
          <span className="fcell name" style={{ color: toneColor(tone) }}>
            {text}
          </span>
        </div>
      ))}
      <h4>Folders</h4>
      {folders.map(([look, text]) => (
        <div key={text} className="fc-legend-row">
          <span className="fcell status">
            <FolderToneIcon look={look} />
          </span>
          <span className="fcell name">{text}</span>
        </div>
      ))}
      {children}
    </div>
  );
}

/** Folder Compare's View > Legend: what the row colours and status glyphs mean. */
export const LegendPopover = (props: PopoverProps) => (
  <Legend {...props} files={FILE_ROWS} folders={FOLDER_ROWS}>
    <p className="muted">
      Each side has its own folder icon, showing the two most important
      colours below it on that side: red, then purple, then grey. Hover a
      folder to see how many rows below have each status. Ignored rows show{" "}
      <EyeOff size={11} /> and count as same.
    </p>
  </Legend>
);

/** Folder Merge's Legend: each side's colours, three-way or (without a base) two-way. */
export const MergeLegendPopover = ({ twoWay, ...props }: PopoverProps & { twoWay: boolean }) =>
  twoWay ? (
    <Legend {...props} files={MERGE_TWO_WAY_FILE_ROWS} folders={MERGE_TWO_WAY_FOLDER_ROWS}>
      <p className="muted">
        Without a base, the merge reads like Folder Compare. Each side's folder
        icon shows the two most important colours below it on that side:
        conflicts first, then newer files, orphans and older files. Conflicts
        carry a light wash of their colour; with Take newer off, every
        difference is one.
      </p>
    </Legend>
  ) : (
    <Legend {...props} files={MERGE_FILE_ROWS} folders={MERGE_FOLDER_ROWS}>
      <p className="muted">
        Changes include additions and deletions (a <Minus size={11} /> marks
        where a file was deleted). Changed file rows carry a light wash of
        their colour. Each side's folder icon shows the two most important
        colours below it on that side: conflicts first, then clean merges,
        left changes, right changes and older copies.
      </p>
    </Legend>
  );
