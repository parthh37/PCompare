// The docked Operations panel of Folder Compare: every
// background file operation with its progress, a Pause / Resume and Cancel
// per operation, name-clash questions, and errors once it is done.
import {
  ArchiveRestore,
  ChevronDown,
  ChevronRight,
  CircleAlert,
  CircleCheck,
  CircleX,
  Hourglass,
  Lock,
  Pause,
  Play,
  X,
} from "lucide-react";
import { useState } from "react";
import { errorMessage } from "../lib/api";
import { fileOps, type ConflictAnswer, type FileOp, type FileOpProgress } from "../lib/fileOpsApi";
import { formatSize, formatTime } from "../lib/format";
import { estimateRemaining, useRunStart } from "../lib/progress";
import { ProgressBar } from "./ProgressBar";
import { useShell } from "../lib/shell";
import "./FileOpsPanel.css";

interface Props {
  ops: FileOp[];
  onClose: () => void;
}

const PHASE_LABEL: Record<FileOpProgress["phase"], string> = {
  queued: "Waiting for an earlier operation on the same files…",
  scanning: "Scanning…",
  running: "",
  paused: "Paused",
  asking: "Waiting for your answer",
  deferred: "Trying the files that were in use again…",
  finished: "Finishing…",
};

/** `started`: when it began copying (`performance.now()`), for the time left. */
function statusLine(op: FileOp, started: number | null): string {
  const p = op.progress;
  const r = op.report;
  if (op.status === "failed") return op.error ?? "Failed";
  if (r) {
    const parts = [`${r.succeeded.toLocaleString()} done`];
    if (r.skipped) parts.push(`${r.skipped.toLocaleString()} skipped`);
    if (r.filtered) parts.push(`${r.filtered.toLocaleString()} filtered out`);
    if (r.failed) parts.push(`${r.failed.toLocaleString()} failed`);
    if (r.backedUp) parts.push(`${r.backedUp.toLocaleString()} replaced file${r.backedUp === 1 ? "" : "s"} kept in Backups`);
    if (r.bytes) parts.push(formatSize(r.bytes));
    parts.push(`${(r.elapsedMs / 1000).toFixed(1)} s`);
    return `${r.cancelled ? "Cancelled · " : ""}${parts.join(" · ")}`;
  }
  if (!p) return "Starting…";
  if (op.paused && p.phase !== "paused") return "Pausing…";
  const [done, total] = amounts(p);
  const left =
    p.phase === "running" && started != null && done != null && total ? estimateRemaining(performance.now() - started, done, total) : null;
  const phase = PHASE_LABEL[p.phase];
  const counts = p.filesTotal ? `${p.filesDone.toLocaleString()} / ${p.filesTotal.toLocaleString()}` : "";
  const bytes = p.bytesTotal ? `${formatSize(p.bytesDone)} of ${formatSize(p.bytesTotal)}` : "";
  const waiting = p.retrying ? `${p.retrying.toLocaleString()} in use, retrying` : "";
  const later = p.deferred && p.phase !== "deferred" ? `${p.deferred.toLocaleString()} left for the end` : "";
  return [phase, counts, bytes, waiting, later, left].filter(Boolean).join(" · ");
}

/** Done and total for the bar: bytes when known, else files; nulls while scanning. */
function amounts(p: FileOpProgress | null): [number | null, number | null] {
  if (!p || p.phase === "scanning") return [null, null];
  if (p.bytesTotal > 0) return [p.bytesDone, p.bytesTotal];
  return p.filesTotal > 0 ? [p.filesDone, p.filesTotal] : [null, null];
}

const ANSWERS: { answer: ConflictAnswer; label: string; title?: string }[] = [
  { answer: "overwrite", label: "Overwrite" },
  { answer: "skip", label: "Skip" },
  { answer: "overwriteAll", label: "Overwrite all" },
  { answer: "skipAll", label: "Skip all" },
  { answer: "overwriteOlderAll", label: "Overwrite older", title: "Overwrite this and later clashes only where the target is older" },
  { answer: "cancel", label: "Cancel operation" },
];

function OpRow({ op }: { op: FileOp }) {
  const [open, setOpen] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const { openBackups } = useShell();
  const p = op.progress;
  const running = op.status === "running";
  // Timed from the first file, not from when it was queued or scanning.
  const started = useRunStart(running && p && p.phase !== "queued" && p.phase !== "scanning" ? op.id : null);
  /** The backup holding the files it replaced (Backups on). */
  const backup = op.report?.backedUp ? op.report.backup : null;
  const q = running ? p?.question : null;
  const issues = (op.report?.failures.length ?? 0) + (op.report?.notes.length ?? 0);
  const act = (f: () => Promise<unknown>) => f().catch((e) => setError(errorMessage(e)));
  const Icon =
    op.status === "running"
      ? p?.phase === "queued"
        ? Hourglass
        : op.paused
          ? Pause
          : null
      : op.status === "done"
        ? CircleCheck
        : op.status === "errors"
          ? CircleAlert
          : CircleX;

  return (
    <div className={`fop-row fop-${op.status}`}>
      <div className="fop-head">
        <button className="fop-twisty" onClick={() => setOpen(!open)} disabled={!issues} aria-label="Details">
          {issues ? open ? <ChevronDown size={13} /> : <ChevronRight size={13} /> : null}
        </button>
        {Icon ? <Icon size={14} className="fop-icon" /> : <span className="fop-spinner" />}
        <span className="fop-title" title={`${op.srcRoot}${op.dstRoot ? ` → ${op.dstRoot}` : ""}`}>
          {op.title}
        </span>
        <span className="fop-status muted">{statusLine(op, started)}</span>
        <span className="spacer" />
        {running && (
          <>
            <button
              className="fop-btn"
              onClick={() => act(() => fileOps.pause(op.id, !op.paused))}
              title={op.paused ? "Resume" : "Pause between files and inside large copies"}
            >
              {op.paused ? <Play size={13} /> : <Pause size={13} />}
              {op.paused ? "Resume" : "Pause"}
            </button>
            <button className="fop-btn" onClick={() => act(() => fileOps.cancel(op.id))} title="Stop at the next safe point">
              <X size={13} /> Cancel
            </button>
          </>
        )}
        {backup && (
          <button className="fop-btn" onClick={() => openBackups(backup)} title="Show the replaced files on the Backups tab">
            <ArchiveRestore size={13} /> View
          </button>
        )}
        {!running && (
          <button className="fop-btn" onClick={() => fileOps.dismiss(op.id)} title="Remove from the list">
            <X size={13} />
          </button>
        )}
      </div>
      {running && p?.phase !== "queued" && (
        <ProgressBar
          className="fop-bar"
          size="slim"
          value={amounts(p)[0]}
          max={amounts(p)[1]}
          tone={op.paused || p?.phase === "paused" ? "warn" : "accent"}
          label={op.title}
        />
      )}
      {running && p?.current && <div className="fop-current" title={p.current}>{p.current}</div>}
      {q && (
        <div className="fop-question">
          <div>
            <b>{q.path}</b> already exists.
          </div>
          <div className="fop-compare muted">
            <span>
              Source: {q.source.isDir ? "folder" : formatSize(q.source.size)} · {formatTime(q.source.modifiedMs)}
            </span>
            <span>
              Target: {q.target.isDir ? "folder" : formatSize(q.target.size)} · {formatTime(q.target.modifiedMs)}
              {q.target.modifiedMs != null && q.source.modifiedMs != null && q.target.modifiedMs > q.source.modifiedMs
                ? " (newer)"
                : ""}
            </span>
          </div>
          <div className="fop-answers">
            {ANSWERS.map((a) => (
              <button key={a.answer} title={a.title} onClick={() => act(() => fileOps.answer(op.id, a.answer))}>
                {a.label}
              </button>
            ))}
          </div>
        </div>
      )}
      {error && <div className="fop-error">{error}</div>}
      {open && op.report && (
        <ul className="fop-issues">
          {op.report.failures.map((f, i) => (
            <li key={`f${i}`} className="s-error-fg">
              <b>{f.path}</b>: {f.message}
              {f.lockedBy?.length ? (
                <div className="fop-lockers">
                  <Lock size={11} /> In use by: {f.lockedBy.map((h) => `${h.name} (pid ${h.pid})`).join(", ")}
                </div>
              ) : null}
            </li>
          ))}
          {op.report.notes.map((n, i) => (
            <li key={`n${i}`} className="muted">
              {n}
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}

/** Running and finished operations, newest first. */
export function FileOpsPanel({ ops, onClose }: Props) {
  const running = ops.filter((o) => o.status === "running").length;
  return (
    <div className="fop-panel">
      <div className="fop-panel-head">
        <span className="fop-panel-title">Operations</span>
        <span className="muted">
          {running ? `${running} running` : "All done"}
          {ops.length > running ? ` · ${ops.length - running} finished` : ""}
        </span>
        <span className="spacer" />
        <button className="fop-btn" disabled={ops.length === running} onClick={() => fileOps.clearFinished()}>
          Clear finished
        </button>
        <button className="fop-btn" onClick={onClose} title="Hide the panel (operations keep running)">
          <X size={13} />
        </button>
      </div>
      <div className="fop-list">
        {ops.length === 0 && <div className="muted fop-empty">No operations yet.</div>}
        {[...ops].reverse().map((op) => (
          <OpRow key={op.id} op={op} />
        ))}
      </div>
    </div>
  );
}
