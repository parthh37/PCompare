import { open, save } from "@tauri-apps/plugin-dialog";
import {
  ArrowLeft,
  ArrowLeftRight,
  ArrowRight,
  ChevronDown,
  ChevronRight,
  CircleCheck,
  Copy,
  Equal,
  EqualNot,
  Eye,
  KeyRound,
  Layers,
  ListOrdered,
  LoaderCircle,
  Pencil,
  Play,
  Plus,
  Rows3,
  Save,
  ScrollText,
  Search,
  Settings2,
  SquareFunction,
  SquareTerminal,
  Table2,
  TriangleAlert,
  Zap,
  type LucideIcon,
} from "lucide-react";
import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState, type WheelEvent } from "react";
import { DEFAULT_TEXT_OPTIONS, errorMessage, type Segment, type TextDiffResult } from "../lib/api";
import {
  dbApi,
  dbPath,
  dbPathLabel,
  DEFAULT_DATA_OPTIONS,
  DEFAULT_SCHEMA_OPTIONS,
  DEFAULT_SCRIPT_OPTIONS,
  ENGINE_INFO,
  KIND_LABEL,
  OBJECT_KINDS,
  objectName,
  parseDbPath,
  type Cell,
  type DataCompareOptions,
  type DataCompareResult,
  type DbConnection,
  type DbProgress,
  type DiffStatus,
  type Engine,
  type ObjectDiff,
  type ObjectKind,
  type RowStatus,
  type SchemaCompareOptions,
  type SchemaCompareResult,
  type ScriptOptions,
  type SideInfo,
  type SyncScript,
  type TableDataDiff,
  type TableDataSummary,
} from "../lib/dbApi";
import { basename } from "../lib/format";
import { matches, shortcutLabel } from "../lib/keymap";
import { mergeApi } from "../lib/mergeApi";
import { LAUNCH_READ_ONLY_HINT, type CompareProps, type ReadOnlySides } from "../lib/sessionProps";
import { DbConnectionDialog, describeConfig, EngineBadge, SQLITE_FILTERS } from "./DbConnectionDialog";
import { Modal } from "./Modal";
import { VirtualList } from "./VirtualList";
import "./TableCompare.css";
import "./FolderSync.css";
import "./DbCompare.css";

type Mode = "schema" | "data";
type Side = "left" | "right";
type SchemaFilter = "all" | "diffs" | "leftOnly" | "rightOnly" | "same";
type DataFilter = "all" | "diffs" | "same" | "errors";
type RowFilter = "all" | RowStatus;

/** What a saved Database Compare session remembers besides its two connection paths. */
export interface DbSettings {
  mode: Mode;
  schemaFilter: SchemaFilter;
  dataFilter: DataFilter;
  schemaOptions: SchemaCompareOptions;
  dataOptions: DataCompareOptions;
  /** Script dialog choices (ids are picked per comparison and not saved). */
  scriptOptions: Omit<ScriptOptions, "ids">;
}

const STATUS_ICON: Record<DiffStatus, LucideIcon> = {
  same: Equal,
  different: EqualNot,
  leftOnly: ArrowLeft,
  rightOnly: ArrowRight,
};
const STATUS_LABEL: Record<DiffStatus, string> = {
  same: "Identical",
  different: "Different",
  leftOnly: "Only in left",
  rightOnly: "Only in right",
};
const KIND_ICON: Record<ObjectKind, LucideIcon> = {
  table: Table2,
  view: Eye,
  procedure: SquareTerminal,
  function: SquareFunction,
  trigger: Zap,
  sequence: ListOrdered,
};

const OBJ_ROW = 24;
const DATA_ROW = 22;
const CHAR_WIDTH = 7.4;
const MIN_COL = 56;
const MAX_COL = 320;
/** Script lines rendered with highlighting; longer scripts are shown as plain text. */
const HIGHLIGHT_LINES = 4000;
const NEW_CONNECTION = "\u0000new";
const OPEN_FILE = "\u0000file";

const matchesSchema = (o: ObjectDiff, f: SchemaFilter) =>
  f === "all" || (f === "diffs" ? o.status !== "same" : o.status === f);

const tableIdentical = (t: TableDataSummary) => !t.error && t.status === "same";
const matchesData = (t: TableDataSummary, f: DataFilter) =>
  f === "all" || (f === "errors" ? !!t.error : f === "same" ? tableIdentical(t) : !tableIdentical(t) && !t.error);
/** Tables that a data script can sync: on both sides, comparable, not identical. */
const scriptable = (t: TableDataSummary) => !t.error && t.status === "different";

const splitLines = (text: string | null) => (text ? text.replace(/\r\n?/g, "\n").replace(/\n$/, "").split("\n") : []);

function Segments({ segs }: { segs: Segment[] }) {
  return (
    <>
      {segs.map((s, i) =>
        s.changed ? (
          <mark key={i} className="seg-changed">
            {s.text}
          </mark>
        ) : (
          <span key={i}>{s.text}</span>
        ),
      )}
    </>
  );
}

function CellText({ v }: { v: Cell }) {
  return v === null ? <span className="db-null">NULL</span> : <>{v.replace(/\r?\n/g, " ↵ ")}</>;
}

/** Settings restored from a save may be partial or from an older version. */
function restore(s: Partial<DbSettings> | undefined): DbSettings {
  return {
    mode: s?.mode ?? "schema",
    schemaFilter: s?.schemaFilter ?? "all",
    dataFilter: s?.dataFilter ?? "all",
    schemaOptions: { ...DEFAULT_SCHEMA_OPTIONS, ...s?.schemaOptions },
    dataOptions: {
      ...DEFAULT_DATA_OPTIONS,
      ...s?.dataOptions,
      rules: { ...DEFAULT_DATA_OPTIONS.rules, ...s?.dataOptions?.rules },
    },
    scriptOptions: { ...DEFAULT_SCRIPT_OPTIONS, ...s?.scriptOptions },
  };
}

export function DbCompare({
  left,
  right,
  onPathsChange,
  settings,
  onSettingsChange,
  readOnlySides, // bc5:reports-script
}: CompareProps<DbSettings>) {
  const [initial] = useState(() => restore(settings));
  const [mode, setMode] = useState<Mode>(initial.mode);
  const [schemaFilter, setSchemaFilter] = useState<SchemaFilter>(initial.schemaFilter);
  const [dataFilter, setDataFilter] = useState<DataFilter>(initial.dataFilter);
  const [schemaOptions, setSchemaOptions] = useState(initial.schemaOptions);
  const [dataOptions, setDataOptions] = useState(initial.dataOptions);
  const [scriptOptions, setScriptOptions] = useState(initial.scriptOptions);
  const reportSettings = useRef(onSettingsChange);
  reportSettings.current = onSettingsChange;
  useEffect(
    () => reportSettings.current?.({ mode, schemaFilter, dataFilter, schemaOptions, dataOptions, scriptOptions }),
    [mode, schemaFilter, dataFilter, schemaOptions, dataOptions, scriptOptions],
  );

  const root = useRef<HTMLDivElement>(null);
  const [connections, setConnections] = useState<DbConnection[]>([]);
  const [editing, setEditing] = useState<{ side: Side; connection?: DbConnection } | null>(null);
  const [notice, setNotice] = useState<{ tone: "ok" | "error"; text: string } | null>(null);
  const [showOptions, setShowOptions] = useState(false);
  const [scriptFor, setScriptFor] = useState<Mode | null>(null);

  const loadConnections = useCallback(
    () =>
      dbApi
        .listConnections()
        .then(setConnections)
        .catch((e) => setNotice({ tone: "error", text: errorMessage(e) })),
    [],
  );
  useEffect(() => {
    loadConnections();
  }, [loadConnections]);

  useEffect(() => {
    if (notice?.tone !== "ok") return;
    const t = setTimeout(() => setNotice(null), 6000);
    return () => clearTimeout(t);
  }, [notice]);

  // ---------- comparisons ----------
  const [schema, setSchema] = useState<SchemaCompareResult | null>(null);
  const [data, setData] = useState<DataCompareResult | null>(null);
  const [busy, setBusy] = useState<Record<Mode, boolean>>({ schema: false, data: false });
  const [errors, setErrors] = useState<Record<Mode, string | null>>({ schema: null, data: null });
  const [progress, setProgress] = useState<DbProgress | null>(null);
  const [cancelling, setCancelling] = useState(false);

  const ready = !!left && !!right;
  const keys: Record<Mode, string> = {
    schema: JSON.stringify([left, right, schemaOptions]),
    data: JSON.stringify([left, right, dataOptions]),
  };
  /** The inputs each mode last compared, so results aren't recomputed on every render. */
  const attempted = useRef<Record<Mode, string>>({ schema: "", data: "" });
  const requests = useRef<Record<Mode, number>>({ schema: 0, data: 0 });

  const compare = useCallback(
    async (which: Mode) => {
      if (!left || !right) return;
      attempted.current[which] = JSON.stringify([left, right, which === "schema" ? schemaOptions : dataOptions]);
      const id = ++requests.current[which];
      const current = () => id === requests.current[which];
      setBusy((b) => ({ ...b, [which]: true }));
      setErrors((e) => ({ ...e, [which]: null }));
      if (which === "data") {
        setProgress(null);
        setCancelling(false);
      }
      try {
        if (which === "schema") {
          const r = await dbApi.compareSchema(left, right, schemaOptions);
          if (current()) setSchema(r);
        } else {
          const r = await dbApi.compareData(left, right, dataOptions);
          if (current()) setData(r);
        }
      } catch (e) {
        if (!current()) return;
        const msg = errorMessage(e);
        setErrors((x) => ({ ...x, [which]: /cancelled/i.test(msg) ? "Data compare cancelled." : msg }));
        if (which === "schema") setSchema(null);
        else setData(null);
      } finally {
        if (current()) setBusy((b) => ({ ...b, [which]: false }));
      }
    },
    [left, right, schemaOptions, dataOptions],
  );

  // Compare the visible mode whenever its inputs change (and on first open).
  useEffect(() => {
    if (ready && attempted.current[mode] !== keys[mode]) compare(mode);
  });

  useEffect(() => {
    let unlisten: (() => void) | undefined;
    let gone = false;
    dbApi
      .onProgress(setProgress)
      .then((u) => (gone ? u() : (unlisten = u)))
      .catch(() => {});
    return () => {
      gone = true;
      unlisten?.();
    };
  }, []);

  const cancel = () => {
    setCancelling(true);
    dbApi.cancel().catch((e) => setNotice({ tone: "error", text: errorMessage(e) }));
  };

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      // Inactive tabs stay mounted but hidden; only the visible one reacts.
      if (!matches(e, "database.refresh") || !root.current?.offsetParent) return;
      e.preventDefault();
      compare(mode);
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [compare, mode]);

  // ---------- schema view state ----------
  const [selectedObj, setSelectedObj] = useState<string | null>(null);
  const [checkedObjs, setCheckedObjs] = useState<Set<string>>(new Set());
  const [search, setSearch] = useState("");
  const [collapsed, setCollapsed] = useState<Set<ObjectKind>>(new Set());

  useEffect(() => {
    const objs = schema?.objects ?? [];
    setCheckedObjs(new Set(objs.filter((o) => o.status !== "same").map((o) => o.id)));
    setSelectedObj((sel) =>
      sel && objs.some((o) => o.id === sel) ? sel : (objs.find((o) => o.status !== "same") ?? objs[0])?.id ?? null,
    );
  }, [schema]);

  const objectItems = useMemo(() => {
    const q = search.trim().toLowerCase();
    const shown = (schema?.objects ?? []).filter(
      (o) => matchesSchema(o, schemaFilter) && (!q || objectName(o.name).toLowerCase().includes(q)),
    );
    const out: ({ group: ObjectKind; objs: ObjectDiff[] } | { obj: ObjectDiff })[] = [];
    for (const kind of OBJECT_KINDS) {
      const objs = shown.filter((o) => o.kind === kind);
      if (!objs.length) continue;
      out.push({ group: kind, objs });
      if (!collapsed.has(kind)) objs.forEach((obj) => out.push({ obj }));
    }
    return out;
  }, [schema, schemaFilter, search, collapsed]);

  const selected = schema?.objects.find((o) => o.id === selectedObj) ?? null;
  const [ddl, setDdl] = useState<{ id: string; diff: TextDiffResult } | null>(null);
  useEffect(() => {
    if (!selected) return setDdl(null);
    let stale = false;
    const whitespace = schemaOptions.ignoreWhitespace ? "ignoreChanges" : "compare";
    mergeApi
      .diffTexts(splitLines(selected.leftDdl), splitLines(selected.rightDdl), { ...DEFAULT_TEXT_OPTIONS, whitespace })
      .then((diff) => !stale && setDdl({ id: selected.id, diff }))
      .catch(() => !stale && setDdl(null));
    return () => {
      stale = true;
    };
  }, [selected, schemaOptions.ignoreWhitespace]);

  // ---------- data view state ----------
  const [selectedTable, setSelectedTable] = useState<string | null>(null);
  const [checkedTables, setCheckedTables] = useState<Set<string>>(new Set());
  const [tableDiff, setTableDiff] = useState<TableDataDiff | null>(null);
  const [tableBusy, setTableBusy] = useState(false);
  const [tableError, setTableError] = useState<string | null>(null);
  const [rowFilter, setRowFilter] = useState<RowFilter>("all");
  const [keyDialog, setKeyDialog] = useState<TableDataSummary | null>(null);

  useEffect(() => {
    const tables = data?.tables ?? [];
    setCheckedTables(new Set(tables.filter(scriptable).map((t) => t.id)));
    setSelectedTable((sel) =>
      sel && tables.some((t) => t.id === sel) ? sel : (tables.find(scriptable) ?? tables[0])?.id ?? null,
    );
  }, [data]);

  const shownTables = useMemo(() => (data?.tables ?? []).filter((t) => matchesData(t, dataFilter)), [data, dataFilter]);
  const selectedSummary = data?.tables.find((t) => t.id === selectedTable) ?? null;

  useEffect(() => {
    setTableDiff(null);
    setTableError(null);
    setRowFilter("all");
    const t = selectedSummary;
    if (!t || !data || t.error || t.status !== "different") return;
    let stale = false;
    setTableBusy(true);
    dbApi
      .tableData(left, right, t.id, dataOptions)
      .then((d) => !stale && setTableDiff(d))
      .catch((e) => !stale && setTableError(errorMessage(e)))
      .finally(() => !stale && setTableBusy(false));
    return () => {
      stale = true;
      setTableBusy(false);
    };
    // Re-fetch only for a new selection or a new comparison (which carries the paths/options).
  }, [selectedSummary, data]);

  // ---------- connections ----------
  const setSide = (side: Side, path: string) =>
    side === "left" ? onPathsChange(path, right) : onPathsChange(left, path);

  const labels: Record<Side, string> = {
    left: left ? dbPathLabel(left, Object.fromEntries(connections.map((c) => [c.id, c.name]))) : "left",
    right: right ? dbPathLabel(right, Object.fromEntries(connections.map((c) => [c.id, c.name]))) : "right",
  };

  const result = mode === "schema" ? schema : data;
  const sides: Record<Side, SideInfo> | null = result ? { left: result.left, right: result.right } : null;
  const crossEngine = !!sides && sides.left.engine !== sides.right.engine;
  const isBusy = busy[mode];
  const error = errors[mode];
  const scriptIds = mode === "schema" ? checkedObjs : checkedTables;

  const toggle = (set: Set<string>, ids: string[], on: boolean) => {
    const next = new Set(set);
    ids.forEach((id) => (on ? next.add(id) : next.delete(id)));
    return next;
  };

  // ---------- rendering ----------
  const s = schema?.stats;
  const schemaCounts: Record<SchemaFilter, number> = {
    all: schema?.objects.length ?? 0,
    diffs: s ? s.different + s.leftOnly + s.rightOnly : 0,
    leftOnly: s?.leftOnly ?? 0,
    rightOnly: s?.rightOnly ?? 0,
    same: s?.same ?? 0,
  };
  const tables = data?.tables ?? [];
  const dataCounts: Record<DataFilter, number> = {
    all: tables.length,
    diffs: tables.filter((t) => matchesData(t, "diffs")).length,
    same: tables.filter(tableIdentical).length,
    errors: tables.filter((t) => t.error).length,
  };

  const renderObjectRow = (i: number) => {
    const item = objectItems[i];
    if ("group" in item) {
      const Icon = KIND_ICON[item.group];
      const syncable = item.objs.filter((o) => o.status !== "same").map((o) => o.id);
      const on = syncable.filter((id) => checkedObjs.has(id)).length;
      const open = !collapsed.has(item.group);
      return (
        <div
          className="db-group"
          onClick={() =>
            setCollapsed((c) => {
              const next = new Set(c);
              if (open) next.add(item.group);
              else next.delete(item.group);
              return next;
            })
          }
        >
          <input
            type="checkbox"
            checked={syncable.length > 0 && on === syncable.length}
            ref={(el) => {
              if (el) el.indeterminate = on > 0 && on < syncable.length;
            }}
            disabled={!syncable.length}
            onClick={(e) => e.stopPropagation()}
            onChange={(e) => setCheckedObjs((c) => toggle(c, syncable, e.target.checked))}
            title="Include in the sync script"
          />
          {open ? <ChevronDown size={13} /> : <ChevronRight size={13} />}
          <Icon size={13} className="db-kind-icon" />
          <span>{KIND_LABEL[item.group]}</span>
          <span className="db-count">{item.objs.length}</span>
        </div>
      );
    }
    const o = item.obj;
    const Icon = STATUS_ICON[o.status];
    return (
      <div
        className={`db-obj s-${o.status}${o.id === selectedObj ? " selected" : ""}`}
        onClick={() => setSelectedObj(o.id)}
        title={`${objectName(o.name)} — ${STATUS_LABEL[o.status]}`}
      >
        <input
          type="checkbox"
          checked={checkedObjs.has(o.id)}
          disabled={o.status === "same"}
          onClick={(e) => e.stopPropagation()}
          onChange={(e) => setCheckedObjs((c) => toggle(c, [o.id], e.target.checked))}
          aria-label="Include in the sync script"
        />
        <Icon size={13} className={`s-${o.status}-fg`} />
        <span className="db-obj-name">
          {o.name.schema && <span className="muted">{o.name.schema}.</span>}
          {o.name.name}
        </span>
        {o.changes.length > 0 && <span className="db-count">{o.changes.length}</span>}
      </div>
    );
  };

  const renderTableRow = (i: number) => {
    const t = shownTables[i];
    const name = objectName(t.left ?? t.right);
    const status = t.error ? (
      <span className="db-status warn" title={t.error}>
        <TriangleAlert size={12} /> {t.error}
      </span>
    ) : t.status === "leftOnly" || t.status === "rightOnly" ? (
      <span className="db-status s-leftOnly-fg">{STATUS_LABEL[t.status]}</span>
    ) : tableIdentical(t) ? (
      <span className="db-status ok">
        <CircleCheck size={12} /> Identical
      </span>
    ) : (
      <span className="db-status s-different-fg">
        {(t.different + t.leftOnly + t.rightOnly).toLocaleString()} row differences
      </span>
    );
    const n = (v: number, cls = "") => <span className={`num${v && cls ? ` ${cls}` : ""}`}>{v.toLocaleString()}</span>;
    const oneSided = t.status === "leftOnly" || t.status === "rightOnly";
    return (
      <div
        className={`db-trow${t.id === selectedTable ? " selected" : ""}${t.error ? " err" : ""}`}
        onClick={() => setSelectedTable(t.id)}
      >
        <span className="check">
          <input
            type="checkbox"
            checked={checkedTables.has(t.id)}
            disabled={!scriptable(t)}
            onClick={(e) => e.stopPropagation()}
            onChange={(e) => setCheckedTables((c) => toggle(c, [t.id], e.target.checked))}
            aria-label="Include in the sync script"
          />
        </span>
        <span className="name" title={name}>
          <Table2 size={13} className="db-kind-icon" /> {name}
        </span>
        <span className="key muted" title={t.key.join(", ")}>
          {t.key.length ? t.key.join(", ") : "—"}
          {dataOptions.keys[t.id] && <KeyRound size={11} className="db-key-override" />}
        </span>
        {n(t.leftRows)}
        {n(t.rightRows)}
        {oneSided ? <span /> : n(t.same)}
        {oneSided ? <span /> : n(t.different, "s-different-fg")}
        {oneSided ? <span /> : n(t.leftOnly, "s-leftOnly-fg")}
        {oneSided ? <span /> : n(t.rightOnly, "s-rightOnly-fg")}
        {status}
      </div>
    );
  };

  return (
    <div className="session db-compare" ref={root}>
      <div className="toolbar">
        <div className="seg-control">
          <button className={mode === "schema" ? "active" : ""} onClick={() => setMode("schema")}>
            <Layers size={14} /> Schema
          </button>
          <button className={mode === "data" ? "active" : ""} onClick={() => setMode("data")}>
            <Rows3 size={14} /> Data
          </button>
        </div>
        <span className="sep" />
        {mode === "schema" ? (
          <>
            <div className="seg-control">
              {(
                [
                  ["all", "All"],
                  ["diffs", "Differences"],
                  ["leftOnly", "Left only"],
                  ["rightOnly", "Right only"],
                  ["same", "Same"],
                ] as [SchemaFilter, string][]
              ).map(([f, label]) => (
                <button key={f} className={schemaFilter === f ? "active" : ""} onClick={() => setSchemaFilter(f)}>
                  {label}
                  {schema && <span className="sync-count">{schemaCounts[f]}</span>}
                </button>
              ))}
            </div>
            <label className="db-search">
              <Search size={13} />
              <input
                type="search"
                placeholder="Find object…"
                value={search}
                onChange={(e) => setSearch(e.target.value)}
                spellCheck={false}
              />
            </label>
          </>
        ) : (
          <div className="seg-control">
            {(
              [
                ["all", "All tables"],
                ["diffs", "Different"],
                ["same", "Identical"],
                ["errors", "Errors"],
              ] as [DataFilter, string][]
            ).map(([f, label]) => (
              <button key={f} className={dataFilter === f ? "active" : ""} onClick={() => setDataFilter(f)}>
                {label}
                {data && <span className="sync-count">{dataCounts[f]}</span>}
              </button>
            ))}
          </div>
        )}
        <span className="spacer" />
        <button onClick={() => setShowOptions(true)} title={`${mode === "schema" ? "Schema" : "Data"} compare options`}>
          <Settings2 size={14} /> Options
        </button>
        <button
          onClick={() => setScriptFor(mode)}
          disabled={!result || isBusy || scriptIds.size === 0}
          title={scriptIds.size ? `Generate a sync script for ${scriptIds.size} selected item(s)` : "Select differences to script"}
        >
          <ScrollText size={14} /> Generate script
        </button>
      </div>

      <div className="db-connbar">
        <ConnPicker
          label="Left"
          path={left}
          connections={connections}
          onChange={(p) => setSide("left", p)}
          onNew={() => setEditing({ side: "left" })}
          onEdit={(c) => setEditing({ side: "left", connection: c })}
        />
        <button className="icon-btn" title="Swap sides" disabled={!left && !right} onClick={() => onPathsChange(right, left)}>
          <ArrowLeftRight size={15} />
        </button>
        <ConnPicker
          label="Right"
          path={right}
          connections={connections}
          onChange={(p) => setSide("right", p)}
          onNew={() => setEditing({ side: "right" })}
          onEdit={(c) => setEditing({ side: "right", connection: c })}
        />
        <button className="primary" disabled={!ready || isBusy} onClick={() => compare(mode)} title={`Compare (${shortcutLabel("database.refresh")})`}>
          {isBusy ? <LoaderCircle size={14} className="db-spin" /> : <Play size={14} />} Compare
        </button>
      </div>

      {sides && (
        <div className="infobar db-info">
          {(["left", "right"] as Side[]).map((side) => (
            <span key={side} className="db-side" title={`${sides[side].label}\n${sides[side].serverVersion}`}>
              <EngineBadge engine={sides[side].engine} />
              <span className="db-side-label">{sides[side].label}</span>
              {sides[side].serverVersion && <span className="db-side-version">{sides[side].serverVersion}</span>}
            </span>
          ))}
        </div>
      )}
      {crossEngine && sides && (
        <div className="banner info db-banner">
          <TriangleAlert size={14} /> Comparing {ENGINE_INFO[sides.left.engine].label} with{" "}
          {ENGINE_INFO[sides.right.engine].label}: data types and code bodies are engine-specific and rarely match
          literally. Review generated scripts carefully.
        </div>
      )}
      {notice && (
        <div className={`banner ${notice.tone} db-banner`} onClick={() => setNotice(null)}>
          {notice.tone === "ok" ? <CircleCheck size={14} /> : <TriangleAlert size={14} />} {notice.text}
        </div>
      )}

      <div className="session-body">
        {isBusy && mode === "schema" && <div className="overlay">Reading both schemas…</div>}
        {isBusy && mode === "data" && (
          <div className="overlay">
            <div className="sync-progress">
              <div className="sync-progress-title">
                {cancelling ? "Cancelling…" : "Comparing data…"}
                <span className="muted">{progress && progress.total ? `${progress.done} / ${progress.total} tables` : ""}</span>
              </div>
              <div className="sync-bar">
                <div style={{ width: `${progress && progress.total ? (progress.done / progress.total) * 100 : 0}%` }} />
              </div>
              <div className="sync-progress-file">{progress?.current || "Reading schemas…"}</div>
              <div className="sync-progress-foot">
                <span className="muted">Stops after the current table</span>
                <button onClick={cancel} disabled={cancelling}>
                  Cancel
                </button>
              </div>
            </div>
          </div>
        )}
        {error && <div className="banner error">{error}</div>}

        {!ready && (
          <div className="empty-state db-empty">
            <p>Choose a saved connection (or a SQLite file) for both sides, then Compare.</p>
            <button onClick={() => setEditing({ side: left ? "right" : "left" })}>
              <Plus size={14} /> New connection
            </button>
          </div>
        )}

        {mode === "schema" && schema && (
          <div className="db-split">
            <div className="db-objects">
              {objectItems.length ? (
                <VirtualList count={objectItems.length} rowHeight={OBJ_ROW} renderRow={renderObjectRow} />
              ) : (
                <div className="empty-state">
                  {schema.objects.length ? "Nothing matches this filter." : "Both databases are empty."}
                </div>
              )}
            </div>
            <div className="db-detail">
              {selected ? (
                <ObjectDetail obj={selected} diff={ddl?.id === selected.id ? ddl.diff : null} labels={labels} />
              ) : (
                <div className="empty-state">Select an object to see its differences.</div>
              )}
            </div>
          </div>
        )}

        {mode === "data" && data && (
          <div className="db-data">
            <div className="db-tables">
              <div className="db-trow db-thead">
                <span className="check">
                  <input
                    type="checkbox"
                    checked={tables.some(scriptable) && tables.filter(scriptable).every((t) => checkedTables.has(t.id))}
                    disabled={!tables.some(scriptable)}
                    onChange={(e) =>
                      setCheckedTables((c) => toggle(c, tables.filter(scriptable).map((t) => t.id), e.target.checked))
                    }
                    title="Include all differing tables in the sync script"
                  />
                </span>
                <span>Table</span>
                <span>Key</span>
                <span className="num">Left rows</span>
                <span className="num">Right rows</span>
                <span className="num">Same</span>
                <span className="num">Different</span>
                <span className="num">Left only</span>
                <span className="num">Right only</span>
                <span>Status</span>
              </div>
              {shownTables.length ? (
                <VirtualList count={shownTables.length} rowHeight={24} renderRow={renderTableRow} />
              ) : (
                <div className="empty-state">{tables.length ? "Nothing matches this filter." : "No tables to compare."}</div>
              )}
            </div>
            <div className="db-rows">
              {selectedSummary ? (
                <TablePane
                  summary={selectedSummary}
                  diff={tableDiff}
                  busy={tableBusy}
                  error={tableError}
                  filter={rowFilter}
                  onFilter={setRowFilter}
                  keyOverridden={!!dataOptions.keys[selectedSummary.id]}
                  onEditKey={() => setKeyDialog(selectedSummary)}
                />
              ) : (
                <div className="empty-state">Select a table to see its differing rows.</div>
              )}
            </div>
          </div>
        )}
      </div>

      <div className="statusbar">
        {mode === "schema" && s && schema && (
          <>
            {schemaCounts.diffs === 0 ? (
              <span className="ok">Schemas are identical ({s.same.toLocaleString()} objects)</span>
            ) : (
              <>
                <span className="s-different-fg">{s.different} different</span>
                <span className="s-leftOnly-fg">{s.leftOnly} left only</span>
                <span className="s-rightOnly-fg">{s.rightOnly} right only</span>
                <span className="muted">{s.same} same</span>
              </>
            )}
            <span className="spacer" />
            <span className="muted">{checkedObjs.size} selected for sync</span>
          </>
        )}
        {mode === "data" && data && (
          <>
            {data.identical ? (
              <span className="ok">Data is identical ({tables.length} tables)</span>
            ) : (
              <>
                <span className="s-different-fg">{dataCounts.diffs} tables differ</span>
                <span className="muted">{dataCounts.same} identical</span>
                {dataCounts.errors > 0 && <span className="s-error-fg">{dataCounts.errors} not comparable</span>}
              </>
            )}
            <span className="spacer" />
            <span className="muted">{checkedTables.size} selected for sync</span>
          </>
        )}
      </div>

      {editing && (
        <DbConnectionDialog
          connection={editing.connection}
          onClose={() => setEditing(null)}
          onSaved={(c) => {
            loadConnections();
            if (!editing.connection) setSide(editing.side, dbPath(c.id));
          }}
          onDeleted={() => loadConnections()}
        />
      )}

      {showOptions && mode === "schema" && (
        <SchemaOptionsDialog value={schemaOptions} onApply={setSchemaOptions} onClose={() => setShowOptions(false)} />
      )}
      {showOptions && mode === "data" && (
        <DataOptionsDialog value={dataOptions} onApply={setDataOptions} onClose={() => setShowOptions(false)} />
      )}

      {keyDialog && (
        <KeyDialog
          summary={keyDialog}
          current={dataOptions.keys[keyDialog.id] ?? null}
          onClose={() => setKeyDialog(null)}
          onApply={(cols) =>
            setDataOptions((o) => {
              const keys = { ...o.keys };
              if (cols) keys[keyDialog.id] = cols;
              else delete keys[keyDialog.id];
              return { ...o, keys };
            })
          }
        />
      )}

      {scriptFor && result && (
        <ScriptDialog
          mode={scriptFor}
          paths={{ left, right }}
          readOnly={readOnlySides}
          labels={labels}
          sides={{ left: result.left, right: result.right }}
          schemaOptions={schemaOptions}
          dataOptions={dataOptions}
          ids={[...(scriptFor === "schema" ? checkedObjs : checkedTables)]}
          options={scriptOptions}
          onOptionsChange={setScriptOptions}
          onClose={() => setScriptFor(null)}
          onExecuted={(text) => {
            setScriptFor(null);
            setNotice({ tone: "ok", text });
            compare(scriptFor);
          }}
        />
      )}
    </div>
  );
}

// ---------- connection picker ----------

function ConnPicker({
  label,
  path,
  connections,
  onChange,
  onNew,
  onEdit,
}: {
  label: string;
  path: string;
  connections: DbConnection[];
  onChange: (path: string) => void;
  onNew: () => void;
  onEdit: (c: DbConnection) => void;
}) {
  const parsed = parseDbPath(path);
  const conn = parsed ? connections.find((c) => c.id === parsed.id) : undefined;
  const isFile = !!path && !parsed;
  const engine: Engine | null = conn?.config.engine ?? (isFile ? "sqlite" : null);
  const [dbs, setDbs] = useState<string[] | null>(null);
  const [dbState, setDbState] = useState<"idle" | "loading" | "error">("idle");
  const [dbError, setDbError] = useState("");

  const connId = conn?.id;
  useEffect(() => {
    setDbs(null);
    setDbState("idle");
  }, [connId]);

  const loadDatabases = () => {
    if (!conn || dbs || dbState === "loading") return;
    setDbState("loading");
    dbApi
      .listDatabases({ id: conn.id, name: conn.name, config: conn.config })
      .then((list) => {
        setDbs(list);
        setDbState("idle");
      })
      .catch((e) => {
        setDbs([]);
        setDbState("error");
        setDbError(errorMessage(e));
      });
  };

  const pick = async (v: string) => {
    if (v === NEW_CONNECTION) return onNew();
    if (v === OPEN_FILE) {
      const file = await open({ multiple: false, directory: false, filters: SQLITE_FILTERS, title: "SQLite database" });
      if (typeof file === "string") onChange(file);
      return;
    }
    onChange(v);
  };

  const value = parsed ? dbPath(parsed.id) : path;
  const defaultDb = conn?.config.database ?? "";
  const override = parsed?.database ?? "";
  return (
    <div className="db-picker">
      <span className="pathfield-label">{label}</span>
      <EngineBadge engine={engine} />
      <select
        className="db-conn-select"
        value={value}
        onChange={(e) => pick(e.target.value)}
        title={conn ? describeConfig(conn.config) : isFile ? path : undefined}
      >
        <option value="" disabled>
          Choose a connection…
        </option>
        {connections.map((c) => (
          <option key={c.id} value={dbPath(c.id)}>
            {c.name} — {c.config.engine ? ENGINE_INFO[c.config.engine].short : "?"} · {describeConfig(c.config)}
          </option>
        ))}
        {parsed && !conn && <option value={value}>{parsed.id} (connection not found)</option>}
        {isFile && <option value={path}>SQLite file — {basename(path)}</option>}
        <option disabled>──────────</option>
        <option value={OPEN_FILE}>Open SQLite file…</option>
        <option value={NEW_CONNECTION}>New connection…</option>
      </select>
      {conn && conn.config.engine !== "sqlite" && (
        <select
          className="db-db-select"
          value={override}
          onFocus={loadDatabases}
          onMouseDown={loadDatabases}
          onChange={(e) => onChange(dbPath(conn.id, e.target.value || null))}
          title={dbState === "error" ? `Couldn't list databases: ${dbError}` : "Database to compare"}
        >
          <option value="">{defaultDb ? `${defaultDb} (default)` : "(default database)"}</option>
          {override && !dbs?.includes(override) && <option value={override}>{override}</option>}
          {dbState === "loading" && <option disabled>Loading…</option>}
          {dbState === "error" && <option disabled>Couldn't list databases</option>}
          {dbs
            ?.filter((d) => d !== defaultDb)
            .map((d) => (
              <option key={d} value={d}>
                {d}
              </option>
            ))}
        </select>
      )}
      <button className="icon-btn" title="Edit connection" disabled={!conn} onClick={() => conn && onEdit(conn)}>
        <Pencil size={13} />
      </button>
      <button className="icon-btn" title="New connection" onClick={onNew}>
        <Plus size={14} />
      </button>
    </div>
  );
}

// ---------- schema detail ----------

function ObjectDetail({
  obj,
  diff,
  labels,
}: {
  obj: ObjectDiff;
  diff: TextDiffResult | null;
  labels: Record<Side, string>;
}) {
  const KindIcon = KIND_ICON[obj.kind];
  const StatusIcon = STATUS_ICON[obj.status];
  const summary =
    obj.status === "same"
      ? "The definitions match."
      : obj.status === "leftOnly"
        ? `Exists only in ${labels.left}. Syncing left → right creates it; right → left drops it.`
        : obj.status === "rightOnly"
          ? `Exists only in ${labels.right}. Syncing right → left creates it; left → right drops it.`
          : null;
  return (
    <div className="db-detail-inner">
      <div className="db-detail-head">
        <KindIcon size={15} className="db-kind-icon" />
        <span className="db-detail-name">{objectName(obj.name)}</span>
        <span className={`db-pill s-${obj.status}-fg`}>
          <StatusIcon size={12} /> {STATUS_LABEL[obj.status]}
        </span>
      </div>
      {(obj.changes.length > 0 || summary) && (
        <ul className="db-changes">
          {obj.changes.map((c, i) => (
            <li key={i}>{c}</li>
          ))}
          {summary && obj.changes.length === 0 && <li className="muted">{summary}</li>}
        </ul>
      )}
      <div className="db-ddl-head">
        <span>{labels.left}</span>
        <span>{labels.right}</span>
      </div>
      <div className="db-ddl">
        {!diff ? (
          <div className="empty-state">{obj.leftDdl || obj.rightDdl ? "Diffing…" : "No definition available."}</div>
        ) : (
          diff.rows.map((row, i) => (
            <div key={i} className={`trow k-${row.kind}`}>
              <span className="ln">{row.leftNo ?? ""}</span>
              <div className={`tcell${row.left ? "" : " absent"}`}>{row.left && <Segments segs={row.left} />}</div>
              <span className="mid" />
              <span className="ln">{row.rightNo ?? ""}</span>
              <div className={`tcell${row.right ? "" : " absent"}`}>{row.right && <Segments segs={row.right} />}</div>
            </div>
          ))
        )}
      </div>
    </div>
  );
}

// ---------- data rows ----------

function TablePane({
  summary,
  diff,
  busy,
  error,
  filter,
  onFilter,
  keyOverridden,
  onEditKey,
}: {
  summary: TableDataSummary;
  diff: TableDataDiff | null;
  busy: boolean;
  error: string | null;
  filter: RowFilter;
  onFilter: (f: RowFilter) => void;
  keyOverridden: boolean;
  onEditKey: () => void;
}) {
  const name = objectName(summary.left ?? summary.right);
  const counts: Record<RowFilter, number> = {
    all: summary.different + summary.leftOnly + summary.rightOnly,
    different: summary.different,
    leftOnly: summary.leftOnly,
    rightOnly: summary.rightOnly,
  };
  const extra = [
    summary.leftOnlyColumns.length ? `only in left: ${summary.leftOnlyColumns.join(", ")}` : "",
    summary.rightOnlyColumns.length ? `only in right: ${summary.rightOnlyColumns.join(", ")}` : "",
  ].filter(Boolean);

  let body;
  if (summary.error) body = <div className="banner error">{summary.error}</div>;
  else if (summary.status === "leftOnly" || summary.status === "rightOnly")
    body = (
      <div className="empty-state">
        This table exists only in the {summary.status === "leftOnly" ? "left" : "right"} database. Compare schemas to
        create it on the other side.
      </div>
    );
  else if (summary.status === "same") body = <div className="empty-state">All {summary.same.toLocaleString()} rows are identical.</div>;
  else if (error) body = <div className="banner error">{error}</div>;
  else if (busy || !diff) body = <div className="empty-state">Loading rows…</div>;
  else body = <DataGrid diff={diff} filter={filter} />;

  return (
    <>
      <div className="db-rows-head">
        <Table2 size={13} className="db-kind-icon" />
        <b>{name}</b>
        {summary.key.length > 0 && (
          <span className="muted">
            <KeyRound size={11} /> {summary.key.join(", ")}
          </span>
        )}
        <button className="db-mini" onClick={onEditKey} disabled={!summary.columns.length} title="Choose the columns rows are matched on">
          {keyOverridden ? "Custom key…" : "Key…"}
        </button>
        {extra.length > 0 && (
          <span className="tbl-warn" title={`Not compared — columns ${extra.join("; ")}`}>
            <TriangleAlert size={12} /> {extra.join("; ")}
          </span>
        )}
        <span className="spacer" />
        {diff?.truncated && (
          <span className="tbl-warn">
            <TriangleAlert size={12} /> Showing the first {diff.rows.length.toLocaleString()} of {counts.all.toLocaleString()} differing rows
          </span>
        )}
        {summary.status === "different" && !summary.error && (
          <div className="seg-control">
            {(
              [
                ["all", "All"],
                ["different", "Different"],
                ["leftOnly", "Left only"],
                ["rightOnly", "Right only"],
              ] as [RowFilter, string][]
            ).map(([f, label]) => (
              <button key={f} className={filter === f ? "active" : ""} onClick={() => onFilter(f)}>
                {label}
                <span className="sync-count">{counts[f].toLocaleString()}</span>
              </button>
            ))}
          </div>
        )}
      </div>
      {body}
    </>
  );
}

/** Side-by-side row grid in Table Compare's style: left values | right values, differing cells marked. */
function DataGrid({ diff, filter }: { diff: TableDataDiff; filter: RowFilter }) {
  const { summary } = diff;
  const columns = summary.columns;
  const keyCount = summary.key.length;
  const rows = useMemo(
    () => (filter === "all" ? diff.rows : diff.rows.filter((r) => r.status === filter)),
    [diff, filter],
  );
  const [hx, setHx] = useState(0);
  const [viewW, setViewW] = useState(400);
  const leftBar = useRef<HTMLDivElement>(null);
  const rightBar = useRef<HTMLDivElement>(null);

  const widths = useMemo(() => {
    const chars = columns.map((c, i) => Math.max(c.length, (summary.rightColumns[i] ?? "").length) + (i < keyCount ? 3 : 0));
    const step = Math.max(1, Math.floor(diff.rows.length / 300));
    for (let i = 0; i < diff.rows.length; i += step) {
      for (const cells of [diff.rows[i].left, diff.rows[i].right]) {
        cells?.forEach((v, c) => (chars[c] = Math.max(chars[c], v === null ? 4 : v.length)));
      }
    }
    return chars.map((n) => Math.round(Math.min(MAX_COL, Math.max(MIN_COL, n * CHAR_WIDTH + 14))));
  }, [columns, summary.rightColumns, keyCount, diff.rows]);
  const totalWidth = widths.reduce((a, b) => a + b, 0);
  const maxHx = Math.max(0, totalWidth - viewW);
  useEffect(() => setHx((x) => Math.min(x, maxHx)), [maxHx]);

  useLayoutEffect(() => {
    const el = leftBar.current;
    if (!el) return;
    const ro = new ResizeObserver(() => setViewW(el.clientWidth));
    ro.observe(el);
    setViewW(el.clientWidth);
    return () => ro.disconnect();
  }, []);
  useLayoutEffect(() => {
    for (const el of [leftBar.current, rightBar.current]) {
      if (el && Math.abs(el.scrollLeft - hx) > 0.5) el.scrollLeft = hx;
    }
  }, [hx]);

  const onWheel = (e: WheelEvent<HTMLDivElement>) => {
    const dx = e.shiftKey ? e.deltaY : e.deltaX;
    if (!dx || (!e.shiftKey && Math.abs(e.deltaX) < Math.abs(e.deltaY))) return;
    setHx((x) => Math.max(0, Math.min(maxHx, x + dx)));
  };
  const shift = { transform: `translateX(${-hx}px)` };

  const header = (side: Side) => (
    <div className="tbl-half">
      <div className="tbl-strip" style={shift}>
        {columns.map((c, i) => {
          const label = side === "left" ? c : summary.rightColumns[i] ?? c;
          return (
            <div key={i} className={`tbl-hcell db-hcell${i < keyCount ? " key" : ""}`} style={{ width: widths[i] }} title={label}>
              {i < keyCount && <KeyRound size={11} className="tbl-key-icon" />}
              <span className="tbl-hlabel">{label}</span>
            </div>
          );
        })}
      </div>
    </div>
  );

  const cells = (values: Cell[] | null, diffs: number[]) => (
    <div className={`tbl-half${values ? "" : " absent"}`}>
      {values && (
        <div className="tbl-strip" style={shift}>
          {values.map((v, i) => (
            <div
              key={i}
              className={`tbl-cell${diffs.includes(i) ? " diff" : ""}${i < keyCount ? " key" : ""}`}
              style={{ width: widths[i] }}
              title={v !== null && v.length > 8 ? v : undefined}
            >
              <CellText v={v} />
            </div>
          ))}
        </div>
      )}
    </div>
  );

  const renderRow = (i: number) => {
    const r = rows[i];
    return (
      <div className={`tbl-row s-${r.status}`}>
        <span className="ln">{r.left ? i + 1 : ""}</span>
        {cells(r.left, r.diffs)}
        <span className="mid" />
        <span className="ln">{r.right ? i + 1 : ""}</span>
        {cells(r.right, r.diffs)}
      </div>
    );
  };

  return (
    <div className="tbl-grid db-grid" onWheel={onWheel}>
      <div className="tbl-row tbl-head">
        <span className="ln" />
        {header("left")}
        <span className="mid" />
        <span className="ln" />
        {header("right")}
      </div>
      {rows.length ? (
        <VirtualList className="tbl-list" count={rows.length} rowHeight={DATA_ROW} renderRow={renderRow} />
      ) : (
        <div className="empty-state">No rows for this filter.</div>
      )}
      <div className="tbl-row tbl-hbar">
        <span />
        <div ref={leftBar} className="tbl-hscroll" onScroll={(e) => setHx(e.currentTarget.scrollLeft)}>
          <div style={{ width: totalWidth }} />
        </div>
        <span />
        <span />
        <div ref={rightBar} className="tbl-hscroll" onScroll={(e) => setHx(e.currentTarget.scrollLeft)}>
          <div style={{ width: totalWidth }} />
        </div>
      </div>
    </div>
  );
}

// ---------- options ----------

const SCHEMA_FLAGS: [keyof Omit<SchemaCompareOptions, "kinds">, string, string][] = [
  ["ignoreNameCase", "Ignore case in object and column names", "Match Users and users as the same object (SQL Server, MySQL on Windows)."],
  ["ignoreWhitespace", "Ignore whitespace in code", "Collapse runs of spaces and line breaks in views, routines, triggers and expressions."],
  ["ignoreComments", "Ignore comments in code", "Strip -- and /* */ comments from code objects before comparing."],
  ["ignoreColumnOrder", "Ignore column order", "The same columns in a different order are not a difference."],
  ["ignoreCollation", "Ignore collations", "Column collation differences are not reported."],
  ["ignoreConstraintNames", "Ignore constraint names", "Match keys, foreign keys, checks and unique constraints by definition (system names like PK__Users__3214EC07 differ)."],
  ["ignoreIdentitySeed", "Ignore identity seed and increment", "IDENTITY / AUTO_INCREMENT seeds differ between copies of a database."],
];

function SchemaOptionsDialog({
  value,
  onApply,
  onClose,
}: {
  value: SchemaCompareOptions;
  onApply: (o: SchemaCompareOptions) => void;
  onClose: () => void;
}) {
  const [draft, setDraft] = useState(value);
  const kinds = draft.kinds.length ? draft.kinds : OBJECT_KINDS;
  const toggleKind = (k: ObjectKind, on: boolean) => {
    const next = OBJECT_KINDS.filter((x) => (x === k ? on : kinds.includes(x)));
    setDraft({ ...draft, kinds: next.length === OBJECT_KINDS.length ? [] : next });
  };
  return (
    <Modal
      title="Schema compare options"
      wide
      onClose={onClose}
      footer={
        <>
          <button onClick={() => setDraft(DEFAULT_SCHEMA_OPTIONS)}>Defaults</button>
          <span className="spacer" />
          <button onClick={onClose}>Cancel</button>
          <button
            className="primary"
            disabled={!kinds.length}
            onClick={() => {
              onApply(draft);
              onClose();
            }}
          >
            Apply
          </button>
        </>
      }
    >
      <div className="settings-grid">
        <fieldset>
          <legend>Comparison</legend>
          {SCHEMA_FLAGS.map(([key, label, hint]) => (
            <label key={key} className="field radio">
              <input type="checkbox" checked={draft[key]} onChange={(e) => setDraft({ ...draft, [key]: e.target.checked })} />
              <span>
                {label}
                <small className="muted">{hint}</small>
              </span>
            </label>
          ))}
        </fieldset>
        <fieldset>
          <legend>Object types</legend>
          {OBJECT_KINDS.map((k) => {
            const Icon = KIND_ICON[k];
            return (
              <label key={k} className="field check">
                <input type="checkbox" checked={kinds.includes(k)} onChange={(e) => toggleKind(k, e.target.checked)} />
                <Icon size={13} className="db-kind-icon" /> {KIND_LABEL[k]}
              </label>
            );
          })}
          {!kinds.length && <small className="s-error-fg">Choose at least one object type.</small>}
        </fieldset>
      </div>
    </Modal>
  );
}

const TIME_PRECISION: [string, string][] = [
  ["", "All digits"],
  ["0", "Whole seconds"],
  ["3", "Milliseconds (3)"],
  ["6", "Microseconds (6)"],
];

function DataOptionsDialog({
  value,
  onApply,
  onClose,
}: {
  value: DataCompareOptions;
  onApply: (o: DataCompareOptions) => void;
  onClose: () => void;
}) {
  const [draft, setDraft] = useState(value);
  const [tol, setTol] = useState(String(value.rules.numericTolerance ?? 0.0001));
  const [maxRows, setMaxRows] = useState(String(value.maxRows));
  const rules = draft.rules;
  const setRules = (patch: Partial<DataCompareOptions["rules"]>) => setDraft({ ...draft, rules: { ...rules, ...patch } });
  const tolValue = Number(tol);
  const maxValue = Math.floor(Number(maxRows));
  const valid = (rules.numericTolerance === null || (Number.isFinite(tolValue) && tolValue >= 0)) && maxValue >= 1;
  const overrides = Object.keys(draft.keys);

  return (
    <Modal
      title="Data compare options"
      wide
      onClose={onClose}
      footer={
        <>
          <button
            onClick={() => {
              setDraft({ ...DEFAULT_DATA_OPTIONS, keys: draft.keys });
              setMaxRows(String(DEFAULT_DATA_OPTIONS.maxRows));
            }}
          >
            Defaults
          </button>
          <span className="spacer" />
          <button onClick={onClose}>Cancel</button>
          <button
            className="primary"
            disabled={!valid}
            onClick={() => {
              onApply({
                ...draft,
                rules: { ...rules, numericTolerance: rules.numericTolerance === null ? null : tolValue },
                maxRows: maxValue,
              });
              onClose();
            }}
          >
            Apply
          </button>
        </>
      }
    >
      <div className="settings-grid">
        <fieldset>
          <legend>Values are equal when</legend>
          <label className="field radio">
            <input type="checkbox" checked={rules.ignoreCase} onChange={(e) => setRules({ ignoreCase: e.target.checked })} />
            <span>
              Text differs only in case
              <small className="muted">“ABC” matches “abc”.</small>
            </span>
          </label>
          <label className="field radio">
            <input
              type="checkbox"
              checked={rules.trimWhitespace}
              onChange={(e) => setRules({ trimWhitespace: e.target.checked })}
            />
            <span>
              Text differs only in leading/trailing spaces
              <small className="muted">CHAR padding differs between engines.</small>
            </span>
          </label>
          <label className="field radio">
            <input
              type="checkbox"
              checked={rules.numericTolerance !== null}
              onChange={(e) => setRules({ numericTolerance: e.target.checked ? tolValue || 0 : null })}
            />
            <span>
              Numbers are within a tolerance
              <span className="input-row db-inline">
                ±
                <input
                  type="number"
                  min={0}
                  step="any"
                  value={tol}
                  disabled={rules.numericTolerance === null}
                  onChange={(e) => setTol(e.target.value)}
                />
              </span>
            </span>
          </label>
          <label className="field">
            <span>Compare date/time fractions to</span>
            <select
              value={rules.timePrecision === null ? "" : String(rules.timePrecision)}
              onChange={(e) => setRules({ timePrecision: e.target.value === "" ? null : Number(e.target.value) })}
            >
              {TIME_PRECISION.map(([v, l]) => (
                <option key={v} value={v}>
                  {l}
                </option>
              ))}
            </select>
            <small className="muted">SQL Server datetime keeps about 3 digits; MySQL and PostgreSQL keep 6.</small>
          </label>
        </fieldset>
        <fieldset>
          <legend>Tables</legend>
          <label className="field check">
            <input
              type="checkbox"
              checked={draft.ignoreNameCase}
              onChange={(e) => setDraft({ ...draft, ignoreNameCase: e.target.checked })}
            />
            Ignore case in table and column names
          </label>
          <label className="field">
            <span>Rows shown per table</span>
            <input type="number" min={1} step={100} value={maxRows} onChange={(e) => setMaxRows(e.target.value)} />
            <small className="muted">Only for display; sync scripts always cover every differing row.</small>
          </label>
          <div className="field">
            <span>Custom keys</span>
            {overrides.length ? (
              overrides.map((id) => (
                <div key={id} className="db-override">
                  <KeyRound size={11} /> <code>{id}</code> → {draft.keys[id].join(", ")}
                  <button
                    className="link"
                    onClick={() => {
                      const keys = { ...draft.keys };
                      delete keys[id];
                      setDraft({ ...draft, keys });
                    }}
                  >
                    reset
                  </button>
                </div>
              ))
            ) : (
              <small className="muted">None — rows match on the primary key (or a unique index). Use “Key…” above a table’s rows to override.</small>
            )}
          </div>
        </fieldset>
      </div>
    </Modal>
  );
}

function KeyDialog({
  summary,
  current,
  onApply,
  onClose,
}: {
  summary: TableDataSummary;
  current: string[] | null;
  onApply: (cols: string[] | null) => void;
  onClose: () => void;
}) {
  const [cols, setCols] = useState<string[]>(current ?? summary.key);
  // Keep the table's column order regardless of click order.
  const ordered = summary.columns.filter((c) => cols.includes(c));
  return (
    <Modal
      title={`Key columns — ${objectName(summary.left ?? summary.right)}`}
      onClose={onClose}
      footer={
        <>
          <button
            disabled={!current}
            onClick={() => {
              onApply(null);
              onClose();
            }}
          >
            Use primary key
          </button>
          <span className="spacer" />
          <button onClick={onClose}>Cancel</button>
          <button
            className="primary"
            disabled={!ordered.length}
            onClick={() => {
              onApply(ordered);
              onClose();
            }}
          >
            Apply
          </button>
        </>
      }
    >
      <p className="muted db-hint">Rows are matched on these columns. They should be unique on both sides.</p>
      <div className="db-key-list">
        {summary.columns.map((c) => (
          <label key={c} className="field check">
            <input
              type="checkbox"
              checked={cols.includes(c)}
              onChange={(e) => setCols((x) => (e.target.checked ? [...x, c] : x.filter((y) => y !== c)))}
            />
            <code>{c}</code>
          </label>
        ))}
      </div>
    </Modal>
  );
}

// ---------- sync script ----------

function ScriptDialog({
  mode,
  paths,
  readOnly,
  labels,
  sides,
  schemaOptions,
  dataOptions,
  ids,
  options,
  onOptionsChange,
  onClose,
  onExecuted,
}: {
  mode: Mode;
  paths: Record<Side, string>;
  /** bc5:reports-script — `/lro`, `/rro`: scripts never run on that side. */
  readOnly?: ReadOnlySides;
  labels: Record<Side, string>;
  sides: Record<Side, SideInfo>;
  schemaOptions: SchemaCompareOptions;
  dataOptions: DataCompareOptions;
  ids: string[];
  options: Omit<ScriptOptions, "ids">;
  onOptionsChange: (o: Omit<ScriptOptions, "ids">) => void;
  onClose: () => void;
  onExecuted: (message: string) => void;
}) {
  const [script, setScript] = useState<SyncScript | null>(null);
  const [busy, setBusy] = useState<"generate" | "run" | null>("generate");
  const [error, setError] = useState<string | null>(null);
  const [confirming, setConfirming] = useState(false);
  const [copied, setCopied] = useState(false);
  const target: Side = options.direction === "leftToRight" ? "right" : "left";
  const targetLocked = !!readOnly?.[target]; // bc5:reports-script
  const targetEngine = sides[target].engine;
  const set = (patch: Partial<ScriptOptions>) => {
    setConfirming(false);
    onOptionsChange({ ...options, ...patch });
  };

  const key = JSON.stringify([options, ids]);
  useEffect(() => {
    let stale = false;
    setBusy("generate");
    setError(null);
    const opts: ScriptOptions = { ...options, ids };
    const req =
      mode === "schema"
        ? dbApi.schemaScript(paths.left, paths.right, schemaOptions, opts)
        : dbApi.dataScript(paths.left, paths.right, dataOptions, opts);
    req
      .then((s) => !stale && setScript(s))
      .catch((e) => {
        if (stale) return;
        setScript(null);
        setError(errorMessage(e));
      })
      .finally(() => !stale && setBusy(null));
    return () => {
      stale = true;
    };
    // `key` captures options + ids; paths and compare options are fixed while the dialog is open.
  }, [key]);

  const count = script?.statements.length ?? 0;
  const lines = useMemo(() => (script ? script.text.split("\n") : []), [script]);

  const copy = async () => {
    if (!script) return;
    await navigator.clipboard.writeText(script.text);
    setCopied(true);
    setTimeout(() => setCopied(false), 1500);
  };

  const saveAs = async () => {
    if (!script) return;
    const path = await save({
      title: "Save sync script",
      defaultPath: `sync-${basename(labels[target]).replace(/[^\w.-]+/g, "_")}.sql`,
      filters: [{ name: "SQL script", extensions: ["sql"] }],
    });
    if (!path) return;
    try {
      await mergeApi.saveTextFile(path, script.text, { encoding: "UTF-8", hasBom: false, lineEnding: "lf" });
    } catch (e) {
      setError(errorMessage(e));
    }
  };

  const run = async () => {
    if (!script || targetLocked) return;
    setBusy("run");
    setError(null);
    try {
      await dbApi.executeScript(paths[target], script.statements, script.transactional);
      onExecuted(`Ran ${count} statement${count === 1 ? "" : "s"} on ${labels[target]}. Comparing again…`);
    } catch (e) {
      setError(errorMessage(e));
      setConfirming(false);
      setBusy(null);
    }
  };

  type Flag = "includeDrops" | "includeInserts" | "includeUpdates" | "transactional" | "disableForeignKeys";
  const toggles: [Flag, string][] =
    mode === "schema"
      ? [
          ["includeDrops", "Drop objects that exist only in the target"],
          ["transactional", "Wrap in a transaction"],
        ]
      : [
          ["includeInserts", "Insert missing rows"],
          ["includeUpdates", "Update differing rows"],
          ["includeDrops", "Delete rows that exist only in the target"],
          ["disableForeignKeys", "Disable foreign key checks while applying"],
          ["transactional", "Wrap in a transaction"],
        ];

  const ddlNoRollback = mode === "schema" && targetEngine === "mysql";

  return (
    <Modal
      title={`${mode === "schema" ? "Schema" : "Data"} sync script`}
      wide
      onClose={onClose}
      footer={
        confirming ? (
          <>
            <span className="db-confirm-q">
              Run {count} statement{count === 1 ? "" : "s"} on <b>{labels[target]}</b>?
            </span>
            <span className="spacer" />
            <button onClick={() => setConfirming(false)} disabled={busy === "run"}>
              Cancel
            </button>
            <button className="danger" onClick={run} disabled={busy === "run"} autoFocus>
              {busy === "run" ? <LoaderCircle size={14} className="db-spin" /> : <Play size={14} />}
              {busy === "run" ? "Running…" : `Run on ${labels[target]}`}
            </button>
          </>
        ) : (
          <>
            <button onClick={copy} disabled={!script || !count}>
              <Copy size={14} /> {copied ? "Copied" : "Copy"}
            </button>
            <button onClick={saveAs} disabled={!script || !count}>
              <Save size={14} /> Save as…
            </button>
            <span className="spacer" />
            <button onClick={onClose}>Close</button>
            <button
              className="primary"
              onClick={() => setConfirming(true)}
              disabled={!script || !count || busy !== null || targetLocked}
              title={targetLocked ? LAUNCH_READ_ONLY_HINT : undefined}
            >
              <Play size={14} /> Run on {labels[target]}…
            </button>
          </>
        )
      }
    >
      <div className="db-script-opts">
        <label className="field">
          <span>Direction</span>
          <select value={options.direction} onChange={(e) => set({ direction: e.target.value as ScriptOptions["direction"] })}>
            <option value="leftToRight">Make right match left ({labels.left} → {labels.right})</option>
            <option value="rightToLeft">Make left match right ({labels.right} → {labels.left})</option>
          </select>
        </label>
        <div className="db-script-toggles">
          {toggles.map(([k, label]) => (
            <label key={k} className="field check">
              <input type="checkbox" checked={options[k]} onChange={(e) => set({ [k]: e.target.checked })} />
              {label}
            </label>
          ))}
        </div>
      </div>

      {confirming && (
        <div className="sync-confirm-warn db-confirm">
          <p>
            <TriangleAlert size={14} /> This changes <b>{labels[target]}</b> — {ENGINE_INFO[targetEngine].label}{" "}
            <code>{sides[target].label}</code> — by running {count} statement{count === 1 ? "" : "s"}. Make sure you have a
            backup.
          </p>
          <p>
            {script?.transactional
              ? "The statements run in one transaction and are rolled back if any of them fails."
              : "Statements run one at a time; ones that already ran stay applied if a later one fails."}
            {ddlNoRollback &&
              " MySQL and MariaDB commit DDL implicitly, so schema changes that ran before an error cannot be rolled back."}
          </p>
        </div>
      )}

      {error && <div className="banner error db-script-banner">{error}</div>}
      {script && script.warnings.length > 0 && (
        <ul className="db-warnings">
          {script.warnings.map((w, i) => (
            <li key={i}>
              <TriangleAlert size={12} /> {w}
            </li>
          ))}
        </ul>
      )}

      <div className="db-script-head">
        <span>
          {busy === "generate"
            ? "Generating…"
            : script
              ? `${count} statement${count === 1 ? "" : "s"} · ${ids.length} ${mode === "schema" ? "object" : "table"}${ids.length === 1 ? "" : "s"} · target ${ENGINE_INFO[targetEngine].label}`
              : ""}
        </span>
      </div>
      <pre className="db-script">
        {!script ? (
          busy === "generate" ? (
            "…"
          ) : (
            ""
          )
        ) : count === 0 ? (
          <span className="muted">-- Nothing to do: the selected items already match.</span>
        ) : lines.length > HIGHLIGHT_LINES ? (
          script.text
        ) : (
          lines.map((l, i) => (
            <div key={i} className={/^\s*--/.test(l) ? "c" : /^\s*GO\s*$/i.test(l) ? "go" : undefined}>
              {l || " "}
            </div>
          ))
        )}
      </pre>
    </Modal>
  );
}
