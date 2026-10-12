// Typed bridge to the report commands in src-tauri/src/report.rs (mirrors
// pcompare-report's option structs), plus printing and clipboard helpers.
import { invoke } from "@tauri-apps/api/core";
import type { FolderEntry, LineEnding, TextDiffOptions } from "./api";
import { writeClipboardText } from "./clipboard";
import type { DisplayFilter } from "./folderTree";
import type { TableCompareOptions } from "./tableApi";

// ---------- text ----------

export type TextLayout = "sideBySide" | "unified";
export type TextReportFormat = "html" | "text" | "patch";

export interface TextReportOptions {
  layout: TextLayout;
  format: TextReportFormat;
  /** Only differences, each with `context` lines around it. */
  differencesOnly: boolean;
  context: number;
  lineNumbers: boolean;
  /** Unimportant differences don't count as differences. */
  ignoreUnimportant: boolean;
  /** HTML in black and white. */
  mono: boolean;
  /** HTML wraps long lines. */
  wrap: boolean;
  title: string;
}

export const DEFAULT_TEXT_REPORT: TextReportOptions = {
  layout: "sideBySide",
  format: "html",
  differencesOnly: false,
  context: 3,
  lineNumbers: true,
  ignoreUnimportant: true,
  mono: false,
  wrap: true,
  title: "",
};

/** One side as the Text Compare view holds it (unsaved edits included). */
export interface ReportSide {
  lines: string[];
  eols?: LineEnding[] | null;
  /** Picks the grammar; not shown. */
  path?: string | null;
  /** Shown in the report header. */
  title: string;
}

// ---------- folder ----------

export type FolderLayout = "summary" | "detailed";
export type FolderReportFormat = "html" | "text" | "csv";

export interface FolderReportOptions {
  layout: FolderLayout;
  format: FolderReportFormat;
  filter: DisplayFilter;
  nameFilter: string;
  showSize: boolean;
  showModified: boolean;
  mono: boolean;
  title: string;
}

/** What the view shows: roots plus its entries (re-checks and "ignore" marks included). */
export interface FolderReportInput {
  leftRoot: string;
  rightRoot: string;
  entries: FolderEntry[];
}

export interface WrittenReport {
  path: string;
  filesDir: string | null;
  linked: number;
  /** Differing files without a linked report, as "path: reason". */
  skipped: string[];
}

// ---------- table ----------

export type TableReportFormat = "html" | "csv";
export type TableFilter = "all" | "diffs" | "same" | "orphans";

export interface TableReportOptions {
  format: TableReportFormat;
  filter: TableFilter;
  hiddenColumns: number[];
  mono: boolean;
  title: string;
  /** Header labels; empty for the file paths. */
  left: string;
  right: string;
}

export const reportApi = {
  text: (left: ReportSide, right: ReportSide, rules: TextDiffOptions, options: TextReportOptions) =>
    invoke<string>("report_text", { left, right, rules, options }),
  folder: (input: FolderReportInput, options: FolderReportOptions) =>
    invoke<string>("report_folder", { input, options }),
  /** With `links`, writes a text report per differing text file into `<name>_files/` and links it. */
  folderSave: (
    input: FolderReportInput,
    options: FolderReportOptions,
    path: string,
    links: { text: TextReportOptions; rules: TextDiffOptions } | null,
  ) => invoke<WrittenReport>("report_folder_save", { input, options, path, links }),
  table: (left: string, right: string, compare: TableCompareOptions, options: TableReportOptions) =>
    invoke<string>("report_table", { left, right, compare, options }),
  write: (path: string, content: string) => invoke<void>("report_write", { path, content }),
  /** Opens an HTML report in the default browser (optionally straight into its print dialog). */
  openInBrowser: (html: string, print: boolean) => invoke<string>("report_open_in_browser", { html, print }),
};

// ---------- output ----------

const escapeHtml = (s: string) => s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");

/** A plain-text or CSV report as a printable page. */
export function textAsHtml(title: string, text: string): string {
  return `<!DOCTYPE html><html><head><meta charset="utf-8"><title>${escapeHtml(title)}</title>
<style>body{margin:18px}pre{font:12px/1.4 "Cascadia Mono",Consolas,"SF Mono",Menlo,monospace;white-space:pre-wrap;tab-size:4}</style>
</head><body><pre>${escapeHtml(text)}</pre></body></html>`;
}

/**
 * WebView2 (Windows) and WebKitGTK (Linux) print a hidden iframe's document
 * with `print()`. macOS's WKWebView ignores `window.print()`, so there the
 * report opens in the default browser, which shows its print dialog.
 */
export const PRINTS_IN_APP = !/Mac/i.test(navigator.userAgent);

/** Prints an HTML report; resolves to where the print dialog appeared. */
export async function printHtml(html: string): Promise<"app" | "browser"> {
  if (!PRINTS_IN_APP) {
    await reportApi.openInBrowser(html, true);
    return "browser";
  }
  const frame = document.createElement("iframe");
  frame.setAttribute("aria-hidden", "true");
  Object.assign(frame.style, { position: "fixed", right: "0", bottom: "0", width: "0", height: "0", border: "0" });
  await new Promise<void>((resolve) => {
    frame.onload = () => resolve();
    frame.srcdoc = html;
    document.body.appendChild(frame);
  });
  try {
    frame.contentWindow?.focus();
    frame.contentWindow?.print();
  } finally {
    // print() blocks until the dialog closes; give the spooler a moment anyway.
    window.setTimeout(() => frame.remove(), 1000);
  }
  return "app";
}

/** Copies a report: HTML goes on the clipboard as rich text (pastes formatted into Word or mail) with `plain` alongside. */
export async function copyReport(content: string, html: boolean, plain: string): Promise<void> {
  if (html && typeof ClipboardItem !== "undefined" && navigator.clipboard?.write) {
    try {
      await navigator.clipboard.write([
        new ClipboardItem({
          "text/html": new Blob([content], { type: "text/html" }),
          "text/plain": new Blob([plain], { type: "text/plain" }),
        }),
      ]);
      return;
    } catch {
      // Rich clipboard access refused: fall back to text.
    }
  }
  await writeClipboardText(html ? plain : content);
}

/** Remembered dialog choices (`pcompare.report.<kind>`). */
export function loadReportOptions<T extends object>(kind: string, defaults: T): T {
  try {
    const raw = localStorage.getItem(`pcompare.report.${kind}`);
    return raw ? { ...defaults, ...JSON.parse(raw) } : defaults;
  } catch {
    return defaults;
  }
}

export function saveReportOptions(kind: string, options: object) {
  localStorage.setItem(`pcompare.report.${kind}`, JSON.stringify(options));
}
