# PCompare

A fast, modern file & folder comparison tool for Windows and macOS — inspired by Beyond Compare, built with **Rust + Tauri 2 + React**.

![status](https://img.shields.io/badge/status-early%20development-orange) ![license](https://img.shields.io/badge/license-MIT-blue)

## Features (v0.1)

| Session | What it does |
|---|---|
| **Folder Compare** | Recursive, parallel (rayon) · **quick tests** (size, timestamp with tolerance / DST) then optional **content compare**: *Binary*, *CRC-32* or *Rules-based* (text rules — encoding, line endings, whitespace, case, ignore-regexes — so cosmetic changes show as ≈ *similar*) · override-quick-tests switch · right-click → compare contents of a file or whole folder on demand · **filters**: include/exclude files, exclude folders, size range, modified-date range, hidden files · **display filters**: All / Differences / Same / Orphans / no-orphans / left- or right-only / left- or right-newer · quick name search (`*.rs`) · flatten (ignore folder structure) · swap sides |
| **Text Compare** | **Editable**: gutter buttons and Alt+→ / Alt+← copy a difference across, double-click a line to edit, undo/redo, Ctrl+S saves with the original encoding, BOM and line endings · side-by-side aligned diff · word-level intra-line highlights · Patience or Myers algorithm · ignore whitespace (trailing / changes / all), case and regex-defined text (e.g. `\$Id:[^$]*\$`), with *unimportant* differences shown separately · All / Context / Diffs-only views · overview map · prev/next difference (Alt+↑/↓) · encoding (UTF-8/16 BOM, Windows-1252) and line-ending detection |
| **Text Merge** | 3-way merge of left and right against their common ancestor · non-conflicting changes merged automatically · editable output with marked conflicts · take left / right / both / base per conflict · prev/next conflict (Alt+↑/↓) · works as a `git mergetool` |
| **Table Compare** | Cell-by-cell CSV/TSV (delimiter sniffing), Excel/ODS workbooks (sheet picker) and HTML tables · align rows on **key columns** or by order · ignore columns · columns paired by header name · ignore case, trim, numeric tolerance |
| **Picture Compare** | PNG, JPEG, GIF, BMP, ICO, TIFF, WebP, TGA, PNM, QOI · side-by-side with highlighted differences, difference, blend and swipe views · per-channel tolerance · synced zoom and pan · jump to the differing region |
| **Folder Sync** | Update or mirror left→right / right→left, update both, or **two-way with history** (deletions propagate; edits on both sides are conflicts: skip, newer wins, left or right wins) · preview of every copy, delete and new folder with per-item checkboxes · **fail-safe execution**: pre-flight checks (free space, write access, nested folders), copies flushed to disk and swapped in atomically, optional read-back checksum verification, files changed since the preview are never overwritten or deleted, sources modified mid-copy are retried · **locked files** retried with backoff, then deferred to the end of the run, skipped or stop the sync — on Windows the report names the program holding the file · warnings before a mirror from an empty (unmounted?) folder · deleted and replaced files go to the Recycle Bin/Trash, a versioning folder or are deleted permanently · every run is **journaled**: resume or roll back after a crash, retry failed items, roll back a finished sync, history per folder pair |
| **Scan database** | Embedded SQLite in the app data folder: CRC-32 cache so content compares skip unchanged files on rescans, two-way sync baselines, and the sync job journal · self-heals by setting a corrupt file aside |
| **Folder Merge** | 3-way merge of folder trees against a common ancestor · classifies every path (changed, added, deleted, conflicts) with a proposed action · resolve text conflicts in Text Merge · writes the result to an output folder or into one side |
| **Hex Compare** | Streams files of any size in 4 KB pages · byte-level highlighting · next-difference search |
| **Registry Compare** | `.reg` exports (REGEDIT4 and v5, UTF-16) or live keys such as `HKLM\Software\Vendor` on Windows · key tree with status plus value-by-value differences |
| **Version Compare** | `.exe`/`.dll` version resources (fixed info and all string tables) plus PE header facts: architecture, timestamp, subsystem, ASLR/DEP/CFG, signature presence |
| **MP3 Compare** | Tags (ID3v1/v2, APE, Vorbis, MP4) of MP3, FLAC, M4A, OGG/Opus, WAV and AIFF · audio properties · embedded cover art |
| **Database Compare** | SQL Delta–style **Schema Compare** and **Data Compare** for **SQL Server**, **MySQL / MariaDB**, **PostgreSQL** and **SQLite** · tables (columns, keys, indexes, foreign keys, checks), views, procedures, functions, triggers and sequences, with side-by-side DDL · rows matched by primary key (or a unique index / chosen key) with cell-level differences · **sync scripts** in the target's dialect (dependency-ordered, transactional, SQLite table rebuilds) to copy, save or run on either side · saved connections with passwords in the OS keychain · SQL Server Windows authentication and named instances |
| **Saved sessions** | Name and save any comparison (Ctrl+Shift+S) — paths plus view settings · reopen, rename, delete from Home |
| **Connection profiles** | Amazon S3 & S3-compatible (R2, MinIO, Wasabi…), Azure Blob, Google Cloud Storage, Backblaze B2, FTP/FTPS, SFTP, WebDAV, OneDrive, Dropbox, Google Drive · secrets kept in the OS keychain · use `remote://<profile>/path` (or the globe button) on either side of any compare |
| **Auto-refresh** | Folder, text and hex sessions re-compare by themselves when either side changes on disk (OS file watcher, debounced, ignores excluded folders); remote sides are polled every 30 s |
| **Updates** | Signed over-the-air updates from GitHub Releases — checked at startup and every 6 h, or on demand (Updates); install & restart in one click |
| **Shell** | Tabbed sessions · drag two files/folders onto the window · `pcompare <left> <right>` command line · `pcompare <left> <right> <base> [<output>]` opens a 3-way merge |

## Roadmap

### Phase 2 — Beyond Compare parity
- [x] Editable text panes, copy hunk left ⇄ right, save with original encoding/line endings
- [x] Folder **sync** actions: copy, move, delete, mirror, update (with preview + undo log)
- [x] Two-way sync with history, journaled crash-resumable runs, rollback, locked-file handling
- [x] **Text Merge**: 3-way merge with base, conflict resolution, `git mergetool` support
- [x] Better line alignment inside replace blocks (similarity-based pairing)
- [x] Ignore rules (regex-defined unimportant text) · [x] language grammars (comments, strings)
- [x] **Table / CSV compare** with key columns
- [x] **Image compare** (side-by-side, overlay, pixel-diff, tolerance)
- [ ] Archive compare (zip / 7z / tar as folders)
- [x] Saved sessions
- [x] Workspaces (save all open tabs), file-format rules per extension
- [x] HTML / text reports, folder compare reports
- [x] Progress + cancellation for huge folder trees
- [x] Windows Explorer context menu ("Select left side" / "Compare to…")
- [x] Remote: SFTP / FTP / S3 / WebDAV / OneDrive / Dropbox / Google Drive / Azure / GCS / B2
- [x] Remote write-back (sync actions on remote profiles)

### Phase 3 — Extended features (beyond Beyond Compare)
- [ ] Structural diff for JSON / YAML / XML (key-order-insensitive)
- [x] Syntax highlighting · [ ] semantic (AST-aware) code diff
- [ ] Moved-block detection
- [x] Office/PDF text extraction compare (docx, doc, odt, rtf, pdf)
- [ ] Git integration: compare branches/commits, blame-aware view
- [x] Watch mode — live re-compare on file change
- [ ] AI-generated change summary of a diff
- [x] Light theme & custom color schemes

## Development

Prerequisites: [Rust](https://rustup.rs), Node.js 20+, and on Windows the [WebView2 runtime](https://developer.microsoft.com/microsoft-edge/webview2/) (preinstalled on Windows 10/11).

```bash
npm install
npm run tauri dev          # run the app with hot reload
cargo test -p pcompare-core # engine tests
npm run tauri build        # installers for the current OS (needs the updater key, see below)
```

## Releasing

Every pushed `v*` tag runs [`.github/workflows/release.yml`](.github/workflows/release.yml), which builds
**Windows** (NSIS `.exe` + `.msi`) and **macOS** (Apple Silicon and Intel `.dmg`) installers, signs the update
packages, and publishes a GitHub release with `latest.json` — the feed installed copies poll for updates.
The release stays a draft until every platform has built.

```bash
npm run version:bump 0.2.0         # package.json, Cargo.toml, tauri.conf.json stay in sync
cargo check                        # refresh Cargo.lock
git commit -am "Release v0.2.0" && git tag v0.2.0 && git push --follow-tags
```

The workflow refuses to run if the tag doesn't match the app version — otherwise the updater would keep
offering the same release.

**One-time setup**

1. **Updater signing key** (required). Updates are verified against the public key in `tauri.conf.json`;
   the private half lives outside the repo at `~/.tauri/pcompare-updater.key`. Add it as the repository
   secret `TAURI_SIGNING_PRIVATE_KEY` (the key has no password, so `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` can be
   left unset). Back the key up — losing it means installed copies can never update again.
   ```bash
   gh secret set TAURI_SIGNING_PRIVATE_KEY < ~/.tauri/pcompare-updater.key
   ```
   For a local `npm run tauri build`, export the same variable first:
   `export TAURI_SIGNING_PRIVATE_KEY="$(cat ~/.tauri/pcompare-updater.key)"`.
2. **macOS signing** (optional). Without an Apple Developer ID the app is ad-hoc signed: on first launch users
   right-click → Open (or allow it under *System Settings → Privacy & Security*). To ship a notarized app, add
   `APPLE_CERTIFICATE`, `APPLE_CERTIFICATE_PASSWORD`, `APPLE_SIGNING_IDENTITY`, `APPLE_ID`, `APPLE_PASSWORD`
   (app-specific) and `APPLE_TEAM_ID` secrets — the workflow picks them up automatically.
3. **Windows signing** (optional). Unsigned installers trigger a SmartScreen warning until the app builds
   reputation; an Authenticode certificate removes it.

### Project layout

```
crates/pcompare-core/    Rust diff engines (text, folder, hex) + saved sessions — UI-agnostic, unit tested
crates/pcompare-remote/  Connection profiles and remote file systems (OpenDAL + russh SFTP)
crates/pcompare-merge/   3-way text merge (diff3)
crates/pcompare-table/   Table compare: CSV/TSV, Excel/ODS (calamine), HTML tables
crates/pcompare-picture/ Picture compare (image crate)
crates/pcompare-sync/    Folder sync planning, fail-safe execution (retries, locked files, rollback) and 3-way folder merge
crates/pcompare-index/   Embedded SQLite scan database: checksum cache, two-way sync baselines, job journal
crates/pcompare-meta/    Registry (.reg / live), executable version (pelite) and audio tag (lofty) compare
crates/pcompare-db/      Database schema/data compare + sync scripts (sqlx: MySQL/PostgreSQL/SQLite, tiberius: SQL Server)
crates/pcompare-report/  Text / folder / table reports (HTML, text, unified patch, CSV)
crates/pcompare-script/  Script processor (`pcompare @script.txt`) and headless quick compare (`/qc`)
src-tauri/               Tauri shell exposing the engines as commands
src/                     React + TypeScript UI
```

### Connecting OneDrive, Dropbox or Google Drive

These use OAuth sign-in, which needs an app registration you control:

1. Register an app in the provider's developer console (linked from the profile form).
2. Add the redirect URI `http://localhost:53682/` (Microsoft: "Mobile and desktop applications"; Google: "Desktop app").
3. Paste the Client ID (and, for Google, the client secret) into the profile and click **Sign in…**.

### Testing against live servers

`cargo test -p pcompare-remote --test live` is skipped unless `PCOMPARE_LIVE_PROFILES` points at a JSON list of profiles; see the header of `crates/pcompare-remote/tests/live.rs` for the format and fixture layout.

Database Compare's SQLite tests always run. The MySQL, PostgreSQL and SQL Server tests (`crates/pcompare-db/tests/live_*.rs` and the end-to-end `roundtrip.rs`, which syncs two databases and expects an identical re-compare) run when `PCOMPARE_LIVE_MYSQL`, `PCOMPARE_LIVE_POSTGRES` or `PCOMPARE_LIVE_MSSQL` hold a connection as JSON. Point them at scratch servers, because they create and drop databases:

```bash
docker run -d --name pcompare-mssql -e ACCEPT_EULA=Y -e 'MSSQL_SA_PASSWORD=Pcompare!2024' -p 1433:1433 mcr.microsoft.com/mssql/server:2022-latest
```

```bash
PCOMPARE_LIVE_MSSQL='{"engine":"mssql","host":"127.0.0.1","port":1433,"user":"sa","password":"Pcompare!2024","trustServerCertificate":true,"database":"master"}' cargo test -p pcompare-db --test live_mssql -- --nocapture
```

### Data compare rules

Cells are matched by `values_equal` in `crates/pcompare-db/src/value.rs`. Numbers compare by value across types (`bit` 1 = `tinyint(1)` 1, `12.50` = `12.5`), and NULL only equals NULL. Everything else is opt-in from the Data options dialog: ignore case, trim whitespace (CHAR padding), a numeric tolerance, and a date-time precision (SQL Server `datetime` keeps about 3 fractional digits, MySQL and PostgreSQL keep 6).

### Command line

```text
pcompare [options] <left> [<right>]                  compare two files or folders
pcompare [options] <left> <right> <base> [<output>]  3-way merge (Beyond Compare's argument order)
pcompare @<script> [<param>…] [/silent]               run a script without a window
pcompare /qc[=size|crc|binary|rules] <left> <right>   quick compare without a window
```

Switches take Beyond Compare's spelling (`/name=value`, case-insensitive) or a GNU-style one (`--name value` /
`--name=value`); `/`-switches are recognised only by their exact names, so `/Users/…` stays a path.

| Switch | What it does |
|---|---|
| `/fv=<viewer>`, `--viewer <kind>` | Open as a specific session: `text`, `hex`, `table`, `picture`, `folder`, `sync`, `merge`, `folderMerge`, `registry`, `version`, `mp3`, `database` (BC names such as `"Text Compare"` or `"Data Compare"` work too) |
| `/title1=<text>`, `/title2=<text>`, `--title-left`, `--title-right` | Tab labels instead of the file names (handy for git's temporary files) |
| `/readonly` (`/ro`), `/lro`, `/rro`, `--readonly[-left\|-right]` | Open sides read-only in Text Compare |
| `/mergeoutput=<path>`, `--merge-output <path>` | Where a 3-way merge writes its result |
| `/qc[=<method>]`, `--quick-compare[=<method>]` | Compare headlessly and print one line. `rules` (default) applies the text rules to text files and compares other files byte by byte; `binary`, `crc` and `size` are stricter or quicker. Folders are compared recursively |
| `@<file>`, `/silent` | Run a script (below); `/silent` prints only errors |
| `--select-left <path>`, `--compare-to <path>` | Explorer's "Select Left Side" / "Compare to" |
| `-h`, `--help`, `/?` | Usage |

Exit codes: quick compare `0` = same (or only unimportant differences), `1` = different, `2` = error; scripts `0` =
success, `1` = some commands failed, `2` = the script is unreadable or has syntax errors (nothing ran). With
PCompare already open, a new `pcompare <paths>` (switches included) opens a tab in that window.

On Windows the installed `pcompare.exe` is a GUI program: `cmd.exe` doesn't wait for it, so use
`start /wait pcompare.exe /qc a b` (or PowerShell's `Start-Process -Wait`) to read `%ERRORLEVEL%`; output goes to the
console the command was typed in, or wherever it is redirected (`> result.txt`).

### Reports

**Report…** in Text Compare, Folder Compare (toolbar) and Table Compare saves, copies or prints the comparison:

- **Text**: side-by-side or unified layout, all lines or differences only with *N* context lines, as HTML (the app's
  diff colours with changed-word highlights), plain text or a unified patch (`patch` / `git apply`); line numbers,
  ignore unimportant differences, black and white. Unsaved edits are included.
- **Folder**: summary (status and path) or detailed (both sides with sizes and dates), honouring the display filter
  and name filter (or differences only / everything), as HTML, text or CSV. *Link to file comparison reports* writes a
  text report for every differing text file into `<report>_files/` next to the saved HTML report and links it.
- **Table**: an HTML grid with differing cells highlighted (left value over right value), or CSV.

Copying HTML puts rich text on the clipboard (pastes formatted into Word or mail). Printing uses the webview's print
dialog on Windows and Linux; on macOS, whose web view can't print, the report opens in the default browser with its
print dialog.

### Scripts

`pcompare @nightly.txt "D:/work" "E:/backup"` runs a Beyond Compare–style script without opening a window. One
command per line, keywords are case-insensitive, `#` starts a comment line, arguments with spaces go in double quotes,
`%1`–`%9` are the parameters after the script name and `%NAME%` is an environment variable. The whole script is
checked before anything runs.

```text
# Mirror a project to a backup and keep a report of what changed.
log verbose "%TEMP%/backup.log"
criteria timestamp:2sec size
filter "-target\;-node_modules\;-*.tmp"
load "%1" "%2"
expand all
folder-report layout:side-by-side options:display-mismatches,include-file-links output-to:"%2/../changes.html"
sync create-empty mirror:left->right
```

| Command | |
|---|---|
| `log [normal\|verbose] [append:]<file>` | Log commands, results and errors (verbose: every file operation) |
| `criteria <timestamp[:Nsec;IgnoreDST]\|size\|crc\|binary\|rules-based\|version\|attrib>…` | How files are compared (default `timestamp size`) |
| `filter "<mask>;-<mask>;<folder>\;-<folder>\"` | Include / exclude file masks; masks ending in `\` or `/` are folders |
| `load <left> <right>` | Compare two folders (or two files, for `text-report`) |
| `expand all\|<folder>`, `collapse all\|<folder>` | Like the folder view: `select` and reports only see expanded folders' contents |
| `select <side>.<status>.<type>…` | e.g. `all.diff.files`, `left.orphan`, `right.newer.files`, `all`; side `left\|right\|all`, status `all\|diff\|same\|exact\|orphan\|newer\|older`, type `files\|folders` |
| `compare [crc\|binary\|rules-based]` | Content-compare the selected files |
| `folder-report layout:<summary\|side-by-side> [options:<display-all\|display-mismatches\|display-matches\|display-orphans\|display-left-newer…>,include-file-links,column-size,column-timestamp] [title:<text>] output-to:<file> [output-options:<html-color\|html-mono\|text\|csv>]` | Folder report (format defaults from the file extension) |
| `text-report layout:<side-by-side\|interleaved\|patch> [options:display-all\|display-mismatches\|display-context[=N],line-numbers,ignore-unimportant] output-to:<file> [output-options:html-color\|html-mono\|text,wrap-none]` | Text report of the loaded files, or of every selected file pair in one report |
| `sync [create-empty] <mirror\|update>:<left->right\|right->left>`, `sync update:all` | Synchronize (fail-safe executor: atomic copies, retries, deletions to the Recycle Bin / Trash) |
| `copy [left->right\|right->left]`, `delete [left\|right\|all]` | Copy or delete the selection (folders with their contents) |
| `option confirm:<yes-to-all\|no-to-all\|prompt>` | Overwrites and deletions by `copy` / `delete` need confirmation: asked on the terminal (`prompt`, the default), never asked with `/silent` (skipped) |
| `option stop-on-error`, `option recycle:<yes\|no>` | Stop at the first failing command (otherwise the script goes on and exits with 1); delete permanently instead of to the Recycle Bin |

Scripts work on local folders; `printer` and `clipboard` outputs need the window.

### Use as a git difftool

```bash
git config --global diff.tool pcompare
# $BASE is the file's name in the repository; the titles replace git's temporary file names.
git config --global difftool.pcompare.cmd '"C:/Program Files/PCompare/pcompare.exe" /title1="$BASE (old)" /title2="$BASE (new)" "$LOCAL" "$REMOTE"'
```

To check whether two revisions differ without opening a window: `pcompare /qc "$LOCAL" "$REMOTE"` (exit code 0 or 1).

### Use as a git mergetool

The argument order matches Beyond Compare (`left right base output`):

```bash
git config --global merge.tool pcompare
git config --global mergetool.pcompare.cmd '"C:/Program Files/PCompare/pcompare.exe" "$LOCAL" "$REMOTE" "$BASE" /mergeoutput="$MERGED"'
git config --global mergetool.pcompare.trustExitCode false
```

## License

MIT
