import {
  ArrowDown,
  ArrowLeft,
  ArrowRight,
  ArrowUp,
  Binary,
  ChevronLeft,
  ChevronRight,
  Lock,
  Redo2,
  RefreshCw,
  Save,
  Undo2,
  // bc5:text-ui
  Bookmark,
  ChevronsLeft,
  ChevronsRight,
  ClipboardPaste,
  Code,
  PanelBottom,
  Search,
  TextWrap,
  X,
} from "lucide-react";
import { embedInactive } from "../lib/embeddedPane"; // araxis:folder
import {
  useCallback,
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
  type ChangeEvent,
  type ClipboardEvent as ReactClipboardEvent,
  type KeyboardEvent as ReactKeyboardEvent,
  type MouseEvent as ReactMouseEvent,
  type WheelEvent,
} from "react";
import {
  DEFAULT_TEXT_OPTIONS,
  errorMessage,
  isRemote,
  type DiffRow,
  type Hunk,
  type LineEnding,
  type Segment,
  type TextDiffOptions,
  type TextDiffResult,
  type TextFileInfo,
  type WhitespaceMode,
} from "../lib/api";
import { formatSize } from "../lib/format";
import { joinLines, type TextDocument, type TextFormat } from "../lib/mergeApi";
// bc5:text-engine — grammar-aware diffing, rules dialog, per-side encodings, save backups.
import { EyeOff, SlidersHorizontal } from "lucide-react";
import { FilePen } from "lucide-react"; // bc5:editor-patch
import { addRange, lineEndingsFor, loadBackups, textApi, type DiffSide } from "../lib/textApi";
import { SEPARATOR, type MenuEntry, type MenuItems } from "../lib/nativeMenu";
import { useContextMenu } from "./ContextMenu";
import { TextEncodingPicker } from "./TextEncodingPicker";
import { TextRulesDialog } from "./TextRulesDialog";
import { autoRefreshTitle, useAutoRefresh } from "../lib/useAutoRefresh";
// bc5:text-ui — find/replace, bookmarks, wrap, line details, syntax, line editing, clipboard sides.
import { readClipboardText, writeClipboardText } from "../lib/clipboard";
import {
  compileQuery,
  DEFAULT_FIND_OPTIONS,
  findInLines,
  replaceAllInLines,
  replaceAt,
  type FindOptions,
  type LineMatch,
} from "../lib/find";
import { languageFor, lineStates, lineTokens, type TokenKind } from "../lib/syntax";
import {
  columnOf,
  copyRows,
  indexAtColumn,
  insertAt,
  joinWithNext,
  joinWithPrevious,
  lineSpan,
  lineToRow,
  replaceLines,
  shiftBookmarks,
  splitText,
  type Bookmark as BookmarkMark,
  type EditResult,
} from "../lib/textEdit";
import { composeRuns, segmentRanges, visualLines, wrapRuns, type Run, type StyledRange } from "../lib/textRuns";
import { FindBar, type FindScope } from "./FindBar";
import { LineDetails } from "./LineDetails";
import { PathBar } from "./PathBar";
import { VarList, type VarListHandle } from "./VarList";
// bc5:converters — converted documents, file format rules, formatted HTML view.
import { AppWindow } from "lucide-react";
import type { Conversion } from "../lib/convertApi";
import { ConversionBanner } from "./ConversionBanner";
import { FileFormatsDialog } from "./FileFormatsDialog";
import { HtmlRenderedView, isHtmlPath } from "./HtmlRenderedView";
// bc5:reports-script — Report… (HTML / text / patch; save, copy, print)
import { FileText } from "lucide-react";
import { ReportDialog } from "./ReportDialog";
// araxis:text-view — linked lines, sync points, go to line, block indent, line endings, replace file,
// resolved / edit markers, IME-safe keys, line drag-and-drop.
import { Ellipsis, Hash, Link2, Pilcrow, Rows3 } from "lucide-react";
import { isComposing } from "../lib/ime";
import { isMac, matches as keyMatches, shortcutLabel } from "../lib/keymap";
import { setPrefs, usePref } from "../lib/prefs";
import { indentRange, indentUnitOf } from "../lib/textEdit";
import { changedRegion, EOL_GLYPH, eolsOfText, linkedLayout, shiftLinePairs } from "../lib/textLinks";
import { TextLinkedView } from "./TextLinkedView";
// araxis:text-view end
// araxis:text-review — bookmark comments, statistics, comparison archives.
import { useTextReview } from "./TextReview";
// araxis:text-review end
import "./TextCompare.css";

const ROW_HEIGHT = 20;
const CHAR_WIDTH = 7.8;
const CONTEXT_LINES = 3;
const MAX_UNDO = 200;
// bc5:converters — also shown for sides a file format rule made read-only.
const READ_ONLY_HINT =
  "Read-only: remote files and converted documents (see File formats) can't be edited here; copy or convert the file locally to edit it";
const TAB_SIZE = 4;
/** Grid geometry from TextCompare.css (line-number gutters, middle gutter, cell padding). */
const LN_WIDTH = 52;
const MID_WIDTH = 40;
const CELL_PAD = 6;
/** While the inline editor is open, re-compare after this pause in typing. */
const RECOMPARE_DELAY_MS = 120;
/** Longer lines are shown without syntax colours. */
const MAX_SYNTAX_LINE = 4000;
const PROBE_CHARS = 100;
const CLIPBOARD_LABEL = "Clipboard";
/** Stands in for a side with no file while the other side has one (an orphan from a folder compare). */
const NO_FILE_LABEL = "no file";

type ViewMode = "all" | "context" | "diffs";
type Side = "left" | "right";
const SIDES: Side[] = ["left", "right"];
const other = (s: Side): Side => (s === "left" ? "right" : "left");

/** What a saved text session remembers besides its paths. */
export interface TextSettings {
  options: TextDiffOptions;
  mode: ViewMode;
  autoRefresh: boolean;
  // bc5:text-ui
  wordWrap?: boolean;
  syntax?: boolean;
  lineDetails?: boolean;
  bookmarks?: BookmarkMark[];
  /** bc5:text-engine — manual encoding per side; null/absent = detect. */
  encodings?: Encodings;
  /**
   * bc5:editor-patch — sides opened as text rather than files (e.g. a patched
   * file from the Patch Viewer). Read once when the tab opens; not saved back.
   */
  memory?: Partial<Record<Side, { text: string; label: string }>>;
  // araxis:text-view
  /** Line-ending glyphs at the end of every line. */
  showEol?: boolean;
  /** Lines marked resolved (dimmed; view-only, the diff is unchanged). */
  resolved?: BookmarkMark[];
  // araxis:text-view end
}
type DisplayItem = { row: number } | { gap: number };

/** Text compared from memory instead of a file (Compare clipboard contents). */
interface MemoryText {
  lines: string[];
  trailingNewline: boolean;
  size: number;
  lineEnding: LineEnding;
  /** bc5:editor-patch — shown instead of "Clipboard" (e.g. "Patched: a.c"). */
  label?: string;
  /** araxis:text-view — each line's terminator (for "Show line endings"). */
  eols?: LineEnding[];
}

function detectLineEnding(text: string): LineEnding {
  const crlf = /\r\n/.test(text);
  const lone = /\r(?!\n)|(^|[^\r])\n/.test(text);
  if (crlf) return lone ? "mixed" : "crlf";
  if (/\r/.test(text)) return /\n/.test(text) ? "mixed" : "cr";
  return /\n/.test(text) ? "lf" : "none";
}

function memoryText(text: string): MemoryText {
  const { lines, trailingNewline } = splitText(text);
  return {
    lines,
    trailingNewline,
    size: new TextEncoder().encode(text).length,
    lineEnding: detectLineEnding(text),
    eols: eolsOfText(text), // araxis:text-view araxis:text-view end
  };
}

const memoryDoc = (m: MemoryText): TextDocument => {
  // araxis:text-view — carries `eols` like a loaded file (see rememberEols).
  const doc: TextDocument & { eols?: LineEnding[] } = {
    info: { path: m.label ?? CLIPBOARD_LABEL, encoding: "Text", hasBom: false, lineEnding: m.lineEnding, size: m.size },
    format: { encoding: "UTF-8", hasBom: false, lineEnding: m.lineEnding === "crlf" ? "crlf" : "lf" },
    lines: m.lines,
    trailingNewline: m.trailingNewline,
    binary: false,
    eols: m.eols,
  };
  return doc;
};

/** Loads both sides; in-memory sides skip the backend, and a side with no file loads empty. */
async function loadSides(
  left: string,
  right: string,
  memory: Record<Side, MemoryText | null>,
  encodings: Encodings = NO_ENCODINGS, // bc5:text-engine
): Promise<{ left: TextDocument; right: TextDocument }> {
  if (!memory.left && !memory.right && left && right)
    return textApi.loadTextPair(left, right, encodings.left, encodings.right);
  const file = async (path: string) =>
    path
      ? textApi.loadText(path, path === left ? encodings.left : encodings.right)
      : // No lines at all, like an empty file (splitting "" would give one blank line).
        memoryDoc({ lines: [], trailingNewline: false, size: 0, lineEnding: "none", label: NO_FILE_LABEL, eols: [] });
  const [l, r] = await Promise.all([
    memory.left ? memoryDoc(memory.left) : file(left),
    memory.right ? memoryDoc(memory.right) : file(right),
  ]);
  return { left: l, right: r };
}

// bc5:text-engine — manual encoding per side (null = detect), saved with the session.
type Encodings = Record<Side, string | null>;
const NO_ENCODINGS: Encodings = { left: null, right: null };
/** Line terminators of loaded documents, keyed by their `lines` array (see `lineEndingsFor`). */
type EolMap = WeakMap<string[], LineEnding[]>;
function rememberEols(map: EolMap, pair: { left: TextDocument; right: TextDocument }, cur: Docs | null) {
  for (const side of SIDES) {
    const eols = (pair[side] as Partial<{ eols: LineEnding[] }>).eols;
    if (!eols) continue;
    map.set(pair[side].lines, eols);
    // A quiet reload keeps the open array when the text is unchanged.
    if (cur && sameLines(cur[side].saved, pair[side].lines)) map.set(cur[side].saved, eols);
  }
}

/** The inline line editor: which line of which side, and where its caret goes. */
interface LineEditor {
  side: Side;
  /** 0-based line in the side's current `lines`. */
  line: number;
  caret: number;
  /** Bumped by every open/move so the caret is re-applied. */
  seq: number;
}

/** A find hit placed on its diff row. */
interface FindHit extends LineMatch {
  side: Side;
  row: number;
}
const NO_HITS: FindHit[] = [];
const sideOrder = (s: Side) => (s === "left" ? 0 : 1);
const compareHits = (a: FindHit, b: FindHit) => a.row - b.row || sideOrder(a.side) - sideOrder(b.side) || a.start - b.start;

/** Joined text of a row side's segments. */
const segText = (segs: Segment[]) => (segs.length === 1 ? segs[0].text : segs.map((s) => s.text).join(""));

/** Engine grammar tokens (when the diff carries them) mapped onto the TS tokenizer's classes. */
const ENGINE_KIND: Record<string, TokenKind> = { comment: "com", string: "str", keyword: "kw", number: "num" };

const renderRun = (r: Run, i: number) => (
  <span key={i} className={r.cls || undefined}>
    {r.text}
  </span>
);

interface Props {
  left: string;
  right: string;
  onPathsChange: (left: string, right: string) => void;
  onOpenHex: () => void;
  settings?: Partial<TextSettings>;
  onSettingsChange?: (s: TextSettings) => void;
  onDirtyChange?: (dirty: boolean, save?: () => unknown) => void;
  /** bc5:editor-patch — "Open in Text Edit" when only one file is chosen. */
  onOpenEditor?: (path: string) => void;
  /** bc5:reports-script — sides opened read-only from the command line (`/readonly`). */
  readOnlySides?: { left: boolean; right: boolean };
}

/** One side's file as edited in memory. */
interface SideDoc {
  info: TextFileInfo;
  format: TextFormat;
  trailingNewline: boolean;
  binary: boolean;
  lines: string[];
  /** The `lines` array last loaded or saved; the side is dirty when `lines` is a different array. */
  saved: string[];
  /** Clipboard text rather than a file: editable, never saved. */
  memory: boolean;
  /** bc5:converters — the file format rule applied when loading (converted text, read-only, grammar). */
  conversion: Conversion | null;
}
type Docs = Record<Side, SideDoc>;
type Snapshot = Record<Side, string[]>;

const toSide = (d: TextDocument, memory = false): SideDoc => ({
  info: d.info,
  format: d.format,
  trailingNewline: d.trailingNewline,
  binary: d.binary,
  lines: d.lines,
  saved: d.lines,
  memory,
  conversion: (d as Partial<{ conversion: Conversion | null }>).conversion ?? null, // bc5:converters
});
const isDirty = (d: SideDoc) => d.lines !== d.saved;
const sameLines = (a: string[], b: string[]) => a.length === b.length && a.every((l, i) => l === b[i]);

/**
 * Folds a re-read file into an open side: unsaved edits are kept, unchanged
 * content keeps its array (so undo history stays valid), anything else is
 * replaced. Returns `null` when the side is unchanged.
 */
function reloadSide(cur: SideDoc, fresh: TextDocument): SideDoc | null {
  if (isDirty(cur)) return null;
  if (cur.binary === fresh.binary && sameLines(cur.lines, fresh.lines)) {
    return {
      ...cur,
      info: fresh.info,
      format: fresh.format,
      trailingNewline: fresh.trailingNewline,
      conversion: toSide(fresh).conversion, // bc5:converters
    };
  }
  return toSide(fresh, cur.memory);
}
const lineNo = (r: DiffRow, side: Side) => (side === "left" ? r.leftNo : r.rightNo);

const isImportant = (r: DiffRow) => r.kind !== "equal" && r.kind !== "unimportant";

function buildDisplay(rows: DiffRow[], mode: ViewMode): DisplayItem[] {
  if (mode === "all") return rows.map((_, row) => ({ row }));
  const ctx = mode === "context" ? CONTEXT_LINES : 0;
  const keep = new Uint8Array(rows.length);
  rows.forEach((r, i) => {
    if (!isImportant(r)) return;
    for (let j = Math.max(0, i - ctx); j <= Math.min(rows.length - 1, i + ctx); j++) keep[j] = 1;
  });
  const out: DisplayItem[] = [];
  let hidden = 0;
  for (let i = 0; i < rows.length; i++) {
    if (keep[i]) {
      if (hidden) out.push({ gap: hidden });
      hidden = 0;
      out.push({ row: i });
    } else hidden++;
  }
  if (hidden) out.push({ gap: hidden });
  return out;
}

function FileInfo({ doc, readOnly }: { doc: SideDoc; readOnly: boolean }) {
  const { info } = doc;
  return (
    <span className="fileinfo">
      {readOnly && (
        <span className="tc-readonly" title={READ_ONLY_HINT}>
          <Lock size={11} /> read-only ·{" "}
        </span>
      )}
      {isDirty(doc) && !doc.memory && (
        <span className="tc-dirty" title="Unsaved changes (Ctrl+S to save)">
          <span className="tc-dirty-dot" /> modified ·{" "}
        </span>
      )}
      {doc.memory ? (
        <>
          {isDirty(doc) ? "edited " : ""}
          {info.path === CLIPBOARD_LABEL ? "clipboard text" : info.path /* bc5:editor-patch */} · {doc.lines.length.toLocaleString()} line
          {doc.lines.length === 1 ? "" : "s"} · {info.lineEnding.toUpperCase()} · {formatSize(info.size)}
        </>
      ) : doc.binary ? (
        <>binary · {formatSize(info.size)}</>
      ) : (
        <>
          {info.encoding}
          {info.hasBom ? " BOM" : ""} · {info.lineEnding.toUpperCase()} · {formatSize(info.size)}
        </>
      )}
    </span>
  );
}

export function TextCompare({
  left,
  right,
  onPathsChange,
  onOpenHex,
  settings,
  onSettingsChange,
  onDirtyChange,
  onOpenEditor, // bc5:editor-patch
  readOnlySides, // bc5:reports-script
}: Props) {
  const [options, setOptions] = useState<TextDiffOptions>({ ...DEFAULT_TEXT_OPTIONS, ...settings?.options });
  const [mode, setMode] = useState<ViewMode>(settings?.mode ?? "all");
  const [autoRefresh, setAutoRefresh] = useState(settings?.autoRefresh ?? false);
  const [editingRules, setEditingRules] = useState(false);
  const [reporting, setReporting] = useState(false); // bc5:reports-script
  // bc5:converters — File formats dialog and the formatted (rendered) HTML view.
  const [fileFormatsOpen, setFileFormatsOpen] = useState(false);
  const [htmlView, setHtmlView] = useState(false);
  // bc5:text-engine
  const [encodings, setEncodings] = useState<Encodings>({ ...NO_ENCODINGS, ...settings?.encodings });
  const encodingsRef = useRef(encodings);
  encodingsRef.current = encodings;
  const eolMap = useRef<EolMap>(new WeakMap());
  // bc5:text-ui — view toggles and bookmarks are saved with the session.
  // araxis:text-view — padding off (linked lines) has no word wrap: each side has its own row heights.
  const padding = usePref("textPadding");
  const [wordWrapOn, setWordWrap] = useState(settings?.wordWrap ?? false);
  const wordWrap = wordWrapOn && padding;
  const [showEol, setShowEol] = useState(settings?.showEol ?? false);
  const [resolved, setResolved] = useState<BookmarkMark[]>(settings?.resolved ?? []);
  /** Lines changed by editing or copying since the files were loaded or saved. */
  const [edited, setEdited] = useState<BookmarkMark[]>([]);
  /** The first line of a sync point, waiting for its partner on the other side. */
  const [syncPick, setSyncPick] = useState<BookmarkMark | null>(null);
  const [gotoOpen, setGotoOpen] = useState(false);
  const [gotoSide, setGotoSide] = useState<Side>("left");
  const [gotoText, setGotoText] = useState("");
  /** Caret index in the line editor (for the Ln/Col indicator). */
  const [caretIdx, setCaretIdx] = useState(0);
  // araxis:text-view end
  const [syntaxOn, setSyntaxOn] = useState(settings?.syntax ?? true);
  const [showDetails, setShowDetails] = useState(settings?.lineDetails ?? false);
  const [bookmarks, setBookmarks] = useState<BookmarkMark[]>(settings?.bookmarks ?? []);
  const reportSettings = useRef(onSettingsChange);
  reportSettings.current = onSettingsChange;
  useEffect(
    () =>
      reportSettings.current?.({
        options,
        mode,
        autoRefresh,
        wordWrap: wordWrapOn, // araxis:text-view
        syntax: syntaxOn,
        lineDetails: showDetails,
        bookmarks,
        encodings, // bc5:text-engine
        showEol, // araxis:text-view
        resolved, // araxis:text-view
      }),
    [options, mode, autoRefresh, wordWrapOn, syntaxOn, showDetails, bookmarks, encodings, showEol, resolved],
  );

  const [docs, setDocs] = useState<Docs | null>(null);
  const docsRef = useRef(docs);
  docsRef.current = docs;
  /** An auto-refresh found a file changed on disk but kept the unsaved edits of that side. */
  const [staleOnDisk, setStaleOnDisk] = useState(false);
  const [history, setHistory] = useState<{ undo: Snapshot[]; redo: Snapshot[] }>({ undo: [], redo: [] });
  /** The diff plus the exact line arrays it was computed from. */
  const [diffed, setDiffed] = useState<{ result: TextDiffResult; for: Snapshot } | null>(null);
  const [error, setError] = useState<string | null>(null);
  /** Diffing failed, e.g. because an ignore rule is not a valid regex. */
  const [diffError, setDiffError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const [saving, setSaving] = useState(false);
  const [hunkIdx, setHunkIdx] = useState(-1);
  const [hx, setHx] = useState(0);
  /** The inline line editor (double-click / Enter), or null. */
  const [editor, setEditor] = useState<LineEditor | null>(null);
  const editorSeq = useRef(0);
  const editorEl = useRef<HTMLTextAreaElement>(null);
  const openEditor = (side: Side, line: number, caret: number) => {
    // Typing after the editor opens or moves is a new undo step.
    coalesceKey.current = null;
    setEditor({ side, line, caret, seq: ++editorSeq.current });
  };
  /** Clipboard text standing in for a side's file. */
  // bc5:editor-patch — a tab may open with text sides (settings.memory).
  const [memory, setMemory] = useState<Record<Side, MemoryText | null>>(() => ({
    left: settings?.memory?.left ? { ...memoryText(settings.memory.left.text), label: settings.memory.left.label } : null,
    right: settings?.memory?.right ? { ...memoryText(settings.memory.right.text), label: settings.memory.right.label } : null,
  }));
  /** Caret line (`cursor`) and, for a line selection, the row it was extended from. */
  const [cursor, setCursor] = useState<{ side: Side; row: number } | null>(null);
  const [anchor, setAnchor] = useState<number | null>(null);
  const [hoverHunk, setHoverHunk] = useState(-1);
  // Row context menu; built after the right-click's caret/hunk change renders.
  const menu = useContextMenu(({ side, row }: { side: Side; row: number }): MenuItems => {
    if (!rows[row]) return [];
    const line = lineNo(rows[row], side);
    const resolvedHere = rangeResolved(resolveRange());
    const what = selRange ? "Selection" : "Line";
    const lines = selRange ? "Selected Lines" : "Line";
    return [
      ...(rowHunk[row] >= 0
        ? [
            menuItem("Copy to Right", "Alt+→", !fresh || readOnly.right, () => copyHunk(rowHunk[row], "left")),
            menuItem("Copy to Left", "Alt+←", !fresh || readOnly.left, () => copyHunk(rowHunk[row], "right")),
          ]
        : []),
      menuItem(
        `Copy ${what} to Right`,
        selRange && cursor?.side === "left" ? "Alt+→" : null,
        !fresh || readOnly.right || !pickRange,
        () => copySelection("left", pickRange),
      ),
      menuItem(
        `Copy ${what} to Left`,
        selRange && cursor?.side === "right" ? "Alt+←" : null,
        !fresh || readOnly.left || !pickRange,
        () => copySelection("right", pickRange),
      ),
      SEPARATOR,
      menuItem(line == null ? "Insert Line Here" : "Edit Line", "Enter", !fresh || readOnly[side], () => startEdit(row, side)),
      menuItem(
        `Delete ${lines}`,
        selRange ? "Del" : null,
        !fresh || readOnly[side] || !hasLines(pickRange, side),
        () => deleteLines(pickRange),
      ),
      menuItem(`Copy ${lines} as Text`, "Ctrl+C", !pickRange, () => copyLinesText()),
      SEPARATOR,
      menuItem("Toggle Bookmark", "Ctrl+F2", false, toggleBookmark),
      menuItem("Comment…", null, false, () => review.commentRow(side, row)), // araxis:text-review
      menuItem("Find…", "Ctrl+F", false, () => openFind(false)),
      menuItem(showDetails ? "Hide Line Details" : "Line Details", null, false, () => setShowDetails((s) => !s)),
      // araxis:text-view
      SEPARATOR,
      menuItem(
        syncPick && syncPick.side !== side ? `Link to ${syncPick.side} line ${syncPick.line}` : "Set Sync Point Here",
        syncHint,
        !fresh || line == null,
        () => setSyncPoint(side, row),
      ),
      line != null && syncLines[side].has(line) && menuItem("Remove Sync Point", null, false, () => removeSyncPoint(side, row)),
      menuItem(resolvedHere ? "Unmark Resolved" : "Mark Resolved", shortcutLabel("text.markResolved") || null, false, () =>
        markResolved(resolvedHere),
      ),
      ...(selRange && cursor?.side === side
        ? [
            menuItem("Indent Lines", "Tab", !fresh || readOnly[side], () => indentSelection(1)),
            menuItem("Un-indent Lines", "Shift+Tab", !fresh || readOnly[side], () => indentSelection(-1)),
          ]
        : []),
      menuItem("Copy All to Right", shortcutLabel("text.copyAllToRight") || null, readOnly.right, () => replaceFile("left")),
      menuItem("Copy All to Left", shortcutLabel("text.copyAllToLeft") || null, readOnly.left, () => replaceFile("right")),
      // araxis:text-view end
    ];
  });

  // araxis:text-view — toolbar "…" menu
  const toolsMenu = useContextMenu((): MenuItems => [
    menuItem("Go to Line…", shortcutLabel("text.goToLine") || null, !docs || binary, openGoto),
    SEPARATOR,
    menuItem("Copy All to Right (replace right text)", shortcutLabel("text.copyAllToRight") || null, !docs || readOnly.right, () =>
      replaceFile("left"),
    ),
    menuItem("Copy All to Left (replace left text)", shortcutLabel("text.copyAllToLeft") || null, !docs || readOnly.left, () =>
      replaceFile("right"),
    ),
    menuItem("Indent Selected Lines", "Tab", !selRange || !cursor || readOnly[cursor.side], () => indentSelection(1)),
    menuItem("Un-indent Selected Lines", "Shift+Tab", !pickRange || !cursor || readOnly[cursor.side], () =>
      indentSelection(-1),
    ),
    SEPARATOR,
    menuItem("Mark Selected Lines Resolved", shortcutLabel("text.markResolved") || null, !resolveRange(), () =>
      markResolved(false),
    ),
    menuItem("Unmark Resolved", null, !rangeResolved(resolveRange()), () => markResolved(true)),
    menuItem(`Clear All Resolved Marks${resolved.length ? ` (${resolved.length})` : ""}`, null, !resolved.length, () =>
      setResolved([]),
    ),
    SEPARATOR,
    menuItem("Set Sync Point on Caret Line", shortcutLabel("text.setSyncPoint") || syncHint, !cursor || !fresh, () =>
      cursor ? setSyncPoint(cursor.side, cursor.row) : undefined,
    ),
    menuItem(
      `Clear Sync Points${options.anchors.length ? ` (${options.anchors.length})` : ""}`,
      null,
      !options.anchors.length && !syncPick,
      clearSyncPoints,
    ),
    menuItem("Clear Edit Markers", null, !edited.length, () => setEdited([])),
    SEPARATOR,
    menuItem(
      padding ? "Vertical Padding Off (Linked Lines)" : "Vertical Padding On",
      shortcutLabel("text.togglePadding") || null,
      false,
      togglePadding,
    ),
    menuItem(showEol ? "Hide Line Endings" : "Show Line Endings", shortcutLabel("text.lineEndings") || null, false, () =>
      setShowEol((v) => !v),
    ),
  ]);
  /** A short status message (e.g. how many matches Replace All changed). */
  const [notice, setNotice] = useState<string | null>(null);
  const list = useRef<VarListHandle>(null);
  const root = useRef<HTMLDivElement>(null);

  const readOnly: Record<Side, boolean> = {
    // bc5:converters — converted documents are read-only unless their rule allows saving.
    // araxis:text-review — `readOnlySides` also covers in-memory texts (a comparison archive under review).
    left: (!memory.left || !!readOnlySides?.left) && (isRemote(left) || !!docs?.left.conversion?.readOnly || !!readOnlySides?.left), // bc5:reports-script
    right: (!memory.right || !!readOnlySides?.right) && (isRemote(right) || !!docs?.right.conversion?.readOnly || !!readOnlySides?.right),
  };
  const dirty = !!docs && SIDES.some((s) => isDirty(docs[s]) && !docs[s].memory);
  const reportDirty = useRef(onDirtyChange);
  reportDirty.current = onDirtyChange;
  // Save all (closing the window or quitting) calls the latest save.
  const saveForClose = useRef<() => unknown>(() => {});
  useEffect(() => void reportDirty.current?.(dirty, () => saveForClose.current()), [dirty]);

  const requestSeq = useRef(0);
  /**
   * Reads both files. `quiet` reloads (auto-refresh) keep the view, the
   * current difference and any unsaved edits instead of resetting.
   */
  const load = useCallback(
    async (quiet = false) => {
      if (!left && !memory.left && !right && !memory.right) {
        // Neither side has a file or text: nothing to show.
        if (!quiet) {
          requestSeq.current++;
          setDocs(null);
          setDiffed(null);
        }
        return;
      }
      const seq = ++requestSeq.current;
      if (!quiet) setLoading(true);
      try {
        const pair = await loadSides(left, right, memory, encodingsRef.current); // bc5:text-engine
        rememberEols(eolMap.current, pair, docsRef.current);
        if (seq !== requestSeq.current) return;
        setError(null);
        const cur = docsRef.current;
        if (quiet && cur) {
          const l = reloadSide(cur.left, pair.left);
          const r = reloadSide(cur.right, pair.right);
          const kept = (side: SideDoc, fresh: TextDocument) => isDirty(side) && !sameLines(side.saved, fresh.lines);
          setStaleOnDisk(kept(cur.left, pair.left) || kept(cur.right, pair.right));
          if ((l && l.lines !== cur.left.lines) || (r && r.lines !== cur.right.lines)) {
            setHistory({ undo: [], redo: [] });
            setEdited([]); // araxis:text-view araxis:text-view end
          }
          setDocs({ left: l ?? cur.left, right: r ?? cur.right });
          return;
        }
        // A side with no file is in-memory text: editable, never saved back or reported dirty.
        setDocs({ left: toSide(pair.left, !!memory.left || !left), right: toSide(pair.right, !!memory.right || !right) });
        setHistory({ undo: [], redo: [] });
        setDiffed(null);
        setHunkIdx(-1);
        setStaleOnDisk(false);
        setEditor(null);
        setCursor(null);
        setAnchor(null);
        // araxis:text-view
        setEdited([]);
        setSyncPick(null);
        // araxis:text-view end
      } catch (e) {
        if (seq !== requestSeq.current) return;
        setError(errorMessage(e));
        if (!quiet) setDocs(null);
      } finally {
        if (seq === requestSeq.current) setLoading(false);
      }
    },
    [left, right, memory],
  );

  useEffect(() => {
    load();
  }, [load]);

  // Clipboard sides have nothing on disk to watch.
  const watched = [memory.left ? "" : left, memory.right ? "" : right].filter(Boolean);
  const watchState = useAutoRefresh({
    enabled: autoRefresh && watched.length > 0,
    paths: watched,
    onChange: () => load(true),
  });

  const binary = !!docs && (docs.left.binary || docs.right.binary);
  const leftLines = docs?.left.lines;
  const rightLines = docs?.right.lines;
  /** bc5:text-engine — a side as the engine diffs it: path (picks the grammar) and terminators. */
  const engineSide = (side: Side, lines: string[]): DiffSide => {
    const d = docsRef.current?.[side];
    if (!d) return { lines };
    const eols = options.compareLineEndings
      ? lineEndingsFor(lines, d.saved, eolMap.current.get(d.saved), d.format, d.trailingNewline)
      : null;
    return { lines, eols, path: d.memory ? null : d.info.path };
  };
  /** bc5:converters — a file format rule's grammar applies unless the rules dialog picked one. */
  const ruleGrammar = docs?.left.conversion?.grammar ?? docs?.right.conversion?.grammar ?? null;
  const editorOpen = useRef(false);
  editorOpen.current = !!editor;
  useEffect(() => {
    if (!leftLines || !rightLines || binary) return;
    let stale = false;
    // bc5:text-ui — while typing in the line editor, re-compare once typing pauses.
    const timer = window.setTimeout(
      () =>
        textApi // bc5:text-engine
          .diffTexts(
            engineSide("left", leftLines),
            engineSide("right", rightLines),
            options.grammar || !ruleGrammar ? options : { ...options, grammar: ruleGrammar }, // bc5:converters
          )
          .then((result) => {
            if (stale) return;
            setDiffed({ result, for: { left: leftLines, right: rightLines } });
            setDiffError(null);
          })
          .catch((e) => !stale && setDiffError(errorMessage(e))),
      editorOpen.current ? RECOMPARE_DELAY_MS : 0,
    );
    return () => {
      stale = true;
      window.clearTimeout(timer);
    };
  }, [leftLines, rightLines, binary, options, ruleGrammar]);

  const diff = binary ? null : diffed?.result ?? null;
  /** Row line numbers match the current text, so edits can be mapped onto it. */
  const fresh = !!diffed && !!docs && diffed.for.left === docs.left.lines && diffed.for.right === docs.right.lines;
  const rows = diff?.rows ?? [];
  const hunks = diff?.hunks ?? [];
  const display = useMemo(() => buildDisplay(rows, mode), [rows, mode]);
  const rowToDisplay = useMemo(() => {
    const map = new Int32Array(rows.length).fill(-1);
    display.forEach((d, i) => "row" in d && (map[d.row] = i));
    return map;
  }, [display, rows.length]);
  const rowHunk = useMemo(() => {
    const map = new Int32Array(rows.length).fill(-1);
    hunks.forEach((h, i) => map.fill(i, h.startRow, h.endRow));
    return map;
  }, [hunks, rows.length]);
  const maxChars = useMemo(() => {
    let m = 0;
    for (const r of rows)
      for (const side of [r.left, r.right])
        if (side) m = Math.max(m, side.reduce((n, s) => n + s.text.length, 0));
    return m;
  }, [rows]);

  // Copying a hunk removes it; keep the selection on the hunk that took its place.
  useEffect(() => {
    if (hunkIdx >= hunks.length) setHunkIdx(hunks.length - 1);
  }, [hunks.length, hunkIdx]);

  const goHunk = useCallback(
    (idx: number) => {
      if (!hunks.length) return;
      const clamped = Math.max(0, Math.min(hunks.length - 1, idx));
      setHunkIdx(clamped);
      list.current?.scrollToRow(rowToDisplay[hunks[clamped].startRow]);
    },
    [hunks, rowToDisplay],
  );

  // ---------- bc5:text-ui: layout, syntax and find over the displayed rows ----------

  /** The line arrays `rows` were computed from (lags the docs while a re-compare runs). */
  const shown = diff ? diffed!.for : null;
  /** 0-based line → row, per side, for the displayed rows. */
  const lineRow = useMemo(
    () =>
      shown && {
        left: lineToRow(rows, "left", shown.left.length),
        right: lineToRow(rows, "right", shown.right.length),
      },
    [rows, shown],
  );

  // Character width and columns per pane, measured from the real font (the editor font size is a preference).
  const gridRef = useRef<HTMLDivElement>(null);
  const probeRef = useRef<HTMLSpanElement>(null);
  const [metrics, setMetrics] = useState({ charW: CHAR_WIDTH, cols: 80, cellW: 400 });
  const hasGrid = !!diff;
  useLayoutEffect(() => {
    const grid = gridRef.current;
    const probe = probeRef.current;
    if (!grid || !probe) return;
    const listEl = grid.querySelector<HTMLElement>(".text-list");
    const measure = () => {
      const charW = probe.getBoundingClientRect().width / PROBE_CHARS || CHAR_WIDTH;
      const cellW = Math.max(40, ((listEl?.clientWidth ?? grid.clientWidth) - 2 * LN_WIDTH - MID_WIDTH) / 2);
      const cols = Math.max(8, Math.floor((cellW - CELL_PAD - 4) / charW));
      setMetrics((m) => (m.charW === charW && m.cols === cols && m.cellW === cellW ? m : { charW, cols, cellW }));
    };
    const ro = new ResizeObserver(measure);
    ro.observe(grid);
    ro.observe(probe);
    if (listEl) ro.observe(listEl);
    measure();
    return () => ro.disconnect();
  }, [hasGrid, padding]); // araxis:text-view — the linked view is a different list element

  /** Word wrap: each row is as tall as the longer of its two wrapped lines, so both sides stay aligned. */
  const offsets = useMemo(() => {
    if (!wordWrap) return null;
    const off = new Float64Array(display.length + 1);
    for (let i = 0; i < display.length; i++) {
      const d = display[i];
      let n = 1;
      if ("row" in d) {
        const r = rows[d.row];
        n = Math.max(
          r.left ? visualLines(segText(r.left), metrics.cols, TAB_SIZE) : 1,
          r.right ? visualLines(segText(r.right), metrics.cols, TAB_SIZE) : 1,
        );
      }
      off[i + 1] = off[i] + n * ROW_HEIGHT;
    }
    return off;
  }, [wordWrap, display, rows, metrics.cols]);

  // Syntax: a clipboard side borrows the other side's language.
  const langs = useMemo(() => {
    const l = memory.left ? null : languageFor(left);
    const r = memory.right ? null : languageFor(right);
    return { left: l ?? r, right: r ?? l };
  }, [left, right, memory.left, memory.right]);
  const lineStateBySide = useMemo(
    () =>
      syntaxOn && shown
        ? {
            left: langs.left ? lineStates(shown.left, langs.left) : null,
            right: langs.right ? lineStates(shown.right, langs.right) : null,
          }
        : null,
    [syntaxOn, shown, langs],
  );

  // Find & Replace.
  const [findOpen, setFindOpen] = useState(false);
  const [replaceMode, setReplaceMode] = useState(false);
  const [findFocus, setFindFocus] = useState(0);
  const [query, setQuery] = useState("");
  const [replacement, setReplacement] = useState("");
  const [findOpts, setFindOpts] = useState<FindOptions>(DEFAULT_FIND_OPTIONS);
  const [findScope, setFindScope] = useState<FindScope>("both");
  const [findIdx, setFindIdx] = useState(-1);
  const compiled = useMemo(() => compileQuery(query, findOpts), [query, findOpts]);
  /** Hits in the displayed text, in on-screen order (rows hidden by Context/Diffs are skipped). */
  const matches = useMemo(() => {
    if (!findOpen || !compiled.re || !shown || !lineRow) return NO_HITS;
    const out: FindHit[] = [];
    for (const side of SIDES) {
      if (findScope !== "both" && findScope !== side) continue;
      for (const m of findInLines(shown[side], compiled.re)) {
        const row = lineRow[side][m.line];
        if (row >= 0 && rowToDisplay[row] >= 0) out.push({ ...m, side, row });
      }
    }
    return out.sort(compareHits);
  }, [findOpen, compiled, shown, lineRow, findScope, rowToDisplay]);
  const hitRanges = useMemo(() => {
    const map = new Map<string, StyledRange[]>();
    matches.forEach((m, i) => {
      const key = `${m.side}:${m.line}`;
      const ranges = map.get(key) ?? [];
      ranges.push({ start: m.start, end: m.end, cls: i === findIdx ? "find-hit find-cur" : "find-hit" });
      map.set(key, ranges);
    });
    return map;
  }, [matches, findIdx]);

  // Bookmarks, located on the displayed rows.
  const bookmarkKeys = useMemo(() => new Set(bookmarks.map((b) => `${b.side}:${b.line}`)), [bookmarks]);
  const bookmarkRows = useMemo(() => {
    if (!lineRow) return [];
    const set = new Set<number>();
    for (const b of bookmarks) {
      const r = lineRow[b.side][b.line - 1];
      if (r !== undefined && r >= 0) set.add(r);
    }
    return [...set].sort((a, b) => a - b);
  }, [bookmarks, lineRow]);

  /** The explicit line selection (rows, inclusive) on `cursor.side`. */
  const selRange: [number, number] | null =
    cursor && anchor !== null ? [Math.min(anchor, cursor.row), Math.max(anchor, cursor.row)] : null;
  /** The selection, or just the caret line. */
  const pickRange: [number, number] | null = selRange ?? (cursor ? [cursor.row, cursor.row] : null);

  /** Row of the line being edited in the displayed rows (-1 while it is not there yet). */
  const editorRow =
    editor && lineRow && editor.line < lineRow[editor.side].length ? lineRow[editor.side][editor.line] : -1;

  useEffect(() => {
    if (cursor && cursor.row >= rows.length) {
      setCursor(rows.length ? { ...cursor, row: rows.length - 1 } : null);
      setAnchor(null);
    }
  }, [rows.length, cursor]);

  // The caret follows the line editor.
  useEffect(() => {
    if (!editor || editorRow < 0) return;
    if (cursor?.row !== editorRow || cursor.side !== editor.side) setCursor({ side: editor.side, row: editorRow });
    if (rowToDisplay[editorRow] >= 0) list.current?.ensureVisible(rowToDisplay[editorRow]);
    // Only when the edited row moves, not on every cursor change.
  }, [editor, editorRow]);

  // Focus and place the caret once per open/move, as soon as the editor is on screen (an
  // inserted line may only get a row after the re-compare).
  const caretApplied = useRef(0);
  useLayoutEffect(() => {
    const el = editorEl.current;
    if (!el || !editor || caretApplied.current === editor.seq) return;
    caretApplied.current = editor.seq;
    el.focus({ preventScroll: true });
    el.setSelectionRange(editor.caret, editor.caret);
    setCaretIdx(editor.caret); // araxis:text-view
  });

  useEffect(() => {
    if (!notice) return;
    const t = window.setTimeout(() => setNotice(null), 4000);
    return () => window.clearTimeout(t);
  }, [notice]);


  /** Typing in one line merges into one undo step; anything else starts a new one. */
  const coalesceKey = useRef<string | null>(null);

  // araxis:text-view
  /**
   * Keeps the line marks in step with a change from `prev` to `next`: edit
   * markers flag the changed lines (cleared when a side is back to its saved
   * text), resolved marks and sync points move with the text below the change.
   */
  const trackEdits = (prev: Snapshot, next: Snapshot) => {
    for (const side of SIDES) {
      const clean = !!docs && next[side] === docs[side].saved;
      if (clean) setEdited((m) => (m.some((b) => b.side === side) ? m.filter((b) => b.side !== side) : m));
      const region = changedRegion(prev[side], next[side]);
      if (!region) continue;
      const { start, removed, inserted } = region;
      if (!clean)
        setEdited((m) => {
          const kept = shiftBookmarks(m, side, start, removed, inserted);
          const have = new Set(kept.flatMap((b) => (b.side === side ? [b.line] : [])));
          const add: BookmarkMark[] = [];
          for (let i = start + 1; i <= start + inserted; i++) if (!have.has(i)) add.push({ side, line: i });
          return add.length ? [...kept, ...add] : kept;
        });
      setResolved((m) => shiftBookmarks(m, side, start, removed, inserted));
      if (removed !== inserted && options.anchors.length)
        setOptions((o) => ({ ...o, anchors: shiftLinePairs(o.anchors, side, start, removed, inserted) }));
    }
  };
  // araxis:text-view end

  /** Replaces the text of both sides as one undoable step; returns the new docs. */
  const commit = (next: Snapshot, coalesce?: string): Docs | null => {
    if (!docs) return null;
    if (next.left === docs.left.lines && next.right === docs.right.lines) return docs;
    trackEdits({ left: docs.left.lines, right: docs.right.lines }, next); // araxis:text-view
    const updated: Docs = {
      left: { ...docs.left, lines: next.left },
      right: { ...docs.right, lines: next.right },
    };
    const merge = !!coalesce && coalesce === coalesceKey.current && history.undo.length > 0;
    coalesceKey.current = coalesce ?? null;
    setHistory((h) =>
      merge
        ? { undo: h.undo, redo: [] }
        : { undo: [...h.undo.slice(-(MAX_UNDO - 1)), { left: docs.left.lines, right: docs.right.lines }], redo: [] },
    );
    setDocs(updated);
    return updated;
  };

  const snapshot = (): Snapshot | null => docs && { left: docs.left.lines, right: docs.right.lines };

  /**
   * Replaces one side's lines as an undoable step. `span` says which lines
   * were replaced by how many, so bookmarks below it move with their text.
   */
  const commitSide = (
    side: Side,
    lines: string[],
    span?: { start: number; removed: number; inserted: number },
    coalesce?: string,
  ) => {
    const snap = snapshot();
    if (!snap) return;
    commit({ ...snap, [side]: lines }, coalesce);
    if (span) setBookmarks((b) => shiftBookmarks(b, side, span.start, span.removed, span.inserted));
  };

  /** After undo/redo the editor stays on its line when that still exists. */
  const followEditor = (snap: Snapshot) => {
    if (!editor) return;
    const lines = snap[editor.side];
    if (editor.line < lines.length) openEditor(editor.side, editor.line, lines[editor.line].length);
    else setEditor(null);
  };

  const undo = () => {
    const cur = snapshot();
    const prev = history.undo[history.undo.length - 1];
    if (!cur || !prev) return;
    coalesceKey.current = null;
    trackEdits(cur, prev); // araxis:text-view
    setHistory({ undo: history.undo.slice(0, -1), redo: [...history.redo, cur] });
    setDocs((d) => d && { left: { ...d.left, lines: prev.left }, right: { ...d.right, lines: prev.right } });
    followEditor(prev);
  };

  const redo = () => {
    const cur = snapshot();
    const next = history.redo[history.redo.length - 1];
    if (!cur || !next) return;
    coalesceKey.current = null;
    trackEdits(cur, next); // araxis:text-view
    setHistory({ undo: [...history.undo, cur], redo: history.redo.slice(0, -1) });
    setDocs((d) => d && { left: { ...d.left, lines: next.left }, right: { ...d.right, lines: next.right } });
    followEditor(next);
  };

  /** Copies the lines `from` shows in rows `[startRow, endRow)` over the other side's lines there. */
  const copyRowsFrom = (startRow: number, endRow: number, from: Side): boolean => {
    const to = other(from);
    if (!docs || !fresh || readOnly[to]) return false;
    const res = copyRows(rows, startRow, endRow, from, docs[from].lines, docs[to].lines);
    if (!res) return false;
    commitSide(to, res.lines, { start: res.span.start, removed: res.span.end - res.span.start, inserted: res.inserted });
    return true;
  };

  /** Replaces the destination side of hunk `idx` with the lines from `from`. */
  const copyHunk = (idx: number, from: Side) => {
    if (!fresh || readOnly[other(from)] || idx < 0 || idx >= hunks.length) return;
    setHunkIdx(idx);
    copyRowsFrom(hunks[idx].startRow, hunks[idx].endRow, from);
  };

  /** Copy Selection to Left/Right: only the selected rows' lines. */
  const copySelection = (from: Side, range: [number, number] | null) => {
    if (range && copyRowsFrom(range[0], range[1] + 1, from)) setAnchor(null);
  };

  /** Alt+→ / Alt+← and the toolbar arrows: the selected lines when the selection is on the source side, else the current difference. */
  const copyTo = (to: Side) => {
    const from = other(to);
    if (selRange && cursor?.side === from) copySelection(from, selRange);
    else copyHunk(hunkIdx, from);
  };

  /** Deletes the lines of `range` on the caret's side. */
  const deleteLines = (range: [number, number] | null) => {
    if (!range || !cursor || !docs || !fresh || readOnly[cursor.side]) return;
    const side = cursor.side;
    const span = lineSpan(rows, range[0], range[1] + 1, side);
    if (span.start === span.end) return;
    commitSide(side, replaceLines(docs[side].lines, span.start, span.end, []), {
      start: span.start,
      removed: span.end - span.start,
      inserted: 0,
    });
    setAnchor(null);
    setCursor({ side, row: range[0] });
  };

  /** Ctrl+C without a text selection: the selected lines (or the caret line) as text. */
  const copyLinesText = (): boolean => {
    if (!cursor || !shown || !pickRange) return false;
    const span = lineSpan(rows, pickRange[0], pickRange[1] + 1, cursor.side);
    if (span.start === span.end) return false;
    const text = shown[cursor.side].slice(span.start, span.end).join("\n");
    writeClipboardText(span.end - span.start > 1 ? `${text}\n` : text).catch((e) => setError(errorMessage(e)));
    return true;
  };

  // ---------- line editing ----------

  /**
   * Opens the line editor on `row`. A padding row (no line on that side)
   * first gets an empty line inserted where it belongs.
   */
  const startEdit = (row: number, side: Side, caretAt?: number) => {
    if (!docs || !fresh || readOnly[side] || !rows[row]) return;
    const n = lineNo(rows[row], side);
    if (n != null) {
      const text = docs[side].lines[n - 1] ?? "";
      openEditor(side, n - 1, Math.min(caretAt ?? text.length, text.length));
    } else {
      const { start } = lineSpan(rows, row, row + 1, side);
      commitSide(side, replaceLines(docs[side].lines, start, start, [""]), { start, removed: 0, inserted: 1 });
      openEditor(side, start, 0);
    }
    setCursor({ side, row });
    setAnchor(null);
  };

  /** The character under a click in a cell, for placing the caret. */
  const caretFromPoint = (e: ReactMouseEvent<HTMLElement>, text: string) => {
    const rect = e.currentTarget.getBoundingClientRect();
    let col = (e.clientX - rect.left - CELL_PAD + (wordWrap ? 0 : hx)) / metrics.charW;
    if (wordWrap) col += Math.floor((e.clientY - rect.top) / ROW_HEIGHT) * metrics.cols;
    return indexAtColumn(text, Math.max(0, col), TAB_SIZE);
  };

  /** Applies a structural edit (split, join, multi-line paste) and moves the editor to its caret. */
  const applyEdit = (side: Side, res: EditResult | null) => {
    if (!res) return;
    const span =
      res.delta > 0
        ? { start: res.at, removed: 0, inserted: res.delta }
        : res.delta < 0
          ? { start: res.at, removed: -res.delta, inserted: 0 }
          : undefined;
    commitSide(side, res.lines, span, res.delta === 0 ? `type:${side}:${res.line}` : undefined);
    openEditor(side, res.line, res.caret);
  };

  const onEditorChange = (e: ChangeEvent<HTMLTextAreaElement>) => {
    if (!editor || !docs) return;
    const value = e.target.value;
    const lines = docs[editor.side].lines;
    if (/[\r\n]/.test(value)) {
      // Line breaks that arrive without Enter/paste (drag-and-drop, IME) split the line too.
      applyEdit(editor.side, insertAt(lines, editor.line, 0, (lines[editor.line] ?? "").length, value));
      return;
    }
    const next = lines.slice();
    next[editor.line] = value;
    commitSide(editor.side, next, undefined, `type:${editor.side}:${editor.line}`);
  };

  /** Up/Down leave the line unless word wrap still has visual lines to move through. */
  const leavesLine = (ta: HTMLTextAreaElement, up: boolean) => {
    if (!wordWrap) return true;
    const at = Math.floor(columnOf(ta.value, ta.selectionStart, TAB_SIZE) / metrics.cols);
    return up ? at === 0 : at >= visualLines(ta.value, metrics.cols, TAB_SIZE) - 1;
  };

  const moveEditor = (delta: number, caret: number) => {
    if (!editor || !docs) return;
    const lines = docs[editor.side].lines;
    const target = editor.line + delta;
    if (target < 0 || target >= lines.length) return;
    // Lines hidden by the Context/Diffs view are not editable in place.
    const row = lineRow?.[editor.side][target] ?? -1;
    if (fresh && (row < 0 || rowToDisplay[row] < 0)) return;
    const col = columnOf(lines[editor.line] ?? "", caret, TAB_SIZE);
    openEditor(editor.side, target, indexAtColumn(lines[target], col, TAB_SIZE));
  };

  const onEditorKey = (e: ReactKeyboardEvent<HTMLTextAreaElement>) => {
    if (!editor || !docs || isComposing(e)) return; // araxis:text-view — keys belong to an open IME composition
    const ta = e.currentTarget;
    const { selectionStart: s, selectionEnd: end, value } = ta;
    const lines = docs[editor.side].lines;
    const mod = e.ctrlKey || e.metaKey;
    const key = e.key.toLowerCase();
    if (e.key === "Enter" && !mod && !e.altKey) applyEdit(editor.side, insertAt(lines, editor.line, s, end, "\n"));
    // araxis:text-view — Shift+Tab un-indents the line.
    else if (e.key === "Tab" && e.shiftKey && !mod && !e.altKey) outdentEditorLine(s);
    else if (e.key === "Tab" && !mod && !e.altKey) applyEdit(editor.side, insertAt(lines, editor.line, s, end, "\t"));
    else if (e.key === "Backspace" && !mod && !e.altKey && s === 0 && end === 0)
      applyEdit(editor.side, joinWithPrevious(lines, editor.line));
    else if (e.key === "Delete" && !mod && !e.altKey && s === value.length && end === s)
      applyEdit(editor.side, joinWithNext(lines, editor.line));
    else if ((e.key === "ArrowUp" || e.key === "ArrowDown") && !mod && !e.altKey && !e.shiftKey) {
      const up = e.key === "ArrowUp";
      if (!leavesLine(ta, up)) return;
      moveEditor(up ? -1 : 1, s);
    } else if (e.key === "Escape") setEditor(null);
    else if (mod && !e.shiftKey && !e.altKey && key === "z") undo();
    else if (mod && !e.altKey && (key === "y" || (e.shiftKey && key === "z"))) redo();
    else return;
    e.preventDefault();
  };

  const onEditorPaste = (e: ReactClipboardEvent<HTMLTextAreaElement>) => {
    if (!editor || !docs) return;
    const text = e.clipboardData.getData("text/plain");
    if (!/[\r\n]/.test(text)) return; // single-line paste: the textarea handles it
    e.preventDefault();
    const ta = e.currentTarget;
    applyEdit(editor.side, insertAt(docs[editor.side].lines, editor.line, ta.selectionStart, ta.selectionEnd, text));
  };

  // ---------- find ----------

  const resumeAt = useRef<{ side: Side; line: number; col: number } | null>(null);
  const findKey = `${findOpen}\u0000${query}\u0000${findOpts.matchCase}${findOpts.wholeWord}${findOpts.regex}\u0000${findScope}`;
  const lastFindKey = useRef("");

  /** Selects hit `i`, scrolls it into view (vertically and horizontally) and puts the caret on it. */
  const revealHit = (i: number) => {
    const m = matches[i];
    if (!m) return setFindIdx(-1);
    setFindIdx(i);
    setCursor({ side: m.side, row: m.row });
    setAnchor(null);
    list.current?.ensureVisible(rowToDisplay[m.row], true);
    if (!wordWrap && shown) {
      const text = shown[m.side][m.line];
      const x0 = columnOf(text, m.start, TAB_SIZE) * metrics.charW;
      const x1 = columnOf(text, m.end, TAB_SIZE) * metrics.charW;
      const view = metrics.cellW - CELL_PAD;
      if (x0 < hx || x1 > hx + view) setHx(Math.max(0, x0 - view / 3));
    }
  };

  useEffect(() => {
    // A new search jumps to the first hit at or after the caret (incremental search).
    if (lastFindKey.current !== findKey) {
      lastFindKey.current = findKey;
      if (!matches.length) return setFindIdx(-1);
      const from = cursor?.row ?? 0;
      const i = matches.findIndex((m) => m.row >= from);
      revealHit(i >= 0 ? i : 0);
      return;
    }
    // After Replace, continue after the replaced text once the re-compare has landed.
    const r = resumeAt.current;
    if (r && docs && shown && lineRow && shown[r.side] === docs[r.side].lines) {
      resumeAt.current = null;
      const row = lineRow[r.side][r.line];
      const i = matches.findIndex(
        (m) =>
          m.row > row ||
          (m.row === row && (sideOrder(m.side) > sideOrder(r.side) || (m.side === r.side && m.start >= r.col))),
      );
      if (matches.length) revealHit(i >= 0 ? i : 0);
      else setFindIdx(-1);
      return;
    }
    if (findIdx >= matches.length) setFindIdx(matches.length ? 0 : -1);
    // Runs when the hits change; the find key and resume marker say why.
  }, [matches]);

  const openFind = (replace: boolean) => {
    const active = document.activeElement;
    const picked =
      active instanceof HTMLTextAreaElement
        ? active.value.slice(active.selectionStart, active.selectionEnd)
        : (window.getSelection()?.toString() ?? "");
    if (picked && !/[\r\n]/.test(picked)) setQuery(picked);
    setFindOpen(true);
    setReplaceMode(replace);
    setFindFocus((f) => f + 1);
  };

  /** F3 / Shift+F3: next or previous hit, wrapping around. */
  const findStep = (dir: 1 | -1) => {
    if (!findOpen) {
      if (!query) return openFind(false);
      setFindOpen(true);
      return;
    }
    if (!matches.length) return;
    if (findIdx >= 0 && findIdx < matches.length) return revealHit((findIdx + dir + matches.length) % matches.length);
    const from = cursor?.row ?? -1;
    let i = -1;
    if (dir > 0) i = matches.findIndex((m) => m.row > from);
    else for (let k = matches.length - 1; k >= 0 && i < 0; k--) if (matches[k].row < from) i = k;
    revealHit(i >= 0 ? i : dir > 0 ? 0 : matches.length - 1);
  };

  const scopeSides = SIDES.filter((s) => findScope === "both" || findScope === s);
  const replaceBlocked = !fresh
    ? "Waiting for the comparison to catch up"
    : scopeSides.every((s) => readOnly[s])
      ? READ_ONLY_HINT
      : null;

  const replaceCurrent = () => {
    if (replaceBlocked || !docs || !compiled.re) return;
    const m = matches[findIdx];
    if (!m) return findStep(1);
    if (readOnly[m.side]) {
      setNotice(READ_ONLY_HINT);
      return findStep(1);
    }
    const lines = docs[m.side].lines;
    const out = replaceAt(lines[m.line], compiled.re, m.start, replacement, findOpts.regex);
    if (out === null) return;
    const next = lines.slice();
    next[m.line] = out;
    resumeAt.current = { side: m.side, line: m.line, col: m.end + out.length - lines[m.line].length };
    commitSide(m.side, next);
  };

  const replaceAll = () => {
    if (replaceBlocked || !docs || !compiled.re) return;
    const next = snapshot()!;
    let total = 0;
    for (const s of scopeSides) {
      if (readOnly[s]) continue;
      const r = replaceAllInLines(docs[s].lines, compiled.re, replacement, findOpts.regex);
      next[s] = r.lines;
      total += r.count;
    }
    if (total) commit(next);
    setNotice(`Replaced ${total.toLocaleString()} occurrence${total === 1 ? "" : "s"}`);
  };

  // ---------- bookmarks ----------

  /** Ctrl+F2: toggles a bookmark on the caret line (the other side's line on a padding row). */
  const toggleBookmark = () => {
    if (!cursor || !rows[cursor.row]) return;
    let side = cursor.side;
    let n = lineNo(rows[cursor.row], side);
    if (n == null) {
      side = other(side);
      n = lineNo(rows[cursor.row], side);
    }
    if (n == null) return;
    const line = n;
    setBookmarks((bs) =>
      bs.some((b) => b.side === side && b.line === line)
        ? bs.filter((b) => !(b.side === side && b.line === line))
        : [...bs, { side, line }],
    );
  };

  /** F2 / Shift+F2: next or previous bookmark, wrapping around. */
  const gotoBookmark = (dir: 1 | -1) => {
    const visible = bookmarkRows.filter((r) => rowToDisplay[r] >= 0);
    if (!visible.length || !lineRow) return;
    const from = cursor?.row ?? (dir > 0 ? -1 : rows.length);
    let target = -1;
    if (dir > 0) target = visible.find((r) => r > from) ?? visible[0];
    else for (let k = visible.length - 1; k >= 0 && target < 0; k--) if (visible[k] < from) target = visible[k];
    if (target < 0) target = visible[visible.length - 1];
    const mark = bookmarks.find((b) => lineRow[b.side][b.line - 1] === target);
    setCursor({ side: mark?.side ?? cursor?.side ?? "left", row: target });
    setAnchor(null);
    list.current?.ensureVisible(rowToDisplay[target], true);
  };

  /** Up/Down (Shift extends the selection) move the caret line. */
  const moveCursor = (delta: 1 | -1, extend: boolean) => {
    if (!display.length) return;
    const side = cursor?.side ?? "left";
    let d = cursor ? rowToDisplay[cursor.row] + delta : delta > 0 ? 0 : display.length - 1;
    while (d >= 0 && d < display.length && !("row" in display[d])) d += delta;
    if (d < 0 || d >= display.length) return;
    const item = display[d] as { row: number };
    if (extend) setAnchor(anchor ?? cursor?.row ?? item.row);
    else setAnchor(null);
    setCursor({ side, row: item.row });
    list.current?.ensureVisible(d);
  };

  // ---------- clipboard sides ----------

  const loadClipboard = async (side: Side) => {
    if (!confirmDiscard("compare the clipboard text instead")) return;
    try {
      const text = await readClipboardText();
      setMemory((m) => ({ ...m, [side]: memoryText(text) }));
      setBookmarks((bs) => bs.filter((b) => b.side !== side));
      setError(null);
    } catch (e) {
      setError(`Could not read the clipboard: ${errorMessage(e)}`);
    }
  };

  const closeMemory = (side: Side) => {
    if (!confirmDiscard("close the clipboard text")) return;
    setMemory((m) => ({ ...m, [side]: null }));
    setBookmarks((bs) => bs.filter((b) => b.side !== side));
  };

  /** Whether `side` holds unsaved edits that can be written back to its file. */
  const sideSavable = (side: Side) =>
    !!docs && isDirty(docs[side]) && !readOnly[side] && !docs[side].memory;

  /** Saves `only` that side, or every modified side when omitted. */
  const save = async (only?: Side) => {
    const current = docs;
    if (!current || saving) return;
    const sides = (only ? [only] : SIDES).filter((s) => sideSavable(s));
    if (!sides.length) return;
    setSaving(true);
    setError(null);
    try {
      for (const s of sides) {
        const doc = current[s];
        // bc5:text-engine — backups per the rules dialog setting.
        const { info, backup } = await textApi.saveTextFile(
          doc.info.path,
          joinLines(doc.lines, doc.trailingNewline),
          doc.format,
          loadBackups(),
        );
        if (backup) setNotice(`Previous version backed up to ${backup}`);
        // Edits made while saving stay dirty: only the saved array is marked clean.
        setDocs((d) => d && { ...d, [s]: { ...d[s], info: { ...info, path: doc.info.path }, saved: doc.lines } });
        setEdited((m) => m.filter((b) => b.side !== s)); // araxis:text-view araxis:text-view end
      }
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setSaving(false);
    }
  };

  saveForClose.current = () => save();

  const confirmDiscard = (action: string) =>
    !dirty || window.confirm(`You have unsaved changes. Discard them and ${action}?`);

  const refresh = () => {
    if (confirmDiscard("reload both files")) load();
  };
  // bc5:text-engine — how many importance rules are active (shown on the Rules button).
  const ruleCount =
    options.ignorePatterns.filter((p) => p.trim()).length +
    options.pageHeaderPatterns.filter((p) => p.trim()).length +
    options.ignoreColumns.length +
    options.replacements.length +
    options.unimportantLines.left.length +
    options.unimportantLines.right.length +
    options.anchors.length +
    options.breaks.length +
    [options.ignoreComments, options.ignoreStrings, options.compareLineEndings].filter(Boolean).length +
    (options.pageHeaderLines || options.pageFooterLines ? 1 : 0) +
    // araxis:text-engine
    (options.numericTolerance?.enabled ? 1 : 0) +
    (options.ignoreBlocks?.length ?? 0);
  // araxis:text-engine end

  /** Marks the selected rows (or the current difference) unimportant on both sides. */
  const markUnimportant = () => {
    if (!fresh) return;
    let from: number;
    let to: number;
    if (cursor) {
      from = Math.min(anchor ?? cursor.row, cursor.row);
      to = Math.max(anchor ?? cursor.row, cursor.row) + 1;
    } else if (hunkIdx >= 0 && hunkIdx < hunks.length) {
      ({ startRow: from, endRow: to } = hunks[hunkIdx]);
    } else return;
    let marks = options.unimportantLines;
    for (const side of SIDES) {
      const nums = rows.slice(from, to).flatMap((r) => lineNo(r, side) ?? []);
      if (nums.length) marks = { ...marks, [side]: addRange(marks[side], Math.min(...nums), Math.max(...nums)) };
    }
    setOptions({ ...options, unimportantLines: marks });
  };

  // ---------- araxis:text-view: sync points, go to line, indent, replace file, resolved, line drag ----------

  const resolvedKeys = useMemo(() => new Set(resolved.map((b) => `${b.side}:${b.line}`)), [resolved]);
  const editedKeys = useMemo(() => new Set(edited.map((b) => `${b.side}:${b.line}`)), [edited]);
  /** Lines (1-based) that are one end of a sync point, per side. */
  const syncLines = useMemo(
    () => ({ left: new Set(options.anchors.map((a) => a.left)), right: new Set(options.anchors.map((a) => a.right)) }),
    [options.anchors],
  );
  const syncPairs = useMemo(() => new Set(options.anchors.map((a) => `${a.left}:${a.right}`)), [options.anchors]);
  /** Each shown line's terminator, for "Show line endings". */
  const eolBySide = useMemo(() => {
    if (!showEol || !shown || !docs) return null;
    const of = (side: Side) =>
      lineEndingsFor(
        shown[side],
        docs[side].saved,
        eolMap.current.get(docs[side].saved),
        docs[side].format,
        docs[side].trailingNewline,
      );
    return { left: of("left"), right: of("right") };
  }, [showEol, shown, docs]);
  /** Padding off: each side's own lines, in blocks linked across the middle gutter. */
  const linked = useMemo(() => (padding ? null : linkedLayout(display, rows, rowHunk)), [padding, display, rows, rowHunk]);
  /** Rows holding edited lines (overview strip). */
  const editRows = useMemo(() => {
    if (!lineRow || !edited.length) return [];
    const set = new Set<number>();
    for (const b of edited) {
      const r = lineRow[b.side][b.line - 1];
      if (r !== undefined && r >= 0) set.add(r);
    }
    return [...set].sort((a, b) => a - b);
  }, [edited, lineRow]);
  /** Sync points as display indices of their two lines (drawn in the linked view's gutter). */
  const syncLinks = useMemo(() => {
    if (!linked || !lineRow) return [];
    return options.anchors.flatMap((a) => {
      const l = lineRow.left[a.left - 1];
      const r = lineRow.right[a.right - 1];
      if (l === undefined || r === undefined || l < 0 || r < 0) return [];
      const dl = rowToDisplay[l];
      const dr = rowToDisplay[r];
      return dl >= 0 && dr >= 0 ? [{ left: dl, right: dr }] : [];
    });
  }, [linked, lineRow, options.anchors, rowToDisplay]);

  const togglePadding = () => setPrefs({ textPadding: !padding });

  /**
   * Ctrl/⌘+click or "Set Sync Point Here": the first line waits for a partner
   * on the other side; the pair becomes an alignment anchor and the files are
   * compared again with those two lines side by side.
   */
  const setSyncPoint = (side: Side, row: number) => {
    const n = rows[row] ? lineNo(rows[row], side) : null;
    if (!fresh || n == null) {
      setNotice(fresh ? "A sync point needs a line on that side, not a padding row" : "Waiting for the comparison to catch up");
      return;
    }
    if (syncPick && syncPick.side !== side) {
      const pair = side === "left" ? { left: n, right: syncPick.line } : { left: syncPick.line, right: n };
      setOptions((o) => ({
        ...o,
        anchors: [...o.anchors.filter((a) => a.left !== pair.left && a.right !== pair.right), pair],
      }));
      setSyncPick(null);
      setNotice(`Sync point: left line ${pair.left} ↔ right line ${pair.right}`);
    } else setSyncPick({ side, line: n });
  };

  const removeSyncPoint = (side: Side, row: number) => {
    const n = rows[row] ? lineNo(rows[row], side) : null;
    if (n != null) setOptions((o) => ({ ...o, anchors: o.anchors.filter((a) => a[side] !== n) }));
  };

  const clearSyncPoints = () => {
    setSyncPick(null);
    if (options.anchors.length) setOptions((o) => ({ ...o, anchors: [] }));
  };

  /** Moves the caret to `row` and scrolls it into view, showing all lines first if the view hides it. */
  const pendingReveal = useRef<number | null>(null);
  const revealRow = (side: Side, row: number) => {
    setCursor({ side, row });
    setAnchor(null);
    if (rowToDisplay[row] >= 0) list.current?.ensureVisible(rowToDisplay[row], true);
    else {
      pendingReveal.current = row;
      setMode("all");
    }
  };
  useEffect(() => {
    const row = pendingReveal.current;
    if (row === null || !(rowToDisplay[row] >= 0)) return;
    pendingReveal.current = null;
    list.current?.ensureVisible(rowToDisplay[row], true);
  }, [rowToDisplay]);

  // araxis:text-review — Comments panel, Statistics, Save comparison archive.
  const review = useTextReview({
    bookmarks,
    setBookmarks,
    docs,
    rows,
    hunks,
    lineRow,
    rowToDisplay,
    options,
    settings: { mode, wordWrap, syntax: syntaxOn, lineDetails: showDetails },
    reveal: revealRow,
    onNotice: setNotice,
  });
  // araxis:text-review end

  /** Ctrl+G: the line bar opens on the caret's side with its line number. */
  const openGoto = () => {
    if (!docs || binary) return;
    const side = cursor?.side ?? "left";
    const n = cursor && rows[cursor.row] ? lineNo(rows[cursor.row], side) : null;
    setGotoSide(side);
    setGotoText(n ? String(n) : "");
    setGotoOpen(true);
  };

  const gotoLine = () => {
    const n = parseInt(gotoText, 10);
    if (!lineRow || !shown || !Number.isFinite(n) || !shown[gotoSide].length) return;
    const line = Math.max(1, Math.min(shown[gotoSide].length, n));
    const row = lineRow[gotoSide][line - 1];
    if (row === undefined || row < 0) return;
    setGotoOpen(false);
    revealRow(gotoSide, row);
  };

  /** Tab / Shift+Tab: indents or un-indents the selected lines of the caret's side. */
  const indentSelection = (dir: 1 | -1) => {
    const range = selRange ?? pickRange;
    if (!range || !cursor || !docs || !fresh || readOnly[cursor.side]) return;
    const side = cursor.side;
    const span = lineSpan(rows, range[0], range[1] + 1, side);
    if (span.end <= span.start) return;
    const lines = docs[side].lines;
    const next = indentRange(lines, span.start, span.end - 1, dir, indentUnitOf(lines, TAB_SIZE), TAB_SIZE);
    if (next !== lines) commitSide(side, next);
  };

  /** Shift+Tab in the line editor: un-indents the line, keeping the caret on its text. */
  const outdentEditorLine = (caret: number) => {
    if (!editor || !docs) return;
    const lines = docs[editor.side].lines;
    const next = indentRange(lines, editor.line, editor.line, -1, "", TAB_SIZE);
    if (next === lines) return;
    commitSide(editor.side, next, undefined, `type:${editor.side}:${editor.line}`);
    openEditor(editor.side, editor.line, Math.max(0, caret - (lines[editor.line].length - next[editor.line].length)));
  };

  /** Copy All to Right / Left: the other side's whole text replaced by this side's (undoable). */
  const replaceFile = (from: Side) => {
    const to = other(from);
    if (!docs || readOnly[to]) return;
    if (sameLines(docs[from].lines, docs[to].lines)) {
      setNotice(`The ${to} text already matches the ${from}`);
      return;
    }
    commitSide(to, docs[from].lines.slice());
    setNotice(`Replaced the ${to} text with the ${from} (Ctrl+Z undoes)`);
  };

  /** Rows the resolved commands act on: the selection or caret line, else the current difference. */
  const resolveRange = (): [number, number] | null =>
    pickRange ?? (hunkIdx >= 0 && hunkIdx < hunks.length ? [hunks[hunkIdx].startRow, hunks[hunkIdx].endRow - 1] : null);
  const rangeLines = (range: [number, number]) => {
    const out: BookmarkMark[] = [];
    for (let i = range[0]; i <= range[1] && i < rows.length; i++)
      for (const side of SIDES) {
        const n = lineNo(rows[i], side);
        if (n != null) out.push({ side, line: n });
      }
    return out;
  };
  const rangeResolved = (range: [number, number] | null) => {
    const marks = range ? rangeLines(range) : [];
    return marks.length > 0 && marks.every((m) => resolvedKeys.has(`${m.side}:${m.line}`));
  };
  /** Marks (or unmarks) both sides' lines in the selection as resolved: dimmed, the diff is unchanged. */
  const markResolved = (unmark: boolean) => {
    const range = resolveRange();
    if (!range) return;
    const marks = rangeLines(range);
    const keys = new Set(marks.map((m) => `${m.side}:${m.line}`));
    setResolved((rs) => {
      const rest = rs.filter((b) => !keys.has(`${b.side}:${b.line}`));
      return unmark ? rest : [...rest, ...marks];
    });
  };
  const toggleResolved = () => markResolved(rangeResolved(resolveRange()));

  // Line drag-and-drop: drag a selection by its line numbers onto either pane.
  const lineDrag = useRef<{ side: Side; range: [number, number]; row: number; x: number; y: number; active: boolean } | null>(
    null,
  );
  const [dropAt, setDropAt] = useState<{ side: Side; row: number; after: boolean; copy: boolean } | null>(null);
  const dropRef = useRef(dropAt);
  dropRef.current = dropAt;

  /** Inserts the dragged lines at the drop line: copied to the other side (or with Ctrl/Alt), moved within a side. */
  const dropLines = (
    drag: { side: Side; range: [number, number] },
    drop: { side: Side; row: number; after: boolean; copy: boolean },
  ) => {
    const from = drag.side;
    const to = drop.side;
    if (readOnly[to]) return setNotice(READ_ONLY_HINT);
    if (!docs || !fresh) return;
    const span = lineSpan(rows, drag.range[0], drag.range[1] + 1, from);
    if (span.end <= span.start) return;
    const block = docs[from].lines.slice(span.start, span.end);
    const target = lineSpan(rows, drop.row, drop.row + 1, to);
    const at = drop.after && target.end > target.start ? target.end : target.start;
    const copy = to !== from || drop.copy;
    if (copy) commitSide(to, replaceLines(docs[to].lines, at, at, block), { start: at, removed: 0, inserted: block.length });
    else {
      if (at >= span.start && at <= span.end) return;
      const rest = replaceLines(docs[from].lines, span.start, span.end, []);
      const first = at > span.end ? at - block.length : at;
      commitSide(from, replaceLines(rest, first, first, block));
    }
    setAnchor(null);
    setNotice(`${copy ? "Copied" : "Moved"} ${block.length} line${block.length === 1 ? "" : "s"} (Ctrl+Z undoes)`);
  };

  // Window listeners call the latest handlers (they read this render's state).
  const dragHandlers = useRef({ move: (_e: MouseEvent) => {}, up: () => {} });
  dragHandlers.current = {
    move: (e) => {
      const d = lineDrag.current;
      if (!d) return;
      if (!(e.buttons & 1)) {
        lineDrag.current = null;
        setDropAt(null);
        return;
      }
      if (!d.active && Math.hypot(e.clientX - d.x, e.clientY - d.y) < 4) return;
      d.active = true;
      const hit = document.elementFromPoint(e.clientX, e.clientY);
      const el = hit instanceof HTMLElement ? hit.closest<HTMLElement>("[data-tc-row]") : null;
      if (!el || !root.current?.contains(el)) return setDropAt(null);
      const side = el.dataset.tcSide === "right" ? "right" : "left";
      const row = Number(el.dataset.tcRow);
      const rect = el.getBoundingClientRect();
      const after = e.clientY > rect.top + rect.height / 2;
      const copy = e.ctrlKey || e.metaKey || e.altKey;
      const cur = dropRef.current;
      if (!cur || cur.side !== side || cur.row !== row || cur.after !== after || cur.copy !== copy)
        setDropAt({ side, row, after, copy });
    },
    up: () => {
      const d = lineDrag.current;
      lineDrag.current = null;
      if (!d) return;
      const drop = dropRef.current;
      setDropAt(null);
      if (!d.active) {
        // A plain click on the selection selects just that line, as elsewhere.
        setAnchor(d.row);
        setCursor({ side: d.side, row: d.row });
      } else if (drop) dropLines(d, drop);
    },
  };
  useEffect(() => {
    const move = (e: MouseEvent) => dragHandlers.current.move(e);
    const up = () => dragHandlers.current.up();
    window.addEventListener("mousemove", move);
    window.addEventListener("mouseup", up);
    return () => {
      window.removeEventListener("mousemove", move);
      window.removeEventListener("mouseup", up);
    };
  }, []);
  const dropCls = (row: number, side: Side) =>
    dropAt && dropAt.side === side && dropAt.row === row ? (dropAt.after ? " tc-drop-after" : " tc-drop-before") : "";
  /** Ctrl+click (⌘+click on macOS) on a line sets a sync point there. */
  const syncClick = (e: ReactMouseEvent, row: number, side: Side) => {
    if (e.button !== 0 || !(isMac ? e.metaKey : e.ctrlKey)) return false;
    e.preventDefault();
    setSyncPoint(side, row);
    return true;
  };

  // araxis:text-view end

  /**
   * Picks a side's encoding: an unedited file is re-read in it; with unsaved edits
   * only the save format changes (the text is converted when saved).
   */
  const changeEncoding = async (side: Side, enc: string | null) => {
    setEncodings((e) => ({ ...e, [side]: enc }));
    const d = docs?.[side];
    if (!d || d.memory) return;
    if (isDirty(d)) {
      if (enc) {
        const format = { ...d.format, encoding: enc.replace(/ BOM$/, ""), hasBom: enc.endsWith(" BOM") };
        setDocs((ds) => ds && { ...ds, [side]: { ...ds[side], format } });
      }
      return;
    }
    try {
      const doc = await textApi.loadText(d.info.path, enc);
      eolMap.current.set(doc.lines, doc.eols);
      setDocs((ds) => ds && { ...ds, [side]: toSide(doc) });
      setHistory({ undo: [], redo: [] });
      setError(null);
    } catch (e) {
      setError(errorMessage(e));
    }
  };

  const changePaths = (l: string, r: string) => {
    if (!confirmDiscard("open the new files")) return;
    // bc5:text-ui — a newly chosen file replaces that side's clipboard text; swapping swaps it too.
    if (memory.left || memory.right) {
      const swapped = l === right && r === left && l !== r;
      setMemory((m) =>
        swapped ? { left: m.right, right: m.left } : { left: l !== left ? null : m.left, right: r !== right ? null : m.right },
      );
    }
    if (l !== left || r !== right) setBookmarks([]);
    onPathsChange(l, r);
  };

  // Re-registered every render so the handler always sees current state.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      // Inactive tabs stay mounted but hidden; only the visible session reacts.
      if (e.defaultPrevented || !root.current?.offsetParent) return;
      if (embedInactive(root.current)) return; // araxis:folder — split view: the folder list has the keys
      if (isComposing(e)) return; // araxis:text-view — keys belong to an open IME composition
      const mod = e.ctrlKey || e.metaKey;
      const key = e.key.toLowerCase();
      const t = e.target as HTMLElement | null;
      const inField = !!t && (t.tagName === "INPUT" || t.tagName === "TEXTAREA" || t.tagName === "SELECT");
      const plain = !mod && !e.altKey;
      if (mod && !e.shiftKey && !e.altKey && key === "s") save();
      // bc5:text-ui — find, bookmarks: also from the line editor and the find bar.
      else if (mod && !e.shiftKey && !e.altKey && key === "f") openFind(false);
      else if (e.ctrlKey && !e.shiftKey && !e.altKey && key === "h") openFind(true);
      else if (e.key === "F3" && !mod && !e.altKey) findStep(e.shiftKey ? -1 : 1);
      else if (e.key === "F2" && mod && e.shiftKey) setBookmarks([]);
      else if (e.key === "F2" && mod) toggleBookmark();
      else if (e.key === "F2" && !e.altKey) gotoBookmark(e.shiftKey ? -1 : 1);
      // araxis:text-view
      else if (keyMatches(e, "text.goToLine")) openGoto();
      else if (keyMatches(e, "text.togglePadding")) togglePadding();
      else if (keyMatches(e, "text.lineEndings")) setShowEol((v) => !v);
      // araxis:text-view end
      else if (inField) return;
      else if (mod && !e.shiftKey && key === "z") undo();
      else if (mod && (key === "y" || (e.shiftKey && key === "z"))) redo();
      else if (mod && !e.shiftKey && !e.altKey && key === "c") {
        // A text selection inside the panes copies as usual; otherwise copy whole lines.
        if (window.getSelection()?.toString() || !copyLinesText()) return;
      }
      // araxis:text-view
      else if (plain && e.key === "Tab" && selRange && cursor && t?.tagName !== "BUTTON") indentSelection(e.shiftKey ? -1 : 1);
      else if (keyMatches(e, "text.setSyncPoint")) cursor && setSyncPoint(cursor.side, cursor.row);
      else if (keyMatches(e, "text.copyAllToRight")) replaceFile("left");
      else if (keyMatches(e, "text.copyAllToLeft")) replaceFile("right");
      else if (keyMatches(e, "text.markResolved")) toggleResolved();
      else if (e.key === "Escape" && syncPick) setSyncPick(null);
      else if (e.key === "Escape" && gotoOpen) setGotoOpen(false);
      // araxis:text-view end
      else if (e.altKey && e.key === "ArrowDown") goHunk(hunkIdx + 1);
      else if (e.altKey && e.key === "ArrowUp") goHunk(hunkIdx - 1);
      else if (e.altKey && e.key === "ArrowRight") copyTo("right");
      else if (e.altKey && e.key === "ArrowLeft") copyTo("left");
      else if (plain && (e.key === "ArrowDown" || e.key === "ArrowUp")) moveCursor(e.key === "ArrowDown" ? 1 : -1, e.shiftKey);
      else if (plain && !e.shiftKey && cursor && (e.key === "ArrowLeft" || e.key === "ArrowRight")) {
        setCursor({ ...cursor, side: e.key === "ArrowLeft" ? "left" : "right" });
        setAnchor(null);
      } else if (plain && !e.shiftKey && e.key === "Enter" && cursor && t?.tagName !== "BUTTON")
        startEdit(cursor.row, cursor.side);
      else if (plain && e.key === "Delete" && selRange) deleteLines(selRange);
      else if (e.key === "Escape" && anchor !== null) setAnchor(null);
      else if (e.key === "Escape" && findOpen) setFindOpen(false);
      else if (e.key === "F5") refresh();
      else return;
      e.preventDefault();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  });

  // Line selection by dragging over the line numbers.
  const dragSide = useRef<Side | null>(null);
  useEffect(() => {
    const up = () => (dragSide.current = null);
    window.addEventListener("mouseup", up);
    return () => window.removeEventListener("mouseup", up);
  }, []);

  const onWheel = (e: WheelEvent<HTMLDivElement>) => {
    if (wordWrap) return;
    const dx = e.shiftKey ? e.deltaY : e.deltaX;
    if (!dx || (!e.shiftKey && Math.abs(e.deltaX) < Math.abs(e.deltaY))) return;
    setHx((x) => Math.max(0, Math.min(maxChars * metrics.charW, x + dx)));
  };

  const shift = { transform: `translateX(${-hx}px)` };

  /** Click (caret), Shift+click (extend the selection) on a cell or a line number. */
  const pointAt = (e: ReactMouseEvent, row: number, side: Side, selectLine: boolean) => {
    if (e.button !== 0) return;
    if (e.shiftKey && cursor?.side === side) {
      e.preventDefault();
      setAnchor(anchor ?? cursor.row);
    } else setAnchor(selectLine ? row : null);
    setCursor({ side, row });
    if (selectLine) {
      e.preventDefault();
      dragSide.current = side;
    }
  };

  const openMenu = (e: ReactMouseEvent, row: number, side: Side) => {
    e.preventDefault();
    const inSelection = !!selRange && cursor?.side === side && row >= selRange[0] && row <= selRange[1];
    if (!inSelection) {
      setCursor({ side, row });
      setAnchor(null);
    }
    if (rowHunk[row] >= 0) setHunkIdx(rowHunk[row]);
    menu.open(e, { side, row });
  };

  /** One side of a row: diff marks, syntax colours and find hits layered into runs, wrapped if needed. */
  const renderContent = (row: DiffRow, side: Side, segs: Segment[]) => {
    const text = segText(segs);
    const n = lineNo(row, side);
    const layers: StyledRange[][] = [segmentRanges(segs, "seg-changed")];
    const lng = syntaxOn ? langs[side] : null;
    if (syntaxOn && text.length <= MAX_SYNTAX_LINE) {
      const spans = side === "left" ? row.leftSpans : row.rightSpans;
      if (spans?.length) layers.push(spans.map((t) => ({ start: t.start, end: t.end, cls: `syn-${ENGINE_KIND[t.kind]}` })));
      else if (lng && n != null) {
        const state = lineStateBySide?.[side]?.[n - 1] ?? 0;
        layers.push(lineTokens(text, lng, state).map((t) => ({ start: t.start, end: t.end, cls: `syn-${t.kind}` })));
      }
    }
    const hits = n != null ? hitRanges.get(`${side}:${n - 1}`) : undefined;
    if (hits) layers.push(hits);
    const runs = composeRuns(text, layers);
    // araxis:text-view — dimmed line-ending glyph after the text.
    const eol = n != null ? eolBySide?.[side]?.[n - 1] : undefined;
    const eolMark = eol && EOL_GLYPH[eol] && (
      <span className="tc-eol" title={eol.toUpperCase()}>
        {EOL_GLYPH[eol]}
      </span>
    );
    // araxis:text-view end
    if (wordWrap) {
      const vlines = wrapRuns(runs, metrics.cols, TAB_SIZE);
      return vlines.map((line, i) => (
        <div key={i} className="tc-vline">
          {line.map(renderRun)}
          {i === vlines.length - 1 && eolMark}
        </div>
      ));
    }
    return (
      <div className="tcontent" style={shift}>
        {runs.map(renderRun)}
        {eolMark}
      </div>
    );
  };

  const renderCell = (rowIdx: number, row: DiffRow, side: Side) => {
    const segs = side === "left" ? row.left : row.right;
    const selected = !!selRange && cursor?.side === side && rowIdx >= selRange[0] && rowIdx <= selRange[1];
    const caret = cursor?.side === side && cursor.row === rowIdx;
    // araxis:text-view — resolved lines are dimmed; drop target for dragged lines; Ctrl+click sets a sync point.
    const n = lineNo(row, side);
    const done = n != null && resolvedKeys.has(`${side}:${n}`);
    return (
      <div
        className={`tcell${segs ? "" : " absent"}${selected ? " tc-sel" : ""}${caret ? " tc-caret" : ""}${
          done ? " tc-resolved" : ""
        }${dropCls(rowIdx, side)}`}
        data-tc-row={rowIdx}
        data-tc-side={side}
        onMouseDown={(e) => syncClick(e, rowIdx, side) || pointAt(e, rowIdx, side, false)}
        onDoubleClick={(e) => startEdit(rowIdx, side, segs ? caretFromPoint(e, segText(segs)) : undefined)}
        onContextMenu={(e) => openMenu(e, rowIdx, side)}
        title={readOnly[side] ? READ_ONLY_HINT : undefined}
      >
        {segs && renderContent(row, side, segs)}
      </div>
    );
  };

  const gutterButton = (to: Side, title: string, onClick: () => void, selection = false) => (
    <button
      key={to}
      className={`tc-gutter-btn${selection ? " sel" : ""}`}
      disabled={!fresh || readOnly[to]}
      title={readOnly[to] ? READ_ONLY_HINT : title}
      onMouseDown={(e) => e.stopPropagation()}
      onClick={(e) => {
        e.stopPropagation();
        onClick();
      }}
    >
      {selection ? (
        to === "right" ? <ChevronsRight size={13} /> : <ChevronsLeft size={13} />
      ) : to === "right" ? (
        <ChevronRight size={13} />
      ) : (
        <ChevronLeft size={13} />
      )}
    </button>
  );

  /**
   * Adaptive copy buttons in the middle gutter: on the first row of a line
   * selection, one button copying just those lines away from their side;
   * otherwise on the hovered or current difference, pointing away from the
   * caret's side when the caret is in it (both ways when it is not).
   */
  const renderGutter = (rowIdx: number, h: number) => {
    if (selRange && cursor && rowIdx === selRange[0]) {
      const to = other(cursor.side);
      const n = selRange[1] - selRange[0] + 1;
      return gutterButton(
        to,
        `Copy the ${n} selected line${n === 1 ? "" : "s"} to the ${to} (Alt+${to === "right" ? "→" : "←"})`,
        () => copySelection(cursor.side, selRange),
        true,
      );
    }
    if (h < 0 || hunks[h].startRow !== rowIdx || (h !== hunkIdx && h !== hoverHunk)) return null;
    if (selRange && selRange[0] < hunks[h].endRow && selRange[1] >= hunks[h].startRow) return null;
    const from = cursor && rowHunk[cursor.row] === h ? cursor.side : null;
    const targets = (from ? [other(from)] : (["right", "left"] as Side[])).filter((to) => !readOnly[to]);
    return targets.map((to) =>
      gutterButton(to, `Copy this difference to the ${to} (Alt+${to === "right" ? "→" : "←"})`, () =>
        copyHunk(h, other(to)),
      ),
    );
  };

  const renderLineNo = (rowIdx: number, row: DiffRow, side: Side) => {
    const n = lineNo(row, side);
    const marked = n != null && bookmarkKeys.has(`${side}:${n}`);
    // araxis:text-view — edit markers, sync points, dragging a selection by its line numbers.
    const edit = n != null && editedKeys.has(`${side}:${n}`);
    const synced = n != null && syncLines[side].has(n);
    const picked = n != null && syncPick?.side === side && syncPick.line === n;
    const inSel = !!selRange && cursor?.side === side && rowIdx >= selRange[0] && rowIdx <= selRange[1];
    const hints = [
      marked ? "Bookmark (F2 / Shift+F2 to jump, Ctrl+F2 to remove)" : "Click or drag to select lines",
      review.comment(side, n) ?? "", // araxis:text-review
      edit ? "edited since the last save" : "",
      synced ? "sync point (right-click to remove)" : "",
      inSel ? "drag the selection to move it, or onto the other side to copy it" : "",
    ];
    return (
      <span
        className={`ln${marked ? " tc-bm" : ""}${edit ? " tc-edited" : ""}${synced ? " tc-syncln" : ""}${
          picked ? " tc-syncpick" : ""
        }${inSel && fresh ? " tc-draggable" : ""}${dropCls(rowIdx, side)}`}
        title={hints.filter(Boolean).join(" · ")}
        data-tc-row={rowIdx}
        data-tc-side={side}
        onMouseDown={(e) => {
          if (syncClick(e, rowIdx, side)) return;
          if (e.button === 0 && !e.shiftKey && inSel && fresh && selRange) {
            e.preventDefault();
            lineDrag.current = { side, range: selRange, row: rowIdx, x: e.clientX, y: e.clientY, active: false };
            return;
          }
          pointAt(e, rowIdx, side, true);
        }}
        onContextMenu={(e) => openMenu(e, rowIdx, side)}
      >
        {n ?? ""}
        {review.mark(side, n) /* araxis:text-review */}
      </span>
    );
  };

  const renderRow = (i: number) => {
    const item = display[i];
    if ("gap" in item) {
      return <div className="tgap">⋯ {item.gap.toLocaleString()} identical lines hidden ⋯</div>;
    }
    const row = rows[item.row];
    const h = rowHunk[item.row];
    const inHunk = h >= 0 && h === hunkIdx;
    return (
      <div
        className={`trow k-${row.kind}${inHunk ? " current" : ""}`}
        onClick={() => h >= 0 && h !== hunkIdx && setHunkIdx(h)}
        onMouseEnter={(e) => {
          setHoverHunk(h);
          const side = dragSide.current;
          if (side && e.buttons & 1) setCursor({ side, row: item.row });
        }}
      >
        {renderLineNo(item.row, row, "left")}
        {renderCell(item.row, row, "left")}
        <span
          className={`mid${
            row.leftNo != null && row.rightNo != null && syncPairs.has(`${row.leftNo}:${row.rightNo}`) ? " tc-sync" : ""
          }`} /* araxis:text-view — sync point */
        >
          {renderGutter(item.row, h)}
        </span>
        {renderLineNo(item.row, row, "right")}
        {renderCell(item.row, row, "right")}
      </div>
    );
  };

  /** Where the line editor floats: the edited row, or its last known spot while the re-compare runs. */
  const editorPos = useRef<{ top: number; height: number } | null>(null);
  const renderEditor = () => {
    if (!editor || !docs) {
      editorPos.current = null;
      return null;
    }
    const d = editorRow >= 0 ? rowToDisplay[editorRow] : -1;
    if (d >= 0) {
      editorPos.current = offsets
        ? { top: offsets[d], height: offsets[d + 1] - offsets[d] }
        : { top: d * ROW_HEIGHT, height: ROW_HEIGHT };
    }
    const pos = editorPos.current;
    if (!pos) return null;
    return (
      <div className="vrow tc-editor-layer" style={{ top: pos.top, height: pos.height }}>
        <div className="trow">{editorArea(editor.side === "left" ? 2 : 5)}</div>
      </div>
    );
  };

  /** The line editor's textarea, in grid column `column` of its row. */
  const editorArea = (column: number) =>
    editor &&
    docs && (
      <textarea
        ref={editorEl}
        className="tc-edit-area"
        style={{ gridColumn: column }}
        wrap={wordWrap ? "soft" : "off"}
        spellCheck={false}
        rows={1}
        value={docs[editor.side].lines[editor.line] ?? ""}
        onChange={onEditorChange}
        onKeyDown={onEditorKey}
        onPaste={onEditorPaste}
        onSelect={(e) => setCaretIdx(e.currentTarget.selectionStart)} // araxis:text-view — Ln/Col
        // Switching windows keeps the editor; clicking elsewhere in the app closes it.
        onBlur={() => document.hasFocus() && setEditor(null)}
      />
    );

  // ---------- araxis:text-view: the linked (padding off) view ----------

  /** Padding off: the line editor floats over its line in that side's pane. */
  const linkedEditorPos = useRef<{ side: Side; index: number } | null>(null);
  const linkedOverlay = () => {
    if (!linked || !editor || !docs) {
      linkedEditorPos.current = null;
      return null;
    }
    const d = editorRow >= 0 ? rowToDisplay[editorRow] : -1;
    const index = d >= 0 ? linked.pos[editor.side][d] : -1;
    if (index >= 0) linkedEditorPos.current = { side: editor.side, index };
    const pos = linkedEditorPos.current;
    if (!pos || pos.side !== editor.side) return null;
    return { ...pos, node: <div className="trow tc-prow">{editorArea(2)}</div> };
  };

  /** One side's row in the linked view: its line number and text, or a hidden-lines gap. */
  const renderSide = (side: Side, d: number) => {
    const item = display[d];
    if ("gap" in item) return <div className="tgap">⋯ {item.gap.toLocaleString()} identical lines hidden ⋯</div>;
    const row = rows[item.row];
    const h = rowHunk[item.row];
    return (
      <div
        className={`trow tc-prow k-${row.kind}${h >= 0 && h === hunkIdx ? " current" : ""}`}
        onClick={() => h >= 0 && h !== hunkIdx && setHunkIdx(h)}
        onMouseEnter={(e) => {
          setHoverHunk(h);
          if (dragSide.current === side && e.buttons & 1) setCursor({ side, row: item.row });
        }}
      >
        {renderLineNo(item.row, row, side)}
        {renderCell(item.row, row, side)}
      </div>
    );
  };

  /** Copy buttons on the current difference's band. */
  const renderLinkButtons = (h: number) =>
    (["right", "left"] as Side[])
      .filter((to) => !readOnly[to])
      .map((to) =>
        gutterButton(to, `Copy this difference to the ${to} (Alt+${to === "right" ? "→" : "←"})`, () => copyHunk(h, other(to))),
      );
  // araxis:text-view end

  /** Per-side "Save", "Clipboard" button, plus the way back to the file while clipboard text is shown. */
  const clipControls = (side: Side) => (
    <span className="tc-clip">
      {!memory[side] && !readOnly[side] && (
        <button
          className="tc-mini"
          onClick={() => save(side)}
          disabled={!sideSavable(side) || saving}
          title={`Save the ${side} file only`}
        >
          <Save size={11} /> Save
        </button>
      )}
      {memory[side] && (
        <button
          className="tc-mini"
          onClick={() => closeMemory(side)}
          title={(side === "left" ? left : right) ? "Compare the file again" : "Close the clipboard text"}
        >
          <X size={11} /> {memory[side]?.label ?? CLIPBOARD_LABEL /* bc5:editor-patch */}
        </button>
      )}
      <button
        className="tc-mini"
        onClick={() => loadClipboard(side)}
        title={`Compare the clipboard's text on the ${side} (in memory; edits are not saved)`}
      >
        <ClipboardPaste size={11} /> {memory[side] ? "Reload" : "Clipboard"}
      </button>
    </span>
  );

  /** The line pair Line Details shows: the caret row, else the current difference. */
  const detailRow = cursor?.row ?? (hunkIdx >= 0 && hunkIdx < hunks.length ? hunks[hunkIdx].startRow : -1);
  const detail = rows[detailRow];

  /** Whether `side` has any lines in the rows of `range`. */
  const hasLines = (range: [number, number] | null, side: Side) => {
    if (!range) return false;
    const span = lineSpan(rows, range[0], range[1] + 1, side);
    return span.end > span.start;
  };

  /** A context-menu entry; `hint` is its shortcut text. */
  const menuItem = (label: string, hint: string | null, disabled: boolean, act: () => void): MenuEntry => ({
    label,
    shortcut: hint,
    disabled,
    onClick: act,
  });

  const stats = diff?.stats;
  const canSave = SIDES.some(sideSavable);
  const allReadOnly = readOnly.left && readOnly.right;
  const hasHunk = fresh && hunkIdx >= 0 && hunkIdx < hunks.length;
  // araxis:text-view — Ln/Col of the caret (the line editor's caret, else the caret line).
  const sideName = (s: Side) => (s === "left" ? "Left" : "Right");
  const caretLine = editor ? editor.line + 1 : cursor && rows[cursor.row] ? lineNo(rows[cursor.row], cursor.side) : null;
  const lnCol =
    editor && docs
      ? `${sideName(editor.side)} · Ln ${editor.line + 1}, Col ${columnOf(docs[editor.side].lines[editor.line] ?? "", caretIdx, TAB_SIZE) + 1}`
      : cursor && caretLine != null
        ? `${sideName(cursor.side)} · Ln ${caretLine}, Col 1`
        : null;
  const syncHint = isMac ? "⌘+click" : "Ctrl+click";
  // araxis:text-view end
  return (
    <div className="session tc-editable" ref={root}>
      <div className="toolbar">
        <button onClick={() => goHunk(hunkIdx - 1)} disabled={!hunks.length} title="Previous difference (Alt+↑)">
          <ArrowUp size={14} /> Prev
        </button>
        <button onClick={() => goHunk(hunkIdx + 1)} disabled={!hunks.length} title="Next difference (Alt+↓)">
          <ArrowDown size={14} /> Next
        </button>
        <button
          className="icon-btn"
          onClick={() => copyTo("right")}
          disabled={!(hasHunk || (fresh && selRange && cursor?.side === "left")) || readOnly.right}
          title={
            readOnly.right
              ? READ_ONLY_HINT
              : "Copy the selected left lines, or else the current difference, to the right (Alt+→)"
          }
        >
          <ArrowRight size={14} />
        </button>
        <button
          className="icon-btn"
          onClick={() => copyTo("left")}
          disabled={!(hasHunk || (fresh && selRange && cursor?.side === "right")) || readOnly.left}
          title={
            readOnly.left ? READ_ONLY_HINT : "Copy the selected right lines, or else the current difference, to the left (Alt+←)"
          }
        >
          <ArrowLeft size={14} />
        </button>
        <span className="sep" />
        <div className="seg-control">
          {(["all", "context", "diffs"] as ViewMode[]).map((m) => (
            <button key={m} className={mode === m ? "active" : ""} onClick={() => setMode(m)}>
              {m === "all" ? "All" : m === "context" ? "Context" : "Diffs"}
            </button>
          ))}
        </div>
        <span className="sep" />
        <label>
          Whitespace
          <select
            value={options.whitespace}
            onChange={(e) => setOptions({ ...options, whitespace: e.target.value as WhitespaceMode })}
          >
            <option value="compare">Compare</option>
            <option value="ignoreTrailing">Ignore trailing</option>
            <option value="ignoreChanges">Ignore changes</option>
            <option value="ignoreAll">Ignore all</option>
          </select>
        </label>
        <label className="check">
          <input
            type="checkbox"
            checked={options.ignoreCase}
            onChange={(e) => setOptions({ ...options, ignoreCase: e.target.checked })}
          />
          Ignore case
        </label>
        <label>
          Algorithm
          <select
            value={options.algorithm}
            onChange={(e) => setOptions({ ...options, algorithm: e.target.value as TextDiffOptions["algorithm"] })}
          >
            <option value="patience">Patience</option>
            <option value="myers">Myers</option>
          </select>
        </label>
        {/* bc5:text-engine — rules dialog and manual "unimportant" marks */}
        <button
          onClick={() => setEditingRules(true)}
          title="Importance (grammar comments and strings, regexes, columns, page headers, line endings), replacements, alignment and backups"
        >
          <SlidersHorizontal size={14} /> Rules{ruleCount ? ` · ${ruleCount}` : ""}
        </button>
        <button
          className="icon-btn"
          onClick={markUnimportant}
          disabled={!fresh || (!cursor && !(hunkIdx >= 0 && hunkIdx < hunks.length))}
          title="Mark the selected lines (or the current difference) as unimportant"
        >
          <EyeOff size={14} />
        </button>
        {/* bc5:converters — formatted HTML view */}
        {(isHtmlPath(left) || isHtmlPath(right)) && (
          <button
            className={`icon-btn${htmlView ? " active" : ""}`}
            onClick={() => setHtmlView((v) => !v)}
            disabled={!docs || binary}
            title="Rendered view: both pages rendered side by side (no scripts or network)"
          >
            <AppWindow size={14} />
          </button>
        )}
        <span className="spacer" />
        {/* bc5:text-ui — find, view toggles, bookmarks */}
        <button
          className={`icon-btn${findOpen ? " active" : ""}`}
          onClick={() => (findOpen ? setFindOpen(false) : openFind(false))}
          title="Find (Ctrl+F, F3 / Shift+F3) · Replace (Ctrl+H)"
        >
          <Search size={14} />
        </button>
        <button
          className={`icon-btn${wordWrapOn ? " active" : ""}`}
          onClick={() => setWordWrap((w) => !w)}
          disabled={!padding /* araxis:text-view */}
          title={padding ? "Word wrap (both sides stay aligned)" : "Word wrap needs vertical padding on"}
        >
          <TextWrap size={14} />
        </button>
        <button
          className={`icon-btn${syntaxOn ? " active" : ""}`}
          onClick={() => setSyntaxOn((s) => !s)}
          title={`Syntax highlighting${langs.left || langs.right ? ` (${(langs.left ?? langs.right)!.name})` : " (no language recognised for these files)"}`}
        >
          <Code size={14} />
        </button>
        <button
          className={`icon-btn${showDetails ? " active" : ""}`}
          onClick={() => setShowDetails((s) => !s)}
          title="Line details: the current line pair aligned character by character, as text and hex"
        >
          <PanelBottom size={14} />
        </button>
        <button
          className="icon-btn"
          onClick={toggleBookmark}
          disabled={!cursor}
          title={`Toggle bookmark on the current line (Ctrl+F2) · next / previous: F2 / Shift+F2 · clear all: Ctrl+Shift+F2${
            bookmarks.length ? ` · ${bookmarks.length} set` : ""
          }`}
        >
          <Bookmark size={14} />
        </button>
        {review.toolbar /* araxis:text-review */}
        {/* araxis:text-view — vertical padding (off = linked lines), line endings, more commands */}
        <button
          className={`icon-btn${padding ? " active" : ""}`}
          onClick={togglePadding}
          title={`Vertical padding ${padding ? "on" : "off"}: ${
            padding
              ? "blank rows keep both sides aligned; turn off to show only real lines, linked across the middle"
              : "each side shows only its own lines; the middle links each difference across"
          }${shortcutLabel("text.togglePadding") ? ` (${shortcutLabel("text.togglePadding")})` : ""}`}
        >
          <Rows3 size={14} />
        </button>
        <button
          className={`icon-btn${showEol ? " active" : ""}`}
          onClick={() => setShowEol((v) => !v)}
          title="Show line endings (↵ LF, ␍␊ CRLF, ␍ CR)"
        >
          <Pilcrow size={14} />
        </button>
        <button
          className={`icon-btn${toolsMenu.isOpen ? " active" : ""}`}
          onClick={(e) => {
            e.stopPropagation();
            const r = e.currentTarget.getBoundingClientRect();
            toolsMenu.open({ clientX: r.left, clientY: r.bottom + 2 }, null);
          }}
          title="More: go to line, copy all, sync points, resolved lines, indent"
        >
          <Ellipsis size={14} />
        </button>
        {/* araxis:text-view end */}
        <span className="sep" />
        <button className="icon-btn" onClick={undo} disabled={!history.undo.length} title="Undo (Ctrl+Z)">
          <Undo2 size={14} />
        </button>
        <button
          className="icon-btn"
          onClick={redo}
          disabled={!history.redo.length}
          title="Redo (Ctrl+Y or Ctrl+Shift+Z)"
        >
          <Redo2 size={14} />
        </button>
        <button
          onClick={() => save()}
          disabled={!canSave || saving}
          title={allReadOnly ? READ_ONLY_HINT : "Save all modified files (Ctrl+S)"}
        >
          <Save size={14} /> {saving ? "Saving…" : "Save"}
        </button>
        <label
          className={`check${watchState === "watching" || watchState === "polling" ? " live" : ""}`}
          title={`${autoRefreshTitle(watchState)}; unsaved edits are never overwritten`}
        >
          <input type="checkbox" checked={autoRefresh} onChange={(e) => setAutoRefresh(e.target.checked)} />
          Auto-refresh
        </label>
        {/* bc5:reports-script */}
        <button
          onClick={() => setReporting(true)}
          disabled={!docs || binary}
          title="Report: save, copy or print this comparison as HTML, plain text or a unified patch"
        >
          <FileText size={14} /> Report…
        </button>
        <button onClick={refresh} title="Refresh (F5)">
          <RefreshCw size={14} /> Refresh
        </button>
      </div>
      {/* bc5:reports-script — reports diff the text as shown, unsaved edits included. */}
      {reporting && docs && (
        <ReportDialog
          source={{
            kind: "text",
            left: { ...engineSide("left", docs.left.lines), title: docs.left.info.path },
            right: { ...engineSide("right", docs.right.lines), title: docs.right.info.path },
            rules: options,
          }}
          onClose={() => setReporting(false)}
          bookmarks={bookmarks} /* araxis:reports araxis:reports end */
        />
      )}
      {editingRules && (
        <TextRulesDialog
          options={options}
          detected={diff ? { left: diff.leftGrammar ?? null, right: diff.rightGrammar ?? null } : undefined}
          lineCounts={docs ? { left: docs.left.lines.length, right: docs.right.lines.length } : undefined}
          onClose={() => setEditingRules(false)}
          onApply={setOptions}
          onOpenFileFormats={() => (setEditingRules(false), setFileFormatsOpen(true))} // bc5:converters
        />
      )}
      {/* bc5:converters */}
      {fileFormatsOpen && <FileFormatsDialog onClose={() => setFileFormatsOpen(false)} onSaved={() => load()} />}

      <PathBar
        left={left}
        right={right}
        directory={false}
        onCompare={changePaths}
        // bc5:text-ui — with clipboard text on one side, a single chosen file is enough.
        oneSided={!!(memory.left || memory.right)}
      />

      {docs && (
        <div className="infobar">
          {SIDES.map((side) => (
            // bc5:text-engine — per-side encoding picker
            <span key={side} className="tc-side-info">
              <FileInfo doc={docs[side]} readOnly={readOnly[side]} />
              {!docs[side].binary && !docs[side].memory && !docs[side].conversion?.convertedFrom /* bc5:converters */ && (
                <TextEncodingPicker
                  value={encodings[side]}
                  detected={docs[side].info.encoding}
                  hasBom={docs[side].info.hasBom}
                  guessed={!!(docs[side] as Partial<{ encodingGuessed: boolean }>).encodingGuessed}
                  dirty={isDirty(docs[side])}
                  onChange={(enc) => changeEncoding(side, enc)}
                />
              )}
              {/* bc5:text-ui — compare clipboard text on this side */}
              {clipControls(side)}
            </span>
          ))}
        </div>
      )}
      {findOpen && (
        <FindBar
          query={query}
          onQuery={setQuery}
          replacement={replacement}
          onReplacement={setReplacement}
          options={findOpts}
          onOptions={setFindOpts}
          scope={findScope}
          onScope={setFindScope}
          replaceMode={replaceMode}
          onReplaceMode={setReplaceMode}
          count={matches.length}
          current={findIdx}
          error={compiled.error}
          replaceBlocked={replaceBlocked}
          focusSignal={findFocus}
          onNext={() => findStep(1)}
          onPrev={() => findStep(-1)}
          onReplace={replaceCurrent}
          onReplaceAll={replaceAll}
          onClose={() => setFindOpen(false)}
        />
      )}
      {/* araxis:text-view — Go to line (Ctrl+G) */}
      {gotoOpen && docs && (
        <div className="tc-goto">
          <Hash size={13} />
          <span>Go to line</span>
          <select value={gotoSide} onChange={(e) => setGotoSide(e.target.value === "right" ? "right" : "left")}>
            <option value="left">Left</option>
            <option value="right">Right</option>
          </select>
          <input
            autoFocus
            inputMode="numeric"
            value={gotoText}
            onChange={(e) => setGotoText(e.target.value)}
            onFocus={(e) => e.currentTarget.select()}
            onKeyDown={(e) => {
              if (isComposing(e)) return;
              if (e.key === "Enter") gotoLine();
              else if (e.key === "Escape") setGotoOpen(false);
              else return;
              e.preventDefault();
            }}
            placeholder={`1–${docs[gotoSide].lines.length.toLocaleString()}`}
          />
          <button onClick={gotoLine}>Go</button>
          <button className="icon-btn" onClick={() => setGotoOpen(false)} title="Close (Esc)">
            <X size={12} />
          </button>
        </div>
      )}
      {/* araxis:text-view end */}

      <div className="session-body">
        {review.layer /* araxis:text-review — Bookmarks & comments panel */}
        {review.dialogs /* araxis:text-review */}
        {(loading || (docs && !binary && !diffed && !error && !diffError)) && (
          <div className="overlay">Comparing…</div>
        )}
        {error && <div className="banner error">{error}</div>}
        {diffError && <div className="banner error">{diffError}</div>}
        {/* bc5:converters — converted / read-only sides */}
        {docs && (
          <ConversionBanner
            conversions={{ left: docs.left.conversion, right: docs.right.conversion }}
            onConfigure={() => setFileFormatsOpen(true)}
            onOpenHex={onOpenHex}
          />
        )}
        {docs && !binary && htmlView && (isHtmlPath(left) || isHtmlPath(right)) && (
          <HtmlRenderedView
            left={{ path: left, lines: docs.left.lines, converted: !!docs.left.conversion?.convertedFrom }}
            right={{ path: right, lines: docs.right.lines, converted: !!docs.right.conversion?.convertedFrom }}
          />
        )}
        {binary && (
          <div className="empty-state">
            <p>One or both files look binary.</p>
            <button className="primary" onClick={onOpenHex}>
              <Binary size={14} /> Open in Hex Compare
            </button>
          </div>
        )}
        {/* bc5:text-ui — nothing to compare yet: offer the clipboard */}
        {!docs && !loading && !error && (
          <div className="empty-state">
            <p>Choose two files above, or compare text from the clipboard.</p>
            <div className="tc-empty-actions">
              {SIDES.map((side) => (
                <button key={side} onClick={() => loadClipboard(side)}>
                  <ClipboardPaste size={14} /> {memory[side] ? "Reload clipboard" : "Clipboard"} on the {side}
                </button>
              ))}
              {/* bc5:editor-patch — one file chosen: edit it on its own instead */}
              {onOpenEditor && !!(left || right) && !(left && right) && !isRemote(left || right) && (
                <button onClick={() => onOpenEditor(left || right)} title={left || right}>
                  <FilePen size={14} /> Open in Text Edit
                </button>
              )}
            </div>
          </div>
        )}
        {diff && !(htmlView && (isHtmlPath(left) || isHtmlPath(right))) /* bc5:converters */ && (
          <div className="text-grid" ref={gridRef} onMouseLeave={() => setHoverHunk(-1)}>
            <span className="tc-probe" ref={probeRef} aria-hidden>
              {"0".repeat(PROBE_CHARS)}
            </span>
            <Overview
              rows={rows}
              hunks={hunks}
              marks={bookmarkRows}
              edits={editRows} // araxis:text-view
              onJump={(row) => list.current?.scrollToRow(rowToDisplay[row])}
            />
            {/* araxis:text-view — padding off: linked lines */}
            {linked ? (
              <TextLinkedView
                ref={list}
                className={`text-list tc-linked${dropAt ? " tc-dragging" : ""}`}
                layout={linked}
                rowHeight={ROW_HEIGHT}
                midWidth={MID_WIDTH}
                renderSide={renderSide}
                currentHunk={hunkIdx}
                renderLinkButtons={renderLinkButtons}
                onPickHunk={setHunkIdx}
                syncLinks={syncLinks}
                overlay={linkedOverlay()}
                onWheel={onWheel}
              />
            ) : (
              <VarList
                ref={list}
                className={`text-list${wordWrap ? " tc-wrap" : ""}${dropAt ? " tc-dragging" : ""}`}
                count={display.length}
                rowHeight={ROW_HEIGHT}
                offsets={offsets}
                renderRow={renderRow}
                overlay={renderEditor()}
                onWheel={onWheel}
              />
            )}
            {/* araxis:text-view end */}
          </div>
        )}
        {diff && showDetails && (
          <LineDetails
            left={detail?.left ? segText(detail.left) : null}
            right={detail?.right ? segText(detail.right) : null}
            leftNo={detail?.leftNo ?? null}
            rightNo={detail?.rightNo ?? null}
            ignoreCase={options.ignoreCase}
            onClose={() => setShowDetails(false)}
          />
        )}
      </div>

      {menu.element}
      {toolsMenu.element /* araxis:text-view */}

      <div className="statusbar">
        {diff &&
          (diff.identical ? (
            <span className="ok">{dirty ? "Texts are identical" : "Files are identical"}</span>
          ) : (
            <>
              <span>
                {hunks.length} difference{hunks.length === 1 ? "" : "s"}
                {hunkIdx >= 0 && hunkIdx < hunks.length ? ` (at ${hunkIdx + 1})` : ""}
              </span>
              <span className="k-changed-fg">{stats!.changed} changed</span>
              <span className="k-leftOnly-fg">{stats!.leftOnly} left only</span>
              <span className="k-rightOnly-fg">{stats!.rightOnly} right only</span>
              {stats!.unimportant > 0 && <span className="k-unimportant-fg">{stats!.unimportant} unimportant</span>}
            </>
          ))}
        {staleOnDisk && (
          <span className="tc-stale" title="Your unsaved edits were kept; Refresh discards them and reloads">
            Changed on disk
          </span>
        )}
        {notice && <span className="tc-notice">{notice}</span>}
        {/* araxis:text-view — pending sync point */}
        {syncPick && (
          <span className="tc-syncwait">
            <Link2 size={11} /> Sync point from {syncPick.side} line {syncPick.line}: {syncHint} or right-click a line on the{" "}
            {other(syncPick.side)} (Esc cancels)
          </span>
        )}
        {docs && !binary && (
          <span className="tc-hint">
            {allReadOnly
              ? "Read-only"
              : editor
                ? "Enter splits · Backspace/Delete at the ends join lines · Esc stops editing"
                : "Double-click or Enter to edit · drag line numbers to select · right-click for more"}
          </span>
        )}
        {/* araxis:text-view */}
        {lnCol && (
          <span className="tc-lncol" title={`Caret position (${shortcutLabel("text.goToLine") || "Ctrl+G"}: go to line)`}>
            {lnCol}
          </span>
        )}
        {/* araxis:text-view end */}
      </div>
    </div>
  );
}

/** Thumbnail strip showing where differences (and bookmarks) sit in the whole file; click to jump. */
function Overview({
  rows,
  hunks,
  marks,
  edits = [],
  onJump,
}: {
  rows: DiffRow[];
  hunks: Hunk[];
  /** Bookmarked rows. */
  marks: number[];
  /** araxis:text-view — rows with edited lines. */
  edits?: number[];
  onJump: (row: number) => void;
}) {
  const total = Math.max(rows.length, 1);
  return (
    <div
      className="overview"
      onClick={(e) => {
        const rect = e.currentTarget.getBoundingClientRect();
        onJump(Math.floor(((e.clientY - rect.top) / rect.height) * total));
      }}
    >
      {hunks.map((h, i) => (
        <div
          key={i}
          className={`ov-mark k-${rows[h.startRow].kind}`}
          style={{
            top: `${(h.startRow / total) * 100}%`,
            height: `max(2px, ${((h.endRow - h.startRow) / total) * 100}%)`,
          }}
        />
      ))}
      {marks.map((row) => (
        <div key={`bm${row}`} className="ov-bm" style={{ top: `${(row / total) * 100}%` }} />
      ))}
      {/* araxis:text-view — edit markers */}
      {edits.map((row) => (
        <div key={`ed${row}`} className="ov-edit" style={{ top: `${(row / total) * 100}%` }} />
      ))}
    </div>
  );
}
