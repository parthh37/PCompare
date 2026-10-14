// araxis:text-review — a comparison archive (.pcmpz) opened as a read-only
// session: the embedded texts in Text Compare, or the stored folder tree,
// with the author's bookmarks and comments. Reviewers can add comments and
// write them back into the archive.
import {
  ArrowLeft,
  ArrowRight,
  ChartColumn,
  ChevronDown,
  ChevronRight,
  Equal,
  EqualApproximately,
  EqualNot,
  EyeOff,
  FileText,
  FolderOpen,
  Lock,
  MessageSquare,
  Package,
  Save,
  TriangleAlert,
  type LucideIcon,
} from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { DEFAULT_TEXT_OPTIONS, errorMessage, type EntryStatus, type FolderEntry, type TextDiffOptions } from "../lib/api";
import { allDirs, DISPLAY_FILTERS, effectiveStatus, visibleSet, type DisplayFilter } from "../lib/folderTree";
import { formatSize, formatTime } from "../lib/format";
import {
  ensureFolderBookmark,
  pickArchive,
  reviewApi,
  sameBookmarks,
  setFolderComment,
  toggleFolderBookmark,
  type FolderBookmark,
  type FolderReview,
  type ReviewArchive,
  type ReviewTextSide,
  type TextReview,
} from "../lib/reviewApi";
import type { Bookmark } from "../lib/textEdit";
import { CommentMark, CommentsPanel } from "./CommentsPanel";
import { SEPARATOR, type MenuItems } from "../lib/nativeMenu";
import { useContextMenu } from "./ContextMenu";
import { FileIcon } from "./icons";
import { StatsDialog } from "./StatsDialog";
import { TextCompare, type TextSettings } from "./TextCompare";
import { VirtualList, type VirtualListHandle } from "./VirtualList";
import "./Review.css";

interface Props {
  /** The archive's path. */
  left: string;
  right: string;
  onPathsChange: (left: string, right: string) => void;
  onDirtyChange?: (dirty: boolean) => void;
}

const READ_ONLY = { left: true, right: true };
const textKey = (b: Bookmark) => `${b.side}:${b.line}`;
const folderKey = (b: FolderBookmark) => b.relPath;

export function ReviewArchiveView({ left: path, onPathsChange, onDirtyChange }: Props) {
  const [archive, setArchive] = useState<ReviewArchive | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  /** Bookmarks as last read from / written to the archive, and as the reviewer has them now. */
  const [savedMarks, setSavedMarks] = useState<(Bookmark | FolderBookmark)[]>([]);
  const [marks, setMarks] = useState<(Bookmark | FolderBookmark)[]>([]);
  const [saving, setSaving] = useState(false);

  useEffect(() => {
    setArchive(null);
    setError(null);
    if (!path) return;
    let stale = false;
    reviewApi
      .open(path)
      .then((a) => {
        if (stale) return;
        setArchive(a);
        setSavedMarks(a.comparison.bookmarks);
        setMarks(a.comparison.bookmarks);
      })
      .catch((e) => !stale && setError(errorMessage(e)));
    return () => {
      stale = true;
    };
  }, [path]);

  const kind = archive?.comparison.kind;
  const dirty =
    !!archive &&
    (kind === "text"
      ? !sameBookmarks(savedMarks as Bookmark[], marks as Bookmark[], textKey)
      : !sameBookmarks(savedMarks as FolderBookmark[], marks as FolderBookmark[], folderKey));
  const reportDirty = useRef(onDirtyChange);
  reportDirty.current = onDirtyChange;
  useEffect(() => void reportDirty.current?.(dirty), [dirty]);

  useEffect(() => {
    if (!notice) return;
    const t = window.setTimeout(() => setNotice(null), 4000);
    return () => window.clearTimeout(t);
  }, [notice]);

  const openOther = useCallback(async () => {
    if (dirty && !window.confirm("Your comments aren't saved into the archive yet. Discard them?")) return;
    const picked = await pickArchive().catch(() => null);
    if (picked) onPathsChange(picked, "");
  }, [dirty, onPathsChange]);

  // Opened from Home with no archive yet: ask for one straight away.
  const asked = useRef(false);
  useEffect(() => {
    if (path || asked.current) return;
    asked.current = true;
    void openOther();
  }, [path, openOther]);

  const saveComments = async () => {
    setSaving(true);
    try {
      await reviewApi.saveBookmarks(path, marks as Bookmark[] | FolderBookmark[]);
      setSavedMarks(marks);
      setNotice("Comments saved into the archive");
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setSaving(false);
    }
  };

  if (!path) {
    return (
      <div className="session">
        <div className="empty-state">
          <p>Open a comparison archive (.pcmpz) to review it read-only, with its bookmarks and comments.</p>
          <button className="primary" onClick={openOther}>
            <FolderOpen size={14} /> Open comparison archive…
          </button>
        </div>
      </div>
    );
  }

  const m = archive?.manifest;
  return (
    <div className="rv-session">
      <div className="rv-banner">
        <Package size={14} />
        <b title={path}>{m?.title || path}</b>
        {m && (
          <span className="muted">
            {m.kind === "text" ? "text" : "folder"} comparison archive · saved {formatTime(m.createdMs)}
            {m.appVersion ? ` with PCompare ${m.appVersion}` : ""}
          </span>
        )}
        <span className="muted" title="The archive's contents can't be edited; comments can, and are saved back into the archive">
          <Lock size={11} /> read-only
        </span>
        {notice && <span className="tc-notice">{notice}</span>}
        <span className="spacer" />
        <button onClick={saveComments} disabled={!dirty || saving} title="Write your bookmarks and comments back into this archive">
          <Save size={13} /> {saving ? "Saving…" : "Save comments"}
        </button>
        <button onClick={openOther} title="Open another comparison archive">
          <FolderOpen size={13} /> Open…
        </button>
      </div>
      {error && <div className="banner error">{error}</div>}
      {!archive && !error && <div className="empty-state">Opening the archive…</div>}
      {archive?.comparison.kind === "text" && (
        <TextReviewPane key={path + m?.createdMs} review={archive.comparison} marks={marks as Bookmark[]} onMarks={setMarks} />
      )}
      {archive?.comparison.kind === "folder" && (
        <FolderReviewPane
          key={path + m?.createdMs}
          archivePath={path}
          review={archive.comparison}
          marks={marks as FolderBookmark[]}
          onMarks={setMarks}
        />
      )}
    </div>
  );
}

/** The archive's text rules over today's defaults (older archives lack newer options). */
const reviewOptions = (options: Partial<TextDiffOptions> | undefined): TextDiffOptions => ({
  ...DEFAULT_TEXT_OPTIONS,
  ...options,
});

const memorySide = (s: ReviewTextSide) => ({ text: s.text ?? "", label: s.label || "(untitled)" });

type Pair = "lr" | "bl" | "br";

/** Text Compare over the embedded texts, read-only; bookmarks flow back up for saving. */
function TextReviewPane({ review, marks, onMarks }: { review: TextReview; marks: Bookmark[]; onMarks: (m: Bookmark[]) => void }) {
  const [pair, setPair] = useState<Pair>("lr");
  const base = review.base;
  const [l, r] = pair === "lr" || !base ? [review.left, review.right] : pair === "bl" ? [base, review.left] : [base, review.right];
  // Captured once per pair: Text Compare reads its settings when it mounts.
  const settings = useMemo<Partial<TextSettings>>(
    () => ({
      ...(review.settings as Partial<TextSettings>),
      autoRefresh: false,
      options: reviewOptions(review.options),
      bookmarks: pair === "lr" ? marks : [],
      memory: { left: memorySide(l), right: memorySide(r) },
    }),
    // Not on `marks`: they start from the archive and then live in Text Compare.
    [review, pair],
  );
  const [paths, setPaths] = useState({ left: l.path, right: r.path });
  useEffect(() => setPaths({ left: l.path, right: r.path }), [l.path, r.path]);
  return (
    <>
      {base && (
        <div className="toolbar">
          <span className="muted">3-way archive:</span>
          <div className="seg-control">
            {(
              [
                ["lr", `${review.left.label} ↔ ${review.right.label}`],
                ["bl", `Base ↔ ${review.left.label}`],
                ["br", `Base ↔ ${review.right.label}`],
              ] as [Pair, string][]
            ).map(([p, label]) => (
              <button key={p} className={pair === p ? "active" : ""} onClick={() => setPair(p)}>
                {label}
              </button>
            ))}
          </div>
          {pair !== "lr" && <span className="muted">Bookmarks and comments belong to the left ↔ right pair.</span>}
        </div>
      )}
      <TextCompare
        key={pair}
        left={paths.left}
        right={paths.right}
        onPathsChange={(left, right) => setPaths({ left, right })}
        onOpenHex={() => {}}
        settings={settings}
        onSettingsChange={(s) => pair === "lr" && s.bookmarks && onMarks(s.bookmarks)}
        readOnlySides={READ_ONLY}
      />
    </>
  );
}

const STATUS_ICON: Record<EntryStatus, LucideIcon> = {
  same: Equal,
  similar: EqualApproximately,
  different: EqualNot,
  leftOnly: ArrowLeft,
  rightOnly: ArrowRight,
  typeMismatch: TriangleAlert,
  error: TriangleAlert,
};

const STATUS_LABEL: Record<EntryStatus, string> = {
  same: "Same",
  similar: "Similar",
  different: "Different",
  leftOnly: "Left only",
  rightOnly: "Right only",
  typeMismatch: "File ↔ folder",
  error: "Error",
};

interface FlatRow {
  entry: FolderEntry;
  depth: number;
}

/** The stored folder tree with display filters, bookmarks and comments; embedded files open in a read-only text compare. */
function FolderReviewPane({
  archivePath,
  review,
  marks,
  onMarks,
}: {
  archivePath: string;
  review: FolderReview;
  marks: FolderBookmark[];
  onMarks: (m: FolderBookmark[]) => void;
}) {
  const entries = review.result.entries;
  const [filter, setFilter] = useState<DisplayFilter>(
    ((review.settings as { filter?: DisplayFilter }).filter as DisplayFilter | undefined) ?? "all",
  );
  const [expanded, setExpanded] = useState<Set<string>>(() => allDirs(entries));
  const [selected, setSelected] = useState<string | null>(null);
  const [panel, setPanel] = useState(marks.length > 0);
  const [focusKey, setFocusKey] = useState<string | null>(null);
  const [stats, setStats] = useState(false);
  const [file, setFile] = useState<{ relPath: string; left: { text: string; label: string }; right: { text: string; label: string } } | null>(
    null,
  );
  const [error, setError] = useState<string | null>(null);
  const list = useRef<VirtualListHandle>(null);

  const embedded = useMemo(() => new Map(review.embedded.map((e) => [e.relPath, e])), [review.embedded]);
  const markMap = useMemo(() => new Map(marks.map((b) => [b.relPath, b])), [marks]);
  const visible = useMemo(() => visibleSet(entries, filter, null), [entries, filter]);
  const byPath = useMemo(() => {
    const map = new Map<string, FolderEntry>();
    const walk = (list: FolderEntry[]) => list.forEach((e) => (map.set(e.relPath, e), walk(e.children)));
    walk(entries);
    return map;
  }, [entries]);

  const flat = useMemo(() => {
    const out: FlatRow[] = [];
    const walk = (list: FolderEntry[], depth: number) => {
      for (const e of list) {
        if (!visible.has(e.relPath)) continue;
        out.push({ entry: e, depth });
        if (e.isDir && expanded.has(e.relPath)) walk(e.children, depth + 1);
      }
    };
    walk(entries, 0);
    return out;
  }, [entries, visible, expanded]);

  const menu = useContextMenu((e: FolderEntry): MenuItems => [
    embedded.has(e.relPath) && { label: "Compare stored contents", onClick: () => void openFile(e) },
    SEPARATOR,
    {
      label: markMap.has(e.relPath) ? "Remove bookmark" : "Bookmark",
      onClick: () => onMarks(toggleFolderBookmark(marks, e.relPath)),
    },
    { label: markMap.get(e.relPath)?.comment ? "Edit comment…" : "Comment…", onClick: () => comment(e.relPath) },
  ]);

  const toggle = (rel: string) =>
    setExpanded((prev) => {
      const next = new Set(prev);
      next.has(rel) ? next.delete(rel) : next.add(rel);
      return next;
    });

  const openFile = async (e: FolderEntry) => {
    if (e.isDir) return toggle(e.relPath);
    if (!embedded.has(e.relPath)) return;
    try {
      const pair = await reviewApi.readFile(archivePath, e.relPath);
      setFile({
        relPath: e.relPath,
        left: { text: pair.left?.text ?? "", label: `Left: ${e.leftRelPath ?? e.relPath}` },
        right: { text: pair.right?.text ?? "", label: `Right: ${e.relPath}` },
      });
    } catch (err) {
      setError(errorMessage(err));
    }
  };

  /** Shows a bookmarked row: switches to All when the filter hides it and opens its folders. */
  const jump = (rel: string) => {
    if (!byPath.has(rel)) return;
    if (!visible.has(rel)) setFilter("all");
    setExpanded((prev) => {
      const next = new Set(prev);
      for (let i = rel.indexOf("/"); i >= 0; i = rel.indexOf("/", i + 1)) next.add(rel.slice(0, i));
      return next;
    });
    setSelected(rel);
    setFile(null);
  };
  // Scroll once the row is in the flattened list.
  useEffect(() => {
    if (!selected) return;
    const i = flat.findIndex((f) => f.entry.relPath === selected);
    if (i >= 0) list.current?.scrollToRow(i);
    // Only when the selection moves, not on every expand.
  }, [selected]);

  const comment = (rel: string) => {
    onMarks(ensureFolderBookmark(marks, rel));
    setPanel(true);
    setFocusKey(rel);
  };

  if (file) {
    return (
      <>
        <div className="toolbar">
          <button onClick={() => setFile(null)}>
            <ArrowLeft size={14} /> Back to the folder comparison
          </button>
          <span className="muted">{file.relPath} — contents stored in the archive</span>
        </div>
        <TextCompare
          key={file.relPath}
          left=""
          right=""
          onPathsChange={() => {}}
          onOpenHex={() => {}}
          settings={{
            options: reviewOptions((review.options as { rules?: Partial<TextDiffOptions> } | null)?.rules),
            memory: { left: file.left, right: file.right },
          }}
          readOnlySides={READ_ONLY}
        />
      </>
    );
  }

  const renderSide = (e: FolderEntry, side: "left" | "right", depth: number) => {
    const meta = e[side];
    const newer = e.newer === side ? " newer" : e.newer ? " older" : "";
    const mark = markMap.get(e.relPath);
    const markSide = e.left ? "left" : "right";
    return (
      <>
        <div className={`fcell name${meta ? "" : " absent"}${newer}`} style={{ paddingLeft: 6 + depth * 16 }}>
          {e.isDir && meta ? (
            <span
              className="chevron"
              onClick={(ev) => {
                ev.stopPropagation();
                toggle(e.relPath);
              }}
            >
              {expanded.has(e.relPath) ? <ChevronDown size={13} /> : <ChevronRight size={13} />}
            </span>
          ) : (
            <span className="chevron" />
          )}
          {meta && (
            <>
              <span className="ficon">
                <FileIcon name={e.name} isDir={e.isDir} open={e.isDir && expanded.has(e.relPath)} />
              </span>
              {side === "left" && e.leftRelPath ? e.leftRelPath.slice(e.leftRelPath.lastIndexOf("/") + 1) : e.name}
              {side === markSide && embedded.has(e.relPath) && (
                <span className="rv-embedded" title="Contents stored in the archive: double-click to compare">
                  <FileText size={11} />
                </span>
              )}
              {side === markSide && mark?.comment && <CommentMark comment={mark.comment} />}
            </>
          )}
        </div>
        <div className={`fcell num${newer}`}>{meta && !meta.isDir ? formatSize(meta.size) : ""}</div>
        <div className={`fcell time${newer}`}>{meta ? formatTime(meta.modifiedMs) : ""}</div>
      </>
    );
  };

  const renderRow = (i: number) => {
    const { entry: e, depth } = flat[i];
    const StatusIcon = e.ignored ? EyeOff : STATUS_ICON[e.status];
    const mark = markMap.get(e.relPath);
    return (
      <div
        className={`frow s-${effectiveStatus(e)}${e.ignored ? " ignored" : ""}${selected === e.relPath ? " selected" : ""}${mark ? " rv-bookmarked" : ""}`}
        onClick={() => setSelected(e.relPath)}
        onDoubleClick={() => openFile(e)}
        onContextMenu={(ev) => {
          ev.preventDefault();
          setSelected(e.relPath);
          menu.open(ev, e);
        }}
        title={[STATUS_LABEL[e.status], mark?.comment].filter(Boolean).join(" · ")}
      >
        {renderSide(e, "left", depth)}
        <div className="fcell status">
          <StatusIcon size={13} />
        </div>
        {renderSide(e, "right", depth)}
      </div>
    );
  };

  const s = review.result.stats;
  return (
    <div className="session">
      <div className="toolbar">
        <div className="seg-control">
          {DISPLAY_FILTERS.filter((f) => f.primary).map((f) => (
            <button key={f.value} className={filter === f.value ? "active" : ""} onClick={() => setFilter(f.value)}>
              {f.label}
            </button>
          ))}
        </div>
        <button onClick={() => setExpanded(allDirs(entries))}>Expand all</button>
        <button onClick={() => setExpanded(new Set())}>Collapse all</button>
        <span className="spacer" />
        <button
          className={`icon-btn${panel ? " active" : ""}`}
          onClick={() => setPanel((p) => !p)}
          title={`Bookmarks & comments${marks.length ? ` · ${marks.length}` : ""}`}
        >
          <MessageSquare size={14} />
        </button>
        <button className="icon-btn" onClick={() => setStats(true)} title="Comparison statistics">
          <ChartColumn size={14} />
        </button>
      </div>
      <div className="infobar">
        <span title={review.leftRoot}>{review.leftRoot}</span>
        <span title={review.rightRoot}>{review.rightRoot}</span>
      </div>
      <div className="fheader">
        <div>Name</div>
        <div className="num">Size</div>
        <div>Modified</div>
        <div />
        <div>Name</div>
        <div className="num">Size</div>
        <div>Modified</div>
      </div>
      <div className="session-body">
        {error && <div className="banner error">{error}</div>}
        {flat.length === 0 && <div className="empty-state">Nothing to show for this filter.</div>}
        <VirtualList ref={list} className="folder-list" count={flat.length} rowHeight={24} renderRow={renderRow} />
        {panel && (
          <CommentsPanel
            items={marks.map((b) => {
              const e = byPath.get(b.relPath);
              return {
                key: b.relPath,
                title: b.relPath,
                detail: e ? STATUS_LABEL[e.status] : "not in this comparison",
                comment: b.comment ?? "",
                stale: !e || !visible.has(b.relPath),
              };
            })}
            focusKey={focusKey}
            onComment={(key, text) => onMarks(setFolderComment(marks, key, text))}
            onJump={jump}
            onRemove={(key) => onMarks(marks.filter((b) => b.relPath !== key))}
            onClose={() => setPanel(false)}
            emptyHint="Right-click a row and choose Comment… to bookmark it with a review comment."
          />
        )}
      </div>
      <div className="statusbar">
        <span className="s-same-fg">{s.same} same</span>
        {s.similar > 0 && <span className="s-similar-fg">{s.similar} similar</span>}
        <span className="s-different-fg">{s.different} different</span>
        <span className="s-leftOnly-fg">{s.leftOnly} left only</span>
        <span className="s-rightOnly-fg">{s.rightOnly} right only</span>
        <span className="spacer" />
        <span className="muted">
          {review.embedded.length} file{review.embedded.length === 1 ? "" : "s"} with contents · double-click one to compare
        </span>
      </div>
      {menu.element}
      {stats && (
        <StatsDialog
          source={{ kind: "folder", entries, leftRoot: review.leftRoot, rightRoot: review.rightRoot }}
          onClose={() => setStats(false)}
        />
      )}
    </div>
  );
}
