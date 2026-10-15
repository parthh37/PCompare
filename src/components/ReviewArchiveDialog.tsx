// "Save comparison archive…": packs the comparison in
// front (texts or folder tree, options, bookmarks and comments) into a .pcmpz.
import { Archive } from "lucide-react";
import { useState } from "react";
import { errorMessage } from "../lib/api";
import { basename } from "../lib/format";
import { pickSavePath, reviewApi, type ReviewComparison } from "../lib/reviewApi";
import { Modal } from "./Modal";
import "./Review.css";

interface Props {
  comparison: ReviewComparison;
  defaultTitle: string;
  onClose: () => void;
  /** Called with a status line once the archive is written. */
  onSaved?: (message: string) => void;
}

const lineCount = (text = "") => {
  const n = text ? text.split("\n").length - (text.endsWith("\n") ? 1 : 0) : 0;
  return `${n.toLocaleString()} line${n === 1 ? "" : "s"}`;
};

const KB = 1024;
const MB = 1024 * 1024;

export function ReviewArchiveDialog({ comparison, defaultTitle, onClose, onSaved }: Props) {
  const [title, setTitle] = useState(defaultTitle);
  const [embed, setEmbed] = useState(true);
  const [maxFileKb, setMaxFileKb] = useState(1024);
  const [maxTotalMb, setMaxTotalMb] = useState(64);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const comments = comparison.bookmarks.filter((b) => b.comment).length;

  const saveAs = async () => {
    setError(null);
    const path = await pickSavePath(title || defaultTitle).catch(() => null);
    if (!path) return;
    setBusy(true);
    try {
      const folder = comparison.kind === "folder";
      const opts = folder && embed ? { maxFileBytes: Math.max(1, maxFileKb) * KB, maxTotalBytes: Math.max(1, maxTotalMb) * MB } : null;
      const saved = await reviewApi.save(path, title.trim() || defaultTitle, comparison, opts);
      const files = folder && embed ? ` with ${saved.embedded} differing file${saved.embedded === 1 ? "" : "s"} embedded` : "";
      onSaved?.(`Saved comparison archive ${basename(saved.path)}${files}`);
      onClose();
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <Modal
      title="Save comparison archive"
      onClose={onClose}
      footer={
        <>
          <button onClick={onClose}>Cancel</button>
          <button className="primary" onClick={saveAs} disabled={busy}>
            <Archive size={14} /> {busy ? "Saving…" : "Save as…"}
          </button>
        </>
      }
    >
      <div className="rv-archive-dialog">
        <p className="muted">
          One <code>.pcmpz</code> file a reviewer can open read-only on another machine, with your bookmarks and
          comments.
        </p>
        <label className="field">
          <span>Title</span>
          <input value={title} onChange={(e) => setTitle(e.target.value)} spellCheck={false} autoFocus />
        </label>
        {comparison.kind === "text" ? (
          <p className="muted">
            Includes both texts as shown, unsaved edits too ({comparison.left.label}: {lineCount(comparison.left.text)} ·{" "}
            {comparison.right.label}: {lineCount(comparison.right.text)}), the text rules and{" "}
            {comparison.bookmarks.length} bookmark{comparison.bookmarks.length === 1 ? "" : "s"} ({comments} with comments).
          </p>
        ) : (
          <>
            <p className="muted">
              Includes the comparison as shown — every row with its sizes, dates and status — the rules and{" "}
              {comparison.bookmarks.length} bookmark{comparison.bookmarks.length === 1 ? "" : "s"} ({comments} with comments).
            </p>
            <label className="field check">
              <input type="checkbox" checked={embed} onChange={(e) => setEmbed(e.target.checked)} />
              <span>Include the contents of differing text files, so they can be opened in the archive</span>
            </label>
            <div className="field-row">
              <label className="field">
                <span>Largest file (KB)</span>
                <input
                  type="number"
                  min={1}
                  value={maxFileKb}
                  disabled={!embed}
                  onChange={(e) => setMaxFileKb(Number(e.target.value) || 1)}
                />
              </label>
              <label className="field">
                <span>At most (MB in total)</span>
                <input
                  type="number"
                  min={1}
                  value={maxTotalMb}
                  disabled={!embed}
                  onChange={(e) => setMaxTotalMb(Number(e.target.value) || 1)}
                />
              </label>
            </div>
            <small className="muted">Binary files, and folders on remote profiles or inside archives, are left out.</small>
          </>
        )}
        {error && <div className="banner error">{error}</div>}
      </div>
    </Modal>
  );
}
