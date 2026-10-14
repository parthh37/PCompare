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
import { FileDiff, FilePen } from "lucide-react"; // bc5:editor-patch
import { Package } from "lucide-react"; // araxis:text-review
import { CalendarClock } from "lucide-react"; // automation:ui
import { isArchivePath } from "./archiveApi"; // bc5:archives

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
  | "database"
  // bc5:editor-patch
  | "edit"
  | "patch"
  | "review"; // araxis:text-review — a comparison archive (.pcmpz), read-only

export type SessionKind = "home" | "profiles" | "automation" | CompareKind; // automation:ui — automation

export interface SessionInfo {
  label: string;
  icon: LucideIcon;
  /** Accent used for the icon tile; keep these distinguishable at a glance. */
  color: string;
  description: string;
  /** Merge sessions also take an output path and an optional base (ancestor). */
  threeWay?: boolean;
  /** The path pickers choose folders rather than files. */
  directory?: boolean;
  /** Only meaningful on Windows (shown with a badge elsewhere). */
  windowsOnly?: boolean;
  /** bc5:editor-patch — works on one file (`left`); `right` stays empty. */
  single?: boolean;
}

export const SESSIONS: Record<SessionKind, SessionInfo> = {
  home: { label: "Home", icon: House, color: "#8892a4", description: "" },
  profiles: {
    label: "Connection Profiles",
    icon: Globe,
    color: "#38bdf8",
    description: "Connect S3, OneDrive, SharePoint, Dropbox, Google Drive, FTP, SFTP, WebDAV and more.",
  },
  // automation:ui
  automation: {
    label: "Sync Automation",
    icon: CalendarClock,
    color: "#fb7185",
    description: "Run folder syncs on a schedule or when files change, with failure alerts to Teams, email and more.",
  },
  // automation:ui end
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
    description: "2- or 3-way merge with an editable output; the common ancestor is optional.",
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
    description: "2- or 3-way merge of folder trees, with text files merged automatically.",
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
  // bc5:editor-patch
  edit: {
    label: "Text Edit",
    icon: FilePen,
    color: "#818cf8",
    description: "Edit one text file: syntax colours, find & replace, bookmarks, encodings and line endings.",
    single: true,
  },
  patch: {
    label: "Patch Viewer",
    icon: FileDiff,
    color: "#e879f9",
    description: "View a .patch or .diff side by side, and apply it to a folder to compare the result.",
    single: true,
  },
  // araxis:text-review — opened from Home ("Open comparison archive…"), a dropped .pcmpz or the command line.
  review: {
    label: "Comparison Archive",
    icon: Package,
    color: "#facc15",
    description: "Review a saved comparison (.pcmpz) read-only, with its bookmarks and comments.",
    single: true,
  },
  // araxis:text-review end
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
  "edit", // bc5:editor-patch
  "patch",
];

export const isCompareKind = (k: SessionKind): k is CompareKind =>
  k !== "home" && k !== "profiles" && k !== "automation"; // automation:ui — automation

/** Lower-cased extension without the dot ("" when there is none). */
export function extension(path: string): string {
  let name = path.replace(/[\\/]+$/, "").split(/[\\/]/).pop() ?? "";
  // araxis:vcs — `git:/x/a.png@HEAD~1`: the revision isn't part of the name.
  if (/^(git|hg|svn):/i.test(path)) name = name.replace(/@[^@]*$/, "");
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
register("patch", "patch diff"); // bc5:editor-patch
// SQLite files open as ad-hoc Database Compare sides. Plain `.db` stays with
// Hex (many `.db` files aren't SQLite); App sniffs those via `dbApi.isSqlite`.
register("database", "sqlite sqlite3 db3 s3db sl3");
// bc5:converters — PDF and Word .doc (like .docx, .rtf, .odt and binary .dfm)
// open in Text Compare, converted to text by the File formats rules; Hex
// Compare stays one click away ("Open in Hex").
register(
  "hex",
  "bin dat img iso zip 7z rar gz tgz bz2 xz zst jar war apk class o obj lib a so dylib pdb " +
    "ppt msi cab db woff woff2 ttf otf mp4 mov mkv avi",
);

/**
 * Picks the viewer for a pair of *files* (folders always open Folder Compare).
 * Used for drag-and-drop, the command line, and double-clicks in Folder Compare.
 * A specialised viewer is chosen only when both sides agree on it; anything
 * else (unknown or mismatched types) opens in Text Compare.
 */
export function sessionForFiles(left: string, right: string): CompareKind {
  // bc5:archives — two archives (zip, tar.gz, 7z…) compare as folders of their contents; Hex stays in the menus.
  if (isArchivePath(left) && isArchivePath(right)) return "folder";
  const l = VIEWER_BY_EXT[extension(left)];
  const r = VIEWER_BY_EXT[extension(right)];
  // bc5:editor-patch — one-file viewers (a .patch) open alone; a pair of them is a text compare.
  if (l && SESSIONS[l].single && left !== right) return "text";
  return l && l === r ? l : "text";
}
