// Typed bridge to the Database Compare commands in src-tauri/src/db.rs.
// Field names mirror the serde `camelCase` structs in crates/pcompare-db.
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { useSyncExternalStore } from "react";

// ---------- connections (connection.rs) ----------
export type Engine = "mssql" | "mysql" | "postgres" | "sqlite";
export type AuthMethod = "password" | "integrated";
export type TlsMode = "disable" | "prefer" | "require";

export const ENGINES: Engine[] = ["mssql", "mysql", "postgres", "sqlite"];

export const ENGINE_INFO: Record<Engine, { label: string; short: string; color: string; port: number | null }> = {
  mssql: { label: "SQL Server", short: "MSSQL", color: "#e2574c", port: 1433 },
  mysql: { label: "MySQL / MariaDB", short: "MySQL", color: "#f29111", port: 3306 },
  postgres: { label: "PostgreSQL", short: "PG", color: "#5b9cff", port: 5432 },
  sqlite: { label: "SQLite", short: "SQLite", color: "#7dd3fc", port: null },
};

export interface ConnectionConfig {
  engine: Engine | null;
  host: string;
  /** `null` = the engine's default port. */
  port: number | null;
  /** SQL Server named instance. */
  instance: string;
  database: string;
  user: string;
  auth: AuthMethod;
  tls: TlsMode;
  trustServerCertificate: boolean;
  /** SQLite database file. */
  file: string;
  /** PostgreSQL / SQL Server schemas to compare; empty = all user schemas. */
  schemas: string[];
}

export const emptyConfig = (engine: Engine): ConnectionConfig => ({
  engine,
  host: engine === "sqlite" ? "" : "localhost",
  port: null,
  instance: "",
  database: "",
  user: "",
  auth: "password",
  tls: "prefer",
  trustServerCertificate: engine === "mssql",
  file: "",
  schemas: [],
});

/**
 * Two configs with the same key point at the same place, so "Import
 * connections…" shows the second as a duplicate (of a saved connection, or of
 * one another tool also lists) instead of saving it twice.
 */
export function connectionKey(c: ConnectionConfig): string {
  const engine = c.engine ?? "mssql";
  if (engine === "sqlite") return `sqlite|${c.file.replace(/\\/g, "/").toLowerCase()}`;
  // TODO(you): decide what counts as "the same connection". This default is
  // strict: every field below must match (host, instance and login compared
  // case-insensitively, a missing port = the engine's default). Questions
  // worth settling: should `localhost`, `127.0.0.1` and `.` be one host? Is
  // `server/SalesDb` a duplicate of `server/HrDb` with the same login, given
  // the header's database picker can switch databases on one saved
  // connection? And PostgreSQL role names are case-sensitive, unlike SQL
  // Server logins.
  const port = c.port ?? ENGINE_INFO[engine].port;
  const login = c.auth === "integrated" ? "(windows)" : c.user.toLowerCase();
  return [engine, c.host.trim().toLowerCase(), c.instance.toLowerCase(), port, c.database.toLowerCase(), login].join("|");
}

export interface DbConnection {
  id: string;
  name: string;
  config: ConnectionConfig;
  hasPassword: boolean;
  createdMs: number;
  modifiedMs: number;
}

export interface DbConnectionInput {
  id?: string;
  name: string;
  config: ConnectionConfig;
  /** A value replaces the stored password, `""` deletes it, absent keeps it. */
  password?: string;
}

// ---------- schema (model.rs / schema_diff.rs) ----------
export type ObjectKind = "table" | "view" | "procedure" | "function" | "trigger" | "sequence";
export const OBJECT_KINDS: ObjectKind[] = ["table", "view", "procedure", "function", "trigger", "sequence"];
export const KIND_LABEL: Record<ObjectKind, string> = {
  table: "Tables",
  view: "Views",
  procedure: "Procedures",
  function: "Functions",
  trigger: "Triggers",
  sequence: "Sequences",
};

export type DiffStatus = "same" | "different" | "leftOnly" | "rightOnly";

export interface ObjectName {
  /** `""` on engines without schemas inside a database (MySQL, SQLite). */
  schema: string;
  name: string;
}

export const objectName = (n: ObjectName | null | undefined) => (!n ? "" : n.schema ? `${n.schema}.${n.name}` : n.name);

export interface SchemaCompareOptions {
  ignoreNameCase: boolean;
  ignoreWhitespace: boolean;
  ignoreComments: boolean;
  ignoreColumnOrder: boolean;
  ignoreCollation: boolean;
  ignoreConstraintNames: boolean;
  ignoreIdentitySeed: boolean;
  /** Empty = all kinds. */
  kinds: ObjectKind[];
}

export const DEFAULT_SCHEMA_OPTIONS: SchemaCompareOptions = {
  ignoreNameCase: true,
  ignoreWhitespace: true,
  ignoreComments: false,
  ignoreColumnOrder: true,
  ignoreCollation: false,
  ignoreConstraintNames: true,
  ignoreIdentitySeed: true,
  kinds: [],
};

export interface ObjectDiff {
  /** `kind:schema.name` — stable key for selection and script ids. */
  id: string;
  kind: ObjectKind;
  name: ObjectName;
  status: DiffStatus;
  changes: string[];
  leftDdl: string | null;
  rightDdl: string | null;
}

export interface DiffStats {
  same: number;
  different: number;
  leftOnly: number;
  rightOnly: number;
}

export interface SideInfo {
  engine: Engine;
  /** `host/database` or the SQLite file (never credentials). */
  label: string;
  database: string;
  serverVersion: string;
}

export interface SchemaCompareResult {
  left: SideInfo;
  right: SideInfo;
  objects: ObjectDiff[];
  stats: DiffStats;
  crossEngine: boolean;
}

// ---------- data (value.rs / data_diff.rs) ----------
export interface ValueRules {
  ignoreCase: boolean;
  trimWhitespace: boolean;
  numericTolerance: number | null;
  /** Fractional-second digits compared; `null` = all. */
  timePrecision: number | null;
}

export interface DataCompareOptions {
  rules: ValueRules;
  /** Table ids to compare; empty = every table on both sides. */
  tables: string[];
  /** Table id → key columns. */
  keys: Record<string, string[]>;
  ignoredColumns: Record<string, string[]>;
  ignoreNameCase: boolean;
  maxRows: number;
}

export const DEFAULT_DATA_OPTIONS: DataCompareOptions = {
  rules: { ignoreCase: false, trimWhitespace: false, numericTolerance: null, timePrecision: null },
  tables: [],
  keys: {},
  ignoredColumns: {},
  ignoreNameCase: true,
  maxRows: 1000,
};

/** Cell values arrive as display text; `null` is SQL NULL. */
export type Cell = string | null;

export type RowStatus = "different" | "leftOnly" | "rightOnly";

export interface DataRow {
  status: RowStatus;
  key: Cell[];
  /** Values in `columns` order. */
  left: Cell[] | null;
  right: Cell[] | null;
  /** Indices into `columns` whose values differ. */
  diffs: number[];
}

export interface TableDataSummary {
  id: string;
  left: ObjectName | null;
  right: ObjectName | null;
  key: string[];
  columns: string[];
  rightColumns: string[];
  leftOnlyColumns: string[];
  rightOnlyColumns: string[];
  /** `leftOnly` / `rightOnly` = the table exists on one side only. */
  status: DiffStatus | null;
  leftRows: number;
  rightRows: number;
  same: number;
  different: number;
  leftOnly: number;
  rightOnly: number;
  error: string | null;
}

export interface TableDataDiff {
  summary: TableDataSummary;
  rows: DataRow[];
  truncated: boolean;
}

export interface DataCompareResult {
  left: SideInfo;
  right: SideInfo;
  tables: TableDataSummary[];
  identical: boolean;
}

export interface DbProgress {
  done: number;
  total: number;
  current: string;
}

// ---------- scripts (script.rs) ----------
export type SyncDirection = "leftToRight" | "rightToLeft";

export interface ScriptOptions {
  direction: SyncDirection;
  /** Object ids (schema) or table ids (data); empty = every difference. */
  ids: string[];
  includeDrops: boolean;
  includeInserts: boolean;
  includeUpdates: boolean;
  transactional: boolean;
  disableForeignKeys: boolean;
  /** Data: table id → keys of the only rows to sync there (as `DataRow.key`). */
  rows?: Record<string, Cell[][]>;
}

export const DEFAULT_SCRIPT_OPTIONS: ScriptOptions = {
  direction: "leftToRight",
  ids: [],
  includeDrops: true,
  includeInserts: true,
  includeUpdates: true,
  transactional: true,
  disableForeignKeys: true,
};

export interface SyncScript {
  targetEngine: Engine | null;
  /** Executable statements without terminators. */
  statements: string[];
  /** Human-readable script with header, terminators and BEGIN/COMMIT. */
  text: string;
  warnings: string[];
  transactional: boolean;
}

export const dbApi = {
  listConnections: () => invoke<DbConnection[]>("db_list_connections").then(rememberNames),
  saveConnection: (input: DbConnectionInput) => invoke<DbConnection>("db_save_connection", { input }),
  deleteConnection: (id: string) => invoke<void>("db_delete_connection", { id }),
  testConnection: (input: DbConnectionInput) => invoke<string>("db_test_connection", { input }),
  listDatabases: (input: DbConnectionInput) => invoke<string[]>("db_list_databases", { input }),
  isSqlite: (paths: string[]) => invoke<boolean>("db_is_sqlite", { paths }),

  compareSchema: (left: string, right: string, options: SchemaCompareOptions) =>
    invoke<SchemaCompareResult>("db_compare_schema", { left, right, options }),
  schemaScript: (left: string, right: string, compare: SchemaCompareOptions, options: ScriptOptions) =>
    invoke<SyncScript>("db_schema_script", { left, right, compare, options }),

  compareData: (left: string, right: string, options: DataCompareOptions) =>
    invoke<DataCompareResult>("db_compare_data", { left, right, options }),
  tableData: (left: string, right: string, tableId: string, options: DataCompareOptions) =>
    invoke<TableDataDiff>("db_table_data", { left, right, tableId, options }),
  dataScript: (left: string, right: string, compare: DataCompareOptions, options: ScriptOptions) =>
    invoke<SyncScript>("db_data_script", { left, right, compare, options }),
  cancel: () => invoke<void>("db_cancel"),
  onProgress: (handler: (p: DbProgress) => void): Promise<UnlistenFn> =>
    listen<DbProgress>("db-progress", (e) => handler(e.payload)),

  executeScript: (target: string, statements: string[], transactional: boolean) =>
    invoke<void>("db_execute_script", { target, statements, transactional }),
};

// ---------- session paths ----------
export const DB_SCHEME = "db://";

export const isDbPath = (path: string) => path.startsWith(DB_SCHEME);

/** `db://<id>[/<database>]` → parts; `null` for anything else (a SQLite file path). */
export function parseDbPath(path: string): { id: string; database: string | null } | null {
  if (!isDbPath(path)) return null;
  const rest = path.slice(DB_SCHEME.length);
  const slash = rest.indexOf("/");
  const id = slash < 0 ? rest : rest.slice(0, slash);
  const database = slash < 0 ? null : rest.slice(slash + 1) || null;
  return id ? { id, database } : null;
}

export const dbPath = (id: string, database?: string | null) => `${DB_SCHEME}${id}${database ? `/${database}` : ""}`;

// Connection names by id, so the tab bar can title `db://` sessions by name.
// Filled whenever the list is fetched; App subscribes with `useDbConnectionNames`.
let names: Record<string, string> = {};
const listeners = new Set<() => void>();

function rememberNames(list: DbConnection[]): DbConnection[] {
  const next = Object.fromEntries(list.map((c) => [c.id, c.name]));
  if (JSON.stringify(next) !== JSON.stringify(names)) {
    names = next;
    listeners.forEach((l) => l());
  }
  return list;
}

export function useDbConnectionNames(): Record<string, string> {
  return useSyncExternalStore(
    (cb) => {
      listeners.add(cb);
      return () => listeners.delete(cb);
    },
    () => names,
  );
}

/** Short label for a session side: the connection name (+ database override) or the file name. */
export function dbPathLabel(path: string, known: Record<string, string>): string {
  const p = parseDbPath(path);
  if (!p) return path.replace(/[\\/]+$/, "").split(/[\\/]/).pop() ?? path;
  const name = known[p.id] ?? p.id;
  return p.database ? `${name} / ${p.database}` : name;
}
