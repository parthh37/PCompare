// araxis:text-review — "Bookmarks & comments": a floating panel listing a
// session's bookmarks with editable review comments; click one to jump to it.
import { BookmarkX, ClipboardCopy, MessageSquare, MessageSquareText, X } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { writeClipboardText } from "../lib/clipboard";
import "./Review.css";

export interface CommentItem {
  key: string;
  /** "Left 12", "src/main.rs"… */
  title: string;
  /** A preview of the bookmarked line / the row's status. */
  detail?: string;
  comment: string;
  /** The bookmark points at something the view doesn't show right now. */
  stale?: boolean;
}

interface Props {
  items: CommentItem[];
  onComment: (key: string, comment: string) => void;
  onJump: (key: string) => void;
  onRemove: (key: string) => void;
  onClose: () => void;
  /** Focus this item's comment box (after "Comment…" in a context menu). */
  focusKey?: string | null;
  /** Shown when there are no bookmarks yet. */
  emptyHint: string;
}

/** A small icon marking a bookmark that carries a comment; the comment shows on hover. */
export function CommentMark({ comment }: { comment: string }) {
  return (
    <span className="rv-mark" title={comment}>
      <MessageSquareText size={11} />
    </span>
  );
}

export function CommentsPanel({ items, onComment, onJump, onRemove, onClose, focusKey, emptyHint }: Props) {
  const boxes = useRef(new Map<string, HTMLTextAreaElement>());
  const [copied, setCopied] = useState(false);
  const commented = items.filter((i) => i.comment).length;

  useEffect(() => {
    if (!focusKey) return;
    const el = boxes.current.get(focusKey);
    if (!el) return;
    el.focus();
    el.scrollIntoView({ block: "nearest" });
    el.setSelectionRange(el.value.length, el.value.length);
  }, [focusKey, items.length]);

  const copyAll = () => {
    const text = items.map((i) => `${i.title}${i.detail ? ` — ${i.detail}` : ""}${i.comment ? `\n  ${i.comment.replace(/\n/g, "\n  ")}` : ""}`).join("\n");
    writeClipboardText(text)
      .then(() => setCopied(true))
      .catch(() => setCopied(false));
  };

  return (
    <aside className="rv-panel" onKeyDown={(e) => e.key === "Escape" && (e.stopPropagation(), onClose())}>
      <header className="rv-panel-head">
        <MessageSquare size={13} />
        <span>
          Bookmarks &amp; comments
          {items.length > 0 && (
            <span className="muted">
              {" "}
              · {items.length} bookmark{items.length === 1 ? "" : "s"}, {commented} commented
            </span>
          )}
        </span>
        <span className="spacer" />
        <button className="rv-mini" onClick={copyAll} disabled={!items.length} title="Copy every bookmark and comment as text">
          <ClipboardCopy size={11} /> {copied ? "Copied" : "Copy"}
        </button>
        <button className="tab-close" onClick={onClose} aria-label="Close comments">
          <X size={13} />
        </button>
      </header>
      {items.length === 0 ? (
        <p className="rv-empty muted">{emptyHint}</p>
      ) : (
        <ul className="rv-list">
          {items.map((i) => (
            <li key={i.key} className={i.stale ? "stale" : undefined}>
              <div className="rv-item-head">
                <button className="rv-jump" onClick={() => onJump(i.key)} title={i.stale ? "Hidden by the current view" : "Go to this bookmark"}>
                  <b>{i.title}</b>
                  {i.detail && <span className="rv-detail">{i.detail}</span>}
                </button>
                <button className="tab-close" onClick={() => onRemove(i.key)} title="Remove this bookmark and its comment">
                  <BookmarkX size={12} />
                </button>
              </div>
              <textarea
                ref={(el) => {
                  if (el) boxes.current.set(i.key, el);
                  else boxes.current.delete(i.key);
                }}
                rows={i.comment ? Math.min(6, i.comment.split("\n").length + 1) : 1}
                placeholder="Add a comment…"
                spellCheck
                value={i.comment}
                onChange={(e) => onComment(i.key, e.target.value)}
              />
            </li>
          ))}
        </ul>
      )}
    </aside>
  );
}
