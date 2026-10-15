// Colour, filled icons: a file icon per
// type (so a tree of mixed files reads at a glance) and toolbar glyphs.
// Everything is drawn on a 16×16 grid and scaled, so icons stay crisp at any size.
import type { ReactNode } from "react";

// ---------- file and folder icons ----------

type FileKind =
  | "text"
  | "code"
  | "markup"
  | "data"
  | "image"
  | "pdf"
  | "doc"
  | "sheet"
  | "slides"
  | "archive"
  | "audio"
  | "video"
  | "exe"
  | "binary"
  | "file";

const KIND_BY_EXT: Record<string, FileKind> = {};
const register = (kind: FileKind, exts: string) => exts.split(" ").forEach((e) => (KIND_BY_EXT[e] = kind));
register("text", "txt log md markdown rst readme license changelog nfo diff patch");
register(
  "code",
  "rs ts tsx js jsx mjs cjs py rb go java kt kts swift c h cc cpp cxx hpp hh cs fs vb php pl lua r dart scala sh bash zsh ps1 bat cmd sql asm m mm zig nim ex exs erl hs clj",
);
register("markup", "html htm xhtml xml xaml svg css scss sass less vue svelte astro");
register("data", "json jsonc json5 yaml yml toml ini cfg conf config env properties lock plist reg editorconfig gitignore gitattributes");
register("image", "png jpg jpeg gif bmp webp ico icns tif tiff heic avif psd raw");
register("pdf", "pdf");
register("doc", "doc docx odt rtf pages");
register("sheet", "csv tsv xls xlsx xlsm ods numbers");
register("slides", "ppt pptx odp key");
register("archive", "zip 7z rar tar gz tgz bz2 xz zst cab iso jar war nupkg whl deb rpm");
register("audio", "mp3 wav flac aac ogg m4a wma aiff opus mid");
register("video", "mp4 mkv mov avi wmv webm m4v flv mpg mpeg");
register("exe", "exe msi app com scr appx msix dmg pkg");
register("binary", "dll so dylib bin dat o obj lib a class pyc wasm pdb sys db sqlite");

function fileKind(name: string): FileKind {
  const lower = name.toLowerCase();
  const dot = lower.lastIndexOf(".");
  const ext = dot > 0 ? lower.slice(dot + 1) : dot === 0 ? lower.slice(1) : lower;
  return KIND_BY_EXT[ext] ?? "file";
}

const INK = "#7b8496";

/** A sheet of paper with a folded corner; `band` tints the lower half for office-style documents. */
function Page({ band, children }: { band?: string; children?: ReactNode }) {
  return (
    <>
      <path d="M3 1.5h6.6L13 4.9v9.6H3z" fill="#f6f8fb" stroke={INK} strokeLinejoin="round" />
      {band && <path d="M3.5 8.5h9v5.5h-9z" fill={band} />}
      <path d="M9.5 1.5V5H13" fill="#d9dee7" stroke={INK} strokeLinejoin="round" />
      {children}
    </>
  );
}

const GLYPH: Record<FileKind, ReactNode> = {
  file: <Page />,
  text: (
    <Page>
      <path d="M5 7.5h6M5 9.5h6M5 11.5h4" stroke="#98a2b3" strokeLinecap="round" />
    </Page>
  ),
  code: (
    <Page>
      <path d="M6.6 7.4 4.9 9.9l1.7 2.5M9.4 7.4l1.7 2.5-1.7 2.5" fill="none" stroke="#2f7cf6" strokeWidth="1.4" strokeLinecap="round" strokeLinejoin="round" />
    </Page>
  ),
  markup: (
    <Page>
      <path d="M6.2 7.6 4.6 9.9l1.6 2.3M9.8 7.6l1.6 2.3-1.6 2.3M8.7 7.2l-1.4 5.4" fill="none" stroke="#ef6c1a" strokeWidth="1.3" strokeLinecap="round" strokeLinejoin="round" />
    </Page>
  ),
  data: (
    <Page>
      <path
        d="M6.8 7.2c-1 0-1.1.5-1.1 1.2s-.2 1.3-1 1.5c.8.2 1 .8 1 1.5s.1 1.2 1.1 1.2M9.2 7.2c1 0 1.1.5 1.1 1.2s.2 1.3 1 1.5c-.8.2-1 .8-1 1.5s-.1 1.2-1.1 1.2"
        fill="none"
        stroke="#8b5cf6"
        strokeWidth="1.2"
        strokeLinecap="round"
      />
    </Page>
  ),
  image: (
    <Page>
      <path d="M4 8h8v5.5H4z" fill="#bfe1ff" />
      <path d="m4 13.5 2.8-3.4 1.9 2 1.3-1.3 2 2.7z" fill="#22a35a" />
      <circle cx="10.3" cy="9.4" r="0.9" fill="#f5b400" />
    </Page>
  ),
  pdf: (
    <Page band="#e5484d">
      <path d="M5 10.5h6M5 12.3h4" stroke="#fff" strokeLinecap="round" />
    </Page>
  ),
  doc: (
    <Page band="#2f6fde">
      <path d="M5 10.3h6M5 12.3h6" stroke="#fff" strokeLinecap="round" />
    </Page>
  ),
  sheet: (
    <Page band="#1f9d55">
      <path d="M3.5 11.2h9M6.5 8.5V14M9.5 8.5V14" stroke="#fff" strokeWidth="0.9" />
    </Page>
  ),
  slides: (
    <Page band="#e8590c">
      <path d="M5.2 9.8h5.6v3H5.2z" fill="#fff" />
    </Page>
  ),
  archive: (
    <Page>
      <path d="M7 1.8h2v1.4H7zM7 4.4h2v1.4H7zM7 7h2v1.4H7z" fill="#b7791f" />
      <path d="M6.4 9.2h3.2v3.6H6.4z" fill="#f2c94c" stroke="#a5671a" strokeWidth="0.8" />
    </Page>
  ),
  audio: (
    <Page>
      <path d="M7.4 12.2V7.6l3.6-.9v4.6" fill="none" stroke="#a24de8" strokeWidth="1.2" strokeLinejoin="round" />
      <circle cx="6.5" cy="12.3" r="1.1" fill="#a24de8" />
      <circle cx="10.1" cy="11.4" r="1.1" fill="#a24de8" />
    </Page>
  ),
  video: (
    <Page>
      <path d="M4.2 8h7.6v5.5H4.2z" fill="#334155" />
      <path d="m7.2 9.3 2.6 1.45-2.6 1.45z" fill="#fff" />
    </Page>
  ),
  exe: (
    <>
      <rect x="1.5" y="2.5" width="13" height="11" rx="1.5" fill="#eef2f8" stroke="#5b6b82" />
      <path d="M2 3.8a1 1 0 0 1 1-1h10a1 1 0 0 1 1 1V5.5H2z" fill="#2f7cf6" />
      <path d="M4 7.5h3.5v4H4z" fill="#9fc3fb" />
      <path d="M9 8h3.5M9 10h3.5" stroke="#8a94a6" strokeLinecap="round" />
    </>
  ),
  binary: (
    <Page>
      <path d="M5 7.4h1.4v1.4H5zM8.6 7.4H10v1.4H8.6zM6.8 9.6h1.4V11H6.8zM10.2 9.6h1.4V11h-1.4zM5 11.8h1.4v1.4H5zM8.6 11.8H10v1.4H8.6z" fill="#64748b" />
    </Page>
  ),
};

interface FileIconProps {
  name: string;
  isDir?: boolean;
  /** Folders: draw the open variant (an expanded tree node). */
  open?: boolean;
  size?: number;
}

/** The icon for one file or folder, chosen from its extension. */
export function FileIcon({ name, isDir, open, size = 16 }: FileIconProps) {
  return (
    <svg className="file-icon" width={size} height={size} viewBox="0 0 16 16" aria-hidden>
      {isDir ? <FolderGlyph open={open} /> : GLYPH[fileKind(name)]}
    </svg>
  );
}

function FolderGlyph({ open }: { open?: boolean }) {
  return (
    <>
      <path d="M1.5 3.5a1 1 0 0 1 1-1h3.7l1.5 1.6h5.8a1 1 0 0 1 1 1v8.4H1.5z" fill="#e3a21a" stroke="#b97b0c" strokeWidth="0.8" strokeLinejoin="round" />
      {open ? (
        <path d="M3.4 6.6h11.4l-1.9 6.9H1.4z" fill="#ffd45e" stroke="#c98b12" strokeWidth="0.8" strokeLinejoin="round" />
      ) : (
        <>
          <path d="M1.5 5.8h13v7.7h-13z" fill="#ffcd4a" stroke="#c98b12" strokeWidth="0.8" strokeLinejoin="round" />
          <path d="M2.2 6.7h11.6" stroke="#ffe596" strokeLinecap="round" />
        </>
      )}
    </>
  );
}

// ---------- toolbar icons ----------

export type ToolIconName =
  | "all"
  | "diffs"
  | "same"
  | "orphans"
  | "expand"
  | "collapse"
  | "flatten"
  | "rules"
  | "swap"
  | "refresh"
  | "sync"
  | "selectLeft"
  | "compareTo"
  | "text"
  | "hex"
  | "copy"
  | "report"
  | "snapshot";

/** A small page used inside toolbar glyphs. */
const MiniPage = ({ x = 0, y = 0 }: { x?: number; y?: number }) => (
  <g transform={`translate(${x} ${y})`}>
    <path d="M2.5 1.5h5.4L10.5 4v8.5h-8z" fill="#f6f8fb" stroke={INK} strokeLinejoin="round" />
    <path d="M7.5 1.5v3h3" fill="#d9dee7" stroke={INK} strokeLinejoin="round" />
  </g>
);

/** A round status badge in the lower right of a toolbar glyph. */
const Badge = ({ color, children }: { color: string; children: ReactNode }) => (
  <>
    <circle cx="11.5" cy="11.5" r="4" fill={color} stroke="#0003" strokeWidth="0.6" />
    <g stroke="#fff" strokeWidth="1.3" strokeLinecap="round" fill="none">
      {children}
    </g>
  </>
);

const TOOL: Record<ToolIconName, ReactNode> = {
  all: (
    <>
      <MiniPage x={4} y={0} />
      <MiniPage x={1} y={2.5} />
    </>
  ),
  diffs: (
    <>
      <MiniPage />
      <Badge color="#e5484d">
        <path d="M9.6 10.4h3.8M9.6 12.6h3.8M12.6 9.2l-2.2 4.6" strokeWidth="1.1" />
      </Badge>
    </>
  ),
  same: (
    <>
      <MiniPage />
      <Badge color="#1f9d55">
        <path d="M9.7 10.6h3.6M9.7 12.4h3.6" />
      </Badge>
    </>
  ),
  orphans: (
    <>
      <MiniPage />
      <Badge color="#9d5ce6">
        <path d="M9.6 11.5h3.6M11.8 10l1.5 1.5-1.5 1.5" />
      </Badge>
    </>
  ),
  expand: (
    <>
      <path d="M3.5 2.5v9h4M3.5 6.5h4" fill="none" stroke={INK} />
      <rect x="1" y="0.8" width="5" height="3.4" rx="0.6" fill="#ffcd4a" stroke="#c98b12" strokeWidth="0.7" />
      <Badge color="#2f7cf6">
        <path d="M9.7 11.5h3.6M11.5 9.7v3.6" />
      </Badge>
    </>
  ),
  collapse: (
    <>
      <path d="M3.5 2.5v9h4M3.5 6.5h4" fill="none" stroke={INK} />
      <rect x="1" y="0.8" width="5" height="3.4" rx="0.6" fill="#ffcd4a" stroke="#c98b12" strokeWidth="0.7" />
      <Badge color="#2f7cf6">
        <path d="M9.7 11.5h3.6" />
      </Badge>
    </>
  ),
  flatten: (
    <>
      {[2, 6.5, 11].map((y) => (
        <g key={y}>
          <rect x="1.5" y={y} width="3" height="3" rx="0.5" fill="#f6f8fb" stroke={INK} strokeWidth="0.8" />
          <path d={`M6.5 ${y + 1.5}h8`} stroke="#2f7cf6" strokeWidth="1.6" strokeLinecap="round" />
        </g>
      ))}
    </>
  ),
  rules: (
    <>
      <circle cx="8" cy="8" r="5.4" fill="none" stroke="#6b7a90" strokeWidth="2.6" strokeDasharray="2.12 2.12" />
      <circle cx="8" cy="8" r="4.4" fill="#9aa8bc" />
      <circle cx="8" cy="8" r="1.7" fill="#eef2f8" />
    </>
  ),
  swap: (
    <>
      <path d="M2 5h10.5M10 2.5 12.5 5 10 7.5" fill="none" stroke="#2f7cf6" strokeWidth="1.8" strokeLinecap="round" strokeLinejoin="round" />
      <path d="M14 11H3.5M6 8.5 3.5 11 6 13.5" fill="none" stroke="#ef8a1a" strokeWidth="1.8" strokeLinecap="round" strokeLinejoin="round" />
    </>
  ),
  refresh: (
    <>
      <path d="M13 6.5A5.2 5.2 0 0 0 3.6 5" fill="none" stroke="#1f9d55" strokeWidth="1.9" strokeLinecap="round" />
      <path d="M2.4 2.4v3.4h3.4z" fill="#1f9d55" stroke="#1f9d55" strokeLinejoin="round" />
      <path d="M3 9.5A5.2 5.2 0 0 0 12.4 11" fill="none" stroke="#1f9d55" strokeWidth="1.9" strokeLinecap="round" />
      <path d="M13.6 13.6v-3.4h-3.4z" fill="#1f9d55" stroke="#1f9d55" strokeLinejoin="round" />
    </>
  ),
  sync: (
    <>
      <path d="M.8 3.2a.7.7 0 0 1 .7-.7h2.2l1 1h2.1a.7.7 0 0 1 .7.7v4.3H.8z" fill="#ffcd4a" stroke="#c98b12" strokeWidth="0.7" />
      <path d="M8.5 7.7a.7.7 0 0 1 .7-.7h2.2l1 1h2.1a.7.7 0 0 1 .7.7v4.3H8.5z" fill="#ffcd4a" stroke="#c98b12" strokeWidth="0.7" />
      <path d="M3.5 10.5c0 2 1.5 3 4 3M6.5 12l1.2 1.5L6.2 14.8" fill="none" stroke="#1f9d55" strokeWidth="1.4" strokeLinecap="round" strokeLinejoin="round" />
      <path d="M12.5 5.5c0-2-1.5-3-4-3M9.5 1.1 8.3 2.5l1.4 1.3" fill="none" stroke="#1f9d55" strokeWidth="1.4" strokeLinecap="round" strokeLinejoin="round" />
    </>
  ),
  selectLeft: (
    <>
      <MiniPage x={4} />
      <path d="M4 8H.8M2.5 6 .6 8l1.9 2" fill="none" stroke="#2f7cf6" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round" />
    </>
  ),
  compareTo: (
    <>
      <MiniPage x={-1} y={1} />
      <MiniPage x={5.5} y={2.5} />
    </>
  ),
  text: (
    <>
      <MiniPage x={2} />
      <path d="M6.5 7h4M6.5 9h4M6.5 11h2.5" stroke="#2f7cf6" strokeLinecap="round" />
    </>
  ),
  hex: (
    <>
      <MiniPage x={2} />
      <path d="M6 6.5h1.3v1.3H6zM8.7 6.5H10v1.3H8.7zM7.3 9h1.3v1.3H7.3zM6 11.2h1.3v1.3H6zM8.7 11.2H10v1.3H8.7z" fill="#64748b" />
    </>
  ),
  copy: (
    <>
      <MiniPage x={3.5} y={-0.5} />
      <MiniPage x={0.5} y={3} />
    </>
  ),
  // a page with a small bar chart.
  report: (
    <>
      <MiniPage x={2} />
      <path d="M6.5 11.5V9M8.5 11.5V6.8M10.5 11.5V8" stroke="#2f7cf6" strokeWidth="1.4" strokeLinecap="round" />
    </>
  ),
  // a camera: "Save snapshot"
  snapshot: (
    <>
      <path d="M1.5 5.5h3l1.2-2h4.6l1.2 2h3v8h-13z" fill="#e8ecf3" stroke={INK} strokeLinejoin="round" />
      <circle cx="8" cy="9.3" r="2.6" fill="#2f7cf6" stroke={INK} strokeWidth="0.8" />
      <circle cx="8" cy="9.3" r="1" fill="#cfe0ff" />
    </>
  ),
};

/** A colour toolbar/menu glyph. */
export function ToolIcon({ name, size = 20 }: { name: ToolIconName; size?: number }) {
  return (
    <svg className="tool-icon" width={size} height={size} viewBox="0 0 16 16" aria-hidden>
      {TOOL[name]}
    </svg>
  );
}
