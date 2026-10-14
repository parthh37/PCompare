// Small Sync Automation dialogs: global settings, and importing / exporting
// task definitions as JSON (no secrets: channels are referenced by id only).
import { open, save } from "@tauri-apps/plugin-dialog";
import { ClipboardCopy, Download, FolderOpen, Upload } from "lucide-react";
import { useEffect, useState } from "react";
import { errorMessage } from "../lib/api";
import { automationApi, type AutomationSettings, type SyncTask } from "../lib/automationApi";
import { writeClipboardText } from "../lib/clipboard";
import { mergeApi } from "../lib/mergeApi";
import { textApi } from "../lib/textApi";
import "./Automation.css";
import { Modal } from "./Modal";

const JSON_FILTER = [{ name: "PCompare automation", extensions: ["json"] }];

export function AutomationSettingsDialog({
  value,
  onSaved,
  onClose,
}: {
  value: AutomationSettings;
  onSaved: (s: AutomationSettings) => void;
  onClose: () => void;
}) {
  const [s, setS] = useState(value);
  const [error, setError] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  const set = (patch: Partial<AutomationSettings>) => setS((p) => ({ ...p, ...patch }));
  const int = (v: string, min: number, max: number) => Math.min(max, Math.max(min, Math.round(Number(v) || 0)));

  const submit = async () => {
    setSaving(true);
    setError(null);
    try {
      await automationApi.saveSettings(s);
      onSaved(s);
      onClose();
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setSaving(false);
    }
  };

  return (
    <Modal
      title="Automation settings"
      className="auto-settings"
      onClose={onClose}
      footer={
        <>
          <button onClick={onClose}>Cancel</button>
          <button className="primary" onClick={submit} disabled={saving}>
            {saving ? "Saving…" : "Save"}
          </button>
        </>
      }
    >
      {error && <div className="banner error auto-banner">{error}</div>}
      <label className="field check">
        <input type="checkbox" checked={s.runInBackground} onChange={(e) => set({ runInBackground: e.target.checked })} />
        <span>
          Keep running in the background when the window is closed
          <small className="muted auto-help">
            PCompare stays in the notification area (tray) so scheduled and real-time syncs keep working. Quit from the
            tray icon.
          </small>
        </span>
      </label>
      <label className="field check">
        <input type="checkbox" checked={s.startAtLogin} onChange={(e) => set({ startAtLogin: e.target.checked })} />
        <span>
          Start PCompare when I sign in
          <small className="muted auto-help">Starts in the background, without opening a window.</small>
        </span>
      </label>
      <div className="field-row">
        <label className="field">
          <span>Tasks running at the same time</span>
          <input type="number" min={1} max={16} value={s.maxParallel} onChange={(e) => set({ maxParallel: int(e.target.value, 1, 16) })} />
        </label>
        <label className="field">
          <span>Keep run history for (days)</span>
          <input type="number" min={1} max={3650} value={s.historyDays} onChange={(e) => set({ historyDays: int(e.target.value, 1, 3650) })} />
        </label>
      </div>
      {!s.runInBackground && (
        <p className="sync-confirm-warn auto-inline-warn">
          Automated syncs only run while the PCompare window is open.
        </p>
      )}
    </Modal>
  );
}

/** Export selected tasks as JSON (clipboard or file). */
export function ExportTasksDialog({ tasks, onClose }: { tasks: SyncTask[]; onClose: () => void }) {
  const [chosen, setChosen] = useState<Set<string>>(() => new Set(tasks.map((t) => t.id)));
  const [json, setJson] = useState("");
  const [note, setNote] = useState<{ ok: boolean; text: string } | null>(null);

  useEffect(() => {
    let live = true;
    if (!chosen.size) return void setJson("");
    automationApi.exportTasks([...chosen]).then(
      (text) => live && setJson(text),
      (e) => live && setNote({ ok: false, text: errorMessage(e) }),
    );
    return () => {
      live = false;
    };
  }, [chosen]);

  const toggle = (id: string) =>
    setChosen((prev) => {
      const next = new Set(prev);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });

  const copy = () =>
    writeClipboardText(json).then(
      () => setNote({ ok: true, text: "Copied to the clipboard." }),
      (e) => setNote({ ok: false, text: errorMessage(e) }),
    );

  const saveFile = async () => {
    const path = await save({ title: "Export automation tasks", defaultPath: "pcompare-automation.json", filters: JSON_FILTER });
    if (!path) return;
    try {
      await mergeApi.saveTextFile(path, json, { encoding: "UTF-8", hasBom: false, lineEnding: "lf" });
      setNote({ ok: true, text: `Saved ${path}` });
    } catch (e) {
      setNote({ ok: false, text: errorMessage(e) });
    }
  };

  return (
    <Modal
      title="Export tasks"
      wide
      onClose={onClose}
      footer={
        <>
          <span className="sync-foot-left muted">Notification secrets are never exported.</span>
          <button onClick={copy} disabled={!json}>
            <ClipboardCopy size={14} /> Copy
          </button>
          <button onClick={saveFile} disabled={!json}>
            <Download size={14} /> Save as…
          </button>
          <button className="primary" onClick={onClose}>
            Done
          </button>
        </>
      }
    >
      {note && <div className={`banner ${note.ok ? "ok" : "error"} auto-banner`}>{note.text}</div>}
      <div className="auto-transfer-list">
        {tasks.map((t) => (
          <label key={t.id} className="field check">
            <input type="checkbox" checked={chosen.has(t.id)} onChange={() => toggle(t.id)} />
            {t.name}
          </label>
        ))}
      </div>
      <textarea className="auto-json" readOnly value={json} spellCheck={false} />
    </Modal>
  );
}

/** Import tasks from JSON text or a file (the engine merges them by name, with new ids). */
export function ImportTasksDialog({ onImported, onClose }: { onImported: (tasks: SyncTask[]) => void; onClose: () => void }) {
  const [json, setJson] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const openFile = async () => {
    const path = await open({ title: "Import automation tasks", multiple: false, filters: JSON_FILTER });
    if (typeof path !== "string") return;
    try {
      const doc = await textApi.loadText(path, null);
      setJson(doc.lines.join("\n"));
      setError(null);
    } catch (e) {
      setError(errorMessage(e));
    }
  };

  const submit = async () => {
    setBusy(true);
    setError(null);
    try {
      onImported(await automationApi.importTasks(json));
      onClose();
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <Modal
      title="Import tasks"
      wide
      onClose={onClose}
      footer={
        <>
          <button className="sync-foot-left" onClick={openFile}>
            <FolderOpen size={14} /> Open file…
          </button>
          <button onClick={onClose}>Cancel</button>
          <button className="primary" onClick={submit} disabled={busy || !json.trim()}>
            <Upload size={14} /> {busy ? "Importing…" : "Import"}
          </button>
        </>
      }
    >
      {error && <div className="banner error auto-banner">{error}</div>}
      <p className="muted auto-help-p">
        Paste exported tasks, or open a file. Notification secrets aren't part of an export: on another computer, pick
        the task's channels again on its Notifications tab.
      </p>
      <textarea
        className="auto-json"
        value={json}
        onChange={(e) => setJson(e.target.value)}
        placeholder='{ "tasks": [ … ] }'
        spellCheck={false}
      />
    </Modal>
  );
}
