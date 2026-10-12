// Typed wrappers for the registry / version / audio commands (src-tauri/src/meta.rs).
// Field names mirror the serde `camelCase` structs in crates/pcompare-meta.
import { invoke } from "@tauri-apps/api/core";

export type DiffStatus = "same" | "different" | "leftOnly" | "rightOnly";

// ---------- shared property model ----------

export interface PropertyRow {
  group: string;
  name: string;
  left: string | null;
  right: string | null;
  status: DiffStatus;
}

export interface PropertyStats {
  same: number;
  different: number;
  leftOnly: number;
  rightOnly: number;
}

export interface PropertyCompareResult {
  rows: PropertyRow[];
  stats: PropertyStats;
}

// ---------- version ----------

export interface VersionSummary {
  fileVersion: string | null;
  productVersion: string | null;
  description: string | null;
  architecture: string | null;
}

export interface VersionCompareResult extends PropertyCompareResult {
  left: VersionSummary;
  right: VersionSummary;
}

// ---------- audio ----------

export interface PictureInfo {
  kind: string;
  mime: string | null;
  width: number | null;
  height: number | null;
  size: number;
  /** `data:` URL of the embedded image, when small enough to display. */
  dataUrl: string | null;
}

export interface AudioSummary {
  format: string;
  title: string | null;
  artist: string | null;
  durationMs: number;
  bitrateKbps: number | null;
}

export interface AudioCompareResult extends PropertyCompareResult {
  left: AudioSummary;
  right: AudioSummary;
  leftPictures: PictureInfo[];
  rightPictures: PictureInfo[];
}

// ---------- registry ----------

export interface RegValueDiff {
  /** Empty for the key's default value. */
  name: string;
  leftType: string | null;
  rightType: string | null;
  left: string | null;
  right: string | null;
  /** Full data in the value editor's text form (numbers as 0x hex, binary as spaced hex, multi-strings one per line). */
  leftText: string | null;
  rightText: string | null;
  status: DiffStatus;
}

export interface RegKeyDiff {
  name: string;
  /** Lower-case path from the compare root; unique within a result. */
  id: string;
  /** Full key path on each side (`HKEY_…\…`); a side missing the key gets the path it would have there. */
  leftPath: string;
  rightPath: string;
  /** Status of the whole subtree. */
  status: DiffStatus;
  /** This key's own values (or deletion marker) differ, or it is an orphan. */
  valuesDiffer: boolean;
  leftDeleted: boolean;
  rightDeleted: boolean;
  values: RegValueDiff[];
  children: RegKeyDiff[];
}

export interface DiffCounts {
  same: number;
  different: number;
  leftOnly: number;
  rightOnly: number;
}

export type RegistrySourceKind = "file" | "live" | "remote";

export interface RegistrySourceInfo {
  spec: string;
  kind: RegistrySourceKind;
  keys: number;
  values: number;
  warnings: string[];
}

export interface RegistryCompareResult {
  roots: RegKeyDiff[];
  stats: { keys: DiffCounts; values: DiffCounts };
  leftBase: string;
  rightBase: string;
  left: RegistrySourceInfo;
  right: RegistrySourceInfo;
}

export type RegSide = "left" | "right";

/** One registry change; key paths are full (`HKEY_…\…`). */
export type RegEdit =
  | { op: "setValue"; key: string; name: string; valueType: string; data: string }
  | { op: "deleteValue"; key: string; name: string }
  | { op: "addKey"; key: string }
  | { op: "deleteKey"; key: string }
  /** Makes `to` a recursive copy of `from` on the other side (deletes it when `from` is absent there). */
  | { op: "copyKey"; from: string; to: string }
  /** Makes the value equal to the other side's (deletes it when absent there). */
  | { op: "copyValue"; fromKey: string; toKey: string; name: string };

export interface SideEdit {
  side: RegSide;
  edit: RegEdit;
}

export const metaApi = {
  /**
   * Compares after replaying `edits` (unsaved .reg changes, oldest first).
   * `reload` (default true) re-reads live keys instead of reusing the last read.
   */
  compareRegistry: (left: string, right: string, edits: SideEdit[] = [], reload = true) =>
    invoke<RegistryCompareResult>("compare_registry", { left, right, edits, reload }),
  /** Writes the .reg files of `sides` with `edits` applied, keeping each file's format. */
  registrySave: (left: string, right: string, edits: SideEdit[], sides: RegSide[]) =>
    invoke<void>("registry_save", { left, right, edits, sides }),
  /** The `reg …` commands that would apply `edit` to the live key on `side`. */
  registryLivePlan: (left: string, right: string, edits: SideEdit[], side: RegSide, edit: RegEdit) =>
    invoke<string[]>("registry_live_plan", { left, right, edits, side, edit }),
  /** Applies `edit` to the live key on `side` with reg add / reg delete (Windows only). */
  registryApplyLive: (left: string, right: string, edits: SideEdit[], side: RegSide, edit: RegEdit) =>
    invoke<void>("registry_apply_live", { left, right, edits, side, edit }),
  compareVersions: (left: string, right: string) => invoke<VersionCompareResult>("compare_versions", { left, right }),
  compareAudio: (left: string, right: string) => invoke<AudioCompareResult>("compare_audio", { left, right }),
};
