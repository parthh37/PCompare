// The engine tag and `server/database` label shared by the database dialogs.
import { Database } from "lucide-react";
import { ENGINE_INFO, type ConnectionConfig, type Engine } from "../lib/dbApi";

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
