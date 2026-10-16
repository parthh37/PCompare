import { open, save, type DialogFilter } from "@tauri-apps/plugin-dialog";
import { File as FileIcon, FileArchive, FolderOpen, Globe, X } from "lucide-react";
import { CONTAINER_FILTERS } from "../lib/archiveApi";
import { useEffect, useRef, useState, type KeyboardEvent } from "react";
import { PATH_DROP_EVENT } from "../lib/pathDrop";
import { useShell } from "../lib/shell";
import { RemoteBrowser } from "./RemoteBrowser";
import { GitBranch } from "lucide-react";
import { VcsRevisionDialog } from "./VcsRevisionDialog";
import { forgetPath, matchRecent, rememberPath, useRecentPaths, type PathKind } from "../lib/recentPaths";

interface Props {
  value: string;
  onChange: (value: string) => void;
  /** Enter pressed in the field. */
  onSubmit?: () => void;
  /** A path chosen from the browse or remote dialogs. */
  onPick: (path: string) => void;
  directory: boolean;
  placeholder: string;
  /** Short caption drawn inside the field's left edge, e.g. "Base". */
  label?: string;
  /** Restricts the file dialog, e.g. `[{ name: "Images", extensions: ["png"] }]`. */
  filters?: DialogFilter[];
  /** Use a "Save as" dialog (for output paths) instead of an open dialog. */
  saveDialog?: boolean;
  /** Hide the remote-profile browser (e.g. for local-only outputs). */
  localOnly?: boolean;
  /** Recent-path history to offer while focused; defaults by `directory`, off for save dialogs. */
  history?: PathKind | false;
  /** Takes a file or a folder (Home's Quick Compare): a second browse button picks folders. */
  anyKind?: boolean;
}

/** Last path segment, for the bold part of a suggestion. */
const baseName = (p: string) => p.replace(/[\\/]+$/, "").split(/[\\/]/).pop() || p;

/** One path input with local-browse and remote-browse buttons. */
export function PathField({
  value,
  onChange,
  onSubmit,
  onPick,
  directory,
  placeholder,
  label,
  filters,
  saveDialog,
  localOnly,
  history = saveDialog ? false : directory ? "folder" : "file",
  anyKind,
}: Props) {
  const shell = useShell();
  const [remote, setRemote] = useState(false);
  const [revision, setRevision] = useState(false);

  // a file or folder dropped onto this field fills it (see pathDrop.ts).
  const box = useRef<HTMLDivElement>(null);
  const pick = useRef(onPick);
  pick.current = (p: string) => {
    if (history) rememberPath(history, p);
    onPick(p);
  };
  useEffect(() => {
    const el = box.current;
    if (!el) return;
    const onDrop = (e: Event) => pick.current((e as CustomEvent<string>).detail);
    el.addEventListener(PATH_DROP_EVENT, onDrop);
    return () => el.removeEventListener(PATH_DROP_EVENT, onDrop);
  }, []);

  // Recent paths: a dropdown under the field while it has focus.
  const recent = useRecentPaths(history || "folder");
  const [suggesting, setSuggesting] = useState(false);
  const [active, setActive] = useState(-1);
  const matches = history && suggesting ? matchRecent(recent, value) : [];
  const showRecent = matches.length > 0;
  const choose = (p: string) => {
    setSuggesting(false);
    pick.current(p);
  };
  const onKeyDown = (e: KeyboardEvent<HTMLInputElement>) => {
    if (e.key === "ArrowDown" || e.key === "ArrowUp") {
      e.preventDefault();
      if (!suggesting) return setSuggesting(true);
      const n = matches.length;
      if (n) setActive((i) => (e.key === "ArrowDown" ? (i + 1) % n : (i <= 0 ? n : i) - 1));
    } else if (e.key === "Escape" && showRecent) {
      e.stopPropagation();
      setSuggesting(false);
    } else if (e.key === "Enter") {
      if (showRecent && active >= 0 && matches[active]) choose(matches[active]);
      else {
        setSuggesting(false);
        onSubmit?.();
      }
    }
  };
  useEffect(() => setActive(-1), [value, suggesting]);

  const browse = async (folder = directory) => {
    const picked = saveDialog && !folder
      ? await save({ title: "Save as", defaultPath: value || undefined, filters })
      : await open({ directory: folder, multiple: false, filters, title: folder ? "Choose folder" : "Choose file" });
    if (typeof picked === "string") pick.current(picked);
  };

  return (
    <div className="pathfield" ref={box}>
      {label && <span className="pathfield-label">{label}</span>}
      <input
        value={value}
        placeholder={placeholder}
        onChange={(e) => {
          onChange(e.target.value);
          setSuggesting(true);
        }}
        onFocus={() => setSuggesting(true)}
        onMouseDown={() => setSuggesting(true) /* reopen after Escape or a pick */}
        onBlur={() => setSuggesting(false)}
        onKeyDown={onKeyDown}
        role="combobox"
        aria-expanded={showRecent}
        aria-autocomplete="list"
        spellCheck={false}
      />
      {showRecent && history && (
        <div className="path-recent" role="listbox" onMouseDown={(e) => e.preventDefault() /* keep focus */}>
          <div className="ctx-label">Recent {history === "folder" ? "folders" : "files"}</div>
          {matches.map((p, i) => (
            <div
              key={p}
              role="option"
              aria-selected={i === active}
              className={i === active ? "active" : undefined}
              title={p}
              onMouseEnter={() => setActive(i)}
              onClick={() => choose(p)}
            >
              <span className="path-recent-name">{baseName(p)}</span>
              <span className="path-recent-dir">{p}</span>
              <button
                className="path-recent-forget"
                title="Remove from recent paths"
                onClick={(e) => {
                  e.stopPropagation();
                  forgetPath(history, p);
                }}
              >
                <X size={12} />
              </button>
            </div>
          ))}
        </div>
      )}
      {anyKind ? (
        <>
          <button className="icon-btn" title="Choose a file…" aria-label="Choose a file" onClick={() => browse(false)}>
            <FileIcon size={15} />
          </button>
          <button className="icon-btn" title="Choose a folder…" aria-label="Choose a folder" onClick={() => browse(true)}>
            <FolderOpen size={15} />
          </button>
        </>
      ) : (
        <button className="icon-btn" title="Browse…" onClick={() => browse()}>
          <FolderOpen size={15} />
        </button>
      )}
      {/* folder sides can also be an archive or a snapshot file */}
      {directory && !saveDialog && (
        <button
          className="icon-btn"
          title="Open an archive (zip, tar, 7z…) or a snapshot (.pcsnap) as a folder"
          onClick={async () => {
            const picked = await open({ multiple: false, filters: CONTAINER_FILTERS, title: "Choose archive or snapshot" });
            if (typeof picked === "string") pick.current(picked);
          }}
        >
          <FileArchive size={15} />
        </button>
      )}
      {/* a git/hg revision of a working-tree file or folder (typed git: paths still work in Quick Compare) */}
      {!saveDialog && !anyKind && (
        <button className="icon-btn" title="Compare a Git or Mercurial revision (git:/hg: path)" onClick={() => setRevision(true)}>
          <GitBranch size={15} />
        </button>
      )}
      {revision && (
        <VcsRevisionDialog
          initial={value}
          directory={directory}
          onClose={() => setRevision(false)}
          onPick={(p) => {
            pick.current(p);
            setRevision(false);
          }}
        />
      )}
      {!localOnly && (
        <button className="icon-btn" title="Browse remote (S3, SFTP, OneDrive…)" onClick={() => setRemote(true)}>
          <Globe size={15} />
        </button>
      )}
      {remote && (
        <RemoteBrowser
          directory={directory}
          initial={value}
          onClose={() => setRemote(false)}
          onManageProfiles={() => {
            setRemote(false);
            shell.openProfiles();
          }}
          onPick={(p) => {
            pick.current(p);
            setRemote(false);
          }}
        />
      )}
    </div>
  );
}
