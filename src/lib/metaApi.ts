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
  status: DiffStatus;
}

export interface RegKeyDiff {
  name: string;
  /** Lower-case path from the compare root; unique within a result. */
  id: string;
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

export const metaApi = {
  compareRegistry: (left: string, right: string) =>
    invoke<RegistryCompareResult>("compare_registry", { left, right }),
  compareVersions: (left: string, right: string) => invoke<VersionCompareResult>("compare_versions", { left, right }),
  compareAudio: (left: string, right: string) => invoke<AudioCompareResult>("compare_audio", { left, right }),
};
