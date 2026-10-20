// Patch Viewer: shows a unified/context diff (.patch, .diff) as a file list
// plus a side-by-side view of each file's hunks, and can apply the patch
// against a folder: the patched text opens in Text Compare (in memory) next
// to the original file, with hunks that failed to apply reported. Hunks and
// files can be ticked/unticked and only the ticked ones written to disk.
import { open } from "@tauri-apps/plugin-dialog";
import {
  ArrowDown,
  ArrowUp,
  ChevronDown,
  ChevronUp,
  CircleCheck,
  CircleMinus,
  CircleX,
  FilePen,
  FolderOpen,
  Info,
  ListChecks,
  RefreshCw,
  X,
} from "lucide-react";
import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState, type WheelEvent } from "react";
import { errorMessage, isRemote, type DiffRow, type Segment } from "../lib/api";
import { backupsOn } from "../lib/backupsApi";
import { basename } from "../lib/format";
import { matches as keyMatches, shortcutLabel } from "../lib/keymap";
import {
  applySummary,
  hunkOutcomeText,
  patchApi,
  type ChangeKind,
  type FileApply,
  type FileSelection,
  type HunkOutcome,
  type MemorySide,
  type PatchCheck,
  type PatchFileView,
  type PatchHunkView,
  type PatchView,
  type WriteAction,
  type WrittenFile,
} from "../lib/patchApi";
import { languageFor, lineTokens } from "../lib/syntax";
import { columnOf } from "../lib/textEdit";
import { composeRuns, segmentRanges, type Run, type StyledRange } from "../lib/textRuns";
import { ToolIcon } from "./icons";
import { PathField } from "./PathField";
import { VarList, type VarListHandle } from "./VarList";
import "./TextCompare.css";
import "./PatchViewer.css";

const ROW_HEIGHT = 20;
/** Fallback glyph width until the editor font is measured. */
const CHAR_WIDTH = 7.8;
/** Row geometry; mirrors `.trow` / `.tcell` in styles.css (line numbers, divider, cell's left padding, tab-size). */
const LN_WIDTH = 52;
const MID_WIDTH = 6;
const CELL_PAD = 6;
const TAB_SIZE = 4;
/** Characters in the hidden probe that measures the editor font's glyph width. */
const PROBE_CHARS = 100;
/** File list entry for the patch's description (commit message, mail headers). */
const DESCRIPTION = -1;

/** What a saved Patch Viewer session remembers besides the patch path. */
export interface PatchSettings {
  /** Folder the patch was last applied against. */
  base?: string;
  /** Path components to strip; null/absent = detect. */
  strip?: number | null;
  syntax?: boolean;
}

interface Props {
  /** The patch file. */
  left: string;
  right: string;
  onPathsChange: (left: string, right: string) => void;
  settings?: Partial<PatchSettings>;
  onSettingsChange?: (s: PatchSettings) => void;
  /** Opens a file in Text Edit. */
  onOpenEditor: (path: string) => void;
  /** Opens Text Compare: `left` is a file ("" when in memory), sides in `memory` are texts. */
  onOpenCompare: (left: string, memory: { left?: MemorySide; right?: MemorySide }) => void;
}

const CHANGE_BADGE: Record<ChangeKind, { letter: string; title: string }> = {
  modified: { letter: "M", title: "Modified" },
  added: { letter: "A", title: "New file" },
  deleted: { letter: "D", title: "Deleted" },
  renamed: { letter: "R", title: "Renamed" },
  copied: { letter: "C", title: "Copied" },
};

type Item = { hunk: number } | { hunk: number; row: number };

const WRITE_TEXT: Record<WriteAction, string> = {
  modified: "patched in place",
  created: "created",
  deleted: "deleted (moved to the Trash)",
  renamed: "written under its new name (old file moved to the Trash)",
  copied: "copied to its new name",
};

/** Key of an unticked hunk (`file:hunk`) or of an unticked file without hunks (`file:*`). */
const hunkKey = (file: number, hunk: number | "*") => `${file}:${hunk}`;

/** Whether a hunk failed for real (not just left out). */
const hunkFailed = (h: HunkOutcome) => !h.applied && !h.skipped;

/** A checkbox that can show a partial (some hunks ticked) state. */
function TriCheck({
  state,
  onChange,
  title,
  disabled,
}: {
  state: "all" | "some" | "none";
  onChange: (on: boolean) => void;
  title: string;
  disabled?: boolean;
}) {
  return (
    <input
      type="checkbox"
      className="pv-pick"
      checked={state === "all"}
      ref={(el) => {
        if (el) el.indeterminate = state === "some";
      }}
      disabled={disabled}
      title={title}
      aria-label={title}
      onClick={(e) => e.stopPropagation()}
      onChange={(e) => {
        onChange(e.target.checked);
        // Hand the keyboard back to the viewer's shortcuts (Space, Alt+↓…).
        e.currentTarget.blur();
      }}
    />
  );
}

const segText = (segs: Segment[]) => (segs.length === 1 ? segs[0].text : segs.map((s) => s.text).join(""));
const renderRun = (r: Run, i: number) => (
  <span key={i} className={r.cls || undefined}>
    {r.text}
  </span>
);

function dirOf(path: string): string {
  const i = path.lastIndexOf("/");
  return i > 0 ? path.slice(0, i + 1) : "";
}

/** Why a file has nothing to show side by side. */
function noHunksReason(f: PatchFileView): string {
  if (f.binary) return "Binary file: the patch doesn't carry its contents as text.";
  if (f.change === "renamed") return `Renamed without content changes${f.similarity != null ? ` (${f.similarity}% similar)` : ""}.`;
  if (f.change === "copied") return "Copied without content changes.";
  if (f.oldMode && f.newMode && f.oldMode !== f.newMode) return `Only the file mode changed (${f.oldMode} → ${f.newMode}).`;
  if (f.change === "added") return "New empty file.";
  if (f.change === "deleted") return "Deleted empty file.";
  return "No content changes.";
}

export function PatchViewer({ left: path, onPathsChange, settings, onSettingsChange, onOpenEditor, onOpenCompare }: Props) {
  const [base, setBase] = useState(settings?.base ?? "");
  const [strip, setStrip] = useState<number | null>(settings?.strip ?? null);
  const [syntaxOn, setSyntaxOn] = useState(settings?.syntax ?? true);
  const reportSettings = useRef(onSettingsChange);
  reportSettings.current = onSettingsChange;
  useEffect(() => void reportSettings.current?.({ base, strip, syntax: syntaxOn }), [base, strip, syntaxOn]);

  const [view, setView] = useState<PatchView | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const [sel, setSel] = useState(0);
  const [hunkIdx, setHunkIdx] = useState(0);
  const [check, setCheck] = useState<PatchCheck | null>(null);
  const [checkError, setCheckError] = useState<string | null>(null);
  const [checking, setChecking] = useState(false);
  /** Outcome of the last "Open in Text Compare", shown until dismissed. */
  const [report, setReport] = useState<{ file: string; apply: FileApply } | null>(null);
  /** Unticked hunks/files (see `hunkKey`); everything is ticked after a load. */
  const [unpicked, setUnpicked] = useState<Set<string>>(() => new Set());
  /** Outcome of the last "Apply selected", shown until dismissed. */
  const [written, setWritten] = useState<WrittenFile[] | null>(null);
  const [writing, setWriting] = useState(false);
  /** Bumped to re-run the dry run after files were written. */
  const [checkNonce, setCheckNonce] = useState(0);
  const [hx, setHx] = useState(0);
  const root = useRef<HTMLDivElement>(null);
  const list = useRef<VarListHandle>(null);

  const loadSeq = useRef(0);
  const load = useCallback(async () => {
    const seq = ++loadSeq.current;
    setCheck(null);
    setReport(null);
    setWritten(null);
    setUnpicked(new Set());
    if (!path) {
      setView(null);
      setError(null);
      return;
    }
    setLoading(true);
    try {
      const v = await patchApi.load(path);
      if (seq !== loadSeq.current) return;
      setView(v);
      setError(null);
      setSel((s) => (s < v.files.length ? s : 0));
      setHunkIdx(0);
    } catch (e) {
      if (seq !== loadSeq.current) return;
      setView(null);
      setError(errorMessage(e));
    } finally {
      if (seq === loadSeq.current) setLoading(false);
    }
  }, [path]);
  useEffect(() => {
    load();
  }, [load]);

  // Dry-run the patch against the chosen folder whenever the patch, folder or strip level changes.
  const checkSeq = useRef(0);
  useEffect(() => {
    const seq = ++checkSeq.current;
    if (!view || !base || !path) {
      setCheck(null);
      setCheckError(null);
      return;
    }
    setChecking(true);
    patchApi
      .check(path, base, strip)
      .then((c) => {
        if (seq !== checkSeq.current) return;
        setCheck(c);
        setCheckError(null);
      })
      .catch((e) => {
        if (seq !== checkSeq.current) return;
        setCheck(null);
        setCheckError(errorMessage(e));
      })
      .finally(() => seq === checkSeq.current && setChecking(false));
  }, [view, base, strip, path, checkNonce]);

  const files = view?.files ?? [];
  const hasDescription = !!view?.preamble.trim();
  const file: PatchFileView | null = sel >= 0 ? (files[sel] ?? null) : null;
  const fileCheck = check && sel >= 0 ? check.files[sel] : undefined;
  const lang = useMemo(() => (file ? languageFor(file.display) : null), [file]);

  const items = useMemo(() => {
    const out: Item[] = [];
    file?.hunks.forEach((h, hunk) => {
      out.push({ hunk });
      h.rows.forEach((_, row) => out.push({ hunk, row }));
    });
    return out;
  }, [file]);
  const hunkItem = useMemo(() => {
    const idx: number[] = [];
    items.forEach((it, i) => !("row" in it) && idx.push(i));
    return idx;
  }, [items]);
  /** The widest line in columns (tabs expanded): how far the panes scroll sideways. */
  const maxCols = useMemo(() => {
    let m = 0;
    for (const h of file?.hunks ?? [])
      for (const r of h.rows)
        for (const segs of [r.left, r.right]) {
          if (!segs) continue;
          const text = segText(segs);
          m = Math.max(m, text.includes("\t") ? columnOf(text, text.length, TAB_SIZE) : text.length);
        }
    return m;
  }, [file]);

  // The panes' glyph width (the editor font size is a preference), list width and vertical scrollbar width,
  // which the horizontal scrollbars under the panes leave free.
  const grid = useRef<HTMLDivElement>(null);
  const probe = useRef<HTMLSpanElement>(null);
  const [metrics, setMetrics] = useState({ charW: CHAR_WIDTH, listW: 0, vsb: 0 });
  const hasGrid = !!file?.hunks.length;
  useLayoutEffect(() => {
    const listEl = grid.current?.querySelector<HTMLElement>(".pv-list");
    const probeEl = probe.current;
    if (!listEl || !probeEl) return;
    const measure = () => {
      const charW = probeEl.getBoundingClientRect().width / PROBE_CHARS || CHAR_WIDTH;
      const listW = listEl.clientWidth;
      const vsb = listEl.offsetWidth - listEl.clientWidth;
      setMetrics((m) => (m.charW === charW && m.listW === listW && m.vsb === vsb ? m : { charW, listW, vsb }));
    };
    const ro = new ResizeObserver(measure);
    ro.observe(listEl);
    ro.observe(probeEl);
    measure();
    return () => ro.disconnect();
  }, [hasGrid]);

  // Sideways scrolling: both sides share `hx`, moved by the wheel and the scrollbars under the panes.
  const cellW = (metrics.listW - 2 * LN_WIDTH - MID_WIDTH) / 2;
  /** A cell's text width, with a character of room after the longest line. */
  const textW = CELL_PAD + (maxCols + 1) * metrics.charW;
  const maxHx = metrics.listW > 0 ? Math.max(0, Math.ceil(textW - cellW)) : 0;
  useEffect(() => setHx((x) => Math.min(x, maxHx)), [maxHx]);
  const hbars = useRef<(HTMLDivElement | null)[]>([]);
  useLayoutEffect(() => {
    for (const el of hbars.current) if (el && Math.abs(el.scrollLeft - hx) > 0.5) el.scrollLeft = hx;
  }, [hx, maxHx]);

  const selectFile = (i: number) => {
    if (i < (hasDescription ? DESCRIPTION : 0) || i >= files.length) return;
    setSel(i);
    setHunkIdx(0);
    setHx(0);
    list.current?.scrollToRow(0);
  };

  const goHunk = (i: number) => {
    if (!hunkItem.length) return;
    const clamped = Math.max(0, Math.min(hunkItem.length - 1, i));
    setHunkIdx(clamped);
    list.current?.scrollToRow(hunkItem[clamped]);
  };

  const chooseBase = async () => {
    const dir = await open({ directory: true, multiple: false, title: "Apply the patch against folder", defaultPath: base || undefined });
    if (typeof dir === "string") {
      setBase(dir);
      setReport(null);
    }
  };

  const isPicked = (f: number, h: number | "*") => !unpicked.has(hunkKey(f, h));
  /** Whether a file's hunks are all, some or none ticked. */
  const fileState = (f: number): "all" | "some" | "none" => {
    const hunks = files[f]?.hunks ?? [];
    if (!hunks.length) return isPicked(f, "*") ? "all" : "none";
    const n = hunks.filter((_, h) => isPicked(f, h)).length;
    return n === hunks.length ? "all" : n ? "some" : "none";
  };
  const setPicked = (keys: string[], on: boolean) =>
    setUnpicked((cur) => {
      const next = new Set(cur);
      for (const k of keys) {
        if (on) next.delete(k);
        else next.add(k);
      }
      return next;
    });
  const fileKeys = (f: number) => {
    const hunks = files[f]?.hunks ?? [];
    return hunks.length ? hunks.map((_, h) => hunkKey(f, h)) : [hunkKey(f, "*")];
  };
  /** Ticked hunks of a file, or undefined when all of them are ticked. */
  const pickedHunks = (f: number): number[] | undefined => {
    const hunks = files[f]?.hunks ?? [];
    const picked = hunks.flatMap((_, h) => (isPicked(f, h) ? [h] : []));
    return picked.length === hunks.length ? undefined : picked;
  };
  /** What "Apply selected" would write: ticked hunks of text files, plus ticked bare renames/copies. */
  const selection = useMemo(() => {
    const out: FileSelection[] = [];
    let hunks = 0;
    files.forEach((f, i) => {
      if (f.binary) return;
      if (!f.hunks.length) {
        if ((f.change === "renamed" || f.change === "copied") && !unpicked.has(hunkKey(i, "*"))) out.push({ index: i });
        return;
      }
      const picked = f.hunks.flatMap((_, h) => (unpicked.has(hunkKey(i, h)) ? [] : [h]));
      if (!picked.length) return;
      hunks += picked.length;
      out.push(picked.length === f.hunks.length ? { index: i } : { index: i, hunks: picked });
    });
    return { files: out, hunks };
  }, [files, unpicked]);

  const [opening, setOpening] = useState(false);
  /** Applies the selected file's ticked hunks to its original and opens the result in Text Compare. */
  const openInCompare = async () => {
    if (!file || !check || !base || opening) return;
    setOpening(true);
    try {
      const r = await patchApi.applyFile(path, base, check.strip, sel, pickedHunks(sel));
      if (r.error || r.text === undefined) {
        setReport({ file: file.display, apply: r });
        return;
      }
      setReport(r.hunks.some(hunkFailed) ? { file: file.display, apply: r } : null);
      const label = file.change === "deleted" ? `Deleted: ${file.display}` : `Patched: ${file.display}`;
      onOpenCompare(r.original ?? "", {
        left: r.original ? undefined : { text: "", label: `New file: ${file.display}` },
        right: { text: r.text, label },
      });
    } catch (e) {
      setCheckError(errorMessage(e));
    } finally {
      setOpening(false);
    }
  };

  /** Writes the ticked hunks into the real files, after confirming. */
  const applySelected = async () => {
    if (!check || !base || writing || !selection.files.length) return;
    const n = selection.files.length;
    const what = selection.hunks
      ? `${selection.hunks} hunk${selection.hunks === 1 ? "" : "s"} in ${n} file${n === 1 ? "" : "s"}`
      : `${n} file${n === 1 ? "" : "s"}`;
    const kept = backupsOn()
      ? "The previous versions of changed files, deleted files and the old names of renamed files are kept in Backups."
      : "Deleted files and the old names of renamed files go to the Trash.";
    if (!window.confirm(`Apply ${what} to ${base}?\n\nThe files are changed in place. ${kept}`)) return;
    setWriting(true);
    setReport(null);
    try {
      setWritten(await patchApi.writeSelected(path, base, check.strip, selection.files));
      setCheckError(null);
    } catch (e) {
      setCheckError(errorMessage(e));
    } finally {
      setWriting(false);
      setCheckNonce((x) => x + 1);
    }
  };

  const canOpenCompare = !!file && !!check && !file.binary && !fileCheck?.error;
  const canApplySelected = !!check && !!selection.files.length && !writing;
  const target = fileCheck?.target ?? fileCheck?.original ?? null;

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      // Inactive tabs stay mounted but hidden; only the visible session reacts.
      if (e.defaultPrevented || !root.current?.offsetParent) return;
      const t = e.target as HTMLElement | null;
      if (t && (t.tagName === "INPUT" || t.tagName === "TEXTAREA" || t.tagName === "SELECT")) return;
      if (keyMatches(e, "patch.nextHunk")) goHunk(hunkIdx + 1);
      else if (keyMatches(e, "patch.prevHunk")) goHunk(hunkIdx - 1);
      else if (keyMatches(e, "patch.nextFile")) selectFile(sel + 1);
      else if (keyMatches(e, "patch.prevFile")) selectFile(sel - 1);
      else if (keyMatches(e, "patch.applyFolder")) chooseBase();
      else if (keyMatches(e, "patch.openCompare")) canOpenCompare && openInCompare();
      else if (keyMatches(e, "patch.reload")) load();
      else if (keyMatches(e, "patch.toggleHunk") && sel >= 0 && file?.hunks[hunkIdx])
        setPicked([hunkKey(sel, hunkIdx)], !isPicked(sel, hunkIdx));
      else if (keyMatches(e, "patch.applySelected")) canApplySelected && applySelected();
      else return;
      e.preventDefault();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  });

  const onWheel = (e: WheelEvent<HTMLDivElement>) => {
    const dx = e.shiftKey ? e.deltaY : e.deltaX;
    if (!dx || (!e.shiftKey && Math.abs(e.deltaX) < Math.abs(e.deltaY))) return;
    setHx((x) => Math.max(0, Math.min(maxHx, x + dx)));
  };
  const shift = { transform: `translateX(${-hx}px)` };

  const renderCell = (segs: Segment[] | null) => {
    if (!segs) return <div className="tcell absent" />;
    const text = segText(segs);
    const layers: StyledRange[][] = [segmentRanges(segs, "seg-changed")];
    if (syntaxOn && lang && text.length <= 4000)
      layers.push(lineTokens(text, lang, 0).map((t) => ({ start: t.start, end: t.end, cls: `syn-${t.kind}` })));
    return (
      <div className="tcell">
        <div className="tcontent" style={shift}>
          {composeRuns(text, layers).map(renderRun)}
        </div>
      </div>
    );
  };

  const renderHunkHeader = (h: PatchHunkView, i: number) => {
    const outcome = fileCheck?.hunks[i];
    const on = isPicked(sel, i);
    return (
      <div className={`pv-hunk${i === hunkIdx ? " current" : ""}${on ? "" : " unpicked"}`} onClick={() => setHunkIdx(i)}>
        <TriCheck
          state={on ? "all" : "none"}
          onChange={(v) => setPicked([hunkKey(sel, i)], v)}
          title={`${on ? "Untick" : "Tick"} this hunk for Apply selected / Open in Text Compare (${shortcutLabel("patch.toggleHunk")})`}
        />
        <span className="pv-hunk-range">
          @@ -{h.oldStart},{h.oldLines} +{h.newStart},{h.newLines} @@
        </span>
        {h.section && <span className="pv-hunk-section">{h.section}</span>}
        {outcome && (
          <span className={`pv-outcome ${outcome.applied ? "ok" : "fail"}`}>
            {outcome.applied ? <CircleCheck size={12} /> : <CircleX size={12} />} {hunkOutcomeText(outcome)}
          </span>
        )}
      </div>
    );
  };

  const renderRow = (i: number) => {
    const it = items[i];
    const h = file!.hunks[it.hunk];
    if (!("row" in it)) return renderHunkHeader(h, it.hunk);
    const r: DiffRow = h.rows[it.row];
    return (
      <div
        className={`trow k-${r.kind}${it.hunk === hunkIdx ? " current" : ""}${isPicked(sel, it.hunk) ? "" : " pv-unpicked"}`}
        onClick={() => setHunkIdx(it.hunk)}
      >
        <span className="ln">{r.leftNo ?? ""}</span>
        {renderCell(r.left)}
        <span className="mid" />
        <span className="ln">{r.rightNo ?? ""}</span>
        {renderCell(r.right)}
      </div>
    );
  };

  const fileStatus = (i: number) => {
    const c = check?.files[i];
    if (!c) return null;
    const failed = c.error || c.hunks.some(hunkFailed);
    return (
      <span className={`pv-status ${failed ? "fail" : "ok"}`} title={applySummary(c)}>
        {failed ? <CircleX size={12} /> : <CircleCheck size={12} />}
      </span>
    );
  };

  const totals = files.reduce((t, f) => ({ added: t.added + f.added, removed: t.removed + f.removed }), { added: 0, removed: 0 });
  const cleanFiles = check ? check.files.filter((c) => !c.error && !c.hunks.some(hunkFailed)).length : 0;

  return (
    <div className="session pv-session" ref={root}>
      <div className="toolbar">
        <button onClick={() => selectFile(sel - 1)} disabled={sel <= (hasDescription ? DESCRIPTION : 0)} title={`Previous file (${shortcutLabel("patch.prevFile")})`}>
          <ChevronUp size={14} /> File
        </button>
        <button onClick={() => selectFile(sel + 1)} disabled={sel >= files.length - 1} title={`Next file (${shortcutLabel("patch.nextFile")})`}>
          <ChevronDown size={14} /> File
        </button>
        <span className="sep" />
        <button onClick={() => goHunk(hunkIdx - 1)} disabled={!hunkItem.length} title={`Previous hunk (${shortcutLabel("patch.prevHunk")})`}>
          <ArrowUp size={14} /> Prev
        </button>
        <button onClick={() => goHunk(hunkIdx + 1)} disabled={!hunkItem.length} title={`Next hunk (${shortcutLabel("patch.nextHunk")})`}>
          <ArrowDown size={14} /> Next
        </button>
        <span className="sep" />
        <button
          onClick={chooseBase}
          disabled={!view}
          title={`Pick the folder the patch applies to; each file's hunks are tried against the real files (${shortcutLabel("patch.applyFolder")})`}
        >
          <FolderOpen size={14} /> Apply against folder…
        </button>
        {base && (
          <span className="pv-base" title={base}>
            {basename(base) || base}
            <button className="icon-btn" aria-label="Forget the folder" title="Forget the folder" onClick={() => setBase("")}>
              <X size={11} />
            </button>
          </span>
        )}
        {base && (
          <label title="Leading path components removed from the patch's file names (patch -p)">
            Strip
            <select value={strip ?? ""} onChange={(e) => setStrip(e.target.value === "" ? null : Number(e.target.value))}>
              <option value="">Auto{check && strip === null ? ` (-p${check.strip})` : ""}</option>
              {[0, 1, 2, 3, 4].map((n) => (
                <option key={n} value={n}>
                  -p{n}
                </option>
              ))}
            </select>
          </label>
        )}
        {base && (
          <button
            onClick={applySelected}
            disabled={!canApplySelected}
            title={
              check
                ? `Write the ticked hunks into the files under ${base} (${shortcutLabel("patch.applySelected")}); untick hunks or files to leave them out`
                : "Waiting for the dry run against the folder"
            }
          >
            <ListChecks size={14} /> {writing ? "Applying…" : `Apply selected${selection.hunks ? ` (${selection.hunks})` : ""}`}
          </button>
        )}
        <span className="spacer" />
        <label className="check">
          <input type="checkbox" checked={syntaxOn} onChange={(e) => setSyntaxOn(e.target.checked)} />
          Syntax
        </label>
        <button onClick={() => path && onOpenEditor(path)} disabled={!path || isRemote(path)} title="Open the patch itself in Text Edit">
          <FilePen size={14} /> Edit patch
        </button>
        <button onClick={load} disabled={!path} title={`Reload the patch (${shortcutLabel("patch.reload")})`}>
          <RefreshCw size={14} /> Reload
        </button>
      </div>

      <div className="pathbar pv-pathbar">
        <PatchPathField path={path} onOpen={(p) => p !== path && onPathsChange(p, "")} />
      </div>

      {checkError && <div className="banner error">{checkError}</div>}
      {written && (
        <div className={`pv-report pv-written${written.some((w) => w.error || w.hunks.some(hunkFailed)) ? " error" : ""}`}>
          <span>
            <b>Applied to {basename(base) || base}</b>:{" "}
            {written.filter((w) => w.written).length} of {written.length} file{written.length === 1 ? "" : "s"} changed
          </span>
          <div className="pv-written-list">
            {written.map((w) => (
              <div key={w.index} className="pv-written-file">
                <span className={w.error || w.hunks.some(hunkFailed) ? "pv-fail" : w.written ? "pv-ok" : "muted"}>
                  {w.error || w.hunks.some(hunkFailed) ? <CircleX size={12} /> : <CircleCheck size={12} />}{" "}
                  <b>{files[w.index]?.display ?? `file ${w.index + 1}`}</b>:{" "}
                  {w.error ?? (w.written ? WRITE_TEXT[w.written] : "nothing to change")}
                </span>
                {w.hunks.map((h, i) => (
                  <span key={i} className={`pv-written-hunk ${h.applied ? "ok" : h.skipped ? "skip" : "fail"}`}>
                    {h.applied ? <CircleCheck size={11} /> : h.skipped ? <CircleMinus size={11} /> : <CircleX size={11} />} hunk{" "}
                    {i + 1} {hunkOutcomeText(h)}
                  </span>
                ))}
              </div>
            ))}
          </div>
          <button className="icon-btn" aria-label="Dismiss" onClick={() => setWritten(null)}>
            <X size={12} />
          </button>
        </div>
      )}
      {report && (
        <div className={`pv-report${report.apply.error ? " error" : ""}`}>
          <span>
            <b>{report.file}</b>: {applySummary(report.apply)}
            {!report.apply.error && " — the failed hunks were left out of the patched text:"}
          </span>
          {!report.apply.error && (
            <span className="pv-report-list">
              {report.apply.hunks.map((h, i) =>
                !hunkFailed(h) ? null : (
                  <span key={i}>
                    hunk {i + 1} {hunkOutcomeText(h)}
                  </span>
                ),
              )}
            </span>
          )}
          <button className="icon-btn" aria-label="Dismiss" onClick={() => setReport(null)}>
            <X size={12} />
          </button>
        </div>
      )}

      <div className="session-body pv-body">
        {loading && <div className="overlay">Reading patch…</div>}
        {error && <div className="banner error">{error}</div>}
        {!path && !loading && (
          <div className="empty-state">
            <p>Choose a .patch or .diff file above.</p>
          </div>
        )}
        {view && (
          <div className="pv-split">
            <div className="pv-files" role="listbox" aria-label="Files in the patch">
              {hasDescription && (
                <div className={`pv-file${sel === DESCRIPTION ? " selected" : ""}`} onClick={() => selectFile(DESCRIPTION)}>
                  <span className="pv-badge info">
                    <Info size={11} />
                  </span>
                  <span className="pv-name">Description</span>
                </div>
              )}
              {files.map((f, i) => (
                <div
                  key={i}
                  className={`pv-file${i === sel ? " selected" : ""}`}
                  title={`${CHANGE_BADGE[f.change].title}${f.binary ? " (binary)" : ""}: ${f.display}`}
                  onClick={() => selectFile(i)}
                  // The clicks before it already selected this file.
                  onDoubleClick={() => canOpenCompare && openInCompare()}
                >
                  <TriCheck
                    state={f.binary ? "none" : fileState(i)}
                    disabled={f.binary}
                    onChange={(v) => setPicked(fileKeys(i), v)}
                    title={
                      f.binary
                        ? "Binary changes can't be applied"
                        : `${fileState(i) === "all" ? "Untick" : "Tick"} every hunk of this file for Apply selected`
                    }
                  />
                  <span className={`pv-badge c-${f.change}`}>{f.binary ? "B" : CHANGE_BADGE[f.change].letter}</span>
                  <span className="pv-name">
                    <span className="pv-dir">{dirOf(f.display)}</span>
                    {basename(f.display)}
                  </span>
                  <span className="pv-counts">
                    {f.added > 0 && <span className="k-rightOnly-fg">+{f.added}</span>}
                    {f.removed > 0 && <span className="k-leftOnly-fg">−{f.removed}</span>}
                  </span>
                  {fileStatus(i)}
                </div>
              ))}
            </div>
            <div className="pv-main">
              {sel === DESCRIPTION ? (
                <pre className="pv-description">{view.preamble}</pre>
              ) : file ? (
                <>
                  <div className="pv-filehead">
                    <span className={`pv-badge c-${file.change}`}>{CHANGE_BADGE[file.change].letter}</span>
                    <span className="pv-filepath">
                      {file.change === "renamed" || file.change === "copied" ? (
                        <>
                          {file.oldPath} → {file.newPath}
                        </>
                      ) : (
                        file.display
                      )}
                    </span>
                    {file.oldMode && file.newMode && file.oldMode !== file.newMode && (
                      <span className="muted">
                        mode {file.oldMode} → {file.newMode}
                      </span>
                    )}
                    {fileCheck && (
                      <span className={fileCheck.error || fileCheck.hunks.some(hunkFailed) ? "pv-fail" : "pv-ok"}>
                        {applySummary(fileCheck)}
                      </span>
                    )}
                    <span className="spacer" />
                    {target && !fileCheck?.missing && file.change !== "deleted" && fileCheck?.original && (
                      <button onClick={() => onOpenEditor(fileCheck.original!)} title={`Edit ${fileCheck.original}`}>
                        <FilePen size={14} /> Edit original
                      </button>
                    )}
                    <button
                      className="primary"
                      onClick={check ? openInCompare : chooseBase}
                      disabled={!!check && (!canOpenCompare || opening)}
                      title={
                        check
                          ? `Original on the left, the patched text (in memory) on the right (${shortcutLabel("patch.openCompare")})`
                          : "Pick the folder the patch applies to first"
                      }
                    >
                      <ToolIcon name="text" size={14} /> {opening ? "Applying…" : "Open in Text Compare"}
                    </button>
                  </div>
                  {file.hunks.length ? (
                    <div className="text-grid pv-grid" ref={grid}>
                      <span className="tc-probe" ref={probe} aria-hidden>
                        {"0".repeat(PROBE_CHARS)}
                      </span>
                      <VarList
                        ref={list}
                        className="text-list pv-list"
                        count={items.length}
                        rowHeight={ROW_HEIGHT}
                        renderRow={renderRow}
                        onWheel={onWheel}
                      />
                      {/* one horizontal scrollbar under each side's text, kept in step */}
                      {maxHx > 0 && (
                        <div className="pv-hbar" style={{ paddingRight: metrics.vsb }}>
                          {[0, 1].map((i) => (
                            <div
                              key={i}
                              ref={(el) => void (hbars.current[i] = el)}
                              className={`pv-hscroll pv-hscroll-${i ? "right" : "left"}`}
                              onScroll={(e) => setHx(e.currentTarget.scrollLeft)}
                            >
                              <div style={{ width: textW }} />
                            </div>
                          ))}
                        </div>
                      )}
                    </div>
                  ) : (
                    <div className="empty-state">
                      <p>{noHunksReason(file)}</p>
                    </div>
                  )}
                </>
              ) : (
                <div className="empty-state">
                  <p>Select a file.</p>
                </div>
              )}
            </div>
          </div>
        )}
      </div>

      <div className="statusbar">
        {view && (
          <>
            <span>
              {files.length} file{files.length === 1 ? "" : "s"}
            </span>
            <span className="k-rightOnly-fg">+{totals.added}</span>
            <span className="k-leftOnly-fg">−{totals.removed}</span>
            <span className="muted">
              {view.format === "git" ? "git diff" : view.format === "context" ? "context diff" : "unified diff"} · {view.encoding}
            </span>
            {view.warnings.length > 0 && (
              <span className="pv-fail" title={view.warnings.join("\n")}>
                {view.warnings.length} warning{view.warnings.length === 1 ? "" : "s"}
              </span>
            )}
            {checking && <span className="muted">Checking against {basename(base)}…</span>}
            {check && !checking && (
              <span className={cleanFiles === files.length ? "ok" : "pv-fail"}>
                {cleanFiles} of {files.length} file{files.length === 1 ? "" : "s"} apply cleanly to {basename(base)} (-p{check.strip})
              </span>
            )}
          </>
        )}
      </div>
    </div>
  );
}

function PatchPathField({ path, onOpen }: { path: string; onOpen: (p: string) => void }) {
  const [value, setValue] = useState(path);
  useEffect(() => setValue(path), [path]);
  return (
    <PathField
      value={value}
      onChange={setValue}
      onSubmit={() => value.trim() && onOpen(value.trim())}
      onPick={onOpen}
      directory={false}
      filters={[
        { name: "Patches", extensions: ["patch", "diff"] },
        { name: "All files", extensions: ["*"] },
      ]}
      placeholder="Patch file (.patch, .diff)… or remote://profile/path"
    />
  );
}
