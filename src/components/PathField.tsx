import { open, save, type DialogFilter } from "@tauri-apps/plugin-dialog";
import { FileArchive, FolderOpen, Globe } from "lucide-react";
import { CONTAINER_FILTERS } from "../lib/archiveApi"; // bc5:archives
import { useEffect, useRef, useState } from "react";
import { PATH_DROP_EVENT } from "../lib/pathDrop"; // araxis:folder
import { useShell } from "../lib/shell";
import { RemoteBrowser } from "./RemoteBrowser";
import { GitBranch } from "lucide-react"; // araxis:vcs
import { VcsRevisionDialog } from "./VcsRevisionDialog"; // araxis:vcs

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
}

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
}: Props) {
  const shell = useShell();
  const [remote, setRemote] = useState(false);
  const [revision, setRevision] = useState(false); // araxis:vcs

  // araxis:folder — a file or folder dropped onto this field fills it (see pathDrop.ts).
  const box = useRef<HTMLDivElement>(null);
  const pick = useRef(onPick);
  pick.current = onPick;
  useEffect(() => {
    const el = box.current;
    if (!el) return;
    const onDrop = (e: Event) => pick.current((e as CustomEvent<string>).detail);
    el.addEventListener(PATH_DROP_EVENT, onDrop);
    return () => el.removeEventListener(PATH_DROP_EVENT, onDrop);
  }, []);
  // araxis:folder end

  const browse = async () => {
    const picked = saveDialog && !directory
      ? await save({ title: "Save as", defaultPath: value || undefined, filters })
      : await open({ directory, multiple: false, filters, title: directory ? "Choose folder" : "Choose file" });
    if (typeof picked === "string") onPick(picked);
  };

  return (
    <div className="pathfield" ref={box /* araxis:folder */}>
      {label && <span className="pathfield-label">{label}</span>}
      <input
        value={value}
        placeholder={placeholder}
        onChange={(e) => onChange(e.target.value)}
        onKeyDown={(e) => e.key === "Enter" && onSubmit?.()}
        spellCheck={false}
      />
      <button className="icon-btn" title="Browse…" onClick={browse}>
        <FolderOpen size={15} />
      </button>
      {/* bc5:archives — folder sides can also be an archive or a snapshot file */}
      {directory && !saveDialog && (
        <button
          className="icon-btn"
          title="Open an archive (zip, tar, 7z…) or a snapshot (.pcsnap) as a folder"
          onClick={async () => {
            const picked = await open({ multiple: false, filters: CONTAINER_FILTERS, title: "Choose archive or snapshot" });
            if (typeof picked === "string") onPick(picked);
          }}
        >
          <FileArchive size={15} />
        </button>
      )}
      {/* araxis:vcs — a git/hg revision of a working-tree file or folder */}
      {!saveDialog && (
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
            onPick(p);
            setRevision(false);
          }}
        />
      )}
      {/* araxis:vcs end */}
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
            onPick(p);
            setRemote(false);
          }}
        />
      )}
    </div>
  );
}
