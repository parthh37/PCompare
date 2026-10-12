// "Report…" for Text, Folder and Table Compare: pick a layout, format and
// options, then save the report to a file, copy it, preview or print it.
// Printed pages get the running header/footer templates from Settings-backed
// prefs; bookmarks (with comments) from the view are annotated in HTML/XML.
import { save } from "@tauri-apps/plugin-dialog";
import { ArrowLeft, Copy, Eye, Printer, Save } from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";
import { errorMessage, type TextDiffOptions } from "../lib/api";
import { basename } from "../lib/format";
import { DISPLAY_FILTERS, type DisplayFilter } from "../lib/folderTree";
import { usePrefs } from "../lib/prefs";
import {
  copyReport,
  decorateForPrint,
  DEFAULT_TEXT_REPORT,
  loadReportOptions,
  PAPER,
  PRINT_TOKENS_HELP,
  printHtml,
  PRINTS_IN_APP,
  reportApi,
  RUNNING_SPACE,
  saveReportOptions,
  textAsHtml,
  type FolderLayout,
  type FolderReportBookmark,
  type FolderReportFormat,
  type FolderReportInput,
  type FolderReportOptions,
  type PaperSize,
  type PrintTemplate,
  type PrintTokens,
  type ReportBookmark,
  type ReportSide,
  type TableFilter,
  type TableReportFormat,
  type TextReportBookmark,
  type TextReportOptions,
} from "../lib/reportApi";
import type { TableCompareOptions } from "../lib/tableApi";
import { Modal } from "./Modal";
import "./ReportDialog.css";

export type ReportSource =
  | { kind: "text"; left: ReportSide; right: ReportSide; rules: TextDiffOptions }
  | {
      kind: "folder";
      input: FolderReportInput;
      /** The view's display filter and name filter. */
      filter: DisplayFilter;
      nameFilter: string;
      /** Text rules for linked per-file reports. */
      rules: TextDiffOptions;
    }
  | {
      kind: "table";
      left: string;
      right: string;
      compare: TableCompareOptions;
      filter: TableFilter;
      hiddenColumns: number[];
    };

/** Which rows a folder or table report lists. */
type Scope = "view" | "diffs" | "all";

interface FolderChoices {
  layout: FolderLayout;
  format: FolderReportFormat;
  scope: Scope;
  showSize: boolean;
  showModified: boolean;
  mono: boolean;
  links: boolean;
}

const DEFAULT_FOLDER: FolderChoices = {
  layout: "detailed",
  format: "html",
  scope: "view",
  showSize: true,
  showModified: true,
  mono: false,
  links: false,
};

interface TableChoices {
  format: TableReportFormat;
  scope: Scope;
  mono: boolean;
}

const DEFAULT_TABLE: TableChoices = { format: "html", scope: "view", mono: false };

const EXTENSIONS: Record<string, { ext: string; name: string }> = {
  html: { ext: "html", name: "HTML report" },
  slideshow: { ext: "html", name: "HTML slideshow" },
  text: { ext: "txt", name: "Text report" },
  patch: { ext: "diff", name: "Unified patch" },
  normalDiff: { ext: "diff", name: "Normal diff" },
  xml: { ext: "xml", name: "XML report" },
  csv: { ext: "csv", name: "CSV" },
};

/** Formats rendered as HTML pages (printed as they are). */
const HTML_FORMATS = new Set(["html", "slideshow"]);

const KIND_LABEL: Record<ReportSource["kind"], string> = {
  text: "Text Compare",
  folder: "Folder Compare",
  table: "Table Compare",
};

const TABLE_FILTER_LABEL: Record<TableFilter, string> = {
  all: "All rows",
  diffs: "Differences",
  same: "Same",
  orphans: "Orphans",
};

interface Props {
  source: ReportSource;
  onClose: () => void;
  /**
   * The view's bookmarks: `{side, line, comment?}` for Text Compare, `{path, comment?}`
   * for Folder Compare. HTML reports mark them and show their comments; XML lists them.
   */
  bookmarks?: readonly ReportBookmark[];
}

export function ReportDialog({ source, onClose, bookmarks }: Props) {
  const [text, setText] = useState<TextReportOptions>(() => ({
    ...loadReportOptions("text", DEFAULT_TEXT_REPORT),
    title: "",
  }));
  const [folder, setFolder] = useState<FolderChoices>(() => loadReportOptions("folder", DEFAULT_FOLDER));
  const [table, setTable] = useState<TableChoices>(() => loadReportOptions("table", DEFAULT_TABLE));
  const [title, setTitle] = useState("");
  const [busy, setBusy] = useState(false);
  const [notice, setNotice] = useState<{ ok: boolean; text: string } | null>(null);
  const [prefs, setPrefs] = usePrefs();
  /** The page being previewed (before header/footer decoration), with its tokens. */
  const [preview, setPreview] = useState<{ page: string; tokens: PrintTokens } | null>(null);

  const format: string =
    source.kind === "text" ? text.format : source.kind === "folder" ? folder.format : table.format;
  const isHtml = HTML_FORMATS.has(format);
  const viewFilterLabel =
    source.kind === "folder"
      ? (DISPLAY_FILTERS.find((f) => f.value === source.filter)?.label ?? "All")
      : source.kind === "table"
        ? TABLE_FILTER_LABEL[source.filter]
        : "";
  const template: PrintTemplate = { header: prefs.printHeader, footer: prefs.printFooter, paper: prefs.printPaper };

  const textBookmarks: TextReportBookmark[] = (bookmarks ?? [])
    .filter((b): b is TextReportBookmark => "side" in b)
    .map(({ side, line, comment }) => ({ side, line, comment: comment ?? null }));
  const folderBookmarks: FolderReportBookmark[] = (bookmarks ?? [])
    .filter((b): b is FolderReportBookmark => "path" in b)
    .map(({ path, comment }) => ({ path, comment: comment ?? null }));
  const bookmarkCount = source.kind === "text" ? textBookmarks.length : source.kind === "folder" ? folderBookmarks.length : 0;

  const folderOptions = (fmt: FolderReportFormat = folder.format): FolderReportOptions => {
    if (source.kind !== "folder") throw new Error("not a folder report");
    const view = folder.scope === "view";
    return {
      layout: folder.layout,
      format: fmt,
      filter: view ? source.filter : folder.scope === "diffs" ? "diffs" : "all",
      nameFilter: view ? source.nameFilter : "",
      showSize: folder.showSize,
      showModified: folder.showModified,
      mono: folder.mono,
      title,
    };
  };

  /** Renders the report, optionally in another format (for the clipboard's plain-text flavour). */
  const render = async (fmt?: string): Promise<string> => {
    switch (source.kind) {
      case "text":
        return reportApi.text(
          source.left,
          source.right,
          source.rules,
          { ...text, format: (fmt as TextReportOptions["format"]) ?? text.format, title },
          textBookmarks,
        );
      case "folder":
        return reportApi.folder(source.input, folderOptions((fmt as FolderReportFormat) ?? folder.format), folderBookmarks);
      case "table":
        return reportApi.table(source.left, source.right, source.compare, {
          format: (fmt as TableReportFormat) ?? table.format,
          filter: table.scope === "view" ? source.filter : table.scope === "diffs" ? "diffs" : "all",
          hiddenColumns: table.scope === "view" ? source.hiddenColumns : [],
          mono: table.mono,
          title,
          left: "",
          right: "",
        });
    }
  };

  const remember = () => {
    saveReportOptions("text", { ...text, title: "" });
    saveReportOptions("folder", folder);
    saveReportOptions("table", table);
  };

  const run = async (what: () => Promise<string | null>) => {
    setBusy(true);
    setNotice(null);
    remember();
    try {
      const done = await what();
      if (done) setNotice({ ok: true, text: done });
    } catch (e) {
      setNotice({ ok: false, text: errorMessage(e) });
    } finally {
      setBusy(false);
    }
  };

  const reportTitle = () => title.trim() || KIND_LABEL[source.kind];

  const printTokens = (): PrintTokens => {
    const [left, right] =
      source.kind === "text"
        ? [source.left.title, source.right.title]
        : source.kind === "folder"
          ? [source.input.leftRoot, source.input.rightRoot]
          : [source.left, source.right];
    return { title: reportTitle(), left, right, date: new Date().toLocaleString() };
  };

  /** The report as a printable HTML page (plain text, patches, CSV and XML wrapped in <pre>). */
  const printablePage = async () => {
    const content = await render();
    return isHtml ? content : textAsHtml(reportTitle(), content);
  };

  const printPage = async (page: string, tokens: PrintTokens) => {
    const where = await printHtml(decorateForPrint(page, template, tokens));
    return where === "browser" ? "The report opened in your browser; print it from there." : null;
  };

  const copy = () =>
    run(async () => {
      const content = await render();
      const plain = isHtml ? await render(source.kind === "table" ? "csv" : "text") : content;
      await copyReport(content, isHtml, plain);
      return "Report copied to the clipboard.";
    });

  const print = () => run(async () => printPage(await printablePage(), printTokens()));

  const openPreview = () =>
    run(async () => {
      setPreview({ page: await printablePage(), tokens: printTokens() });
      return null;
    });

  const defaultName = () => {
    const stem =
      source.kind === "text"
        ? `${basename(source.left.title) || "text"}-compare`
        : source.kind === "folder"
          ? `${basename(source.input.leftRoot || source.input.rightRoot) || "folder"}-report`
          : `${basename(source.left) || "table"}-compare`;
    return `${stem.replace(/[^\w.-]+/g, "_")}.${EXTENSIONS[format].ext}`;
  };

  const saveAs = () =>
    run(async () => {
      const kind = EXTENSIONS[format];
      const path = await save({
        title: "Save report",
        defaultPath: defaultName(),
        filters: [{ name: kind.name, extensions: [kind.ext] }],
      });
      if (!path) return null;
      if (source.kind === "folder" && folder.links && (format === "html" || format === "xml")) {
        const written = await reportApi.folderSave(
          source.input,
          folderOptions(),
          path,
          { text: { ...text, format: "html" }, rules: source.rules },
          folderBookmarks,
        );
        const skipped = written.skipped.length ? ` · ${written.skipped.length} skipped (binary or unreadable)` : "";
        const what = format === "xml" ? "embedded file comparison" : "linked file report";
        return `Saved ${basename(path)} with ${written.linked} ${what}${written.linked === 1 ? "" : "s"}${skipped}.`;
      }
      await reportApi.write(path, await render());
      return `Saved ${basename(path)}.`;
    });

  const radio = <T extends string>(name: string, value: T, current: T, set: (v: T) => void, label: string, hint?: string) => (
    <label className="field radio">
      <input type="radio" name={name} checked={current === value} onChange={() => set(value)} />
      <span>
        {label}
        {hint && <small className="muted">{hint}</small>}
      </span>
    </label>
  );

  const check = (checked: boolean, set: (v: boolean) => void, label: string, disabled = false, hint?: string) => (
    <label className="field check" title={hint}>
      <input type="checkbox" checked={checked} disabled={disabled} onChange={(e) => set(e.target.checked)} />
      {label}
    </label>
  );

  const setT = (patch: Partial<TextReportOptions>) => setText((t) => ({ ...t, ...patch }));
  const setF = (patch: Partial<FolderChoices>) => setFolder((f) => ({ ...f, ...patch }));
  const setTb = (patch: Partial<TableChoices>) => setTable((t) => ({ ...t, ...patch }));

  /** "Print only pages/sections with changes": differences-only text, or the differences scope. */
  const changesOnly =
    source.kind === "text" ? text.differencesOnly : source.kind === "folder" ? folder.scope === "diffs" : table.scope === "diffs";
  const setChangesOnly = (on: boolean) => {
    if (source.kind === "text") setT({ differencesOnly: on });
    else if (source.kind === "folder") setF({ scope: on ? "diffs" : "view" });
    else setTb({ scope: on ? "diffs" : "view" });
  };

  if (preview) {
    return (
      <PrintPreview
        page={preview.page}
        tokens={preview.tokens}
        template={template}
        onPaper={(printPaper) => setPrefs({ printPaper })}
        onPrint={() => printPage(preview.page, preview.tokens)}
        onBack={() => setPreview(null)}
      />
    );
  }

  const textual = text.format === "patch" || text.format === "normalDiff" || text.format === "xml";
  const linkable = folder.format === "html" || folder.format === "xml";

  return (
    <Modal
      title={`${KIND_LABEL[source.kind]} Report`}
      onClose={onClose}
      wide
      footer={
        <>
          {notice && <span className={`report-notice${notice.ok ? "" : " error"}`}>{notice.text}</span>}
          <button onClick={copy} disabled={busy} title="Copy the report (HTML pastes formatted into Word or mail)">
            <Copy size={14} /> Copy
          </button>
          <button onClick={openPreview} disabled={busy} title="See the printed pages with their header and footer">
            <Eye size={14} /> Preview…
          </button>
          <button
            onClick={print}
            disabled={busy}
            title={PRINTS_IN_APP ? "Print the report" : "Open the report in your browser and print it from there"}
          >
            <Printer size={14} /> Print…
          </button>
          <button className="primary" onClick={saveAs} disabled={busy}>
            <Save size={14} /> Save…
          </button>
        </>
      }
    >
      <div className="settings-grid report-grid">
        {source.kind === "text" && (
          <>
            <fieldset>
              <legend>Layout</legend>
              {radio("layout", "sideBySide", text.layout, (layout) => setT({ layout }), "Side by side")}
              {radio("layout", "unified", text.layout, (layout) => setT({ layout }), "Unified", "Removed lines above added ones")}
              <div className="report-sub">Show</div>
              {radio("show", "all", text.differencesOnly ? "diffs" : "all", () => setT({ differencesOnly: false }), "All lines")}
              {radio("show", "diffs", text.differencesOnly ? "diffs" : "all", () => setT({ differencesOnly: true }), "Differences only")}
              <label className="field indent report-context">
                <span>Context lines</span>
                <input
                  type="number"
                  min={0}
                  max={1000}
                  value={text.context}
                  disabled={!text.differencesOnly && text.format !== "patch" && text.format !== "slideshow"}
                  onChange={(e) => setT({ context: Math.max(0, Math.min(1000, Number(e.target.value) || 0)) })}
                />
              </label>
            </fieldset>
            <fieldset>
              <legend>Format</legend>
              {radio("format", "html", text.format, (f) => setT({ format: f }), "HTML", "Diff colours and changed-word highlights")}
              {radio("format", "slideshow", text.format, (f) => setT({ format: f }), "HTML slideshow", "One difference per slide with its context lines; ← / → to step")}
              {radio("format", "text", text.format, (f) => setT({ format: f }), "Plain text")}
              {radio("format", "patch", text.format, (f) => setT({ format: f }), "Unified diff (patch)", "For patch / git apply; uses the context lines")}
              {radio("format", "normalDiff", text.format, (f) => setT({ format: f }), "Normal diff", "Classic diff output: 3c3, 5a6,7 with < / > lines")}
              {radio("format", "xml", text.format, (f) => setT({ format: f }), "XML", "Summary counts and every difference, for other tools")}
              <div className="report-sub">Options</div>
              {check(text.lineNumbers, (lineNumbers) => setT({ lineNumbers }), "Line numbers", textual)}
              {check(
                text.ignoreUnimportant,
                (ignoreUnimportant) => setT({ ignoreUnimportant }),
                "Ignore unimportant differences",
                false,
                "Differences the comparison rules call unimportant don't count (they stay visible, in blue)",
              )}
              {check(text.mono, (mono) => setT({ mono }), "Black and white", !HTML_FORMATS.has(text.format))}
              {check(text.wrap, (wrap) => setT({ wrap }), "Wrap long lines", !HTML_FORMATS.has(text.format))}
            </fieldset>
          </>
        )}

        {source.kind === "folder" && (
          <>
            <fieldset>
              <legend>Layout</legend>
              {radio("layout", "summary", folder.layout, (layout) => setF({ layout }), "Summary", "Status and path of each file")}
              {radio("layout", "detailed", folder.layout, (layout) => setF({ layout }), "Detailed", "Both sides with sizes, dates and status")}
              {check(folder.showSize, (showSize) => setF({ showSize }), "Size column", folder.layout !== "detailed")}
              {check(folder.showModified, (showModified) => setF({ showModified }), "Modified column", folder.layout !== "detailed")}
              <div className="report-sub">Include</div>
              {radio("scope", "view", folder.scope, (scope) => setF({ scope }), `As shown (${viewFilterLabel}${source.nameFilter.trim() ? `, “${source.nameFilter.trim()}”` : ""})`)}
              {radio("scope", "diffs", folder.scope, (scope) => setF({ scope }), "Differences only")}
              {radio("scope", "all", folder.scope, (scope) => setF({ scope }), "Everything")}
            </fieldset>
            <fieldset>
              <legend>Format</legend>
              {radio("format", "html", folder.format, (f) => setF({ format: f }), "HTML")}
              {radio("format", "text", folder.format, (f) => setF({ format: f }), "Plain text")}
              {radio("format", "csv", folder.format, (f) => setF({ format: f }), "CSV", "For spreadsheets")}
              {radio("format", "xml", folder.format, (f) => setF({ format: f }), "XML", "Every row with status, sizes and dates, for other tools")}
              <div className="report-sub">Options</div>
              {check(folder.mono, (mono) => setF({ mono }), "Black and white", folder.format !== "html")}
              {check(
                folder.links,
                (links) => setF({ links }),
                folder.format === "xml" ? "Embed file comparisons" : "Link to file comparison reports",
                !linkable,
                folder.format === "xml"
                  ? "When saving: adds the text comparison of every differing text file to its entry"
                  : "When saving: writes a text-compare report for every differing text file into “<name>_files” next to the report and links it",
              )}
            </fieldset>
          </>
        )}

        {source.kind === "table" && (
          <>
            <fieldset>
              <legend>Include</legend>
              {radio("scope", "view", table.scope, (scope) => setTb({ scope }), `As shown (${viewFilterLabel}${source.hiddenColumns.length ? `, ${source.hiddenColumns.length} hidden column${source.hiddenColumns.length === 1 ? "" : "s"}` : ""})`)}
              {radio("scope", "diffs", table.scope, (scope) => setTb({ scope }), "Differences only")}
              {radio("scope", "all", table.scope, (scope) => setTb({ scope }), "Every row and column")}
            </fieldset>
            <fieldset>
              <legend>Format</legend>
              {radio("format", "html", table.format, (f) => setTb({ format: f }), "HTML", "A grid with differing cells highlighted")}
              {radio("format", "csv", table.format, (f) => setTb({ format: f }), "CSV", "Left and right value of every column")}
              {check(table.mono, (mono) => setTb({ mono }), "Black and white", table.format !== "html")}
            </fieldset>
          </>
        )}
      </div>
      <label className="field report-title">
        <span>Title</span>
        <input value={title} onChange={(e) => setTitle(e.target.value)} placeholder={reportTitle()} spellCheck={false} />
      </label>
      {bookmarkCount > 0 && (
        <div className="report-bookmarks muted">
          {bookmarkCount} bookmark{bookmarkCount === 1 ? "" : "s"} from the view {bookmarkCount === 1 ? "is" : "are"} annotated
          (with {bookmarkCount === 1 ? "its comment" : "their comments"}) in HTML and XML reports.
        </div>
      )}
      <fieldset className="report-print">
        <legend>Printing</legend>
        <label className="field report-template">
          <span>Header</span>
          <input
            value={prefs.printHeader}
            onChange={(e) => setPrefs({ printHeader: e.target.value })}
            placeholder="No header"
            spellCheck={false}
          />
        </label>
        <label className="field report-template">
          <span>Footer</span>
          <input
            value={prefs.printFooter}
            onChange={(e) => setPrefs({ printFooter: e.target.value })}
            placeholder="No footer"
            spellCheck={false}
          />
        </label>
        <small className="muted report-tokens">{PRINT_TOKENS_HELP}</small>
        {check(
          changesOnly,
          setChangesOnly,
          "Print only pages/sections with changes",
          false,
          source.kind === "text"
            ? "Same as Differences only: unchanged runs are left out apart from the context lines"
            : "Same as Include → Differences only",
        )}
      </fieldset>
    </Modal>
  );
}

interface PreviewProps {
  /** The report page before header/footer decoration. */
  page: string;
  tokens: PrintTokens;
  template: PrintTemplate;
  onPaper: (paper: PaperSize) => void;
  onPrint: () => Promise<string | null>;
  onBack: () => void;
}

/**
 * The printed report as one long sheet at the paper's printable width, with
 * dashed guides where pages break (estimated: browsers don't expose their
 * pagination) and the running header and footer at the top and bottom.
 */
function PrintPreview({ page, tokens, template, onPaper, onPrint, onBack }: PreviewProps) {
  const frame = useRef<HTMLIFrameElement>(null);
  const [layout, setLayout] = useState<{ height: number; start: number; usable: number; pages: number } | null>(null);
  const [busy, setBusy] = useState(false);
  const [notice, setNotice] = useState<{ ok: boolean; text: string } | null>(null);
  const { header, footer, paper: paperSize } = template;
  const paper = PAPER[paperSize];
  const html = useMemo(
    () => decorateForPrint(page, { header, footer, paper: paperSize }, tokens, true),
    [page, header, footer, paperSize, tokens],
  );

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.stopPropagation();
        onBack();
      }
    };
    window.addEventListener("keydown", onKey, true);
    return () => window.removeEventListener("keydown", onKey, true);
  }, [onBack]);

  useEffect(() => setLayout(null), [html]);

  const measure = () => {
    const doc = frame.current?.contentDocument;
    if (!doc) return;
    const head = doc.querySelector<HTMLElement>(".pc-head");
    const foot = doc.querySelector<HTMLElement>(".pc-foot");
    const outer = (el: HTMLElement | null) => (el ? el.offsetHeight + 8 : 0);
    const height = doc.documentElement.scrollHeight;
    const usable = paper.height - (head ? RUNNING_SPACE : 0) - (foot ? RUNNING_SPACE : 0);
    const content = Math.max(0, height - outer(head) - outer(foot));
    const pages = Math.max(1, Math.ceil(content / usable));
    doc.querySelectorAll(".pc-pages").forEach((el) => (el.textContent = String(pages)));
    setLayout({ height, start: outer(head), usable, pages });
  };

  const print = async () => {
    setBusy(true);
    setNotice(null);
    try {
      const done = await onPrint();
      if (done) setNotice({ ok: true, text: done });
    } catch (e) {
      setNotice({ ok: false, text: errorMessage(e) });
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="modal-backdrop" onMouseDown={(e) => e.target === e.currentTarget && onBack()}>
      <div className="modal report-preview" role="dialog" aria-label="Print preview">
        <header className="modal-head">
          <span>Print preview</span>
          <span className="muted report-pages">
            {layout ? `≈ ${layout.pages} page${layout.pages === 1 ? "" : "s"} (${paper.label})` : "Measuring…"}
          </span>
        </header>
        <div className="modal-body report-preview-body">
          <div className="report-sheet" style={{ width: paper.width }}>
            <iframe
              ref={frame}
              title="Report preview"
              srcDoc={html}
              onLoad={measure}
              style={{ width: paper.width, height: layout?.height ?? paper.height }}
            />
            {layout &&
              Array.from({ length: layout.pages - 1 }, (_, k) => (
                <div key={k} className="report-guide" style={{ top: `calc(var(--sheet-pad-y) + ${layout.start + (k + 1) * layout.usable}px)` }}>
                  <span>Page {k + 2}</span>
                </div>
              ))}
          </div>
        </div>
        <footer className="modal-foot">
          {notice && <span className={`report-notice${notice.ok ? "" : " error"}`}>{notice.text}</span>}
          <label className="report-paper">
            Paper
            <select value={paperSize} onChange={(e) => onPaper(e.target.value as PaperSize)}>
              {(Object.keys(PAPER) as PaperSize[]).map((p) => (
                <option key={p} value={p}>
                  {PAPER[p].label}
                </option>
              ))}
            </select>
          </label>
          <button onClick={onBack}>
            <ArrowLeft size={14} /> Back
          </button>
          <button
            className="primary"
            onClick={print}
            disabled={busy}
            title={PRINTS_IN_APP ? "Print the report" : "Open the report in your browser and print it from there"}
          >
            <Printer size={14} /> Print…
          </button>
        </footer>
      </div>
    </div>
  );
}
