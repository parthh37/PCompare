// Folder-compare status glyphs: per-detail icons for files, combination
// colours for folders (from their status counts), and the View > Legend
// popover that explains them.
import {
  ArrowLeft,
  ArrowRight,
  CircleDashed,
  Clock,
  Equal,
  EqualApproximately,
  EqualNot,
  EyeOff,
  TriangleAlert,
  type LucideIcon,
} from "lucide-react";
import {
  useEffect,
  useId,
  useLayoutEffect,
  useRef,
  type CSSProperties,
} from "react";
import type { FolderEntry, StatusDetail } from "../lib/api";
import { countsNewer, detailOf } from "../lib/folderTree";

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

/** What a folder can hold that colours it, in drawing order. */
export type Tone = "different" | "orphan" | "similar" | "error" | "unknown";

const TONE_OF: Partial<Record<StatusDetail, Tone>> = {
  leftNewer: "different",
  rightNewer: "different",
  different: "different",
  typeMismatch: "different",
  leftOnly: "orphan",
  rightOnly: "orphan",
  similar: "similar",
  error: "error",
  unknown: "unknown",
};
const TONES: Tone[] = ["different", "orphan", "similar", "error", "unknown"];

export const TONE_COLOR: Record<Tone, string> = {
  different: "var(--different)",
  orphan: "var(--orphan)",
  similar: "var(--unimportant)",
  error: "var(--warn)",
  unknown: "var(--muted)",
};

/**
 * The tones of a folder row: what its counts hold, or its own status when
 * it's an orphan, an error or not opened. Empty for same folders and files.
 */
export function folderTones(e: FolderEntry): Tone[] {
  if (!e.isDir || e.ignored) return [];
  const own = detailOf(e);
  const derived = e.counts && e.left && e.right && !e.pending;
  if (!derived) {
    const t = TONE_OF[own];
    return t ? [t] : [];
  }
  const present = new Set<Tone>();
  for (const [d, n] of Object.entries(e.counts!) as [StatusDetail, number][]) {
    const t = TONE_OF[d];
    if (t && n > 0) present.add(t);
  }
  return TONES.filter((t) => present.has(t));
}

/** Text colour for a folder's name: one tone plain, several blended left to right. */
export function toneStyle(tones: Tone[]): CSSProperties | undefined {
  if (!tones.length) return undefined;
  if (tones.length === 1) return { color: TONE_COLOR[tones[0]] };
  return {
    backgroundImage: `linear-gradient(90deg, ${tones.map((t) => TONE_COLOR[t]).join(", ")})`,
    WebkitBackgroundClip: "text",
    backgroundClip: "text",
    color: "transparent",
  };
}

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

const FOLDER_PATH =
  "M1 3.2a1 1 0 0 1 1-1h3.6l1.5 1.5H14a1 1 0 0 1 1 1V12a1 1 0 0 1-1 1H2a1 1 0 0 1-1-1z";

/** A folder glyph filled with one band per tone (combination folder icons). */
export function FolderMix({
  tones,
  size = 14,
}: {
  tones: Tone[];
  size?: number;
}) {
  const clip = useId();
  const w = 14 / tones.length;
  return (
    <svg
      className="folder-mix"
      width={size}
      height={size}
      viewBox="0 0 16 14"
      aria-hidden
    >
      <clipPath id={clip}>
        <path d={FOLDER_PATH} />
      </clipPath>
      <g clipPath={`url(#${clip})`}>
        {tones.map((t, i) => (
          <rect
            key={t}
            x={1 + i * w}
            y={0}
            width={w}
            height={14}
            fill={TONE_COLOR[t]}
          />
        ))}
      </g>
      <path
        d={FOLDER_PATH}
        fill="none"
        stroke="currentColor"
        strokeOpacity={0.45}
        strokeWidth={0.9}
      />
    </svg>
  );
}

/** The status cell's glyph for a row. */
export function StatusGlyph({ entry }: { entry: FolderEntry }) {
  if (entry.ignored) return <EyeOff size={13} />;
  const tones = folderTones(entry);
  if (tones.length && entry.counts) return <FolderMix tones={tones} />;
  const Icon = DETAIL_ICON[detailOf(entry)];
  return <Icon size={13} />;
}

const FILE_ROWS: [StatusDetail, string][] = [
  ["same", "Same"],
  ["sameDiff", "Same contents, size or timestamp differ"],
  ["similar", "Similar: only unimportant differences"],
  ["rightNewer", "Different: newer side bold red, older grey"],
  ["different", "Different, neither side newer"],
  ["leftOnly", "Orphan: only on the left (or right)"],
  ["error", "Error, or a file facing a folder"],
  ["unknown", "Not compared yet"],
];

const FOLDER_ROWS: [Tone[], string][] = [
  [["different"], "Holds differences"],
  [["orphan"], "Holds orphans only (or is one)"],
  [["different", "orphan"], "Differences and orphans"],
  [["similar"], "Only similar files"],
  [["different", "orphan", "error"], "Differences, orphans and errors"],
  [["unknown"], "Contents not compared yet"],
];

/** View > Legend: what the row colours and status glyphs mean. */
export function LegendPopover({
  x,
  y,
  onClose,
}: {
  x: number;
  y: number;
  onClose: () => void;
}) {
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
      {FILE_ROWS.map(([d, text]) => {
        const Icon = DETAIL_ICON[d];
        return (
          <div
            key={d}
            className={`fc-legend-row frow s-${d === "rightNewer" ? "different" : d} d-${d}`}
          >
            <span className="fcell status">
              <Icon size={13} />
            </span>
            <span className="fcell name">{text}</span>
          </div>
        );
      })}
      <h4>Folders</h4>
      {FOLDER_ROWS.map(([tones, text]) => (
        <div key={tones.join()} className="fc-legend-row">
          <span className="fcell status">
            <FolderMix tones={tones} />
          </span>
          <span className="fcell name">
            <span style={toneStyle(tones)}>{text}</span>
          </span>
        </div>
      ))}
      <div className="fc-legend-row">
        <span className="fcell status">
          <Equal size={13} />
        </span>
        <span className="fcell name">Everything inside is the same</span>
      </div>
      <p className="muted">
        A folder&apos;s name is bold on the side all its newer files are on;
        hover it to see how many rows below have each status. Ignored rows show{" "}
        <EyeOff size={11} /> and count as same.
      </p>
    </div>
  );
}
