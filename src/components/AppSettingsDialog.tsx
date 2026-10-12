import { CircleArrowUp } from "lucide-react";
import type { Updater } from "../lib/useUpdater";
import { Modal } from "./Modal";

interface Props {
  updater: Updater;
  onClose: () => void;
}

function updateStatus({ phase, checkedAt }: Updater): string {
  const at = checkedAt
    ? ` Last checked ${checkedAt.toLocaleTimeString([], { hour: "numeric", minute: "2-digit" })}.`
    : "";
  switch (phase.kind) {
    case "checking":
      return "Checking for updates…";
    case "available":
      return `Version ${phase.update.version} is available.`;
    case "downloading":
      return `Downloading version ${phase.update.version}…`;
    case "error":
      return `Update check failed: ${phase.message}`;
    case "idle":
      return checkedAt ? `You're on the latest version.${at}` : "Not checked yet this session.";
  }
}

/** Application-wide preferences (per-session options live in each session's own settings). */
export function AppSettingsDialog({ updater, onClose }: Props) {
  const { phase } = updater;
  const busy = phase.kind === "checking" || phase.kind === "downloading";

  return (
    <Modal title="Settings" onClose={onClose} footer={<button onClick={onClose}>Close</button>}>
      <div className="settings-grid single">
        <fieldset>
          <legend>Updates</legend>
          <div className="update-status">
            <span>
              <span>
                PCompare <b>{updater.version || "—"}</b>
              </span>
              <small className="muted">{updateStatus(updater)}</small>
            </span>
            {phase.kind === "available" ? (
              <button className="primary" onClick={updater.install}>
                Install &amp; restart
              </button>
            ) : (
              <button disabled={busy} onClick={updater.checkNow}>
                <CircleArrowUp size={14} /> {phase.kind === "checking" ? "Checking…" : "Check for updates"}
              </button>
            )}
          </div>
          <label className="field check">
            <input
              type="checkbox"
              checked={updater.autoCheck}
              onChange={(e) => updater.setAutoCheck(e.target.checked)}
            />
            Check for updates automatically
          </label>
        </fieldset>
      </div>
    </Modal>
  );
}
