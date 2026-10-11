import { useState } from "react";
import { api, errorMessage, type SavedKind, type SavedSession } from "../lib/api";
import { Modal } from "./Modal";

interface Props {
  kind: SavedKind;
  left: string;
  right: string;
  settings: unknown;
  /** Set when the tab was opened from (or already saved as) a session. */
  sessionId?: string;
  defaultName: string;
  onSaved: (s: SavedSession) => void;
  onClose: () => void;
}

export function SaveSessionDialog({ kind, left, right, settings, sessionId, defaultName, onSaved, onClose }: Props) {
  const [name, setName] = useState(defaultName);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const save = async (asNew: boolean) => {
    if (!name.trim()) return;
    setBusy(true);
    setError(null);
    try {
      const saved = await api.saveSession({ id: asNew ? undefined : sessionId, name, kind, left, right, settings });
      onSaved(saved);
      onClose();
    } catch (e) {
      setError(errorMessage(e));
      setBusy(false);
    }
  };

  return (
    <Modal
      title={sessionId ? "Save session" : "Save session as…"}
      onClose={onClose}
      footer={
        <>
          <button onClick={onClose}>Cancel</button>
          {sessionId && (
            <button onClick={() => save(true)} disabled={busy || !name.trim()}>
              Save as new
            </button>
          )}
          <button className="primary" onClick={() => save(false)} disabled={busy || !name.trim()}>
            Save
          </button>
        </>
      }
    >
      <label className="field">
        <span>Session name</span>
        <input
          autoFocus
          value={name}
          onChange={(e) => setName(e.target.value)}
          onKeyDown={(e) => e.key === "Enter" && save(false)}
          onFocus={(e) => e.target.select()}
          spellCheck={false}
        />
      </label>
      <div className="save-summary muted">
        <div title={left}>◀ {left || "—"}</div>
        <div title={right}>▶ {right || "—"}</div>
      </div>
      {error && <div className="banner error">{error}</div>}
    </Modal>
  );
}
