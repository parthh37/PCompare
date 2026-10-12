import { ExternalLink } from "lucide-react";
import { useEffect, useState } from "react";
import { errorMessage } from "../lib/api";
import { convertApi, type ExternalTool } from "../lib/convertApi";

interface Props {
  left: string;
  right: string;
  /** Closes the menu the items sit in. */
  onClose: () => void;
  onError: (message: string) => void;
  /** Opens the External tools settings (shown when no tool matches). */
  onConfigure: () => void;
}

/**
 * Context-menu items opening a file pair in the matching third-party
 * comparison tools ("External tools" rules), or a link to set one up.
 */
export function ExternalToolItems({ left, right, onClose, onError, onConfigure }: Props) {
  const [tools, setTools] = useState<ExternalTool[] | null>(null);
  useEffect(() => {
    let stale = false;
    convertApi
      .toolsFor(left, right)
      .then((t) => !stale && setTools(t))
      .catch(() => !stale && setTools([]));
    return () => {
      stale = true;
    };
  }, [left, right]);

  if (!tools) return null;
  if (!tools.length) {
    return (
      <button className="ctx-icon" title="No external tool matches these files — set one up" onClick={onConfigure}>
        <ExternalLink size={16} /> Open with external tool…
      </button>
    );
  }
  return (
    <>
      {tools.map((t, i) => (
        <button
          key={i}
          className="ctx-icon"
          title={t.command}
          onClick={() => {
            onClose();
            convertApi.launchTool(t.command, left, right).catch((e) => onError(errorMessage(e)));
          }}
        >
          <ExternalLink size={16} /> Open with {t.name}
        </button>
      ))}
    </>
  );
}
