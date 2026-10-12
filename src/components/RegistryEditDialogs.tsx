// Dialogs for Registry Compare editing: value editor, new key, and the
// confirmation shown before changing a live registry.
import { TriangleAlert } from "lucide-react";
import { useState } from "react";
import type { RegSide } from "../lib/metaApi";
import { Modal } from "./Modal";

/** Types offered in the value editor (others stay selectable when a value already has one). */
const VALUE_TYPES = [
  "REG_SZ",
  "REG_EXPAND_SZ",
  "REG_MULTI_SZ",
  "REG_DWORD",
  "REG_QWORD",
  "REG_BINARY",
  "REG_NONE",
  "REG_DWORD_BIG_ENDIAN",
];

const STRING_TYPES = new Set(["REG_SZ", "REG_EXPAND_SZ", "REG_LINK"]);

function parseNumber(text: string, max: bigint): bigint | string {
  const t = text.trim();
  if (!/^(0x[0-9a-f]+|\d+)$/i.test(t)) return "Enter a decimal number or 0x-prefixed hex";
  const v = BigInt(t);
  return v > max ? `Too large (maximum ${max})` : v;
}

const HEX_TOKEN = /^([0-9a-f]{1,2}|([0-9a-f]{2})+)$/i;

/** Mirrors the engine's `parse_value_input`; returns an error message or null. */
export function validateValue(type: string, text: string): string | null {
  if (STRING_TYPES.has(type) || type === "REG_MULTI_SZ") return null;
  if (type === "REG_DWORD" || type === "REG_DWORD_BIG_ENDIAN") {
    const v = parseNumber(text, 0xffffffffn);
    return typeof v === "string" ? v : null;
  }
  if (type === "REG_QWORD") {
    const v = parseNumber(text, 0xffffffffffffffffn);
    return typeof v === "string" ? v : null;
  }
  const bad = text.split(/[\s,]+/).find((t) => t && !HEX_TOKEN.test(t));
  return bad ? `"${bad}" is not hex bytes (e.g. 01 ab ff)` : null;
}

function numberHint(type: string, text: string): string | null {
  if (type !== "REG_DWORD" && type !== "REG_DWORD_BIG_ENDIAN" && type !== "REG_QWORD") return null;
  const v = parseNumber(text, type === "REG_QWORD" ? 0xffffffffffffffffn : 0xffffffffn);
  if (typeof v === "string") return null;
  return `= ${v.toString()} (0x${v.toString(16).padStart(type === "REG_QWORD" ? 16 : 8, "0")})`;
}

export interface ValueDraft {
  name: string;
  type: string;
  text: string;
}

/** Adds or edits one value on one side. */
export function RegValueDialog({
  side,
  keyPath,
  initial,
  adding,
  existing,
  onSubmit,
  onClose,
}: {
  side: RegSide;
  keyPath: string;
  initial: ValueDraft;
  adding: boolean;
  /** Lower-case names of the key's values on this side, to warn about overwrites. */
  existing: Set<string>;
  onSubmit: (draft: ValueDraft) => void;
  onClose: () => void;
}) {
  const [name, setName] = useState(initial.name);
  const [type, setType] = useState(initial.type || "REG_SZ");
  const [text, setText] = useState(initial.text);
  const types = VALUE_TYPES.includes(type) ? VALUE_TYPES : [...VALUE_TYPES, type];
  const error = validateValue(type, text);
  const renamed = !adding && name.toLowerCase() !== initial.name.toLowerCase();
  const clash = (adding || renamed) && existing.has(name.toLowerCase());
  const multiline = type === "REG_MULTI_SZ" || !(STRING_TYPES.has(type) || numberHint(type, "0") !== null);
  const submit = () => !error && onSubmit({ name, type, text });

  return (
    <Modal
      title={`${adding ? "Add" : "Edit"} ${side} value`}
      onClose={onClose}
      wide
      footer={
        <>
          <button onClick={onClose}>Cancel</button>
          <button className="primary" disabled={!!error} onClick={submit}>
            {adding ? "Add" : "Apply"}
          </button>
        </>
      }
    >
      <div className="field">
        <span>Key</span>
        <code className="rv-dialog-key">{keyPath}</code>
      </div>
      <label className="field">
        <span>Name</span>
        <input
          value={name}
          autoFocus={adding}
          placeholder="(Default)"
          onChange={(e) => setName(e.target.value)}
          onKeyDown={(e) => e.key === "Enter" && !multiline && submit()}
        />
        <small className={clash ? "reg-warn" : "muted"}>
          {clash ? "A value with this name exists and will be replaced." : "Leave empty for the key's default value."}
          {renamed && !clash && ` The value "${initial.name || "(Default)"}" is renamed.`}
        </small>
      </label>
      <label className="field">
        <span>Type</span>
        <select value={type} onChange={(e) => setType(e.target.value)}>
          {types.map((t) => (
            <option key={t} value={t}>
              {t}
            </option>
          ))}
        </select>
      </label>
      <label className="field">
        <span>Data</span>
        {multiline ? (
          <textarea
            className="rv-dialog-data"
            rows={type === "REG_MULTI_SZ" ? 6 : 5}
            value={text}
            autoFocus={!adding}
            spellCheck={false}
            onChange={(e) => setText(e.target.value)}
          />
        ) : (
          <input
            className="rv-dialog-data"
            value={text}
            autoFocus={!adding}
            spellCheck={false}
            onChange={(e) => setText(e.target.value)}
            onKeyDown={(e) => e.key === "Enter" && submit()}
          />
        )}
        <small className={error ? "reg-warn" : "muted"}>
          {error ??
            numberHint(type, text) ??
            (type === "REG_MULTI_SZ"
              ? "One string per line; empty lines are dropped."
              : STRING_TYPES.has(type)
                ? "Text, stored as is."
                : "Hex bytes separated by spaces or commas, e.g. 01 ab ff.")}
        </small>
      </label>
    </Modal>
  );
}

/** Asks for the name of a new subkey. */
export function RegKeyDialog({
  side,
  parentPath,
  existing,
  onSubmit,
  onClose,
}: {
  side: RegSide;
  parentPath: string;
  /** Lower-case names of the parent's subkeys on this side. */
  existing: Set<string>;
  onSubmit: (name: string) => void;
  onClose: () => void;
}) {
  const [name, setName] = useState("");
  const trimmed = name.trim();
  const error = !trimmed
    ? "Enter a key name"
    : trimmed.includes("\\")
      ? "Key names cannot contain a backslash"
      : existing.has(trimmed.toLowerCase())
        ? "A subkey with this name already exists"
        : null;
  const submit = () => !error && onSubmit(trimmed);
  return (
    <Modal
      title={`Add ${side} key`}
      onClose={onClose}
      footer={
        <>
          <button onClick={onClose}>Cancel</button>
          <button className="primary" disabled={!!error} onClick={submit}>
            Add
          </button>
        </>
      }
    >
      <div className="field">
        <span>Under</span>
        <code className="rv-dialog-key">{parentPath}</code>
      </div>
      <label className="field">
        <span>Name</span>
        <input
          value={name}
          autoFocus
          onChange={(e) => setName(e.target.value)}
          onKeyDown={(e) => e.key === "Enter" && submit()}
        />
        {error && trimmed && <small className="reg-warn">{error}</small>}
      </label>
    </Modal>
  );
}

/** Shows the reg.exe commands about to run against a live registry. */
export function LiveConfirmDialog({
  side,
  description,
  commands,
  busy,
  error,
  onConfirm,
  onClose,
}: {
  side: RegSide;
  description: string;
  /** `null` while the plan is being computed. */
  commands: string[] | null;
  busy: boolean;
  error: string | null;
  onConfirm: () => void;
  onClose: () => void;
}) {
  return (
    <Modal
      title="Change the live registry?"
      onClose={busy ? () => {} : onClose}
      wide
      footer={
        <>
          <button onClick={onClose} disabled={busy}>
            Cancel
          </button>
          <button className="danger" disabled={busy || !commands?.length} onClick={onConfirm}>
            {busy ? "Applying…" : "Apply to registry"}
          </button>
        </>
      }
    >
      <p>
        {description} on the <strong>{side}</strong> side, which is a live registry key. The change is made immediately
        and cannot be undone here.
      </p>
      {commands === null && !error && <p className="muted">Preparing…</p>}
      {commands?.length === 0 && <p className="muted">Nothing to change: both sides already agree.</p>}
      {commands && commands.length > 0 && <pre className="reg-commands">{commands.join("\n")}</pre>}
      {error && (
        <div className="banner error">
          <TriangleAlert size={13} /> {error}
        </div>
      )}
    </Modal>
  );
}
