// picks a revision of a working-tree file or folder for a path
// field: the result is a read-only `git:<path>@<rev>` / `hg:<path>@<rev>` side.
import { open } from "@tauri-apps/plugin-dialog";
import { FolderOpen } from "lucide-react";
import { useEffect, useState } from "react";
import { errorMessage, isRemote } from "../lib/api";
import { formatTime } from "../lib/format";
import { splitVcsPath, VCS_NAMES, vcsApi, vcsPath, type RepoInfo, type Revision } from "../lib/vcsApi";
import { Modal } from "./Modal";
import "./Vcs.css";

interface Props {
  /** The field's current value: a plain path or a git:/hg: path. */
  initial: string;
  directory: boolean;
  onPick: (path: string) => void;
  onClose: () => void;
}

export function VcsRevisionDialog({ initial, directory, onPick, onClose }: Props) {
  const parsed = splitVcsPath(initial);
  const [location, setLocation] = useState(parsed?.location ?? (isRemote(initial) ? "" : initial));
  const [rev, setRev] = useState(parsed?.rev ?? "");
  const [repo, setRepo] = useState<RepoInfo | null>(null);
  const [revisions, setRevisions] = useState<Revision[]>([]);
  const [error, setError] = useState("");

  // Detect the repository and load its history as the path settles.
  useEffect(() => {
    const path = location.trim();
    setRepo(null);
    setRevisions([]);
    setError("");
    if (!path) return;
    let live = true;
    const timer = window.setTimeout(() => {
      vcsApi
        .detect(path)
        .then(async (info) => {
          if (!live) return;
          setRepo(info);
          if (info.kind) {
            const revs = await vcsApi.revisions(info.kind, path, 50);
            if (live) setRevisions(revs);
          }
        })
        .catch((e) => live && setError(errorMessage(e)));
    }, 300);
    return () => {
      live = false;
      window.clearTimeout(timer);
    };
  }, [location]);

  const kind = repo?.kind ?? parsed?.kind ?? null;
  const browse = async () => {
    const picked = await open({ directory, multiple: false, title: directory ? "Choose folder in a clone" : "Choose file in a clone" });
    if (typeof picked === "string") setLocation(picked);
  };
  const use = (r = rev) => {
    if (!kind || !location.trim()) return;
    onPick(vcsPath(kind, location.trim(), r));
  };

  let note = "";
  if (repo && !repo.kind) {
    note =
      !repo.gitAvailable && !repo.hgAvailable
        ? "Neither git nor hg is installed (or on PATH)."
        : "This path isn't inside a Git or Mercurial clone.";
  }

  return (
    <Modal
      title="Compare a revision"
      onClose={onClose}
      wide
      footer={
        <>
          <button onClick={onClose}>Cancel</button>
          <button className="primary" disabled={!kind || !location.trim()} onClick={() => use()}>
            Use {rev.trim() || (kind === "hg" ? "." : "HEAD")}
          </button>
        </>
      }
    >
      <label className="field">
        <span>{directory ? "Folder" : "File"} in a working copy</span>
        <div className="input-row">
          <input value={location} onChange={(e) => setLocation(e.target.value)} spellCheck={false} autoFocus />
          <button className="icon-btn" title="Browse…" onClick={browse}>
            <FolderOpen size={15} />
          </button>
        </div>
        <small className="muted">
          {kind ? `${VCS_NAMES[kind]} working copy.` : note || "A file or folder inside a Git or Mercurial clone."}
        </small>
      </label>
      <label className="field">
        <span>Revision</span>
        <input
          value={rev}
          placeholder={kind === "hg" ? ". (the working copy's parent), tip, 42, a hash…" : "HEAD, HEAD~1, main, v1.2, a hash…"}
          onChange={(e) => setRev(e.target.value)}
          onKeyDown={(e) => e.key === "Enter" && use()}
          spellCheck={false}
        />
      </label>
      {error && <div className="banner error">{error}</div>}
      {revisions.length > 0 && (
        <div className="vcs-revisions" role="listbox" aria-label="Recent revisions">
          {revisions.map((r) => (
            <button
              key={r.id}
              role="option"
              aria-selected={rev === r.short}
              className={`vcs-revision${rev === r.short ? " active" : ""}`}
              onClick={() => setRev(r.short)}
              onDoubleClick={() => use(r.short)}
              title={r.id}
            >
              <code>{r.short}</code>
              <span className="vcs-revision-summary">
                {r.refs.map((ref) => (
                  <span key={ref} className="vcs-ref">
                    {ref}
                  </span>
                ))}
                {r.summary}
              </span>
              <span className="muted">{r.author}</span>
              <span className="muted">{formatTime(r.timeMs)}</span>
            </button>
          ))}
        </div>
      )}
    </Modal>
  );
}
