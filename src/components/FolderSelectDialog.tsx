// "Select…" for Folder Compare: selects the
// rows matching status, name, type, size and date criteria, replacing or
// adding to the current multi-selection that the file operations act on.
import { useMemo, useState } from "react";
import type { SelSide } from "./FolderFileOps";
import { ALL_SELECT_STATUSES, type SelectCriteria, type SelectStatus } from "../lib/folderTree";
import { Modal } from "./Modal";

export interface SelectRequest {
  criteria: SelectCriteria;
  side: SelSide;
  /** Keep the current selection and add the matches to it. */
  add: boolean;
  /** Expand collapsed folders that hold matches. */
  reachCollapsed: boolean;
}

interface Props {
  /** How many rows `criteria` would select, for the live count. */
  count: (criteria: SelectCriteria, reachCollapsed: boolean) => number;
  /** The flattened view has no folder rows. */
  flatten: boolean;
  onSelect: (req: SelectRequest) => void;
  onClose: () => void;
}

const STATUS_LABEL: Record<SelectStatus, string> = {
  diffLeftNewer: "Different — left newer",
  diffRightNewer: "Different — right newer",
  diffOther: "Different — same date / other",
  similar: "Similar (unimportant differences)",
  same: "Same",
  leftOnly: "Left only (orphans)",
  rightOnly: "Right only (orphans)",
  error: "Errors",
};

const DIFFERENT: SelectStatus[] = ["diffLeftNewer", "diffRightNewer", "diffOther"];

const DEFAULTS: SelectCriteria = {
  statuses: [...DIFFERENT, "leftOnly", "rightOnly"],
  include: "",
  exclude: "",
  files: true,
  folders: false,
  minSizeKb: null,
  maxSizeKb: null,
  modifiedAfter: "",
  modifiedBefore: "",
};

/** The last criteria used, so reopening the dialog picks up where it left off. */
let remembered: { criteria: SelectCriteria; side: SelSide; reachCollapsed: boolean } = {
  criteria: DEFAULTS,
  side: "both",
  reachCollapsed: true,
};

export function FolderSelectDialog({ count, flatten, onSelect, onClose }: Props) {
  const [c, setC] = useState<SelectCriteria>(remembered.criteria);
  const [side, setSide] = useState<SelSide>(remembered.side);
  const [reachCollapsed, setReachCollapsed] = useState(remembered.reachCollapsed);
  const set = <K extends keyof SelectCriteria>(k: K, v: SelectCriteria[K]) => setC((p) => ({ ...p, [k]: v }));
  const toggle = (list: SelectStatus[], on: boolean) =>
    setC((p) => ({ ...p, statuses: on ? [...new Set([...p.statuses, ...list])] : p.statuses.filter((s) => !list.includes(s)) }));
  const num = (v: string) => (v.trim() === "" ? null : Math.max(0, Number(v)));
  const matches = useMemo(() => count(c, reachCollapsed), [count, c, reachCollapsed]);

  const go = (add: boolean) => {
    remembered = { criteria: c, side, reachCollapsed };
    onSelect({ criteria: c, side, add, reachCollapsed });
    onClose();
  };

  return (
    <Modal
      title="Select files and folders"
      wide
      onClose={onClose}
      footer={
        <>
          <span className="muted fs-count">
            {matches.toLocaleString()} matching row{matches === 1 ? "" : "s"}
          </span>
          <button onClick={() => (setC(DEFAULTS), setSide("both"))}>Reset</button>
          <button onClick={onClose}>Cancel</button>
          <button disabled={!matches} onClick={() => go(true)} title="Keep the current selection and add the matches">
            Add to selection
          </button>
          <button className="primary" disabled={!matches} onClick={() => go(false)} title="Select only the matches">
            Select
          </button>
        </>
      }
    >
      <div className="settings-grid">
        <fieldset>
          <legend>Status</legend>
          <label className="field check">
            <input
              type="checkbox"
              checked={DIFFERENT.every((s) => c.statuses.includes(s))}
              onChange={(e) => toggle(DIFFERENT, e.target.checked)}
            />
            <b>All different</b>
          </label>
          {ALL_SELECT_STATUSES.map((s) => (
            <label key={s} className={`field check${DIFFERENT.includes(s) ? " indent" : ""}`}>
              <input type="checkbox" checked={c.statuses.includes(s)} onChange={(e) => toggle([s], e.target.checked)} />
              {STATUS_LABEL[s]}
            </label>
          ))}
        </fieldset>

        <fieldset>
          <legend>Names</legend>
          <label className="field">
            <span>Include</span>
            <input
              value={c.include}
              placeholder="(any)  e.g. *.cs;*.xml"
              spellCheck={false}
              autoFocus
              onChange={(e) => set("include", e.target.value)}
            />
          </label>
          <label className="field">
            <span>Exclude</span>
            <input
              value={c.exclude}
              placeholder="e.g. *.Designer.cs;obj/*"
              spellCheck={false}
              onChange={(e) => set("exclude", e.target.value)}
            />
          </label>
          <small className="muted">* and ? wildcards, separated by ; or ,. Patterns with a / match the relative path.</small>
          <div className="field-row">
            <label className="field check">
              <input type="checkbox" checked={c.files} onChange={(e) => set("files", e.target.checked)} />
              Files
            </label>
            <label className="field check" title={flatten ? "The flattened view lists files only" : undefined}>
              <input
                type="checkbox"
                checked={c.folders && !flatten}
                disabled={flatten}
                onChange={(e) => set("folders", e.target.checked)}
              />
              Folders
            </label>
          </div>
        </fieldset>

        <fieldset>
          <legend>Size &amp; date (files)</legend>
          <div className="field-row">
            <label className="field">
              <span>Min size (KB)</span>
              <input type="number" min={0} value={c.minSizeKb ?? ""} onChange={(e) => set("minSizeKb", num(e.target.value))} />
            </label>
            <label className="field">
              <span>Max size (KB)</span>
              <input type="number" min={0} value={c.maxSizeKb ?? ""} onChange={(e) => set("maxSizeKb", num(e.target.value))} />
            </label>
          </div>
          <div className="field-row">
            <label className="field">
              <span>Modified after</span>
              <input type="date" value={c.modifiedAfter} onChange={(e) => set("modifiedAfter", e.target.value)} />
            </label>
            <label className="field">
              <span>Modified before</span>
              <input type="date" value={c.modifiedBefore} onChange={(e) => set("modifiedBefore", e.target.value)} />
            </label>
          </div>
          <small className="muted">A pair matches when either side is in range.</small>
        </fieldset>

        <fieldset>
          <legend>Selection</legend>
          <label className="field">
            <span>Select on</span>
            <select value={side} onChange={(e) => setSide(e.target.value as SelSide)}>
              <option value="both">Both sides</option>
              <option value="left">Left side</option>
              <option value="right">Right side</option>
            </select>
          </label>
          <small className="muted">File operations (copy, delete…) act on the selected side’s items.</small>
          <label className="field check">
            <input
              type="checkbox"
              checked={reachCollapsed || flatten}
              disabled={flatten}
              onChange={(e) => setReachCollapsed(e.target.checked)}
            />
            Expand folders to select matches inside them
          </label>
          <small className="muted">Only rows the display filter shows can be selected.</small>
        </fieldset>
      </div>
    </Modal>
  );
}
