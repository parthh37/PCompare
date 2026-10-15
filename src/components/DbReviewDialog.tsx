// Review & apply for Database Compare: lists the arrow picks in plain
// English, then builds and runs the SQL in the background once confirmed.
import { save } from "@tauri-apps/plugin-dialog";
import { ArrowLeft, ArrowRight, Check, CircleX, Code, Copy, LoaderCircle, Pencil, Play, Plus, Save, Trash, TriangleAlert, X } from "lucide-react";
import { useMemo, useState } from "react";
import { errorMessage } from "../lib/api";
import {
  dbApi,
  DEFAULT_SCRIPT_OPTIONS,
  ENGINE_INFO,
  type DataCompareOptions,
  type DataCompareResult,
  type SchemaCompareOptions,
  type SchemaCompareResult,
  type SideInfo,
  type SyncScript,
} from "../lib/dbApi";
import {
  describeObject,
  describeRows,
  describeTable,
  explainApplyError,
  syncDirection,
  targetSide,
  type ChangePlan,
  type MoveDir,
  type PlainChange,
  type Side,
  type Tone,
} from "../lib/dbChanges";
import { mergeApi } from "../lib/mergeApi";
import { Modal } from "./Modal";

const TONE_ICON: Record<Tone, typeof Plus> = { add: Plus, change: Pencil, remove: Trash };
const DIRS: MoveDir[] = ["toRight", "toLeft"];

/** One background step: build a script for one direction, then run it on its target. */
interface Step {
  what: "schema" | "data";
  dir: MoveDir;
  label: string;
}
type StepState = { state: "waiting" | "running" | "done" | "skipped" } | { state: "failed"; error: string };

export function DbReviewDialog({
  plan,
  schema,
  data,
  paths,
  labels,
  sides,
  schemaOptions,
  dataOptions,
  onRemove,
  onClose,
  onApplied,
}: {
  plan: ChangePlan;
  schema: SchemaCompareResult | null;
  data: DataCompareResult | null;
  paths: Record<Side, string>;
  labels: Record<Side, string>;
  sides: Record<Side, SideInfo> | null;
  schemaOptions: SchemaCompareOptions;
  dataOptions: DataCompareOptions;
  onRemove: (ref: PlainChange["ref"]) => void;
  onClose: () => void;
  /** Called after every step finished (or after a failure, so the views refresh). */
  onApplied: (message: string, ok: boolean) => void;
}) {
  const changes = useMemo(() => {
    const out: PlainChange[] = [];
    for (const o of schema?.objects ?? []) {
      const dir = plan.objects.get(o.id);
      if (dir) out.push(describeObject(o, dir, labels));
    }
    for (const t of data?.tables ?? []) {
      const dir = plan.tables.get(t.id);
      if (dir) out.push(describeTable(t, dir, labels));
      const rows = plan.rows.get(t.id);
      if (rows?.size) out.push(...describeRows(t, [...rows.values()], labels));
    }
    return out;
  }, [plan, schema, data, labels]);

  const steps = useMemo(() => {
    const out: Step[] = [];
    for (const dir of DIRS) {
      if ([...plan.objects.values()].includes(dir))
        out.push({ what: "schema", dir, label: `Update the structure of ${labels[targetSide(dir)]}` });
    }
    for (const dir of DIRS) {
      const rows = [...plan.rows.values()].some((m) => [...m.values()].some((r) => r.dir === dir));
      if ([...plan.tables.values()].includes(dir) || rows)
        out.push({ what: "data", dir, label: `Copy rows into ${labels[targetSide(dir)]}` });
    }
    return out;
  }, [plan, labels]);

  /** Steps are snapshotted when applying starts; the plan may change underneath once views refresh. */
  const [run, setRun] = useState<{ steps: Step[]; state: StepState[] } | null>(null);
  const progress = run?.state ?? null;
  const [showSql, setShowSql] = useState(false);
  const [sql, setSql] = useState<{ step: Step; script: SyncScript }[] | null>(null);
  const [sqlError, setSqlError] = useState<string | null>(null);
  const [copied, setCopied] = useState(false);
  const running = !!progress && progress.some((p) => p.state === "running");
  const finished = !!progress && !running;

  /** The script for one step. Ids are never empty here: an empty list means "everything" to the engine. */
  const build = (step: Step): Promise<SyncScript> => {
    const base = { ...DEFAULT_SCRIPT_OPTIONS, direction: syncDirection(step.dir), transactional: true };
    if (step.what === "schema") {
      const ids = [...plan.objects].filter(([, d]) => d === step.dir).map(([id]) => id);
      return dbApi.schemaScript(paths.left, paths.right, schemaOptions, { ...base, ids, includeDrops: true });
    }
    const whole = [...plan.tables].filter(([, d]) => d === step.dir).map(([id]) => id);
    const rows: Record<string, (string | null)[][]> = {};
    plan.rows.forEach((picks, id) => {
      const keys = [...picks.values()].filter((p) => p.dir === step.dir).map((p) => p.key);
      if (keys.length) rows[id] = keys;
    });
    const ids = [...whole, ...Object.keys(rows)];
    return dbApi.dataScript(paths.left, paths.right, dataOptions, { ...base, ids, rows });
  };

  const apply = async () => {
    const state: StepState[] = steps.map(() => ({ state: "waiting" }));
    const show = () => setRun({ steps, state: [...state] });
    const warnings: string[] = [];
    let ran = 0;
    for (let i = 0; i < steps.length; i++) {
      state[i] = { state: "running" };
      show();
      try {
        const script = await build(steps[i]);
        warnings.push(...script.warnings);
        if (script.statements.length) {
          await dbApi.executeScript(paths[targetSide(steps[i].dir)], script.statements, script.transactional);
          ran++;
          state[i] = { state: "done" };
        } else {
          state[i] = { state: "skipped" };
        }
        show();
      } catch (e) {
        state[i] = { state: "failed", error: errorMessage(e) };
        for (let j = i + 1; j < steps.length; j++) state[j] = { state: "skipped" };
        show();
        onApplied(
          ran
            ? `“${steps[i].label}” failed, so it and the steps after it were not applied; the steps before it were. Comparing again…`
            : `Nothing was changed: ${explainApplyError(errorMessage(e)) ?? errorMessage(e)}`,
          false,
        );
        return;
      }
    }
    const extra = warnings.length ? ` Note: ${warnings.join(" ")}` : "";
    onApplied(`Applied ${changes.length} change${changes.length === 1 ? "" : "s"}. Comparing again…${extra}`, true);
  };

  const toggleSql = async () => {
    setShowSql((v) => !v);
    if (sql || showSql) return;
    setSqlError(null);
    try {
      const scripts = [];
      for (const step of steps) scripts.push({ step, script: await build(step) });
      setSql(scripts);
    } catch (e) {
      setSqlError(errorMessage(e));
    }
  };
  const sqlText = sql?.map(({ step, script }) => `-- ${step.label}\n${script.text}`).join("\n\n") ?? "";

  const saveSql = async () => {
    const path = await save({
      title: "Save SQL",
      defaultPath: `pcompare-changes.sql`,
      filters: [{ name: "SQL script", extensions: ["sql"] }],
    });
    if (!path) return;
    try {
      await mergeApi.saveTextFile(path, sqlText, { encoding: "UTF-8", hasBom: false, lineEnding: "lf" });
    } catch (e) {
      setSqlError(errorMessage(e));
    }
  };

  const removals = changes.filter((c) => c.tone === "remove").length;
  const mysqlTargets = steps.filter((s) => s.what === "schema" && sides?.[targetSide(s.dir)].engine === "mysql");

  return (
    <Modal
      title="Review changes"
      wide
      className="db-review"
      onClose={running ? () => {} : onClose}
      footer={
        <>
          {!progress && (
            <button onClick={toggleSql} disabled={!steps.length} title="See the SQL PCompare will run">
              <Code size={14} /> {showSql ? "Hide SQL" : "Show SQL"}
            </button>
          )}
          <span className="spacer" />
          {finished ? (
            <button className="primary" onClick={onClose}>
              Close
            </button>
          ) : (
            <>
              <button onClick={onClose} disabled={running}>
                Cancel
              </button>
              <button className={removals ? "danger" : "primary"} onClick={apply} disabled={!changes.length || running} autoFocus>
                {running ? <LoaderCircle size={14} className="db-spin" /> : <Play size={14} />}
                {running ? "Applying…" : `Apply ${changes.length} change${changes.length === 1 ? "" : "s"}`}
              </button>
            </>
          )}
        </>
      }
    >
      {run ? (
        <ol className="db-steps">
          {run.steps.map((s, i) => {
            const p = run.state[i];
            return (
              <li key={i} className={`db-step ${p.state}`}>
                {p.state === "running" ? (
                  <LoaderCircle size={15} className="db-spin" />
                ) : p.state === "done" ? (
                  <Check size={15} />
                ) : p.state === "failed" ? (
                  <CircleX size={15} />
                ) : (
                  <span className="db-step-dot" />
                )}
                <span>
                  {s.label}
                  {p.state === "skipped" && <small className="muted"> — skipped</small>}
                  {p.state === "failed" && (
                    <>
                      {explainApplyError(p.error) && <small className="db-step-reason">{explainApplyError(p.error)}</small>}
                      <small className="db-step-error">{p.error}</small>
                    </>
                  )}
                </span>
              </li>
            );
          })}
        </ol>
      ) : (
        <>
          <p className="db-review-intro">
            Nothing has changed yet. Check the list below, then press <b>Apply</b>.
          </p>
          {changes.length === 0 && <div className="empty-state">No changes picked. Use the ← → arrows next to a difference.</div>}
          {(["right", "left"] as Side[]).map((target) => {
            const mine = changes.filter((c) => c.target === target);
            if (!mine.length) return null;
            const Arrow = target === "right" ? ArrowRight : ArrowLeft;
            return (
              <section key={target} className="db-review-group">
                <h4>
                  <Arrow size={14} /> Changes to <b>{labels[target]}</b>
                  {sides && (
                    <span className="muted">
                      {" "}
                      · {ENGINE_INFO[sides[target].engine].label} · {sides[target].label}
                    </span>
                  )}
                </h4>
                <ul>
                  {mine.map((c) => {
                    const Icon = TONE_ICON[c.tone];
                    return (
                      <li key={JSON.stringify(c.ref)} className={`db-change t-${c.tone}`}>
                        <Icon size={14} className="db-change-icon" />
                        <div>
                          <div className="db-change-title">{c.title}</div>
                          {c.details.length > 0 && (
                            <ul className="db-change-details">
                              {c.details.map((d, i) => (
                                <li key={i}>{d}</li>
                              ))}
                            </ul>
                          )}
                        </div>
                        <button className="icon-btn" title="Leave this out" onClick={() => onRemove(c.ref)}>
                          <X size={13} />
                        </button>
                      </li>
                    );
                  })}
                </ul>
              </section>
            );
          })}
          {changes.length > 0 && (
            <div className="sync-confirm-warn db-review-warn">
              <p>
                <TriangleAlert size={14} />{" "}
                {removals
                  ? `${removals} of these delete${removals === 1 ? "s" : ""} something. Deleted objects and rows can’t be brought back without a backup.`
                  : "These changes write to your databases. Make sure you have a backup."}
              </p>
              <p>Changes are applied in all-or-nothing batches: if a batch fails, that batch is rolled back.</p>
              {mysqlTargets.length > 0 && (
                <p>
                  MySQL / MariaDB can’t undo structure changes, so if one fails partway, the ones before it stay applied.
                </p>
              )}
            </div>
          )}
          {showSql && (
            <div className="db-review-sql">
              {sqlError && <div className="banner error">{sqlError}</div>}
              <div className="db-script-head">
                <span>{sql ? "The SQL PCompare will run" : sqlError ? "" : "Building…"}</span>
                <span>
                  <button className="db-mini" disabled={!sql} onClick={() => void navigator.clipboard.writeText(sqlText).then(() => setCopied(true))}>
                    <Copy size={12} /> {copied ? "Copied" : "Copy"}
                  </button>{" "}
                  <button className="db-mini" disabled={!sql} onClick={saveSql}>
                    <Save size={12} /> Save as…
                  </button>
                </span>
              </div>
              <pre className="db-script">{sqlText}</pre>
            </div>
          )}
        </>
      )}
    </Modal>
  );
}
