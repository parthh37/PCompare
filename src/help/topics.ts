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
Type or browse to two files or folders in **Quick Compare** at the top of Home and press **Compare** (or Enter): PCompare picks the viewer from what they are, as it does for two items dropped anywhere on the window. A path that doesn't exist is reported under the fields before anything opens. To start a particular kind of session, pick it under **New session** on the right. Dropping one item fills the empty side of the compare in front, or opens it on its own.

## Opening comparisons
- **Command line**: \`pcompare <left> <right>\` picks the viewer from the paths; \`pcompare <left> <right> <base> [<output>]\` opens a 3-way merge (the usual mergetool order, usable as a git mergetool).
- Launching PCompare again while it runs opens the new comparison as a tab in the existing window.
- **Select Left Side** in a file list (or Explorer) remembers one path; **Compare to…** on another item compares the two.

## Home lists
Your own work is on the left; once there are more than six entries, a filter box narrows all three lists.
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
With *Settings → General → Auto-save sessions on close* (on by default):
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
The toolbar:
- **All / Diffs / Same / Context** filter the lines: everything, only differences, only matching lines, or differences with a few lines around them.
- **Minor** (on by default) ignores unimportant differences: they count as same, so Diffs hides them and section navigation skips them. Turn it off to treat them as differences.
- **Rules**: whitespace (trailing, changes, all), case, diff algorithm (Patience or Myers) and regexes for text that never matters, e.g. version stamps. Matching text is shown as unimportant (blue).
- **Prev / Next Section** step through difference sections; **Copy Left / Copy Right** copy the selected lines, or else the current section, to the other side.
- **Swap** exchanges the left and right files; **Reload** reloads both (unsaved edits are kept unless you confirm).
- **Undo / Redo** for edits and copied changes.

## Thumbnail and status bar
- The strip on the left is a miniature of both files, coloured by difference. The box marks what is on screen; click or drag to scroll.
- The status bar counts difference sections and names the caret line's state (Same, Important / Unimportant difference, Left / Right orphan). Line Details under the panes is open by default.

## Editing
- Gutter arrows (or the copy shortcuts) copy the current difference to the other side.
- Double-click a line to edit it. Saving keeps each file's encoding, BOM and line endings.
- Files are watched: when one changes on disk the compare refreshes by itself (never over unsaved edits).

## Colours
Changed lines are amber, lines only on the left red, only on the right green. Change them in *Settings → Colors*.
` +
      `
## Numbers and ignored blocks
In *Rules → Importance*, **Compare numbers by value** makes 1.0 match 1.00 and 1e3 match 1000; with an absolute tolerance (e.g. 0.01) or a relative one (percent of the larger value), lines that differ only in nearly equal numbers are unimportant and those numbers are not highlighted. Signs count, so -0.5 and 0.5 differ. *Rules → Ignored blocks* takes begin/end regex pairs: every line from a begin match through the next end match (both included) is unimportant, e.g. an auto-generated region, even when only one side has it. A block that never ends runs to the end of the file.
` +
      `
## Linked lines and sync points
- **Vertical padding** (toolbar, rows icon) is on by default: blank rows keep both sides aligned. Turn it off to see only each file's real lines; the panes then scroll together so matching blocks meet, and curved bands in the middle link each difference's left lines to its right lines (click a band to select it). The setting is remembered.
- **Sync points** force two lines to sit side by side: Ctrl+click (⌘+click on macOS) or right-click → *Set Sync Point Here* on a line on one side, then on its partner on the other side. The files are compared again around them. Sync points show as purple dots and dashed links; right-click → *Remove Sync Point*, or clear them all from the **…** menu. They also appear under *Rules → Alignment*.

## Editing extras
- **Go to line** (Ctrl+G) jumps to a line of either side; the status bar shows the caret's side, line and column.
- **Tab / Shift+Tab** indent or un-indent the selected lines (Shift+Tab also un-indents the line being edited).
- **Drag selected lines** by their line numbers: drop them on the other side to copy them there, or elsewhere on the same side to move them (hold Ctrl or Alt to copy). Undo reverts it.
- **Copy All to Right / Left** (right-click or **…**) replaces the whole other side with this one, as an undoable edit.
- Lines changed by editing or copying get an orange bar by their number (and in the overview strip) until the file is saved or reloaded.
- **Mark Resolved** dims the selected lines (or the current difference) once you have dealt with them; it changes only the view, unlike *Mark unimportant*, and is saved with the session. *Unmark Resolved* undoes it.
- **Show line endings** (pilcrow icon) shows ↵ for LF, ␍␊ for CRLF and ␍ for CR at the end of each line.
- **Moved blocks** are tinted violet. A block of lines that left one place and reappears unchanged elsewhere still counts as a difference, but its first line carries a *moved to N* / *moved from N* link that jumps to the other end. Turn the tint off from the **…** menu.
- **Blame** (commit icon) shows who last changed each line of a file in a Git working tree, or of a \`git:\` side at its revision: commit, author and age, with a brighter bar for newer commits. Unsaved edits show as *Not committed*. Click a commit to open its change to the file as a new comparison. Mercurial isn't supported yet.
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
` +
      `
## Automatic merge and undo
- **Start with** chooses what the output holds after loading: **Auto-merged** (every one-sided change applied, conflicts between markers) or **Base only** (the ancestor text, nothing merged yet). Changing it rebuilds the output as one undoable step.
- **Automatic merge** (Ctrl+Shift+M) applies every change not merged yet in one step: one-sided changes are taken and dimmed as resolved, conflicts are wrapped in markers. Changes you already took or edited are left alone.
- **Undo** / **Redo** (Ctrl+Z / Ctrl+Y when the output pane doesn't have the focus) step back through automatic merges, takes, marks and bursts of typing. Right after loading, the first Undo is **Undo automatic merge**.

## Marking resolved
- Select a change (or pick lines in an input pane) and click **Resolved** or **Unresolved** (Ctrl+Shift+R toggles). Resolved changes are dimmed and no longer counted as conflicts, whatever the output holds.

## Highlights and edit markers
- Inside changed lines, the characters that differ from the base are highlighted in the left, base and right panes.
- The output's left margin shows where each line came from: **Auto**-merged (green), taken from **Left** (blue), **Right** (purple) or **Base** (grey), or **Edited** by hand (orange). The legend is in the output header.
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
Compares two folder trees. Red names differ (the older file is grey), purple items exist on one side only, blue (≈) items differ only in unimportant ways. A clock means the contents are the same but the size or timestamp differs. Folders take the colours of everything inside them: a folder holding both newer files and orphans shows red and purple, and its name is bold on the side its newer files are on. **View → Legend** explains every colour and icon.

## Toolbar
- **All · Diffs · Same** pick what shows. The ▾ on **Diffs** offers the rest: differences without orphans, no orphans, orphans (either or one side), left or right newer, and newer plus that side's orphans.
- **Structure ▾** sets when folders show: **Always show folders**, **Compare files and folder structure** (folders follow the display filter like files; the default) or **Compare files only** (just folders holding a shown file). **Ignore folder structure** lists every file at one level.
- **Minor** (on by default) counts unimportant differences as same: files that differ only in ways the text rules call unimportant (≈). Off, Diffs shows them and Same doesn't.
- **Rules** opens the session settings. **Copy** copies the selection to the other side (it asks which way when both sides are selected).
- **Expand / Collapse** open or close every folder. **Select…** selects by criteria and **Files** selects every file shown. **Refresh** (F5) lists and compares both folders again.
- Second row: **Swap** exchanges the sides, **Stop** (Esc) stops a running compare, and the **Filters** box takes files to include and \`-\`names to exclude, separated by \`;\` (e.g. \`*.rs;*.ts;-target;-*.tmp\`). The **Filters** button opens the size, date, attribute and content filters, and **Peek** shows what the filters leave out, in teal.
- The small icons on the right: Folder Sync, report, split view, comments, statistics, comparison archive, snapshot, delete, operations, and **View** (attributes column, log, legend).
- Folders are watched with **Auto-refresh** and compared again when something changes.

## Log and status
The log under the folders says when a comparison was loaded, how long it took, and what was cancelled, refreshed or failed (**View → Log** shows or hides it). Under each side, the footer shows what is selected there and how much space is free on that disk.

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
` + // Selecting, File operations, Operations panel
      `
## Split view
**Split** (Ctrl+Shift+V) shows the selected file pair's comparison below the folder list, in the same viewer a double-click opens (text, hex, table or picture). Drag the divider to resize; the split and its size are remembered. A file on one side only shows a read-only preview; folders show nothing. Click inside the lower pane to give it the keyboard (F5, Alt+↓, Ctrl+S…); click the folder list to take it back. Saving edits there rescans the folders.

## Select by criteria
**Select…** (Ctrl+Shift+E) selects the rows matching a status (different with the left or right newer, similar, same, orphans on either side, errors), name wildcards to include or exclude (e.g. \`*.cs;*.xml\`), files and/or folders, and size or date ranges. **Select** replaces the selection, **Add to selection** extends it; matches inside collapsed folders can be expanded into view. The selection then drives copy, move, delete and the other file operations.

## Comparison type and large files
In *Rules → Comparison*, **Quick** compares sizes and timestamps, **Thorough** reads the files and compares text with the text rules, and **Byte-by-byte** compares every byte. *For files larger than N MB use* keeps a thorough (or CRC) comparison fast: bigger pairs are compared byte-by-byte, by size only, or by size and timestamp instead.

## Dropping files
Drop a folder (or file) onto the left or right path field to fill just that side. Dropping anywhere else fills the empty side or opens a new tab.

## Recent paths
Click a path field (or press ↓ in it) to list the folders or files you compared recently; type to filter, then pick one with the mouse or ↑/↓ and Enter. The **×** on a row forgets that path.
`,
  },
  {
    id: "sync",
    title: "Folder Sync",
    kinds: ["sync"],
    body: `
Copies, mirrors or two-way syncs folders with a preview of every operation before anything changes.

## Modes
- **Update** or **Mirror** left→right or right→left, **Update both**, or **Two-way with history** (deletions propagate; edits on both sides are conflicts settled by the conflict policy).
- **Custom…** picks the action for each kind of item yourself (**Matrix…**): newer on left/right, only on left/right, deleted on left/right (known from the last sync), different with neither newer, and same. Each can be left alone, copied either way, deleted on either side, or set to **Prompt**, which lists the item as a conflict for you to decide in the preview. Every fixed mode is one row of presets in this table; *Start from* loads one.
- **Empty folders** off skips creating folders that hold no files (or only filtered-out ones).
- In Folder Compare, right-click → **Synchronize selection…** opens Folder Sync for just the selected items.

## Preview and run
- Every copy, delete and new folder is listed with a checkbox; include or exclude everything shown at once.
- Items left alone and conflicts are listed too. Select rows (Ctrl/⌘+click, Shift+click) and set their action from the action buttons or the right-click menu: **Leave alone**, **Copy left → right**, **Copy right → left**, **Delete left** or **Delete right** — conflicts included. On a folder, delete removes it whole and the other actions apply to everything inside that differs. Only those rows are re-planned (no rescan); **✎** marks them, and **Reset to plan** undoes it. **Preview** rescans and starts from the mode again.
- **Synchronize** runs pre-flight checks (free space, write access), then executes with retries and locked-file handling. **Pause** holds a running sync between files (and inside large copies); **Resume** continues where it stopped.
- **Sync options** set safety rules such as versioning replaced files into a folder, whether new folders get their source folder's dates (on by default) and creation dates are copied (Windows and macOS), and on Windows whether NTFS permissions (ACLs) and short 8.3 names are copied.
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
Tables (columns, keys, indexes, foreign keys, checks), views, procedures, functions, triggers and sequences, with side-by-side DDL.

## Data compare
Rows are matched by primary key (or a unique index, or key columns you choose) with cell-level differences. Options cover case, whitespace, numeric tolerance and date-time precision.

## Copying changes
Every difference has two arrows: **→** copies it into the right database, **←** into the left one (click an active arrow again to undo). Arrows work on single objects, a whole group (all tables, all views…), a whole table's data, or single rows in the middle of the row grid. **Alt+→** / **Alt+←** pick the selected object or table.

Nothing is changed while you pick. **Review changes** lists what will happen in plain words, per database: what is added, updated and deleted. **Apply** then builds and runs the SQL in the background, in all-or-nothing batches, and compares again. **Show SQL** in the review shows (and lets you copy or save) the exact statements first.

Copying a row that refers to another row (a foreign key) needs that row on the other side too: pick both, or copy the whole table.
`,
  },
  {
    id: "profiles",
    title: "Connection profiles",
    kinds: ["profiles"],
    body: `
Profiles connect remote storage: Amazon S3 and compatibles, Azure Blob, Google Cloud Storage, Backblaze B2, FTP/FTPS, SFTP, WebDAV, OneDrive, SharePoint, Dropbox, Google Drive and GitHub. Secrets are stored in the OS keychain, and the profile list itself is encrypted with a key kept there too, so a copied settings folder can't be read on another computer. Right-click a profile to delete it.

Use \`remote://<profile>/path\` (or the globe button next to a path) on either side of any comparison. **Test** checks a profile before saving.

**OneDrive, SharePoint, Dropbox, Google Drive**: click **Sign in** and approve PCompare in your browser. To go through an app registration of your own instead (some organisations require it), open **Advanced** and enter its Client ID. Builds without a built-in app always need one — follow the link in the form. While the browser is open, **Cancel** stops waiting.

**Dropbox access token**: instead of signing in, paste a token generated on your app's page in the Dropbox App Console into **Access token**. It is used while set (signing in replaces it); generated tokens expire after about 4 hours. Tick **files.metadata.read**, **files.content.read** and **files.content.write** on the app's Permissions tab (and click Submit) *before* generating it: a token keeps the permissions it was created with, so a \`missing_scope\` error means generating a new one.

**Dropbox team accounts**: team folders are found automatically. For accounts with a team space, paths start at the team's top level, where your own folder appears as a folder too.

**Root folder**: click **Browse…** next to it to pick from the folders on the server. It works before saving, once sign-in details are filled in.

**SharePoint**: paste the site's address from the browser (e.g. \`https://contoso.sharepoint.com/sites/Marketing\`); a link into a library also picks that library. Leave **Library** empty for the site's default *Documents* library. Deletes go to the site's recycle bin. SharePoint adds metadata to Office files it stores, so a .docx synced there can differ in size from the original even though nothing was edited.

**GitHub** (read-only): sign in with a personal access token, with the GitHub CLI's login (run \`gh auth login\` once), or not at all for public repositories (60 requests an hour). Paths are \`remote://<profile>/<owner>/<repository>/<branch or tag>/path\`. The remote browser walks owners → repositories → branches and tags, so comparing two branches is picking one on each side; a commit id works in place of a branch name. A branch name with a slash is written with \`%2F\` (\`feature%2Flogin\`). Set **Owner** and **Repository** in the profile to start paths at that repository's branches (\`remote://<profile>/main/src\`). Every file shows the time of its branch's latest commit. Between two GitHub sides, files compare by their Git ids — exact and without downloading anything, whatever the timestamps say; against a local folder, turn on a content comparison, since the commit time won't match the files' own times. Files stored with Git LFS compare as their pointer files.
`,
  },
  // automation:ui
  {
    id: "automation",
    title: "Sync Automation",
    kinds: ["automation"],
    body: `
Saved folder syncs that run by themselves: on a schedule, when files change, or when PCompare starts — with safety checks, retries, a run history and alerts when something goes wrong. Open it from **Automation** in the tab bar, **New session** on Home, or the tray icon's menu. In a Folder Sync tab, **Automate…** turns the folders and settings you're using into a new task.

## Tasks
- Each task is a folder pair, a sync mode and its options — the same engine as Folder Sync, without the preview step. **Mirror** and **Two-way** delete files; the editor says so before you save.
- **Run now** always works, even for a disabled task or while automation is paused. A task never runs twice at once: triggers that arrive during a run collapse into one follow-up run.
- The switch at the start of a row enables or disables a task. **Pause all** stops every automatic run until you resume.

## Triggers
- **Interval**: every N minutes, counted from the end of the previous run.
- **Daily / weekly**: fixed times on chosen weekdays (local time).
- **Cron**: five fields (\`minute hour day month weekday\`) or six with seconds first, e.g. \`0 */2 * * *\` every two hours, \`30 8 * * 1-5\` weekdays at 08:30; \`@hourly\`, \`@daily\`, \`@weekly\`, \`@monthly\` too.
- **Real-time**: watches the source folder(s) — left for left→right modes, right for right→left, both for two-way and update-both, and for Custom the side(s) its matrix copies from — and syncs once changes have been quiet for a few seconds (or after the maximum delay under constant change). Remote folders and some network shares can't be watched; a periodic **rescan** covers them, and catches anything the system didn't report. The eye icon shows watching is active; a crossed-out eye explains why it isn't.
- **At startup**: once, a little after PCompare starts.
- **Catch up missed runs**: if PCompare was closed or the computer slept through a scheduled time, run once as soon as possible.
The editor's **Next runs** list shows exactly when the schedule fires.

## Safety
- **Abort when one folder is empty** — almost always an unplugged drive or an unmounted share. **Abort on mass deletion** and the deletion limits (a count, or a percentage of the files on that side) stop a run before it deletes. Aborted runs change nothing and are reported as failures.
- **Timeout** cancels a run that takes too long.
- **Retry the whole run** for run-wide failures such as an unreachable NAS: the wait doubles each time (up to an hour) and only the last attempt notifies. Files in use are retried per file, as in Folder Sync (Options tab).
- Deleted and replaced files go to the Recycle Bin / Trash, a versioning folder (exact rollback), or are deleted permanently — set on the Options tab.

## Notifications
Add channels with **Notification channels**, then tick them on a task's Notifications tab. By default you hear about failures, runs with errors or conflicts, and the first success after a failure; the same failure isn't repeated more than every 6 hours. **Send test** checks a channel. Webhook URLs and passwords are kept in the system keychain and are never exported.
- **Microsoft Teams**: in the channel open **⋯ › Workflows**, choose **Post to a channel when a webhook request is received**, pick the team and channel, and paste the URL it shows. Legacy Incoming Webhook URLs also work.
- **Slack**: an app with **Incoming Webhooks**; paste the webhook URL.
- **Email (SMTP)**: Microsoft 365 uses \`smtp.office365.com\`, port 587, STARTTLS (SMTP AUTH must be allowed for the mailbox). Gmail uses \`smtp.gmail.com\`, 587, STARTTLS with an **app password** (Google Account › Security › App passwords), not your normal password. Leave the port blank for the default of the security mode.
- **Webhook**: PCompare POSTs a JSON summary; an optional signing secret adds an HMAC-SHA256 signature header.
- **Desktop**: an OS notification on this computer.

## Running in the background
With *Automation settings → Keep running in the background*, closing the window leaves PCompare in the notification area (tray) so tasks keep running; quit from the tray icon. *Start PCompare when I sign in* starts it in the background at login. Tasks only run while PCompare is running.

## History and rollback
The bottom panel lists runs for all tasks or the selected one: what started it, the attempt, the result, operations, failures, conflicts, bytes copied, duration and which notifications were sent. Click a run for details. **Open in Sync History** (the arrow at the end of a row) shows the run's individual operations and can **roll back** what it changed. A run interrupted by a crash is repaired automatically before that task's next run. History older than the number of days in Automation settings is removed.
`,
  },
  // automation:ui end
  {
    id: "edit",
    title: "Text Edit",
    kinds: ["edit"],
    body: `
Edits a single text file. Open it from **Text Edit** under **New session** on Home, **Edit file** in a Folder Compare's right-click menu, or **Open in Text Edit** when a Text Compare has only one file. Leave the path empty to start a new file and use **Save As** to name it.

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
` +
      `
## Applying only some hunks
- Every hunk header and every file in the list has a checkbox; untick hunks (Space toggles the current one) or whole files to leave them out. Unticked hunks are greyed and also left out of **Open in Text Compare**.
- **Apply selected** (Ctrl+Shift+Enter) writes the ticked hunks into the real files under the folder, in their own encoding and line endings, after asking. New files are created; deleted files and the old names of renamed files go to the Trash (a file is only deleted when all its hunks are ticked and apply).
- A report lists each file and hunk: applied (with offset or fuzz), skipped, already applied, or failed with the reason. The dry run then re-checks the folder.
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
  {
    id: "review",
    title: "Comments, statistics and comparison archives",
    kinds: ["review"],
    body: `
## Bookmarks and comments
- **Text Compare**: bookmark a line (Ctrl+F2 or the bookmark button), or right-click a line and choose **Comment…** to bookmark it and type a comment straight away.
- **Folder Compare**: right-click a row and choose **Bookmark** or **Comment…**; rows are bookmarked by their relative path.
- The **Comments** button (speech bubble) opens the *Bookmarks & comments* panel: every bookmark with its comment, editable in place. Click a bookmark to jump to it, the cross to remove it, **Copy** to put them all on the clipboard as text.
- A commented bookmark shows a speech bubble in the line-number gutter (or after the file name); hover it to read the comment.
- Bookmarks and comments are saved with the session and travel in comparison archives.

## Statistics
The **Statistics** button (bar chart) shows figures for the comparison in front:
- Text: lines per side, identical, changed, inserted, deleted and unimportant lines, difference blocks, characters changed, similarity, encodings, sizes and line endings.
- Folders: files and folders per side, bytes per side (and in differing files), orphans, newer files, and files and folders per status.
**Copy** puts the table on the clipboard as tab-separated text.

## Comparison archives (.pcmpz)
**Archive** in Text or Folder Compare saves the whole comparison in one file a reviewer can open on another machine, without the originals:
- Text: both texts as shown (unsaved edits included), their encodings, sizes and line endings, the text rules, and the bookmarks and comments.
- Folders: every row with its sizes, dates and status (re-checks and ignore marks included), the rules, bookmarks and comments — and optionally the contents of differing text files, up to a size per file and in total. Binary files and remote or archived folders are left out.

Open an archive with **Open comparison archive…** on Home, by dropping the \`.pcmpz\` on the window, or from the command line (\`pcompare review.pcmpz\`). It opens read-only: texts in Text Compare, folders as the stored tree (double-click a file with stored contents to compare it). Reviewers can add or edit comments and **Save comments** writes them back into the archive.
`,
  },
  {
    id: "reports",
    title: "Reports and printing",
    kinds: [],
    body: `
**Report…** in Text, Folder and Table Compare saves, copies, previews or prints the comparison.

## Text comparison formats
- **HTML**: side by side or unified, with the diff colours and changed-word highlights.
- **HTML slideshow**: one self-contained page showing one difference per slide with its context lines. Step with **Prev / Next**, the ← / → keys (Home / End jump to the ends); \`#slide-5\` in the address opens slide 5. Printed, every slide gets its own page.
- **Plain text**, **Unified diff (patch)** (\`--- / +++ / @@\`, for \`patch\` and \`git apply\`) and **Normal diff** — classic \`diff\` output such as \`3c3\`, \`5a6,7\` and \`8d7\` with \`<\` / \`>\` lines.
- **XML**: the options, summary counts and every difference block (\`<difference kind="changed" leftStart=… >\` with its \`<line>\`s) for other tools to read.

## Folder comparison formats
HTML, plain text, CSV or **XML** (one \`<entry>\` per row with status, sizes and dates). When saving, *Link to file comparison reports* writes an HTML report for every differing text file next to an HTML report; for XML, *Embed file comparisons* adds each file's text comparison to its entry instead.

## Bookmark comments
Bookmarks in Text Compare (and their comments) are marked next to their lines in HTML reports, with a **Comments** list at the end; differences-only reports keep bookmarked lines visible. XML reports list them as \`<bookmark>\` elements.

## Printing
- **Preview…** shows the printed report on a sheet of the chosen paper size, with the running header and footer and dashed lines where pages are likely to break, plus an estimated page count. Print from there or go back.
- **Header** and **Footer** templates (kept in your preferences) take the tokens \`{title}\`, \`{left}\`, \`{right}\`, \`{date}\`, \`{page}\` and \`{pages}\`; \`|\` splits a template into left | center | right parts. Leave one empty for none. Page numbers print where the browser engine supports them (Windows; Chrome); elsewhere the parts with page numbers are left out.
- **Print only pages/sections with changes** leaves out unchanged runs (keeping the context lines) — the same as *Differences only*.
- On macOS the report opens in your default browser, which shows the print dialog.
`,
  },
  {
    id: "vcs",
    title: "Version control: git, Mercurial, Perforce, Subversion",
    kinds: [],
    body: `
## Revisions as sides
Write \`git:\` or \`hg:\` before a file or folder of a clone, then \`@\` and a revision, in any path field:
- \`git:C:\\src\\app\\main.c@HEAD~1\` — one file one commit back, against \`C:\\src\\app\\main.c\` for what you changed since.
- \`git:/src/app@v1.2\` against \`git:/src/app@main\` — two whole trees in Folder Compare.
- \`hg:/src/app@tip\`, \`hg:/src/app/docs@42\` — Mercurial revisions (numbers, hashes, \`tip\`, \`.\`, bookmarks).
Without \`@\`, \`HEAD\` (git) or \`.\` (hg) is used. Branches with slashes work (\`git:/src/app@origin/main\`). A path that itself contains \`@\` needs an explicit revision: \`git:/x/img@2x.png@HEAD\`. The **branch button** next to a path field picks a revision from the file's history. Revisions are read-only; PCompare runs the \`git\` / \`hg\` command-line client, which must be on the PATH. \`svn:\` URLs work the same way (see *Archives, snapshots and Subversion*).

## As git's difftool and mergetool
*Settings → Source control → Set up git…* shows the \`git config --global\` commands it would run and runs them when you confirm: \`difftool.pcompare\` and \`mergetool.pcompare\` (with \`trustExitCode\`), optionally as the default \`diff.tool\` / \`merge.tool\`. Then \`git difftool\`, \`git difftool -d\` (whole folders) and \`git mergetool\` open PCompare.

## Waiting for the tab
\`pcompare --wait <left> <right>\` (or \`/wait\`; implied by \`/mergeoutput=\`) doesn't return until you close that comparison's tab — also when PCompare is already open and the comparison goes to a tab there. The exit code is 0, or 1 for a merge whose output was never saved, so a tool can tell an abandoned merge. Quitting PCompare counts as closing the tab.

## Other tools
The same Settings page has copy-paste snippets for Mercurial (\`[extdiff]\`, \`[merge-tools]\`), Perforce (\`P4DIFF\`, \`P4MERGE\` and P4V) and Subversion (\`--diff-cmd\`, a \`merge-tool-cmd\` script). \`--base-first\` takes merge paths in \`base theirs yours merged\` order (P4MERGE, svn), and \`-L <label>\` labels the sides as \`svn diff\` passes them.
`,
  },
];

/** The help page F1 should open for a tab of `kind`. */
export function topicFor(kind: SessionKind): HelpTopic {
  return HELP_TOPICS.find((t) => t.kinds.includes(kind)) ?? HELP_TOPICS[0];
}
