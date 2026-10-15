// review features for Text Compare, as one hook so the
// view only places what it returns: toolbar buttons (Comments, Statistics,
// Archive), the Bookmarks & comments panel, the dialogs, and comment marks
// for the line-number gutter.
import { ChartColumn, MessageSquare, Package } from "lucide-react";
import { useState, type Dispatch, type ReactNode, type SetStateAction } from "react";
import type { DiffRow, Hunk, TextDiffOptions } from "../lib/api";
import { basename } from "../lib/format";
import {
  archiveTitle,
  ensureTextBookmark,
  reviewTextSide,
  setTextComment,
  textBookmarkMap,
  type TextSideSource,
} from "../lib/reviewApi";
import type { TextSideInput } from "../lib/reviewStats";
import type { Bookmark } from "../lib/textEdit";
import { CommentMark, CommentsPanel } from "./CommentsPanel";
import { ReviewArchiveDialog } from "./ReviewArchiveDialog";
import { StatsDialog } from "./StatsDialog";

type Side = "left" | "right";

interface Args {
  bookmarks: Bookmark[];
  setBookmarks: Dispatch<SetStateAction<Bookmark[]>>;
  docs: Record<Side, TextSideSource & { binary: boolean }> | null;
  /** The displayed diff (empty while there is none). */
  rows: DiffRow[];
  hunks: Hunk[];
  /** 0-based line → row per side, for the displayed rows. */
  lineRow: Record<Side, Int32Array> | null;
  /** Row → index in the displayed list, -1 where the view hides it. */
  rowToDisplay: Int32Array;
  options: TextDiffOptions;
  /** View settings archived with the comparison. */
  settings: Record<string, unknown>;
  /** Moves the caret to a row and scrolls it into view. */
  reveal: (side: Side, row: number) => void;
  onNotice: (message: string) => void;
}

const keyOf = (b: Pick<Bookmark, "side" | "line">) => `${b.side}:${b.line}`;
const sideLabel = (s: Side) => (s === "left" ? "Left" : "Right");
const docLabel = (d: TextSideSource) => (d.memory ? d.info.path : basename(d.info.path) || d.info.path);

export function useTextReview(a: Args) {
  const [panel, setPanel] = useState(false);
  const [focusKey, setFocusKey] = useState<string | null>(null);
  const [stats, setStats] = useState(false);
  const [archiving, setArchiving] = useState(false);
  const byKey = textBookmarkMap(a.bookmarks);
  const usable = !!a.docs && !a.docs.left.binary && !a.docs.right.binary;
  const commented = a.bookmarks.filter((b) => b.comment).length;

  const rowOf = (b: Bookmark) => a.lineRow?.[b.side][b.line - 1] ?? -1;

  /** Bookmarks the caret line (the other side's line on a padding row) and opens its comment box. */
  const commentRow = (side: Side, row: number) => {
    const r = a.rows[row];
    if (!r) return;
    let s = side;
    let n = s === "left" ? r.leftNo : r.rightNo;
    if (n == null) {
      s = s === "left" ? "right" : "left";
      n = s === "left" ? r.leftNo : r.rightNo;
    }
    if (n == null) return;
    const line = n;
    a.setBookmarks((bs) => ensureTextBookmark(bs, s, line));
    setPanel(true);
    setFocusKey(keyOf({ side: s, line }));
  };

  const items = [...a.bookmarks]
    .sort((x, y) => (rowOf(x) - rowOf(y)) || (x.side === y.side ? x.line - y.line : x.side === "left" ? -1 : 1))
    .map((b) => {
      const row = rowOf(b);
      const text = a.docs?.[b.side].lines[b.line - 1];
      return {
        key: keyOf(b),
        title: `${sideLabel(b.side)} ${b.line}`,
        detail: text === undefined ? "line no longer exists" : text.trim() || "(blank line)",
        comment: b.comment ?? "",
        stale: row < 0 || !(a.rowToDisplay[row] >= 0),
      };
    });

  const statsSide = (d: TextSideSource): TextSideInput => ({
    label: docLabel(d),
    lines: d.lines.length,
    size: d.info.size,
    encoding: d.info.encoding,
    hasBom: d.info.hasBom,
    lineEnding: d.info.lineEnding,
  });

  const toolbar: ReactNode = (
    <>
      <button
        className={`icon-btn${panel ? " active" : ""}`}
        onClick={() => setPanel((p) => !p)}
        title={`Bookmarks & comments${a.bookmarks.length ? ` · ${a.bookmarks.length} bookmarks, ${commented} commented` : ""}`}
      >
        <MessageSquare size={14} />
      </button>
      <button className="icon-btn" onClick={() => setStats(true)} disabled={!usable || !a.rows.length} title="Comparison statistics">
        <ChartColumn size={14} />
      </button>
      <button
        className="icon-btn"
        onClick={() => setArchiving(true)}
        disabled={!usable}
        title="Save comparison archive (.pcmpz): both texts, rules, bookmarks and comments in one file to review elsewhere"
      >
        <Package size={14} />
      </button>
    </>
  );

  const layer: ReactNode = panel && (
    <CommentsPanel
      items={items}
      focusKey={focusKey}
      onComment={(key, text) => {
        const b = byKey.get(key);
        if (b) a.setBookmarks((bs) => setTextComment(bs, b, text));
      }}
      onJump={(key) => {
        const b = byKey.get(key);
        const row = b ? rowOf(b) : -1;
        if (b && row >= 0) a.reveal(b.side, row);
      }}
      onRemove={(key) => a.setBookmarks((bs) => bs.filter((b) => keyOf(b) !== key))}
      onClose={() => setPanel(false)}
      emptyHint="Bookmark a line (Ctrl+F2), or right-click a line and choose Comment…, to add a review comment."
    />
  );

  const docs = a.docs;
  const dialogs: ReactNode = (
    <>
      {stats && docs && (
        <StatsDialog
          source={{ kind: "text", rows: a.rows, hunks: a.hunks, left: statsSide(docs.left), right: statsSide(docs.right) }}
          onClose={() => setStats(false)}
        />
      )}
      {archiving && docs && (
        <ReviewArchiveDialog
          comparison={{
            kind: "text",
            left: reviewTextSide(docs.left),
            right: reviewTextSide(docs.right),
            options: a.options,
            settings: a.settings,
            bookmarks: a.bookmarks.filter((b) => b.line <= docs[b.side].lines.length),
          }}
          defaultTitle={archiveTitle(docLabel(docs.left), docLabel(docs.right))}
          onClose={() => setArchiving(false)}
          onSaved={a.onNotice}
        />
      )}
    </>
  );

  return {
    toolbar,
    layer,
    dialogs,
    commentRow,
    /** The comment on a line, if its bookmark has one. */
    comment: (side: Side, n: number | null) => (n == null ? undefined : byKey.get(`${side}:${n}`)?.comment),
    /** A speech-bubble mark for the gutter when the line's bookmark has a comment. */
    mark: (side: Side, n: number | null) => {
      const c = n == null ? undefined : byKey.get(`${side}:${n}`)?.comment;
      return c ? <CommentMark comment={c} /> : null;
    },
  };
}
