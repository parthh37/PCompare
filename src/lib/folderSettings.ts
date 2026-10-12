// Folder session settings as the UI edits and saves them, and their mapping
// onto the engine's FolderCompareOptions.
import {
  DEFAULT_TEXT_OPTIONS,
  type AlignRule,
  type ContentMethod,
  type FolderCompareOptions,
  type TextDiffOptions,
} from "./api";
import type { DisplayFilter } from "./folderTree";

/** Settings that change what the engine computes (editing them re-runs the compare). */
export interface CompareSettings {
  compareSize: boolean;
  compareTimestamp: boolean;
  timeToleranceSecs: number;
  ignoreDstShift: boolean;
  content: ContentMethod;
  overrideQuickTests: boolean;
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
  /** bc5:archives — list zip/tar/7z files in the trees as folders (Beyond Compare's default: compare them as files). */
  expandArchives: boolean;
}

/** Everything a saved folder session remembers besides its paths. */
export interface FolderSettings extends CompareSettings {
  filter: DisplayFilter;
  flatten: boolean;
  autoRefresh: boolean;
  /** Show the Attributes column. */
  showAttributes: boolean;
  /** Relative paths marked "ignore" (shown as same). */
  ignored: string[];
}

const IS_WINDOWS = typeof navigator !== "undefined" && navigator.userAgent.includes("Windows");

export const DEFAULT_FOLDER_SETTINGS: FolderSettings = {
  compareSize: true,
  compareTimestamp: true,
  timeToleranceSecs: 2,
  ignoreDstShift: false,
  content: "off",
  overrideQuickTests: true,
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
  expandArchives: false, // bc5:archives
  filter: "all",
  flatten: false,
  autoRefresh: false,
  showAttributes: false,
  ignored: [],
};

/** Toolbar shortcuts for the most common quick-test/content combinations. */
export const COMPARE_PRESETS = {
  sizeAndTimestamp: { label: "Size & timestamp", compareSize: true, compareTimestamp: true, content: "off" },
  size: { label: "Size only", compareSize: true, compareTimestamp: false, content: "off" },
  binary: { label: "Binary contents", compareSize: true, compareTimestamp: true, content: "binary" },
  crc: { label: "CRC-32", compareSize: true, compareTimestamp: true, content: "crc" },
  rules: { label: "Rules-based", compareSize: true, compareTimestamp: true, content: "rules" },
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
