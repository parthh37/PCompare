// "Paste a connection string": parses ADO.NET, ODBC, OLE DB, JDBC, URL and
// libpq strings into a connection, for the import list and the New
// connection dialog.
import { TriangleAlert, X } from "lucide-react";
import { useEffect, useState } from "react";
import { errorMessage } from "../lib/api";
import { connectionImportApi, type ImportedConnection } from "../lib/importApi";
import { describeConfig, EngineBadge } from "./DbBadge";
import { Modal } from "./Modal";

/** Parses one pasted connection string (ADO.NET, ODBC, JDBC, URL…) as you type. */
export function PasteConnectionDialog({
  actionLabel,
  onAdd,
  onClose,
}: {
  actionLabel: string;
  onAdd: (conn: ImportedConnection, password: string | null) => void;
  onClose: () => void;
}) {
  const [text, setText] = useState("");
  const [result, setResult] = useState<{ conn: ImportedConnection; password: string | null } | string | null>(null);

  useEffect(() => {
    if (!text.trim()) return setResult(null);
    let stale = false;
    const t = setTimeout(() => {
      connectionImportApi
        .parseString(text)
        .then((r) => !stale && setResult({ conn: r.connection, password: r.password }))
        .catch((e) => !stale && setResult(errorMessage(e)));
    }, 150);
    return () => {
      stale = true;
      clearTimeout(t);
    };
  }, [text]);

  const parsed = result && typeof result === "object" ? result : null;
  return (
    <Modal
      title="Paste a connection string"
      onClose={onClose}
      footer={
        <>
          <span className="spacer" />
          <button onClick={onClose}>Cancel</button>
          <button className="primary" disabled={!parsed} onClick={() => parsed && onAdd(parsed.conn, parsed.password)}>
            {actionLabel}
          </button>
        </>
      }
    >
      <div className="conn-paste">
        <textarea
          autoFocus
          rows={4}
          spellCheck={false}
          value={text}
          placeholder={
            "Server=sql01\\PROD;Database=Sales;Integrated Security=True\npostgresql://me@pg.local:5432/app\njdbc:mysql://db:3306/shop"
          }
          onChange={(e) => setText(e.target.value)}
        />
        <small className="muted">
          ADO.NET, ODBC and OLE DB strings, JDBC URLs, <code>postgres://</code> / <code>mysql://</code> URLs and libpq{" "}
          <code>host=… dbname=…</code> strings all work.
        </small>
        {typeof result === "string" && (
          <div className="banner error">
            <X size={14} /> {result}
          </div>
        )}
        {parsed && (
          <div className="conn-paste-result">
            <EngineBadge engine={parsed.conn.config.engine} />
            <span>
              <b>{describeConfig(parsed.conn.config)}</b>
              <small className="muted">
                {parsed.conn.config.auth === "integrated"
                  ? "Windows authentication"
                  : parsed.conn.config.user || "no login"}
                {parsed.password ? " · password included" : ""}
              </small>
              {parsed.conn.notes.map((n) => (
                <small key={n} className="import-note">
                  <TriangleAlert size={11} /> {n}
                </small>
              ))}
            </span>
          </div>
        )}
      </div>
    </Modal>
  );
}
