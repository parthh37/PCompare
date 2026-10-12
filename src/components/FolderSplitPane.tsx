// The bottom half of Folder Compare's split view: the selected row's file
// pair, compared in the same viewer a double-click would open (routing comes
// from `sessionForFiles`). Orphans preview their one file; folders show a hint.
import { useEffect, useMemo, useRef, useState, type CSSProperties } from "react";
import { errorMessage, type TextDiffOptions } from "../lib/api";
import { EMBED_CLASS } from "../lib/embeddedPane";
import { basename, formatSize } from "../lib/format";
import { SESSIONS, sessionForFiles, type CompareKind } from "../lib/sessions";
import { textApi } from "../lib/textApi";
import { HexCompare } from "./HexCompare";
import { PictureCompare } from "./PictureCompare";
import { TableCompare } from "./TableCompare";
import { TextCompare } from "./TextCompare";
import { VirtualList } from "./VirtualList";

export interface SplitPair {
  /** Identifies the row (its relative path). */
  id: string;
  /** "" for the side the file is missing from. */
  left: string;
  right: string;
  /** Size of the file shown for an orphan. */
  size: number;
}

interface Props {
  /** null: nothing to show (no selection, a folder, several rows). */
  pair: SplitPair | null;
  hint: string;
  /** The folder session's text rules, carried into Text Compare. */
  rules: TextDiffOptions;
  style?: CSSProperties;
  /** Opens the shown pair in its own tab. */
  onOpenInTab: (left: string, right: string) => void;
  onDirtyChange: (dirty: boolean) => void;
  /** The embedded viewer saved its edits: the folder comparison should rescan. */
  onSaved: () => void;
}

/** Selection changes settle for this long before a (possibly heavy) viewer loads. */
const DEBOUNCE_MS = 220;
/** Viewers that can live in the pane; other kinds offer to open in a tab. */
const EMBEDDABLE = new Set<CompareKind>(["text", "hex", "table", "picture"]);
/** Orphan previews read at most this much text. */
const PREVIEW_MAX_BYTES = 8 * 1024 * 1024;

const samePair = (a: SplitPair | null, b: SplitPair | null) =>
  a === b || (!!a && !!b && a.id === b.id && a.left === b.left && a.right === b.right);

export function FolderSplitPane({ pair, hint, rules, style, onOpenInTab, onDirtyChange, onSaved }: Props) {
  const [shown, setShown] = useState<SplitPair | null>(pair);
  /** The embedded viewer's paths: its own swap button may change them. */
  const [paths, setPaths] = useState({ left: pair?.left ?? "", right: pair?.right ?? "" });
  /** Text Compare's "Open in Hex" switches this pane, not the tab. */
  const [hex, setHex] = useState(false);
  const dirty = useRef(false);
  const shownRef = useRef(shown);
  shownRef.current = shown;
  const report = useRef({ onDirtyChange, onSaved });
  report.current = { onDirtyChange, onSaved };
  const textSettings = useMemo(() => ({ options: rules }), [rules]);

  useEffect(() => {
    if (samePair(pair, shownRef.current)) return;
    const timer = window.setTimeout(() => {
      if (dirty.current && !window.confirm("The file comparison below has unsaved edits. Discard them?")) return;
      if (dirty.current) {
        dirty.current = false;
        report.current.onDirtyChange(false);
      }
      setShown(pair);
      setPaths({ left: pair?.left ?? "", right: pair?.right ?? "" });
      setHex(false);
    }, DEBOUNCE_MS);
    return () => window.clearTimeout(timer);
  }, [pair]);

  // Closing the split (or the tab) drops the viewer and its dirty state.
  useEffect(() => () => report.current.onDirtyChange(false), []);

  const setDirty = (d: boolean) => {
    const saved = dirty.current && !d;
    dirty.current = d;
    report.current.onDirtyChange(d);
    if (saved) report.current.onSaved();
  };

  const body = () => {
    if (!shown) return <div className="empty-state">{hint}</div>;
    const { left, right } = paths;
    if (!left || !right) return <OrphanPreview pair={shown} onOpenInTab={onOpenInTab} />;
    const routed = sessionForFiles(left, right);
    const kind: CompareKind = hex ? "hex" : routed;
    if (!EMBEDDABLE.has(kind)) {
      return (
        <div className="empty-state fc-embed-note">
          <p>{SESSIONS[kind].label} opens in its own tab.</p>
          <button onClick={() => onOpenInTab(left, right)}>Open {basename(right)}</button>
        </div>
      );
    }
    // A new pair remounts the viewer, so no state leaks from the previous one.
    const key = `${kind}\n${left}\n${right}`;
    const common = {
      left,
      right,
      onPathsChange: (l: string, r: string) => setPaths({ left: l, right: r }),
      onDirtyChange: setDirty,
    };
    switch (kind) {
      case "hex":
        return <HexCompare key={key} {...common} />;
      case "table":
        return <TableCompare key={key} {...common} />;
      case "picture":
        return <PictureCompare key={key} {...common} />;
      default:
        return <TextCompare key={key} {...common} settings={textSettings} onOpenHex={() => setHex(true)} />;
    }
  };

  return (
    // tabIndex: clicks inside focus the pane, which hands it the keyboard (see embeddedPane.ts).
    <div className={EMBED_CLASS} style={style} tabIndex={-1}>
      <div className="fc-embed-body">{body()}</div>
    </div>
  );
}

/** One file of an orphan row: a read-only text preview, or a note for binaries and other viewers. */
function OrphanPreview({ pair, onOpenInTab }: { pair: SplitPair; onOpenInTab: (l: string, r: string) => void }) {
  const side = pair.left ? "left" : "right";
  const path = pair.left || pair.right;
  const kind = sessionForFiles(path, path);
  const previewable = kind === "text" && pair.size <= PREVIEW_MAX_BYTES;
  const [lines, setLines] = useState<string[] | null>(null);
  const [note, setNote] = useState<string | null>(null);

  useEffect(() => {
    setLines(null);
    setNote(null);
    if (!previewable) return;
    let live = true;
    textApi
      .loadText(path, null)
      .then((d) => {
        if (!live) return;
        if (d.binary) setNote("Binary file — no text preview.");
        else setLines(d.lines);
      })
      .catch((e) => live && setNote(errorMessage(e)));
    return () => {
      live = false;
    };
  }, [path, previewable]);

  return (
    <div className="fc-orphan">
      <div className={`fc-orphan-head s-${side === "left" ? "leftOnly" : "rightOnly"}-fg`}>
        <span>
          Only on the {side}: <b>{basename(path)}</b>
        </span>
        <span className="muted" title={path}>
          {formatSize(pair.size)} · {path}
        </span>
        <span className="spacer" />
        <button onClick={() => onOpenInTab(pair.left, pair.right)}>Open in tab</button>
      </div>
      {lines ? (
        <VirtualList
          className="fc-orphan-lines"
          count={lines.length}
          rowHeight={20}
          renderRow={(i) => (
            <div className="fc-orphan-line">
              <span className="ln">{i + 1}</span>
              {lines[i]}
            </div>
          )}
        />
      ) : (
        <div className="empty-state">
          {note ??
            (previewable
              ? "Loading…"
              : kind === "text"
                ? "Too large to preview here."
                : `${SESSIONS[kind].label} file — nothing to compare it with on the ${side === "left" ? "right" : "left"}.`)}
        </div>
      )}
    </div>
  );
}
