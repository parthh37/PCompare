// Confirmation and settings dialogs for Folder Compare's file operations
// (bc5:file-ops): copy / move (to the other side or a folder), delete,
// rename, new folder, attributes and touch. They only gather choices; the
// session builds the requests and hands them to the Operations queue.
import { open } from "@tauri-apps/plugin-dialog";
import { FolderOpen } from "lucide-react";
import { Fragment, useEffect, useRef, useState } from "react";
import { ATTR, type Side } from "../lib/api";
import {
  describeCount,
  isRemotePath,
  type AttrChange,
  type ConflictMode,
  type FileItem,
  type FileOpOptions,
  type TouchTime,
} from "../lib/fileOpsApi";
import { formatSize } from "../lib/format";
import { Modal } from "./Modal";
import { TRASH_NAME } from "./SyncOptionsDialog";

export const IS_WINDOWS = navigator.userAgent.includes("Windows");

/** The items of one side an operation works on. */
export interface OpPart {
  side: Side;
  srcRoot: string;
  /** The other side's root (copy/move to the other side, touch from the other side). */
  dstRoot: string | null;
  items: FileItem[];
  files: number;
  folders: number;
  bytes: number;
}

/** A request ready for the queue. */
export interface ReadyPart {
  title: string;
  srcRoot: string;
  dstRoot: string | null;
  items: FileItem[];
}

const sideName = (s: Side) => (s === "left" ? "left" : "right");
const lastName = (rel: string) => rel.slice(rel.lastIndexOf("/") + 1);

function ItemList({ parts }: { parts: OpPart[] }) {
  const all = parts.flatMap((p) => p.items.map((i) => (parts.length > 1 ? `${sideName(p.side)}: ${i.src}` : i.src)));
  const shown = all.slice(0, 200);
  return (
    <ul className="fop-items">
      {shown.map((s, i) => (
        <li key={i}>{s}</li>
      ))}
      {all.length > shown.length && <li className="muted">… and {all.length - shown.length} more</li>}
    </ul>
  );
}

const CONFLICTS: { value: ConflictMode; label: string }[] = [
  { value: "ask", label: "Ask for each file" },
  { value: "overwrite", label: "Overwrite" },
  { value: "overwriteOlder", label: "Overwrite older files only" },
  { value: "skip", label: "Skip" },
];

// ---------- copy / move ----------

interface TransferProps {
  action: "copy" | "move";
  parts: OpPart[];
  /** Ask for a destination folder instead of using the other side. */
  toFolder: boolean;
  onCancel: () => void;
  onConfirm: (parts: ReadyPart[], options: Partial<FileOpOptions>) => void;
}

/** Confirms a copy or move, with conflict handling and (to a folder) the destination. */
export function TransferDialog({ action, parts, toFolder, onCancel, onConfirm }: TransferProps) {
  const [folder, setFolder] = useState(() => localStorage.getItem("pcompare.fileOps.lastFolder") ?? "");
  const [keepStructure, setKeepStructure] = useState(true);
  const [conflicts, setConflicts] = useState<ConflictMode>(
    () => (localStorage.getItem("pcompare.fileOps.conflicts") as ConflictMode | null) ?? "ask",
  );
  const [trashReplaced, setTrashReplaced] = useState(true);
  const [copyAcls, setCopyAcls] = useState(false);
  const [copyShortNames, setCopyShortNames] = useState(false);

  const verb = action === "copy" ? "Copy" : "Move";
  const files = parts.reduce((n, p) => n + p.files, 0);
  const folders = parts.reduce((n, p) => n + p.folders, 0);
  const bytes = parts.reduce((n, p) => n + p.bytes, 0);
  const dstOf = (p: OpPart) => (toFolder ? folder.trim() : (p.dstRoot ?? ""));
  const anyRemoteDst = parts.some((p) => isRemotePath(dstOf(p)));
  const allLocal = parts.every((p) => !isRemotePath(p.srcRoot) && !isRemotePath(dstOf(p)));
  const remoteSrc = parts.some((p) => isRemotePath(p.srcRoot));
  const where = toFolder
    ? "to a folder"
    : parts.length === 1
      ? `to the ${sideName(parts[0].side === "left" ? "right" : "left")}`
      : "to the other side";
  const ready = !toFolder || folder.trim() !== "";

  const browse = async () => {
    const picked = await open({ directory: true, multiple: false, title: `${verb} to folder` });
    if (typeof picked === "string") setFolder(picked);
  };

  const confirm = () => {
    if (!ready) return;
    localStorage.setItem("pcompare.fileOps.conflicts", conflicts);
    if (toFolder) localStorage.setItem("pcompare.fileOps.lastFolder", folder.trim());
    const out: ReadyPart[] = parts.map((p) => ({
      title: `${verb} ${describeCount(p.files, p.folders)} ${
        toFolder ? `to ${folder.trim()}` : `${sideName(p.side)} → ${sideName(p.side === "left" ? "right" : "left")}`
      }`,
      srcRoot: p.srcRoot,
      dstRoot: dstOf(p),
      items: toFolder
        ? p.items.map((i) => ({ src: i.src, dst: keepStructure ? i.src : lastName(i.src) }))
        : p.items,
    }));
    onConfirm(out, {
      conflicts,
      trashReplaced,
      copyAcls: IS_WINDOWS && allLocal && copyAcls,
      copyShortNames: IS_WINDOWS && allLocal && copyShortNames,
    });
  };

  return (
    <Modal
      title={`${verb} ${where}`}
      onClose={onCancel}
      footer={
        <>
          <button onClick={onCancel}>Cancel</button>
          <button className="primary" onClick={confirm} disabled={!ready || files + folders === 0}>
            {verb}
          </button>
        </>
      }
    >
      <div className="fop-dialog" onKeyDown={(e) => e.key === "Enter" && confirm()}>
        <div className="fop-summary">
          {verb} <b>{describeCount(files, folders)}</b>
          {bytes ? ` (${formatSize(bytes)})` : ""} {where}
          {parts.length > 1 ? " — from both sides" : ""}.
        </div>
        <ItemList parts={parts} />
        {toFolder && (
          <>
            <label className="field">
              Destination folder
              <div className="input-row">
                <input
                  autoFocus
                  value={folder}
                  onChange={(e) => setFolder(e.target.value)}
                  placeholder="Folder (or remote://profile/path)"
                  spellCheck={false}
                />
                <button onClick={browse} title="Choose folder">
                  <FolderOpen size={14} /> Browse…
                </button>
              </div>
            </label>
            <label className="field check">
              <input type="checkbox" checked={keepStructure} onChange={(e) => setKeepStructure(e.target.checked)} />
              Keep the folder structure (relative paths)
            </label>
          </>
        )}
        <label className="field">
          When a file already exists
          <select value={conflicts} onChange={(e) => setConflicts(e.target.value as ConflictMode)}>
            {CONFLICTS.map((c) => (
              <option key={c.value} value={c.value}>
                {c.label}
              </option>
            ))}
          </select>
        </label>
        {!anyRemoteDst && (
          <label className="field check">
            <input type="checkbox" checked={trashReplaced} onChange={(e) => setTrashReplaced(e.target.checked)} />
            Move replaced files to the {TRASH_NAME}
          </label>
        )}
        {IS_WINDOWS && allLocal && (
          <>
            <label className="field check">
              <input type="checkbox" checked={copyAcls} onChange={(e) => setCopyAcls(e.target.checked)} />
              Copy permissions (NTFS owners and ACLs)
            </label>
            <label className="field check">
              <input type="checkbox" checked={copyShortNames} onChange={(e) => setCopyShortNames(e.target.checked)} />
              Copy short (8.3) names
            </label>
          </>
        )}
        {anyRemoteDst && (
          <div className="fop-warn">
            The destination is remote: files replaced there are overwritten permanently — remote folders have no Recycle
            Bin.
          </div>
        )}
        {action === "move" && remoteSrc && (
          <div className="fop-warn">The originals are removed from the remote folder once they have been copied.</div>
        )}
      </div>
    </Modal>
  );
}

// ---------- delete ----------

interface DeleteProps {
  parts: OpPart[];
  /** Shift+Delete: start with "permanently" chosen. */
  permanent: boolean;
  onCancel: () => void;
  onConfirm: (permanent: boolean) => void;
}

export function DeleteDialog({ parts, permanent: initial, onCancel, onConfirm }: DeleteProps) {
  const [permanent, setPermanent] = useState(initial);
  const files = parts.reduce((n, p) => n + p.files, 0);
  const folders = parts.reduce((n, p) => n + p.folders, 0);
  const remote = parts.filter((p) => isRemotePath(p.srcRoot));
  const local = parts.length > remote.length;
  const sides = parts.length > 1 ? "both sides" : `the ${sideName(parts[0]?.side ?? "left")} side`;
  return (
    <Modal
      title={permanent || !local ? "Delete permanently" : `Move to ${TRASH_NAME}`}
      onClose={onCancel}
      footer={
        <>
          <button onClick={onCancel}>Cancel</button>
          <button className={permanent || !local ? "danger" : "primary"} autoFocus onClick={() => onConfirm(permanent)}>
            {permanent || !local ? "Delete permanently" : "Delete"}
          </button>
        </>
      }
    >
      <div className="fop-dialog">
        <div className="fop-summary">
          Delete <b>{describeCount(files, folders)}</b> from {sides}
          {local && !permanent ? ` — they go to the ${TRASH_NAME}` : ""}.
        </div>
        <ItemList parts={parts} />
        {local && (
          <label className="field check">
            <input type="checkbox" checked={permanent} onChange={(e) => setPermanent(e.target.checked)} />
            <span className={permanent ? "fop-danger" : undefined}>Delete permanently (skip the {TRASH_NAME})</span>
          </label>
        )}
        {permanent && local && <div className="fop-warn">Permanently deleted items can't be restored.</div>}
        {remote.length > 0 && (
          <div className="fop-warn">
            The {remote.map((p) => sideName(p.side)).join(" and ")} folder is remote: deletions there are permanent — remote
            folders have no Recycle Bin.
          </div>
        )}
      </div>
    </Modal>
  );
}

// ---------- rename / new folder ----------

function NameDialog({
  title,
  label,
  initial,
  action,
  note,
  onCancel,
  onConfirm,
}: {
  title: string;
  label: string;
  initial: string;
  action: string;
  note?: string;
  onCancel: () => void;
  onConfirm: (name: string) => void;
}) {
  const [name, setName] = useState(initial);
  const input = useRef<HTMLInputElement>(null);
  useEffect(() => {
    // Select the name without its extension, like Explorer and Finder.
    const el = input.current;
    if (!el) return;
    el.focus();
    const dot = initial.lastIndexOf(".");
    el.setSelectionRange(0, dot > 0 ? dot : initial.length);
  }, [initial]);
  const trimmed = name.trim();
  const invalid = !trimmed || /[/\\]/.test(trimmed) || trimmed === "." || trimmed === "..";
  const submit = () => !invalid && trimmed !== initial && onConfirm(trimmed);
  return (
    <Modal
      title={title}
      onClose={onCancel}
      footer={
        <>
          <button onClick={onCancel}>Cancel</button>
          <button className="primary" onClick={submit} disabled={invalid || trimmed === initial}>
            {action}
          </button>
        </>
      }
    >
      <div className="fop-dialog">
        <label className="field">
          {label}
          <input
            ref={input}
            value={name}
            onChange={(e) => setName(e.target.value)}
            onKeyDown={(e) => e.key === "Enter" && submit()}
            spellCheck={false}
          />
        </label>
        {trimmed && invalid && <small className="s-error-fg">A name can't contain / or \.</small>}
        {note && <small className="muted">{note}</small>}
      </div>
    </Modal>
  );
}

export function RenameDialog(props: {
  name: string;
  side: Side;
  remote: boolean;
  onCancel: () => void;
  onConfirm: (name: string) => void;
}) {
  return (
    <NameDialog
      title={`Rename on the ${sideName(props.side)}`}
      label="New name"
      initial={props.name}
      action="Rename"
      note={props.remote ? "Some remote servers (object stores) can't rename." : undefined}
      onCancel={props.onCancel}
      onConfirm={props.onConfirm}
    />
  );
}

export function NewFolderDialog(props: { side: Side; where: string; onCancel: () => void; onConfirm: (name: string) => void }) {
  return (
    <NameDialog
      title={`New folder on the ${sideName(props.side)}`}
      label={`Name (in ${props.where || "the top folder"})`}
      initial=""
      action="Create"
      onCancel={props.onCancel}
      onConfirm={props.onConfirm}
    />
  );
}

// ---------- attributes ----------

/** A checkbox that can also show "leave as is" (mixed values). */
function TriCheck({ value, mixed, onChange }: { value: boolean | null; mixed: boolean; onChange: (v: boolean | null) => void }) {
  const ref = useRef<HTMLInputElement>(null);
  useEffect(() => {
    if (ref.current) ref.current.indeterminate = value === null;
  }, [value]);
  return (
    <input
      ref={ref}
      type="checkbox"
      checked={value === true}
      onChange={() => onChange(value === null ? true : value ? false : mixed ? null : true)}
    />
  );
}

/** Current state of one bit across the selection: all on, all off, or mixed (`null`). */
function bitState(values: number[], bit: number): boolean | null {
  const on = values.filter((v) => v & bit).length;
  return on === 0 ? false : on === values.length ? true : null;
}

const DOS_BITS: { bit: number; label: string }[] = [
  { bit: ATTR.readonly, label: "Read-only" },
  { bit: ATTR.hidden, label: "Hidden" },
  { bit: ATTR.system, label: "System" },
  { bit: ATTR.archive, label: "Archive" },
];
const WHO = ["Owner", "Group", "Others"];
const RWX = ["Read", "Write", "Execute"];

interface AttrProps {
  /** DOS attribute bits of the selected items (Windows). */
  attributes: number[];
  /** Unix permission bits; non-empty switches the dialog to permissions. */
  modes: number[];
  count: number;
  anyFolder: boolean;
  onCancel: () => void;
  onConfirm: (change: AttrChange, recursive: boolean) => void;
}

export function AttributesDialog({ attributes, modes, count, anyFolder, onCancel, onConfirm }: AttrProps) {
  const unix = modes.length > 0;
  const bits = unix ? [0o400, 0o200, 0o100, 0o040, 0o020, 0o010, 0o004, 0o002, 0o001] : DOS_BITS.map((b) => b.bit);
  const values = unix ? modes : attributes;
  const initial = bits.map((b) => bitState(values, b));
  const [state, setState] = useState<(boolean | null)[]>(initial);
  const [recursive, setRecursive] = useState(false);
  const setAt = (i: number, v: boolean | null) => setState((s) => s.map((x, j) => (j === i ? v : x)));

  const confirm = () => {
    const change: AttrChange = { set: 0, clear: 0, modeSet: 0, modeClear: 0 };
    bits.forEach((b, i) => {
      // Only what the user changed: mixed or untouched bits keep each item's own value.
      if (state[i] === null || state[i] === initial[i]) return;
      if (unix) change[state[i] ? "modeSet" : "modeClear"] |= b;
      else change[state[i] ? "set" : "clear"] |= b;
    });
    onConfirm(change, recursive);
  };

  return (
    <Modal
      title="Change attributes"
      onClose={onCancel}
      footer={
        <>
          <button onClick={onCancel}>Cancel</button>
          <button className="primary" onClick={confirm}>
            Apply
          </button>
        </>
      }
    >
      <div className="fop-dialog">
        <div className="fop-summary">
          {count === 1 ? "1 item" : `${count.toLocaleString()} items`}. A filled box means the items differ; leave it to keep
          each item's own setting.
        </div>
        {unix ? (
          <div className="fop-attr-grid">
            <span />
            {RWX.map((r) => (
              <span key={r} className="head">
                {r}
              </span>
            ))}
            {WHO.map((w, row) => (
              <Fragment key={w}>
                <span>{w}</span>
                {RWX.map((_, col) => {
                  const i = row * 3 + col;
                  return <TriCheck key={col} value={state[i]} mixed={initial[i] === null} onChange={(v) => setAt(i, v)} />;
                })}
              </Fragment>
            ))}
          </div>
        ) : (
          <div className="fop-attr-list">
            {DOS_BITS.map((b, i) => (
              <label key={b.bit} className="field check">
                <TriCheck value={state[i]} mixed={initial[i] === null} onChange={(v) => setAt(i, v)} />
                {b.label}
              </label>
            ))}
          </div>
        )}
        {anyFolder && (
          <label className="field check">
            <input type="checkbox" checked={recursive} onChange={(e) => setRecursive(e.target.checked)} />
            Apply to everything inside the selected folders
          </label>
        )}
      </div>
    </Modal>
  );
}

// ---------- touch ----------

/** `datetime-local` value for a time in ms. */
function localInput(ms: number): string {
  const d = new Date(ms);
  const pad = (n: number) => String(n).padStart(2, "0");
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}T${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}`;
}

interface TouchProps {
  count: number;
  anyFolder: boolean;
  /** The other side exists (for "use the other side's time"). */
  otherSide: boolean;
  /** Pre-filled time for "specific time" (the first item's). */
  initialMs: number | null;
  onCancel: () => void;
  onConfirm: (time: TouchTime, recursive: boolean) => void;
}

export function TouchDialog({ count, anyFolder, otherSide, initialMs, onCancel, onConfirm }: TouchProps) {
  const [mode, setMode] = useState<TouchTime["kind"]>("now");
  const [when, setWhen] = useState(() => localInput(initialMs ?? Date.now()));
  const [recursive, setRecursive] = useState(false);
  const at = new Date(when).getTime();
  const confirm = () => {
    if (mode === "at" && Number.isNaN(at)) return;
    onConfirm(mode === "at" ? { kind: "at", ms: at } : { kind: mode }, recursive);
  };
  return (
    <Modal
      title="Set modification time"
      onClose={onCancel}
      footer={
        <>
          <button onClick={onCancel}>Cancel</button>
          <button className="primary" onClick={confirm} disabled={mode === "at" && Number.isNaN(at)}>
            Touch
          </button>
        </>
      }
    >
      <div className="fop-dialog">
        <div className="fop-summary">{count === 1 ? "1 item" : `${count.toLocaleString()} items`}</div>
        <label className="field radio">
          <input type="radio" name="touch" checked={mode === "now"} onChange={() => setMode("now")} />
          The current time
        </label>
        {otherSide && (
          <label className="field radio">
            <input type="radio" name="touch" checked={mode === "otherSide"} onChange={() => setMode("otherSide")} />
            The other side's time (items missing there are skipped)
          </label>
        )}
        <label className="field radio">
          <input type="radio" name="touch" checked={mode === "at"} onChange={() => setMode("at")} />
          This time:
          <input
            type="datetime-local"
            step={1}
            value={when}
            onChange={(e) => {
              setWhen(e.target.value);
              setMode("at");
            }}
          />
        </label>
        {anyFolder && (
          <label className="field check">
            <input type="checkbox" checked={recursive} onChange={(e) => setRecursive(e.target.checked)} />
            Also everything inside the selected folders
          </label>
        )}
      </div>
    </Modal>
  );
}
