import { open } from "@tauri-apps/plugin-dialog";
import { CircleCheck, Database, FolderOpen, LoaderCircle, RefreshCw, TriangleAlert } from "lucide-react";
import { useState } from "react";
import { errorMessage } from "../lib/api";
import {
  dbApi,
  emptyConfig,
  ENGINE_INFO,
  ENGINES,
  type AuthMethod,
  type ConnectionConfig,
  type DbConnection,
  type DbConnectionInput,
  type Engine,
  type TlsMode,
} from "../lib/dbApi";
import { Modal } from "./Modal";

export const SQLITE_FILTERS = [
  { name: "SQLite database", extensions: ["db", "sqlite", "sqlite3", "db3", "s3db", "sl3"] },
  { name: "All files", extensions: ["*"] },
];

/** `server/database` label like `ConnectionConfig::describe` in Rust (never credentials). */
export function describeConfig(c: ConnectionConfig): string {
  if (c.engine === "sqlite") return c.file;
  let host = c.host;
  if (c.instance) host = `${host}\\${c.instance}`;
  else if (c.port !== null) host = `${host}:${c.port}`;
  return c.database ? `${host}/${c.database}` : host;
}

/** A small engine tag tinted with the engine's colour. */
export function EngineBadge({ engine }: { engine: Engine | null | undefined }) {
  if (!engine)
    return (
      <span className="db-engine none" title="No connection chosen">
        <Database size={12} />
      </span>
    );
  const { short, label, color } = ENGINE_INFO[engine];
  return (
    <span className="db-engine" style={{ color, borderColor: `${color}66`, background: `${color}1a` }} title={label}>
      {short}
    </span>
  );
}

interface Props {
  /** The connection being edited; absent = create a new one. */
  connection?: DbConnection;
  onClose: () => void;
  onSaved: (c: DbConnection) => void;
  onDeleted: (id: string) => void;
}

type Status = { tone: "ok" | "error"; text: string } | null;

const TLS_LABEL: Record<TlsMode, string> = {
  disable: "Disabled",
  prefer: "Prefer (encrypt when supported)",
  require: "Require",
};

/** Create / edit a saved database connection. The password goes to the OS keychain. */
export function DbConnectionDialog({ connection, onClose, onSaved, onDeleted }: Props) {
  const [name, setName] = useState(connection?.name ?? "");
  const [config, setConfig] = useState<ConnectionConfig>(connection?.config ?? emptyConfig("mssql"));
  /** `undefined` = keep the stored password, `""` = remove it, text = replace it. */
  const [password, setPassword] = useState<string | undefined>(undefined);
  const [schemasText, setSchemasText] = useState(config.schemas.join(", "));
  const [databases, setDatabases] = useState<string[] | null>(null);
  const [busy, setBusy] = useState<"test" | "save" | "load" | "delete" | null>(null);
  const [status, setStatus] = useState<Status>(null);

  const engine = config.engine ?? "mssql";
  const isSqlite = engine === "sqlite";
  const integrated = engine === "mssql" && config.auth === "integrated";
  const stored = !!connection?.hasPassword;
  const cleared = stored && password === "";

  const set = (patch: Partial<ConnectionConfig>) => {
    setConfig((c) => ({ ...c, ...patch }));
    setStatus(null);
  };

  const setEngine = (e: Engine) => {
    // Keep what carries over (host, user, database) and reset engine-specific bits.
    setConfig((c) => ({
      ...emptyConfig(e),
      host: e === "sqlite" ? "" : c.host || "localhost",
      user: c.user,
      database: c.database,
      file: c.file,
    }));
    setDatabases(null);
    setStatus(null);
  };

  const input = (): DbConnectionInput => ({
    id: connection?.id,
    name: name.trim() || defaultName(),
    config: {
      ...config,
      schemas: schemasText
        .split(",")
        .map((s) => s.trim())
        .filter(Boolean),
    },
    password,
  });

  const defaultName = () => {
    if (isSqlite) return config.file.split(/[\\/]/).pop() || "SQLite database";
    const db = config.database ? `/${config.database}` : "";
    return `${config.host || "localhost"}${db}`;
  };

  const missing = isSqlite ? !config.file.trim() : !config.host.trim();

  const run = async (what: "test" | "load") => {
    setBusy(what);
    setStatus(null);
    try {
      if (what === "test") {
        setStatus({ tone: "ok", text: await dbApi.testConnection(input()) });
      } else {
        const list = await dbApi.listDatabases(input());
        setDatabases(list);
        if (!list.length) setStatus({ tone: "error", text: "The server returned no databases." });
      }
    } catch (e) {
      setStatus({ tone: "error", text: errorMessage(e) });
    } finally {
      setBusy(null);
    }
  };

  const save = async () => {
    setBusy("save");
    try {
      onSaved(await dbApi.saveConnection(input()));
      onClose();
    } catch (e) {
      setStatus({ tone: "error", text: errorMessage(e) });
      setBusy(null);
    }
  };

  const remove = async () => {
    if (!connection) return;
    if (!window.confirm(`Delete the connection “${connection.name}”? Its saved password is removed too.`)) return;
    setBusy("delete");
    try {
      await dbApi.deleteConnection(connection.id);
      onDeleted(connection.id);
      onClose();
    } catch (e) {
      setStatus({ tone: "error", text: errorMessage(e) });
      setBusy(null);
    }
  };

  const browse = async () => {
    const picked = await open({ multiple: false, directory: false, filters: SQLITE_FILTERS, title: "SQLite database" });
    if (typeof picked === "string") set({ file: picked });
  };

  const portPlaceholder = ENGINE_INFO[engine].port ? String(ENGINE_INFO[engine].port) : "";

  return (
    <Modal
      title={connection ? `Edit connection — ${connection.name}` : "New database connection"}
      wide
      onClose={onClose}
      footer={
        <>
          {connection && (
            <button className="danger" onClick={remove} disabled={busy !== null}>
              Delete
            </button>
          )}
          <button onClick={() => run("test")} disabled={busy !== null || missing}>
            {busy === "test" ? <LoaderCircle size={14} className="db-spin" /> : <CircleCheck size={14} />}
            {busy === "test" ? "Testing…" : "Test connection"}
          </button>
          <span className="spacer" />
          <button onClick={onClose}>Cancel</button>
          <button className="primary" onClick={save} disabled={busy !== null || missing}>
            {busy === "save" ? "Saving…" : "Save"}
          </button>
        </>
      }
    >
      <div className="db-dialog">
        <div className="field-row">
          <label className="field">
            <span>Engine</span>
            <select value={engine} onChange={(e) => setEngine(e.target.value as Engine)} autoFocus={!connection}>
              {ENGINES.map((e) => (
                <option key={e} value={e}>
                  {ENGINE_INFO[e].label}
                </option>
              ))}
            </select>
          </label>
          <label className="field">
            <span>Name</span>
            <input value={name} placeholder={defaultName()} onChange={(e) => setName(e.target.value)} spellCheck={false} />
          </label>
        </div>

        {isSqlite ? (
          <label className="field">
            <span>
              Database file <i className="req">*</i>
            </span>
            <div className="input-row">
              <input
                value={config.file}
                placeholder="/path/to/database.sqlite"
                onChange={(e) => set({ file: e.target.value })}
                spellCheck={false}
              />
              <button className="icon-btn" title="Browse…" onClick={browse}>
                <FolderOpen size={14} />
              </button>
            </div>
          </label>
        ) : (
          <>
            <div className="field-row">
              <label className="field db-grow">
                <span>
                  Host <i className="req">*</i>
                </span>
                <input value={config.host} onChange={(e) => set({ host: e.target.value })} spellCheck={false} />
              </label>
              {engine === "mssql" && (
                <label className="field">
                  <span>Instance</span>
                  <input
                    value={config.instance}
                    placeholder="default"
                    onChange={(e) => set({ instance: e.target.value })}
                    spellCheck={false}
                  />
                </label>
              )}
              <label className="field db-port">
                <span>Port</span>
                <input
                  type="number"
                  min={1}
                  max={65535}
                  value={config.port ?? ""}
                  placeholder={portPlaceholder}
                  disabled={engine === "mssql" && !!config.instance}
                  title={engine === "mssql" && config.instance ? "Named instances are found via SQL Browser" : undefined}
                  onChange={(e) => set({ port: e.target.value ? Number(e.target.value) : null })}
                />
              </label>
            </div>

            {engine === "mssql" && (
              <label className="field">
                <span>Authentication</span>
                <select value={config.auth} onChange={(e) => set({ auth: e.target.value as AuthMethod })}>
                  <option value="password">SQL Server login</option>
                  <option value="integrated">Windows authentication</option>
                </select>
                {integrated && (
                  <small className="muted">
                    Uses the signed-in Windows account. Only available when PCompare runs on Windows.
                  </small>
                )}
              </label>
            )}

            {!integrated && (
              <div className="field-row">
                <label className="field">
                  <span>User</span>
                  <input
                    value={config.user}
                    onChange={(e) => set({ user: e.target.value })}
                    spellCheck={false}
                    autoComplete="off"
                  />
                </label>
                <label className="field">
                  <span>Password</span>
                  <div className="input-row">
                    <input
                      type="password"
                      value={password ?? ""}
                      autoComplete="new-password"
                      placeholder={
                        cleared ? "Will be removed on save" : stored ? "•••••••• saved in keychain" : "Not saved"
                      }
                      // Typing then erasing everything goes back to "keep the stored password".
                      onChange={(e) => setPassword(e.target.value || undefined)}
                    />
                    {stored && (
                      <button title="Remove the saved password" disabled={cleared} onClick={() => setPassword("")}>
                        Clear
                      </button>
                    )}
                  </div>
                </label>
              </div>
            )}

            <label className="field">
              <span>Database</span>
              <div className="input-row">
                <input
                  value={config.database}
                  list="db-dialog-databases"
                  placeholder={engine === "mysql" ? "(none — choose per session)" : "default for the login"}
                  onChange={(e) => set({ database: e.target.value })}
                  spellCheck={false}
                />
                <datalist id="db-dialog-databases">
                  {databases?.map((d) => <option key={d} value={d} />)}
                </datalist>
                <button onClick={() => run("load")} disabled={busy !== null || missing} title="List the server's databases">
                  {busy === "load" ? <LoaderCircle size={14} className="db-spin" /> : <RefreshCw size={14} />} Load
                </button>
              </div>
              {databases && databases.length > 0 && (
                <div className="db-db-chips">
                  {databases.map((d) => (
                    <button
                      key={d}
                      className={`db-chip${d === config.database ? " active" : ""}`}
                      onClick={() => set({ database: d })}
                    >
                      {d}
                    </button>
                  ))}
                </div>
              )}
            </label>

            <div className="field-row">
              <label className="field">
                <span>Encryption (TLS)</span>
                <select value={config.tls} onChange={(e) => set({ tls: e.target.value as TlsMode })}>
                  {(Object.keys(TLS_LABEL) as TlsMode[]).map((t) => (
                    <option key={t} value={t}>
                      {TLS_LABEL[t]}
                    </option>
                  ))}
                </select>
              </label>
              {(engine === "postgres" || engine === "mssql") && (
                <label className="field">
                  <span>Schemas</span>
                  <input
                    value={schemasText}
                    placeholder={engine === "mssql" ? "all (e.g. dbo, sales)" : "all (e.g. public, billing)"}
                    onChange={(e) => setSchemasText(e.target.value)}
                    spellCheck={false}
                  />
                </label>
              )}
            </div>
            <label className="field check">
              <input
                type="checkbox"
                checked={config.trustServerCertificate}
                disabled={config.tls === "disable"}
                onChange={(e) => set({ trustServerCertificate: e.target.checked })}
              />
              Trust the server certificate (accept self-signed certificates)
            </label>
          </>
        )}

        {status && (
          <div className={`banner ${status.tone} db-dialog-status`}>
            {status.tone === "ok" ? <CircleCheck size={14} /> : <TriangleAlert size={14} />} {status.text}
          </div>
        )}
        {!isSqlite && !status && (
          <small className="muted">The password is stored in the operating system's keychain, never on disk.</small>
        )}
      </div>
    </Modal>
  );
}
