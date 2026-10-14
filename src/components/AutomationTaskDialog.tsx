// The Sync Automation task editor: folders and mode, triggers, compare and
// execution options, safety guards and notifications. Validation runs in the
// engine (`automation_validate`) as you type; errors block saving, warnings
// don't.
import {
  ArrowUpDown,
  Bell,
  CalendarClock,
  CalendarDays,
  CircleX,
  Clock,
  Eye,
  FolderSync as FolderSyncIcon,
  Plus,
  Power,
  ShieldCheck,
  SlidersHorizontal,
  Timer,
  TriangleAlert,
  X,
  type LucideIcon,
} from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";
import { errorMessage, isRemote } from "../lib/api";
import {
  automationApi,
  type ChannelWithSecrets,
  type IssueArea,
  type SyncTask,
  type TaskIssue,
  type Trigger,
  type TriggerKind,
  type UpcomingRun,
} from "../lib/automationApi";
import {
  RUN_TRIGGER_LABELS,
  TRIGGER_KIND_LABELS,
  WEEKDAYS,
  defaultTrigger,
  describeTrigger,
  formatWhen,
  relativeTime,
} from "../lib/automationFormat";
import { COMPARE_PRESETS, type ComparePreset } from "../lib/folderSettings";
import type { ConflictPolicy, SyncMode } from "../lib/syncApi";
import "./Automation.css";
import { CONFLICT_POLICIES, MODES, modeLabel } from "./FolderSync";
import { SyncMatrixEditor } from "./SyncMatrixEditor"; // bc5:sync-matrix
import { matrixFor } from "../lib/syncMatrix"; // bc5:sync-matrix
import { Modal } from "./Modal";
import { ChannelIcon, channelSummary } from "./NotificationChannelsDialog";
import { PathField } from "./PathField";
import { ExecOptionsForm } from "./SyncOptionsDialog";

interface Props {
  /** A saved task, or a new one from `automationApi.newTask` (id ""). */
  initial: SyncTask;
  channels: ChannelWithSecrets[];
  onSaved: (task: SyncTask, warnings: TaskIssue[]) => void;
  onClose: () => void;
  onManageChannels: () => void;
}

const TABS: { id: IssueArea; label: string; icon: LucideIcon }[] = [
  { id: "general", label: "General", icon: FolderSyncIcon },
  { id: "triggers", label: "Triggers", icon: CalendarClock },
  { id: "options", label: "Options", icon: SlidersHorizontal },
  { id: "safety", label: "Safety", icon: ShieldCheck },
  { id: "notify", label: "Notifications", icon: Bell },
];

export const TRIGGER_ICONS: Record<TriggerKind, LucideIcon> = {
  interval: Timer,
  daily: CalendarDays,
  cron: Clock,
  realtime: Eye,
  onStartup: Power,
};

/** Plain-English consequences of each mode, beyond FolderSync's one-line hints. */
const MODE_DETAILS: Partial<Record<SyncMode, string>> = {
  mirrorLeftToRight: "Anything on the right that isn't on the left is deleted — including files someone saves there.",
  mirrorRightToLeft: "Anything on the left that isn't on the right is deleted — including files someone saves there.",
  twoWay:
    "Deleting a file on either side deletes it on the other at the next run. The first run only copies (it has no history yet).",
};

const INTERVAL_PRESETS: [number, string][] = [
  [5, "5 min"],
  [15, "15 min"],
  [30, "30 min"],
  [60, "1 h"],
  [240, "4 h"],
  [1440, "1 day"],
];

const REPEAT_PRESETS: [number, string][] = [
  [0, "every time it happens"],
  [60, "at most once an hour"],
  [360, "at most every 6 hours"],
  [720, "at most every 12 hours"],
  [1440, "at most once a day"],
];

const globs = (text: string) =>
  text
    .split(",")
    .map((p) => p.trim())
    .filter(Boolean);

const int = (v: string, min: number, max: number) => Math.min(max, Math.max(min, Math.round(Number(v) || 0)));

/** The compare preset whose quick tests and content method the options match, or "custom". */
function presetOf(c: SyncTask["compare"]): ComparePreset | "custom" {
  for (const [key, p] of Object.entries(COMPARE_PRESETS))
    if (p.compareSize === c.compareSize && p.compareTimestamp === c.compareTimestamp && p.content === c.content)
      return key as ComparePreset;
  return "custom";
}

/** Create or edit an automated sync task. */
export function AutomationTaskDialog({ initial, channels, onSaved, onClose, onManageChannels }: Props) {
  const [task, setTask] = useState<SyncTask>(initial);
  const [tab, setTab] = useState<IssueArea>("general");
  const [issues, setIssues] = useState<TaskIssue[]>([]);
  const [validateError, setValidateError] = useState<string | null>(null);
  const [saveError, setSaveError] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  // Comma lists edited as typed text; parsed into the options on every change.
  const [excludeText, setExcludeText] = useState(initial.compare.exclude.join(", "));
  const [includeFilesText, setIncludeFilesText] = useState(initial.compare.filters.includeFiles.join(", "));
  const [excludeFoldersText, setExcludeFoldersText] = useState(initial.compare.filters.excludeFolders.join(", "));

  const dirty = useMemo(() => JSON.stringify(task) !== JSON.stringify(initial), [task, initial]);
  const set = (patch: Partial<SyncTask>) => setTask((t) => ({ ...t, ...patch }));

  // Live validation, debounced; answers to superseded requests are dropped.
  useEffect(() => {
    let live = true;
    const timer = window.setTimeout(() => {
      automationApi.validate(task).then(
        (r) => {
          if (!live) return;
          setIssues(r);
          setValidateError(null);
        },
        (e) => live && setValidateError(errorMessage(e)),
      );
    }, 300);
    return () => {
      live = false;
      window.clearTimeout(timer);
    };
  }, [task]);

  const errors = issues.filter((i) => i.error);
  const byArea = (area: IssueArea) => issues.filter((i) => i.area === area);

  const close = () => {
    if (saving) return;
    if (dirty && !window.confirm("Discard your changes to this task?")) return;
    onClose();
  };

  const save = async () => {
    setSaving(true);
    setSaveError(null);
    try {
      const [saved, warnings] = await automationApi.save(task);
      onSaved(saved, warnings);
    } catch (e) {
      setSaveError(errorMessage(e));
    } finally {
      setSaving(false);
    }
  };

  const issueList = (area: IssueArea) => {
    const list = byArea(area);
    if (!list.length) return null;
    return (
      <ul className="auto-issues">
        {list.map((i, n) => (
          <li key={n} className={i.error ? "err" : "warn"}>
            {i.error ? <CircleX size={14} /> : <TriangleAlert size={14} />}
            <span>{i.message}</span>
          </li>
        ))}
      </ul>
    );
  };

  return (
    <Modal
      title={task.id ? `Edit task — ${initial.name}` : "New automated sync"}
      className="auto-editor"
      onClose={close}
      footer={
        <>
          <span className="sync-foot-left muted auto-foot-note">
            {validateError
              ? `Couldn't check the settings: ${validateError}`
              : errors.length
                ? `${errors.length} problem${errors.length === 1 ? "" : "s"} to fix before saving`
                : dirty
                  ? "Unsaved changes"
                  : ""}
          </span>
          <button onClick={close} disabled={saving}>
            Cancel
          </button>
          <button className="primary" onClick={save} disabled={saving || errors.length > 0}>
            {saving ? "Saving…" : task.id ? "Save" : "Create task"}
          </button>
        </>
      }
    >
      <div className="app-settings auto-editor-body">
        <nav className="app-settings-nav">
          {TABS.map(({ id, label, icon: Icon }) => {
            const list = byArea(id);
            const errs = list.filter((i) => i.error).length;
            const warns = list.length - errs;
            return (
              <button key={id} className={tab === id ? "active" : undefined} onClick={() => setTab(id)}>
                <Icon size={14} /> {label}
                <span className="spacer" />
                {errs > 0 && <span className="auto-count err">{errs}</span>}
                {warns > 0 && <span className="auto-count warn">{warns}</span>}
              </button>
            );
          })}
        </nav>
        <div className="app-settings-page auto-editor-page">
          {saveError && <div className="banner error auto-banner">{saveError}</div>}
          {issueList(tab)}
          {tab === "general" && <GeneralTab task={task} set={set} />}
          {tab === "triggers" && <TriggersTab task={task} set={set} />}
          {tab === "options" && (
            <>
              <div className="settings-grid single auto-compare">
                <fieldset>
                  <legend>Compare</legend>
                  <div className="field-row">
                    <label className="field">
                      <span>Files are different when</span>
                      <select
                        value={presetOf(task.compare)}
                        onChange={(e) => {
                          const p = COMPARE_PRESETS[e.target.value as ComparePreset];
                          if (p)
                            set({
                              compare: {
                                ...task.compare,
                                compareSize: p.compareSize,
                                compareTimestamp: p.compareTimestamp,
                                content: p.content,
                              },
                            });
                        }}
                      >
                        {Object.entries(COMPARE_PRESETS).map(([key, p]) => (
                          <option key={key} value={key}>
                            {p.label}
                          </option>
                        ))}
                        {presetOf(task.compare) === "custom" && <option value="custom">Custom (from Folder Compare)</option>}
                      </select>
                    </label>
                    <label className="field auto-narrow">
                      <span>Time tolerance (s)</span>
                      <input
                        type="number"
                        min={0}
                        max={86400}
                        value={task.compare.timeToleranceSecs}
                        onChange={(e) => set({ compare: { ...task.compare, timeToleranceSecs: int(e.target.value, 0, 86400) } })}
                      />
                    </label>
                  </div>
                  <label className="field">
                    <span>Exclude</span>
                    <input
                      value={excludeText}
                      placeholder="*.tmp, ~$*, Thumbs.db, node_modules"
                      spellCheck={false}
                      onChange={(e) => {
                        setExcludeText(e.target.value);
                        set({ compare: { ...task.compare, exclude: globs(e.target.value) } });
                      }}
                    />
                    <small className="muted">
                      Comma-separated names or wildcards; matching files and folders are neither synced nor watched.
                    </small>
                  </label>
                  <div className="field-row">
                    <label className="field">
                      <span>Only files matching</span>
                      <input
                        value={includeFilesText}
                        placeholder="All files"
                        spellCheck={false}
                        onChange={(e) => {
                          setIncludeFilesText(e.target.value);
                          set({
                            compare: { ...task.compare, filters: { ...task.compare.filters, includeFiles: globs(e.target.value) } },
                          });
                        }}
                      />
                    </label>
                    <label className="field">
                      <span>Skip folders</span>
                      <input
                        value={excludeFoldersText}
                        placeholder="e.g. cache, logs"
                        spellCheck={false}
                        onChange={(e) => {
                          setExcludeFoldersText(e.target.value);
                          set({
                            compare: {
                              ...task.compare,
                              filters: { ...task.compare.filters, excludeFolders: globs(e.target.value) },
                            },
                          });
                        }}
                      />
                    </label>
                  </div>
                  <label className="field check">
                    <input
                      type="checkbox"
                      checked={task.compare.includeHidden}
                      onChange={(e) => set({ compare: { ...task.compare, includeHidden: e.target.checked } })}
                    />
                    Include hidden and system files
                  </label>
                </fieldset>
              </div>
              <ExecOptionsForm value={task.exec} onChange={(exec) => set({ exec })} />
            </>
          )}
          {tab === "safety" && <SafetyTab task={task} set={set} />}
          {tab === "notify" && (
            <NotifyTab task={task} set={set} channels={channels} onManageChannels={onManageChannels} />
          )}
        </div>
      </div>
    </Modal>
  );
}

type TabProps = { task: SyncTask; set: (patch: Partial<SyncTask>) => void };

function GeneralTab({ task, set }: TabProps) {
  const mode = MODES.find((m) => m.value === task.mode) ?? MODES[0];
  const showConflicts = task.mode === "twoWay" || task.mode === "updateBoth" || task.mode === "custom";
  // bc5:sync-matrix — Custom starts from the mode in use (saving resets a fixed mode's matrix to its preset).
  const pickMode = (next: SyncMode) =>
    set(next === "custom" ? { mode: next, matrix: matrixFor(task.mode, task.matrix) } : { mode: next });
  return (
    <>
      <div className="field-row">
        <label className="field auto-grow2">
          <span>
            Name<em className="req"> *</em>
          </span>
          <input
            value={task.name}
            onChange={(e) => set({ name: e.target.value })}
            placeholder="e.g. Projects → NAS backup"
            autoFocus={!task.id}
          />
        </label>
        <label className="field check auto-enabled" title="Disabled tasks only run when you click Run now">
          <input type="checkbox" checked={task.enabled} onChange={(e) => set({ enabled: e.target.checked })} />
          Enabled
        </label>
      </div>
      <label className="field">
        <span>Description</span>
        <input
          value={task.description}
          onChange={(e) => set({ description: e.target.value })}
          placeholder="Optional: what it's for, who to ask"
        />
      </label>

      <div className="field">
        <span>
          Folders<em className="req"> *</em>
        </span>
        <div className="auto-folders">
          <PathField
            label="Left"
            value={task.left}
            onChange={(left) => set({ left })}
            onPick={(left) => set({ left })}
            directory
            placeholder="Left folder (local, network share or remote profile)"
          />
          <button
            className="icon-btn auto-swap"
            title="Swap left and right"
            onClick={() => set({ left: task.right, right: task.left })}
          >
            <ArrowUpDown size={15} />
          </button>
          <PathField
            label="Right"
            value={task.right}
            onChange={(right) => set({ right })}
            onPick={(right) => set({ right })}
            directory
            placeholder="Right folder"
          />
        </div>
        {(isRemote(task.left) || isRemote(task.right)) && (
          <small className="muted">
            Remote folders can't be watched for changes: real-time triggers fall back to their periodic rescan.
          </small>
        )}
      </div>

      <div className="field">
        <span>Mode</span>
        <div className="auto-modes">
          {MODES.map((m) => {
            const Icon = m.icon;
            return (
              <label key={m.value} className={`auto-mode${task.mode === m.value ? " active" : ""}${m.deletes ? " deletes" : ""}`}>
                <input type="radio" name="auto-mode" checked={task.mode === m.value} onChange={() => pickMode(m.value)} />
                <span className={`sync-mode-icon${m.deletes ? " mirror" : ""}`}>
                  <Icon size={14} />
                </span>
                <span className="auto-mode-text">
                  <b>{m.label}</b>
                  <small className="muted">{m.hint}</small>
                </span>
              </label>
            );
          })}
        </div>
        {MODE_DETAILS[mode.value] && (
          <p className="sync-confirm-warn auto-inline-warn">
            <TriangleAlert size={13} /> {MODE_DETAILS[mode.value]}
          </p>
        )}
        {/* bc5:sync-matrix */}
        {task.mode === "custom" && (
          <SyncMatrixEditor
            value={matrixFor("custom", task.matrix)}
            onChange={(matrix) => set({ matrix })}
            presetLabel={modeLabel}
          />
        )}
        <label className="field check" title="Create folders that hold no files (or only filtered-out ones) on the other side">
          <input
            type="checkbox"
            checked={!task.skipEmptyFolders}
            onChange={(e) => set({ skipEmptyFolders: !e.target.checked })}
          />
          Create empty folders
        </label>
      </div>

      {showConflicts && (
        <label className="field auto-conflicts">
          <span>When a file changed on both sides</span>
          <select value={task.conflicts} onChange={(e) => set({ conflicts: e.target.value as ConflictPolicy })}>
            {CONFLICT_POLICIES.map((c) => (
              <option key={c.value} value={c.value}>
                {c.label}
              </option>
            ))}
          </select>
          <small className="muted">
            {task.conflicts === "skip"
              ? "Both versions are left alone and the run reports a conflict (a warning notification)."
              : "One version overwrites the other; the replaced file goes where the Options tab says."}
          </small>
        </label>
      )}
    </>
  );
}

function TriggersTab({ task, set }: TabProps) {
  const [upcoming, setUpcoming] = useState<UpcomingRun[] | null>(null);
  const [upcomingError, setUpcomingError] = useState<string | null>(null);
  const triggers = task.triggers;
  const setTrigger = (i: number, t: Trigger) => set({ triggers: triggers.map((x, j) => (j === i ? t : x)) });
  const scheduled = triggers.some((t) => t.kind === "interval" || t.kind === "daily" || t.kind === "cron" || (t.kind === "realtime" && t.rescanMinutes > 0));

  // The next few runs, recomputed (debounced) as the triggers change.
  useEffect(() => {
    let live = true;
    const timer = window.setTimeout(() => {
      automationApi.upcoming(triggers, 5).then(
        (r) => {
          if (!live) return;
          setUpcoming(r);
          setUpcomingError(null);
        },
        (e) => {
          if (!live) return;
          setUpcoming(null);
          setUpcomingError(errorMessage(e));
        },
      );
    }, 400);
    return () => {
      live = false;
      window.clearTimeout(timer);
    };
  }, [triggers]);

  const now = Date.now();
  return (
    <>
      {triggers.length === 0 && (
        <div className="auto-no-triggers muted">
          No triggers: this task only runs when you click <b>Run now</b>. Add one below.
        </div>
      )}
      {triggers.map((t, i) => {
        const Icon = TRIGGER_ICONS[t.kind];
        return (
          <div key={i} className="auto-trigger">
            <div className="auto-trigger-head">
              <Icon size={15} className="auto-trigger-icon" />
              <b>{TRIGGER_KIND_LABELS[t.kind]}</b>
              <span className="muted">· {describeTrigger(t)}</span>
              <span className="spacer" />
              <button
                className="icon-btn"
                title="Remove this trigger"
                onClick={() => set({ triggers: triggers.filter((_, j) => j !== i) })}
              >
                <X size={14} />
              </button>
            </div>
            <TriggerEditor trigger={t} mode={task.mode} onChange={(n) => setTrigger(i, n)} />
          </div>
        );
      })}

      <div className="auto-add-trigger">
        <span className="muted">Add trigger:</span>
        {(Object.keys(TRIGGER_KIND_LABELS) as TriggerKind[]).map((k) => {
          const Icon = TRIGGER_ICONS[k];
          return (
            <button key={k} onClick={() => set({ triggers: [...triggers, defaultTrigger(k)] })}>
              <Plus size={12} />
              <Icon size={14} /> {TRIGGER_KIND_LABELS[k].replace(" (when files change)", "")}
            </button>
          );
        })}
      </div>

      <label className="field check auto-catchup">
        <input type="checkbox" checked={task.catchUpMissed} onChange={(e) => set({ catchUpMissed: e.target.checked })} />
        <span>
          Catch up missed runs
          <small className="muted">
            {" "}
            — if PCompare was closed or the computer slept through a scheduled time, run once as soon as possible.
          </small>
        </span>
      </label>

      <div className="auto-upcoming">
        <div className="auto-upcoming-title">Next runs</div>
        {upcomingError ? (
          <div className="s-error-fg">{upcomingError}</div>
        ) : !scheduled ? (
          <div className="muted">
            {triggers.length ? "No clock-based triggers: runs happen on file changes, at startup or on demand." : "Manual only."}
          </div>
        ) : upcoming === null ? (
          <div className="muted">Calculating…</div>
        ) : upcoming.length === 0 ? (
          <div className="muted">None — check the triggers above.</div>
        ) : (
          <ol>
            {upcoming.map((u, n) => (
              <li key={n}>
                <span>{formatWhen(u.atMs)}</span>
                <span className="muted">{relativeTime(u.atMs, now)}</span>
                <span className="muted">{RUN_TRIGGER_LABELS[u.trigger]}</span>
              </li>
            ))}
          </ol>
        )}
      </div>
    </>
  );
}

/** Per-kind fields of one trigger. */
function TriggerEditor({ trigger: t, mode, onChange }: { trigger: Trigger; mode: SyncMode; onChange: (t: Trigger) => void }) {
  const [newTime, setNewTime] = useState("12:00");
  switch (t.kind) {
    case "interval":
      return (
        <div className="auto-trigger-body">
          <label className="auto-inline">
            Every
            <input
              type="number"
              min={1}
              max={10080}
              value={t.everyMinutes}
              onChange={(e) => onChange({ ...t, everyMinutes: int(e.target.value, 1, 10080) })}
            />
            minutes
          </label>
          <div className="seg-control auto-presets">
            {INTERVAL_PRESETS.map(([m, label]) => (
              <button key={m} className={t.everyMinutes === m ? "active" : undefined} onClick={() => onChange({ ...t, everyMinutes: m })}>
                {label}
              </button>
            ))}
          </div>
          <small className="muted auto-trigger-help">Counted from the end of the previous run, so runs never pile up.</small>
        </div>
      );
    case "daily": {
      const days = new Set(t.weekdays);
      const everyDay = days.size === 0 || days.size === 7;
      const toggleDay = (d: number) => {
        const next = new Set(everyDay ? [0, 1, 2, 3, 4, 5, 6] : days);
        if (next.has(d)) next.delete(d);
        else next.add(d);
        onChange({ ...t, weekdays: next.size === 7 ? [] : [...next].sort() });
      };
      const addTime = () => {
        if (/^\d{2}:\d{2}$/.test(newTime) && !t.times.includes(newTime)) onChange({ ...t, times: [...t.times, newTime].sort() });
      };
      return (
        <div className="auto-trigger-body">
          <div className="auto-inline">
            At
            {t.times.map((time) => (
              <span key={time} className="auto-time-chip">
                {time}
                <button title="Remove this time" onClick={() => onChange({ ...t, times: t.times.filter((x) => x !== time) })}>
                  <X size={11} />
                </button>
              </span>
            ))}
            <input type="time" value={newTime} onChange={(e) => setNewTime(e.target.value)} onKeyDown={(e) => e.key === "Enter" && addTime()} />
            <button onClick={addTime} disabled={t.times.includes(newTime)}>
              <Plus size={13} /> Add time
            </button>
          </div>
          <div className="auto-inline">
            On
            <div className="seg-control auto-days">
              {WEEKDAYS.map((name, d) => (
                <button key={d} className={everyDay || days.has(d) ? "active" : undefined} onClick={() => toggleDay(d)}>
                  {name}
                </button>
              ))}
            </div>
            <button className="link" onClick={() => onChange({ ...t, weekdays: [0, 1, 2, 3, 4] })}>
              Weekdays
            </button>
            <button className="link" onClick={() => onChange({ ...t, weekdays: [] })}>
              Every day
            </button>
          </div>
        </div>
      );
    }
    case "cron":
      return <CronEditor trigger={t} onChange={onChange} />;
    case "realtime": {
      const sides =
        mode === "updateLeftToRight" || mode === "mirrorLeftToRight"
          ? "the left folder"
          : mode === "updateRightToLeft" || mode === "mirrorRightToLeft"
            ? "the right folder"
            : "both folders";
      return (
        <div className="auto-trigger-body">
          <div className="field-row">
            <label className="field">
              <span>Wait for quiet (s)</span>
              <input
                type="number"
                min={1}
                max={3600}
                value={t.debounceSecs}
                onChange={(e) => onChange({ ...t, debounceSecs: int(e.target.value, 1, 3600) })}
              />
            </label>
            <label className="field">
              <span>But sync at least every (s)</span>
              <input
                type="number"
                min={t.debounceSecs}
                max={86400}
                value={t.maxDelaySecs}
                onChange={(e) => onChange({ ...t, maxDelaySecs: int(e.target.value, 1, 86400) })}
              />
            </label>
            <label className="field">
              <span>Full rescan every (min, 0 = never)</span>
              <input
                type="number"
                min={0}
                max={10080}
                value={t.rescanMinutes}
                onChange={(e) => onChange({ ...t, rescanMinutes: int(e.target.value, 0, 10080) })}
              />
            </label>
          </div>
          <small className="muted auto-trigger-help">
            Watches {sides} and syncs once changes have settled for {t.debounceSecs} s. The periodic rescan catches
            changes the system didn't report; remote folders and network shares that can't be watched rely on it alone.
          </small>
        </div>
      );
    }
    case "onStartup":
      return (
        <div className="auto-trigger-body">
          <label className="auto-inline">
            Run
            <input
              type="number"
              min={0}
              max={3600}
              value={t.delaySecs}
              onChange={(e) => onChange({ ...t, delaySecs: int(e.target.value, 0, 3600) })}
            />
            seconds after PCompare starts (including at login, when that's enabled in Automation settings).
          </label>
        </div>
      );
  }
}

/** A cron expression with syntax help and its next fire time (or the parse error). */
function CronEditor({ trigger: t, onChange }: { trigger: Extract<Trigger, { kind: "cron" }>; onChange: (t: Trigger) => void }) {
  const [result, setResult] = useState<{ ok: boolean; text: string } | null>(null);
  const seq = useRef(0);
  useEffect(() => {
    const n = ++seq.current;
    const timer = window.setTimeout(() => {
      automationApi.upcoming([t], 1).then(
        (r) =>
          n === seq.current &&
          setResult(r.length ? { ok: true, text: `Next: ${formatWhen(r[0].atMs)}` } : { ok: false, text: "Never fires." }),
        (e) => n === seq.current && setResult({ ok: false, text: errorMessage(e) }),
      );
    }, 300);
    return () => window.clearTimeout(timer);
  }, [t]);
  return (
    <div className="auto-trigger-body">
      <div className="auto-inline">
        <input
          className="auto-cron"
          value={t.expr}
          onChange={(e) => onChange({ ...t, expr: e.target.value })}
          placeholder="0 */2 * * *"
          spellCheck={false}
        />
        {result && <span className={result.ok ? "auto-ok" : "s-error-fg"}>{result.text}</span>}
      </div>
      <small className="muted auto-trigger-help">
        Five fields — <code>minute hour day-of-month month day-of-week</code> — or six with seconds first. Examples:{" "}
        <code>0 */2 * * *</code> every 2 hours, <code>30 8 * * 1-5</code> weekdays at 08:30, <code>0 3 1 * *</code> 03:00
        on the 1st. <code>@hourly</code>, <code>@daily</code>, <code>@weekly</code> and <code>@monthly</code> work too. Local
        time.
      </small>
    </div>
  );
}

/** A number input with a "no limit" checkbox in front (null = no limit). */
function Limit({
  label,
  value,
  fallback,
  min,
  max,
  unit,
  onChange,
}: {
  label: string;
  value: number | null;
  fallback: number;
  min: number;
  max: number;
  unit: string;
  onChange: (v: number | null) => void;
}) {
  return (
    <div className="field check auto-limit">
      <input type="checkbox" checked={value != null} onChange={(e) => onChange(e.target.checked ? fallback : null)} />
      <span>{label}</span>
      <input
        type="number"
        min={min}
        max={max}
        disabled={value == null}
        value={value ?? ""}
        placeholder="no limit"
        onChange={(e) => onChange(int(e.target.value, min, max))}
      />
      <span className="muted">{unit}</span>
    </div>
  );
}

function SafetyTab({ task, set }: TabProps) {
  const s = task.safety;
  const setSafety = (patch: Partial<SyncTask["safety"]>) => set({ safety: { ...s, ...patch } });
  return (
    <div className="settings-grid single">
      <fieldset>
        <legend>Stop a run before it does damage</legend>
        <label className="field check">
          <input type="checkbox" checked={s.abortOnEmptySide} onChange={(e) => setSafety({ abortOnEmptySide: e.target.checked })} />
          <span>
            Abort when one folder is empty and the other isn't
            <small className="muted auto-help">Almost always an unplugged drive or an unmounted share, not a real change.</small>
          </span>
        </label>
        <label className="field check">
          <input
            type="checkbox"
            checked={s.abortOnMassDeletion}
            onChange={(e) => setSafety({ abortOnMassDeletion: e.target.checked })}
          />
          <span>
            Abort when the plan deletes most of a folder
            <small className="muted auto-help">The same check Folder Sync warns about before a manual run.</small>
          </span>
        </label>
        <Limit
          label="Abort if it would delete more than"
          value={s.maxDeletions}
          fallback={100}
          min={0}
          max={1_000_000}
          unit="items"
          onChange={(maxDeletions) => setSafety({ maxDeletions })}
        />
        <Limit
          label="Abort if it would delete more than"
          value={s.maxDeletionPercent}
          fallback={50}
          min={1}
          max={100}
          unit="% of the files on that side"
          onChange={(maxDeletionPercent) => setSafety({ maxDeletionPercent })}
        />
        <Limit
          label="Cancel a run that takes longer than"
          value={s.timeoutMinutes}
          fallback={240}
          min={1}
          max={10080}
          unit="minutes"
          onChange={(timeoutMinutes) => setSafety({ timeoutMinutes })}
        />
        <small className="muted">Aborted runs change nothing and are reported as failures.</small>
      </fieldset>
      <fieldset>
        <legend>Retry the whole run</legend>
        <div className="field-row">
          <label className="field">
            <span>Extra attempts</span>
            <input
              type="number"
              min={0}
              max={10}
              value={task.retry.attempts}
              onChange={(e) => set({ retry: { ...task.retry, attempts: int(e.target.value, 0, 10) } })}
            />
          </label>
          <label className="field">
            <span>First wait (seconds)</span>
            <input
              type="number"
              min={1}
              max={3600}
              disabled={task.retry.attempts === 0}
              value={task.retry.delaySecs}
              onChange={(e) => set({ retry: { ...task.retry, delaySecs: int(e.target.value, 1, 3600) } })}
            />
          </label>
        </div>
        <small className="muted">
          For run-wide failures such as an unreachable network share or a sleeping NAS. The wait doubles each time (up
          to an hour); only the last attempt sends a notification. Files in use are retried separately, per file (Options
          tab).
        </small>
      </fieldset>
    </div>
  );
}

function NotifyTab({ task, set, channels, onManageChannels }: TabProps & { channels: ChannelWithSecrets[]; onManageChannels: () => void }) {
  const n = task.notify;
  const setNotify = (patch: Partial<SyncTask["notify"]>) => set({ notify: { ...n, ...patch } });
  const toggleChannel = (id: string, on: boolean) =>
    setNotify({ channels: on ? [...n.channels, id] : n.channels.filter((c) => c !== id) });
  const custom = !REPEAT_PRESETS.some(([m]) => m === n.repeatAfterMinutes);
  // Channels deleted since the task was saved still show, so they can be unticked.
  const missing = n.channels.filter((id) => !channels.some((c) => c.channel.id === id));

  return (
    <div className="settings-grid single">
      <fieldset>
        <legend>Send to</legend>
        {channels.length === 0 && <p className="muted auto-help-p">No notification channels yet.</p>}
        {channels.map((c) => (
          <label key={c.channel.id} className="field check auto-channel-check">
            <input
              type="checkbox"
              checked={n.channels.includes(c.channel.id)}
              onChange={(e) => toggleChannel(c.channel.id, e.target.checked)}
            />
            <ChannelIcon kind={c.channel.settings.kind} />
            <span>
              {c.channel.name}
              {!c.channel.enabled && <span className="badge">Off</span>}
            </span>
            <small className="muted">{channelSummary(c)}</small>
          </label>
        ))}
        {missing.map((id) => (
          <label key={id} className="field check auto-channel-check">
            <input type="checkbox" checked onChange={() => toggleChannel(id, false)} />
            <span className="s-error-fg">Deleted channel</span>
          </label>
        ))}
        <label className="field check auto-channel-check">
          <input type="checkbox" checked={n.desktop} onChange={(e) => setNotify({ desktop: e.target.checked })} />
          <Bell size={15} />
          <span>Desktop notification on this computer</span>
        </label>
        <div>
          <button onClick={onManageChannels}>Manage channels…</button>
        </div>
      </fieldset>
      <fieldset>
        <legend>When</legend>
        <label className="field check">
          <input type="checkbox" checked={n.onFailure} onChange={(e) => setNotify({ onFailure: e.target.checked })} />
          <span>
            A run fails or is aborted <small className="muted auto-help">after its last retry</small>
          </span>
        </label>
        <label className="field check">
          <input type="checkbox" checked={n.onWarning} onChange={(e) => setNotify({ onWarning: e.target.checked })} />
          <span>
            A run finishes with errors or conflicts <small className="muted auto-help">some files couldn't be synced</small>
          </span>
        </label>
        <label className="field check">
          <input type="checkbox" checked={n.onRecovery} onChange={(e) => setNotify({ onRecovery: e.target.checked })} />
          <span>
            It works again <small className="muted auto-help">the first success after a failure</small>
          </span>
        </label>
        <label className="field check">
          <input type="checkbox" checked={n.onSuccess} onChange={(e) => setNotify({ onSuccess: e.target.checked })} />
          <span>
            Every successful run that changed something <small className="muted auto-help">can be noisy</small>
          </span>
        </label>
        <div className="auto-inline auto-repeat">
          Send the same failure again
          <select
            value={custom ? "custom" : String(n.repeatAfterMinutes)}
            onChange={(e) =>
              setNotify({ repeatAfterMinutes: e.target.value === "custom" ? n.repeatAfterMinutes || 30 : Number(e.target.value) })
            }
          >
            {REPEAT_PRESETS.map(([m, label]) => (
              <option key={m} value={m}>
                {label}
              </option>
            ))}
            <option value="custom">at most every … minutes</option>
          </select>
          {custom && (
            <>
              <input
                type="number"
                min={1}
                max={10080}
                value={n.repeatAfterMinutes}
                onChange={(e) => setNotify({ repeatAfterMinutes: int(e.target.value, 1, 10080) })}
              />
              minutes
            </>
          )}
        </div>
        <small className="muted">A failing task that keeps retrying on its schedule won't flood the channel.</small>
      </fieldset>
    </div>
  );
}
