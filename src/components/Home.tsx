import type { SessionKind } from "../App";

interface Props {
  onNew: (kind: Exclude<SessionKind, "home">) => void;
}

const SESSIONS: { kind: Exclude<SessionKind, "home">; icon: string; title: string; text: string }[] = [
  { kind: "folder", icon: "🗂️", title: "Folder Compare", text: "Compare two folder trees by size, timestamp or content." },
  { kind: "text", icon: "📝", title: "Text Compare", text: "Side-by-side line diff with word-level highlights." },
  { kind: "hex", icon: "🔢", title: "Hex Compare", text: "Byte-level comparison of binary files of any size." },
];

const COMING = ["Folder Sync", "Text Merge (3-way)", "Table / CSV Compare", "Image Compare", "Archive Compare", "Git integration"];

export function Home({ onNew }: Props) {
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
      </div>
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
