// araxis:text-review — review features for Folder Compare, as one hook so the
// view only places what it returns: toolbar buttons (Comments, Statistics,
// Archive), the Bookmarks & comments panel, the dialogs, row marks and
// context-menu items. Rows are bookmarked by relative path.
import { ChartColumn, MessageSquare, Package } from "lucide-react";
import { useEffect, useMemo, useRef, useState, type ReactNode, type RefObject } from "react";
import type { EntryStatus, FolderCompareResult, FolderEntry, FolderStats, Side } from "../lib/api";
import { basename } from "../lib/format";
import type { MenuItems } from "../lib/nativeMenu";
import {
  archiveTitle,
  ensureFolderBookmark,
  setFolderComment,
  toggleFolderBookmark,
  type FolderBookmark,
} from "../lib/reviewApi";
import { CommentMark, CommentsPanel } from "./CommentsPanel";
import { ReviewArchiveDialog } from "./ReviewArchiveDialog";
import { StatsDialog } from "./StatsDialog";
import "./Review.css";

interface Args {
  bookmarks: FolderBookmark[];
  onBookmarks: (marks: FolderBookmark[]) => void;
  result: FolderCompareResult | null;
  /** The tree as shown: content re-checks and ignore marks applied. */
  entries: FolderEntry[];
  stats: FolderStats | undefined;
  /** Relative paths the display filter shows, and the rows on screen. */
  visible: Set<string>;
  flat: { entry: FolderEntry }[];
  /** The session root, to find its folder list for scrolling. */
  root: RefObject<HTMLDivElement | null>;
  rowHeight: number;
  /** Opens these folders. */
  expand: (relPaths: string[]) => void;
  /** Switches the display filter to All (a bookmark the filter hides was chosen). */
  showAll: () => void;
  engineOptions: unknown;
  settings: Record<string, unknown>;
}

const STATUS_WORD: Record<EntryStatus, string> = {
  same: "same",
  similar: "similar",
  different: "different",
  leftOnly: "left only",
  rightOnly: "right only",
  typeMismatch: "file ↔ folder",
  error: "error",
};

export function useFolderReview(a: Args) {
  const [panel, setPanel] = useState(false);
  const [focusKey, setFocusKey] = useState<string | null>(null);
  const [stats, setStats] = useState(false);
  const [archiving, setArchiving] = useState(false);
  const [notice, setNotice] = useState<string | null>(null);
  /** The row a jump landed on, briefly highlighted. */
  const [flash, setFlash] = useState<string | null>(null);
  const pending = useRef<string | null>(null);
  const byPath = useMemo(() => {
    const map = new Map<string, FolderEntry>();
    const walk = (list: FolderEntry[]) => list.forEach((e) => (map.set(e.relPath, e), walk(e.children)));
    walk(a.entries);
    return map;
  }, [a.entries]);
  const marks = useMemo(() => new Map(a.bookmarks.map((b) => [b.relPath, b])), [a.bookmarks]);

  useEffect(() => {
    if (!notice) return;
    const t = window.setTimeout(() => setNotice(null), 4000);
    return () => window.clearTimeout(t);
  }, [notice]);
  useEffect(() => {
    if (!flash) return;
    const t = window.setTimeout(() => setFlash(null), 1500);
    return () => window.clearTimeout(t);
  }, [flash]);

  // Scroll to a chosen bookmark once its folders are open and it is in the list.
  useEffect(() => {
    const rel = pending.current;
    if (!rel) return;
    const i = a.flat.findIndex((r) => r.entry.relPath === rel);
    if (i < 0) return;
    pending.current = null;
    const el = a.root.current?.querySelector<HTMLElement>(".folder-list");
    if (el) el.scrollTop = Math.max(0, i * a.rowHeight - el.clientHeight / 3);
    setFlash(rel);
  }, [a.flat, a.root, a.rowHeight]);

  const reveal = (rel: string) => {
    if (!byPath.has(rel)) return;
    if (!a.visible.has(rel)) a.showAll();
    const parents: string[] = [];
    for (let i = rel.indexOf("/"); i >= 0; i = rel.indexOf("/", i + 1)) parents.push(rel.slice(0, i));
    a.expand(parents);
    pending.current = rel;
    // Already on screen: the effect above won't rerun, so scroll now.
    if (a.visible.has(rel) && a.flat.some((r) => r.entry.relPath === rel)) {
      const i = a.flat.findIndex((r) => r.entry.relPath === rel);
      const el = a.root.current?.querySelector<HTMLElement>(".folder-list");
      if (el) el.scrollTop = Math.max(0, i * a.rowHeight - el.clientHeight / 3);
      pending.current = null;
      setFlash(rel);
    }
  };

  const comment = (rel: string) => {
    a.onBookmarks(ensureFolderBookmark(a.bookmarks, rel));
    setPanel(true);
    setFocusKey(rel);
  };

  const commented = a.bookmarks.filter((b) => b.comment).length;
  const toolbar: ReactNode = (
    <>
      <button
        className={`tool-btn${panel ? " active" : ""}`}
        aria-pressed={panel}
        title={`Bookmarks & comments${a.bookmarks.length ? ` · ${a.bookmarks.length} bookmarks, ${commented} commented` : " (right-click a row → Comment…)"}`}
        onClick={() => setPanel((p) => !p)}
      >
        <MessageSquare size={20} className="rv-tool" />
        <span>{a.bookmarks.length ? `Comments · ${a.bookmarks.length}` : "Comments"}</span>
      </button>
      <button className="tool-btn" title="Comparison statistics" disabled={!a.result} onClick={() => setStats(true)}>
        <ChartColumn size={20} className="rv-tool" />
        <span>Stats</span>
      </button>
      <button
        className="tool-btn"
        title="Save comparison archive (.pcmpz): the compared tree, rules, bookmarks and comments (and optionally differing text files) in one file to review elsewhere"
        disabled={!a.result}
        onClick={() => setArchiving(true)}
      >
        <Package size={20} className="rv-tool" />
        <span>Archive</span>
      </button>
    </>
  );

  const layer: ReactNode = (
    <>
      {panel && (
        <CommentsPanel
          items={a.bookmarks.map((b) => {
            const e = byPath.get(b.relPath);
            return {
              key: b.relPath,
              title: b.relPath,
              detail: e ? STATUS_WORD[e.ignored ? "same" : e.status] : "not in this comparison",
              comment: b.comment ?? "",
              stale: !e || !a.visible.has(b.relPath),
            };
          })}
          focusKey={focusKey}
          onComment={(key, text) => a.onBookmarks(setFolderComment(a.bookmarks, key, text))}
          onJump={reveal}
          onRemove={(key) => a.onBookmarks(a.bookmarks.filter((b) => b.relPath !== key))}
          onClose={() => setPanel(false)}
          emptyHint="Right-click a row and choose Comment… to bookmark it with a review comment."
        />
      )}
      {notice && <div className="toast">{notice}</div>}
    </>
  );

  const r = a.result;
  const dialogs: ReactNode = r && (
    <>
      {stats && (
        <StatsDialog
          source={{ kind: "folder", entries: a.entries, leftRoot: r.leftRoot, rightRoot: r.rightRoot }}
          onClose={() => setStats(false)}
        />
      )}
      {archiving && (
        <ReviewArchiveDialog
          comparison={{
            kind: "folder",
            leftRoot: r.leftRoot,
            rightRoot: r.rightRoot,
            result: { ...r, entries: a.entries, stats: a.stats ?? r.stats },
            options: a.engineOptions,
            settings: a.settings,
            bookmarks: a.bookmarks,
            embedded: [],
          }}
          defaultTitle={archiveTitle(basename(r.leftRoot) || r.leftRoot, basename(r.rightRoot) || r.rightRoot)}
          onClose={() => setArchiving(false)}
          onSaved={setNotice}
        />
      )}
    </>
  );

  return {
    toolbar,
    layer,
    dialogs,
    /** Extra classes for a row: bookmarked, or just jumped to. */
    rowClass: (rel: string) => `${marks.has(rel) ? " rv-bookmarked" : ""}${flash === rel ? " selected" : ""}`,
    /** The row tooltip with the bookmark's comment added. */
    title: (rel: string, base: string) => {
      const c = marks.get(rel)?.comment;
      return c ? `${base}\nComment: ${c}` : base;
    },
    /** A speech bubble after the name (on the first side the entry exists on) when the row has a comment. */
    mark: (e: FolderEntry, side: Side) => {
      const c = marks.get(e.relPath)?.comment;
      return c && side === (e.left ? "left" : "right") ? <CommentMark comment={c} /> : null;
    },
    /** Context-menu entries for a row. */
    menuItems: (e: FolderEntry): MenuItems => [
      { label: marks.get(e.relPath)?.comment ? "Edit comment…" : "Comment…", onClick: () => comment(e.relPath) },
      {
        label: marks.has(e.relPath) ? "Remove bookmark" : "Bookmark",
        onClick: () => a.onBookmarks(toggleFolderBookmark(a.bookmarks, e.relPath)),
      },
    ],
  };
}
