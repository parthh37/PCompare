// "Import sessions…": reads saved sessions from Beyond Compare, WinMerge,
// FreeFileSync and other tools, lets the user pick which to keep, and saves
// them as PCompare sessions.
import { open } from "@tauri-apps/plugin-dialog";
import { FileInput, FolderOpen, TriangleAlert } from "lucide-react";
import { useEffect, useMemo, useState } from "react";
import { errorMessage, type SavedSession } from "../lib/api";
import { IMPORT_FILTERS, importApi, type ImportCandidate, type ImportedSession, type ImportSource } from "../lib/importApi";
import { sessionForFiles, SESSIONS } from "../lib/sessions";
import { Modal } from "./Modal";
import { SessionIcon } from "./SessionIcon";
import "./ImportSessionsDialog.css";

interface Props {
  /** Sessions already saved here, so re-imports don't duplicate them. */
  saved: SavedSession[];
  onImported: (count: number) => void;
  onClose: () => void;
}

/** Paths compare case-insensitively and without trailing separators (`C:\a\` = `c:\a`). */
const samePath = (a = "", b = "") => a.replace(/[\\/]+$/, "").toLowerCase() === b.replace(/[\\/]+$/, "").toLowerCase();

function alreadySaved(saved: SavedSession[], s: ImportedSession): boolean {
  return saved.some(
    (x) => x.kind === s.kind && samePath(x.left, s.left) && samePath(x.right, s.right) && samePath(x.base, s.base),
  );
}

/** BC's session folders become part of the name, since PCompare's list is flat. */
const fullName = (s: ImportedSession) => (s.group ? `${s.group} / ${s.name}` : s.name);

export function ImportSessionsDialog({ saved, onImported, onClose }: Props) {
  const [candidates, setCandidates] = useState<ImportCandidate[] | null>(null);
  const [source, setSource] = useState<ImportSource | null>(null);
  /** Indexes into `source.sessions` that will be imported. */
  const [chosen, setChosen] = useState<Set<number>>(new Set());
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    importApi
      .discover()
      .then(setCandidates)
      .catch(() => setCandidates([]));
  }, []);

  const duplicates = useMemo(
    () => new Set((source?.sessions ?? []).flatMap((s, i) => (alreadySaved(saved, s) ? [i] : []))),
    [source, saved],
  );

  const load = async (path: string) => {
    setBusy(true);
    setError(null);
    try {
      const src = await importApi.read(path);
      // Two files open in the viewer their extensions call for, like dropped files.
      for (const s of src.sessions) if (s.fileKindByExtension) s.kind = sessionForFiles(s.left, s.right);
      setSource(src);
      // Named sessions not saved here yet start ticked; auto-saved ones are opt-in.
      setChosen(new Set(src.sessions.flatMap((s, i) => (!s.autoSaved && !alreadySaved(saved, s) ? [i] : []))));
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setBusy(false);
    }
  };

  const browse = async () => {
    const path = await open({ multiple: false, directory: false, filters: IMPORT_FILTERS });
    if (typeof path === "string") load(path);
  };

  const toggle = (i: number) =>
    setChosen((c) => {
      const next = new Set(c);
      if (next.has(i)) next.delete(i);
      else next.add(i);
      return next;
    });

  const selectable = (source?.sessions ?? []).map((_, i) => i).filter((i) => !duplicates.has(i));
  const allChosen = selectable.length > 0 && selectable.every((i) => chosen.has(i));

  const commit = async () => {
    if (!source) return;
    setBusy(true);
    setError(null);
    try {
      const picked = source.sessions.filter((_, i) => chosen.has(i));
      const done = await importApi.commit(
        picked.map((s) => ({
          name: fullName(s),
          kind: s.kind,
          left: s.left,
          right: s.right,
          base: s.base,
          output: s.output,
          settings: s.settings,
        })),
      );
      onImported(done.length);
      onClose();
    } catch (e) {
      setError(errorMessage(e));
      setBusy(false);
    }
  };

  return (
    <Modal
      title="Import sessions"
      wide
      onClose={onClose}
      footer={
        <>
          {source && (
            <button className="link import-back" onClick={() => setSource(null)} disabled={busy}>
              ← Choose another source
            </button>
          )}
          <button onClick={onClose}>Cancel</button>
          <button className="primary" disabled={busy || !source || chosen.size === 0} onClick={commit}>
            <FileInput size={14} /> Import {chosen.size || ""} session{chosen.size === 1 ? "" : "s"}
          </button>
        </>
      }
    >
      <div className="import-dialog">
        {!source ? (
          <>
            <p className="muted">
              Bring saved sessions over from Beyond Compare (3, 4 and 5, including <code>.bcpkg</code> exports), WinMerge
              projects, FreeFileSync configurations and Meld comparisons.
            </p>
            {candidates === null ? (
              <p className="muted">Looking for installed tools…</p>
            ) : candidates.length > 0 ? (
              <div className="import-candidates">
                <h3>Found on this computer</h3>
                {candidates.map((c) => (
                  <button key={c.path} className="import-candidate" disabled={busy} onClick={() => load(c.path)}>
                    <b>{c.label}</b>
                    <span className="muted" title={c.path}>
                      {c.path}
                    </span>
                  </button>
                ))}
              </div>
            ) : (
              <p className="muted">No other compare tool's sessions were found in their usual places.</p>
            )}
            <button onClick={browse} disabled={busy}>
              <FolderOpen size={14} /> Choose a file…
            </button>
            <small className="muted import-hint">
              Beyond Compare keeps sessions in <code>%APPDATA%\Scooter Software\Beyond Compare 5\BCSessions.xml</code>;
              WinMerge projects are <code>.WinMerge</code> files and FreeFileSync uses <code>.ffs_gui</code> /{" "}
              <code>.ffs_batch</code>.
            </small>
          </>
        ) : (
          <>
            <div className="import-source">
              <b>{source.label}</b>
              <span className="muted" title={source.path}>
                {source.path}
              </span>
            </div>
            {source.sessions.length === 0 ? (
              <p className="muted">This file has no sessions PCompare can open.</p>
            ) : (
              <div className="import-table">
                <label className="import-row import-head">
                  <input
                    type="checkbox"
                    checked={allChosen}
                    onChange={() => setChosen(allChosen ? new Set() : new Set(selectable))}
                  />
                  <span>Session</span>
                  <span>Compares</span>
                </label>
                {source.sessions.map((s, i) => {
                  const dup = duplicates.has(i);
                  return (
                    <label key={i} className={`import-row${dup ? " dim" : ""}`}>
                      <input type="checkbox" checked={chosen.has(i)} disabled={dup} onChange={() => toggle(i)} />
                      <span className="import-name">
                        <SessionIcon kind={s.kind} size={13} tile />
                        <span>
                          {fullName(s)}
                          <small className="muted">
                            {SESSIONS[s.kind].label}
                            {s.autoSaved && " · auto-saved"}
                            {dup && " · already saved"}
                          </small>
                        </span>
                      </span>
                      <span className="import-paths muted">
                        <span title={s.left}>{s.left || "—"}</span>
                        <span title={s.right}>{s.right || "—"}</span>
                        {s.base && <span title={s.base}>base: {s.base}</span>}
                        {s.notes.map((n) => (
                          <span key={n} className="import-note">
                            <TriangleAlert size={11} /> {n}
                          </span>
                        ))}
                      </span>
                    </label>
                  );
                })}
              </div>
            )}
            {source.skipped.length > 0 && (
              <details className="import-skipped">
                <summary className="muted">
                  {source.skipped.length} session{source.skipped.length === 1 ? "" : "s"} can't be imported
                </summary>
                <ul>
                  {source.skipped.map((s, i) => (
                    <li key={i}>
                      <b>{s.name}</b> <span className="muted">— {s.reason}</span>
                    </li>
                  ))}
                </ul>
              </details>
            )}
          </>
        )}
        {error && <p className="import-error">{error}</p>}
      </div>
    </Modal>
  );
}
