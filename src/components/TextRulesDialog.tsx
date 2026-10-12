import { open } from "@tauri-apps/plugin-dialog";
import { FolderOpen, Plus, X } from "lucide-react";
import { useEffect, useState } from "react";
import type { Alignment, LinePair, LineRange, Replacement, TextDiffOptions, WhitespaceMode } from "../lib/api";
import {
  addRange,
  formatColumns,
  loadBackups,
  parseColumns,
  saveBackups,
  textApi,
  type BackupOptions,
  type GrammarInfo,
} from "../lib/textApi";
import { Modal } from "./Modal";
import "./TextRulesDialog.css";

export type RulesPage = "importance" | "replacements" | "alignment" | "unimportant" | "backups";

interface Props {
  options: TextDiffOptions;
  /** Grammar ids the last diff picked per side (shown next to "Detect"). */
  detected?: { left: string | null; right: string | null };
  /** Line counts, to bound the line-number inputs. */
  lineCounts?: { left: number; right: number };
  initialPage?: RulesPage;
  onApply: (o: TextDiffOptions) => void;
  onClose: () => void;
  /** bc5:converters — opens the File formats settings (converters, read-only, grammar per file type). */
  onOpenFileFormats?: () => void;
}

const PAGES: [RulesPage, string][] = [
  ["importance", "Importance"],
  ["replacements", "Replacements"],
  ["alignment", "Alignment"],
  ["unimportant", "Unimportant lines"],
  ["backups", "Backups"],
];

const ALIGNMENTS: { value: Alignment; label: string; help: string }[] = [
  {
    value: "similar",
    label: "Normal (similar lines)",
    help: "Inside a changed block, the most similar lines are paired; lines with no similar partner stand alone.",
  },
  { value: "positional", label: "By position", help: "Changed lines are paired in order, top to bottom." },
  {
    value: "unaligned",
    label: "Unaligned",
    help: "Changed blocks are shown as a whole, without pairing lines or highlighting within them.",
  },
];

const lines = (text: string) => text.split("\n").map((p) => p.trim()).filter(Boolean);
const toInt = (v: string) => Math.max(0, Math.floor(Number(v) || 0));

/**
 * Text Compare rules: what is unimportant (grammar comments and strings, regexes,
 * columns, page headers, line endings, replaced text, marked lines), how lines are
 * aligned (style plus manual anchors/breaks), and save backups.
 */
export function TextRulesDialog({
  options,
  detected,
  lineCounts,
  initialPage,
  onApply,
  onClose,
  onOpenFileFormats, // bc5:converters
}: Props) {
  const [o, setO] = useState<TextDiffOptions>(options);
  const [page, setPage] = useState<RulesPage>(initialPage ?? "importance");
  const [patterns, setPatterns] = useState(options.ignorePatterns.join("\n"));
  const [headers, setHeaders] = useState(options.pageHeaderPatterns.join("\n"));
  const [columns, setColumns] = useState(formatColumns(options.ignoreColumns));
  const [backups, setBackups] = useState<BackupOptions>(loadBackups);
  const [grammars, setGrammars] = useState<GrammarInfo[]>([]);
  const [error, setError] = useState<string | null>(null);
  const set = <K extends keyof TextDiffOptions>(k: K, v: TextDiffOptions[K]) => setO((p) => ({ ...p, [k]: v }));

  useEffect(() => {
    textApi.grammars().then(setGrammars, () => setGrammars([]));
  }, []);

  const grammarName = (id: string | null | undefined) => (id && grammars.find((g) => g.id === id)?.name) || id;
  const detectedLabel = (() => {
    if (!detected) return "";
    const [l, r] = [grammarName(detected.left), grammarName(detected.right)];
    if (!l && !r) return " (none found)";
    return l === r ? ` (${l})` : ` (${l ?? "none"} / ${r ?? "none"})`;
  })();

  const apply = () => {
    let ignoreColumns;
    try {
      ignoreColumns = parseColumns(columns);
    } catch (e) {
      setPage("importance");
      setError(e instanceof Error ? e.message : String(e));
      return;
    }
    if (backups.mode === "folder" && !backups.folder.trim()) {
      setPage("backups");
      setError("Choose a backup folder, or pick another backup mode.");
      return;
    }
    saveBackups(backups);
    onApply({
      ...o,
      ignorePatterns: lines(patterns),
      pageHeaderPatterns: lines(headers),
      ignoreColumns,
      replacements: o.replacements.filter((r) => r.left !== ""),
    });
    onClose();
  };

  return (
    <Modal
      title="Text compare rules"
      wide
      onClose={onClose}
      footer={
        <>
          {error && <span className="trd-error">{error}</span>}
          {/* bc5:converters */}
          {onOpenFileFormats && (
            <button className="trd-formats" onClick={onOpenFileFormats} title="Per file type: convert to text, read-only, grammar">
              File formats…
            </button>
          )}
          <button onClick={onClose}>Cancel</button>
          <button className="primary" onClick={apply}>
            Apply
          </button>
        </>
      }
    >
      <div className="seg-control settings-pages">
        {PAGES.map(([p, label]) => (
          <button key={p} className={page === p ? "active" : ""} onClick={() => setPage(p)}>
            {label}
          </button>
        ))}
      </div>

      {page === "importance" && (
        <div className="settings-grid">
          <fieldset>
            <legend>Language grammar</legend>
            <label className="field">
              <span>Grammar</span>
              <select value={o.grammar ?? ""} onChange={(e) => set("grammar", e.target.value || null)}>
                <option value="">Detect from file extension{detectedLabel}</option>
                <option value="text">Plain text (no grammar)</option>
                {grammars.map((g) => (
                  <option key={g.id} value={g.id}>
                    {g.name}
                  </option>
                ))}
              </select>
            </label>
            <label className="field check">
              <input
                type="checkbox"
                checked={o.ignoreComments}
                onChange={(e) => set("ignoreComments", e.target.checked)}
              />
              Comments are unimportant
            </label>
            <label className="field check">
              <input
                type="checkbox"
                checked={o.ignoreStrings}
                onChange={(e) => set("ignoreStrings", e.target.checked)}
              />
              String contents are unimportant
            </label>
            <small className="muted">
              Differences only inside comments or string literals are shown as unimportant. A line holding nothing but a
              comment is unimportant even when the other side has no such line.
            </small>
          </fieldset>

          <fieldset>
            <legend>Text</legend>
            <label className="field">
              <span>Whitespace</span>
              <select value={o.whitespace} onChange={(e) => set("whitespace", e.target.value as WhitespaceMode)}>
                <option value="compare">Compare</option>
                <option value="ignoreTrailing">Ignore trailing</option>
                <option value="ignoreChanges">Ignore changes</option>
                <option value="ignoreAll">Ignore all</option>
              </select>
            </label>
            <label className="field check">
              <input type="checkbox" checked={o.ignoreCase} onChange={(e) => set("ignoreCase", e.target.checked)} />
              Ignore case
            </label>
            <label className="field check">
              <input
                type="checkbox"
                checked={o.compareLineEndings}
                onChange={(e) => set("compareLineEndings", e.target.checked)}
              />
              Line endings (CRLF / LF / CR) are differences
            </label>
            <label className="field">
              <span>Ignored columns</span>
              <input
                value={columns}
                spellCheck={false}
                placeholder="e.g. 1-6, 73-80 (COBOL sequence numbers) or 81-"
                onChange={(e) => {
                  setColumns(e.target.value);
                  setError(null);
                }}
              />
              <small className="muted">Character columns, counted from 1; "73-" runs to the end of the line.</small>
            </label>
          </fieldset>

          <fieldset>
            <legend>Ignored text</legend>
            <label className="field">
              <span>Text matching (one regex per line)</span>
              <textarea
                rows={4}
                spellCheck={false}
                value={patterns}
                placeholder={"\\$Id:[^$]*\\$\n\\d{4}-\\d\\d-\\d\\d \\d\\d:\\d\\d"}
                onChange={(e) => setPatterns(e.target.value)}
              />
              <small className="muted">Matches are removed before lines are compared.</small>
            </label>
          </fieldset>

          <fieldset>
            <legend>Page headers and footers</legend>
            <label className="field">
              <span>Header / footer lines matching (one regex per line)</span>
              <textarea
                rows={3}
                spellCheck={false}
                value={headers}
                placeholder={"^Page \\d+ of \\d+$\n^Printed: "}
                onChange={(e) => setHeaders(e.target.value)}
              />
            </label>
            <div className="field-row">
              <label className="field">
                <span>Header lines per page</span>
                <input
                  type="number"
                  min={0}
                  value={o.pageHeaderLines}
                  onChange={(e) => set("pageHeaderLines", toInt(e.target.value))}
                />
              </label>
              <label className="field">
                <span>Footer lines per page</span>
                <input
                  type="number"
                  min={0}
                  value={o.pageFooterLines}
                  onChange={(e) => set("pageFooterLines", toInt(e.target.value))}
                />
              </label>
            </div>
            <small className="muted">
              Pages break at form feeds. Header and footer lines are wholly unimportant, and so are form feeds while
              any of these is set.
            </small>
          </fieldset>
        </div>
      )}

      {page === "replacements" && (
        <ReplacementsPage value={o.replacements} onChange={(r) => set("replacements", r)} />
      )}

      {page === "alignment" && (
        <div className="settings-grid">
          <fieldset>
            <legend>Alignment of changed lines</legend>
            {ALIGNMENTS.map((a) => (
              <label key={a.value} className="field radio">
                <input
                  type="radio"
                  name="alignment"
                  checked={o.alignment === a.value}
                  onChange={() => set("alignment", a.value)}
                />
                <span>
                  <b>{a.label}</b>
                  <small className="muted">{a.help}</small>
                </span>
              </label>
            ))}
            <label className="field">
              <span>Matching algorithm</span>
              <select
                value={o.algorithm}
                onChange={(e) => set("algorithm", e.target.value as TextDiffOptions["algorithm"])}
              >
                <option value="patience">Patience (anchors on unique lines)</option>
                <option value="myers">Myers (minimal edit script)</option>
              </select>
            </label>
          </fieldset>
          <fieldset>
            <legend>Manual alignment</legend>
            <PairList
              label="Always align"
              help="Each left line is shown next to its right line; the text between anchors is compared separately."
              value={o.anchors}
              max={lineCounts}
              onChange={(v) => set("anchors", v)}
            />
            <PairList
              label="Never align"
              help="These lines are kept apart even when they match."
              value={o.breaks}
              max={lineCounts}
              onChange={(v) => set("breaks", v)}
            />
          </fieldset>
        </div>
      )}

      {page === "unimportant" && (
        <div className="settings-grid">
          {(["left", "right"] as const).map((side) => (
            <fieldset key={side}>
              <legend>{side === "left" ? "Left" : "Right"} side</legend>
              <RangeList
                value={o.unimportantLines[side]}
                max={lineCounts?.[side]}
                onChange={(v) => set("unimportantLines", { ...o.unimportantLines, [side]: v })}
              />
            </fieldset>
          ))}
          <small className="muted trd-span">
            Marked lines are always unimportant. Use “Mark unimportant” on the toolbar to mark the current difference;
            marks are saved with the session.
          </small>
        </div>
      )}

      {page === "backups" && (
        <div className="settings-grid single">
          <fieldset>
            <legend>When saving a file</legend>
            {(
              [
                ["off", "No backup", "Files are overwritten in place."],
                ["bak", "Keep a .bak next to the file", "The previous version is copied to <name>.bak."],
                [
                  "folder",
                  "Keep backups in a folder",
                  "The previous version is copied to <folder>/<name>.<date-time>.bak, so older backups are kept.",
                ],
              ] as const
            ).map(([mode, label, help]) => (
              <label key={mode} className="field radio">
                <input
                  type="radio"
                  name="backups"
                  checked={backups.mode === mode}
                  onChange={() => {
                    setBackups({ ...backups, mode });
                    setError(null);
                  }}
                />
                <span>
                  <b>{label}</b>
                  <small className="muted">{help}</small>
                </span>
              </label>
            ))}
            <label className="field indent">
              <span>Backup folder</span>
              <div className="input-row">
                <input
                  value={backups.folder}
                  disabled={backups.mode !== "folder"}
                  spellCheck={false}
                  onChange={(e) => setBackups({ ...backups, folder: e.target.value })}
                />
                <button
                  className="icon-btn"
                  disabled={backups.mode !== "folder"}
                  title="Browse…"
                  onClick={async () => {
                    const dir = await open({ directory: true, defaultPath: backups.folder || undefined });
                    if (typeof dir === "string") setBackups({ ...backups, folder: dir });
                  }}
                >
                  <FolderOpen size={14} />
                </button>
              </div>
            </label>
            <label className="field check">
              <input
                type="checkbox"
                checked={backups.everySave}
                disabled={backups.mode === "off"}
                onChange={(e) => setBackups({ ...backups, everySave: e.target.checked })}
              />
              Back up on every save
            </label>
            <small className="muted">
              Off: only the first save of a file while PCompare is running makes a backup, so it keeps the version from
              before you started editing. Applies to every text session.
            </small>
          </fieldset>
        </div>
      )}
    </Modal>
  );
}

function ReplacementsPage({ value, onChange }: { value: Replacement[]; onChange: (r: Replacement[]) => void }) {
  const update = (i: number, patch: Partial<Replacement>) =>
    onChange(value.map((r, j) => (j === i ? { ...r, ...patch } : r)));
  return (
    <div className="settings-grid single">
      <fieldset>
        <legend>Ignore replaced text</legend>
        <small className="muted">
          Text on the left that was consistently replaced on the right (a renamed identifier, a new prefix). Lines that
          differ only by these replacements are unimportant.
        </small>
        <table className="trd-table">
          <thead>
            <tr>
              <th>Left text</th>
              <th />
              <th>Right text</th>
              <th>Regex</th>
              <th />
            </tr>
          </thead>
          <tbody>
            {value.map((r, i) => (
              <tr key={i}>
                <td>
                  <input
                    value={r.left}
                    spellCheck={false}
                    placeholder={r.regex ? "old_(\\w+)" : "oldName"}
                    onChange={(e) => update(i, { left: e.target.value })}
                  />
                </td>
                <td className="trd-arrow">→</td>
                <td>
                  <input
                    value={r.right}
                    spellCheck={false}
                    placeholder={r.regex ? "new_$1" : "newName"}
                    onChange={(e) => update(i, { right: e.target.value })}
                  />
                </td>
                <td className="trd-center">
                  <input type="checkbox" checked={r.regex} onChange={(e) => update(i, { regex: e.target.checked })} />
                </td>
                <td>
                  <button className="icon-btn" title="Remove" onClick={() => onChange(value.filter((_, j) => j !== i))}>
                    <X size={13} />
                  </button>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
        <button onClick={() => onChange([...value, { left: "", right: "", regex: false }])}>
          <Plus size={14} /> Add replacement
        </button>
      </fieldset>
    </div>
  );
}

function PairList({
  label,
  help,
  value,
  max,
  onChange,
}: {
  label: string;
  help: string;
  value: LinePair[];
  max?: { left: number; right: number };
  onChange: (v: LinePair[]) => void;
}) {
  const [left, setLeft] = useState("");
  const [right, setRight] = useState("");
  const l = toInt(left);
  const r = toInt(right);
  const valid = l >= 1 && r >= 1 && (!max || (l <= max.left && r <= max.right));
  return (
    <div className="field">
      <span>{label}</span>
      <ul className="trd-list">
        {value.map((p, i) => (
          <li key={`${p.left}:${p.right}`}>
            Left line {p.left} ↔ right line {p.right}
            <button className="icon-btn" title="Remove" onClick={() => onChange(value.filter((_, j) => j !== i))}>
              <X size={13} />
            </button>
          </li>
        ))}
        {!value.length && <li className="muted">None</li>}
      </ul>
      <div className="input-row trd-add">
        <input type="number" min={1} max={max?.left} placeholder="Left line" value={left} onChange={(e) => setLeft(e.target.value)} />
        <span>↔</span>
        <input
          type="number"
          min={1}
          max={max?.right}
          placeholder="Right line"
          value={right}
          onChange={(e) => setRight(e.target.value)}
        />
        <button
          disabled={!valid}
          onClick={() => {
            const next = value.filter((p) => p.left !== l && p.right !== r);
            onChange([...next, { left: l, right: r }].sort((a, b) => a.left - b.left));
            setLeft("");
            setRight("");
          }}
        >
          <Plus size={14} /> Add
        </button>
      </div>
      <small className="muted">{help}</small>
    </div>
  );
}

function RangeList({
  value,
  max,
  onChange,
}: {
  value: LineRange[];
  max?: number;
  onChange: (v: LineRange[]) => void;
}) {
  const [from, setFrom] = useState("");
  const [to, setTo] = useState("");
  const a = toInt(from);
  const b = to.trim() ? toInt(to) : a;
  const valid = a >= 1 && b >= a && (max === undefined || b <= max);
  return (
    <div className="field">
      <ul className="trd-list">
        {value.map((r, i) => (
          <li key={`${r.start}-${r.end}`}>
            {r.start === r.end ? `Line ${r.start}` : `Lines ${r.start}–${r.end}`}
            <button className="icon-btn" title="Remove" onClick={() => onChange(value.filter((_, j) => j !== i))}>
              <X size={13} />
            </button>
          </li>
        ))}
        {!value.length && <li className="muted">No lines marked</li>}
      </ul>
      <div className="input-row trd-add">
        <input type="number" min={1} max={max} placeholder="From line" value={from} onChange={(e) => setFrom(e.target.value)} />
        <span>–</span>
        <input type="number" min={1} max={max} placeholder="To line" value={to} onChange={(e) => setTo(e.target.value)} />
        <button
          disabled={!valid}
          onClick={() => {
            onChange(addRange(value, a, b));
            setFrom("");
            setTo("");
          }}
        >
          <Plus size={14} /> Add
        </button>
      </div>
      {value.length > 0 && (
        <button className="trd-clear" onClick={() => onChange([])}>
          Clear all
        </button>
      )}
    </div>
  );
}
