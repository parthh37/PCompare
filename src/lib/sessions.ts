// One registry describing every session type: its label, icon and accent
// colour, and how many paths it takes. The tab bar, Home cards, saved-session
// list and file routing all read from here, so adding a viewer is one entry.
import {
  Binary,
  Cpu,
  Database,
  FileText,
  FolderGit2,
  FolderSync,
  Folders,
  GitMerge,
  Globe,
  House,
  Image,
  Music,
  Network,
  Table2,
  type LucideIcon,
} from "lucide-react";

export type CompareKind =
  | "text"
  | "merge"
  | "table"
  | "picture"
  | "folder"
  | "sync"
  | "folderMerge"
  | "hex"
  | "registry"
  | "version"
  | "mp3"
  | "database";

export type SessionKind = "home" | "profiles" | CompareKind;

export interface SessionInfo {
  label: string;
  icon: LucideIcon;
  /** Accent used for the icon tile; keep these distinguishable at a glance. */
  color: string;
  description: string;
  /** 3-way sessions also take a base (ancestor) and an output path. */
  threeWay?: boolean;
  /** The path pickers choose folders rather than files. */
  directory?: boolean;
  /** Only meaningful on Windows (shown with a badge elsewhere). */
  windowsOnly?: boolean;
}

export const SESSIONS: Record<SessionKind, SessionInfo> = {
  home: { label: "Home", icon: House, color: "#8892a4", description: "" },
  profiles: {
    label: "Connection Profiles",
    icon: Globe,
    color: "#38bdf8",
    description: "Connect S3, OneDrive, Dropbox, Google Drive, FTP, SFTP, WebDAV and more.",
  },
  text: {
    label: "Text Compare",
    icon: FileText,
    color: "#5b9cff",
    description: "Side-by-side source diff. Copy changes across and save.",
  },
  merge: {
    label: "Text Merge",
    icon: GitMerge,
    color: "#a78bfa",
    description: "3-way merge against a common ancestor with an editable output.",
    threeWay: true,
  },
  table: {
    label: "Table Compare",
    icon: Table2,
    color: "#34d399",
    description: "Cell-by-cell CSV, TSV and Excel comparison with key columns.",
  },
  picture: {
    label: "Picture Compare",
    icon: Image,
    color: "#f472b6",
    description: "Images side-by-side with differing pixels highlighted.",
  },
  folder: {
    label: "Folder Compare",
    icon: Folders,
    color: "#fbbf24",
    description: "Compare two folder trees by size, timestamp or content.",
    directory: true,
  },
  sync: {
    label: "Folder Sync",
    icon: FolderSync,
    color: "#2dd4bf",
    description: "Update, mirror or copy folders with a preview of every operation.",
    directory: true,
  },
  folderMerge: {
    label: "Folder Merge",
    icon: FolderGit2,
    color: "#fb923c",
    description: "3-way merge of folder trees against a common ancestor.",
    threeWay: true,
    directory: true,
  },
  hex: {
    label: "Hex Compare",
    icon: Binary,
    color: "#94a3b8",
    description: "Byte-level comparison of binary files of any size.",
  },
  registry: {
    label: "Registry Compare",
    icon: Network,
    color: "#60a5fa",
    description: "Compare .reg exports or live Windows registry keys.",
  },
  version: {
    label: "Version Compare",
    icon: Cpu,
    color: "#c084fc",
    description: "Compare version information of .exe and .dll files.",
  },
  mp3: {
    label: "MP3 Compare",
    icon: Music,
    color: "#f87171",
    description: "Compare tags and audio properties of MP3 and other audio files.",
  },
  database: {
    label: "Database Compare",
    icon: Database,
    color: "#a3e635",
    description: "Schema and data compare for SQL Server, MySQL, PostgreSQL and SQLite with sync scripts.",
  },
};

/** Order of the "new session" cards on Home. */
export const COMPARE_KINDS: CompareKind[] = [
  "text",
  "merge",
  "table",
  "picture",
  "folder",
  "sync",
  "folderMerge",
  "hex",
  "registry",
  "version",
  "mp3",
  "database",
];

export const isCompareKind = (k: SessionKind): k is CompareKind => k !== "home" && k !== "profiles";

/** Lower-cased extension without the dot ("" when there is none). */
export function extension(path: string): string {
  const name = path.replace(/[\\/]+$/, "").split(/[\\/]/).pop() ?? "";
  const dot = name.lastIndexOf(".");
  return dot > 0 ? name.slice(dot + 1).toLowerCase() : "";
}

// Extensions each engine can open (keep in step with the crates' readers).
const VIEWER_BY_EXT: Record<string, CompareKind> = {};
const register = (kind: CompareKind, exts: string) => {
  for (const ext of exts.split(" ")) VIEWER_BY_EXT[ext] = kind;
};
register("table", "csv tsv tab xlsx xlsm xlsb xls xla xlam ods");
register("picture", "png jpg jpeg gif bmp ico tif tiff webp tga pnm pbm pgm ppm qoi");
register("mp3", "mp3 flac ogg opus m4a aac wav aif aiff wv ape mpc spx");
register("version", "exe dll sys ocx cpl drv scr");
register("registry", "reg");
// SQLite files open as ad-hoc Database Compare sides. Plain `.db` stays with
// Hex (many `.db` files aren't SQLite); App sniffs those via `dbApi.isSqlite`.
register("database", "sqlite sqlite3 db3 s3db sl3");
register(
  "hex",
  "bin dat img iso zip 7z rar gz tgz bz2 xz zst jar war apk class o obj lib a so dylib pdb " +
    "pdf doc ppt msi cab db woff woff2 ttf otf mp4 mov mkv avi",
);

/**
 * Picks the viewer for a pair of *files* (folders always open Folder Compare).
 * Used for drag-and-drop, the command line, and double-clicks in Folder Compare.
 * A specialised viewer is chosen only when both sides agree on it; anything
 * else (unknown or mismatched types) opens in Text Compare.
 */
export function sessionForFiles(left: string, right: string): CompareKind {
  const l = VIEWER_BY_EXT[extension(left)];
  const r = VIEWER_BY_EXT[extension(right)];
  return l && l === r ? l : "text";
}
