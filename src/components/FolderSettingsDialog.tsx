import { useState } from "react";
import { ATTR, type ContentMethod, type LargeFileMethod, type WhitespaceMode } from "../lib/api";
import { COMPARE_PRESETS, NAMED_PRESETS, parseAlignRules, presetOf, type CompareSettings } from "../lib/folderSettings";
import { Modal } from "./Modal";

interface Props {
  value: CompareSettings;
  onApply: (s: CompareSettings) => void;
  onClose: () => void;
}

type Page = "comparison" | "alignment" | "filters" | "rules";

/** Attribute exclusions offered on the Filters page (hidden has its own switch). */
const ATTR_FILTERS: { bit: number; label: string }[] = [
  { bit: ATTR.readonly, label: "Read-only" },
  { bit: ATTR.system, label: "System" },
  { bit: ATTR.archive, label: "Archive" },
];

const CONTENT_METHODS: { value: ContentMethod; label: string; help: string }[] = [
  { value: "off", label: "Off", help: "Only the quick tests decide." },
  {
    value: "binary",
    label: "Binary (byte-by-byte)",
    help: "Byte-for-byte; stops at the first difference. Fastest content check.",
  },
  { value: "crc", label: "CRC-32", help: "Reads both files fully and shows their checksums." },
  {
    value: "rules",
    label: "Rules-based (thorough)",
    help: "Compares text with the rules tab: encoding, line endings, whitespace, case and ignored patterns can be unimportant (shown as ≈ similar). Binary files fall back to binary.",
  },
  {
    value: "version",
    label: "Exe/dll version",
    help: "Executables and libraries with the same version resource are the same even if their bytes differ. Other files fall back to binary.",
  },
];

const LARGE_FILE_METHODS: { value: LargeFileMethod; label: string }[] = [
  { value: "binary", label: "Binary (byte-by-byte)" },
  { value: "size", label: "Size only" },
  { value: "sizeTimestamp", label: "Size and timestamp" },
];

/** Beyond Compare-style session settings: comparison, filters, text rules. */
export function FolderSettingsDialog({ value, onApply, onClose }: Props) {
  const [s, setS] = useState<CompareSettings>(value);
  const [page, setPage] = useState<Page>("comparison");
  const set = <K extends keyof CompareSettings>(k: K, v: CompareSettings[K]) => setS((p) => ({ ...p, [k]: v }));
  const setRules = (patch: Partial<CompareSettings["rules"]>) => setS((p) => ({ ...p, rules: { ...p.rules, ...patch } }));
  const num = (v: string) => (v.trim() === "" ? null : Math.max(0, Number(v)));
  const toggleAttr = (bit: number, on: boolean) =>
    setS((p) => ({ ...p, excludeAttributes: on ? p.excludeAttributes | bit : p.excludeAttributes & ~bit }));
  const badRules = parseAlignRules(s.alignRules).invalid;
  const preset = presetOf(s);

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
            ["alignment", "Alignment"],
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
          <fieldset className="fs-presets">
            <legend>Comparison type</legend>
            <div className="seg-control">
              {NAMED_PRESETS.map((p) => (
                <button
                  key={p.key}
                  className={preset === p.key ? "active" : ""}
                  title={p.help}
                  onClick={() => {
                    const { compareSize, compareTimestamp, content } = COMPARE_PRESETS[p.key];
                    setS((cur) => ({ ...cur, compareSize, compareTimestamp, content }));
                  }}
                >
                  {p.name}
                </button>
              ))}
            </div>
            <small className="muted">
              {NAMED_PRESETS.find((p) => p.key === preset)?.help ??
                "Custom: the quick tests and content method below don’t match a named type."}
            </small>
          </fieldset>

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
            <label className="field check">
              <input
                type="checkbox"
                checked={s.compareAttributes}
                onChange={(e) => set("compareAttributes", e.target.checked)}
              />
              Compare attributes
            </label>
            <small className="muted">
              Read-only, hidden and system attributes (Unix: permission bits) must match too. Archive is ignored.
            </small>
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
            <div className="field-row fs-large">
              <label className="field check">
                <input
                  type="checkbox"
                  checked={s.largeFileThresholdMb != null}
                  disabled={s.content === "off"}
                  onChange={(e) => set("largeFileThresholdMb", e.target.checked ? 100 : null)}
                />
                For files larger than
              </label>
              <input
                type="number"
                min={0}
                step="any"
                aria-label="Size threshold in MB"
                value={s.largeFileThresholdMb ?? ""}
                disabled={s.content === "off" || s.largeFileThresholdMb == null}
                onChange={(e) => set("largeFileThresholdMb", num(e.target.value) ?? 0)}
              />
              <span>MB use</span>
              <select
                value={s.largeFileMethod}
                disabled={s.content === "off" || s.largeFileThresholdMb == null}
                onChange={(e) => set("largeFileMethod", e.target.value as LargeFileMethod)}
              >
                {LARGE_FILE_METHODS.map((m) => (
                  <option key={m.value} value={m.value}>
                    {m.label}
                  </option>
                ))}
              </select>
            </div>
            <small className="muted">
              Keeps a thorough comparison fast on big files: above the size, the pair is compared byte-by-byte (or by
              metadata alone) instead of being decoded as text.
            </small>
          </fieldset>
        </div>
      )}

      {page === "alignment" && (
        <div className="settings-grid">
          <fieldset>
            <legend>Names</legend>
            <label className="field check">
              <input
                type="checkbox"
                checked={s.caseSensitiveNames}
                onChange={(e) => set("caseSensitiveNames", e.target.checked)}
              />
              Case-sensitive filenames
            </label>
            <small className="muted">When off, Readme.md lines up with README.md (the Windows default).</small>
            <label className="field check">
              <input type="checkbox" checked={s.alignUnicode} onChange={(e) => set("alignUnicode", e.target.checked)} />
              Align different Unicode forms
            </label>
            <small className="muted">
              Pairs composed (NFC) and decomposed (NFD) spellings of a name — macOS stores the latter.
            </small>
            <label className="field check">
              <input type="checkbox" checked={s.followLinks} onChange={(e) => set("followLinks", e.target.checked)} />
              Follow symbolic links and junctions
            </label>
            <small className="muted">
              When off, links are listed as links (marked with a link icon) and compared by where they point.
            </small>
            {/* bc5:archives */}
            <label className="field check">
              <input
                type="checkbox"
                checked={s.expandArchives}
                onChange={(e) => set("expandArchives", e.target.checked)}
              />
              Expand archives as folders
            </label>
            <small className="muted">
              Zip, jar, tar(.gz/.bz2/.xz), gz and 7z files in the trees open like folders and their contents are
              compared. When off (the default), archives are compared as files.
            </small>
          </fieldset>

          <fieldset>
            <legend>Alignment overrides</legend>
            <label className="field check">
              <input
                type="checkbox"
                checked={s.ignoreExtensions}
                onChange={(e) => set("ignoreExtensions", e.target.checked)}
              />
              Ignore file extensions (report.txt ↔ report.md)
            </label>
            <label className="field">
              <span>Equivalent extensions (one group per line)</span>
              <textarea
                rows={3}
                spellCheck={false}
                disabled={s.ignoreExtensions}
                placeholder={"htm, html\njpg, jpeg"}
                value={s.extensionGroups}
                onChange={(e) => set("extensionGroups", e.target.value)}
              />
            </label>
            <label className="field">
              <span>Align names (one rule per line: left =&gt; right)</span>
              <textarea
                rows={3}
                spellCheck={false}
                placeholder={"Apples* => Bananas*\nreport-?.csv => summary-?.csv"}
                value={s.alignRules}
                onChange={(e) => set("alignRules", e.target.value)}
              />
              {badRules.length > 0 ? (
                <small className="fc-error-text">Not a rule (needs “=&gt;”): {badRules.join(" · ")}</small>
              ) : (
                <small className="muted">* matches any text, ? one character; the matched parts must be equal.</small>
              )}
            </label>
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

          <fieldset>
            <legend>Contents</legend>
            <label className="field">
              <span>Only files containing</span>
              <input
                value={s.includeContaining}
                placeholder="(any contents)"
                spellCheck={false}
                onChange={(e) => set("includeContaining", e.target.value)}
              />
            </label>
            <label className="field">
              <span>Exclude files containing</span>
              <input
                value={s.excludeContaining}
                placeholder="DO NOT EDIT"
                spellCheck={false}
                onChange={(e) => set("excludeContaining", e.target.value)}
              />
            </label>
            <div className="field-row">
              <label className="field check">
                <input
                  type="checkbox"
                  checked={s.containingRegex}
                  onChange={(e) => set("containingRegex", e.target.checked)}
                />
                Regular expression
              </label>
              <label className="field check">
                <input
                  type="checkbox"
                  checked={s.containingMatchCase}
                  onChange={(e) => set("containingMatchCase", e.target.checked)}
                />
                Match case
              </label>
            </div>
            <small className="muted">
              Reads the first 4 MB of each text file; binary files never match. A pair stays if either side passes.
            </small>
          </fieldset>

          <fieldset>
            <legend>Attributes</legend>
            <small className="muted">Exclude entries that are…</small>
            {ATTR_FILTERS.map((a) => (
              <label key={a.bit} className="field check">
                <input
                  type="checkbox"
                  checked={(s.excludeAttributes & a.bit) !== 0}
                  onChange={(e) => toggleAttr(a.bit, e.target.checked)}
                />
                {a.label}
              </label>
            ))}
            <label className="field check">
              <input
                type="checkbox"
                checked={s.excludeExecutable}
                onChange={(e) => set("excludeExecutable", e.target.checked)}
              />
              Executable (Unix)
            </label>
            <small className="muted">Hidden files: see “Include hidden files”. Remote folders have no attributes.</small>
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
