import { Globe, Pencil, Trash } from "lucide-react";
import { useState } from "react";
import { isRemote, type SavedSession } from "../lib/api";
import { formatTime } from "../lib/format";
import { COMPARE_KINDS, SESSIONS, type SessionKind } from "../lib/sessions";
import { SessionIcon } from "./SessionIcon";

interface Props {
  onNew: (kind: Exclude<SessionKind, "home">) => void;
  saved: SavedSession[];
  onOpenSaved: (s: SavedSession) => void;
  onRenameSaved: (s: SavedSession, name: string) => void;
  onDeleteSaved: (s: SavedSession) => void;
}

const COMING = ["Archive Compare", "Git integration", "Structural JSON / XML diff", "Folder reports"];

/** Remote paths get a small globe so cloud locations stand out in the list. */
function PathText({ path }: { path: string }) {
  return (
    <span title={path}>
      {isRemote(path) && <Globe size={11} className="inline-icon" />}
      {path}
    </span>
  );
}

function SavedRow({ s, onOpen, onRename, onDelete }: {
  s: SavedSession;
  onOpen: () => void;
  onRename: (name: string) => void;
  onDelete: () => void;
}) {
  const [renaming, setRenaming] = useState(false);
  const [name, setName] = useState(s.name);
  const [confirm, setConfirm] = useState(false);

  const commit = () => {
    setRenaming(false);
    if (name.trim() && name !== s.name) onRename(name.trim());
    else setName(s.name);
  };

  return (
    <div className="saved-row" onDoubleClick={() => !renaming && onOpen()}>
      <SessionIcon kind={s.kind} size={14} tile />
      <div className="saved-main">
        {renaming ? (
          <input
            autoFocus
            value={name}
            onChange={(e) => setName(e.target.value)}
            onBlur={commit}
            onKeyDown={(e) => {
              if (e.key === "Enter") commit();
              if (e.key === "Escape") {
                setName(s.name);
                setRenaming(false);
              }
            }}
          />
        ) : (
          <button className="link saved-name" onClick={onOpen}>
            {s.name}
          </button>
        )}
        <div className="saved-paths muted">
          <PathText path={s.left} />
          <span>↔</span>
          <PathText path={s.right} />
        </div>
      </div>
      <span className="muted saved-time">{formatTime(s.modifiedMs)}</span>
      <div className="saved-actions">
        <button className="icon-btn" onClick={() => setRenaming(true)} title="Rename" aria-label="Rename">
          <Pencil size={13} />
        </button>
        {confirm ? (
          <button className="danger" onClick={onDelete} onBlur={() => setConfirm(false)} autoFocus>
            Delete?
          </button>
        ) : (
          <button className="icon-btn" onClick={() => setConfirm(true)} title="Delete" aria-label="Delete">
            <Trash size={13} />
          </button>
        )}
      </div>
    </div>
  );
}

export function Home({ onNew, saved, onOpenSaved, onRenameSaved, onDeleteSaved }: Props) {
  const [query, setQuery] = useState("");
  const q = query.trim().toLowerCase();
  const shown = q
    ? saved.filter((s) => [s.name, s.left, s.right].some((v) => v.toLowerCase().includes(q)))
    : saved;

  return (
    <div className="home">
      <header>
        <h1>PCompare</h1>
        <p className="muted">Choose a session, or drop two files or folders anywhere in this window.</p>
      </header>
      <div className="cards">
        {[...COMPARE_KINDS, "profiles" as const].map((kind) => {
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

      <div className="saved-head">
        <h2>Saved sessions</h2>
        {saved.length > 5 && (
          <input className="saved-search" placeholder="Filter…" value={query} onChange={(e) => setQuery(e.target.value)} />
        )}
      </div>
      {saved.length === 0 ? (
        <p className="muted">
          No saved sessions yet. Open a comparison and press <kbd>Ctrl</kbd>+<kbd>Shift</kbd>+<kbd>S</kbd> to save it with a name.
        </p>
      ) : (
        <div className="saved-list">
          {shown.map((s) => (
            <SavedRow
              key={s.id}
              s={s}
              onOpen={() => onOpenSaved(s)}
              onRename={(name) => onRenameSaved(s, name)}
              onDelete={() => onDeleteSaved(s)}
            />
          ))}
        </div>
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
