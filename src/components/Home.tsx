import { useState } from "react";
import type { SessionKind } from "../App";
import { isRemote, type SavedKind, type SavedSession } from "../lib/api";
import { formatTime } from "../lib/format";

interface Props {
  onNew: (kind: Exclude<SessionKind, "home">) => void;
  saved: SavedSession[];
  onOpenSaved: (s: SavedSession) => void;
  onRenameSaved: (s: SavedSession, name: string) => void;
  onDeleteSaved: (s: SavedSession) => void;
}

const SESSIONS: { kind: SavedKind; icon: string; title: string; text: string }[] = [
  { kind: "folder", icon: "🗂️", title: "Folder Compare", text: "Compare two folder trees by size, timestamp or content." },
  { kind: "text", icon: "📝", title: "Text Compare", text: "Side-by-side line diff with word-level highlights." },
  { kind: "hex", icon: "🔢", title: "Hex Compare", text: "Byte-level comparison of binary files of any size." },
];

const KIND_ICON: Record<SavedKind, string> = { folder: "🗂️", text: "📝", hex: "🔢" };

const COMING = ["Folder Sync", "Text Merge (3-way)", "Table / CSV Compare", "Image Compare", "Archive Compare", "Git integration"];

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
      <span className="saved-icon">{KIND_ICON[s.kind]}</span>
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
          <span title={s.left}>{isRemote(s.left) ? "🌐 " : ""}{s.left}</span>
          <span>↔</span>
          <span title={s.right}>{isRemote(s.right) ? "🌐 " : ""}{s.right}</span>
        </div>
      </div>
      <span className="muted saved-time">{formatTime(s.modifiedMs)}</span>
      <div className="saved-actions">
        <button onClick={() => setRenaming(true)} title="Rename">
          ✎
        </button>
        {confirm ? (
          <button className="danger" onClick={onDelete} onBlur={() => setConfirm(false)} autoFocus>
            Delete?
          </button>
        ) : (
          <button onClick={() => setConfirm(true)} title="Delete">
            🗑
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
        {SESSIONS.map((s) => (
          <button key={s.kind} className="card" onClick={() => onNew(s.kind)}>
            <span className="card-icon">{s.icon}</span>
            <span className="card-title">{s.title}</span>
            <span className="card-text">{s.text}</span>
          </button>
        ))}
        <button className="card" onClick={() => onNew("profiles")}>
          <span className="card-icon">🌐</span>
          <span className="card-title">Connection Profiles</span>
          <span className="card-text">Connect S3, OneDrive, Dropbox, Google Drive, FTP, SFTP, WebDAV and more.</span>
        </button>
      </div>

      <div className="saved-head">
        <h2>Saved sessions</h2>
        {saved.length > 5 && (
          <input className="saved-search" placeholder="Filter…" value={query} onChange={(e) => setQuery(e.target.value)} />
        )}
      </div>
      {saved.length === 0 ? (
        <p className="muted">
          No saved sessions yet. Open a comparison and press <kbd>Ctrl</kbd>+<kbd>S</kbd> to save it with a name.
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
