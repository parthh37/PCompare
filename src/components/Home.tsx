import { Camera, DatabaseZap, FileCog, FileInput, Globe, Layers, Lightbulb, Package, Pencil, Search, Trash, X } from "lucide-react";
import { useEffect, useRef, useState, type ReactNode } from "react";
import { isRemote, type SavedSession } from "../lib/api";
import { formatTime, formatWhen } from "../lib/format";
import { shortcutLabel } from "../lib/keymap";
import { usePref } from "../lib/prefs";
import { HOME_GROUPS, SESSIONS, type SessionKind } from "../lib/sessions";
import type { RecentSession, Workspace } from "../lib/workspaceApi";
import "./AppShell.css";
import "./Home.css";
import { SessionIcon } from "./SessionIcon";
import { FileFormatsDialog } from "./FileFormatsDialog";
import { SnapshotDialog } from "./SnapshotDialog";
import { GettingStarted } from "./GettingStarted";
import { setPrefs } from "../lib/prefs";
import { ImportSessionsDialog } from "./ImportSessionsDialog";
import { ImportConnectionsDialog } from "./ImportConnectionsDialog";
import { QuickCompare } from "./QuickCompare";

interface Props {
  onNew: (kind: Exclude<SessionKind, "home">) => void;
  /** Quick Compare: open two paths in whichever viewer suits them. */
  onCompare: (left: string, right: string) => void;
  saved: SavedSession[];
  onOpenSaved: (s: SavedSession) => void;
  onRenameSaved: (s: SavedSession, name: string) => void;
  onDeleteSaved: (s: SavedSession) => void;
  /** After "Import sessions…" saved `count` sessions from another tool. */
  onImported: (count: number) => void;
  /** After "Import database connections…" saved `count` connections. */
  onImportedConnections: (count: number) => void;
  workspaces: Workspace[];
  onOpenWorkspace: (w: Workspace) => void;
  onRenameWorkspace: (w: Workspace, name: string) => void;
  onDeleteWorkspace: (w: Workspace) => void;
  recent: RecentSession[];
  onOpenRecent: (r: RecentSession) => void;
  onRemoveRecent: (r: RecentSession) => void;
  onClearRecent: () => void;
}

/** Remote paths get a small globe so cloud locations stand out in the list. */
function PathText({ path }: { path: string }) {
  return (
    <span title={path}>
      {isRemote(path) && <Globe size={11} className="inline-icon" />}
      {path}
    </span>
  );
}

function Paths({ left, right }: { left: string; right: string }) {
  return (
    <div className="saved-paths muted">
      <PathText path={left || "—"} />
      <span>↔</span>
      <PathText path={right || "—"} />
    </div>
  );
}

/** One row of a Home list: open on click/double-click, optional rename, delete with confirmation. */
function ListRow({
  icon,
  name,
  detail,
  time,
  onOpen,
  onRename,
  onDelete,
  deleteLabel = "Delete",
}: {
  icon: ReactNode;
  name: string;
  detail: ReactNode;
  time: number;
  onOpen: () => void;
  onRename?: (name: string) => void;
  onDelete: () => void;
  deleteLabel?: string;
}) {
  const [renaming, setRenaming] = useState(false);
  const [draft, setDraft] = useState(name);
  const [confirm, setConfirm] = useState(false);

  const commit = () => {
    setRenaming(false);
    if (draft.trim() && draft !== name) onRename?.(draft.trim());
    else setDraft(name);
  };

  return (
    <div className="saved-row" onDoubleClick={() => !renaming && onOpen()}>
      {icon}
      <div className="saved-main">
        {renaming ? (
          <input
            autoFocus
            value={draft}
            onChange={(e) => setDraft(e.target.value)}
            onBlur={commit}
            onKeyDown={(e) => {
              if (e.key === "Enter") commit();
              if (e.key === "Escape") {
                setDraft(name);
                setRenaming(false);
              }
            }}
          />
        ) : (
          <button className="link saved-name" onClick={onOpen}>
            {name}
          </button>
        )}
        {detail}
      </div>
      <span className="muted saved-time" title={formatTime(time)}>
        {formatWhen(time)}
      </span>
      <div className="saved-actions">
        {onRename && (
          <button
            className="icon-btn"
            onClick={() => {
              setDraft(name);
              setRenaming(true);
            }}
            title="Rename"
            aria-label="Rename"
          >
            <Pencil size={13} />
          </button>
        )}
        {confirm ? (
          <button className="danger" onClick={onDelete} onBlur={() => setConfirm(false)} autoFocus>
            {deleteLabel}?
          </button>
        ) : (
          <button className="icon-btn" onClick={() => setConfirm(true)} title={deleteLabel} aria-label={deleteLabel}>
            <Trash size={13} />
          </button>
        )}
      </div>
    </div>
  );
}

/** The kinds in a workspace as small icons, then a count. */
function WorkspaceDetail({ w }: { w: Workspace }) {
  const kinds = w.tabs.map((t) => t.kind).filter((k) => k !== "home");
  return (
    <div className="saved-paths muted ws-detail">
      <span className="ws-kinds">
        {kinds.slice(0, 12).map((k, i) => (
          <SessionIcon key={i} kind={k} size={11} />
        ))}
      </span>
      <span>
        {w.tabs.length} tab{w.tabs.length === 1 ? "" : "s"}
      </span>
    </div>
  );
}

export function Home(props: Props) {
  const { onNew, saved, workspaces, recent } = props;
  const autoSave = usePref("autoSaveSessions");
  const [query, setQuery] = useState("");
  const [fileFormats, setFileFormats] = useState(false);
  const [snapshotting, setSnapshotting] = useState(false);
  const [importing, setImporting] = useState(false);
  const [importingConnections, setImportingConnections] = useState(false);
  // the Getting started overview, once at first run and on request.
  const overviewDismissed = usePref("gettingStartedDismissed");
  const [overview, setOverview] = useState(false);
  // Only the Home tab in front shows it (inactive tabs stay mounted, hidden).
  const homeRef = useRef<HTMLDivElement>(null);
  const [visible, setVisible] = useState(false);
  useEffect(() => {
    const el = homeRef.current;
    if (!el || typeof IntersectionObserver === "undefined") return;
    const io = new IntersectionObserver(([e]) => setVisible(e.isIntersecting));
    io.observe(el);
    return () => io.disconnect();
  }, []);
  const closeOverview = () => {
    setOverview(false);
    if (!overviewDismissed) setPrefs({ gettingStartedDismissed: true });
  };
  const q = query.trim().toLowerCase();
  const hit = (...values: (string | undefined)[]) => !q || values.some((v) => v?.toLowerCase().includes(q));
  const shownSaved = saved.filter((s) => hit(s.name, s.left, s.right));
  const shownWorkspaces = workspaces.filter((w) => hit(w.name, ...w.tabs.flatMap((t) => [t.left, t.right])));
  const shownRecent = recent.filter((r) => hit(r.left, r.right, SESSIONS[r.kind].label));
  const total = saved.length + workspaces.length + recent.length;

  return (
    <div className="home" ref={homeRef}>
      <header className="home-head">
        <div>
          <h1>PCompare</h1>
          <p className="muted">Compare, merge and sync files, folders and data.</p>
        </div>
        {/* first-run overview, reopenable */}
        <button className="home-head-btn" onClick={() => setOverview(true)}>
          <Lightbulb size={14} /> Getting started
        </button>
        {(overview || (!overviewDismissed && visible)) && <GettingStarted onClose={closeOverview} />}
      </header>

      <QuickCompare onCompare={props.onCompare} />

      <div className="home-cols">
        <div className="home-main">
          {total === 0 ? (
            <section className="home-empty">
              <h2>Your comparisons</h2>
              <p>
                Comparisons you open are listed here, so you can pick up where you left off. Save one with a name using{" "}
                <kbd>{shortcutLabel("app.saveSession") || "Save session"}</kbd> to keep it for good.
              </p>
              <p>
                Coming from another compare or sync tool?{" "}
                <button className="link" onClick={() => setImporting(true)}>
                  Import its saved sessions…
                </button>
              </p>
            </section>
          ) : (
            total > 6 && (
              <div className="home-filter">
                <Search size={14} className="home-filter-icon" aria-hidden />
                <input
                  className="saved-search"
                  placeholder="Filter workspaces and sessions…"
                  value={query}
                  onChange={(e) => setQuery(e.target.value)}
                />
              </div>
            )
          )}
          {importing && (
            <ImportSessionsDialog saved={saved} onImported={props.onImported} onClose={() => setImporting(false)} />
          )}
          {total > 0 && <HomeLists {...props} {...{ shownSaved, shownWorkspaces, shownRecent, autoSave }} onImport={() => setImporting(true)} />}
        </div>

        <aside className="home-side" aria-label="New session">
          <h2>New session</h2>
          {HOME_GROUPS.map((g) => (
            <div key={g.title} className="kind-group">
              <div className="kind-group-title">{g.title}</div>
              {g.kinds.map((kind) => {
                const info = SESSIONS[kind];
                return (
                  <button key={kind} className="kind-row" onClick={() => onNew(kind)} title={info.description}>
                    <SessionIcon kind={kind} size={14} tile />
                    <span className="kind-text">
                      <span className="kind-label">
                        {info.label}
                        {info.windowsOnly && <span className="badge">Windows</span>}
                      </span>
                      <span className="kind-desc">{info.description}</span>
                    </span>
                  </button>
                );
              })}
            </div>
          ))}
          <div className="kind-group">
            <div className="kind-group-title">More</div>
            {/* review a saved comparison (.pcmpz) read-only */}
            <button className="more-row" onClick={() => onNew("review")}>
              <Package size={14} aria-hidden /> Open comparison archive…
            </button>
            {/* snapshots of a folder to compare against later */}
            <button className="more-row" onClick={() => setSnapshotting(true)}>
              <Camera size={14} aria-hidden /> Save folder snapshot…
            </button>
            {/* file format rules (document converters) and external tools */}
            <button className="more-row" onClick={() => setFileFormats(true)}>
              <FileCog size={14} aria-hidden /> File formats and external tools…
            </button>
            <button className="more-row" onClick={() => setImporting(true)}>
              <FileInput size={14} aria-hidden /> Import sessions from other tools…
            </button>
            <button className="more-row" onClick={() => setImportingConnections(true)}>
              <DatabaseZap size={14} aria-hidden /> Import database connections…
            </button>
          </div>
          {importingConnections && (
            <ImportConnectionsDialog
              onImported={(added) => props.onImportedConnections(added.length)}
              onClose={() => setImportingConnections(false)}
            />
          )}
          {fileFormats && <FileFormatsDialog onClose={() => setFileFormats(false)} />}
          {snapshotting && <SnapshotDialog folders={[]} onClose={() => setSnapshotting(false)} />}
        </aside>
      </div>
    </div>
  );
}

/** Home's lists of the user's own work: workspaces, saved sessions and recent comparisons. */
function HomeLists(
  props: Props & {
    shownSaved: SavedSession[];
    shownWorkspaces: Workspace[];
    shownRecent: RecentSession[];
    autoSave: boolean;
    onImport: () => void;
  },
) {
  const { saved, workspaces, recent, shownSaved, shownWorkspaces, shownRecent, autoSave } = props;
  return (
    <>
      {workspaces.length > 0 && (
        <>
          <div className="saved-head">
            <h2>Workspaces</h2>
          </div>
          <div className="saved-list">
            {shownWorkspaces.map((w) => (
              <ListRow
                key={w.id}
                icon={
                  <span className="session-tile ws-tile">
                    <Layers size={14} aria-hidden />
                  </span>
                }
                name={w.name}
                detail={<WorkspaceDetail w={w} />}
                time={w.modifiedMs}
                onOpen={() => props.onOpenWorkspace(w)}
                onRename={(name) => props.onRenameWorkspace(w, name)}
                onDelete={() => props.onDeleteWorkspace(w)}
              />
            ))}
          </div>
        </>
      )}

      <div className="saved-head">
        <h2>Saved sessions</h2>
        <button className="link muted" onClick={props.onImport} title="Import sessions from other compare and sync tools">
          <FileInput size={12} /> Import…
        </button>
      </div>
      {saved.length === 0 ? (
        <p className="muted home-note">
          No saved sessions yet. Open a comparison and press <kbd>{shortcutLabel("app.saveSession") || "Save session"}</kbd>{" "}
          to save it with a name.
        </p>
      ) : (
        <div className="saved-list">
          {shownSaved.map((s) => (
            <ListRow
              key={s.id}
              icon={<SessionIcon kind={s.kind} size={14} tile />}
              name={s.name}
              detail={<Paths left={s.left} right={s.right} />}
              time={s.modifiedMs}
              onOpen={() => props.onOpenSaved(s)}
              onRename={(name) => props.onRenameSaved(s, name)}
              onDelete={() => props.onDeleteSaved(s)}
            />
          ))}
        </div>
      )}

      {(recent.length > 0 || autoSave) && (
        <>
          <div className="saved-head">
            <h2>Recent sessions</h2>
            {recent.length > 0 && (
              <button className="link muted" onClick={props.onClearRecent} title="Forget all recent sessions">
                <X size={12} /> Clear
              </button>
            )}
          </div>
          {recent.length === 0 ? (
            <p className="muted home-note">Comparisons you close without saving are kept here automatically.</p>
          ) : (
            <div className="saved-list">
              {shownRecent.map((r) => (
                <ListRow
                  key={r.id}
                  icon={<SessionIcon kind={r.kind} size={14} tile />}
                  name={SESSIONS[r.kind].label}
                  detail={<Paths left={r.left} right={r.right} />}
                  time={r.closedMs}
                  onOpen={() => props.onOpenRecent(r)}
                  onDelete={() => props.onRemoveRecent(r)}
                  deleteLabel="Remove"
                />
              ))}
            </div>
          )}
        </>
      )}
    </>
  );
}
