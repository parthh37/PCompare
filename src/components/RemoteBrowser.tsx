import { CornerLeftUp, FolderPlus, Pencil, RefreshCw, Trash } from "lucide-react";
import { useEffect, useState } from "react";
import { api, errorMessage, REMOTE_SCHEME, type Profile, type RemoteEntry } from "../lib/api";
import { formatSize, formatTime } from "../lib/format";
import { remoteApi } from "../lib/remoteApi";
import { Modal } from "./Modal";
import { FileIcon } from "./icons";
import "./Remote.css";

interface Props {
  /** Pick a folder (folder compare) or a file (text / hex compare). */
  directory: boolean;
  /** Current value of the path field; reopens there if it is a remote path. */
  initial: string;
  onPick: (path: string) => void;
  onClose: () => void;
  onManageProfiles: () => void;
}

function splitRemote(path: string): { id: string; rel: string } | null {
  if (!path.toLowerCase().startsWith(REMOTE_SCHEME)) return null;
  const rest = path.slice(REMOTE_SCHEME.length);
  const slash = rest.indexOf("/");
  const id = slash < 0 ? rest : rest.slice(0, slash);
  const rel = slash < 0 ? "" : rest.slice(slash + 1).replace(/^\/+|\/+$/g, "");
  return id ? { id, rel } : null;
}

const toUri = (id: string, rel: string) => `${REMOTE_SCHEME}${id}/${rel}`;
const joinRel = (dir: string, name: string) => (dir ? `${dir}/${name}` : name);

/** A file operation being entered or confirmed below the toolbar. */
type Edit =
  | { kind: "mkdir"; value: string }
  | { kind: "rename"; target: string; value: string }
  | { kind: "delete"; target: string; isDir: boolean }
  | null;

const validName = (name: string) => {
  const n = name.trim();
  return n !== "" && n !== "." && n !== ".." && !/[/\\]/.test(n);
};

export function RemoteBrowser({ directory, initial, onPick, onClose, onManageProfiles }: Props) {
  const start = splitRemote(initial);
  const [profiles, setProfiles] = useState<Profile[] | null>(null);
  const [profileId, setProfileId] = useState(start?.id ?? "");
  // A file path opens its parent folder with the file preselected.
  const [dir, setDir] = useState(() => {
    if (!start) return "";
    return directory ? start.rel : start.rel.split("/").slice(0, -1).join("/");
  });
  const [entries, setEntries] = useState<RemoteEntry[]>([]);
  const [selected, setSelected] = useState<string | null>(
    start && !directory ? (start.rel.split("/").pop() ?? null) : null,
  );
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  // bc5:remote-write — create / rename / delete on the remote.
  const [edit, setEdit] = useState<Edit>(null);
  const [busy, setBusy] = useState(false);
  const [reload, setReload] = useState(0);
  const [canRename, setCanRename] = useState(false);

  useEffect(() => {
    api
      .listProfiles()
      .then((ps) => {
        setProfiles(ps);
        if (!profileId && ps.length) setProfileId(ps[0].id);
      })
      .catch((e) => setError(errorMessage(e)));
  }, []);

  useEffect(() => {
    if (!profileId) return;
    let stale = false;
    setLoading(true);
    setError(null);
    api
      .remoteList(toUri(profileId, dir))
      .then((es) => !stale && setEntries(directory ? es.filter((e) => e.isDir) : es))
      .catch((e) => !stale && (setError(errorMessage(e)), setEntries([])))
      .finally(() => !stale && setLoading(false));
    return () => {
      stale = true;
    };
  }, [profileId, dir, directory, reload]);

  useEffect(() => {
    if (!profileId) return;
    let stale = false;
    remoteApi.canRename(toUri(profileId, "")).then(
      (ok) => !stale && setCanRename(ok),
      () => !stale && setCanRename(false),
    );
    return () => {
      stale = true;
    };
  }, [profileId]);

  const enter = (name: string) => {
    setDir(joinRel(dir, name));
    setSelected(null);
    setEdit(null);
  };
  const crumbs = dir ? dir.split("/") : [];
  const pickFile = (name: string) => onPick(toUri(profileId, joinRel(dir, name)));
  const selectedEntry = entries.find((e) => e.name === selected);

  const apply = async () => {
    if (!edit || busy) return;
    setBusy(true);
    setError(null);
    try {
      if (edit.kind === "mkdir") {
        await remoteApi.mkdir(toUri(profileId, joinRel(dir, edit.value.trim())));
        setSelected(edit.value.trim());
      } else if (edit.kind === "rename") {
        await remoteApi.rename(toUri(profileId, joinRel(dir, edit.target)), edit.value.trim());
        setSelected(edit.value.trim());
      } else {
        await remoteApi.remove(toUri(profileId, joinRel(dir, edit.target)));
        setSelected(null);
      }
      setEdit(null);
      setReload((n) => n + 1);
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setBusy(false);
    }
  };

  if (profiles && profiles.length === 0) {
    return (
      <Modal title="Browse remote" onClose={onClose}>
        <div className="empty-state">
          <p>No connection profiles yet.</p>
          <button className="primary" onClick={onManageProfiles}>
            Create a connection profile…
          </button>
        </div>
      </Modal>
    );
  }

  return (
    <Modal
      title={directory ? "Choose remote folder" : "Choose remote file"}
      wide
      onClose={onClose}
      footer={
        <>
          <button onClick={onManageProfiles}>Manage profiles…</button>
          <span className="spacer" />
          <button onClick={onClose}>Cancel</button>
          {directory ? (
            <button className="primary" disabled={!profileId} onClick={() => onPick(toUri(profileId, dir))}>
              Use this folder
            </button>
          ) : (
            <button
              className="primary"
              disabled={!selectedEntry || selectedEntry.isDir}
              onClick={() => selected && pickFile(selected)}
            >
              Open
            </button>
          )}
        </>
      }
    >
      <div className="rb-top">
        <select
          value={profileId}
          onChange={(e) => {
            setProfileId(e.target.value);
            setDir("");
            setSelected(null);
            setEdit(null);
          }}
        >
          {profiles?.map((p) => (
            <option key={p.id} value={p.id}>
              {p.name}
            </option>
          ))}
        </select>
        <div className="crumbs">
          <button className="link" onClick={() => setDir("")}>
            /
          </button>
          {crumbs.map((c, i) => (
            <span key={i}>
              <button className="link" onClick={() => setDir(crumbs.slice(0, i + 1).join("/"))}>
                {c}
              </button>
              {i < crumbs.length - 1 && "/"}
            </span>
          ))}
        </div>
        <div className="rb-tools">
          <button
            className="icon-btn"
            title="New folder"
            disabled={!profileId || busy}
            onClick={() => setEdit({ kind: "mkdir", value: "" })}
          >
            <FolderPlus size={14} />
          </button>
          <button
            className="icon-btn"
            title={canRename ? "Rename" : "This server can't rename files"}
            disabled={!selectedEntry || !canRename || busy}
            onClick={() => selectedEntry && setEdit({ kind: "rename", target: selectedEntry.name, value: selectedEntry.name })}
          >
            <Pencil size={14} />
          </button>
          <button
            className="icon-btn"
            title="Delete permanently"
            disabled={!selectedEntry || busy}
            onClick={() => selectedEntry && setEdit({ kind: "delete", target: selectedEntry.name, isDir: selectedEntry.isDir })}
          >
            <Trash size={14} />
          </button>
          <button className="icon-btn" title="Refresh" disabled={!profileId || loading} onClick={() => setReload((n) => n + 1)}>
            <RefreshCw size={14} />
          </button>
        </div>
      </div>
      {edit && edit.kind !== "delete" && (
        <form
          className="rb-edit"
          onSubmit={(e) => {
            e.preventDefault();
            if (validName(edit.value)) apply();
          }}
        >
          <span>{edit.kind === "mkdir" ? "New folder:" : `Rename “${edit.target}” to:`}</span>
          <input
            autoFocus
            value={edit.value}
            spellCheck={false}
            onChange={(e) => setEdit({ ...edit, value: e.target.value })}
          />
          <button className="primary" type="submit" disabled={busy || !validName(edit.value)}>
            {edit.kind === "mkdir" ? "Create" : "Rename"}
          </button>
          <button type="button" onClick={() => setEdit(null)} disabled={busy}>
            Cancel
          </button>
        </form>
      )}
      {edit?.kind === "delete" && (
        <div className="rb-edit danger-zone">
          <span>
            Delete “{edit.target}”{edit.isDir && " and everything in it"} permanently? Remote folders have no Recycle Bin.
          </span>
          <button className="danger" onClick={apply} disabled={busy}>
            {busy ? "Deleting…" : "Delete"}
          </button>
          <button onClick={() => setEdit(null)} disabled={busy}>
            Cancel
          </button>
        </div>
      )}
      {error && <div className="banner error">{error}</div>}
      <div className="rb-list">
        {dir && (
          <div className="rb-row" onDoubleClick={() => setDir(crumbs.slice(0, -1).join("/"))}>
            <span className="rb-name">
              <CornerLeftUp size={14} className="ic-folder" /> ..
            </span>
          </div>
        )}
        {loading && <div className="rb-row muted">Loading…</div>}
        {!loading &&
          entries.map((e) => (
            <div
              key={e.name}
              className={`rb-row${selected === e.name ? " selected" : ""}`}
              onClick={() => setSelected(e.name)}
              onDoubleClick={() => (e.isDir ? enter(e.name) : pickFile(e.name))}
            >
              <span className="rb-name">
                <FileIcon name={e.name} isDir={e.isDir} /> {e.name}
              </span>
              <span className="muted num">{e.isDir ? "" : formatSize(e.size)}</span>
              <span className="muted">{formatTime(e.modifiedMs)}</span>
            </div>
          ))}
        {!loading && !error && entries.length === 0 && (
          <div className="rb-row muted">{directory ? "No subfolders" : "Empty folder"}</div>
        )}
      </div>
    </Modal>
  );
}
