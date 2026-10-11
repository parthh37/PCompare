# PCompare

A fast, modern file & folder comparison tool for Windows — inspired by Beyond Compare, built with **Rust + Tauri 2 + React**.

![status](https://img.shields.io/badge/status-early%20development-orange) ![license](https://img.shields.io/badge/license-MIT-blue)

## Features (v0.1)

| Session | What it does |
|---|---|
| **Folder Compare** | Recursive, parallel (rayon) comparison by size+timestamp, size, or byte content · filters (All / Differences / Same / Orphans) · glob excludes · hidden-file toggle · case-insensitive name pairing on Windows · double-click to drill into files |
| **Text Compare** | Side-by-side aligned diff · word-level intra-line highlights · Patience or Myers algorithm · ignore whitespace (trailing / changes / all) and case, with *unimportant* differences shown separately · All / Context / Diffs-only views · overview map · prev/next difference (Alt+↑/↓) · encoding (UTF-8/16 BOM, Windows-1252) and line-ending detection |
| **Hex Compare** | Streams files of any size in 4 KB pages · byte-level highlighting · next-difference search |
| **Shell** | Tabbed sessions · drag two files/folders onto the window · `pcompare <left> <right>` command line |

## Roadmap

### Phase 2 — Beyond Compare parity
- [ ] Editable text panes, copy hunk left ⇄ right, save with original encoding/line endings
- [ ] Folder **sync** actions: copy, move, delete, mirror, update (with preview + undo log)
- [ ] **Text Merge**: 3-way merge with base, conflict resolution, `git mergetool` support
- [ ] Better line alignment inside replace blocks (similarity-based pairing)
- [ ] Ignore rules / "grammar" (comments, regex-defined unimportant text)
- [ ] **Table / CSV compare** with key columns
- [ ] **Image compare** (side-by-side, overlay, pixel-diff, tolerance)
- [ ] Archive compare (zip / 7z / tar as folders)
- [ ] Saved sessions & workspaces, file-format rules per extension
- [ ] HTML / text reports, folder compare reports
- [ ] Progress + cancellation for huge folder trees
- [ ] Windows Explorer context menu ("Select left side" / "Compare to…")
- [ ] Remote: SFTP / FTP / S3 / WebDAV

### Phase 3 — Extended features (beyond Beyond Compare)
- [ ] Structural diff for JSON / YAML / XML (key-order-insensitive)
- [ ] Syntax highlighting & semantic (AST-aware) code diff
- [ ] Moved-block detection
- [ ] Office/PDF text extraction compare (docx, xlsx, pdf)
- [ ] Git integration: compare branches/commits, blame-aware view
- [ ] Watch mode — live re-compare on file change
- [ ] AI-generated change summary of a diff
- [ ] Light theme & custom color schemes

## Development

Prerequisites: [Rust](https://rustup.rs), Node.js 20+, and on Windows the [WebView2 runtime](https://developer.microsoft.com/microsoft-edge/webview2/) (preinstalled on Windows 10/11).

```bash
npm install
npm run tauri dev          # run the app with hot reload
cargo test -p pcompare-core # engine tests
npm run tauri build        # produce MSI + NSIS installers (on Windows)
```

### Project layout

```
crates/pcompare-core/   Rust diff engines (text, folder, hex) — UI-agnostic, unit tested
src-tauri/              Tauri shell exposing the engines as commands
src/                    React + TypeScript UI
```

### Use as a git difftool

```bash
git config --global diff.tool pcompare
git config --global difftool.pcompare.cmd '"C:/Program Files/PCompare/pcompare.exe" "$LOCAL" "$REMOTE"'
```

## License

MIT
