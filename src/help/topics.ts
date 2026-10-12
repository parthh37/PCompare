// Built-in help pages, one per view plus a few general topics. F1 opens the
// page for the tab in front. Bodies use a tiny markdown subset rendered by
// HelpPanel: "## heading", "- bullet", blank-line paragraphs, **bold**, `code`.
// Keyboard shortcuts are NOT written here: each page lists the live bindings
// for its `kinds` from keymap.ts, so rebinding a key updates the help too.
import type { SessionKind } from "../lib/sessions";

export interface HelpTopic {
  id: string;
  title: string;
  /** Views this page documents; F1 in such a tab opens it. */
  kinds: SessionKind[];
  /** List the global (app-wide) shortcuts too. */
  globalKeys?: boolean;
  body: string;
}

export const HELP_TOPICS: HelpTopic[] = [
  {
    id: "home",
    title: "Getting started",
    kinds: ["home"],
    globalKeys: true,
    body: `
Pick a session type on Home, or drop two files or folders anywhere on the window to compare them. Dropping one item fills the empty side of the compare in front, or opens it on its own.

## Opening comparisons
- **Command line**: \`pcompare <left> <right>\` picks the viewer from the paths; \`pcompare <left> <right> <base> [<output>]\` opens a 3-way merge (Beyond Compare's order, usable as a git mergetool).
- Launching PCompare again while it runs opens the new comparison as a tab in the existing window.
- **Select Left Side** in a file list (or Explorer) remembers one path; **Compare to…** on another item compares the two.

## Home lists
- **Workspaces**: named sets of open tabs. Open, rename or delete them here; save one with the **Workspace** button in the tab bar.
- **Saved sessions**: comparisons saved with a name (Save session). Double-click or click the name to reopen.
- **Recent sessions**: comparisons you closed without saving, kept automatically (newest 20) while *Auto-save sessions on close* is on.
`,
  },
  {
    id: "sessions",
    title: "Sessions, workspaces and startup",
    kinds: [],
    body: `
## Saved sessions
**Save session** (tab bar) stores what a tab compares and its view settings under a name. Saving again updates it; **Save as new** forks it.

## Auto-save on close
With *Settings → General → Auto-save sessions on close* (on by default, like Beyond Compare):
- closing a tab opened from a saved session writes its current paths and settings back into that session;
- closing any other comparison adds it to **Recent sessions** on Home.

## Workspaces
A workspace remembers every open tab — kind, paths, base/output, view settings, its saved-session link — and which tab was in front. **Workspace** in the tab bar saves the current tabs (new or overwriting an existing workspace). Opening a workspace from Home replaces the open tabs.

## Restore on startup
PCompare always records the open tabs when it closes. Turn on *Restore last workspace on startup* to reopen them next time; comparisons passed on the command line still open, in front.
`,
  },
  {
    id: "text",
    title: "Text Compare",
    kinds: ["text"],
    body: `
Side-by-side line diff with word-level highlights inside changed lines. Both sides are editable.

## Toolbar
- **Previous / Next difference** step through change blocks.
- **Ignore rules**: whitespace (trailing, changes, all), case, diff algorithm (Patience or Myers) and regexes for text that never matters, e.g. version stamps. Matching text is shown as unimportant (blue).
- **Refresh** reloads both files; unsaved edits are kept unless you confirm.
- **Undo / Redo** for edits and copied changes.

## Editing
- Gutter arrows (or the copy shortcuts) copy the current difference to the other side.
- Double-click a line to edit it. Saving keeps each file's encoding, BOM and line endings.
- Files are watched: when one changes on disk the compare refreshes by itself (never over unsaved edits).

## Colours
Changed lines are amber, lines only on the left red, only on the right green. Change them in *Settings → Colors*.
`,
  },
  {
    id: "merge",
    title: "Text Merge",
    kinds: ["merge"],
    body: `
3-way merge: left and right are compared with their common ancestor (base). Changes made on only one side are merged automatically; changes on both sides become conflicts.

## Resolving
- Step through conflicts with **Previous / Next conflict**.
- For each conflict take left, right, both or base, or edit the output pane directly.
- The output is written to the **Output** path; save it when every conflict is resolved.

## As a git mergetool
\`pcompare "$LOCAL" "$REMOTE" "$BASE" "$MERGED"\` opens this view with the output set to the merged file.
`,
  },
  {
    id: "table",
    title: "Table Compare",
    kinds: ["table"],
    body: `
Cell-by-cell comparison of CSV/TSV, Excel/ODS workbooks and HTML tables.

## Lining up rows
- By default rows are paired in order. Choose **key columns** (click a column header, or **Columns**) to match rows by key, or **Sort rows** before aligning.
- Columns are paired by header name; **Columns** lets you pair, rearrange or unmatch them manually.

## Rules
- Ignore columns, case and surrounding whitespace.
- **Numeric tolerance** treats numbers within a margin as equal; **date tolerance** does the same for dates and times.
- Workbooks: pick a worksheet, or compare every sheet paired by name.
`,
  },
  {
    id: "picture",
    title: "Picture Compare",
    kinds: ["picture"],
    body: `
Compares images pixel by pixel: PNG, JPEG, GIF, BMP, ICO, TIFF, WebP, TGA, PNM and QOI.

## Views and tools
- Side-by-side with differing pixels highlighted, plus difference, blend (opacity slider) and swipe views.
- **Tolerance**: the largest per-channel difference (0–255) still treated as equal.
- Zoom and pan are synced between the sides. **Zoom to differences** jumps to the area that changed; **Fit** and **Actual size** reset the view.
- Statistics are computed at full resolution even when zoomed out.
`,
  },
  {
    id: "folder",
    title: "Folder Compare",
    kinds: ["folder"],
    body: `
Compares two folder trees. Red names differ (the older file is grey), purple items exist on one side only, blue (≈) items differ only in unimportant ways.

## Toolbar
- **Expand / Collapse** all folders; **Flatten** lists every file at one level.
- **Swap** exchanges the sides; **Sync** opens Folder Sync for these folders.
- Display filters show all, differences, same or orphans; name filters include or exclude files and folders.
- **Refresh** rescans. Folders are watched and rescan by themselves when something changes.

## Comparing contents
The session settings choose quick tests (size, timestamp with tolerance) and an optional content compare: binary, CRC-32 or rules-based (text rules, so cosmetic changes count as similar). Right-click a file or folder to compare contents on demand.

## Opening files
Double-click a file pair to open it in the matching viewer (text, table, picture, hex…). Either side may be a \`remote://profile/path\`.

## Selecting
Click the left or right half of a row to select that side's item; click the middle status column to select both sides. **Ctrl/⌘-click** adds or removes rows, **Shift-click** selects a range, **Ctrl+A** selects everything shown, and **↑ / ↓** (with Shift to extend) move through the list. Enter opens the selection. Compare contents and Ignore in the context menu work on every selected row.

## File operations
Right-click, the toolbar or the shortcuts below act on the selection:
- **Copy / Move to Right or Left** put items in the matching folder on the other side (aligned names included); **Copy / Move to Folder…** sends them anywhere, keeping their relative paths if you like.
- **Delete** moves items to the Recycle Bin / Trash; **Delete Permanently** (Shift+Delete) skips it. Deletions and overwrites on remote folders are always permanent.
- **Rename** (F2), **New Folder**, **Change Attributes** (read-only, hidden, system, archive on Windows; permission bits elsewhere) and **Touch** (set the modified time to now, a chosen time or the other side's time, optionally for everything inside folders).
- Each operation is confirmed first, with what happens to files that already exist: ask, overwrite, overwrite older, or skip. On Windows, copies can carry NTFS permissions (ACLs) and short 8.3 names along.

## Operations panel
Operations run in the background while you keep comparing. The **Ops** button shows the panel: progress, the current file, **Pause / Resume** and **Cancel** per operation, questions about existing files, and errors when it is done. Operations on overlapping folders wait for each other; the comparison rescans when one finishes.
`, // bc5:file-ops — Selecting, File operations, Operations panel
  },
  {
    id: "sync",
    title: "Folder Sync",
    kinds: ["sync"],
    body: `
Copies, mirrors or two-way syncs folders with a preview of every operation before anything changes.

## Modes
- **Update** or **Mirror** left→right or right→left, **Update both**, or **Two-way with history** (deletions propagate; edits on both sides are conflicts settled by the conflict policy).

## Preview and run
- Every copy, delete and new folder is listed with a checkbox; include or exclude everything shown at once.
- **Synchronize** runs pre-flight checks (free space, write access), then executes with retries and locked-file handling. **Pause** holds a running sync between files (and inside large copies); **Resume** continues where it stopped.
- **Sync options** set safety rules such as versioning replaced files into a folder, and on Windows whether NTFS permissions (ACLs) and short 8.3 names are copied.
- **History** lists past syncs; a finished sync can be **rolled back**, and an interrupted one resumed or repaired.
- **Rebuild preview** re-scans both folders.
`,
  },
  {
    id: "folderMerge",
    title: "Folder Merge",
    kinds: ["folderMerge"],
    body: `
3-way merge of folder trees against a common ancestor. Every path is classified (changed, added or deleted on either side, or a conflict) and gets a proposed action you can change.

## Resolving
- Step through conflicts with **Previous / Next conflict**.
- Text files changed on both sides can be merged line by line automatically, or opened in Text Merge.
- Optionally propose the newer file where the sides differ.
- **Merge folders** writes the result into the output folder, or into the left or right folder.
`,
  },
  {
    id: "hex",
    title: "Hex Compare",
    kinds: ["hex"],
    body: `
Byte-level comparison of binary files of any size, streamed in pages. Differing bytes are highlighted in both the hex and the text columns. Use **Next difference** to jump to the next differing byte and **Reload** to read both files again.
`,
  },
  {
    id: "registry",
    title: "Registry Compare",
    kinds: ["registry"],
    body: `
Compares \`.reg\` exports (REGEDIT4 and version 5, including UTF-16 files) or, on Windows, live keys such as \`HKLM\\Software\\Vendor\`.

- The key tree shows each key's status; select a key to see its values side by side.
- Arrow keys move through the tree (Left/Right collapse and expand).
- **Expand / Collapse all**, **Swap** sides and **Refresh** are on the toolbar.
`,
  },
  {
    id: "version",
    title: "Version Compare",
    kinds: ["version"],
    body: `
Compares the version resources of \`.exe\` and \`.dll\` files (fixed file info and every string table) plus PE header facts: architecture, link timestamp, subsystem, ASLR/DEP/CFG flags and whether the file is signed. Filter to show all properties or only differences.
`,
  },
  {
    id: "mp3",
    title: "MP3 Compare",
    kinds: ["mp3"],
    body: `
Compares tags (ID3v1/v2, APE, Vorbis comments, MP4) and audio properties (duration, bitrate, sample rate, channels) of MP3, FLAC, M4A, OGG/Opus, WAV and AIFF files, with embedded cover art side by side. Filter to show all properties or only differences.
`,
  },
  {
    id: "database",
    title: "Database Compare",
    kinds: ["database"],
    body: `
Schema and data compare for SQL Server, MySQL/MariaDB, PostgreSQL and SQLite.

## Connections
Pick a saved connection (or create one) and a database for each side. Passwords are kept in the OS keychain.

## Schema compare
Tables (columns, keys, indexes, foreign keys, checks), views, procedures, functions, triggers and sequences, with side-by-side DDL. Tick objects to include them in the **sync script**.

## Data compare
Rows are matched by primary key (or a unique index, or key columns you choose) with cell-level differences. Options cover case, whitespace, numeric tolerance and date-time precision. Generate a data sync script for the ticked tables, review it, then run it.
`,
  },
  {
    id: "profiles",
    title: "Connection profiles",
    kinds: ["profiles"],
    body: `
Profiles connect remote storage: Amazon S3 and compatibles, Azure Blob, Google Cloud Storage, Backblaze B2, FTP/FTPS, SFTP, WebDAV, OneDrive, Dropbox and Google Drive. Secrets are stored in the OS keychain.

Use \`remote://<profile>/path\` (or the globe button next to a path) on either side of any comparison. **Test** checks a profile before saving; OAuth providers need an app registration you control — follow the link in the form.
`,
  },
  // bc5:editor-patch
  {
    id: "edit",
    title: "Text Edit",
    kinds: ["edit"],
    body: `
Edits a single text file. Open it from the **Text Edit** card on Home, **Edit file** in a Folder Compare's right-click menu, or **Open in Text Edit** when a Text Compare has only one file. Leave the path empty to start a new file and use **Save As** to name it.

## Editing
- Click anywhere to type. Enter keeps the line's indentation; Backspace/Delete at the ends of a line join lines.
- Drag over the line numbers (or Shift+click, Shift+↑/↓) to select whole lines, then cut, copy, paste over, delete, indent (Tab / Shift+Tab), duplicate or move them.
- Ctrl+C with nothing selected copies the whole line; Ctrl+A selects every line.
- Undo and Redo cover every edit, including Replace all.

## Files
- Saving keeps the file's encoding, byte-order mark and line endings. Change either from the toolbar: an unedited file is re-read in the chosen encoding, otherwise the text is converted when saved.
- The first save of a file backs up the previous version (\`.bak\` or a backup folder, per the Text Compare rules dialog).
- Remote files open read-only; Save As writes a local copy.

## View
Syntax colours, line numbers and word wrap are toggles; bookmarks mark lines to jump between. Large files (100k+ lines) stay responsive because only the visible lines are drawn.
`,
  },
  {
    id: "patch",
    title: "Patch Viewer",
    kinds: ["patch"],
    body: `
Shows a \`.patch\` or \`.diff\` file as a side-by-side comparison. Opening (or dropping) a single \`.patch\`/\`.diff\` file opens it here; two of them still open in Text Compare.

## Reading a patch
- Understands unified diffs (\`diff -u\`, \`diff -ruN\`), git diffs (renames, copies, new and deleted files, mode changes, binary files) and context diffs (\`diff -c\`).
- The file list shows each file's change (**M**odified, **A**dded, **D**eleted, **R**enamed, **C**opied, **B**inary) and its added/removed line counts. A commit message or mail headers appear under **Description**.
- Each hunk shows the old lines on the left and the new lines on the right, numbered from the hunk header, with word-level highlights inside changed lines.

## Applying against a folder
1. **Apply against folder…** picks the folder the patch was made for. Every file is dry-run against it and marked as applying cleanly or not; the strip level (\`-p\`) is detected, or choose it.
2. Hunks are found where their header says, or nearby when the file moved on (offset), ignoring up to two outer context lines if needed (fuzz) — like \`patch\`.
3. **Open in Text Compare** (or double-click a file) shows the real file on the left and the patched text on the right, in memory. Hunks that failed to apply are listed and left out; copy or edit changes across and save the left side to apply them for real.
`,
  },
  {
    id: "appearance",
    title: "Fonts, colours and shortcuts",
    kinds: [],
    globalKeys: true,
    body: `
Open **Settings** in the tab bar.

- **Appearance**: Dark, Light or System theme (System follows the OS setting live) and the diff colours — changed, inserted, deleted, unimportant text and the intra-line highlights. Each colour can be reset to the theme's default.
- **Fonts**: the interface font and size, and the editor (monospace) font and size used by the text, merge, hex and table views.
- **Keyboard**: every command with its shortcut. Click a shortcut and press the new keys; conflicts with another command in the same view are flagged. Shortcuts marked *built-in* can't be changed yet.
`,
  },
  // bc5:archives
  {
    id: "archives",
    title: "Archives, snapshots and Subversion",
    kinds: [],
    body: `
## Archives as folders
Zip (also jar, war, ear, apk), tar, tar.gz/tgz, tar.bz2, tar.xz, single .gz/.bz2/.xz files and 7z archives open as folders: choose one as a Folder Compare side (the archive button next to a folder path picks one), drop two archives on the window, or type a path that goes inside one — \`C:\\x\\a.zip\\dir\\file.txt\` or \`/x/a.tar.gz/dir\`. Office and OpenDocument files (docx, xlsx, odt…) open as zip archives only when named explicitly as a folder side.
- Contents are read in place, never extracted to a temp folder; compressed tars are decompressed once into the cache.
- Comparing two zip (or 7z) archives by **CRC** uses the checksums stored in the archives, without reading the files.
- Double-clicking a file inside an archive opens it in the usual viewer. Archives inside archives open the same way (read-only).
- **Compare as files** on the banner (or **Open in Hex** in the context menu) compares two archive files byte by byte.

## Expand archives inside folders
By default an archive inside a compared folder is compared as a file. *Rules → Alignment → Expand archives as folders* lists archives in both trees as folders instead, so their contents are compared too.

## Writing into archives
Zip archives can be written to: Folder Sync and file operations can copy into, replace in and delete from a zip. Each change rewrites the archive into a temp file beside it (unchanged entries are copied without recompressing) and swaps it in, so the archive is never left half-written. Tar, 7z and gzip archives are read-only, as are archives inside other archives and archives on remote profiles.

## Snapshots
**Snapshot** in Folder Compare (or **Save folder snapshot…** on Home) records a folder's names, sizes, dates and attributes — and with *Include CRCs*, each file's CRC-32 — to a \`.pcsnap\` file, without the contents. Open the snapshot as a Folder Compare side later to see what changed. Content comparisons against a snapshot use the stored CRCs; without them, files are compared by size and date only. Command line: \`pcompare --snapshot <folder> <out.pcsnap> [--crc]\`.

## Subversion repositories
Write \`svn:\` before a repository URL to compare it as a read-only folder: \`svn:https://host/repo/trunk\`, \`svn:svn+ssh://host/repo/branches/x@1234\` (a fixed revision) or \`svn:file:///srv/repo/trunk\`. PCompare runs the \`svn\` command-line client (\`svn list\`, \`svn cat\`), which must be installed and on the PATH; its saved credentials are used.
`,
  },
];

/** The help page F1 should open for a tab of `kind`. */
export function topicFor(kind: SessionKind): HelpTopic {
  return HELP_TOPICS.find((t) => t.kinds.includes(kind)) ?? HELP_TOPICS[0];
}
