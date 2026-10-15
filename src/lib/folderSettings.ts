// Folder session settings as the UI edits and saves them, and their mapping
// onto the engine's FolderCompareOptions.
import {
  DEFAULT_TEXT_OPTIONS,
  type AlignRule,
  type ContentMethod,
  type FolderCompareOptions,
  type LargeFileMethod,
  type TextDiffOptions,
} from "./api";
import type { DisplayFilter, FolderStructure } from "./folderTree";
import type { FolderBookmark } from "./reviewApi";

/** Settings that change what the engine computes (editing them re-runs the compare). */
export interface CompareSettings {
  compareSize: boolean;
  compareTimestamp: boolean;
  timeToleranceSecs: number;
  ignoreDstShift: boolean;
  content: ContentMethod;
  overrideQuickTests: boolean;
  /** Pairs with a file larger than this many MB use `largeFileMethod` instead of `content`; null = no limit. */
  largeFileThresholdMb: number | null;
  largeFileMethod: LargeFileMethod;
  rules: TextDiffOptions;
  includeHidden: boolean;
  /** Comma-separated globs, kept as typed. */
  exclude: string;
  includeFiles: string;
  excludeFiles: string;
  excludeFolders: string;
  minSizeKb: number | null;
  maxSizeKb: number | null;
  /** `yyyy-mm-dd` from a date input, or "". */
  modifiedAfter: string;
  modifiedBefore: string;
  /** Text (or regex) a file must / must not contain. */
  includeContaining: string;
  excludeContaining: string;
  containingRegex: boolean;
  containingMatchCase: boolean;
  /** `ATTR` bits that exclude a file. */
  excludeAttributes: number;
  excludeExecutable: boolean;
  /** Attribute differences make files different. */
  compareAttributes: boolean;
  /** `Readme.md` and `README.md` are different files (the norm off Windows). */
  caseSensitiveNames: boolean;
  /** Pair NFC and NFD spellings of the same name. */
  alignUnicode: boolean;
  ignoreExtensions: boolean;
  /** One group of equivalent extensions per line, e.g. `htm, html`. */
  extensionGroups: string;
  /** One `Left* => Right*` wildcard rule per line. */
  alignRules: string;
  followLinks: boolean;
  /** list zip/tar/7z files in the trees as folders (default: compare them as files). */
  expandArchives: boolean;
}

/** Everything a saved folder session remembers besides its paths. */
export interface FolderSettings extends CompareSettings {
  filter: DisplayFilter;
  /** When folders show, whatever the display filter. */
  structure: FolderStructure;
  /** Unimportant differences count as same for the display filters ("Minor"). */
  ignoreMinor: boolean;
  /** The log panel under the folders. */
  showLog: boolean;
  flatten: boolean;
  autoRefresh: boolean;
  /** Show the Attributes column. */
  showAttributes: boolean;
  /** "Suppress filters": show what the filters leave out, in teal. */
  suppressFilters: boolean;
  /** Relative paths marked "ignore" (shown as same). */
  ignored: string[];
  /** bookmarked rows (by relative path) with review comments. */
  bookmarks?: FolderBookmark[];
}

const IS_WINDOWS = typeof navigator !== "undefined" && navigator.userAgent.includes("Windows");

export const DEFAULT_FOLDER_SETTINGS: FolderSettings = {
  compareSize: true,
  compareTimestamp: true,
  timeToleranceSecs: 2,
  ignoreDstShift: false,
  content: "off",
  overrideQuickTests: true,
  largeFileThresholdMb: null,
  largeFileMethod: "binary",
  rules: DEFAULT_TEXT_OPTIONS,
  includeHidden: true,
  exclude: ".git, node_modules",
  includeFiles: "",
  excludeFiles: "",
  excludeFolders: "",
  minSizeKb: null,
  maxSizeKb: null,
  modifiedAfter: "",
  modifiedBefore: "",
  includeContaining: "",
  excludeContaining: "",
  containingRegex: false,
  containingMatchCase: false,
  excludeAttributes: 0,
  excludeExecutable: false,
  compareAttributes: false,
  caseSensitiveNames: !IS_WINDOWS,
  alignUnicode: true,
  ignoreExtensions: false,
  extensionGroups: "",
  alignRules: "",
  followLinks: true,
  expandArchives: false,
  filter: "all",
  structure: "structure",
  ignoreMinor: true,
  showLog: true,
  flatten: false,
  autoRefresh: false,
  showAttributes: false,
  suppressFilters: false,
  ignored: [],
};

/**
 * Toolbar shortcuts for the most common quick-test/content combinations. The
 * first three are named Quick (metadata only), Thorough
 * (contents, honouring the text rules) and Byte-by-byte.
 */
export const COMPARE_PRESETS = {
  sizeAndTimestamp: { label: "Quick — size & timestamp", compareSize: true, compareTimestamp: true, content: "off" },
  rules: { label: "Thorough — rules-based", compareSize: true, compareTimestamp: true, content: "rules" },
  binary: { label: "Byte-by-byte — binary", compareSize: true, compareTimestamp: true, content: "binary" },
  size: { label: "Size only", compareSize: true, compareTimestamp: false, content: "off" },
  crc: { label: "CRC-32", compareSize: true, compareTimestamp: true, content: "crc" },
  version: { label: "Exe/dll version", compareSize: true, compareTimestamp: true, content: "version" },
} as const satisfies Record<string, { label: string; compareSize: boolean; compareTimestamp: boolean; content: ContentMethod }>;
export type ComparePreset = keyof typeof COMPARE_PRESETS;

export function presetOf(s: CompareSettings): ComparePreset | "custom" {
  for (const [key, p] of Object.entries(COMPARE_PRESETS)) {
    if (p.compareSize === s.compareSize && p.compareTimestamp === s.compareTimestamp && p.content === s.content)
      return key as ComparePreset;
  }
  return "custom";
}

/** The three named presets, for the settings dialog's segmented control. */
export const NAMED_PRESETS: { key: ComparePreset; name: string; help: string }[] = [
  { key: "sizeAndTimestamp", name: "Quick", help: "Sizes and timestamps decide; nothing is read." },
  {
    key: "rules",
    name: "Thorough",
    help: "Reads the files and compares text with the text rules (unimportant differences show as similar).",
  },
  { key: "binary", name: "Byte-by-byte", help: "Reads the files and compares every byte." },
];

/** Loads saved settings, upgrading sessions saved before quick tests/content methods existed. */
export function loadFolderSettings(saved: Record<string, unknown> | undefined): FolderSettings {
  const s = { ...DEFAULT_FOLDER_SETTINGS, ...(saved as Partial<FolderSettings> | undefined) };
  s.rules = { ...DEFAULT_TEXT_OPTIONS, ...s.rules };
  const legacy = saved?.criteria;
  if (legacy === "size") Object.assign(s, { compareTimestamp: false, content: "off" });
  else if (legacy === "content") Object.assign(s, { content: "binary" });
  delete (s as Partial<FolderSettings> & { criteria?: unknown }).criteria;
  return s;
}

const globs = (text: string) =>
  text
    .split(",")
    .map((p) => p.trim())
    .filter(Boolean);

/** Local midnight of a `yyyy-mm-dd` date; `endOfDay` makes "before" inclusive. */
function dateMs(value: string, endOfDay: boolean): number | null {
  if (!value) return null;
  const d = new Date(`${value}T00:00:00`);
  if (Number.isNaN(d.getTime())) return null;
  return d.getTime() + (endOfDay ? 86_400_000 - 1 : 0);
}

const kb = (v: number | null) => (v == null ? null : Math.round(v * 1024));

const lines = (text: string) =>
  text
    .split("\n")
    .map((l) => l.trim())
    .filter(Boolean);

/** Parses `Left* => Right*` lines; returns the rules and the lines that aren't rules. */
export function parseAlignRules(text: string): { rules: AlignRule[]; invalid: string[] } {
  const rules: AlignRule[] = [];
  const invalid: string[] = [];
  for (const line of lines(text)) {
    const [left, right, ...rest] = line.split("=>").map((p) => p.trim());
    if (left && right && !rest.length) rules.push({ left, right });
    else invalid.push(line);
  }
  return { rules, invalid };
}

export function toEngineOptions(s: CompareSettings): FolderCompareOptions {
  return {
    compareSize: s.compareSize,
    compareTimestamp: s.compareTimestamp,
    content: s.content,
    overrideQuickTests: s.overrideQuickTests,
    largeFileThreshold: s.largeFileThresholdMb == null ? null : Math.round(s.largeFileThresholdMb * 1024 * 1024),
    largeFileMethod: s.largeFileMethod,
    rules: { ...s.rules, ignorePatterns: s.rules.ignorePatterns.map((p) => p.trim()).filter(Boolean) },
    recursive: true,
    exclude: globs(s.exclude),
    filters: {
      includeFiles: globs(s.includeFiles),
      excludeFiles: globs(s.excludeFiles),
      excludeFolders: globs(s.excludeFolders),
      minSize: kb(s.minSizeKb),
      maxSize: kb(s.maxSizeKb),
      modifiedAfterMs: dateMs(s.modifiedAfter, false),
      modifiedBeforeMs: dateMs(s.modifiedBefore, true),
      excludeAttributes: s.excludeAttributes,
      excludeExecutable: s.excludeExecutable,
      includeContaining: s.includeContaining,
      excludeContaining: s.excludeContaining,
      containingRegex: s.containingRegex,
      containingMatchCase: s.containingMatchCase,
    },
    includeHidden: s.includeHidden,
    ignoreNameCase: !s.caseSensitiveNames,
    align: {
      unicode: s.alignUnicode,
      ignoreExtensions: s.ignoreExtensions,
      extensionGroups: lines(s.extensionGroups),
      rules: parseAlignRules(s.alignRules).rules,
    },
    compareAttributes: s.compareAttributes,
    followLinks: s.followLinks,
    timeToleranceSecs: s.timeToleranceSecs,
    ignoreDstShift: s.ignoreDstShift,
  };
}

/**
 * The toolbar's one-line file filter, e.g. `*.rs;*.ts;-target;-*.tmp`: names
 * to include, then `-` names to exclude (files or folders). `*.*` stands for
 * "every file". Name, size, date and content filters stay in the dialog.
 */
export function formatFilterBox(s: Pick<CompareSettings, "includeFiles" | "exclude">): string {
  const include = globs(s.includeFiles);
  return [...(include.length ? include : ["*.*"]), ...globs(s.exclude).map((g) => `-${g}`)].join(";");
}

/** Reads {@link formatFilterBox}'s format back; `;` and `,` both separate. */
export function parseFilterBox(text: string): Pick<CompareSettings, "includeFiles" | "exclude"> {
  const parts = text
    .split(/[;,]/)
    .map((p) => p.trim())
    .filter(Boolean);
  // A trailing slash marks a folder ("-node_modules/"); the exclude list matches folders by name anyway.
  const bare = (p: string) => p.replace(/[\\/]+$/, "");
  const include = parts.filter((p) => !p.startsWith("-") && p !== "*.*" && p !== "*");
  const exclude = parts.filter((p) => p.startsWith("-")).map((p) => bare(p.slice(1).trim())).filter(Boolean);
  return { includeFiles: include.join(", "), exclude: exclude.join(", ") };
}

/** Ready-made Filters box values, offered in its drop-down. */
export const FILTER_BOX_PRESETS = [
  "*.*",
  "*.*;-.git;-node_modules",
  "*.*;-*.bak;-*.tmp;-*.log;-~*",
  "*.rs;*.ts;*.tsx;*.js;*.css",
  "*.c;*.cpp;*.h;*.hpp",
  "*.cs;*.csproj;*.sln",
  "*.java;*.kt;*.gradle",
  "*.py",
];

/** How many filters beyond the plain exclude list are active — shown on the toolbar. */
export function activeFilterCount(s: CompareSettings): number {
  return [
    s.includeFiles.trim(),
    s.excludeFiles.trim(),
    s.excludeFolders.trim(),
    s.minSizeKb != null,
    s.maxSizeKb != null,
    s.modifiedAfter,
    s.modifiedBefore,
    s.includeContaining.trim(),
    s.excludeContaining.trim(),
    s.excludeAttributes !== 0 || s.excludeExecutable,
  ].filter(Boolean).length;
}

/** How many alignment overrides are active beyond the defaults. */
export function activeAlignCount(s: CompareSettings): number {
  return [
    s.ignoreExtensions,
    s.extensionGroups.trim(),
    parseAlignRules(s.alignRules).rules.length > 0,
    !s.alignUnicode,
  ].filter(Boolean).length;
}
