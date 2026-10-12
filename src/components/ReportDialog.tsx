// "Report…" for Text, Folder and Table Compare: pick a layout, format and
// options, then save the report to a file, copy it, or print it.
import { save } from "@tauri-apps/plugin-dialog";
import { Copy, Printer, Save } from "lucide-react";
import { useState } from "react";
import { errorMessage, type TextDiffOptions } from "../lib/api";
import { basename } from "../lib/format";
import { DISPLAY_FILTERS, type DisplayFilter } from "../lib/folderTree";
import {
  copyReport,
  DEFAULT_TEXT_REPORT,
  loadReportOptions,
  printHtml,
  PRINTS_IN_APP,
  reportApi,
  saveReportOptions,
  textAsHtml,
  type FolderLayout,
  type FolderReportFormat,
  type FolderReportInput,
  type FolderReportOptions,
  type ReportSide,
  type TableFilter,
  type TableReportFormat,
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
  text: { ext: "txt", name: "Text report" },
  patch: { ext: "diff", name: "Unified patch" },
  csv: { ext: "csv", name: "CSV" },
};

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
}

export function ReportDialog({ source, onClose }: Props) {
  const [text, setText] = useState<TextReportOptions>(() => ({
    ...loadReportOptions("text", DEFAULT_TEXT_REPORT),
    title: "",
  }));
  const [folder, setFolder] = useState<FolderChoices>(() => loadReportOptions("folder", DEFAULT_FOLDER));
  const [table, setTable] = useState<TableChoices>(() => loadReportOptions("table", DEFAULT_TABLE));
  const [title, setTitle] = useState("");
  const [busy, setBusy] = useState(false);
  const [notice, setNotice] = useState<{ ok: boolean; text: string } | null>(null);

  const format: string =
    source.kind === "text" ? text.format : source.kind === "folder" ? folder.format : table.format;
  const isHtml = format === "html";
  const viewFilterLabel =
    source.kind === "folder"
      ? (DISPLAY_FILTERS.find((f) => f.value === source.filter)?.label ?? "All")
      : source.kind === "table"
        ? TABLE_FILTER_LABEL[source.filter]
        : "";

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
        return reportApi.text(source.left, source.right, source.rules, {
          ...text,
          format: (fmt as TextReportOptions["format"]) ?? text.format,
          title,
        });
      case "folder":
        return reportApi.folder(source.input, folderOptions((fmt as FolderReportFormat) ?? folder.format));
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

  const copy = () =>
    run(async () => {
      const content = await render();
      const plain = isHtml ? await render(source.kind === "table" ? "csv" : "text") : content;
      await copyReport(content, isHtml, plain);
      return "Report copied to the clipboard.";
    });

  const print = () =>
    run(async () => {
      const content = await render();
      const where = await printHtml(isHtml ? content : textAsHtml(reportTitle(), content));
      return where === "browser" ? "The report opened in your browser; print it from there." : null;
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
      if (source.kind === "folder" && folder.links && isHtml) {
        const written = await reportApi.folderSave(source.input, folderOptions(), path, {
          text: { ...text, format: "html" },
          rules: source.rules,
        });
        const skipped = written.skipped.length ? ` · ${written.skipped.length} skipped (binary or unreadable)` : "";
        return `Saved ${basename(path)} with ${written.linked} linked file report${written.linked === 1 ? "" : "s"}${skipped}.`;
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
                  disabled={!text.differencesOnly && text.format !== "patch"}
                  onChange={(e) => setT({ context: Math.max(0, Math.min(1000, Number(e.target.value) || 0)) })}
                />
              </label>
            </fieldset>
            <fieldset>
              <legend>Format</legend>
              {radio("format", "html", text.format, (f) => setT({ format: f }), "HTML", "Diff colours and changed-word highlights")}
              {radio("format", "text", text.format, (f) => setT({ format: f }), "Plain text")}
              {radio("format", "patch", text.format, (f) => setT({ format: f }), "Unified patch", "For patch / git apply; uses the context lines")}
              <div className="report-sub">Options</div>
              {check(text.lineNumbers, (lineNumbers) => setT({ lineNumbers }), "Line numbers", text.format === "patch")}
              {check(
                text.ignoreUnimportant,
                (ignoreUnimportant) => setT({ ignoreUnimportant }),
                "Ignore unimportant differences",
                false,
                "Differences the comparison rules call unimportant don't count (they stay visible, in blue)",
              )}
              {check(text.mono, (mono) => setT({ mono }), "Black and white", text.format !== "html")}
              {check(text.wrap, (wrap) => setT({ wrap }), "Wrap long lines", text.format !== "html")}
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
              <div className="report-sub">Options</div>
              {check(folder.mono, (mono) => setF({ mono }), "Black and white", folder.format !== "html")}
              {check(
                folder.links,
                (links) => setF({ links }),
                "Link to file comparison reports",
                folder.format !== "html",
                "When saving: writes a text-compare report for every differing text file into “<name>_files” next to the report and links it",
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
    </Modal>
  );
}
