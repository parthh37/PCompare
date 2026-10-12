// bc5:archives — archives, folder snapshots and svn: repositories as folders.
// Mirrors src-tauri/src/archive.rs and pcompare-archive (serde camelCase).
import { Channel, invoke } from "@tauri-apps/api/core";

export type ArchiveFormat = "zip" | "tar" | "tarGz" | "tarBz2" | "tarXz" | "gz" | "bz2" | "xz" | "sevenZ";

/** What a container side is (`null` in [`SideInfo`] for a plain folder). */
export type ContainerKind = { type: "archive"; format: ArchiveFormat } | { type: "snapshot" } | { type: "svn" };

export interface SnapshotSummary {
  files: number;
  folders: number;
  bytes: number;
  /** Folders that couldn't be listed and files whose CRC couldn't be read. */
  errors: number;
}

export interface SnapshotInfo {
  /** The folder the snapshot was taken of. */
  source: string;
  createdMs: number;
  includeCrcs: boolean;
  summary: SnapshotSummary;
}

export interface SideInfo {
  container: ContainerKind | null;
  /** Why the side can't be written to. */
  readOnly: string | null;
  snapshot: SnapshotInfo | null;
  /** For svn: sides, whether the `svn` client is installed. */
  svnAvailable: boolean | null;
}

export const archiveApi = {
  sideInfo: (path: string) => invoke<SideInfo>("archive_side_info", { path }),
  saveSnapshot: (folder: string, output: string, includeCrcs: boolean, onProgress?: (s: SnapshotSummary) => void) => {
    const channel = new Channel<SnapshotSummary>();
    if (onProgress) channel.onmessage = onProgress;
    return invoke<SnapshotSummary>("snapshot_save", { folder, output, includeCrcs, onProgress: channel });
  },
};

/** Archive file name endings that open as folders (keep in step with pcompare-archive's SUFFIXES). */
const ARCHIVE_SUFFIXES = [
  ".tar.gz",
  ".tar.bz2",
  ".tar.xz",
  ".tgz",
  ".taz",
  ".tbz2",
  ".tbz",
  ".txz",
  ".tar",
  ".gz",
  ".bz2",
  ".xz",
  ".7z",
  ".zip",
  ".jar",
  ".war",
  ".ear",
  ".apk",
];

export const SNAPSHOT_EXT = "pcsnap";

/** File-dialog filter for "open an archive or snapshot as a folder". */
export const CONTAINER_FILTERS = [
  { name: "Archives and snapshots", extensions: ["zip", "jar", "war", "ear", "apk", "tar", "gz", "tgz", "bz2", "tbz2", "xz", "txz", "7z", SNAPSHOT_EXT] },
  { name: "Office / OpenDocument packages (as zip)", extensions: ["docx", "xlsx", "pptx", "odt", "ods", "odp", "epub"] },
  { name: "All files", extensions: ["*"] },
];

const nameOf = (path: string) => (path.replace(/[\\/]+$/, "").split(/[\\/]/).pop() ?? "").toLowerCase();

/** An archive file by its name (zip, jar, tar.gz, 7z, ...): compared as a folder of its contents. */
export function isArchivePath(path: string): boolean {
  const name = nameOf(path);
  return ARCHIVE_SUFFIXES.some((s) => name.length > s.length && name.endsWith(s));
}

export const isSnapshotPath = (path: string) => nameOf(path).endsWith(`.${SNAPSHOT_EXT}`);
export const isSvnPath = (path: string) => /^svn:/i.test(path);

/** Opens as a Folder Compare side rather than a file: archives, snapshots, svn: URLs. */
export const isContainerPath = (path: string) => isArchivePath(path) || isSnapshotPath(path) || isSvnPath(path);

export function containerLabel(kind: ContainerKind): string {
  switch (kind.type) {
    case "snapshot":
      return "snapshot";
    case "svn":
      return "Subversion repository";
    case "archive": {
      const names: Record<ArchiveFormat, string> = {
        zip: "zip archive",
        tar: "tar archive",
        tarGz: "tar.gz archive",
        tarBz2: "tar.bz2 archive",
        tarXz: "tar.xz archive",
        gz: "gzip file",
        bz2: "bzip2 file",
        xz: "xz file",
        sevenZ: "7z archive",
      };
      return names[kind.format];
    }
  }
}
