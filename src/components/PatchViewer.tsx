// Patch Viewer: shows a unified/context diff (.patch, .diff) as a file list
// plus a side-by-side view of each file's hunks, and can apply the patch
// against a folder: the patched text opens in Text Compare (in memory) next
// to the original file, with hunks that failed to apply reported.
import { open } from "@tauri-apps/plugin-dialog";
import {
  ArrowDown,
  ArrowUp,
  ChevronDown,
  ChevronUp,
  CircleCheck,
  CircleX,
  FilePen,
  FolderOpen,
  Info,
  RefreshCw,
  X,
} from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState, type WheelEvent } from "react";
import { errorMessage, isRemote, type DiffRow, type Segment } from "../lib/api";
import { basename } from "../lib/format";
import { matches as keyMatches, shortcutLabel } from "../lib/keymap";
import {
  applySummary,
  hunkOutcomeText,
  patchApi,
  type ChangeKind,
  type FileApply,
  type MemorySide,
  type PatchCheck,
  type PatchFileView,
  type PatchHunkView,
  type PatchView,
} from "../lib/patchApi";
import { languageFor, lineTokens } from "../lib/syntax";
import { composeRuns, segmentRanges, type Run, type StyledRange } from "../lib/textRuns";
import { ToolIcon } from "./icons";
import { PathField } from "./PathField";
import { VarList, type VarListHandle } from "./VarList";
import "./TextCompare.css";
import "./PatchViewer.css";

const ROW_HEIGHT = 20;
const CHAR_WIDTH = 7.8;
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
  const [hx, setHx] = useState(0);
  const root = useRef<HTMLDivElement>(null);
  const list = useRef<VarListHandle>(null);

  const loadSeq = useRef(0);
  const load = useCallback(async () => {
    const seq = ++loadSeq.current;
    setCheck(null);
    setReport(null);
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
  }, [view, base, strip, path]);

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
  const maxChars = useMemo(() => {
    let m = 0;
    for (const h of file?.hunks ?? [])
      for (const r of h.rows)
        for (const s of [r.left, r.right]) if (s) m = Math.max(m, s.reduce((n, x) => n + x.text.length, 0));
    return m;
  }, [file]);

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

  const [opening, setOpening] = useState(false);
  /** Applies the selected file's hunks to its original and opens the result in Text Compare. */
  const openInCompare = async () => {
    if (!file || !check || !base || opening) return;
    setOpening(true);
    try {
      const r = await patchApi.applyFile(path, base, check.strip, sel);
      if (r.error || r.text === undefined) {
        setReport({ file: file.display, apply: r });
        return;
      }
      setReport(r.hunks.some((h) => !h.applied) ? { file: file.display, apply: r } : null);
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

  const canOpenCompare = !!file && !!check && !file.binary && !fileCheck?.error;
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
      else return;
      e.preventDefault();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  });

  const onWheel = (e: WheelEvent<HTMLDivElement>) => {
    const dx = e.shiftKey ? e.deltaY : e.deltaX;
    if (!dx || (!e.shiftKey && Math.abs(e.deltaX) < Math.abs(e.deltaY))) return;
    setHx((x) => Math.max(0, Math.min(maxChars * CHAR_WIDTH, x + dx)));
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
    return (
      <div className={`pv-hunk${i === hunkIdx ? " current" : ""}`} onClick={() => setHunkIdx(i)}>
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
      <div className={`trow k-${r.kind}${it.hunk === hunkIdx ? " current" : ""}`} onClick={() => setHunkIdx(it.hunk)}>
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
    const failed = c.error || c.hunks.some((h) => !h.applied);
    return (
      <span className={`pv-status ${failed ? "fail" : "ok"}`} title={applySummary(c)}>
        {failed ? <CircleX size={12} /> : <CircleCheck size={12} />}
      </span>
    );
  };

  const totals = files.reduce((t, f) => ({ added: t.added + f.added, removed: t.removed + f.removed }), { added: 0, removed: 0 });
  const cleanFiles = check ? check.files.filter((c) => !c.error && c.hunks.every((h) => h.applied)).length : 0;

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
      {report && (
        <div className={`pv-report${report.apply.error ? " error" : ""}`}>
          <span>
            <b>{report.file}</b>: {applySummary(report.apply)}
            {!report.apply.error && " — the failed hunks were left out of the patched text:"}
          </span>
          {!report.apply.error && (
            <span className="pv-report-list">
              {report.apply.hunks.map((h, i) =>
                h.applied ? null : (
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
                      <span className={fileCheck.error || fileCheck.hunks.some((h) => !h.applied) ? "pv-fail" : "pv-ok"}>
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
                    <div className="text-grid pv-grid">
                      <VarList
                        ref={list}
                        className="text-list pv-list"
                        count={items.length}
                        rowHeight={ROW_HEIGHT}
                        renderRow={renderRow}
                        onWheel={onWheel}
                      />
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
