// Backups of the files PCompare overwrites or removes (src-tauri/src/backups.rs,
// crates/pcompare-core/src/backups.rs). Every copy, save, sync or merge that
// replaces something keeps the old version in one folder, one backup per
// operation; the Backups tab lists them and puts files back.
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getPrefs, setPrefs, type BackupPrefs } from "./prefs";

/** Who made a change and what it was, as the Backups tab shows it. */
export interface BackupOrigin {
  /** "Folder Compare", "Text Compare", "Folder Sync", … */
  tool: string;
  /** "Copy left → right", "Save right side", … */
  action: string;
  left?: string;
  right?: string;
}

export interface BackupItem {
  /** Where it lived, and where a restore puts it back. */
  original: string;
  /** Where the copy is, relative to the backup's folder; the item's key. */
  stored: string;
  isDir: boolean;
  size: number;
  modifiedMs: number | null;
  reason: "replaced" | "removed";
}

export interface Backup {
  id: string;
  /** The backup's own folder. */
  folder: string;
  timeMs: number;
  tool: string;
  action: string;
  left: string;
  right: string;
  items: BackupItem[];
  size: number;
}

export interface RestoreOutcome {
  original: string;
  stored: string;
  error: string | null;
}

export interface RestoreReport {
  outcomes: RestoreOutcome[];
  /** The backup of what the restore replaced (restore it to undo). */
  undo: string | null;
}

export interface BackupsInfo {
  folder: string;
  defaultFolder: string;
}

/** How long backups can be kept, as offered everywhere. */
export const KEEP_OPTIONS: { days: number; label: string }[] = [
  { days: 7, label: "1 week" },
  { days: 14, label: "2 weeks" },
  { days: 30, label: "1 month" },
  { days: 90, label: "3 months" },
  { days: 180, label: "6 months" },
  { days: 365, label: "1 year" },
  { days: 0, label: "Forever" },
];

export const SIZE_OPTIONS: { mb: number; label: string }[] = [
  { mb: 0, label: "No limit" },
  { mb: 1024, label: "1 GB" },
  { mb: 5 * 1024, label: "5 GB" },
  { mb: 10 * 1024, label: "10 GB" },
  { mb: 50 * 1024, label: "50 GB" },
];

/** "1 month", "Forever", or "45 days" for a hand-edited value. */
export function keepLabel(days: number): string {
  return KEEP_OPTIONS.find((o) => o.days === days)?.label ?? `${days} days`;
}

/** Updates part of the backup settings. */
export function setBackupPrefs(patch: Partial<BackupPrefs>) {
  setPrefs((p) => ({ backups: { ...p.backups, ...patch } }));
}

/** The Backups tab's description of a save, e.g. "Save right side". */
export function saveOrigin(tool: string, side: "left" | "right" | null, left?: string, right?: string): BackupOrigin {
  return { tool, action: side ? `Save ${side} side` : "Save", left, right };
}

export const backupsApi = {
  info: () => invoke<BackupsInfo>("backups_info"),
  list: () => invoke<Backup[]>("backups_list"),
  /** Puts back the items named by `stored` (all of them when empty). */
  restore: (id: string, stored: string[] = []) => invoke<RestoreReport>("backups_restore", { id, stored }),
  remove: (ids: string[]) => invoke<void>("backups_delete", { ids }),
  /** Applies the keep-for and size settings now. */
  prune: () => invoke<{ removed: number; freed: number }>("backups_prune"),
  /** Called with a backup's id whenever backups are made, restored or removed ("" = several). */
  onChanged: (cb: (id: string) => void) => listen<string>("backups-changed", (e) => cb(e.payload)),
};

/** The absolute path of an item's kept copy. */
export function storedPath(b: Backup, item: BackupItem): string {
  const sep = b.folder.includes("\\") && !b.folder.includes("/") ? "\\" : "/";
  return [b.folder.replace(/[\\/]+$/, ""), ...item.stored.split("/")].join(sep);
}

/** Whether new changes are being backed up right now. */
export const backupsOn = () => getPrefs().backups.enabled;
