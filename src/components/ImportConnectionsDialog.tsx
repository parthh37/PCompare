// "Import connections…": finds the connection lists of database tools on this
// computer (SQL Server Management Studio, Azure Data Studio, VS Code, DBeaver
// and others), shows them as one list, and saves the chosen ones as PCompare
// connections. Passwords never come over from those tools; SQL logins can
// have theirs typed here, or later in the connection's Edit dialog.
import { open } from "@tauri-apps/plugin-dialog";
import { ClipboardPaste, DatabaseZap, FolderOpen, LoaderCircle, Search, TriangleAlert } from "lucide-react";
import { useEffect, useMemo, useState } from "react";
import { errorMessage } from "../lib/api";
import { connectionKey, dbApi, type DbConnection } from "../lib/dbApi";
import {
  CONNECTION_FILTERS,
  connectionImportApi,
  type ConnectionSource,
  type ImportedConnection,
} from "../lib/importApi";
import { describeConfig, EngineBadge } from "./DbBadge";
import { Modal } from "./Modal";
import { PasteConnectionDialog } from "./PasteConnectionDialog";
import "./DbCompare.css";
import "./ImportSessionsDialog.css";
import "./ImportConnectionsDialog.css";

interface Props {
  onImported: (saved: DbConnection[]) => void;
  onClose: () => void;
}

/** A source in the list; `error` = why it couldn't be read (and it lists nothing). */
type Loaded = ConnectionSource & { error?: string };

interface Row {
  /** `<source index>:<connection index>`, stable while the dialog is open. */
  key: string;
  source: number;
  conn: ImportedConnection;
  /** Already saved here, or listed earlier by another source. */
  duplicate: "saved" | "listed" | null;
}

/** A tool's folders become part of the name, since PCompare's connection list is flat. */
const fullName = (c: ImportedConnection) => (c.group ? `${c.group} / ${c.name}` : c.name);

const needsPassword = (c: ImportedConnection) => c.config.engine !== "sqlite" && c.config.auth === "password";

export function ImportConnectionsDialog({ onImported, onClose }: Props) {
  const [saved, setSaved] = useState<DbConnection[]>([]);
  const [sources, setSources] = useState<Loaded[] | null>(null);
  const [chosen, setChosen] = useState<Set<string>>(new Set());
  /** Passwords typed for rows, by row key. */
  const [passwords, setPasswords] = useState<Record<string, string>>({});
  /** Logins typed for rows that came without one (DBeaver keeps them with the password). */
  const [logins, setLogins] = useState<Record<string, string>>({});
  const [filter, setFilter] = useState("");
  const [pasting, setPasting] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  // Read every tool found on this computer straight away: one list, no picking files.
  useEffect(() => {
    let gone = false;
    (async () => {
      const [mine, found] = await Promise.all([
        dbApi.listConnections().catch(() => [] as DbConnection[]),
        connectionImportApi.discover().catch(() => []),
      ]);
      const read = await Promise.all(
        found.map((c) =>
          connectionImportApi
            .read(c.path)
            .then((s): Loaded => ({ ...s, label: c.label }))
            .catch((e): Loaded => ({
              label: c.label,
              path: c.path,
              connections: [],
              skipped: [],
              error: errorMessage(e),
            })),
        ),
      );
      if (gone) return;
      setSaved(mine);
      addSources(read, mine, []);
    })();
    return () => void (gone = true);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const rows = useMemo(() => buildRows(sources ?? [], saved), [sources, saved]);

  /** Appends sources and ticks their new, non-duplicate connections. */
  function addSources(added: Loaded[], mine: DbConnection[], before: Loaded[]) {
    const all = [...before, ...added];
    setSources(all);
    const fresh = buildRows(all, mine).filter((r) => r.source >= before.length && !r.duplicate);
    setChosen((c) => new Set([...c, ...fresh.map((r) => r.key)]));
  }

  const loadFile = async () => {
    const path = await open({ multiple: false, directory: false, filters: CONNECTION_FILTERS });
    if (typeof path !== "string") return;
    if (sources?.some((s) => s.path === path)) return setError("That file is already in the list.");
    setBusy(true);
    setError(null);
    try {
      addSources([await connectionImportApi.read(path)], saved, sources ?? []);
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setBusy(false);
    }
  };

  const addPasted = (conn: ImportedConnection, password: string | null) => {
    const before = sources ?? [];
    const pasted: Loaded = { label: "Pasted connection string", path: "", connections: [conn], skipped: [] };
    addSources([pasted], saved, before);
    if (password) setPasswords((p) => ({ ...p, [`${before.length}:0`]: password }));
    setPasting(false);
  };

  const q = filter.trim().toLowerCase();
  const visible = rows.filter(
    (r) => !q || `${fullName(r.conn)} ${describeConfig(r.conn.config)} ${r.conn.config.user}`.toLowerCase().includes(q),
  );
  const selectable = visible.filter((r) => r.duplicate !== "saved").map((r) => r.key);
  const allChosen = selectable.length > 0 && selectable.every((k) => chosen.has(k));

  const toggle = (key: string) =>
    setChosen((c) => {
      const next = new Set(c);
      if (next.has(key)) next.delete(key);
      else next.add(key);
      return next;
    });

  const commit = async () => {
    setBusy(true);
    setError(null);
    try {
      const picked = rows.filter((r) => chosen.has(r.key));
      const done = await connectionImportApi.commit(
        picked.map((r) => ({
          name: fullName(r.conn),
          config: { ...r.conn.config, user: r.conn.config.user || (logins[r.key] ?? "").trim() },
          password: needsPassword(r.conn) ? passwords[r.key] || undefined : undefined,
        })),
      );
      onImported(done);
      onClose();
    } catch (e) {
      setError(errorMessage(e));
      setBusy(false);
    }
  };

  const total = rows.length;
  return (
    <>
      <Modal
        title="Import database connections"
        wide
        className="conn-import-modal"
        onClose={onClose}
        footer={
          <>
            <button onClick={loadFile} disabled={busy || sources === null}>
              <FolderOpen size={14} /> Choose a file…
            </button>
            <button onClick={() => setPasting(true)} disabled={busy || sources === null}>
              <ClipboardPaste size={14} /> Paste a connection string…
            </button>
            <span className="spacer" />
            <button onClick={onClose}>Cancel</button>
            <button className="primary" disabled={busy || chosen.size === 0} onClick={commit}>
              <DatabaseZap size={14} /> Import {chosen.size || ""} connection{chosen.size === 1 ? "" : "s"}
            </button>
          </>
        }
      >
        <div className="import-dialog conn-import">
          <p className="muted">
            Brings saved connections over from SQL Server Management Studio, Azure Data Studio, VS Code, DBeaver,
            DataGrip and other JetBrains IDEs, and MySQL Workbench. Passwords stay in those tools: type a login's
            password here, or later when you edit the connection.
          </p>

          {sources === null ? (
            <p className="muted conn-import-wait">
              <LoaderCircle size={14} className="db-spin" /> Looking for database tools on this computer…
            </p>
          ) : sources.length === 0 ? (
            <p className="muted">
              No database tool's connections were found in their usual places. Choose an exported file (for example a{" "}
              <code>.regsrvr</code> Registered Servers export or pgAdmin's <code>servers.json</code>) or paste a
              connection string.
            </p>
          ) : (
            <>
              {total > 8 && (
                <div className="conn-import-filter">
                  <Search size={13} aria-hidden />
                  <input placeholder="Filter connections…" value={filter} onChange={(e) => setFilter(e.target.value)} />
                </div>
              )}
              <div className="import-table conn-import-table">
                <label className="conn-row import-head">
                  <input
                    type="checkbox"
                    checked={allChosen}
                    disabled={selectable.length === 0}
                    onChange={() =>
                      setChosen((c) => {
                        const next = new Set(c);
                        for (const k of selectable) {
                          if (allChosen) next.delete(k);
                          else next.add(k);
                        }
                        return next;
                      })
                    }
                  />
                  <span>Connection</span>
                  <span>Server</span>
                  <span>Sign-in</span>
                </label>
                {sources.map((src, si) => {
                  const own = visible.filter((r) => r.source === si);
                  if (q && !own.length) return null;
                  return (
                    <div key={si} className="conn-source">
                      <div className="conn-source-head" title={src.path}>
                        <b>{src.label}</b>
                        {src.path && <span className="muted">{src.path}</span>}
                      </div>
                      {src.error ? (
                        <div className="conn-source-error">
                          <TriangleAlert size={12} /> {src.error}
                        </div>
                      ) : !src.connections.length && !src.skipped.length ? (
                        <div className="muted conn-source-error">No connections in this file.</div>
                      ) : null}
                      {own.map((r) => (
                        <ConnRow
                          key={r.key}
                          row={r}
                          checked={chosen.has(r.key)}
                          password={passwords[r.key] ?? ""}
                          login={logins[r.key] ?? ""}
                          onToggle={() => toggle(r.key)}
                          onPassword={(v) => setPasswords((p) => ({ ...p, [r.key]: v }))}
                          onLogin={(v) => setLogins((l) => ({ ...l, [r.key]: v }))}
                        />
                      ))}
                      {!src.error && src.skipped.length > 0 && (
                        <details className="import-skipped conn-skipped">
                          <summary className="muted">
                            {src.skipped.length} connection{src.skipped.length === 1 ? "" : "s"} can't be imported
                          </summary>
                          <ul>
                            {src.skipped.map((s, i) => (
                              <li key={i}>
                                <b>{s.name}</b> <span className="muted">— {s.reason}</span>
                              </li>
                            ))}
                          </ul>
                        </details>
                      )}
                    </div>
                  );
                })}
              </div>
            </>
          )}
          {sources !== null && (
            <small className="muted import-hint">
              Using SSMS 21 or 22? Its new Connect dialog keeps saved connections encrypted, so they can't be read.
              Add them under View → Registered Servers (or export that list to a <code>.regsrvr</code> file) and
              they'll show up here. Exports work too: pgAdmin's <code>servers.json</code> (Tools → Import/Export
              Servers), HeidiSQL's File → Export settings, and Windows <code>.udl</code> data link files.
            </small>
          )}
          {error && <p className="import-error">{error}</p>}
        </div>
      </Modal>
      {pasting && (
        <PasteConnectionDialog actionLabel="Add to list" onAdd={addPasted} onClose={() => setPasting(false)} />
      )}
    </>
  );
}

/** Rows for every source, marking connections that are saved already or listed earlier. */
function buildRows(sources: Loaded[], saved: DbConnection[]): Row[] {
  const savedKeys = new Set(saved.map((c) => connectionKey(c.config)));
  const listed = new Set<string>();
  const rows: Row[] = [];
  sources.forEach((src, si) => {
    src.connections.forEach((conn, ci) => {
      const k = connectionKey(conn.config);
      const duplicate = savedKeys.has(k) ? "saved" : listed.has(k) ? "listed" : null;
      listed.add(k);
      rows.push({ key: `${si}:${ci}`, source: si, conn, duplicate });
    });
  });
  return rows;
}

function ConnRow({
  row,
  checked,
  password,
  login,
  onToggle,
  onPassword,
  onLogin,
}: {
  row: Row;
  checked: boolean;
  password: string;
  login: string;
  onToggle: () => void;
  onPassword: (v: string) => void;
  onLogin: (v: string) => void;
}) {
  const { conn, duplicate } = row;
  const c = conn.config;
  const signIn = c.engine === "sqlite" ? "" : c.auth === "integrated" ? "Windows authentication" : c.user || "no login";
  const askLogin = needsPassword(conn) && !c.user;
  return (
    <label className={`conn-row${duplicate ? " dim" : ""}`}>
      <input type="checkbox" checked={checked} disabled={duplicate === "saved"} onChange={onToggle} />
      <span className="import-name">
        <EngineBadge engine={c.engine} />
        <span>
          {fullName(conn)}
          <small className="muted">
            {signIn}
            {duplicate === "saved" && " · already saved"}
            {duplicate === "listed" && " · also listed above"}
          </small>
        </span>
      </span>
      <span className="import-paths muted">
        <span title={describeConfig(c)}>{describeConfig(c) || "—"}</span>
        {conn.notes.map((n) => (
          <span key={n} className="import-note">
            <TriangleAlert size={11} /> {n}
          </span>
        ))}
      </span>
      <span className="conn-pass" onClick={(e) => e.preventDefault()}>
        {askLogin && duplicate !== "saved" && (
          <input
            value={login}
            placeholder="Login"
            autoComplete="off"
            spellCheck={false}
            aria-label={`Login for ${fullName(conn)}`}
            disabled={!checked}
            onChange={(e) => onLogin(e.target.value)}
          />
        )}
        {needsPassword(conn) && duplicate !== "saved" && (
          <input
            type="password"
            value={password}
            placeholder={askLogin ? "Password" : "Optional"}
            autoComplete="new-password"
            aria-label={`Password for ${fullName(conn)}`}
            disabled={!checked}
            onChange={(e) => onPassword(e.target.value)}
          />
        )}
      </span>
    </label>
  );
}
