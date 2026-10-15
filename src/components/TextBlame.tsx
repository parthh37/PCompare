// Blame-aware Text Compare: who last changed each line, per side, from `git blame`.
// A working-tree side is blamed as edited (unsaved text is sent along, so edits show as
// "not committed" and line numbers stay right); a `git:` side is blamed at its revision.
// Clicking a commit opens that commit's change to the file as a new comparison.
import { useEffect, useState, type CSSProperties, type Dispatch, type ReactNode, type SetStateAction } from "react";
import { errorMessage, isRemote } from "../lib/api";
import { joinLines } from "../lib/mergeApi";
import { gitFileAt, vcsApi, type Blame, type BlameCommit } from "../lib/vcsApi";

type Side = "left" | "right";
/** Re-blame after edits once typing pauses this long. */
const REBLAME_DELAY_MS = 800;

export interface BlameSide {
  /** The path compared on this side ("" for none). */
  path: string;
  lines: string[];
  trailingNewline: boolean;
  /** Unsaved edits: blame the text as shown rather than the file on disk. */
  dirty: boolean;
  /** Clipboard text or a converted document: there's no history to show. */
  noHistory: boolean;
}

interface SideBlame {
  blame: Blame | null;
  error: string | null;
  /** Newest and oldest commit time on this side, for the age shading. */
  range: [number, number];
}

const EMPTY: SideBlame = { blame: null, error: null, range: [0, 0] };

/** "now", "5m", "3h", "4d", "2mo", "3y". */
export function shortAge(timeMs: number, now = Date.now()): string {
  const s = Math.max(0, (now - timeMs) / 1000);
  if (s < 60) return "now";
  if (s < 3600) return `${Math.floor(s / 60)}m`;
  if (s < 86400) return `${Math.floor(s / 3600)}h`;
  if (s < 86400 * 45) return `${Math.floor(s / 86400)}d`;
  if (s < 86400 * 365) return `${Math.floor(s / (86400 * 30))}mo`;
  return `${Math.floor(s / (86400 * 365))}y`;
}

function timeRange(b: Blame): [number, number] {
  if (!b.commits.length) return [0, 0];
  const times = b.commits.map((c) => c.timeMs);
  return [Math.min(...times), Math.max(...times)];
}

function describe(c: BlameCommit): string {
  const when = new Date(c.timeMs).toLocaleString();
  const how = c.previous ? "Click to compare with the version before this commit" : "This commit added the file";
  return `${c.short} · ${c.author} · ${when}\n${c.summary}\n\n${how}`;
}

/** Blames one side whenever its path or (unsaved) text changes; debounced while editing. */
function useSideBlame(
  side: Side,
  s: BlameSide | undefined,
  enabled: boolean,
  setState: Dispatch<SetStateAction<Record<Side, SideBlame>>>,
) {
  const path = s?.path ?? "";
  const noHistory = !!s?.noHistory;
  const dirty = !!s?.dirty;
  // The text matters only while it differs from the file on disk.
  const text = s && dirty ? joinLines(s.lines, s.trailingNewline) : null;
  useEffect(() => {
    if (!enabled || !s) return;
    if (!path || noHistory || isRemote(path)) {
      setState((st) => ({ ...st, [side]: { ...EMPTY, error: path ? "No Git history for this side" : null } }));
      return;
    }
    let stale = false;
    const timer = window.setTimeout(
      () =>
        vcsApi
          .blame(path, text)
          .then((blame) => !stale && setState((st) => ({ ...st, [side]: { blame, error: null, range: timeRange(blame) } })))
          .catch((e) => !stale && setState((st) => ({ ...st, [side]: { ...EMPTY, error: errorMessage(e) } }))),
      dirty ? REBLAME_DELAY_MS : 0,
    );
    return () => {
      stale = true;
      window.clearTimeout(timer);
    };
    // `s` itself is a new object every render; what it holds is covered by the other deps.
  }, [side, enabled, !!s, path, noHistory, dirty, text, setState]);
}

export function useTextBlame({
  enabled,
  sides,
  onOpenCompare,
  onNotice,
}: {
  enabled: boolean;
  sides: Record<Side, BlameSide> | null;
  onOpenCompare?: (left: string, right: string) => void;
  onNotice: (text: string) => void;
}) {
  const [state, setState] = useState<Record<Side, SideBlame>>({ left: EMPTY, right: EMPTY });

  useSideBlame("left", sides?.left, enabled, setState);
  useSideBlame("right", sides?.right, enabled, setState);

  const openCommit = (side: Side, c: BlameCommit) => {
    const b = state[side].blame;
    if (!b) return;
    if (!c.previous) {
      onNotice(`${c.short} added this file (“${c.summary}”), so there's no earlier version to compare`);
      return;
    }
    onOpenCompare?.(gitFileAt(b.root, c.previous.id, c.previous.path), gitFileAt(b.root, c.id, c.path));
  };

  /** The blame gutter for one line (1-based `line`; null on a padding row). */
  const cell = (side: Side, line: number | null): ReactNode => {
    const { blame, range } = state[side];
    if (!blame || line == null) return <span className="tc-blame" />;
    const idx = blame.lines[line - 1];
    if (idx === undefined) return <span className="tc-blame" />;
    // Only the first line of a run from one commit is labelled; the rest show the age bar.
    const first = line === 1 || blame.lines[line - 2] !== idx;
    if (idx === null) {
      return (
        <span className="tc-blame tc-blame-new" title="Not committed yet (changed in the working tree or in this window)">
          {first && <span className="tc-blame-who">Not committed</span>}
        </span>
      );
    }
    const c = blame.commits[idx];
    const [lo, hi] = range;
    const heat = hi > lo ? (c.timeMs - lo) / (hi - lo) : 1;
    return (
      <span
        className={`tc-blame${first ? " tc-blame-first" : ""}`}
        style={{ "--blame-heat": (0.15 + 0.85 * heat).toFixed(2) } as CSSProperties}
        title={describe(c)}
        onMouseDown={(e) => e.stopPropagation()}
        onClick={(e) => {
          e.stopPropagation();
          openCommit(side, c);
        }}
      >
        {first && (
          <>
            <span className="tc-blame-sha">{c.short}</span>
            <span className="tc-blame-who">{c.author}</span>
            <span className="tc-blame-age">{shortAge(c.timeMs)}</span>
          </>
        )}
      </span>
    );
  };

  /** Why a side has no blame, for the info bar. */
  const note = (side: Side): ReactNode => {
    const { error } = state[side];
    return enabled && error ? (
      <span className="tc-blame-note" title={error}>
        No blame
      </span>
    ) : null;
  };

  return { cell, note };
}
