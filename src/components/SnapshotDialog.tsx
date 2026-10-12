// bc5:archives — "Save snapshot…": records a folder's names, sizes, dates,
// attributes (and optionally CRCs) to a .pcsnap file, which later opens as a
// read-only Folder Compare side.
import { Camera } from "lucide-react";
import { useState } from "react";
import { errorMessage, isRemote } from "../lib/api";
import { archiveApi, SNAPSHOT_EXT, type SnapshotSummary } from "../lib/archiveApi";
import { formatSize } from "../lib/format";
import { Modal } from "./Modal";
import { PathField } from "./PathField";
import "./Archives.css";

interface Props {
  /** Folders offered for the snapshot (e.g. a Folder Compare's two sides); may be empty. */
  folders: string[];
  onClose: () => void;
}

const FILTERS = [{ name: "PCompare snapshot", extensions: [SNAPSHOT_EXT] }];

/** `C:\work\site` → `C:\work\site.pcsnap` (local folders only). */
function defaultOutput(folder: string): string {
  if (!folder || isRemote(folder) || /^svn:/i.test(folder)) return "";
  return `${folder.replace(/[\\/]+$/, "")}.${SNAPSHOT_EXT}`;
}

const summaryText = (s: SnapshotSummary) =>
  `${s.files.toLocaleString()} files, ${s.folders.toLocaleString()} folders, ${formatSize(s.bytes)}`;

export function SnapshotDialog({ folders, onClose }: Props) {
  const offered = folders.filter(Boolean);
  const [folder, setFolder] = useState(offered[0] ?? "");
  const [output, setOutput] = useState(defaultOutput(offered[0] ?? ""));
  const [includeCrcs, setIncludeCrcs] = useState(false);
  const [busy, setBusy] = useState(false);
  const [progress, setProgress] = useState<SnapshotSummary | null>(null);
  const [done, setDone] = useState<SnapshotSummary | null>(null);
  const [error, setError] = useState<string | null>(null);

  const pickFolder = (f: string) => {
    // Follow the folder with the suggested file name until the user picks one.
    if (!output || output === defaultOutput(folder)) setOutput(defaultOutput(f));
    setFolder(f);
  };

  const save = async () => {
    setBusy(true);
    setError(null);
    setDone(null);
    setProgress(null);
    try {
      const out = output.toLowerCase().endsWith(`.${SNAPSHOT_EXT}`) ? output : `${output}.${SNAPSHOT_EXT}`;
      setOutput(out);
      setDone(await archiveApi.saveSnapshot(folder, out, includeCrcs, setProgress));
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <Modal
      title="Save folder snapshot"
      onClose={onClose}
      footer={
        <>
          <button onClick={onClose}>{done ? "Close" : "Cancel"}</button>
          <button className="primary" disabled={busy || !folder || !output} onClick={save}>
            <Camera size={14} /> {busy ? "Saving…" : "Save snapshot"}
          </button>
        </>
      }
    >
      <div className="snapshot-dialog">
        <p className="muted">
          A snapshot records the names, sizes, dates and attributes of everything in a folder — not the contents. Open
          the <code>.{SNAPSHOT_EXT}</code> file as a Folder Compare side later to see what changed since.
        </p>
        {offered.length > 1 && (
          <div className="snapshot-sides">
            {offered.map((f) => (
              <label key={f} className="field radio">
                <input type="radio" checked={folder === f} onChange={() => pickFolder(f)} />
                <span title={f}>{f}</span>
              </label>
            ))}
          </div>
        )}
        <div className="field">
          <span>Folder</span>
          <PathField value={folder} onChange={setFolder} onPick={pickFolder} directory placeholder="Folder to record…" />
        </div>
        <div className="field">
          <span>Save as</span>
          <PathField
            value={output}
            onChange={setOutput}
            onPick={setOutput}
            directory={false}
            saveDialog
            localOnly
            filters={FILTERS}
            placeholder={`snapshot.${SNAPSHOT_EXT}`}
          />
        </div>
        <label className="field check">
          <input type="checkbox" checked={includeCrcs} onChange={(e) => setIncludeCrcs(e.target.checked)} />
          Include CRCs
        </label>
        <small className="muted">
          Reads every file once to store its CRC-32, so content comparisons against the snapshot can tell changed files
          apart. Without CRCs, files are compared by size and date only.
        </small>
        {busy && progress && <div className="banner info">Recording… {summaryText(progress)}</div>}
        {done && (
          <div className="banner ok">
            Saved {summaryText(done)}
            {done.errors > 0 && ` — ${done.errors} item${done.errors === 1 ? "" : "s"} couldn't be read`}.
          </div>
        )}
        {error && <div className="banner error">{error}</div>}
      </div>
    </Modal>
  );
}
