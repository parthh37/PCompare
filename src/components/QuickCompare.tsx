// Home's first control: two paths and a Compare button. What the paths are
// (two folders, two spreadsheets, two images…) picks the viewer, the same way
// dropping them on the window does, so nobody has to know the session types.
import { ArrowLeftRight, CircleAlert } from "lucide-react";
import { useState } from "react";
import { api } from "../lib/api";
import { PathField } from "./PathField";

interface Props {
  /** Opens the comparison; either side may be empty to start with one. */
  onCompare: (left: string, right: string) => void;
}

export function QuickCompare({ onCompare }: Props) {
  const [left, setLeftRaw] = useState("");
  const [right, setRightRaw] = useState("");
  // A path that doesn't exist is reported here, before any tab opens.
  const [missing, setMissing] = useState<string[]>([]);
  const [checking, setChecking] = useState(false);
  const setLeft = (v: string) => (setLeftRaw(v), setMissing([]));
  const setRight = (v: string) => (setRightRaw(v), setMissing([]));
  const ready = !!(left.trim() || right.trim()) && !checking;

  const go = async () => {
    if (!ready) return;
    const paths = [left.trim(), right.trim()];
    setChecking(true);
    try {
      const kinds = await Promise.all(paths.map((p) => (p ? api.pathKind(p) : Promise.resolve(null))));
      const gone = paths.filter((_, i) => kinds[i] === "missing");
      setMissing(gone);
      if (!gone.length) onCompare(paths[0], paths[1]);
    } catch {
      // Can't check (e.g. no backend): let the viewer report what's wrong.
      onCompare(paths[0], paths[1]);
    } finally {
      setChecking(false);
    }
  };

  return (
    <section className="quick-compare" aria-label="Quick compare">
      <div className="qc-row">
        <PathField
          anyKind
          directory={false}
          history={false}
          label="Left"
          placeholder="File or folder"
          value={left}
          onChange={setLeft}
          onPick={setLeft}
          onSubmit={go}
        />
        <button
          className="icon-btn qc-swap"
          title="Swap sides"
          aria-label="Swap sides"
          disabled={!ready}
          onClick={() => {
            setLeft(right);
            setRight(left);
          }}
        >
          <ArrowLeftRight size={14} />
        </button>
        <PathField
          anyKind
          directory={false}
          history={false}
          label="Right"
          placeholder="File or folder"
          value={right}
          onChange={setRight}
          onPick={setRight}
          onSubmit={go}
        />
        <button className="primary qc-go" disabled={!ready} onClick={go}>
          Compare
        </button>
      </div>
      {missing.length ? (
        <p className="qc-hint qc-error" role="alert">
          <CircleAlert size={13} aria-hidden />
          {missing.length === 2 ? "Neither path exists: " : "This path doesn't exist: "}
          {missing.map((p, i) => (
            <span key={i}>
              {i > 0 && ", "}
              <code>{p}</code>
            </span>
          ))}
        </p>
      ) : (
        <p className="qc-hint muted">
          PCompare picks the view from what you choose: folders, text, tables, pictures, archives and more. You can
          also drop two files or folders anywhere in this window.
        </p>
      )}
    </section>
  );
}
