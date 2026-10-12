// One registry describing every session type: its label, icon and accent
// colour, and how many paths it takes. The tab bar, Home cards, saved-session
// list and file routing all read from here, so adding a viewer is one entry.
import {
  Binary,
  Cpu,
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
  | "mp3";

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
];

export const isCompareKind = (k: SessionKind): k is CompareKind => k !== "home" && k !== "profiles";

/** Lower-cased extension without the dot ("" when there is none). */
export function extension(path: string): string {
  const name = path.replace(/[\\/]+$/, "").split(/[\\/]/).pop() ?? "";
  const dot = name.lastIndexOf(".");
  return dot > 0 ? name.slice(dot + 1).toLowerCase() : "";
}

/**
 * Picks the viewer for a pair of *files* (folders always open Folder Compare).
 * Used for drag-and-drop, the command line, and double-clicks in Folder Compare.
 */
export function sessionForFiles(left: string, right: string): CompareKind {
  const l = extension(left);
  const r = extension(right);
  // TODO(you): route files to the right viewer by extension — see the chat notes.
  void l;
  void r;
  return "text";
}
