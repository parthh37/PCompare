import { Database, Eraser, Play, RefreshCw, Undo2 } from "lucide-react";
import { useCallback, useEffect, useState } from "react";
import { errorMessage } from "../lib/api";
import { formatSize, formatTime } from "../lib/format";
import {
  syncApi,
  type IndexStats,
  type JobDetail,
  type JobOp,
  type JobStatus,
  type JobSummary,
  type OpState,
  type SyncMode,
} from "../lib/syncApi";
import { Modal } from "./Modal";
import { VirtualList } from "./VirtualList";

interface Props {
  left: string;
  right: string;
  modeLabel: (m: SyncMode) => string;
  /** A sync is running: job actions are unavailable. */
  busy: boolean;
  onRollback: (jobId: string) => void;
  /** Absent: interrupted jobs can't be resumed from here (Sync Automation repairs them on the next run). */
  onResume?: (jobId: string) => void;
  /** The pair's two-way baseline was dropped, so the plan needs refreshing. */
  onHistoryForgotten: () => void;
  onClose: () => void;
  /** automation:ui — the job to select when the dialog opens. */
  initialJob?: string | null;
}

export const JOB_STATUS_LABELS: Record<JobStatus, string> = {
  running: "Running",
  completed: "Completed",
  completedWithErrors: "Completed with errors",
  cancelled: "Cancelled",
  stopped: "Stopped",
  interrupted: "Interrupted",
  rolledBack: "Rolled back",
};

const OP_STATE_LABELS: Record<OpState, string> = {
  pending: "Not run",
  running: "Unfinished",
  done: "Done",
  failed: "Failed",
  rolledBack: "Rolled back",
};

const opText = (j: JobOp) => {
  const side = j.op.direction === "toRight" ? "right" : "left";
  return j.op.kind === "copy" ? `Copy to ${side}` : j.op.kind === "delete" ? `Delete on ${side}` : `Create on ${side}`;
};

/** Past sync jobs of this folder pair, their operations, and the scan database. */
export function SyncHistoryDialog({
  left,
  right,
  modeLabel,
  busy,
  onRollback,
  onResume,
  onHistoryForgotten,
  onClose,
  initialJob,
}: Props) {
  const [jobs, setJobs] = useState<JobSummary[] | null>(null);
  const [selected, setSelected] = useState<string | null>(initialJob ?? null);
  const [detail, setDetail] = useState<JobDetail | null>(null);
  const [stats, setStats] = useState<IndexStats | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [note, setNote] = useState<string | null>(null);
  const [confirmForget, setConfirmForget] = useState(false);

  const load = useCallback(() => {
    setError(null);
    syncApi.jobs(left, right, 30).then(setJobs, (e) => setError(errorMessage(e)));
    syncApi.indexStats().then(setStats, (e) => setError(errorMessage(e)));
  }, [left, right]);

  useEffect(load, [load]);

  useEffect(() => {
    setDetail(null);
    if (!selected) return;
    let live = true;
    syncApi.jobDetail(selected).then(
      (d) => live && setDetail(d),
      (e) => live && setError(errorMessage(e)),
    );
    return () => {
      live = false;
    };
  }, [selected]);

  const clearChecksums = async () => {
    try {
      const n = await syncApi.clearChecksums();
      setNote(`Cleared ${n.toLocaleString()} cached checksum${n === 1 ? "" : "s"}.`);
      setStats(await syncApi.indexStats());
    } catch (e) {
      setError(errorMessage(e));
    }
  };

  const forget = async () => {
    setConfirmForget(false);
    try {
      await syncApi.forgetHistory(left, right);
      setNote("Two-way history forgotten. The next two-way sync of these folders starts fresh.");
      setStats(await syncApi.indexStats());
      onHistoryForgotten();
    } catch (e) {
      setError(errorMessage(e));
    }
  };

  const job = jobs?.find((j) => j.id === selected) ?? null;
  const ops = detail?.ops ?? [];

  const renderOp = (i: number) => {
    const j = ops[i];
    const message = j.result?.error ?? null;
    return (
      <div className={`sync-history-op st-${j.state}`} title={message ?? j.op.relPath}>
        <span className="state">{OP_STATE_LABELS[j.state]}</span>
        <span className="muted">{opText(j)}</span>
        <span className="path">{j.op.relPath}</span>
        <span className="err">{message}</span>
      </div>
    );
  };

  return (
    <Modal title="Sync history" wide onClose={onClose} footer={<button onClick={onClose}>Close</button>}>
      {error && <div className="banner error sync-dialog-banner">{error}</div>}
      {note && <div className="banner info sync-dialog-banner">{note}</div>}

      <div className="sync-history-head">
        <span>Started</span>
        <span>Mode</span>
        <span>Status</span>
        <span className="num">Done / failed / total</span>
        <span className="num">Copied</span>
      </div>
      <div className="sync-history-list">
        {jobs === null && <div className="empty-state">Loading…</div>}
        {jobs?.length === 0 && <div className="empty-state">No syncs of these folders yet.</div>}
        {jobs?.map((j) => (
          <div
            key={j.id}
            className={`sync-history-row${selected === j.id ? " selected" : ""}`}
            onClick={() => setSelected(selected === j.id ? null : j.id)}
          >
            <span>{formatTime(j.startedMs)}</span>
            <span>{modeLabel(j.mode)}</span>
            <span className={`sync-job-status js-${j.status}`}>{JOB_STATUS_LABELS[j.status]}</span>
            <span className="num">
              {j.doneOps} / <span className={j.failedOps ? "s-error-fg" : undefined}>{j.failedOps}</span> / {j.totalOps}
            </span>
            <span className="num">{formatSize(j.bytesCopied)}</span>
          </div>
        ))}
      </div>

      {job && (
        <div className="sync-history-detail">
          <div className="sync-history-actions">
            <span className="muted">
              {job.finishedMs ? `Finished ${formatTime(job.finishedMs)}` : "Never finished"}
              {job.pendingOps > 0 && ` · ${job.pendingOps} not run`}
            </span>
            <span className="spacer" />
            {job.status === "interrupted" && onResume && (
              <button onClick={() => onResume(job.id)} disabled={busy} title="Repair half-finished operations and run the rest">
                <Play size={14} /> Resume
              </button>
            )}
            {job.doneOps > 0 && job.status !== "rolledBack" && job.status !== "running" && (
              <button onClick={() => onRollback(job.id)} disabled={busy} title="Undo this job's completed operations">
                <Undo2 size={14} /> Roll back
              </button>
            )}
          </div>
          {!detail && <div className="empty-state">Loading…</div>}
          {detail && ops.length === 0 && <div className="empty-state">This job had no operations.</div>}
          {ops.length > 0 && (
            <div className="sync-history-ops">
              <VirtualList count={ops.length} rowHeight={22} renderRow={renderOp} />
            </div>
          )}
        </div>
      )}

      <div className="sync-history-stats">
        <Database size={14} />
        <span>
          {stats
            ? `Scan database: ${stats.checksums.toLocaleString()} cached checksums · ${stats.baselines.toLocaleString()} folder pair${stats.baselines === 1 ? "" : "s"} · ${formatSize(stats.dbBytes)}`
            : "Scan database: …"}
        </span>
        <span className="spacer" />
        <button onClick={load} title="Reload">
          <RefreshCw size={14} />
        </button>
        <button onClick={clearChecksums} disabled={busy || !stats?.checksums} title="Folders are re-read in full next time">
          <Eraser size={14} /> Clear checksum cache
        </button>
        <button onClick={() => setConfirmForget(true)} disabled={busy || confirmForget}>
          Forget two-way history for these folders
        </button>
      </div>
      {confirmForget && (
        <div className="sync-confirm-warn sync-inline-confirm">
          <span>
            Forget the two-way history of these folders? The next two-way sync treats both sides as new: nothing is
            deleted, and deletions made until then won't carry over.
          </span>
          <button className="danger" onClick={forget}>
            Forget
          </button>
          <button onClick={() => setConfirmForget(false)}>Cancel</button>
        </div>
      )}
    </Modal>
  );
}
