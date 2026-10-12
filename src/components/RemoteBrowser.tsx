import { CornerLeftUp } from "lucide-react";
import { useEffect, useState } from "react";
import { api, errorMessage, REMOTE_SCHEME, type Profile, type RemoteEntry } from "../lib/api";
import { formatSize, formatTime } from "../lib/format";
import { Modal } from "./Modal";
import { FileIcon } from "./icons";

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
  }, [profileId, dir, directory]);

  const enter = (name: string) => {
    setDir(dir ? `${dir}/${name}` : name);
    setSelected(null);
  };
  const crumbs = dir ? dir.split("/") : [];
  const pickFile = (name: string) => onPick(toUri(profileId, dir ? `${dir}/${name}` : name));

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
            <button className="primary" disabled={!selected} onClick={() => selected && pickFile(selected)}>
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
      </div>
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
              onClick={() => !e.isDir && setSelected(e.name)}
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
