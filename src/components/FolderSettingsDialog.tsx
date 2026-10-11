import { useState } from "react";
import type { ContentMethod, WhitespaceMode } from "../lib/api";
import type { CompareSettings } from "../lib/folderSettings";
import { Modal } from "./Modal";

interface Props {
  value: CompareSettings;
  onApply: (s: CompareSettings) => void;
  onClose: () => void;
}

type Page = "comparison" | "filters" | "rules";

const CONTENT_METHODS: { value: ContentMethod; label: string; help: string }[] = [
  { value: "off", label: "Off", help: "Only the quick tests decide." },
  { value: "binary", label: "Binary", help: "Byte-for-byte; stops at the first difference. Fastest content check." },
  { value: "crc", label: "CRC-32", help: "Reads both files fully and shows their checksums." },
  {
    value: "rules",
    label: "Rules-based",
    help: "Compares text with the rules tab: encoding, line endings, whitespace, case and ignored patterns can be unimportant (shown as ≈ similar). Binary files fall back to binary.",
  },
];

/** Beyond Compare-style session settings: comparison, filters, text rules. */
export function FolderSettingsDialog({ value, onApply, onClose }: Props) {
  const [s, setS] = useState<CompareSettings>(value);
  const [page, setPage] = useState<Page>("comparison");
  const set = <K extends keyof CompareSettings>(k: K, v: CompareSettings[K]) => setS((p) => ({ ...p, [k]: v }));
  const setRules = (patch: Partial<CompareSettings["rules"]>) => setS((p) => ({ ...p, rules: { ...p.rules, ...patch } }));
  const num = (v: string) => (v.trim() === "" ? null : Math.max(0, Number(v)));

  return (
    <Modal
      title="Session settings"
      wide
      onClose={onClose}
      footer={
        <>
          <button onClick={onClose}>Cancel</button>
          <button
            className="primary"
            onClick={() => {
              onApply(s);
              onClose();
            }}
          >
            Apply
          </button>
        </>
      }
    >
      <div className="seg-control settings-pages">
        {(
          [
            ["comparison", "Comparison"],
            ["filters", "Filters"],
            ["rules", "Text rules"],
          ] as [Page, string][]
        ).map(([p, label]) => (
          <button key={p} className={page === p ? "active" : ""} onClick={() => setPage(p)}>
            {label}
          </button>
        ))}
      </div>

      {page === "comparison" && (
        <div className="settings-grid">
          <fieldset>
            <legend>Quick tests</legend>
            <label className="field check">
              <input type="checkbox" checked={s.compareSize} onChange={(e) => set("compareSize", e.target.checked)} />
              Compare file size
            </label>
            <label className="field check">
              <input
                type="checkbox"
                checked={s.compareTimestamp}
                onChange={(e) => set("compareTimestamp", e.target.checked)}
              />
              Compare timestamps
            </label>
            <label className="field indent">
              <span>Tolerance (seconds)</span>
              <input
                type="number"
                min={0}
                value={s.timeToleranceSecs}
                disabled={!s.compareTimestamp}
                onChange={(e) => set("timeToleranceSecs", num(e.target.value) ?? 0)}
              />
            </label>
            <label className="field check indent">
              <input
                type="checkbox"
                checked={s.ignoreDstShift}
                disabled={!s.compareTimestamp}
                onChange={(e) => set("ignoreDstShift", e.target.checked)}
              />
              Ignore 1-hour differences (DST / FAT)
            </label>
          </fieldset>

          <fieldset>
            <legend>Compare contents</legend>
            {CONTENT_METHODS.map((m) => (
              <label key={m.value} className="field radio">
                <input
                  type="radio"
                  name="content"
                  checked={s.content === m.value}
                  onChange={() => set("content", m.value)}
                />
                <span>
                  <b>{m.label}</b>
                  <small className="muted">{m.help}</small>
                </span>
              </label>
            ))}
            <label className="field check">
              <input
                type="checkbox"
                checked={s.overrideQuickTests}
                disabled={s.content === "off"}
                onChange={(e) => set("overrideQuickTests", e.target.checked)}
              />
              Override quick test results
            </label>
            <small className="muted">
              When on, contents decide (identical files with different timestamps are “same”). When off, files must
              pass both.
            </small>
          </fieldset>
        </div>
      )}

      {page === "filters" && (
        <div className="settings-grid">
          <fieldset>
            <legend>Name filters</legend>
            <small className="muted">Comma-separated globs, e.g. *.rs, src/**/*.ts</small>
            <label className="field">
              <span>Include files</span>
              <input
                value={s.includeFiles}
                placeholder="(all files)"
                spellCheck={false}
                onChange={(e) => set("includeFiles", e.target.value)}
              />
            </label>
            <label className="field">
              <span>Exclude files</span>
              <input
                value={s.excludeFiles}
                placeholder="*.tmp, *.log"
                spellCheck={false}
                onChange={(e) => set("excludeFiles", e.target.value)}
              />
            </label>
            <label className="field">
              <span>Exclude folders</span>
              <input
                value={s.excludeFolders}
                placeholder="bin, obj, target"
                spellCheck={false}
                onChange={(e) => set("excludeFolders", e.target.value)}
              />
            </label>
            <label className="field check">
              <input type="checkbox" checked={s.includeHidden} onChange={(e) => set("includeHidden", e.target.checked)} />
              Include hidden files
            </label>
          </fieldset>

          <fieldset>
            <legend>Size &amp; date</legend>
            <div className="field-row">
              <label className="field">
                <span>Min size (KB)</span>
                <input
                  type="number"
                  min={0}
                  value={s.minSizeKb ?? ""}
                  onChange={(e) => set("minSizeKb", num(e.target.value))}
                />
              </label>
              <label className="field">
                <span>Max size (KB)</span>
                <input
                  type="number"
                  min={0}
                  value={s.maxSizeKb ?? ""}
                  onChange={(e) => set("maxSizeKb", num(e.target.value))}
                />
              </label>
            </div>
            <div className="field-row">
              <label className="field">
                <span>Modified after</span>
                <input type="date" value={s.modifiedAfter} onChange={(e) => set("modifiedAfter", e.target.value)} />
              </label>
              <label className="field">
                <span>Modified before</span>
                <input type="date" value={s.modifiedBefore} onChange={(e) => set("modifiedBefore", e.target.value)} />
              </label>
            </div>
            <small className="muted">Size and date filters apply to files only.</small>
          </fieldset>
        </div>
      )}

      {page === "rules" && (
        <div className="settings-grid single">
          <small className="muted">
            Used by rules-based content comparison (and as defaults when opening files from this folder session).
          </small>
          <div className="field-row">
            <label className="field">
              <span>Whitespace</span>
              <select
                value={s.rules.whitespace}
                onChange={(e) => setRules({ whitespace: e.target.value as WhitespaceMode })}
              >
                <option value="compare">Compare</option>
                <option value="ignoreTrailing">Ignore trailing</option>
                <option value="ignoreChanges">Ignore changes</option>
                <option value="ignoreAll">Ignore all</option>
              </select>
            </label>
            <label className="field check">
              <input
                type="checkbox"
                checked={s.rules.ignoreCase}
                onChange={(e) => setRules({ ignoreCase: e.target.checked })}
              />
              Ignore case
            </label>
          </div>
          <label className="field">
            <span>Ignore text matching (one regex per line)</span>
            <textarea
              rows={5}
              spellCheck={false}
              placeholder={"\\$Id:[^$]*\\$\n^\\s*//.*$\n\\d{4}-\\d{2}-\\d{2}T[\\d:.]+Z?"}
              value={s.rules.ignorePatterns.join("\n")}
              onChange={(e) => setRules({ ignorePatterns: e.target.value.split("\n") })}
            />
            <small className="muted">
              Matching text is removed before lines are compared, so lines that differ only there are unimportant.
            </small>
          </label>
        </div>
      )}
    </Modal>
  );
}
