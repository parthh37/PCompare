import { open } from "@tauri-apps/plugin-dialog";
import { FolderOpen } from "lucide-react";
import { useState } from "react";
import { errorMessage } from "../lib/api";
import {
  DEFAULT_EXEC_OPTIONS,
  syncApi,
  type Disposal,
  type ErrorPolicy,
  type ExecOptions,
  type LockedPolicy,
} from "../lib/syncApi";
import { Modal } from "./Modal";

export const TRASH_NAME = navigator.userAgent.includes("Windows") ? "Recycle Bin" : "Trash";

interface Props {
  value: ExecOptions;
  onApply: (o: ExecOptions) => void;
  onClose: () => void;
}

const LOCKED_POLICIES: { value: LockedPolicy; label: string; help: string }[] = [
  {
    value: "defer",
    label: "Defer to end of run (recommended)",
    help: "Everything else goes first; locked files get another round of retries at the end.",
  },
  { value: "skip", label: "Skip", help: "Report them as failed and carry on." },
  { value: "stop", label: "Stop the sync", help: "Stop at the first file that stays locked. The rest can be run later." },
];

/** One line describing the safety settings, e.g. "Locked files deferred · 3 retries · replaced files → Trash · verify off". */
export function safetySummary(o: ExecOptions): string {
  const parts = [
    o.onLocked === "defer" ? "Locked files deferred" : o.onLocked === "skip" ? "Locked files skipped" : "Stop on locked files",
    o.retryAttempts === 0 ? "no retries" : `${o.retryAttempts} retr${o.retryAttempts === 1 ? "y" : "ies"}`,
    o.disposal.kind === "trash"
      ? `replaced files → ${TRASH_NAME}`
      : o.disposal.kind === "versioning"
        ? "replaced files → versioning folder"
        : "replaced files deleted permanently",
    `verify ${o.verify ? "on" : "off"}`,
  ];
  if (o.onError === "stop") parts.push("stop on errors");
  if (!o.checkDrift) parts.push("drift check off");
  return parts.join(" · ");
}

/** Execution settings for Folder Sync: retries, locked files, verification and what happens to replaced files. */
export function SyncOptionsDialog({ value, onApply, onClose }: Props) {
  const [o, setO] = useState<ExecOptions>(value);
  // Remembered separately so flipping between disposal kinds doesn't lose a typed path.
  const [folder, setFolder] = useState(value.disposal.kind === "versioning" ? value.disposal.folder : "");
  const [folderError, setFolderError] = useState<string | null>(null);
  const set = <K extends keyof ExecOptions>(k: K, v: ExecOptions[K]) => setO((p) => ({ ...p, [k]: v }));
  const clamp = (v: string, min: number, max: number) => Math.min(max, Math.max(min, Math.round(Number(v) || 0)));

  const setDisposal = (kind: Disposal["kind"]) => {
    setO((p) => ({ ...p, disposal: kind === "versioning" ? { kind, folder } : { kind } }));
    if (kind === "versioning" && !folder.trim()) {
      syncApi
        .defaultVersioningFolder()
        .then((f) => setFolder((cur) => cur || f))
        .catch((e) => setFolderError(errorMessage(e)));
    }
  };

  const browse = async () => {
    const picked = await open({ directory: true, multiple: false, title: "Choose versioning folder" });
    if (typeof picked === "string") setFolder(picked);
  };

  const reset = () => {
    setO(DEFAULT_EXEC_OPTIONS);
    setFolderError(null);
  };

  const versioning = o.disposal.kind === "versioning";
  const missingFolder = versioning && !folder.trim();
  const save = () => {
    onApply(versioning ? { ...o, disposal: { kind: "versioning", folder: folder.trim() } } : o);
    onClose();
  };

  return (
    <Modal
      title="Sync options"
      wide
      onClose={onClose}
      footer={
        <>
          <button className="sync-foot-left" onClick={reset} title="Restore the recommended settings (not saved until you click Save)">
            Reset to defaults
          </button>
          <button onClick={onClose}>Cancel</button>
          <button className="primary" onClick={save} disabled={missingFolder}>
            Save
          </button>
        </>
      }
    >
      <div className="settings-grid sync-options">
        <fieldset>
          <legend>Errors and retries</legend>
          <div className="field-row">
            <label className="field">
              <span>Retries</span>
              <input
                type="number"
                min={0}
                max={10}
                value={o.retryAttempts}
                onChange={(e) => set("retryAttempts", clamp(e.target.value, 0, 10))}
              />
            </label>
            <label className="field">
              <span>First delay (ms)</span>
              <input
                type="number"
                min={0}
                step={100}
                value={o.retryDelayMs}
                disabled={o.retryAttempts === 0}
                onChange={(e) => {
                  const ms = clamp(e.target.value, 0, 60_000);
                  setO((p) => ({ ...p, retryDelayMs: ms, retryMaxDelayMs: Math.max(p.retryMaxDelayMs, ms) }));
                }}
              />
            </label>
          </div>
          <small className="muted sync-options-help">
            Files in use, network hiccups and similar transient errors are retried; the delay doubles each time.
          </small>

          <div className="sync-options-group">Files in use (locked)</div>
          {LOCKED_POLICIES.map((p) => (
            <label key={p.value} className="field radio">
              <input type="radio" name="onLocked" checked={o.onLocked === p.value} onChange={() => set("onLocked", p.value)} />
              <span>
                <b>{p.label}</b>
                <small className="muted">{p.help}</small>
              </span>
            </label>
          ))}

          <label className="field">
            <span>On other errors</span>
            <select value={o.onError} onChange={(e) => set("onError", e.target.value as ErrorPolicy)}>
              <option value="continue">Continue with the remaining files</option>
              <option value="stop">Stop the sync</option>
            </select>
          </label>
        </fieldset>

        <fieldset>
          <legend>Safety</legend>
          <label className="field check">
            <input type="checkbox" checked={o.verify} onChange={(e) => set("verify", e.target.checked)} />
            Verify copies (read back and compare checksums)
          </label>
          <label className="field check">
            <input type="checkbox" checked={o.checkDrift} onChange={(e) => set("checkDrift", e.target.checked)} />
            Don't overwrite or delete files changed since the preview
          </label>
          {!o.checkDrift && (
            <p className="sync-confirm-warn sync-options-note">
              Edits made after the preview can be overwritten or deleted without notice.
            </p>
          )}

          <div className="sync-options-group">Deleted and replaced files</div>
          <label className="field radio">
            <input type="radio" name="disposal" checked={o.disposal.kind === "trash"} onChange={() => setDisposal("trash")} />
            <span>
              <b>Move to {TRASH_NAME}</b>
            </span>
          </label>
          <label className="field radio">
            <input type="radio" name="disposal" checked={versioning} onChange={() => setDisposal("versioning")} />
            <span>
              <b>Move to versioning folder</b>
              <small className="muted">Enables exact rollback.</small>
            </span>
          </label>
          {versioning && (
            <div className="field indent">
              <div className="input-row">
                <input
                  value={folder}
                  onChange={(e) => setFolder(e.target.value)}
                  placeholder="Folder for old versions"
                  spellCheck={false}
                />
                <button onClick={browse} title="Choose folder">
                  <FolderOpen size={14} /> Browse…
                </button>
              </div>
              {folderError && <small className="s-error-fg">{folderError}</small>}
              {missingFolder && !folderError && <small className="muted">Choose a folder to save.</small>}
            </div>
          )}
          <label className="field radio sync-danger-choice">
            <input
              type="radio"
              name="disposal"
              checked={o.disposal.kind === "permanent"}
              onChange={() => setDisposal("permanent")}
            />
            <span>
              <b>Delete permanently</b>
              <small>Can't be undone or rolled back.</small>
            </span>
          </label>
        </fieldset>
      </div>
    </Modal>
  );
}
