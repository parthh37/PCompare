import { BellOff, CircleArrowUp } from "lucide-react";
import type { Updater } from "../lib/useUpdater";
import { Modal } from "./Modal";
import { ProgressBar } from "./ProgressBar";

/**
 * "A new version is available" pop-up. It reappears on every hourly check
 * until the update is installed or the user turns reminders off.
 */
export function UpdatePrompt({ updater }: { updater: Updater }) {
  const { phase } = updater;
  if (!updater.promptVisible || (phase.kind !== "available" && phase.kind !== "downloading")) return null;
  const { update } = phase;
  const downloading = phase.kind === "downloading";
  const pct = downloading && phase.total ? Math.round((phase.done / phase.total) * 100) : null;

  return (
    <Modal
      title="Update available"
      className="update-prompt"
      // Escape / backdrop click = "Later"; a running install can't be dismissed.
      onClose={downloading ? () => {} : updater.remindLater}
      footer={
        downloading ? undefined : (
          <>
            <button onClick={updater.disableReminders} title="Stop these pop-ups (re-enable in Settings → Updates)">
              <BellOff size={14} /> Disable reminders
            </button>
            <span className="spacer" />
            <button onClick={updater.remindLater} title="Ask again in an hour">
              Remind me later
            </button>
            <button className="primary" onClick={updater.install} autoFocus>
              Install &amp; restart
            </button>
          </>
        )
      }
    >
      <div className="update-prompt-head">
        <CircleArrowUp size={28} className="update-prompt-icon" />
        <div>
          <b>PCompare {update.version}</b> is available.
          <div className="muted">You have {updater.version || "an older version"}.</div>
        </div>
      </div>
      {update.body && <pre className="update-prompt-notes">{update.body.trim()}</pre>}
      {downloading ? (
        <div className="update-prompt-progress">
          <span>Downloading{pct != null ? ` — ${pct}%` : "…"}</span>
          <ProgressBar value={phase.done} max={phase.total} label="Downloading the update" />
        </div>
      ) : (
        <small className="muted">
          {updater.reminders
            ? "We'll remind you every hour until you update."
            : "Reminders are off; this was a manual check."}
        </small>
      )}
    </Modal>
  );
}
