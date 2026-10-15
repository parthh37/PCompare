// the custom sync table: one action per category, like
// Folder Sync's custom "Sync" settings. Shared by Folder Sync's
// Custom mode dialog and the Sync Automation task editor.
import { TriangleAlert } from "lucide-react";
import type { SyncMatrix } from "../lib/syncApi";
import { ACTIONS, CATEGORIES, PRESET_MATRICES, actionApplies, categorySides, presetOf } from "../lib/syncMatrix";

interface Props {
  value: SyncMatrix;
  onChange: (next: SyncMatrix) => void;
  /** Labels for the "Start from" presets (FolderSync's MODES). */
  presetLabel: (mode: keyof typeof PRESET_MATRICES) => string;
  disabled?: boolean;
}

export function SyncMatrixEditor({ value, onChange, presetLabel, disabled }: Props) {
  const preset = presetOf(value);
  const prompts = CATEGORIES.filter((c) => value[c.value] === "prompt").length;
  return (
    <div className="sync-matrix">
      <label className="sync-matrix-start">
        Start from
        <select
          value={preset ?? ""}
          onChange={(e) => e.target.value && onChange({ ...PRESET_MATRICES[e.target.value as keyof typeof PRESET_MATRICES] })}
          disabled={disabled}
        >
          <option value="">{preset ? "" : "Custom"}</option>
          {(Object.keys(PRESET_MATRICES) as (keyof typeof PRESET_MATRICES)[]).map((m) => (
            <option key={m} value={m}>
              {presetLabel(m)}
            </option>
          ))}
        </select>
      </label>
      <table>
        <thead>
          <tr>
            <th>When an item is…</th>
            <th>Action</th>
          </tr>
        </thead>
        <tbody>
          {CATEGORIES.map((c) => {
            const sides = categorySides(c.value);
            const action = value[c.value];
            const deletes = action === "deleteLeft" || action === "deleteRight";
            return (
              <tr key={c.value} title={c.hint}>
                <td>{c.label}</td>
                <td>
                  <select
                    className={deletes ? "deletes" : undefined}
                    value={action}
                    onChange={(e) => onChange({ ...value, [c.value]: e.target.value })}
                    disabled={disabled}
                    aria-label={c.label}
                  >
                    {ACTIONS.filter((a) => a.value === action || actionApplies(a.value, sides.left, sides.right)).map((a) => (
                      <option key={a.value} value={a.value}>
                        {a.label}
                      </option>
                    ))}
                  </select>
                </td>
              </tr>
            );
          })}
        </tbody>
      </table>
      <small className="muted">
        “Deleted on left/right” needs an earlier sync of these folders to tell a deletion from a new file; until then
        such items count as orphans.
      </small>
      {prompts > 0 && (
        <small className="muted">
          <TriangleAlert size={12} /> Prompt items are listed as conflicts; pick their action in the preview.
        </small>
      )}
    </div>
  );
}
