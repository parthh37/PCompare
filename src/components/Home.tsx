import { Globe, Layers, Pencil, Trash, X } from "lucide-react";
import { useEffect, useRef, useState, type ReactNode } from "react";
import { isRemote, type SavedSession } from "../lib/api";
import { formatTime } from "../lib/format";
import { shortcutLabel } from "../lib/keymap";
import { usePref } from "../lib/prefs";
import { COMPARE_KINDS, SESSIONS, type SessionKind } from "../lib/sessions";
import type { RecentSession, Workspace } from "../lib/workspaceApi";
import "./AppShell.css";
import { SessionIcon } from "./SessionIcon";
import { FileFormatsDialog } from "./FileFormatsDialog"; // bc5:converters
import { SnapshotDialog } from "./SnapshotDialog"; // bc5:archives
import { GettingStarted } from "./GettingStarted"; // araxis:vcs
import { setPrefs } from "../lib/prefs"; // araxis:vcs

interface Props {
  onNew: (kind: Exclude<SessionKind, "home">) => void;
  saved: SavedSession[];
  onOpenSaved: (s: SavedSession) => void;
  onRenameSaved: (s: SavedSession, name: string) => void;
  onDeleteSaved: (s: SavedSession) => void;
  workspaces: Workspace[];
  onOpenWorkspace: (w: Workspace) => void;
  onRenameWorkspace: (w: Workspace, name: string) => void;
  onDeleteWorkspace: (w: Workspace) => void;
  recent: RecentSession[];
  onOpenRecent: (r: RecentSession) => void;
  onRemoveRecent: (r: RecentSession) => void;
  onClearRecent: () => void;
}

// bc5:archives — "Archive Compare" shipped: archives open as folders.
// araxis:vcs — "Git integration" shipped: git:/hg: sides, difftool / mergetool setup.
const COMING = ["Structural JSON / XML diff"];

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
      <span className="muted saved-time">{formatTime(time)}</span>
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
  const [fileFormats, setFileFormats] = useState(false); // bc5:converters
  const [snapshotting, setSnapshotting] = useState(false); // bc5:archives
  // araxis:vcs — the Getting started overview, once at first run and on request.
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
  // araxis:vcs end
  const q = query.trim().toLowerCase();
  const hit = (...values: (string | undefined)[]) => !q || values.some((v) => v?.toLowerCase().includes(q));
  const shownSaved = saved.filter((s) => hit(s.name, s.left, s.right));
  const shownWorkspaces = workspaces.filter((w) => hit(w.name, ...w.tabs.flatMap((t) => [t.left, t.right])));
  const shownRecent = recent.filter((r) => hit(r.left, r.right, SESSIONS[r.kind].label));
  const total = saved.length + workspaces.length + recent.length;

  return (
    <div className="home" ref={homeRef /* araxis:vcs */}>
      <header>
        <h1>PCompare</h1>
        <p className="muted">Choose a session, or drop two files or folders anywhere in this window.</p>
        {/* bc5:converters — file format rules (document converters) and external tools */}
        <p className="muted">
          <button className="link" onClick={() => setFileFormats(true)}>
            File formats and external tools…
          </button>
        </p>
        {fileFormats && <FileFormatsDialog onClose={() => setFileFormats(false)} />}
        {/* bc5:archives — snapshots of a folder to compare against later */}
        <p className="muted">
          <button className="link" onClick={() => setSnapshotting(true)}>
            Save folder snapshot…
          </button>
        </p>
        {snapshotting && <SnapshotDialog folders={[]} onClose={() => setSnapshotting(false)} />}
        {/* araxis:vcs — first-run overview, reopenable */}
        <p className="muted">
          <button className="link" onClick={() => setOverview(true)}>
            Getting started overview…
          </button>
        </p>
        {(overview || (!overviewDismissed && visible)) && <GettingStarted onClose={closeOverview} />}
        {/* araxis:vcs end */}
        {/* araxis:text-review — review a saved comparison (.pcmpz) read-only */}
        <p className="muted">
          <button className="link" onClick={() => onNew("review")}>
            Open comparison archive…
          </button>
        </p>
        {/* araxis:text-review end */}
      </header>
      <div className="cards">
        {[...COMPARE_KINDS, "profiles" as const, "automation" as const /* automation:ui */].map((kind) => {
          const info = SESSIONS[kind];
          return (
            <button key={kind} className="card" onClick={() => onNew(kind)}>
              <SessionIcon kind={kind} size={18} tile />
              <span className="card-title">
                {info.label}
                {info.windowsOnly && <span className="badge">Windows</span>}
              </span>
              <span className="card-text">{info.description}</span>
            </button>
          );
        })}
      </div>

      {total > 6 && (
        <div className="home-filter">
          <input
            className="saved-search"
            placeholder="Filter workspaces and sessions…"
            value={query}
            onChange={(e) => setQuery(e.target.value)}
          />
        </div>
      )}

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
      </div>
      {saved.length === 0 ? (
        <p className="muted">
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
            <p className="muted">Comparisons you close without saving are kept here automatically.</p>
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

      <h2>On the roadmap</h2>
      <div className="chips">
        {COMING.map((c) => (
          <span key={c} className="chip">
            {c}
          </span>
        ))}
      </div>
    </div>
  );
}
