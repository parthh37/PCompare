// The Backups tab: the files PCompare kept before overwriting or removing
// them, one backup per operation, grouped by day. A row expands to its files;
// restoring backs up what is there now first, so a restore can be undone. The
// list follows the backend's "backups-changed" events.
import {
  ArchiveRestore,
  BrushCleaning,
  ChevronDown,
  ChevronRight,
  CircleQuestionMark,
  File,
  Folder,
  FolderOpen,
  GitCompareArrows,
  RefreshCw,
  RotateCcw,
  Search,
  ShieldOff,
  Trash,
  TriangleAlert,
  Undo2,
  X,
} from "lucide-react";
import { memo, useCallback, useDeferredValue, useEffect, useMemo, useRef, useState } from "react";
import { errorMessage, shellApi } from "../lib/api";
import {
  SIZE_OPTIONS,
  backupsApi,
  keepLabel,
  setBackupPrefs,
  storedPath,
  type Backup,
  type BackupItem,
  type BackupsInfo,
  type RestoreReport,
} from "../lib/backupsApi";
import { basename, formatSize, formatTime } from "../lib/format";
import { usePref } from "../lib/prefs";
import "./BackupsPage.css";
import { Modal } from "./Modal";
import { SessionIcon } from "./SessionIcon";

interface Props {
  /** A backup to open, scroll to and highlight (e.g. one a save just made). */
  focusId?: string | null;
  /** Bumped by each request to show `focusId`, so asking for the same backup again still scrolls to it. */
  focusSeq?: number;
  /** Opens a comparison; "Compare with current" passes the kept version and the current file. */
  onCompare: (left: string, right: string) => void;
  onHelp?: () => void;
}

/** A backup as listed: all its items, or only those matching the search. */
interface Shown {
  backup: Backup;
  items: BackupItem[];
}

/** The open dialog: a confirmation, or a restore's problems. */
type Pending =
  | { kind: "restore"; backup: Backup; items: BackupItem[]; busy?: boolean; error?: string }
  | { kind: "delete"; backup: Backup; busy?: boolean; error?: string }
  | { kind: "result"; report: RestoreReport; undoing: boolean };

/** The message at the bottom of the page, with Undo after a restore. */
interface Notice {
  text: string;
  error?: boolean;
  /** The backup that undoes the restore just made. */
  undo?: string;
}

/** Items shown at first when a backup is expanded (a big sync can keep thousands). */
const ITEMS_AT_ONCE = 200;
/** Paths listed in the restore confirmation. */
const CONFIRM_PATHS = 8;

const plural = (n: number, one: string, many = `${one}s`) => `${n.toLocaleString()} ${n === 1 ? one : many}`;

/** "3 files", or "3 items" when folders are among them. */
const countItems = (items: BackupItem[]) => plural(items.length, items.some((i) => i.isDir) ? "item" : "file");

/** The folder a path is in ("" at the top). */
function parentOf(path: string): string {
  const trimmed = path.replace(/[\\/]+$/, "");
  const idx = Math.max(trimmed.lastIndexOf("/"), trimmed.lastIndexOf("\\"));
  return idx > 0 ? trimmed.slice(0, idx) : "";
}

/**
 * Paths are laid out right-to-left so a long one is cut at its start and the
 * name stays visible; the marks keep leading `\\` and trailing separators in
 * place.
 */
const ltr = (path: string) => `\u200e${path}\u200e`;

const startOfDay = (ms: number) => {
  const d = new Date(ms);
  return new Date(d.getFullYear(), d.getMonth(), d.getDate()).getTime();
};
const dayFmt = new Intl.DateTimeFormat(undefined, { weekday: "long", day: "numeric", month: "long", year: "numeric" });
const timeFmt = new Intl.DateTimeFormat(undefined, { hour: "2-digit", minute: "2-digit" });

/** "Today", "Yesterday" or the local date of the day starting at `day`. */
function dayLabel(day: number, today: number): string {
  if (day === today) return "Today";
  if (day === startOfDay(today - 1)) return "Yesterday";
  return dayFmt.format(day);
}

const sizeLimitLabel = (mb: number) => SIZE_OPTIONS.find((o) => o.mb === mb)?.label ?? formatSize(mb * 1024 * 1024);

const turnOn = () => setBackupPrefs({ enabled: true, asked: true });

export function BackupsPage({ focusId, focusSeq, onCompare, onHelp }: Props) {
  const prefs = usePref("backups");
  const [backups, setBackups] = useState<Backup[] | null>(null);
  const [info, setInfo] = useState<BackupsInfo | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [query, setQuery] = useState("");
  const [expanded, setExpanded] = useState<ReadonlySet<string>>(() => new Set());
  const [flash, setFlash] = useState<string | null>(null);
  const [pending, setPending] = useState<Pending | null>(null);
  const [notice, setNotice] = useState<Notice | null>(null);
  const [cleaning, setCleaning] = useState(false);
  const [today, setToday] = useState(() => startOfDay(Date.now()));

  // "Today" and "Yesterday" move on at midnight (a same-day update renders nothing).
  useEffect(() => {
    const timer = window.setInterval(() => setToday(startOfDay(Date.now())), 60_000);
    return () => window.clearInterval(timer);
  }, []);

  // Replies can arrive out of order (an event's reload racing Refresh): only the latest counts.
  const loadSeq = useRef(0);
  const load = useCallback(() => {
    const n = ++loadSeq.current;
    return Promise.all([backupsApi.info(), backupsApi.list()]).then(
      ([i, list]) => {
        if (n !== loadSeq.current) return;
        setInfo(i);
        setBackups(list);
        setError(null);
      },
      (e) => {
        if (n !== loadSeq.current) return;
        setBackups((b) => b ?? []);
        setError(errorMessage(e));
      },
    );
  }, []);

  const reloadTimer = useRef<number | undefined>(undefined);
  const reloadSoon = useCallback(
    (delay = 300) => {
      window.clearTimeout(reloadTimer.current);
      reloadTimer.current = window.setTimeout(() => void load(), delay);
    },
    [load],
  );
  useEffect(() => () => window.clearTimeout(reloadTimer.current), []);

  // Backups made, restored or removed anywhere (other tabs, automated syncs):
  // reload once a burst of them has settled.
  useEffect(() => {
    let unlisten: (() => void) | undefined;
    let cancelled = false;
    backupsApi
      .onChanged(() => reloadSoon())
      .then((u) => (cancelled ? u() : (unlisten = u)))
      .catch(() => {});
    return () => {
      cancelled = true;
      unlisten?.();
    };
  }, [reloadSoon]);

  // Another folder lists other backups. The backend reads the folder from the
  // saved preferences, which are written a moment after a change (prefs.ts).
  const loaded = useRef(false);
  useEffect(() => {
    if (loaded.current) reloadSoon(800);
    else void load();
    loaded.current = true;
  }, [prefs.folder, load, reloadSoon]);

  useEffect(() => {
    if (!flash) return;
    const timer = window.setTimeout(() => setFlash(null), 2000);
    return () => window.clearTimeout(timer);
  }, [flash]);

  useEffect(() => {
    if (!notice) return;
    const timer = window.setTimeout(() => setNotice(null), notice.undo ? 12_000 : 6_000);
    return () => window.clearTimeout(timer);
  }, [notice]);

  // Filtering thousands of backups per keystroke may lag; typing shouldn't.
  const q = useDeferredValue(query).trim().toLowerCase();
  const shown = useMemo<Shown[]>(() => {
    if (!backups) return [];
    if (!q) return backups.map((backup) => ({ backup, items: backup.items }));
    const out: Shown[] = [];
    for (const backup of backups) {
      const items = backup.items.filter((i) => i.original.toLowerCase().includes(q));
      if (items.length) out.push({ backup, items });
      else if ([backup.action, backup.tool, backup.left, backup.right].some((s) => s.toLowerCase().includes(q)))
        out.push({ backup, items: backup.items });
    }
    return out;
  }, [backups, q]);

  // Newest first, so a day's backups are next to each other.
  const groups = useMemo(() => {
    const out: { day: number; rows: Shown[] }[] = [];
    for (const s of shown) {
      const day = startOfDay(s.backup.timeMs);
      const last = out[out.length - 1];
      if (last?.day === day) last.rows.push(s);
      else out.push({ day, rows: [s] });
    }
    return out;
  }, [shown]);

  const totalSize = useMemo(() => backups?.reduce((n, b) => n + b.size, 0) ?? 0, [backups]);

  // Opened to show one backup: expand it, then scroll to it and flash it once it's listed.
  const [focus, setFocus] = useState<string | null>(null);
  const focusReloaded = useRef(false);
  useEffect(() => {
    if (!focusId) return;
    focusReloaded.current = false;
    setFocus(focusId);
    setQuery("");
    setExpanded((prev) => (prev.has(focusId) ? prev : new Set(prev).add(focusId)));
  }, [focusId, focusSeq]);

  const listRef = useRef<HTMLDivElement>(null);
  useEffect(() => {
    if (!focus || !backups) return;
    if (!backups.some((b) => b.id === focus)) {
      // Made a moment ago and not listed yet: look once more, then give up.
      if (focusReloaded.current) setFocus(null);
      else {
        focusReloaded.current = true;
        void load();
      }
      return;
    }
    if (!shown.some((s) => s.backup.id === focus)) return; // the cleared search hasn't applied yet
    listRef.current?.querySelector(`[data-backup="${CSS.escape(focus)}"]`)?.scrollIntoView({ block: "center" });
    setFlash(focus);
    setFocus(null);
  }, [focus, backups, shown, load]);

  const fail = useCallback((e: unknown) => setNotice({ text: errorMessage(e), error: true }), []);

  // Rows are memoised: everything they're handed stays the same between renders.
  const toggle = useCallback(
    (id: string) =>
      setExpanded((prev) => {
        const next = new Set(prev);
        if (!next.delete(id)) next.add(id);
        return next;
      }),
    [],
  );
  const askRestore = useCallback((backup: Backup, items: BackupItem[]) => setPending({ kind: "restore", backup, items }), []);
  const askDelete = useCallback((backup: Backup) => setPending({ kind: "delete", backup }), []);
  const reveal = useCallback((path: string) => void shellApi.revealInFileManager(path).catch(fail), [fail]);
  const onCompareRef = useRef(onCompare);
  onCompareRef.current = onCompare;
  const compare = useCallback(
    (backup: Backup, item: BackupItem) => onCompareRef.current(storedPath(backup, item), item.original),
    [],
  );

  /** Reports a restore or an undo: problems in a dialog, else a notice (with Undo after a restore). */
  const settle = (report: RestoreReport, what: string, undoing: boolean) => {
    reloadSoon();
    if (report.outcomes.some((o) => o.error)) {
      setPending({ kind: "result", report, undoing });
      return;
    }
    setPending(null);
    setNotice(
      undoing
        ? { text: "Restore undone: the files are back as they were before it" }
        : { text: `Restored ${what}`, undo: report.undo ?? undefined },
    );
  };

  const restore = (backup: Backup, items: BackupItem[]) => {
    setPending({ kind: "restore", backup, items, busy: true });
    const all = items.length === backup.items.length;
    backupsApi.restore(backup.id, all ? [] : items.map((i) => i.stored)).then(
      (report) => settle(report, countItems(items), false),
      (e) => setPending({ kind: "restore", backup, items, error: errorMessage(e) }),
    );
  };

  const undo = (id: string) => {
    setPending(null);
    setNotice({ text: "Undoing the restore…" });
    backupsApi.restore(id, []).then((report) => settle(report, "", true), fail);
  };

  const remove = (backup: Backup) => {
    setPending({ kind: "delete", backup, busy: true });
    backupsApi.remove([backup.id]).then(
      () => {
        setPending(null);
        setBackups((list) => list?.filter((b) => b.id !== backup.id) ?? null);
        setNotice({ text: `Deleted the backup from ${formatTime(backup.timeMs)}` });
      },
      (e) => setPending({ kind: "delete", backup, error: errorMessage(e) }),
    );
  };

  const cleanUp = () => {
    setCleaning(true);
    backupsApi
      .prune()
      .then(
        (r) =>
          setNotice({
            text: r.removed
              ? `Removed ${plural(r.removed, "backup")}, freed ${formatSize(r.freed)}`
              : "Nothing to clean up",
          }),
        fail,
      )
      .finally(() => {
        setCleaning(false);
        void load();
      });
  };

  const keep = prefs.keepDays === 0 ? "kept until you delete them" : `kept for ${keepLabel(prefs.keepDays)}`;
  const limit = prefs.maxSizeMb > 0 ? sizeLimitLabel(prefs.maxSizeMb) : null;
  const summary =
    backups === null
      ? "Loading…"
      : [plural(backups.length, "backup"), formatSize(totalSize), keep, limit && `up to ${limit}`]
          .filter(Boolean)
          .join(" · ");
  const cleanUpTitle =
    prefs.keepDays === 0 && !limit
      ? "Backups are kept until you delete them (Settings → Backups)"
      : `Remove now what the settings no longer keep: backups older than ${keepLabel(prefs.keepDays).toLowerCase()}${
          limit ? `, and the oldest ones over ${limit}` : ""
        }`;
  const folder = info?.folder ?? "";

  return (
    <div className="session bk-page">
      <header className="bk-head">
        <SessionIcon kind="backups" size={18} tile />
        <div className="bk-title">
          <h1>Backups</h1>
          <p className="muted">{summary}</p>
          {folder && (
            <p className="muted bk-where">
              <span>Kept in</span>
              <span className="bk-where-path" title={folder}>
                {ltr(folder)}
              </span>
              <button className="link" onClick={() => reveal(folder)} title="Show the backup folder in Explorer / Finder">
                Show folder
              </button>
            </p>
          )}
        </div>
        <span className="spacer" />
        <div className="bk-head-actions">
          <label className="bk-search" title="Find backups by file path, folder, tool or action">
            <Search size={13} aria-hidden />
            <input
              value={query}
              onChange={(e) => setQuery(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Escape" && query) {
                  e.stopPropagation();
                  setQuery("");
                }
              }}
              placeholder="Search files and actions"
              aria-label="Search backups"
              spellCheck={false}
            />
          </label>
          <button onClick={() => void load()} title="Read the backup folder again">
            <RefreshCw size={14} /> Refresh
          </button>
          <button onClick={cleanUp} disabled={cleaning} title={cleanUpTitle}>
            <BrushCleaning size={14} /> Clean up now
          </button>
          {onHelp && (
            <button className="icon-btn" onClick={onHelp} title="Help: Backups" aria-label="Help">
              <CircleQuestionMark size={15} />
            </button>
          )}
        </div>
      </header>

      {!prefs.enabled && (
        <div className="bk-off">
          <ShieldOff size={14} />
          <span>
            <b>Backups are off</b> — new changes aren't being kept.
            {backups?.length ? " Existing backups are below." : ""}
          </span>
          <button className="primary" onClick={turnOn}>
            Turn on
          </button>
        </div>
      )}
      {error && (
        <div className="banner error bk-error">
          <TriangleAlert size={14} /> {error}
          <span className="spacer" />
          <button onClick={() => void load()}>
            <RefreshCw size={13} /> Retry
          </button>
        </div>
      )}

      <div className="bk-list" ref={listRef}>
        {backups === null ? (
          <div className="empty-state">Loading…</div>
        ) : backups.length === 0 ? (
          !error && (
            <div className="bk-empty">
              <ArchiveRestore size={34} strokeWidth={1.5} />
              <h2>No backups yet</h2>
              <p>
                Before PCompare overwrites or removes a file — copying in Folder Compare, saving an edited side,
                merging, syncing or applying a patch — it keeps the old version here first, one backup per operation,
                so you can put it back where it was.
              </p>
            </div>
          )
        ) : shown.length === 0 ? (
          <div className="empty-state">No backups match “{query.trim()}”.</div>
        ) : (
          groups.map((g) => (
            <section key={g.day} className="bk-day">
              <h2 className="bk-day-head">{dayLabel(g.day, today)}</h2>
              {g.rows.map(({ backup, items }) => (
                <BackupRow
                  key={backup.id}
                  backup={backup}
                  items={items}
                  expanded={expanded.has(backup.id)}
                  flash={flash === backup.id}
                  onToggle={toggle}
                  onRestore={askRestore}
                  onDelete={askDelete}
                  onCompare={compare}
                  onShow={reveal}
                />
              ))}
            </section>
          ))
        )}
      </div>

      {notice && (
        <div className={`bk-toast${notice.error ? " error" : ""}`} role="status">
          {notice.error && <TriangleAlert size={14} />}
          <span>{notice.text}</span>
          {notice.undo && (
            <button onClick={() => undo(notice.undo!)} title="Put back what the restore replaced">
              <Undo2 size={13} /> Undo
            </button>
          )}
          <button className="icon-btn bk-toast-close" onClick={() => setNotice(null)} title="Dismiss" aria-label="Dismiss">
            <X size={13} />
          </button>
        </div>
      )}

      {pending?.kind === "restore" && (
        <RestoreDialog
          items={pending.items}
          busy={!!pending.busy}
          error={pending.error}
          onRestore={() => restore(pending.backup, pending.items)}
          onClose={() => !pending.busy && setPending(null)}
        />
      )}
      {pending?.kind === "delete" && (
        <Modal
          title="Delete backup"
          onClose={() => !pending.busy && setPending(null)}
          footer={
            <>
              <button onClick={() => setPending(null)} disabled={pending.busy}>
                Cancel
              </button>
              <button className="danger" onClick={() => remove(pending.backup)} disabled={pending.busy}>
                {pending.busy ? "Deleting…" : "Delete"}
              </button>
            </>
          }
        >
          <p className="bk-dialog-text">
            Delete the backup from <b>{formatTime(pending.backup.timeMs)}</b> ({pending.backup.tool}:{" "}
            {pending.backup.action})? Its {countItems(pending.backup.items)} ({formatSize(pending.backup.size)}){" "}
            {pending.backup.items.length === 1 ? "is" : "are"} removed for good.
          </p>
          {pending.error && (
            <div className="banner error bk-dialog-error">
              <TriangleAlert size={14} /> {pending.error}
            </div>
          )}
        </Modal>
      )}
      {pending?.kind === "result" && (
        <RestoreResultDialog
          report={pending.report}
          undoing={pending.undoing}
          onUndo={undo}
          onClose={() => setPending(null)}
        />
      )}
    </div>
  );
}

interface RowProps {
  backup: Backup;
  /** Its items, or only those matching the search. */
  items: BackupItem[];
  expanded: boolean;
  flash: boolean;
  onToggle: (id: string) => void;
  onRestore: (backup: Backup, items: BackupItem[]) => void;
  onDelete: (backup: Backup) => void;
  onCompare: (backup: Backup, item: BackupItem) => void;
  onShow: (path: string) => void;
}

const BackupRow = memo(function BackupRow(p: RowProps) {
  const { backup: b, items } = p;
  const toggle = () => p.onToggle(b.id);
  // The compared sides, or for a save the folder of what it kept.
  const sides = [b.left, b.right].filter(Boolean);
  const where = sides.length ? sides : b.items.length ? [parentOf(b.items[0].original)].filter(Boolean) : [];

  return (
    <div className={`bk-backup${p.expanded ? " open" : ""}${p.flash ? " flash" : ""}`} data-backup={b.id}>
      <div className="bk-row">
        <div
          className="bk-row-main"
          role="button"
          tabIndex={0}
          aria-expanded={p.expanded}
          onClick={toggle}
          onKeyDown={(e) => {
            if (e.key === "Enter" || e.key === " ") {
              e.preventDefault();
              toggle();
            }
          }}
        >
          <span className="bk-chevron">{p.expanded ? <ChevronDown size={14} /> : <ChevronRight size={14} />}</span>
          <span className="bk-time" title={formatTime(b.timeMs)}>
            {timeFmt.format(b.timeMs)}
          </span>
          <span className="bk-what">
            <span className="bk-action" title={b.action}>
              {b.action}
            </span>
            <span className="muted bk-sub">{b.tool}</span>
          </span>
          <span className="bk-sides" title={where.join("\n")}>
            {where.length === 0 ? (
              <span className="muted">—</span>
            ) : (
              where.map((path, i) => (
                <span key={i} className="bk-side">
                  {i > 0 && <span className="muted bk-arrow">↔</span>}
                  <span className="bk-path">{basename(path) || path}</span>
                </span>
              ))
            )}
          </span>
          <span className="bk-count">
            <span>
              {countItems(b.items)} · {formatSize(b.size)}
            </span>
            {items.length < b.items.length && (
              <span className="muted bk-sub">{items.length.toLocaleString()} matching</span>
            )}
          </span>
        </div>
        <div className="bk-row-actions">
          <button onClick={() => p.onRestore(b, b.items)} title="Put every file of this backup back where it was">
            <RotateCcw size={13} /> Restore all
          </button>
          <button onClick={() => p.onDelete(b)} title="Delete this backup for good">
            <Trash size={13} /> Delete
          </button>
        </div>
      </div>
      {p.expanded && (
        <BackupItems backup={b} items={items} onRestore={p.onRestore} onCompare={p.onCompare} onShow={p.onShow} />
      )}
    </div>
  );
});

function BackupItems({
  backup: b,
  items,
  onRestore,
  onCompare,
  onShow,
}: Pick<RowProps, "backup" | "items" | "onRestore" | "onCompare" | "onShow">) {
  const [limit, setLimit] = useState(ITEMS_AT_ONCE);
  const rest = items.length - limit;
  return (
    <div className="bk-items">
      {(rest > 0 ? items.slice(0, limit) : items).map((item) => {
        const name = basename(item.original) || item.original;
        return (
          <div key={item.stored} className="bk-item">
            <span className="bk-item-icon">
              {item.isDir ? <Folder size={14} className="ic-folder" /> : <File size={14} />}
            </span>
            <span className="bk-item-name" title={item.original}>
              <b>{name}</b>
              <span className="muted bk-parent">{ltr(parentOf(item.original))}</span>
            </span>
            <span
              className={`bk-badge ${item.reason}`}
              title={
                item.reason === "removed"
                  ? "Deleted by a sync or merge"
                  : "Something new was written in its place"
              }
            >
              {item.reason}
            </span>
            <span className="bk-item-size">{formatSize(item.size)}</span>
            <span className="muted bk-item-time" title="When the kept version was last modified">
              {formatTime(item.modifiedMs)}
            </span>
            <span className="bk-item-actions">
              <button onClick={() => onRestore(b, [item])} title={`Put this version of ${name} back where it was`}>
                <RotateCcw size={13} /> Restore
              </button>
              {!item.isDir && (
                <button
                  onClick={() => onCompare(b, item)}
                  title="Compare the kept version (left) with the file as it is now (right)"
                >
                  <GitCompareArrows size={13} /> Compare with current
                </button>
              )}
              <button onClick={() => onShow(storedPath(b, item))} title="Show the kept copy in Explorer / Finder">
                <FolderOpen size={13} /> Show
              </button>
            </span>
          </div>
        );
      })}
      {rest > 0 && (
        <div className="bk-more">
          <span className="muted">{plural(rest, "more file")}</span>
          <button className="link" onClick={() => setLimit((n) => n + 1000)}>
            Show {Math.min(rest, 1000).toLocaleString()} more
          </button>
        </div>
      )}
    </div>
  );
}

function RestoreDialog({
  items,
  busy,
  error,
  onRestore,
  onClose,
}: {
  items: BackupItem[];
  busy: boolean;
  error?: string;
  onRestore: () => void;
  onClose: () => void;
}) {
  const one = items.length === 1;
  return (
    <Modal
      title={one ? "Restore file" : "Restore files"}
      onClose={onClose}
      wide
      footer={
        <>
          <button onClick={onClose} disabled={busy}>
            Cancel
          </button>
          <button className="primary" onClick={onRestore} disabled={busy}>
            {busy ? "Restoring…" : "Restore"}
          </button>
        </>
      }
    >
      <p className="bk-dialog-text">
        Restore {countItems(items)} to where {one ? "it was" : "they were"}?{" "}
        {one ? "The current version is backed up first" : "The current versions are backed up first"}, so you can undo
        this.
      </p>
      <ul className="bk-paths">
        {items.slice(0, CONFIRM_PATHS).map((i) => (
          <li key={i.stored} title={i.original}>
            {i.original}
          </li>
        ))}
        {items.length > CONFIRM_PATHS && (
          <li className="muted">… and {(items.length - CONFIRM_PATHS).toLocaleString()} more</li>
        )}
      </ul>
      {error && (
        <div className="banner error bk-dialog-error">
          <TriangleAlert size={14} /> {error}
        </div>
      )}
    </Modal>
  );
}

function RestoreResultDialog({
  report,
  undoing,
  onUndo,
  onClose,
}: {
  report: RestoreReport;
  undoing: boolean;
  onUndo: (id: string) => void;
  onClose: () => void;
}) {
  const failed = report.outcomes.filter((o) => o.error);
  const done = report.outcomes.length - failed.length;
  const undoId = !undoing && done > 0 ? report.undo : null;
  return (
    <Modal
      title={undoing ? "Undo finished with problems" : "Restore finished with problems"}
      onClose={onClose}
      wide
      footer={
        <>
          {undoId && (
            <button onClick={() => onUndo(undoId)} title="Put back what the restore replaced">
              <Undo2 size={13} /> Undo
            </button>
          )}
          <button className="primary" onClick={onClose}>
            Close
          </button>
        </>
      }
    >
      <p className="bk-dialog-text">
        {done.toLocaleString()} of {plural(report.outcomes.length, "item")} put back.{" "}
        {failed.length === 1 ? "This one couldn't be:" : "These couldn't be:"}
      </p>
      <ul className="bk-failures">
        {failed.map((o) => (
          <li key={o.stored}>
            <span className="bk-failure-path" title={o.original}>
              {o.original}
            </span>
            <span className="bk-failure-error">{o.error}</span>
          </li>
        ))}
      </ul>
    </Modal>
  );
}
