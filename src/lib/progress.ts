import { useRef } from "react";

/** "5,121 of 29,188 (17%)" — what a determinate progress bar reads out beside it. */
export function progressCount(done: number, total: number): string {
  const pct = total > 0 ? Math.floor((Math.min(done, total) / total) * 100) : 0;
  return `${done.toLocaleString()} of ${total.toLocaleString()} (${pct}%)`;
}

/**
 * Time left for work going at a steady rate, as a short phrase ("about
 * 3 min left"), or null while it's too early to tell or not worth saying.
 * `elapsedMs` counts from when the work started; `done` and `total` are in
 * the same unit (files, bytes, rows).
 */
export function estimateRemaining(elapsedMs: number, done: number, total: number): string | null {
  // TODO(you): when an estimate is trustworthy, and how to word it. Early
  // estimates swing wildly (the first files are often the cached or small
  // ones), and a countdown that ticks every second is busier than one that
  // rounds. This default waits 3 s and 2% before saying anything.
  if (elapsedMs < 3000 || done <= 0 || total <= 0 || done / total < 0.02) return null;
  const leftMs = (elapsedMs / done) * (total - done);
  if (leftMs < 5000) return "a few seconds left";
  if (leftMs < 60_000) return `about ${Math.ceil(leftMs / 5000) * 5} s left`;
  if (leftMs < 3_600_000) return `about ${Math.round(leftMs / 60_000)} min left`;
  return `about ${(leftMs / 3_600_000).toFixed(1)} h left`;
}

/**
 * When the run the caller shows started (`performance.now()`), reset each
 * time `runId` changes; null while nothing runs. For {@link estimateRemaining}.
 */
export function useRunStart(runId: unknown): number | null {
  // Set during render on purpose: idempotent per run, so a repeated render keeps it.
  const start = useRef<{ id: unknown; at: number } | null>(null);
  if (runId == null || runId === false) start.current = null;
  else if (!start.current || start.current.id !== runId) start.current = { id: runId, at: performance.now() };
  return start.current?.at ?? null;
}
