import { Search, X } from "lucide-react";
import { Fragment, useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { HELP_TOPICS, type HelpTopic } from "../help/topics";
import { bindingFor, COMMANDS, formatBinding, type CommandDef } from "../lib/keymap";
import { usePref } from "../lib/prefs";
import "./HelpPanel.css";

interface Props {
  /** Page to show when the panel opens (F1 passes the active view's page). */
  topicId: string;
  onClose: () => void;
}

/** `code` and **bold** inside a line of help text. */
function inline(text: string): ReactNode[] {
  return text.split(/(`[^`]+`|\*\*[^*]+\*\*|\*[^*]+\*)/).map((part, i) => {
    if (part.startsWith("`") && part.endsWith("`") && part.length > 1) return <code key={i}>{part.slice(1, -1)}</code>;
    if (part.startsWith("**") && part.endsWith("**") && part.length > 3) return <b key={i}>{part.slice(2, -2)}</b>;
    if (part.startsWith("*") && part.endsWith("*") && part.length > 1) return <i key={i}>{part.slice(1, -1)}</i>;
    return <Fragment key={i}>{part}</Fragment>;
  });
}

/** Renders the small markdown subset used by help/topics.ts. */
function HelpBody({ body }: { body: string }) {
  const blocks: ReactNode[] = [];
  let list: string[] = [];
  const flush = () => {
    if (!list.length) return;
    const items = list;
    blocks.push(
      <ul key={blocks.length}>
        {items.map((li, i) => (
          <li key={i}>{inline(li)}</li>
        ))}
      </ul>,
    );
    list = [];
  };
  for (const raw of body.trim().split("\n")) {
    const line = raw.trim();
    if (line.startsWith("- ")) {
      list.push(line.slice(2));
      continue;
    }
    flush();
    if (!line) continue;
    if (line.startsWith("## ")) blocks.push(<h3 key={blocks.length}>{line.slice(3)}</h3>);
    else blocks.push(<p key={blocks.length}>{inline(line)}</p>);
  }
  flush();
  return <>{blocks}</>;
}

function ShortcutTable({ commands }: { commands: CommandDef[] }) {
  // Re-render when the user rebinds keys.
  usePref("keys");
  const rows = commands.filter((c) => bindingFor(c.id));
  if (!rows.length) return null;
  return (
    <>
      <h3>Keyboard shortcuts</h3>
      <table className="help-keys">
        <tbody>
          {rows.map((c) => (
            <tr key={c.id}>
              <td>{c.label}</td>
              <td>
                {formatBinding(bindingFor(c.id))
                  .split("+")
                  .map((k, i) => (
                    <kbd key={i}>{k || "+"}</kbd>
                  ))}
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </>
  );
}

const topicCommands = (t: HelpTopic) =>
  COMMANDS.filter((c) => c.scope.some((s) => (s === "global" ? t.globalKeys : t.kinds.includes(s))));

/** Plain text of a topic for searching (body + shortcut labels). */
const searchText = (t: HelpTopic) =>
  `${t.title}\n${t.body}\n${topicCommands(t)
    .map((c) => c.label)
    .join("\n")}`.toLowerCase();

/** A short excerpt around the first match, for search results. */
function snippet(t: HelpTopic, q: string): string {
  const text = t.body.replace(/[*`#]/g, "").replace(/\s+/g, " ").trim();
  const at = text.toLowerCase().indexOf(q);
  if (at < 0) return text.slice(0, 110) + (text.length > 110 ? "…" : "");
  const start = Math.max(0, at - 40);
  return (start ? "…" : "") + text.slice(start, at + q.length + 70) + "…";
}

/** Context-sensitive help drawer (F1 or the ? button in the tab bar). */
export function HelpPanel({ topicId, onClose }: Props) {
  const [current, setCurrent] = useState(topicId);
  const [query, setQuery] = useState("");
  const body = useRef<HTMLDivElement>(null);

  // F1 on another tab while the panel is open jumps to that tab's page.
  useEffect(() => {
    setCurrent(topicId);
    setQuery("");
  }, [topicId]);
  useEffect(() => body.current?.scrollTo(0, 0), [current]);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      // A dialog on top handles its own Escape.
      if (e.key !== "Escape" || document.querySelector(".modal-backdrop")) return;
      e.stopPropagation();
      onClose();
    };
    window.addEventListener("keydown", onKey, true);
    return () => window.removeEventListener("keydown", onKey, true);
  }, [onClose]);

  const index = useMemo(() => HELP_TOPICS.map((t) => [t, searchText(t)] as const), []);
  const q = query.trim().toLowerCase();
  const results = q ? index.filter(([, text]) => q.split(/\s+/).every((w) => text.includes(w))).map(([t]) => t) : [];
  const topic = HELP_TOPICS.find((t) => t.id === current) ?? HELP_TOPICS[0];

  return (
    <aside className="help-panel" aria-label="Help">
      <header className="help-head">
        <span className="help-search">
          <Search size={13} />
          <input
            placeholder="Search help…"
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter" && results[0]) {
                setCurrent(results[0].id);
                setQuery("");
              }
            }}
            spellCheck={false}
          />
        </span>
        <button className="tab-close" onClick={onClose} aria-label="Close help" title="Close (Esc)">
          <X size={14} />
        </button>
      </header>
      {q ? (
        <div className="help-results">
          {results.length === 0 && <p className="muted">No help topics match “{query.trim()}”.</p>}
          {results.map((t) => (
            <button
              key={t.id}
              className="help-result"
              onClick={() => {
                setCurrent(t.id);
                setQuery("");
              }}
            >
              <b>{t.title}</b>
              <span className="muted">{snippet(t, q.split(/\s+/)[0])}</span>
            </button>
          ))}
        </div>
      ) : (
        <>
          <nav className="help-topics">
            <select value={topic.id} onChange={(e) => setCurrent(e.target.value)} aria-label="Help topic">
              {HELP_TOPICS.map((t) => (
                <option key={t.id} value={t.id}>
                  {t.title}
                </option>
              ))}
            </select>
          </nav>
          <div className="help-body" ref={body}>
            <h2>{topic.title}</h2>
            <HelpBody body={topic.body} />
            <ShortcutTable commands={topicCommands(topic)} />
          </div>
        </>
      )}
    </aside>
  );
}
