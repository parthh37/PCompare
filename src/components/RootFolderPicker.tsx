import { CornerLeftUp, RefreshCw } from "lucide-react";
import { useEffect, useState } from "react";
import { api, errorMessage, type ProfileInput, type RemoteEntry } from "../lib/api";
import { formatTime } from "../lib/format";
import { Modal } from "./Modal";
import { FileIcon } from "./icons";
import "./Remote.css";

interface Props {
  /** The profile form as typed (may be unsaved); its Root folder is ignored. */
  input: ProfileInput;
  /** The Root folder field's value; the picker opens there. */
  initial: string;
  onPick: (root: string) => void;
  onClose: () => void;
}

const trimSlashes = (p: string) => p.replace(/^\/+|\/+$/g, "");

/** Lets the user pick a profile's Root folder from the account's real folders. */
export function RootFolderPicker({ input, initial, onPick, onClose }: Props) {
  const [dir, setDir] = useState(() => trimSlashes(initial));
  const [folders, setFolders] = useState<RemoteEntry[]>([]);
  const [selected, setSelected] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [reload, setReload] = useState(0);

  useEffect(() => {
    let stale = false;
    setLoading(true);
    setError(null);
    api
      .browseProfile(input, dir)
      .then((fs) => !stale && setFolders(fs))
      .catch((e) => {
        if (stale) return;
        setFolders([]);
        // A stale Root folder may no longer exist: fall back to the top.
        if (dir && /not.?found/i.test(errorMessage(e))) setDir("");
        else setError(errorMessage(e));
      })
      .finally(() => !stale && setLoading(false));
    return () => {
      stale = true;
    };
    // `input` is a fresh object on every render, but the form can't change while this dialog is open.
  }, [dir, reload]);

  const crumbs = dir ? dir.split("/") : [];
  const go = (path: string) => {
    setDir(path);
    setSelected(null);
  };
  const enter = (name: string) => go(dir ? `${dir}/${name}` : name);
  // A selected subfolder wins over the folder being shown.
  const choice = selected ? (dir ? `${dir}/${selected}` : selected) : dir;

  return (
    <Modal
      title="Choose root folder"
      wide
      onClose={onClose}
      footer={
        <>
          <span className="muted">
            Root folder: <code>/{choice}</code>
          </span>
          <span className="spacer" />
          <button onClick={onClose}>Cancel</button>
          <button className="primary" onClick={() => onPick(`/${choice}`)}>
            Use this folder
          </button>
        </>
      }
    >
      <div className="rb-top">
        <div className="crumbs">
          <button className="link" onClick={() => go("")}>
            /
          </button>
          {crumbs.map((c, i) => (
            <span key={i}>
              <button className="link" onClick={() => go(crumbs.slice(0, i + 1).join("/"))}>
                {c}
              </button>
              {i < crumbs.length - 1 && "/"}
            </span>
          ))}
        </div>
        <div className="rb-tools">
          <button className="icon-btn" title="Refresh" disabled={loading} onClick={() => setReload((n) => n + 1)}>
            <RefreshCw size={14} />
          </button>
        </div>
      </div>
      {error && <div className="banner error">{error}</div>}
      <div className="rb-list">
        {dir && (
          <div className="rb-row" onDoubleClick={() => go(crumbs.slice(0, -1).join("/"))}>
            <span className="rb-name">
              <CornerLeftUp size={14} className="ic-folder" /> ..
            </span>
          </div>
        )}
        {loading && <div className="rb-row muted">Loading…</div>}
        {!loading &&
          folders.map((f) => (
            <div
              key={f.name}
              className={`rb-row${selected === f.name ? " selected" : ""}`}
              onClick={() => setSelected(selected === f.name ? null : f.name)}
              onDoubleClick={() => enter(f.name)}
            >
              <span className="rb-name">
                <FileIcon name={f.name} isDir /> {f.name}
              </span>
              <span />
              <span className="muted">{formatTime(f.modifiedMs)}</span>
            </div>
          ))}
        {!loading && !error && folders.length === 0 && <div className="rb-row muted">No subfolders</div>}
      </div>
    </Modal>
  );
}
