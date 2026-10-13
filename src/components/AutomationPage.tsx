// The Sync Automation tab: saved folder-sync tasks with their triggers and
// live status, global pause, and the run history. Everything updates from the
// backend's events (status, recorded runs, config changes); relative times
// re-render every 30 s.
import {
  Bell,
  CalendarClock,
  CircleCheck,
  CircleQuestionMark,
  CopyPlus,
  Download,
  ExternalLink,
  Eye,
  EyeOff,
  Hourglass,
  LoaderCircle,
  Pause,
  Pencil,
  Play,
  Plus,
  RefreshCw,
  RotateCcw,
  ScrollText,
  Settings,
  Square,
  Trash,
  TriangleAlert,
  Upload,
} from "lucide-react";
import { Fragment, useCallback, useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { errorMessage } from "../lib/api";
import {
  automationApi,
  onChanged,
  onRun,
  onStatus,
  type AutomationRun,
  type AutomationSettings,
  type ChannelWithSecrets,
  type SyncTask,
  type TaskIssue,
  type TaskStatus,
} from "../lib/automationApi";
import {
  RUN_STATUS_INFO,
  RUN_TRIGGER_LABELS,
  describeTrigger,
  formatDuration,
  relativeTime,
  runDuration,
  runStatusLabel,
  type Tone,
} from "../lib/automationFormat";
import { formatSize, formatTime } from "../lib/format";
import { syncApi } from "../lib/syncApi";
import "./Automation.css";
import { AutomationSettingsDialog, ExportTasksDialog, ImportTasksDialog } from "./AutomationDialogs";
import { AutomationTaskDialog, TRIGGER_ICONS } from "./AutomationTaskDialog";
import { MODES, RollbackDialog, modeLabel } from "./FolderSync";
import { NotificationChannelsDialog } from "./NotificationChannelsDialog";
import { SessionIcon } from "./SessionIcon";
import { SyncHistoryDialog } from "./SyncHistoryDialog";

export interface AutomationPrefill {
  /** Bumped per request, so asking twice with the same pair opens the editor twice. */
  seq: number;
  task: Partial<SyncTask>;
}

interface Props {
  /** "Automate…" in Folder Sync: open the editor for a new task pre-filled with this. */
  prefill: AutomationPrefill | null;
  onPrefillUsed: () => void;
  onToast: (message: string) => void;
  onHelp: () => void;
}

/** Subscribes to a Tauri event for the component's lifetime (the handler may change freely). */
function useEvent<T>(subscribe: (cb: (payload: T) => void) => Promise<() => void>, handler: (payload: T) => void) {
  const ref = useRef(handler);
  ref.current = handler;
  useEffect(() => {
    let unlisten: (() => void) | undefined;
    let cancelled = false;
    subscribe((p) => ref.current(p))
      .then((u) => (cancelled ? u() : (unlisten = u)))
      .catch(() => {});
    return () => {
      cancelled = true;
      unlisten?.();
    };
  }, [subscribe]);
}

function Badge({ tone, children, title }: { tone: Tone; children: ReactNode; title?: string }) {
  return (
    <span className={`auto-badge tone-${tone}`} title={title}>
      {children}
    </span>
  );
}

const HISTORY_LIMIT = 200;

export function AutomationPage({ prefill, onPrefillUsed, onToast, onHelp }: Props) {
  const [tasks, setTasks] = useState<SyncTask[] | null>(null);
  const [statuses, setStatuses] = useState<Record<string, TaskStatus>>({});
  const [settings, setSettings] = useState<AutomationSettings | null>(null);
  const [channels, setChannels] = useState<ChannelWithSecrets[]>([]);
  const [runs, setRuns] = useState<AutomationRun[] | null>(null);
  const [selected, setSelected] = useState<string | null>(null);
  /** The history shows this task's runs, or every task's when null. */
  const [historyTask, setHistoryTask] = useState<string | null>(null);
  const [expandedRun, setExpandedRun] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [editing, setEditing] = useState<SyncTask | null>(null);
  const [dialog, setDialog] = useState<"channels" | "settings" | "import" | "export" | null>(null);
  const [confirmDelete, setConfirmDelete] = useState<string | null>(null);
  const [jobView, setJobView] = useState<{ left: string; right: string; jobId: string; taskId: string } | null>(null);
  const [rollbackJob, setRollbackJob] = useState<string | null>(null);
  const [now, setNow] = useState(() => Date.now());

  useEffect(() => {
    const timer = window.setInterval(() => setNow(Date.now()), 30_000);
    return () => window.clearInterval(timer);
  }, []);

  // The initial loads fail together when the backend is unavailable: toast each message once.
  const lastError = useRef<string | null>(null);
  const fail = useCallback(
    (e: unknown) => {
      const m = errorMessage(e);
      setError(m);
      if (m !== lastError.current) onToast(m);
      lastError.current = m;
    },
    [onToast],
  );

  const loadStatuses = useCallback(
    () =>
      automationApi
        .statuses()
        .then((list) => setStatuses(Object.fromEntries(list.map((s) => [s.taskId, s]))))
        .catch(fail),
    [fail],
  );

  const loadConfig = useCallback(
    () =>
      Promise.all([automationApi.tasks(), automationApi.settings(), automationApi.channels()])
        .then(([t, s, c]) => {
          setTasks(t);
          setSettings(s);
          setChannels(c);
          setError(null);
        })
        .catch((e) => {
          setTasks((t) => t ?? []);
          fail(e);
        }),
    [fail],
  );

  const historySeq = useRef(0);
  const loadRuns = useCallback(() => {
    const n = ++historySeq.current;
    return automationApi
      .runs(historyTask, HISTORY_LIMIT)
      .then((r) => n === historySeq.current && setRuns(r))
      .catch((e) => {
        if (n === historySeq.current) setRuns([]);
        fail(e);
      });
  }, [historyTask, fail]);

  useEffect(() => {
    loadConfig();
    loadStatuses();
  }, [loadConfig, loadStatuses]);
  useEffect(() => {
    loadRuns();
  }, [loadRuns]);

  useEvent(onStatus, (s) => setStatuses((prev) => ({ ...prev, [s.taskId]: s })));
  useEvent(onRun, (r) => {
    if (!historyTask || historyTask === r.taskId) loadRuns();
    loadStatuses();
  });
  useEvent<void>(onChanged, () => {
    loadConfig();
    loadStatuses();
  });

  // "Automate…" from Folder Sync: a new task with that pair and its settings.
  const usedPrefill = useRef(0);
  useEffect(() => {
    if (!prefill || usedPrefill.current === prefill.seq) return;
    usedPrefill.current = prefill.seq;
    automationApi.newTask(prefill.task).then(setEditing, fail);
    onPrefillUsed();
  }, [prefill, onPrefillUsed, fail]);

  const upsert = (t: SyncTask) =>
    setTasks((list) => {
      const rest = list ?? [];
      return rest.some((x) => x.id === t.id) ? rest.map((x) => (x.id === t.id ? t : x)) : [...rest, t];
    });

  const newTask = () => automationApi.newTask().then(setEditing, fail);

  const setEnabled = (t: SyncTask, enabled: boolean) => {
    upsert({ ...t, enabled }); // optimistic; the reply (or onChanged) settles it
    automationApi.setEnabled(t.id, enabled).then(upsert, (e) => {
      upsert(t);
      fail(e);
    });
  };

  const runNow = (t: SyncTask) =>
    automationApi.runNow(t.id).then(() => onToast(`Started “${t.name}”`), fail);
  const cancel = (t: SyncTask) => automationApi.cancel(t.id).catch(fail);
  const duplicate = (t: SyncTask) =>
    automationApi.duplicate(t.id).then((copy) => {
      upsert(copy);
      setSelected(copy.id);
      onToast(`Duplicated as “${copy.name}”`);
    }, fail);
  const remove = (t: SyncTask) => {
    setConfirmDelete(null);
    automationApi.remove(t.id).then(() => {
      setTasks((list) => list?.filter((x) => x.id !== t.id) ?? null);
      if (selected === t.id) setSelected(null);
      if (historyTask === t.id) setHistoryTask(null);
      onToast(`Deleted “${t.name}”`);
    }, fail);
  };

  const togglePause = () => {
    if (!settings) return;
    const next = { ...settings, paused: !settings.paused };
    setSettings(next);
    automationApi.saveSettings(next).then(loadStatuses, (e) => {
      setSettings(settings);
      fail(e);
    });
  };

  const saved = (t: SyncTask, warnings: TaskIssue[]) => {
    upsert(t);
    setEditing(null);
    setSelected(t.id);
    onToast(
      warnings.length ? `Saved “${t.name}” — ${warnings.map((w) => w.message).join(" · ")}` : `Saved “${t.name}”`,
    );
    loadStatuses();
  };

  const showHistory = (t: SyncTask) => {
    setSelected(t.id);
    setHistoryTask(t.id);
  };

  // The job journal knows the pair even if the task was edited or deleted since.
  const openJob = (r: AutomationRun) => {
    if (!r.jobId) return;
    const jobId = r.jobId;
    syncApi.jobDetail(jobId).then(
      (d) => setJobView({ left: d.summary.left, right: d.summary.right, jobId, taskId: r.taskId }),
      fail,
    );
  };

  const taskName = useMemo(() => new Map((tasks ?? []).map((t) => [t.id, t.name])), [tasks]);
  const paused = !!settings?.paused;
  const runningCount = Object.values(statuses).filter((s) => s.state === "running").length;

  return (
    <div className="session automation">
      <header className="auto-head">
        <SessionIcon kind="automation" size={18} tile />
        <div className="auto-title">
          <h1>Sync Automation</h1>
          <p className="muted">
            Saved folder syncs that run on a schedule, when files change or at startup — with safety checks, retries and
            alerts when something goes wrong.
          </p>
        </div>
        <span className="spacer" />
        <div className="auto-head-actions">
          <button
            className={`auto-pause${paused ? " paused" : ""}`}
            onClick={togglePause}
            disabled={!settings}
            title={paused ? "Resume automatic runs" : "Stop all automatic runs until you resume (Run now still works)"}
          >
            {paused ? <Play size={14} /> : <Pause size={14} />} {paused ? "Resume all" : "Pause all"}
          </button>
          <button className="primary" onClick={newTask}>
            <Plus size={14} /> New task
          </button>
          <button onClick={() => setDialog("channels")} title="Microsoft Teams, Slack, email and webhooks">
            <Bell size={14} /> Notification channels
          </button>
          <button onClick={() => setDialog("import")} title="Import tasks from JSON">
            <Upload size={14} /> Import
          </button>
          <button onClick={() => setDialog("export")} disabled={!tasks?.length} title="Export tasks as JSON">
            <Download size={14} /> Export
          </button>
          <button onClick={() => setDialog("settings")} disabled={!settings} title="Background mode, start at login, limits">
            <Settings size={14} /> Settings
          </button>
          <button className="icon-btn" onClick={onHelp} title="Help: Sync Automation" aria-label="Help">
            <CircleQuestionMark size={15} />
          </button>
        </div>
      </header>

      {paused && (
        <div className="auto-paused">
          <Pause size={14} />
          <span>
            <b>Automation is paused.</b> Scheduled, real-time and startup runs are skipped until you resume; Run now
            still works.
          </span>
          <button onClick={togglePause}>
            <Play size={14} /> Resume
          </button>
        </div>
      )}
      {error && (
        <div className="banner error auto-error">
          <TriangleAlert size={14} /> {error}
          <span className="spacer" />
          <button
            onClick={() => {
              setError(null);
              lastError.current = null;
              loadConfig();
              loadStatuses();
              loadRuns();
            }}
          >
            <RefreshCw size={13} /> Retry
          </button>
        </div>
      )}

      <div className="auto-body">
        <section className={`auto-tasks${tasks?.length || runs?.length ? "" : " full"}`}>
          {tasks === null ? (
            <div className="empty-state">Loading…</div>
          ) : tasks.length === 0 ? (
            <div className="auto-empty">
              <CalendarClock size={34} strokeWidth={1.5} />
              <h2>Create your first automated sync</h2>
              <p className="muted">
                Keep a backup on a NAS up to date every night, mirror a project folder to OneDrive as you save, or sync
                two machines both ways — and get a Teams message or an email if it ever fails.
              </p>
              <button className="primary" onClick={newTask}>
                <Plus size={14} /> New task
              </button>
              <p className="muted auto-empty-hint">
                Tip: in a Folder Sync tab, <b>Automate…</b> turns the folders and settings you're using into a task.
              </p>
            </div>
          ) : (
            <>
              <div className="auto-thead">
                <span title="Enabled">On</span>
                <span>Task</span>
                <span>Folders</span>
                <span>Triggers</span>
                <span>Status</span>
                <span />
              </div>
              {tasks.map((t) => (
                <TaskRow
                  key={t.id}
                  task={t}
                  status={statuses[t.id]}
                  now={now}
                  paused={paused}
                  selected={selected === t.id}
                  confirmingDelete={confirmDelete === t.id}
                  onSelect={() => {
                    setSelected(t.id);
                    if (historyTask) setHistoryTask(t.id);
                  }}
                  onEnabled={(v) => setEnabled(t, v)}
                  onRun={() => runNow(t)}
                  onCancel={() => cancel(t)}
                  onEdit={() => setEditing(t)}
                  onDuplicate={() => duplicate(t)}
                  onHistory={() => showHistory(t)}
                  onDelete={() => setConfirmDelete(t.id)}
                  onConfirmDelete={(yes) => (yes ? remove(t) : setConfirmDelete(null))}
                />
              ))}
            </>
          )}
        </section>

        {/* Hidden until there's something to show (a fresh install shows only the call to action). */}
        {(!!tasks?.length || !!runs?.length) && (
        <section className="auto-history">
          <div className="auto-history-head">
            <ScrollText size={14} />
            <b>Run history</b>
            <div className="seg-control">
              <button className={historyTask ? undefined : "active"} onClick={() => setHistoryTask(null)}>
                All tasks
              </button>
              <button
                className={historyTask ? "active" : undefined}
                disabled={!selected}
                onClick={() => selected && setHistoryTask(selected)}
                title={selected ? undefined : "Select a task above"}
              >
                {historyTask ? (taskName.get(historyTask) ?? "Selected task") : "Selected task"}
              </button>
            </div>
            <span className="spacer" />
            {runningCount > 0 && (
              <span className="muted">
                <LoaderCircle size={12} className="sync-spin" /> {runningCount} running
              </span>
            )}
            <button className="icon-btn" onClick={loadRuns} title="Reload">
              <RefreshCw size={13} />
            </button>
          </div>
          <div className={`auto-hist-thead${historyTask ? " one-task" : ""}`}>
            <span>Started</span>
            {!historyTask && <span>Task</span>}
            <span>Trigger</span>
            <span className="num">Try</span>
            <span>Result</span>
            <span className="num">Done / total</span>
            <span className="num">Failed</span>
            <span className="num">Conflicts</span>
            <span className="num">Copied</span>
            <span className="num">Took</span>
            <span>Message</span>
            <span title="Notifications sent">
              <Bell size={12} />
            </span>
            <span />
          </div>
          <div className="auto-hist-list">
            {runs === null && <div className="empty-state">Loading…</div>}
            {runs?.length === 0 && (
              <div className="empty-state">{historyTask ? "This task hasn't run yet." : "No runs yet."}</div>
            )}
            {runs?.map((r) => (
              <Fragment key={r.id}>
                <HistoryRow
                  run={r}
                  showTask={!historyTask}
                  name={taskName.get(r.taskId) ?? r.taskName}
                  expanded={expandedRun === r.id}
                  onToggle={() => setExpandedRun(expandedRun === r.id ? null : r.id)}
                  onOpenJob={() => openJob(r)}
                />
                {expandedRun === r.id && <RunDetail run={r} />}
              </Fragment>
            ))}
          </div>
        </section>
        )}
      </div>

      {editing && (
        <AutomationTaskDialog
          initial={editing}
          channels={channels}
          onSaved={saved}
          onClose={() => setEditing(null)}
          onManageChannels={() => setDialog("channels")}
        />
      )}
      {dialog === "channels" && <NotificationChannelsDialog onClose={() => setDialog(null)} onChanged={setChannels} />}
      {dialog === "settings" && settings && (
        <AutomationSettingsDialog value={settings} onSaved={setSettings} onClose={() => setDialog(null)} />
      )}
      {dialog === "export" && tasks && <ExportTasksDialog tasks={tasks} onClose={() => setDialog(null)} />}
      {dialog === "import" && (
        <ImportTasksDialog
          onClose={() => setDialog(null)}
          onImported={(added) => {
            added.forEach(upsert);
            onToast(`Imported ${added.length} task${added.length === 1 ? "" : "s"}`);
            loadStatuses();
          }}
        />
      )}
      {jobView && (
        <SyncHistoryDialog
          left={jobView.left}
          right={jobView.right}
          modeLabel={modeLabel}
          busy={statuses[jobView.taskId]?.state === "running"}
          initialJob={jobView.jobId}
          onRollback={(id) => {
            setJobView(null);
            setRollbackJob(id);
          }}
          onHistoryForgotten={() => {}}
          onClose={() => setJobView(null)}
        />
      )}
      {rollbackJob && (
        <RollbackDialog
          jobId={rollbackJob}
          onDone={() => onToast("Rolled back")}
          onClose={() => setRollbackJob(null)}
        />
      )}
    </div>
  );
}

interface RowProps {
  task: SyncTask;
  status: TaskStatus | undefined;
  now: number;
  paused: boolean;
  selected: boolean;
  confirmingDelete: boolean;
  onSelect: () => void;
  onEnabled: (enabled: boolean) => void;
  onRun: () => void;
  onCancel: () => void;
  onEdit: () => void;
  onDuplicate: () => void;
  onHistory: () => void;
  onDelete: () => void;
  onConfirmDelete: (yes: boolean) => void;
}

/**
 * Paths are laid out right-to-left so a long one is cut at its start and the
 * folder name stays visible; the marks keep leading `\\` and trailing
 * separators in place.
 */
const ltr = (path: string) => `\u200e${path}\u200e`;

function TaskRow(p: RowProps) {
  const { task: t, status: s } = p;
  const mode = MODES.find((m) => m.value === t.mode) ?? MODES[0];
  const ModeIcon = mode.icon;
  const running = s?.state === "running";
  const queued = s?.state === "queued";

  return (
    <div
      className={`auto-row${p.selected ? " selected" : ""}${t.enabled ? "" : " disabled"}`}
      onClick={p.onSelect}
      onDoubleClick={p.onEdit}
    >
      <span onClick={(e) => e.stopPropagation()} onDoubleClick={(e) => e.stopPropagation()}>
        <input
          type="checkbox"
          className="auto-switch"
          checked={t.enabled}
          onChange={(e) => p.onEnabled(e.target.checked)}
          title={t.enabled ? "Enabled: runs automatically" : "Disabled: runs only with Run now"}
          aria-label="Enabled"
        />
      </span>
      <span className="auto-cell-task">
        <span className="auto-name" title={t.name}>
          {t.name}
        </span>
        {t.description && (
          <span className="muted auto-sub" title={t.description}>
            {t.description}
          </span>
        )}
      </span>
      <span className="auto-cell-folders" title={`${t.left}\n${mode.label}\n${t.right}`}>
        <span className="auto-pair">
          <span className="auto-path">{ltr(t.left || "—")}</span>
          <span className={`sync-mode-icon small${mode.deletes ? " mirror" : ""}`}>
            <ModeIcon size={12} />
          </span>
          <span className="auto-path">{ltr(t.right || "—")}</span>
        </span>
        <span className="muted auto-sub">{mode.label}</span>
      </span>
      <span className="auto-cell-triggers">
        {t.triggers.length === 0 && <span className="muted">Manual only</span>}
        {t.triggers.map((tr, i) => {
          const Icon = TRIGGER_ICONS[tr.kind];
          return (
            <span key={i} className="auto-chip" title={describeTrigger(tr)}>
              <Icon size={12} /> {describeTrigger(tr)}
            </span>
          );
        })}
      </span>
      <span className="auto-cell-status">
        <StatusCell task={t} status={s} now={p.now} paused={p.paused} onCancel={p.onCancel} />
      </span>
      <span className="auto-actions" onClick={(e) => e.stopPropagation()} onDoubleClick={(e) => e.stopPropagation()}>
        {p.confirmingDelete ? (
          <>
            <button className="danger" onClick={() => p.onConfirmDelete(true)} title={`Delete “${t.name}” and its history`}>
              Delete
            </button>
            <button onClick={() => p.onConfirmDelete(false)}>Keep</button>
          </>
        ) : (
          <>
            <button
              className="icon-btn"
              onClick={p.onRun}
              disabled={running || queued}
              title={running ? "Running" : queued ? "Queued" : "Run now"}
            >
              <Play size={14} />
            </button>
            <button className="icon-btn" onClick={p.onEdit} title="Edit">
              <Pencil size={14} />
            </button>
            <button className="icon-btn" onClick={p.onDuplicate} title="Duplicate">
              <CopyPlus size={14} />
            </button>
            <button className="icon-btn" onClick={p.onHistory} title="Show this task's runs">
              <ScrollText size={14} />
            </button>
            <button className="icon-btn" onClick={p.onDelete} title="Delete" disabled={running}>
              <Trash size={14} />
            </button>
          </>
        )}
      </span>
    </div>
  );
}

function StatusCell({
  task,
  status: s,
  now,
  paused,
  onCancel,
}: {
  task: SyncTask;
  status: TaskStatus | undefined;
  now: number;
  paused: boolean;
  onCancel: () => void;
}) {
  if (!s) return <span className="muted">—</span>;

  const watch = task.triggers.some((t) => t.kind === "realtime") && (
    s.watchError ? (
      <span className="auto-watch warn" title={`Not watching for changes: ${s.watchError}`}>
        <EyeOff size={13} />
      </span>
    ) : s.watching ? (
      <span className="auto-watch" title="Watching for changes">
        <Eye size={13} />
      </span>
    ) : null
  );
  const streak =
    s.consecutiveFailures > 1 ? (
      <Badge tone="bad" title={`The last ${s.consecutiveFailures} runs failed`}>
        {s.consecutiveFailures}× in a row
      </Badge>
    ) : null;

  if (s.state === "running") {
    const pr = s.progress;
    const pct = pr && pr.totalOps > 0 ? Math.round((pr.doneOps / pr.totalOps) * 100) : null;
    const text =
      !pr || pr.phase === "planning"
        ? "Scanning…"
        : `Copying ${pr.doneOps.toLocaleString()} / ${pr.totalOps.toLocaleString()}${
            pr.bytesTotal ? ` · ${formatSize(pr.bytesDone)} of ${formatSize(pr.bytesTotal)}` : ""
          }`;
    return (
      <>
        <span className="auto-status-line">
          <span className={`auto-bar${pct == null ? " indeterminate" : ""}`}>
            <span style={{ width: `${pct ?? 30}%` }} />
          </span>
          <span className="auto-pct">{pct != null && `${pct}%`}</span>
          <button className="icon-btn auto-cancel" onClick={onCancel} title="Cancel this run">
            <Square size={11} />
          </button>
          {s.rerunPending && (
            <Badge tone="muted" title="Changes arrived during this run; one more run follows it">
              +1
            </Badge>
          )}
          {watch}
        </span>
        <span className="muted auto-sub" title={pr?.current ?? undefined}>
          {text}
          {s.currentTrigger && ` · ${RUN_TRIGGER_LABELS[s.currentTrigger]}`}
          {s.attempt > 1 && ` · attempt ${s.attempt}`}
        </span>
      </>
    );
  }

  const last = s.lastStatus ? RUN_STATUS_INFO[s.lastStatus] : null;
  const lastText = s.lastRunMs ? `last run ${relativeTime(s.lastRunMs, now)}` : "never run";
  let next: string;
  if (s.state === "waitingToRetry") next = s.retryAtMs ? `retry ${relativeTime(s.retryAtMs, now)}` : "retrying soon";
  else if (!task.enabled) next = "disabled";
  else if (paused || s.state === "paused") next = "paused";
  else if (s.nextRunMs) next = `next ${relativeTime(s.nextRunMs, now)}`;
  else if (task.triggers.length === 0) next = "manual only";
  else next = "";

  return (
    <>
      <span className="auto-status-line">
        {s.state === "queued" ? (
          <Badge tone="accent">
            <Hourglass size={11} /> Queued
          </Badge>
        ) : s.state === "waitingToRetry" ? (
          <Badge tone="warn" title={s.lastStatus ? RUN_STATUS_INFO[s.lastStatus].label : undefined}>
            <RotateCcw size={11} /> Waiting to retry (attempt {s.attempt})
          </Badge>
        ) : last ? (
          <Badge tone={last.tone}>
            {s.lastStatus === "succeeded" && <CircleCheck size={11} />}
            {s.lastStatus && runStatusLabel(s.lastStatus)}
          </Badge>
        ) : (
          <span className="muted">Not run yet</span>
        )}
        {streak}
        {s.rerunPending && (
          <Badge tone="muted" title="Changes arrived during the last run; one more run is queued">
            +1 queued
          </Badge>
        )}
        {watch}
      </span>
      <span className="muted auto-sub">
        {lastText}
        {next && ` · ${next}`}
      </span>
    </>
  );
}

function HistoryRow({
  run: r,
  showTask,
  name,
  expanded,
  onToggle,
  onOpenJob,
}: {
  run: AutomationRun;
  showTask: boolean;
  name: string;
  expanded: boolean;
  onToggle: () => void;
  onOpenJob: () => void;
}) {
  const info = RUN_STATUS_INFO[r.status];
  const took = runDuration(r);
  const notified = r.notifications.length;
  const notifyFailed = r.notifications.some((n) => /fail|error|couldn't|could not/i.test(n));
  return (
    <div className={`auto-hist-row${showTask ? "" : " one-task"}${expanded ? " expanded" : ""}`} onClick={onToggle}>
      <span title={formatTime(r.startedMs)}>{formatTime(r.startedMs)}</span>
      {showTask && <span title={name}>{name}</span>}
      <span>{RUN_TRIGGER_LABELS[r.trigger]}</span>
      <span className="num">{r.attempt}</span>
      <span>
        <Badge tone={info.tone}>
          {r.status === "running" && <LoaderCircle size={11} className="sync-spin" />}
          {runStatusLabel(r.status, r.opsFailed)}
        </Badge>
      </span>
      <span className="num">
        {r.opsDone.toLocaleString()} / {r.opsTotal.toLocaleString()}
      </span>
      <span className={`num${r.opsFailed ? " s-error-fg" : ""}`}>{r.opsFailed || ""}</span>
      <span className={`num${r.conflicts ? " auto-warn-fg" : ""}`}>{r.conflicts || ""}</span>
      <span className="num">{r.bytesCopied ? formatSize(r.bytesCopied) : ""}</span>
      <span className="num">{took == null ? "" : formatDuration(took)}</span>
      <span className="auto-msg" title={r.message ?? undefined}>
        {r.message}
      </span>
      <span title={notified ? r.notifications.join("\n") : "No notifications"} className={notifyFailed ? "auto-warn-fg" : "muted"}>
        {notified > 0 && (
          <>
            <Bell size={11} /> {notified}
          </>
        )}
      </span>
      <span onClick={(e) => e.stopPropagation()}>
        {r.jobId && (
          <button className="icon-btn" onClick={onOpenJob} title="Open in Sync History (operations, roll back)">
            <ExternalLink size={13} />
          </button>
        )}
      </span>
    </div>
  );
}

function RunDetail({ run: r }: { run: AutomationRun }) {
  return (
    <div className="auto-run-detail">
      <div>
        <b>{r.taskName}</b> · started {formatTime(r.startedMs)}
        {r.finishedMs && ` · finished ${formatTime(r.finishedMs)}`} · {RUN_TRIGGER_LABELS[r.trigger]}
        {r.attempt > 1 && `, attempt ${r.attempt}`}
      </div>
      {r.message && <div className="auto-run-message">{r.message}</div>}
      {r.notifications.length > 0 ? (
        <ul>
          {r.notifications.map((n, i) => (
            <li key={i}>
              <Bell size={11} /> {n}
            </li>
          ))}
        </ul>
      ) : (
        <div className="muted">No notifications were sent for this run.</div>
      )}
      {r.jobId == null && r.opsTotal === 0 && <div className="muted">Nothing was changed.</div>}
    </div>
  );
}
