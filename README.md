# PCompare

A fast, modern file & folder comparison tool for Windows and macOS — inspired by Beyond Compare, built with **Rust + Tauri 2 + React**.

![status](https://img.shields.io/badge/status-early%20development-orange) ![license](https://img.shields.io/badge/license-MIT-blue)

## Features (v0.1)

| Session | What it does |
|---|---|
| **Folder Compare** | Recursive, parallel (rayon) · **quick tests** (size, timestamp with tolerance / DST) then optional **content compare**: *Binary*, *CRC-32* or *Rules-based* (text rules — encoding, line endings, whitespace, case, ignore-regexes — so cosmetic changes show as ≈ *similar*) · override-quick-tests switch · right-click → compare contents of a file or whole folder on demand · **filters**: include/exclude files, exclude folders, size range, modified-date range, hidden files · **display filters**: All / Differences / Same / Orphans / no-orphans / left- or right-only / left- or right-newer · quick name search (`*.rs`) · flatten (ignore folder structure) · swap sides |
| **Text Compare** | Side-by-side aligned diff · word-level intra-line highlights · Patience or Myers algorithm · ignore whitespace (trailing / changes / all), case and regex-defined text (e.g. `\$Id:[^$]*\$`), with *unimportant* differences shown separately · All / Context / Diffs-only views · overview map · prev/next difference (Alt+↑/↓) · encoding (UTF-8/16 BOM, Windows-1252) and line-ending detection |
| **Hex Compare** | Streams files of any size in 4 KB pages · byte-level highlighting · next-difference search |
| **Saved sessions** | Name and save any comparison (Ctrl+S) — paths plus view settings · reopen, rename, delete from Home |
| **Connection profiles** | Amazon S3 & S3-compatible (R2, MinIO, Wasabi…), Azure Blob, Google Cloud Storage, Backblaze B2, FTP/FTPS, SFTP, WebDAV, OneDrive, Dropbox, Google Drive · secrets kept in the OS keychain · use `remote://<profile>/path` (or 🌐) on either side of any compare |
| **Auto-refresh** | Folder, text and hex sessions re-compare by themselves when either side changes on disk (OS file watcher, debounced, ignores excluded folders); remote sides are polled every 30 s |
| **Updates** | Signed over-the-air updates from GitHub Releases — checked at startup and every 6 h, or on demand (⬆ Updates); install & restart in one click |
| **Shell** | Tabbed sessions · drag two files/folders onto the window · `pcompare <left> <right>` command line |

## Roadmap

### Phase 2 — Beyond Compare parity
- [ ] Editable text panes, copy hunk left ⇄ right, save with original encoding/line endings
- [ ] Folder **sync** actions: copy, move, delete, mirror, update (with preview + undo log)
- [ ] **Text Merge**: 3-way merge with base, conflict resolution, `git mergetool` support
- [ ] Better line alignment inside replace blocks (similarity-based pairing)
- [x] Ignore rules (regex-defined unimportant text) · [ ] language grammars (comments, strings)
- [ ] **Table / CSV compare** with key columns
- [ ] **Image compare** (side-by-side, overlay, pixel-diff, tolerance)
- [ ] Archive compare (zip / 7z / tar as folders)
- [x] Saved sessions
- [ ] Workspaces (save all open tabs), file-format rules per extension
- [ ] HTML / text reports, folder compare reports
- [ ] Progress + cancellation for huge folder trees
- [ ] Windows Explorer context menu ("Select left side" / "Compare to…")
- [x] Remote: SFTP / FTP / S3 / WebDAV / OneDrive / Dropbox / Google Drive / Azure / GCS / B2
- [ ] Remote write-back (sync actions on remote profiles)

### Phase 3 — Extended features (beyond Beyond Compare)
- [ ] Structural diff for JSON / YAML / XML (key-order-insensitive)
- [ ] Syntax highlighting & semantic (AST-aware) code diff
- [ ] Moved-block detection
- [ ] Office/PDF text extraction compare (docx, xlsx, pdf)
- [ ] Git integration: compare branches/commits, blame-aware view
- [x] Watch mode — live re-compare on file change
- [ ] AI-generated change summary of a diff
- [ ] Light theme & custom color schemes

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
crates/pcompare-core/   Rust diff engines (text, folder, hex) + saved sessions — UI-agnostic, unit tested
crates/pcompare-remote/ Connection profiles and remote file systems (OpenDAL + russh SFTP)
src-tauri/              Tauri shell exposing the engines as commands
src/                    React + TypeScript UI
```

### Connecting OneDrive, Dropbox or Google Drive

These use OAuth sign-in, which needs an app registration you control:

1. Register an app in the provider's developer console (linked from the profile form).
2. Add the redirect URI `http://localhost:53682/` (Microsoft: "Mobile and desktop applications"; Google: "Desktop app").
3. Paste the Client ID (and, for Google, the client secret) into the profile and click **Sign in…**.

### Testing against live servers

`cargo test -p pcompare-remote --test live` is skipped unless `PCOMPARE_LIVE_PROFILES` points at a JSON list of profiles; see the header of `crates/pcompare-remote/tests/live.rs` for the format and fixture layout.

### Use as a git difftool

```bash
git config --global diff.tool pcompare
git config --global difftool.pcompare.cmd '"C:/Program Files/PCompare/pcompare.exe" "$LOCAL" "$REMOTE"'
```

## License

MIT
