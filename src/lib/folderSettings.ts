// Folder session settings as the UI edits and saves them, and their mapping
// onto the engine's FolderCompareOptions.
import { DEFAULT_TEXT_OPTIONS, type ContentMethod, type FolderCompareOptions, type TextDiffOptions } from "./api";
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
}

/** Everything a saved folder session remembers besides its paths. */
export interface FolderSettings extends CompareSettings {
  filter: DisplayFilter;
  flatten: boolean;
  autoRefresh: boolean;
}

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
  filter: "all",
  flatten: false,
  autoRefresh: false,
};

/** Toolbar shortcuts for the most common quick-test/content combinations. */
export const COMPARE_PRESETS = {
  sizeAndTimestamp: { label: "Size & timestamp", compareSize: true, compareTimestamp: true, content: "off" },
  size: { label: "Size only", compareSize: true, compareTimestamp: false, content: "off" },
  binary: { label: "Binary contents", compareSize: true, compareTimestamp: true, content: "binary" },
  crc: { label: "CRC-32", compareSize: true, compareTimestamp: true, content: "crc" },
  rules: { label: "Rules-based", compareSize: true, compareTimestamp: true, content: "rules" },
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
    },
    includeHidden: s.includeHidden,
    ignoreNameCase: navigator.userAgent.includes("Windows"),
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
  ].filter(Boolean).length;
}
