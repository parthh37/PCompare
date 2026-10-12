// Typed bridge to the report commands in src-tauri/src/report.rs (mirrors
// pcompare-report's option structs), plus printing (running headers and
// footers, print preview) and clipboard helpers.
import { invoke } from "@tauri-apps/api/core";
import type { FolderEntry, LineEnding, TextDiffOptions } from "./api";
import { writeClipboardText } from "./clipboard";
import type { DisplayFilter } from "./folderTree";
import type { TableCompareOptions } from "./tableApi";

// ---------- text ----------

export type TextLayout = "sideBySide" | "unified";
/** `normalDiff` is classic `diff` output (`3c3`, `< ` / `> `); `slideshow` is HTML with one difference per slide. */
export type TextReportFormat = "html" | "slideshow" | "text" | "patch" | "normalDiff" | "xml";

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

/** A bookmarked line (1-based) whose comment HTML reports show next to it. */
export interface TextReportBookmark {
  side: "left" | "right";
  line: number;
  comment?: string | null;
}

/** A bookmarked folder entry (relative path) whose comment HTML/XML reports show on its row. */
export interface FolderReportBookmark {
  path: string;
  comment?: string | null;
}

export type ReportBookmark = TextReportBookmark | FolderReportBookmark;

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
export type FolderReportFormat = "html" | "text" | "csv" | "xml";

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
  text: (
    left: ReportSide,
    right: ReportSide,
    rules: TextDiffOptions,
    options: TextReportOptions,
    bookmarks: TextReportBookmark[] = [],
  ) => invoke<string>("report_text", { left, right, rules, options, bookmarks }),
  folder: (input: FolderReportInput, options: FolderReportOptions, bookmarks: FolderReportBookmark[] = []) =>
    invoke<string>("report_folder", { input, options, bookmarks }),
  /**
   * With `links`, an HTML report writes a text report per differing text file into
   * `<name>_files/` and links it; an XML report embeds each comparison.
   */
  folderSave: (
    input: FolderReportInput,
    options: FolderReportOptions,
    path: string,
    links: { text: TextReportOptions; rules: TextDiffOptions } | null,
    bookmarks: FolderReportBookmark[] = [],
  ) => invoke<WrittenReport>("report_folder_save", { input, options, path, links, bookmarks }),
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

// ---------- print headers and footers ----------

/** Header/footer templates: `|` splits left|center|right (one part is left-aligned, two are left|right). */
export interface PrintTemplate {
  header: string;
  footer: string;
  paper: PaperSize;
}

export type PaperSize = "a4" | "letter";

/** Values for the `{title} {left} {right} {date}` tokens; `{page}` and `{pages}` are filled at print time. */
export interface PrintTokens {
  title: string;
  left: string;
  right: string;
  date: string;
}

export const PRINT_TOKENS_HELP = "Tokens: {title} {left} {right} {date} {page} {pages} · | splits left|center|right";

/** Printable area in CSS px (96/in) with the 14mm × 12mm margins below. */
export const PAPER: Record<PaperSize, { label: string; width: number; height: number; css: string }> = {
  a4: { label: "A4", width: ((210 - 24) / 25.4) * 96, height: ((297 - 28) / 25.4) * 96, css: "A4" },
  letter: { label: "Letter", width: ((215.9 - 24) / 25.4) * 96, height: ((279.4 - 28) / 25.4) * 96, css: "letter" },
};

/** Height (px) reserved on each page for a running header or footer. */
export const RUNNING_SPACE = 30;

const PAGE_TOKEN = /\{(page|pages)\}/g;

function templateParts(template: string, tokens: PrintTokens): string[] {
  if (!template.trim()) return [];
  const fill = (s: string) =>
    s.replace(/\{(title|left|right|date)\}/g, (_, k: keyof PrintTokens) => tokens[k]);
  return template.split("|").slice(0, 3).map((p) => fill(p.trim()));
}

/** Slot names for 1–3 parts. */
const slots = (n: number): ("left" | "center" | "right")[] =>
  n === 1 ? ["left"] : n === 2 ? ["left", "right"] : ["left", "center", "right"];

const cssString = (s: string) => `"${s.replace(/\\/g, "\\\\").replace(/"/g, '\\"').replace(/[\r\n]+/g, " ")}"`;

/** A part as `@page` margin-box content, page numbers as counters. */
function marginContent(part: string): string {
  const out: string[] = [];
  let last = 0;
  for (const m of part.matchAll(PAGE_TOKEN)) {
    if (m.index! > last) out.push(cssString(part.slice(last, m.index)));
    out.push(`counter(${m[1]})`);
    last = m.index! + m[0].length;
  }
  if (last < part.length) out.push(cssString(part.slice(last)));
  return out.join(" ") || '""';
}

/**
 * Chromium 131+ (WebView2, Chrome) draws `@page` margin boxes, which give real
 * page numbers; elsewhere a `position: fixed` header/footer repeats on every
 * printed page instead, and parts with page numbers are left out (no engine
 * fills `counter(page)` outside margin boxes).
 */
const DETECT_MARGIN_BOXES = String.raw`(function(){var u=navigator.userAgent,m=/Chrom(?:e|ium)\/(\d+)/.exec(u);if(m&&+m[1]>=131&&!/Firefox\//.test(u))document.documentElement.classList.add("pc-mb");})();`;

const PRINT_CSS = (paper: PaperSize, margins: string) => `
.pc-run { display: none; }
.pc-frame, .pc-frame > tbody, .pc-frame > tbody > tr, .pc-frame > tbody > tr > td { display: block; padding: 0; }
.pc-frame > thead, .pc-frame > tfoot { display: none; }
.pc-run > span { flex: 1 1 0; min-width: 0; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
.pc-run > .pc-center { text-align: center; }
.pc-run > .pc-right { text-align: right; }
@page { size: ${PAPER[paper].css}; margin: 14mm 12mm; ${margins} }
@media print {
  html:not(.pc-mb) .pc-run { display: flex; gap: 12px; position: fixed; left: 0; right: 0; font: 8.5pt/1.3 "Segoe UI", system-ui, sans-serif; color: #5e697b; background: #fff; }
  html:not(.pc-mb) .pc-head { top: 0; }
  html:not(.pc-mb) .pc-foot { bottom: 0; }
  html:not(.pc-mb) .pc-pg { visibility: hidden; }
  html:not(.pc-mb) .pc-frame { display: table; width: 100%; border-collapse: collapse; }
  html:not(.pc-mb) .pc-frame > thead { display: table-header-group; }
  html:not(.pc-mb) .pc-frame > tfoot { display: table-footer-group; }
  html:not(.pc-mb) .pc-frame > tbody { display: table-row-group; }
  html:not(.pc-mb) .pc-frame > tbody > tr { display: table-row; }
  html:not(.pc-mb) .pc-frame > * > tr > td { display: table-cell; }
  .pc-frame > tbody > tr { break-inside: auto !important; }
  .pc-space { height: ${RUNNING_SPACE}px; }
}
html.pc-preview body { margin: 0; }
html.pc-preview table.diff td { white-space: pre-wrap; overflow-wrap: anywhere; }
html.pc-preview .pc-run { display: flex; gap: 12px; font: 11px/1.3 "Segoe UI", system-ui, sans-serif; color: #5e697b; height: ${RUNNING_SPACE - 8}px; align-items: center; }
html.pc-preview .pc-head { border-bottom: 1px dashed #d3d9e2; margin-bottom: 8px; }
html.pc-preview .pc-foot { border-top: 1px dashed #d3d9e2; margin-top: 8px; }
`;

/**
 * Adds the running header and footer, page-size and print rules to a report
 * page. `preview` lays the header and footer out at the top and bottom of the
 * document (the print preview iframe shows it as one long page).
 */
export function decorateForPrint(html: string, template: PrintTemplate, tokens: PrintTokens, preview = false): string {
  const header = templateParts(template.header, tokens);
  const footer = templateParts(template.footer, tokens);
  const doc = new DOMParser().parseFromString(html, "text/html");
  const margins = [
    ...slots(header.length).map((slot, i) => `@top-${slot} { content: ${marginContent(header[i])}; font: 8.5pt "Segoe UI", system-ui, sans-serif; color: #5e697b; }`),
    ...slots(footer.length).map((slot, i) => `@bottom-${slot} { content: ${marginContent(footer[i])}; font: 8.5pt "Segoe UI", system-ui, sans-serif; color: #5e697b; }`),
  ].join(" ");
  const style = doc.createElement("style");
  style.textContent = PRINT_CSS(template.paper, margins);
  doc.head.appendChild(style);
  if (preview) doc.documentElement.classList.add("pc-preview");

  const running = (cls: string, parts: string[]) => {
    const div = doc.createElement("div");
    div.className = `pc-run ${cls}`;
    div.setAttribute("aria-hidden", "true");
    slots(parts.length).forEach((slot, i) => {
      const span = doc.createElement("span");
      span.className = `pc-${slot}`;
      const part = parts[i];
      if (PAGE_TOKEN.test(part)) span.classList.add("pc-pg");
      PAGE_TOKEN.lastIndex = 0;
      // {page} reads 1 and {pages} is filled in by the preview once measured.
      part.split(PAGE_TOKEN).forEach((piece, j) => {
        if (j % 2 === 0) span.append(piece);
        else {
          const n = doc.createElement("span");
          n.className = piece === "page" ? "pc-pageno" : "pc-pages";
          n.textContent = "1";
          span.append(n);
        }
      });
      div.appendChild(span);
    });
    return div;
  };

  if (header.length || footer.length) {
    const body = doc.body;
    const frame = doc.createElement("table");
    frame.className = "pc-frame";
    const spacer = (tag: "thead" | "tfoot") => {
      const sec = doc.createElement(tag);
      sec.innerHTML = '<tr><td><div class="pc-space"></div></td></tr>';
      return sec;
    };
    const tbody = doc.createElement("tbody");
    const cell = doc.createElement("td");
    tbody.appendChild(doc.createElement("tr")).appendChild(cell);
    while (body.firstChild) cell.appendChild(body.firstChild);
    if (header.length) frame.appendChild(spacer("thead"));
    frame.appendChild(tbody);
    if (footer.length) frame.appendChild(spacer("tfoot"));
    const detect = doc.createElement("script");
    detect.textContent = DETECT_MARGIN_BOXES;
    body.appendChild(detect);
    if (header.length) body.appendChild(running("pc-head", header));
    body.appendChild(frame);
    if (footer.length) body.appendChild(running("pc-foot", footer));
  }
  return `<!DOCTYPE html>
${doc.documentElement.outerHTML}`;
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
