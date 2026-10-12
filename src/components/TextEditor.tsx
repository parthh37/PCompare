// Text Edit: a standalone editor for one text file (Beyond Compare's "Text
// Edit" session). It is line based like the Text Compare panes: the file is a
// virtualized list of lines (VarList), and a single floating textarea edits
// the caret line, so files of 100k+ lines stay responsive. Structural edits
// (split, join, paste, delete, indent, move) go through textEdit.ts; loading
// and saving reuse the Text Compare engine commands, so encodings, BOMs, line
// endings and backups behave exactly as they do there.
import { save as saveDialog } from "@tauri-apps/plugin-dialog";
import {
  Bookmark,
  Code,
  CornerDownRight,
  Hash,
  Lock,
  Redo2,
  RefreshCw,
  Save,
  SaveAll,
  Search,
  TextWrap,
  Undo2,
} from "lucide-react";
import {
  useCallback,
  useDeferredValue,
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
  type ChangeEvent,
  type ClipboardEvent as ReactClipboardEvent,
  type KeyboardEvent as ReactKeyboardEvent,
  type MouseEvent as ReactMouseEvent,
} from "react";
import { errorMessage, isRemote, type LineEnding, type TextFileInfo } from "../lib/api";
import { writeClipboardText } from "../lib/clipboard";
import {
  compileQuery,
  DEFAULT_FIND_OPTIONS,
  findInLines,
  replaceAllInLines,
  replaceAt,
  type FindOptions,
  type LineMatch,
} from "../lib/find";
import { basename, formatSize } from "../lib/format";
import { eventBinding, matches as keyMatches, shortcutLabel } from "../lib/keymap";
import { joinLines, type TextFormat } from "../lib/mergeApi";
import { languageFor, lineStates, lineTokens } from "../lib/syntax";
import { loadBackups, textApi, type TextDocument } from "../lib/textApi";
import {
  columnOf,
  indexAtColumn,
  insertAt,
  joinWithNext,
  joinWithPrevious,
  replaceLines,
  shiftBookmarks,
  splitText,
  type EditResult,
} from "../lib/textEdit";
import { composeRuns, displayWidth, visualLines, wrapRuns, type Run, type StyledRange } from "../lib/textRuns";
import { FindBar } from "./FindBar";
import { PathField } from "./PathField";
import { TextEncodingPicker } from "./TextEncodingPicker";
import { VarList, type VarListHandle } from "./VarList";
import "./TextCompare.css";
import "./TextEditor.css";

const ROW_HEIGHT = 20;
const CHAR_WIDTH = 7.8;
const PROBE_CHARS = 100;
const TAB_SIZE = 4;
/** Left padding of the text column (TextEditor.css `.te-text`). */
const TEXT_PAD = 6;
/** Gutter width when line numbers are hidden (bookmarks still show). */
const NARROW_GUTTER = 14;
const MAX_UNDO = 100;
/** Longer lines are shown without syntax colours. */
const MAX_SYNTAX_LINE = 4000;
const READ_ONLY_HINT = "Remote files are read-only here; use Save As to keep a local copy";

/** What a saved Text Edit session remembers besides its path. */
export interface EditSettings {
  wordWrap?: boolean;
  lineNumbers?: boolean;
  syntax?: boolean;
  /** 1-based bookmarked lines. */
  bookmarks?: number[];
  /** Manual encoding (null = detect). */
  encoding?: string | null;
}

interface Props {
  /** The file being edited ("" = a new, untitled document). */
  left: string;
  right: string;
  onPathsChange: (left: string, right: string) => void;
  settings?: Partial<EditSettings>;
  onSettingsChange?: (s: EditSettings) => void;
  onDirtyChange?: (dirty: boolean) => void;
}

/** The file as edited in memory. */
interface Doc {
  info: TextFileInfo;
  format: TextFormat;
  trailingNewline: boolean;
  binary: boolean;
  guessed: boolean;
  /** Never empty: an empty file is one empty line. */
  lines: string[];
  /** The `lines` array last loaded or saved (dirty when `lines` is another array). */
  saved: string[];
  savedFormat: TextFormat;
}

/**
 * Where the caret is. `seq` changes whenever the caret is placed (not while
 * typing), which re-applies the selection to the textarea.
 */
interface Caret {
  line: number;
  col: number;
  /** End of a character selection on the line. */
  selEnd?: number;
  seq: number;
  /** Move keyboard focus to the editor (not when the find bar placed it). */
  focus: boolean;
  /** Scroll a far-away line to a third of the view instead of the nearest edge. */
  jump: boolean;
}

interface Snapshot {
  lines: string[];
  line: number;
  col: number;
}

const sameFormat = (a: TextFormat, b: TextFormat) =>
  a.encoding === b.encoding && a.hasBom === b.hasBom && a.lineEnding === b.lineEnding;
const isDirty = (d: Doc) => d.lines !== d.saved || !sameFormat(d.format, d.savedFormat);

const NATIVE_EOL: LineEnding = /Win/.test(navigator.platform) ? "crlf" : "lf";

function untitled(): Doc {
  const lines = [""];
  const format: TextFormat = { encoding: "UTF-8", hasBom: false, lineEnding: NATIVE_EOL };
  return {
    info: { path: "", encoding: "UTF-8", hasBom: false, lineEnding: "none", size: 0 },
    format,
    trailingNewline: true,
    binary: false,
    guessed: false,
    lines,
    saved: lines,
    savedFormat: format,
  };
}

function fromLoaded(d: TextDocument): Doc {
  const lines = d.lines.length ? d.lines : [""];
  return {
    info: d.info,
    format: d.format,
    trailingNewline: d.trailingNewline,
    binary: d.binary,
    guessed: d.encodingGuessed,
    lines,
    saved: lines,
    savedFormat: d.format,
  };
}

/** Indentation the file already uses: tabs, else spaces (tabs for a file with none). */
function indentUnitOf(lines: string[]): string {
  let spaces = false;
  for (let i = 0; i < lines.length && i < 5000; i++) {
    const c = lines[i][0];
    if (c === "\t") return "\t";
    if (c === " " && lines[i].startsWith("  ")) spaces = true;
  }
  return spaces ? " ".repeat(TAB_SIZE) : "\t";
}

/** Removes one level of indentation (a tab or up to a tab's worth of spaces). */
function outdent(line: string): string {
  if (line.startsWith("\t")) return line.slice(1);
  let n = 0;
  while (n < TAB_SIZE && line[n] === " ") n++;
  return line.slice(n);
}

const shiftMarks = (marks: number[], start: number, removed: number, inserted: number) =>
  shiftBookmarks(
    marks.map((line) => ({ side: "left" as const, line })),
    "left",
    start,
    removed,
    inserted,
  ).map((m) => m.line);

const renderRun = (r: Run, i: number) => (
  <span key={i} className={r.cls || undefined}>
    {r.text}
  </span>
);

const EOL_LABEL: Record<LineEnding, string> = { lf: "LF", crlf: "CRLF", cr: "CR", mixed: "Mixed", none: "None" };

export function TextEditor({ left: path, onPathsChange, settings, onSettingsChange, onDirtyChange }: Props) {
  const [wordWrap, setWordWrap] = useState(settings?.wordWrap ?? false);
  const [lineNumbers, setLineNumbers] = useState(settings?.lineNumbers ?? true);
  const [syntaxOn, setSyntaxOn] = useState(settings?.syntax ?? true);
  const [bookmarks, setBookmarks] = useState<number[]>(settings?.bookmarks ?? []);
  const [encoding, setEncoding] = useState<string | null>(settings?.encoding ?? null);
  const reportSettings = useRef(onSettingsChange);
  reportSettings.current = onSettingsChange;
  useEffect(
    () => reportSettings.current?.({ wordWrap, lineNumbers, syntax: syntaxOn, bookmarks, encoding }),
    [wordWrap, lineNumbers, syntaxOn, bookmarks, encoding],
  );

  const [doc, setDoc] = useState<Doc | null>(null);
  const docRef = useRef(doc);
  docRef.current = doc;
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const [saving, setSaving] = useState(false);
  const [notice, setNotice] = useState<string | null>(null);
  const [history, setHistory] = useState<{ undo: Snapshot[]; redo: Snapshot[] }>({ undo: [], redo: [] });
  const [caret, setCaret] = useState<Caret>({ line: 0, col: 0, seq: 0, focus: false, jump: false });
  const caretSeq = useRef(0);
  /** Line selection: the line it was extended from (the caret line is the other end). */
  const [anchor, setAnchor] = useState<number | null>(null);
  const [gotoOpen, setGotoOpen] = useState(false);
  const root = useRef<HTMLDivElement>(null);
  const bodyRef = useRef<HTMLDivElement>(null);
  const probeRef = useRef<HTMLSpanElement>(null);
  const list = useRef<VarListHandle>(null);
  const editorEl = useRef<HTMLTextAreaElement>(null);

  const remote = isRemote(path);
  const readOnly = remote || !!doc?.binary;
  const dirty = !!doc && isDirty(doc);
  const reportDirty = useRef(onDirtyChange);
  reportDirty.current = onDirtyChange;
  useEffect(() => reportDirty.current?.(dirty), [dirty]);

  useEffect(() => {
    if (!notice) return;
    const t = window.setTimeout(() => setNotice(null), 4000);
    return () => window.clearTimeout(t);
  }, [notice]);

  /** Typing on one line merges into one undo step; anything else starts a new one. */
  const coalesce = useRef<string | null>(null);

  /** Places the caret (and optionally a selection on its line). */
  const place = (line: number, col: number, opts: { selEnd?: number; focus?: boolean; jump?: boolean } = {}) => {
    coalesce.current = null;
    setCaret({
      line,
      col,
      selEnd: opts.selEnd,
      seq: ++caretSeq.current,
      focus: opts.focus ?? true,
      jump: opts.jump ?? false,
    });
  };

  // ---------- loading ----------

  const encodingRef = useRef(encoding);
  encodingRef.current = encoding;
  /** The path whose text is in `doc` (Save As renames without re-reading). */
  const loadedPath = useRef<string | null>(null);
  const loadSeq = useRef(0);
  const load = useCallback(async (p: string) => {
    const seq = ++loadSeq.current;
    loadedPath.current = p;
    const reset = (d: Doc | null) => {
      setDoc(d);
      setHistory({ undo: [], redo: [] });
      setAnchor(null);
      setCaret({ line: 0, col: 0, seq: ++caretSeq.current, focus: false, jump: true });
    };
    if (!p) {
      reset(untitled());
      setError(null);
      return;
    }
    setLoading(true);
    try {
      const d = await textApi.loadText(p, encodingRef.current);
      if (seq !== loadSeq.current) return;
      reset(fromLoaded(d));
      setError(null);
    } catch (e) {
      if (seq !== loadSeq.current) return;
      setError(errorMessage(e));
      setDoc(null);
    } finally {
      if (seq === loadSeq.current) setLoading(false);
    }
  }, []);

  useEffect(() => {
    if (loadedPath.current === path && docRef.current) return;
    load(path);
  }, [path, load]);

  const confirmDiscard = (action: string) =>
    !dirty || window.confirm(`You have unsaved changes. Discard them and ${action}?`);

  const reload = () => {
    if (path && confirmDiscard("reload the file")) load(path);
  };

  const openPath = (p: string) => {
    if (p === path || !confirmDiscard("open another file")) return;
    setBookmarks([]);
    setEncoding(null);
    encodingRef.current = null;
    onPathsChange(p, "");
  };

  // ---------- saving ----------

  const writeTo = async (target: string): Promise<boolean> => {
    const d = docRef.current;
    if (!d || saving) return false;
    setSaving(true);
    setError(null);
    try {
      const { info, backup } = await textApi.saveTextFile(
        target,
        joinLines(d.lines, d.trailingNewline),
        d.format,
        loadBackups(),
      );
      setNotice(backup ? `Saved; previous version backed up to ${backup}` : `Saved ${basename(target)}`);
      // Edits made while saving stay dirty: only the saved array is marked clean.
      setDoc((cur) => cur && { ...cur, info: { ...info, path: target }, saved: d.lines, savedFormat: d.format });
      return true;
    } catch (e) {
      setError(errorMessage(e));
      return false;
    } finally {
      setSaving(false);
    }
  };

  const saveAs = async () => {
    if (!docRef.current || docRef.current.binary) return;
    const picked = await saveDialog({ title: "Save as", defaultPath: path && !remote ? path : basename(path) || undefined });
    if (typeof picked !== "string" || !(await writeTo(picked))) return;
    if (picked !== path) {
      // The text in memory is what was just written; don't read it back.
      loadedPath.current = picked;
      onPathsChange(picked, "");
    }
  };

  const save = () => {
    if (!docRef.current || docRef.current.binary) return;
    if (!path || remote) saveAs();
    else if (dirty) writeTo(path);
  };

  /**
   * Picks the encoding: an unedited file is re-read in it; with unsaved edits
   * (or a new file) only the save format changes, converting on save.
   */
  const changeEncoding = async (enc: string | null) => {
    setEncoding(enc);
    encodingRef.current = enc;
    const d = docRef.current;
    if (!d) return;
    if (isDirty(d) || !path) {
      if (enc) {
        const format = { ...d.format, encoding: enc.replace(/ BOM$/, ""), hasBom: enc.endsWith(" BOM") };
        setDoc((cur) => cur && { ...cur, format });
      }
      return;
    }
    load(path);
  };

  // ---------- edits ----------

  const lines = doc?.lines ?? [];

  /** Replaces the lines as one undoable step and moves the caret. */
  const commit = (
    next: string[],
    at: { line: number; col: number; selEnd?: number },
    opts: { key?: string; span?: { start: number; removed: number; inserted: number }; placeCaret?: boolean } = {},
  ) => {
    const d = docRef.current;
    if (!d || readOnly || next === d.lines) return;
    const safe = next.length ? next : [""];
    const merge = !!opts.key && opts.key === coalesce.current && history.undo.length > 0;
    const before: Snapshot = { lines: d.lines, line: caret.line, col: caret.col };
    setHistory((h) =>
      merge ? { undo: h.undo, redo: [] } : { undo: [...h.undo.slice(-(MAX_UNDO - 1)), before], redo: [] },
    );
    const nd = { ...d, lines: safe };
    docRef.current = nd;
    setDoc(nd);
    const span = opts.span;
    if (span && span.removed !== span.inserted)
      setBookmarks((b) => shiftMarks(b, span.start, span.removed, span.inserted));
    if (opts.placeCaret === false) setCaret((c) => ({ ...c, line: at.line, col: at.col }));
    else place(Math.min(at.line, safe.length - 1), at.col, { selEnd: at.selEnd });
    coalesce.current = opts.key ?? null;
  };

  /** Applies a structural edit (split, join, multi-line paste). */
  const apply = (res: EditResult | null) => {
    if (!res) return;
    const span =
      res.delta > 0
        ? { start: res.at, removed: 0, inserted: res.delta }
        : res.delta < 0
          ? { start: res.at, removed: -res.delta, inserted: 0 }
          : undefined;
    commit(res.lines, { line: res.line, col: res.caret }, { span, key: res.delta === 0 ? `type:${res.line}` : undefined });
  };

  const restore = (snap: Snapshot) => {
    const d = docRef.current;
    if (!d) return;
    const nd = { ...d, lines: snap.lines };
    docRef.current = nd;
    setDoc(nd);
    setAnchor(null);
    const line = Math.min(snap.line, snap.lines.length - 1);
    place(line, Math.min(snap.col, snap.lines[line].length));
  };

  const undo = () => {
    const d = docRef.current;
    const prev = history.undo[history.undo.length - 1];
    if (!d || !prev || readOnly) return;
    setHistory({
      undo: history.undo.slice(0, -1),
      redo: [...history.redo, { lines: d.lines, line: caret.line, col: caret.col }],
    });
    restore(prev);
  };

  const redo = () => {
    const d = docRef.current;
    const next = history.redo[history.redo.length - 1];
    if (!d || !next || readOnly) return;
    setHistory({
      undo: [...history.undo, { lines: d.lines, line: caret.line, col: caret.col }],
      redo: history.redo.slice(0, -1),
    });
    restore(next);
  };

  /** The selected lines (inclusive), or null without a line selection. */
  const selRange: [number, number] | null =
    anchor !== null ? [Math.min(anchor, caret.line), Math.max(anchor, caret.line)] : null;
  /** The selection, or just the caret line. */
  const pickRange = (): [number, number] => selRange ?? [caret.line, caret.line];

  const deleteLines = ([a, b]: [number, number]) => {
    commit(replaceLines(lines, a, b + 1, []), { line: Math.min(a, Math.max(0, lines.length - (b - a + 1) - 1)), col: 0 }, {
      span: { start: a, removed: b - a + 1, inserted: 0 },
    });
    setAnchor(null);
  };

  const linesText = ([a, b]: [number, number]) => {
    const text = lines.slice(a, b + 1).join("\n");
    return b > a || !selRange ? `${text}\n` : text;
  };

  const copyLines = (cut: boolean) => {
    const range = pickRange();
    writeClipboardText(linesText(range)).catch((e) => setError(errorMessage(e)));
    if (cut && !readOnly) deleteLines(range);
  };

  /** Tab / Shift+Tab over the selected lines (or the caret line). */
  const indentLines = (dir: 1 | -1) => {
    const [a, b] = pickRange();
    const unit = indentUnitOf(lines);
    const next = lines.slice();
    for (let i = a; i <= b; i++) {
      if (dir > 0) {
        if (next[i] || a === b) next[i] = unit + next[i];
      } else next[i] = outdent(next[i]);
    }
    const shift = next[caret.line].length - lines[caret.line].length;
    commit(next, { line: caret.line, col: Math.max(0, caret.col + shift) });
  };

  const duplicateLines = () => {
    const [a, b] = pickRange();
    const n = b - a + 1;
    commit(replaceLines(lines, b + 1, b + 1, lines.slice(a, b + 1)), { line: caret.line + n, col: caret.col }, {
      span: { start: b + 1, removed: 0, inserted: n },
    });
    if (anchor !== null) setAnchor(anchor + n);
  };

  const moveLines = (dir: 1 | -1) => {
    const [a, b] = pickRange();
    if ((dir < 0 && a === 0) || (dir > 0 && b >= lines.length - 1)) return;
    const block = lines.slice(a, b + 1);
    const next =
      dir < 0
        ? replaceLines(lines, a - 1, b + 1, [...block, lines[a - 1]])
        : replaceLines(lines, a, b + 2, [lines[b + 1], ...block]);
    commit(next, { line: caret.line + dir, col: caret.col });
    if (anchor !== null) setAnchor(anchor + dir);
    setBookmarks((bs) =>
      bs.map((m) => {
        const i = m - 1;
        if (i >= a && i <= b) return m + dir;
        if (dir < 0 && i === a - 1) return b + 1;
        if (dir > 0 && i === b + 1) return a + 1;
        return m;
      }),
    );
  };

  /** Replaces the selected lines with `text` (a paste over a line selection). */
  const replaceSelection = (text: string) => {
    if (!selRange) return;
    const [a, b] = selRange;
    const { lines: pasted } = splitText(text);
    commit(replaceLines(lines, a, b + 1, pasted), { line: a + pasted.length - 1, col: pasted[pasted.length - 1].length }, {
      span: { start: a, removed: b - a + 1, inserted: pasted.length },
    });
    setAnchor(null);
  };

  // ---------- layout ----------

  const [metrics, setMetrics] = useState({ charW: CHAR_WIDTH, width: 800, height: 600 });
  const hasDoc = !!doc && !doc.binary;
  useLayoutEffect(() => {
    const body = bodyRef.current;
    const probe = probeRef.current;
    if (!body || !probe) return;
    const measure = () => {
      const charW = probe.getBoundingClientRect().width / PROBE_CHARS || CHAR_WIDTH;
      const listEl = body.querySelector<HTMLElement>(".te-list");
      const width = listEl?.clientWidth ?? body.clientWidth;
      const height = listEl?.clientHeight ?? body.clientHeight;
      setMetrics((m) => (m.charW === charW && m.width === width && m.height === height ? m : { charW, width, height }));
    };
    const ro = new ResizeObserver(measure);
    ro.observe(body);
    ro.observe(probe);
    // The list narrows when its vertical scrollbar appears.
    const listEl = body.querySelector<HTMLElement>(".te-list");
    if (listEl) ro.observe(listEl);
    measure();
    return () => ro.disconnect();
  }, [hasDoc]);

  const gutterW = lineNumbers ? Math.ceil(String(lines.length).length * metrics.charW) + 22 : NARROW_GUTTER;
  const cols = Math.max(8, Math.floor((metrics.width - gutterW - TEXT_PAD - 6) / metrics.charW));

  /** Word wrap: prefix sums of each line's height. */
  const offsets = useMemo(() => {
    if (!wordWrap) return null;
    const off = new Float64Array(lines.length + 1);
    for (let i = 0; i < lines.length; i++) {
      const t = lines[i];
      const n = t.length <= cols && !t.includes("\t") ? 1 : visualLines(t, cols, TAB_SIZE);
      off[i + 1] = off[i] + n * ROW_HEIGHT;
    }
    return off;
  }, [wordWrap, lines, cols]);

  /** Without wrap: the widest line, so the list scrolls horizontally. */
  const contentW = useMemo(() => {
    if (wordWrap) return 0;
    let max = 0;
    for (const t of lines) {
      if (t.length < max) continue;
      max = Math.max(max, t.includes("\t") ? displayWidth(t, TAB_SIZE) : t.length);
    }
    return gutterW + TEXT_PAD + (max + 2) * metrics.charW + 40;
  }, [wordWrap, lines, gutterW, metrics.charW]);

  const rowTop = (i: number) => (offsets ? offsets[Math.min(i, lines.length)] : i * ROW_HEIGHT);

  // Syntax, find and bookmarks work on a deferred copy so typing in huge files stays smooth.
  const deferredLines = useDeferredValue(lines);
  const lang = useMemo(() => languageFor(path || doc?.info.path || ""), [path, doc?.info.path]);
  const states = useMemo(
    () => (syntaxOn && lang && hasDoc ? lineStates(deferredLines, lang) : null),
    [syntaxOn, lang, hasDoc, deferredLines],
  );

  // ---------- find & replace ----------

  const [findOpen, setFindOpen] = useState(false);
  const [replaceMode, setReplaceMode] = useState(false);
  const [findFocus, setFindFocus] = useState(0);
  const [query, setQuery] = useState("");
  const [replacement, setReplacement] = useState("");
  const [findOpts, setFindOpts] = useState<FindOptions>(DEFAULT_FIND_OPTIONS);
  const [findIdx, setFindIdx] = useState(-1);
  const compiled = useMemo(() => compileQuery(query, findOpts), [query, findOpts]);
  const hits = useMemo<LineMatch[]>(
    () => (findOpen && compiled.re && hasDoc ? findInLines(deferredLines, compiled.re) : []),
    [findOpen, compiled, hasDoc, deferredLines],
  );
  const hitRanges = useMemo(() => {
    const map = new Map<number, StyledRange[]>();
    hits.forEach((m, i) => {
      const ranges = map.get(m.line) ?? [];
      ranges.push({ start: m.start, end: m.end, cls: i === findIdx ? "find-hit find-cur" : "find-hit" });
      map.set(m.line, ranges);
    });
    return map;
  }, [hits, findIdx]);

  const revealHit = (i: number) => {
    const m = hits[i];
    if (!m) return setFindIdx(-1);
    setFindIdx(i);
    setAnchor(null);
    place(m.line, m.start, { selEnd: m.end, focus: false, jump: true });
  };

  const findKey = `${findOpen}\u0000${query}\u0000${findOpts.matchCase}${findOpts.wholeWord}${findOpts.regex}`;
  const lastFindKey = useRef("");
  /** After Replace: continue with the first hit after this point once the hits are recomputed. */
  const resumeAt = useRef<{ line: number; col: number; lines: string[] } | null>(null);
  useEffect(() => {
    if (lastFindKey.current !== findKey) {
      // A new search jumps to the first hit at or after the caret (incremental search).
      lastFindKey.current = findKey;
      if (!hits.length) return setFindIdx(-1);
      const i = hits.findIndex((m) => m.line > caret.line || (m.line === caret.line && m.start >= caret.col));
      revealHit(i >= 0 ? i : 0);
      return;
    }
    const r = resumeAt.current;
    if (r && r.lines === deferredLines) {
      resumeAt.current = null;
      const i = hits.findIndex((m) => m.line > r.line || (m.line === r.line && m.start >= r.col));
      if (hits.length) revealHit(i >= 0 ? i : 0);
      else setFindIdx(-1);
      return;
    }
    if (findIdx >= hits.length) setFindIdx(hits.length ? 0 : -1);
    // Runs when the hits change; the find key and resume marker say why.
  }, [hits]);

  const openFind = (replace: boolean) => {
    const ta = editorEl.current;
    const picked =
      ta && document.activeElement === ta
        ? ta.value.slice(ta.selectionStart, ta.selectionEnd)
        : (window.getSelection()?.toString() ?? "");
    if (picked && !/[\r\n]/.test(picked)) setQuery(picked);
    setFindOpen(true);
    setReplaceMode(replace);
    setFindFocus((f) => f + 1);
  };

  const findStep = (dir: 1 | -1) => {
    if (!findOpen) {
      if (!query) return openFind(false);
      setFindOpen(true);
      return;
    }
    if (!hits.length) return;
    if (findIdx >= 0 && findIdx < hits.length) return revealHit((findIdx + dir + hits.length) % hits.length);
    let i = -1;
    if (dir > 0) i = hits.findIndex((m) => m.line > caret.line);
    else for (let k = hits.length - 1; k >= 0 && i < 0; k--) if (hits[k].line < caret.line) i = k;
    revealHit(i >= 0 ? i : dir > 0 ? 0 : hits.length - 1);
  };

  const replaceBlocked = readOnly ? READ_ONLY_HINT : deferredLines !== lines ? "Catching up with your edits" : null;

  const replaceCurrent = () => {
    if (replaceBlocked || !compiled.re) return;
    const m = hits[findIdx];
    if (!m) return findStep(1);
    const out = replaceAt(lines[m.line], compiled.re, m.start, replacement, findOpts.regex);
    if (out === null) return findStep(1);
    const next = lines.slice();
    next[m.line] = out;
    const col = m.end + out.length - lines[m.line].length;
    resumeAt.current = { line: m.line, col, lines: next };
    commit(next, { line: m.line, col }, { placeCaret: false });
  };

  const replaceAll = () => {
    if (replaceBlocked || !compiled.re) return;
    const r = replaceAllInLines(lines, compiled.re, replacement, findOpts.regex);
    if (r.count) commit(r.lines, { line: caret.line, col: 0 });
    setNotice(`Replaced ${r.count.toLocaleString()} occurrence${r.count === 1 ? "" : "s"}`);
  };

  // ---------- bookmarks and navigation ----------

  const bookmarkSet = useMemo(() => new Set(bookmarks), [bookmarks]);

  const toggleBookmark = () => {
    const n = caret.line + 1;
    setBookmarks((bs) => (bs.includes(n) ? bs.filter((b) => b !== n) : [...bs, n].sort((a, b) => a - b)));
  };

  const gotoBookmark = (dir: 1 | -1) => {
    const marks = bookmarks.filter((b) => b <= lines.length).sort((a, b) => a - b);
    if (!marks.length) return;
    const cur = caret.line + 1;
    let target = dir > 0 ? marks.find((b) => b > cur) : [...marks].reverse().find((b) => b < cur);
    target ??= dir > 0 ? marks[0] : marks[marks.length - 1];
    setAnchor(null);
    place(target - 1, 0, { jump: true });
  };

  const [gotoText, setGotoText] = useState("");
  const openGoto = () => {
    setGotoText(String(caret.line + 1));
    setGotoOpen(true);
  };
  const gotoLine = () => {
    const m = /^\s*(\d+)\s*(?:[:,]\s*(\d+))?\s*$/.exec(gotoText);
    if (!m) return setNotice("Type a line number, optionally with a column (120:8)");
    const line = Math.max(1, Math.min(lines.length, Number(m[1]))) - 1;
    const col = m[2] ? indexAtColumn(lines[line], Math.max(0, Number(m[2]) - 1), TAB_SIZE) : 0;
    setGotoOpen(false);
    setAnchor(null);
    place(line, col, { jump: true });
  };

  /** Lines a PageUp/PageDown moves. */
  const pageLines = Math.max(1, Math.floor(metrics.height / ROW_HEIGHT) - 1);

  // Keep the caret in view (vertically, and horizontally without word wrap).
  useEffect(() => {
    if (!hasDoc) return;
    list.current?.ensureVisible(caret.line, caret.jump);
  }, [caret.seq, hasDoc]);
  useEffect(() => {
    if (!hasDoc || wordWrap) return;
    const scroller = bodyRef.current?.querySelector<HTMLElement>(".te-list");
    const text = lines[caret.line];
    if (!scroller || text === undefined) return;
    const x = gutterW + TEXT_PAD + columnOf(text, caret.col, TAB_SIZE) * metrics.charW;
    const view = scroller.clientWidth;
    if (x < scroller.scrollLeft + gutterW + metrics.charW) scroller.scrollLeft = Math.max(0, x - gutterW - view / 3);
    else if (x > scroller.scrollLeft + view - 3 * metrics.charW) scroller.scrollLeft = x - view + view / 3;
  }, [caret.line, caret.col, caret.seq, hasDoc, wordWrap]);

  // Put the caret (and selection) into the textarea once per placement.
  const caretApplied = useRef(-1);
  useLayoutEffect(() => {
    const el = editorEl.current;
    if (!el || caretApplied.current === caret.seq) return;
    caretApplied.current = caret.seq;
    if (caret.focus) el.focus({ preventScroll: true });
    const len = el.value.length;
    el.setSelectionRange(Math.min(caret.col, len), Math.min(caret.selEnd ?? caret.col, len));
    if (!wordWrap) el.scrollLeft = 0;
  });

  // ---------- the line editor ----------

  const onEditorChange = (e: ChangeEvent<HTMLTextAreaElement>) => {
    if (readOnly) return;
    const value = e.target.value;
    const line = caret.line;
    if (/[\r\n]/.test(value)) {
      // Line breaks that arrive without Enter/paste (drag-and-drop, IME) split the line too.
      apply(insertAt(lines, line, 0, lines[line].length, value));
      return;
    }
    const next = lines.slice();
    next[line] = value;
    setAnchor(null);
    commit(next, { line, col: e.target.selectionStart }, { key: `type:${line}`, placeCaret: false });
  };

  /** Up/Down leave the line unless word wrap still has visual lines to move through. */
  const leavesLine = (ta: HTMLTextAreaElement, up: boolean) => {
    if (!wordWrap) return true;
    const at = Math.floor(columnOf(ta.value, ta.selectionStart, TAB_SIZE) / cols);
    return up ? at === 0 : at >= visualLines(ta.value, cols, TAB_SIZE) - 1;
  };

  /** Moves the caret `delta` lines, keeping its display column. */
  const moveBy = (delta: number, extend: boolean) => {
    const target = Math.max(0, Math.min(lines.length - 1, caret.line + delta));
    if (extend) setAnchor(anchor ?? caret.line);
    else setAnchor(null);
    const col = columnOf(lines[caret.line] ?? "", caret.col, TAB_SIZE);
    place(target, indexAtColumn(lines[target], col, TAB_SIZE));
  };

  const onEditorKey = (e: ReactKeyboardEvent<HTMLTextAreaElement>) => {
    const ta = e.currentTarget;
    const { selectionStart: s, selectionEnd: end, value } = ta;
    const line = caret.line;
    const mod = e.ctrlKey || e.metaKey;
    const plain = !mod && !e.altKey;
    const key = e.key.toLowerCase();
    if (selRange && plain && (e.key === "Backspace" || e.key === "Delete")) deleteLines(selRange);
    else if (plain && e.key === "Tab" && (selRange || e.shiftKey)) indentLines(e.shiftKey ? -1 : 1);
    else if (plain && !e.shiftKey && e.key === "Tab") apply(insertAt(lines, line, s, end, indentUnitOf(lines)));
    else if (plain && e.key === "Enter") {
      // Keep the line's indentation on the new line.
      const indent = /^[\t ]*/.exec(value.slice(0, s))?.[0] ?? "";
      apply(insertAt(lines, line, s, end, `\n${indent}`));
    } else if (plain && e.key === "Backspace" && s === 0 && end === 0) apply(joinWithPrevious(lines, line));
    else if (plain && e.key === "Delete" && s === value.length && end === s) apply(joinWithNext(lines, line));
    else if (!mod && !e.altKey && (e.key === "ArrowUp" || e.key === "ArrowDown")) {
      const up = e.key === "ArrowUp";
      if (!e.shiftKey && !leavesLine(ta, up)) return;
      if ((up && line === 0) || (!up && line === lines.length - 1)) {
        if (e.shiftKey) setAnchor(anchor ?? line);
        else place(line, up ? 0 : value.length);
      } else moveBy(up ? -1 : 1, e.shiftKey);
    } else if (plain && !e.shiftKey && e.key === "ArrowLeft" && s === 0 && end === 0 && line > 0)
      place(line - 1, lines[line - 1].length);
    else if (plain && !e.shiftKey && e.key === "ArrowRight" && s === value.length && end === s && line < lines.length - 1)
      place(line + 1, 0);
    else if (!e.altKey && !mod && (e.key === "PageUp" || e.key === "PageDown"))
      moveBy(e.key === "PageUp" ? -pageLines : pageLines, e.shiftKey);
    else if (mod && !e.altKey && (e.key === "Home" || e.key === "End")) {
      if (e.shiftKey) setAnchor(anchor ?? line);
      else setAnchor(null);
      const target = e.key === "Home" ? 0 : lines.length - 1;
      place(target, e.key === "Home" ? 0 : lines[target].length, { jump: true });
    } else if (mod && !e.shiftKey && !e.altKey && key === "a") {
      setAnchor(0);
      place(lines.length - 1, lines[lines.length - 1].length);
    } else if (mod && !e.shiftKey && !e.altKey && (key === "c" || key === "x") && (selRange || s === end))
      copyLines(key === "x");
    else if (e.key === "Escape" && anchor !== null) setAnchor(null);
    else if (e.key === "Escape" && findOpen) setFindOpen(false);
    else return;
    e.preventDefault();
  };

  const onEditorPaste = (e: ReactClipboardEvent<HTMLTextAreaElement>) => {
    const text = e.clipboardData.getData("text/plain");
    if (selRange) {
      e.preventDefault();
      replaceSelection(text);
      return;
    }
    if (!/[\r\n]/.test(text)) return; // single-line paste: the textarea handles it
    e.preventDefault();
    const ta = e.currentTarget;
    apply(insertAt(lines, caret.line, ta.selectionStart, ta.selectionEnd, text));
  };

  // ---------- mouse ----------

  const dragging = useRef(false);
  useEffect(() => {
    const up = () => (dragging.current = false);
    window.addEventListener("mouseup", up);
    return () => window.removeEventListener("mouseup", up);
  }, []);

  /** The character under a click in a line's text. */
  const indexFromPoint = (e: ReactMouseEvent<HTMLElement>, text: string) => {
    const rect = e.currentTarget.getBoundingClientRect();
    let col = (e.clientX - rect.left - TEXT_PAD) / metrics.charW;
    if (wordWrap) col += Math.floor((e.clientY - rect.top) / ROW_HEIGHT) * cols;
    return indexAtColumn(text, Math.max(0, col), TAB_SIZE);
  };

  const clickText = (e: ReactMouseEvent<HTMLElement>, i: number) => {
    if (e.button !== 0) return;
    e.preventDefault();
    if (e.shiftKey) setAnchor(anchor ?? caret.line);
    else setAnchor(null);
    place(i, e.shiftKey ? 0 : indexFromPoint(e, lines[i]));
  };

  const clickGutter = (e: ReactMouseEvent<HTMLElement>, i: number) => {
    if (e.button !== 0) return;
    e.preventDefault();
    if (e.shiftKey) setAnchor(anchor ?? caret.line);
    else setAnchor(i);
    dragging.current = true;
    place(i, 0);
  };

  // ---------- keyboard (commands) ----------

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      // Inactive tabs stay mounted but hidden; only the visible session reacts.
      if (e.defaultPrevented || !root.current?.offsetParent) return;
      const t = e.target as HTMLElement | null;
      const inEditor = !!t && t === editorEl.current;
      const inField = !!t && (t.tagName === "INPUT" || t.tagName === "TEXTAREA" || t.tagName === "SELECT");
      const mod = e.ctrlKey || e.metaKey;
      if (keyMatches(e, "edit.save")) save();
      else if (keyMatches(e, "edit.saveAs")) saveAs();
      else if (keyMatches(e, "edit.find")) openFind(false);
      else if (keyMatches(e, "edit.replace")) openFind(true);
      else if (keyMatches(e, "edit.findNext")) findStep(1);
      else if (keyMatches(e, "edit.findPrev")) findStep(-1);
      else if (keyMatches(e, "edit.goToLine")) hasDoc && openGoto();
      else if (keyMatches(e, "edit.toggleBookmark")) toggleBookmark();
      else if (keyMatches(e, "edit.clearBookmarks")) setBookmarks([]);
      else if (keyMatches(e, "edit.nextBookmark")) gotoBookmark(1);
      else if (keyMatches(e, "edit.prevBookmark")) gotoBookmark(-1);
      else if (keyMatches(e, "edit.wordWrap")) setWordWrap((w) => !w);
      else if (keyMatches(e, "edit.reload")) reload();
      else if (inField && !inEditor) return;
      else if (keyMatches(e, "edit.undo")) undo();
      else if (keyMatches(e, "edit.redo") || eventBinding(e) === "Ctrl+Shift+Z") redo();
      else if (keyMatches(e, "edit.duplicateLines")) duplicateLines();
      else if (keyMatches(e, "edit.moveLinesUp")) moveLines(-1);
      else if (keyMatches(e, "edit.moveLinesDown")) moveLines(1);
      else if (inEditor) return;
      // Read-only files have no line editor: navigate and copy from here.
      else if (!hasDoc) return;
      else if (!mod && !e.altKey && (e.key === "ArrowUp" || e.key === "ArrowDown")) moveBy(e.key === "ArrowUp" ? -1 : 1, e.shiftKey);
      else if (!mod && !e.altKey && (e.key === "PageUp" || e.key === "PageDown"))
        moveBy(e.key === "PageUp" ? -pageLines : pageLines, e.shiftKey);
      else if (mod && !e.shiftKey && !e.altKey && e.key.toLowerCase() === "c" && !window.getSelection()?.toString())
        copyLines(false);
      else if (e.key === "Escape" && anchor !== null) setAnchor(null);
      else return;
      e.preventDefault();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  });

  // ---------- rendering ----------

  const renderContent = (i: number) => {
    const text = lines[i];
    const layers: StyledRange[][] = [];
    if (lang && syntaxOn && text.length <= MAX_SYNTAX_LINE) {
      const state = states?.[i] ?? 0;
      layers.push(lineTokens(text, lang, state).map((t) => ({ start: t.start, end: t.end, cls: `syn-${t.kind}` })));
    }
    const h = hitRanges.get(i);
    if (h) layers.push(h);
    const runs = composeRuns(text, layers);
    if (wordWrap) {
      return wrapRuns(runs, cols, TAB_SIZE).map((line, k) => (
        <div key={k} className="te-vline">
          {line.map(renderRun)}
        </div>
      ));
    }
    return runs.map(renderRun);
  };

  const renderRow = (i: number) => {
    const sel = !!selRange && i >= selRange[0] && i <= selRange[1];
    const marked = bookmarkSet.has(i + 1);
    return (
      <div
        className={`te-row${i === caret.line ? " te-caret" : ""}${sel ? " te-sel" : ""}`}
        onMouseEnter={(e) => dragging.current && e.buttons & 1 && place(i, 0)}
      >
        <span
          className={`te-ln${marked ? " te-bm" : ""}`}
          style={{ width: gutterW }}
          title={marked ? "Bookmark (F2 / Shift+F2 to jump, Ctrl+F2 to remove)" : "Click or drag to select lines"}
          onMouseDown={(e) => clickGutter(e, i)}
        >
          {lineNumbers ? i + 1 : ""}
        </span>
        <div className="te-text" onMouseDown={(e) => clickText(e, i)}>
          {renderContent(i)}
        </div>
      </div>
    );
  };

  /** The textarea floating over the caret line. */
  const renderEditor = () => {
    if (!doc || readOnly || caret.line >= lines.length) return null;
    const top = rowTop(caret.line);
    const height = rowTop(caret.line + 1) - top;
    return (
      <div className="vrow te-editor-layer" style={{ top, height }}>
        <div className={`te-row${selRange ? " te-sel" : ""}`}>
          <span className="te-ln te-ln-ghost" style={{ width: gutterW }} />
          <textarea
            ref={editorEl}
            className="te-area"
            wrap={wordWrap ? "soft" : "off"}
            spellCheck={false}
            autoCapitalize="off"
            autoCorrect="off"
            rows={1}
            value={lines[caret.line]}
            onChange={onEditorChange}
            onKeyDown={onEditorKey}
            onPaste={onEditorPaste}
            onSelect={(e) => {
              const col = e.currentTarget.selectionStart;
              setCaret((c) => (c.col === col ? c : { ...c, col }));
            }}
          />
        </div>
      </div>
    );
  };

  const caretText = lines[caret.line] ?? "";
  const eolTitle =
    doc?.info.lineEnding === "mixed"
      ? `The file mixes line endings; saving normalizes them to ${EOL_LABEL[doc.format.lineEnding]}`
      : "Line endings written when saving";
  const canSave = !!doc && !doc.binary && (dirty || !path) && !saving;

  return (
    <div className={`session te-session${wordWrap ? " te-wrapped" : ""}`} ref={root}>
      <div className="toolbar">
        <button onClick={save} disabled={!canSave} title={`Save (${shortcutLabel("edit.save")})${remote ? ` · ${READ_ONLY_HINT}` : ""}`}>
          <Save size={14} /> {saving ? "Saving…" : "Save"}
        </button>
        <button onClick={saveAs} disabled={!doc || doc.binary || saving} title={`Save as… (${shortcutLabel("edit.saveAs")})`}>
          <SaveAll size={14} /> Save As
        </button>
        <span className="sep" />
        <button className="icon-btn" onClick={undo} disabled={!history.undo.length || readOnly} title={`Undo (${shortcutLabel("edit.undo")})`}>
          <Undo2 size={14} />
        </button>
        <button className="icon-btn" onClick={redo} disabled={!history.redo.length || readOnly} title={`Redo (${shortcutLabel("edit.redo")})`}>
          <Redo2 size={14} />
        </button>
        <span className="sep" />
        <button
          className={`icon-btn${findOpen ? " active" : ""}`}
          onClick={() => (findOpen ? setFindOpen(false) : openFind(false))}
          title={`Find (${shortcutLabel("edit.find")}, ${shortcutLabel("edit.findNext")} / ${shortcutLabel("edit.findPrev")}) · Replace (${shortcutLabel("edit.replace")})`}
        >
          <Search size={14} />
        </button>
        <button className="icon-btn" onClick={openGoto} disabled={!hasDoc} title={`Go to line (${shortcutLabel("edit.goToLine")})`}>
          <CornerDownRight size={14} />
        </button>
        <button
          className="icon-btn"
          onClick={toggleBookmark}
          disabled={!hasDoc}
          title={`Toggle bookmark (${shortcutLabel("edit.toggleBookmark")}) · next / previous: ${shortcutLabel("edit.nextBookmark")} / ${shortcutLabel("edit.prevBookmark")}${
            bookmarks.length ? ` · ${bookmarks.length} set` : ""
          }`}
        >
          <Bookmark size={14} />
        </button>
        <span className="sep" />
        <button
          className={`icon-btn${wordWrap ? " active" : ""}`}
          onClick={() => setWordWrap((w) => !w)}
          title={`Word wrap (${shortcutLabel("edit.wordWrap")})`}
        >
          <TextWrap size={14} />
        </button>
        <button
          className={`icon-btn${lineNumbers ? " active" : ""}`}
          onClick={() => setLineNumbers((n) => !n)}
          title="Line numbers"
        >
          <Hash size={14} />
        </button>
        <button
          className={`icon-btn${syntaxOn ? " active" : ""}`}
          onClick={() => setSyntaxOn((s) => !s)}
          title={`Syntax highlighting${lang ? ` (${lang.name})` : " (no language recognised for this file)"}`}
        >
          <Code size={14} />
        </button>
        <span className="spacer" />
        {doc && !doc.binary && (
          <>
            <TextEncodingPicker
              value={encoding}
              detected={doc.info.encoding || doc.format.encoding}
              hasBom={doc.info.hasBom}
              guessed={doc.guessed}
              dirty={dirty || !path}
              onChange={(enc) => changeEncoding(enc)}
            />
            <select
              className="te-eol"
              value={doc.format.lineEnding}
              title={eolTitle}
              disabled={remote}
              onChange={(e) => {
                const lineEnding = e.target.value as LineEnding;
                setDoc((d) => d && { ...d, format: { ...d.format, lineEnding } });
              }}
            >
              {(["lf", "crlf", "cr"] as LineEnding[]).map((le) => (
                <option key={le} value={le}>
                  {EOL_LABEL[le]}
                </option>
              ))}
            </select>
          </>
        )}
        <button onClick={reload} disabled={!path} title={`Reload from disk (${shortcutLabel("edit.reload")})`}>
          <RefreshCw size={14} /> Reload
        </button>
      </div>

      <div className="pathbar te-pathbar">
        <PathFieldControlled path={path} onOpen={openPath} />
      </div>

      {findOpen && (
        <FindBar
          query={query}
          onQuery={setQuery}
          replacement={replacement}
          onReplacement={setReplacement}
          options={findOpts}
          onOptions={setFindOpts}
          replaceMode={replaceMode}
          onReplaceMode={setReplaceMode}
          count={hits.length}
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

      <div
        className="session-body te-body"
        ref={bodyRef}
        style={{ ["--te-content-w" as string]: contentW ? `${contentW}px` : "100%" }}
      >
        <span className="tc-probe te-probe" ref={probeRef} aria-hidden>
          {"0".repeat(PROBE_CHARS)}
        </span>
        {loading && <div className="overlay">Loading…</div>}
        {error && <div className="banner error">{error}</div>}
        {gotoOpen && (
          <div className="te-goto">
            <span>Go to line</span>
            <input
              autoFocus
              value={gotoText}
              onChange={(e) => setGotoText(e.target.value)}
              onFocus={(e) => e.currentTarget.select()}
              onKeyDown={(e) => {
                if (e.key === "Enter") gotoLine();
                else if (e.key === "Escape") setGotoOpen(false);
                else return;
                e.preventDefault();
              }}
              onBlur={() => setGotoOpen(false)}
              placeholder={`1–${lines.length.toLocaleString()} (line:column)`}
            />
          </div>
        )}
        {doc?.binary && (
          <div className="empty-state">
            <p>This file looks binary, so it can't be edited as text.</p>
          </div>
        )}
        {!doc && !loading && !error && (
          <div className="empty-state">
            <p>Choose a file above to edit it.</p>
          </div>
        )}
        {hasDoc && (
          <VarList
            ref={list}
            className={`te-list${wordWrap ? " te-wrap" : ""}`}
            count={lines.length}
            rowHeight={ROW_HEIGHT}
            offsets={offsets}
            renderRow={renderRow}
            overlay={renderEditor()}
          />
        )}
      </div>

      <div className="statusbar">
        {doc && !doc.binary && (
          <>
            <span>
              Ln {caret.line + 1}, Col {columnOf(caretText, caret.col, TAB_SIZE) + 1}
            </span>
            {selRange && (
              <span>
                {selRange[1] - selRange[0] + 1} line{selRange[1] > selRange[0] ? "s" : ""} selected
              </span>
            )}
            <span className="muted">
              {lines.length.toLocaleString()} line{lines.length === 1 ? "" : "s"}
            </span>
          </>
        )}
        {doc && (
          <span className="muted">
            {path ? `${doc.info.encoding || doc.format.encoding}${doc.info.hasBom ? " BOM" : ""}` : "New file"}
            {doc.info.lineEnding !== "none" && path ? ` · ${EOL_LABEL[doc.info.lineEnding]}` : ""}
            {path ? ` · ${formatSize(doc.info.size)}` : ""}
            {lang && syntaxOn ? ` · ${lang.name}` : ""}
          </span>
        )}
        {remote && (
          <span className="tc-readonly" title={READ_ONLY_HINT}>
            <Lock size={11} /> read-only
          </span>
        )}
        {dirty && (
          <span className="tc-dirty" title={`Unsaved changes (${shortcutLabel("edit.save")} to save)`}>
            <span className="tc-dirty-dot" /> modified
          </span>
        )}
        {notice && <span className="tc-notice">{notice}</span>}
        {hasDoc && !readOnly && (
          <span className="tc-hint">
            Drag line numbers to select lines · Tab / Shift+Tab indent · {shortcutLabel("edit.duplicateLines")} duplicates ·{" "}
            {shortcutLabel("edit.moveLinesUp")} / {shortcutLabel("edit.moveLinesDown")} move lines
          </span>
        )}
      </div>
    </div>
  );

}

/** The file path field: typing + Enter or browsing opens another file. */
function PathFieldControlled({ path, onOpen }: { path: string; onOpen: (p: string) => void }) {
  const [value, setValue] = useState(path);
  useEffect(() => setValue(path), [path]);
  return (
    <PathField
      value={value}
      onChange={setValue}
      onSubmit={() => value.trim() && onOpen(value.trim())}
      onPick={onOpen}
      directory={false}
      placeholder="File to edit… or remote://profile/path (empty = new file, Save As to name it)"
    />
  );
}
