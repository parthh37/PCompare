import { ArrowDown, ArrowUp, Plus, RotateCcw, X } from "lucide-react";
import { useEffect, useState, type ReactNode } from "react";
import { errorMessage } from "../lib/api";
import {
  CONVERTERS,
  convertApi,
  emptyFormat,
  emptyTool,
  type ConvertConfig,
  type Converter,
  type ExternalTool,
  type FileFormat,
} from "../lib/convertApi";
import { textApi, type GrammarInfo } from "../lib/textApi";
import { Modal } from "./Modal";
import "./FileFormatsDialog.css";

export type FileFormatsPage = "formats" | "tools";

/** Converters whose text can never be saved back (see `Converter::is_lossy`). */
const LOSSY: Converter[] = ["word", "pdf", "rtf", "odt", "dfm"];

interface Props {
  initialPage?: FileFormatsPage;
  onClose: () => void;
  /** Called after the rules were saved (e.g. to reload an open compare). */
  onSaved?: (config: ConvertConfig) => void;
}

/** Moves item `i` of `list` by `delta` places. */
function move<T>(list: T[], i: number, delta: number): T[] {
  const j = i + delta;
  if (j < 0 || j >= list.length) return list;
  const out = [...list];
  [out[i], out[j]] = [out[j], out[i]];
  return out;
}

/**
 * The "File formats" and "External tools" settings: which files
 * are converted to text (built-in converter or an external command) before a
 * text compare, made read-only or given a grammar; and which pairs open in a
 * third-party comparison tool. Saved to the app config folder.
 */
export function FileFormatsDialog({ initialPage = "formats", onClose, onSaved }: Props) {
  const [page, setPage] = useState<FileFormatsPage>(initialPage);
  const [config, setConfig] = useState<ConvertConfig | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  const [sel, setSel] = useState<Record<FileFormatsPage, number>>({ formats: 0, tools: 0 });
  const [grammars, setGrammars] = useState<GrammarInfo[]>([]);

  useEffect(() => {
    convertApi
      .loadConfig()
      .then(setConfig)
      .catch((e) => {
        // A damaged rules file: start from the defaults, saying why.
        setError(`${errorMessage(e)} — showing the default rules`);
        convertApi.defaults().then(setConfig).catch(() => {});
      });
    textApi.grammars().then(setGrammars).catch(() => {});
  }, []);

  const save = async () => {
    if (!config) return;
    const bad = config.formats.find((f) => !f.mask.trim()) ?? config.externalTools.find((t) => !t.mask.trim());
    if (bad) return setError(`"${bad.name}" needs a file mask`);
    const noCmd = config.externalTools.find((t) => !t.command.trim());
    if (noCmd) return setError(`"${noCmd.name}" needs a command line`);
    setSaving(true);
    try {
      await convertApi.saveConfig(config);
      onSaved?.(config);
      onClose();
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setSaving(false);
    }
  };

  const resetDefaults = () =>
    convertApi
      .defaults()
      .then((d) => {
        // Only the current page is reset.
        setConfig((c) => (c ? (page === "formats" ? { ...c, formats: d.formats } : { ...c, externalTools: d.externalTools }) : d));
        setSel((s) => ({ ...s, [page]: 0 }));
      })
      .catch((e) => setError(errorMessage(e)));

  return (
    <Modal
      title="File formats and external tools"
      wide
      onClose={onClose}
      footer={
        <>
          {error && <span className="ffd-error">{error}</span>}
          <button onClick={resetDefaults} title="Restore this page's stock rules">
            <RotateCcw size={13} /> Defaults
          </button>
          <button onClick={onClose}>Cancel</button>
          <button className="primary" disabled={!config || saving} onClick={save}>
            Save
          </button>
        </>
      }
    >
      <div className="seg-control settings-pages">
        <button className={page === "formats" ? "active" : ""} onClick={() => setPage("formats")}>
          File formats
        </button>
        <button className={page === "tools" ? "active" : ""} onClick={() => setPage("tools")}>
          External tools
        </button>
      </div>
      {!config ? (
        <div className="muted">Loading…</div>
      ) : page === "formats" ? (
        <RuleEditor
          items={config.formats}
          selected={sel.formats}
          onSelect={(i) => setSel((s) => ({ ...s, formats: i }))}
          onChange={(formats) => setConfig({ ...config, formats })}
          create={emptyFormat}
          help="Text Compare applies the first enabled rule whose mask matches a file — each side separately, so a Word document compares against a text file."
          detail={(f, set) => <FormatFields f={f} set={set} grammars={grammars} />}
        />
      ) : (
        <RuleEditor
          items={config.externalTools}
          selected={sel.tools}
          onSelect={(i) => setSel((s) => ({ ...s, tools: i }))}
          onChange={(externalTools) => setConfig({ ...config, externalTools })}
          create={emptyTool}
          help="Pairs whose files both match a tool's mask can be opened in it from Folder Compare's context menu (Open with …). Tools set to “always” open such pairs directly, instead of a built-in viewer."
          detail={(t, set) => <ToolFields t={t} set={set} />}
        />
      )}
    </Modal>
  );
}

interface Rule {
  name: string;
  mask: string;
  enabled: boolean;
}

/** An ordered rule list (enable, reorder, add, remove) beside the selected rule's fields. */
function RuleEditor<T extends Rule>({
  items,
  selected,
  onSelect,
  onChange,
  create,
  help,
  detail,
}: {
  items: T[];
  selected: number;
  onSelect: (i: number) => void;
  onChange: (items: T[]) => void;
  create: () => T;
  help: string;
  detail: (item: T, set: (patch: Partial<T>) => void) => ReactNode;
}) {
  const cur = Math.min(selected, items.length - 1);
  const item = cur >= 0 ? items[cur] : null;
  const update = (i: number, patch: Partial<T>) => onChange(items.map((it, j) => (j === i ? { ...it, ...patch } : it)));
  return (
    <div className="ffd-layout">
      <div className="ffd-side">
        <ul className="ffd-list" role="listbox">
          {items.map((it, i) => (
            <li
              key={i}
              role="option"
              aria-selected={i === cur}
              className={i === cur ? "selected" : ""}
              onClick={() => onSelect(i)}
            >
              <input
                type="checkbox"
                checked={it.enabled}
                title="Enabled"
                onClick={(e) => e.stopPropagation()}
                onChange={(e) => update(i, { enabled: e.target.checked } as Partial<T>)}
              />
              <span className={it.enabled ? "" : "muted"}>
                {it.name || "(unnamed)"}
                <small className="muted">{it.mask}</small>
              </span>
            </li>
          ))}
          {items.length === 0 && <li className="muted ffd-empty">No rules</li>}
        </ul>
        <div className="ffd-list-buttons">
          <button
            className="icon-btn"
            title="Add a rule"
            onClick={() => {
              onChange([...items, create()]);
              onSelect(items.length);
            }}
          >
            <Plus size={14} />
          </button>
          <button
            className="icon-btn"
            title="Remove the selected rule"
            disabled={!item}
            onClick={() => {
              onChange(items.filter((_, j) => j !== cur));
              onSelect(Math.max(0, cur - 1));
            }}
          >
            <X size={14} />
          </button>
          <button
            className="icon-btn"
            title="Move up (earlier rules win)"
            disabled={cur <= 0}
            onClick={() => {
              onChange(move(items, cur, -1));
              onSelect(cur - 1);
            }}
          >
            <ArrowUp size={14} />
          </button>
          <button
            className="icon-btn"
            title="Move down"
            disabled={cur < 0 || cur >= items.length - 1}
            onClick={() => {
              onChange(move(items, cur, 1));
              onSelect(cur + 1);
            }}
          >
            <ArrowDown size={14} />
          </button>
        </div>
      </div>
      <div className="ffd-detail">
        {item ? (
          <>
            <label className="field">
              <span>Name</span>
              <input value={item.name} onChange={(e) => update(cur, { name: e.target.value } as Partial<T>)} />
            </label>
            <label className="field">
              <span>File masks</span>
              <input
                value={item.mask}
                spellCheck={false}
                placeholder="*.json;*.geojson"
                onChange={(e) => update(cur, { mask: e.target.value } as Partial<T>)}
              />
              <small className="muted">Separate masks with “;”. A mask containing “/” matches the whole path.</small>
            </label>
            {detail(item, (patch) => update(cur, patch))}
          </>
        ) : (
          <p className="muted">Add a rule with +.</p>
        )}
        <p className="muted ffd-help">{help}</p>
      </div>
    </div>
  );
}

function FormatFields({
  f,
  set,
  grammars,
}: {
  f: FileFormat;
  set: (patch: Partial<FileFormat>) => void;
  grammars: GrammarInfo[];
}) {
  // Mirrors `Converter::is_lossy` in pcompare-convert.
  const documentText = !f.command.trim() && LOSSY.includes(f.converter);
  return (
    <>
      <label className="field">
        <span>Conversion</span>
        <select value={f.converter} disabled={!!f.command.trim()} onChange={(e) => set({ converter: e.target.value as Converter })}>
          {CONVERTERS.map((c) => (
            <option key={c.id} value={c.id}>
              {c.label}
            </option>
          ))}
        </select>
      </label>
      <label className="field">
        <span>External conversion command</span>
        <input
          className="ffd-mono"
          value={f.command}
          spellCheck={false}
          placeholder='e.g. "C:\Tools\conv.exe" %s %t'
          onChange={(e) => set({ command: e.target.value })}
        />
        <small className="muted">
          Optional; replaces the built-in conversion. %s is the original file, %t the text file the command writes —
          without %t its standard output is used. Runs directly, not through a shell.
        </small>
      </label>
      <label className="field">
        <span>Grammar</span>
        <select value={f.grammar} onChange={(e) => set({ grammar: e.target.value })}>
          <option value="">By file extension</option>
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
          checked={f.readOnly || documentText}
          disabled={documentText}
          onChange={(e) => set({ readOnly: e.target.checked })}
        />
        Read-only
      </label>
      <small className="muted ffd-note">
        {documentText
          ? "Text extracted from a document is always read-only: saving it would overwrite the document."
          : f.converter === "none" && !f.command.trim()
            ? "Without a conversion, read-only protects the file itself."
            : "Converted text is read-only unless unchecked; saving then writes the converted text over the original file."}
      </small>
    </>
  );
}

function ToolFields({ t, set }: { t: ExternalTool; set: (patch: Partial<ExternalTool>) => void }) {
  return (
    <>
      <label className="field">
        <span>Command line</span>
        <input
          className="ffd-mono"
          value={t.command}
          spellCheck={false}
          placeholder='"C:\Tools\Diff.exe" %1 %2'
          onChange={(e) => set({ command: e.target.value })}
        />
        <small className="muted">
          %1 is the left file and %2 the right; without them both paths are appended. Quote paths containing spaces.
          Remote files are downloaded first.
        </small>
      </label>
      <label className="field check">
        <input type="checkbox" checked={t.always} onChange={(e) => set({ always: e.target.checked })} />
        Always use this tool for matching pairs
      </label>
    </>
  );
}
