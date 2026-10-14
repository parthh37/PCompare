// Pure display helpers for Sync Automation: trigger descriptions, run status
// labels, relative times. Kept local (not IPC) so a list of tasks renders
// without one round trip per trigger; the wording mirrors `describe()` in
// crates/pcompare-automation/src/trigger.rs.
import type { AutomationRun, RunStatus, RunTrigger, Trigger, TriggerKind } from "./automationApi";

/** Monday-first, matching the engine's weekday numbers (0 = Monday … 6 = Sunday). */
export const WEEKDAYS = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];

const plural = (n: number, noun: string) => `${n} ${noun}${n === 1 ? "" : "s"}`;

/** "every 5 minutes", "hourly", "every 2 hours", "daily". */
function every(minutes: number): string {
  if (minutes === 60) return "hourly";
  if (minutes === 1440) return "daily";
  if (minutes % 1440 === 0) return `every ${minutes / 1440} days`;
  if (minutes % 60 === 0) return `every ${minutes / 60} hours`;
  return `every ${plural(minutes, "minute")}`;
}

/** "Weekdays", "Weekends", "Mon, Wed, Fri" — or "" for every day. */
export function describeWeekdays(days: number[]): string {
  const set = [...new Set(days)].filter((d) => d >= 0 && d <= 6).sort();
  if (set.length === 0 || set.length === 7) return "";
  if (set.join() === "0,1,2,3,4") return "Weekdays";
  if (set.join() === "5,6") return "Weekends";
  return set.map((d) => WEEKDAYS[d]).join(", ");
}

/** A short description of a trigger, e.g. "Every 15 minutes", "Weekdays at 09:00, 17:30". */
export function describeTrigger(t: Trigger): string {
  switch (t.kind) {
    case "interval": {
      const e = every(Math.max(1, t.everyMinutes));
      return e.charAt(0).toUpperCase() + e.slice(1);
    }
    case "daily": {
      const times = [...t.times].sort().join(", ") || "—";
      const days = describeWeekdays(t.weekdays);
      return days ? `${days} at ${times}` : `Daily at ${times}`;
    }
    case "cron":
      return `Cron ${t.expr.trim() || "—"}`;
    case "realtime":
      return `Real-time (${t.debounceSecs} s settle, ${t.rescanMinutes ? `rescan ${every(t.rescanMinutes)}` : "no rescan"})`;
    case "onStartup":
      return t.delaySecs ? `At startup (after ${t.delaySecs} s)` : "At startup";
  }
}

export const TRIGGER_KIND_LABELS: Record<TriggerKind, string> = {
  interval: "Interval",
  daily: "Daily / weekly",
  cron: "Cron expression",
  realtime: "Real-time (when files change)",
  onStartup: "At startup",
};

/** A new trigger of `kind` with sensible defaults. */
export function defaultTrigger(kind: TriggerKind): Trigger {
  switch (kind) {
    case "interval":
      return { kind, everyMinutes: 60 };
    case "daily":
      return { kind, times: ["09:00"], weekdays: [] };
    case "cron":
      return { kind, expr: "0 */2 * * *" };
    case "realtime":
      return { kind, debounceSecs: 5, maxDelaySecs: 300, rescanMinutes: 60 };
    case "onStartup":
      return { kind, delaySecs: 60 };
  }
}

/** What started a run, as shown in history. */
export const RUN_TRIGGER_LABELS: Record<RunTrigger, string> = {
  manual: "Run now",
  interval: "Interval",
  daily: "Schedule",
  cron: "Cron",
  realtime: "File change",
  rescan: "Rescan",
  startup: "Startup",
  catchUp: "Catch-up",
  retry: "Retry",
};

export type Tone = "ok" | "warn" | "bad" | "muted" | "accent";

export const RUN_STATUS_INFO: Record<RunStatus, { label: string; tone: Tone }> = {
  running: { label: "Running", tone: "accent" },
  succeeded: { label: "Succeeded", tone: "ok" },
  noChanges: { label: "No changes", tone: "muted" },
  completedWithErrors: { label: "Completed with errors", tone: "warn" },
  failed: { label: "Failed", tone: "bad" },
  aborted: { label: "Aborted", tone: "bad" },
  cancelled: { label: "Cancelled", tone: "muted" },
  interrupted: { label: "Interrupted", tone: "bad" },
};

/** The status label for a run, with the error count where it says more ("3 errors"). */
export function runStatusLabel(status: RunStatus, failed?: number): string {
  if (status === "completedWithErrors" && failed) return plural(failed, "error");
  return RUN_STATUS_INFO[status].label;
}

/** "just now", "5 min ago", "in 12 min", "in 2 h 5 min", "3 days ago". */
export function relativeTime(ms: number, now: number): string {
  const diff = ms - now;
  const abs = Math.abs(diff);
  const future = diff > 0;
  if (abs < 45_000) return future ? "in a moment" : "just now";
  const min = Math.round(abs / 60_000);
  let text: string;
  if (min < 60) text = `${min} min`;
  else if (min < 180) text = min % 60 ? `${Math.floor(min / 60)} h ${min % 60} min` : `${min / 60} h`;
  else if (min < 36 * 60) text = `${Math.round(min / 60)} h`;
  else text = plural(Math.round(min / 1440), "day");
  return future ? `in ${text}` : `${text} ago`;
}

/** "0.8 s", "42 s", "3 min 5 s", "1 h 12 min". */
export function formatDuration(ms: number): string {
  if (ms < 10_000) return `${(ms / 1000).toFixed(1)} s`;
  const s = Math.round(ms / 1000);
  if (s < 60) return `${s} s`;
  if (s < 3600) return `${Math.floor(s / 60)} min${s % 60 ? ` ${s % 60} s` : ""}`;
  const m = Math.round(s / 60);
  return `${Math.floor(m / 60)} h${m % 60 ? ` ${m % 60} min` : ""}`;
}

export const runDuration = (r: AutomationRun) => (r.finishedMs == null ? null : r.finishedMs - r.startedMs);

const dayFmt = new Intl.DateTimeFormat(undefined, { weekday: "short", day: "numeric", month: "short" });
const timeFmt = new Intl.DateTimeFormat(undefined, { hour: "2-digit", minute: "2-digit" });

/** "Tue 14 Oct, 09:00" (local), for the next-runs preview and history. */
export function formatWhen(ms: number): string {
  const d = new Date(ms);
  return `${dayFmt.format(d)}, ${timeFmt.format(d)}`;
}

/** Comma-, semicolon- or line-separated list → trimmed non-empty items. */
export const splitList = (text: string) =>
  text
    .split(/[,;\n]/)
    .map((p) => p.trim())
    .filter(Boolean);
