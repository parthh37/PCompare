import { useState } from "react";
import { errorMessage } from "../lib/api";
import { workspaceApi, type Workspace, type WorkspaceTab } from "../lib/workspaceApi";
import "./AppShell.css";
import { Modal } from "./Modal";
import { SessionIcon } from "./SessionIcon";

interface Props {
  tabs: WorkspaceTab[];
  active: number;
  /** Existing workspaces, offered for overwriting. */
  workspaces: Workspace[];
  /** Tab titles, parallel to `tabs`, for the summary. */
  titles: string[];
  onSaved: (w: Workspace) => void;
  onClose: () => void;
}

/** "Save workspace…": stores every open tab under a new or existing name. */
export function SaveWorkspaceDialog({ tabs, active, workspaces, titles, onSaved, onClose }: Props) {
  const [target, setTarget] = useState<string>("");
  const [name, setName] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const overwriting = workspaces.find((w) => w.id === target);
  const finalName = overwriting && !name.trim() ? overwriting.name : name;

  const save = async () => {
    if (!finalName.trim()) return;
    setBusy(true);
    setError(null);
    try {
      onSaved(await workspaceApi.save({ id: overwriting?.id, name: finalName, tabs, active }));
      onClose();
    } catch (e) {
      setError(errorMessage(e));
      setBusy(false);
    }
  };

  return (
    <Modal
      title="Save workspace"
      onClose={onClose}
      footer={
        <>
          <button onClick={onClose}>Cancel</button>
          <button className="primary" onClick={save} disabled={busy || !finalName.trim()}>
            {overwriting ? "Overwrite" : "Save"}
          </button>
        </>
      }
    >
      {workspaces.length > 0 && (
        <label className="field">
          <span>Save as</span>
          <select value={target} onChange={(e) => setTarget(e.target.value)}>
            <option value="">New workspace</option>
            {workspaces.map((w) => (
              <option key={w.id} value={w.id}>
                Replace “{w.name}”
              </option>
            ))}
          </select>
        </label>
      )}
      <label className="field">
        <span>Workspace name</span>
        <input
          autoFocus
          value={name}
          placeholder={overwriting?.name ?? "e.g. Release 2.4 review"}
          onChange={(e) => setName(e.target.value)}
          onKeyDown={(e) => e.key === "Enter" && save()}
          spellCheck={false}
        />
      </label>
      <div className="muted ws-summary">
        {tabs.length} tab{tabs.length === 1 ? "" : "s"}:
        <ul>
          {tabs.map((t, i) => (
            <li key={i} className={i === active ? "active" : undefined}>
              <SessionIcon kind={t.kind} size={12} /> {titles[i]}
            </li>
          ))}
        </ul>
      </div>
      {error && <div className="banner error">{error}</div>}
    </Modal>
  );
}
