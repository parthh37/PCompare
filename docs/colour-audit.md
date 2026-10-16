# Colour meaning audit: Text, Folder and Merge

How the diff palette (`src/styles.css`, plus the view tokens in `TextCompare.css`) carries meaning across views, where meanings clash, and what changed. Screenshots are in `docs/screenshots/text/`.

## Method

- **Distance**: Euclidean distance in OKLab × 100 between two colours as drawn (tints composited over `--bg`, text colours on `--panel`). It is measured for normal vision and for simulated protanopia, deuteranopia and tritanopia (Machado 2009, severity 1, linear sRGB). The table gives the **worst** of the four. Below about 10, two colours are easy to mix up at a glance.
- **Contrast**: WCAG 2 contrast ratio against `--panel`.

## What each colour means today

| Colour | Text Compare | Folder Compare | Folder / Text Merge | Elsewhere |
| --- | --- | --- | --- | --- |
| Red (`--different`, `--del-*`) | line only on the left (removed) | file differs; the newer side of a pair is bold red, the older grey | side deleted; conflict rows (bar, name, badge, count); a conflicting side's change | Sync: delete ops, failures; Hex: differing bytes |
| Amber / orange (`--chg-*`, `--warn`) | changed line; edit markers and unsaved state (`--tc-edited` = warn) | error / file facing a folder | side changed; current conflict | warnings everywhere |
| Green (`--add-*`, `--ok`) | line only on the right (added) | — | side added; auto-merged | success |
| Purple (`--orphan`, `--tc-sync`, `--tc-moved-*`) | sync points; moved blocks; keywords (syntax) | orphan (one side only) | the right side's change / newer | Sync: create folder |
| Blue-grey (`--unimportant`, `--left-fg`) | unimportant text | similar (only unimportant differences); same-but-timestamp clock | the left side's change | — |

## Clashes

Worst-case distance, before → after this change.

| # | Where | Tokens | Dark | Light | Outcome |
| --- | --- | --- | --- | --- | --- |
| 1 | Folder status column and folder tone gradients: orphan next to similar | `--orphan` / `--unimportant` | 3.8 → **10.0** | 8.2 → **10.0** | **Fixed** (tokens) |
| 2 | Folder: different vs error rows; Merge: deleted vs changed sides | `--different` / `--warn` | 8.8 (kept) | 2.6 → **7.9** | **Fixed in light**; dark was already near the line. Both also differ in shape (≠ vs ⚠, merge state icons) |
| 3 | Merge: right side's change vs left side's change | `--orphan` / `--left-fg` | 4.5 → **11.2** | 2.0 → 3.9 | Dark fixed by #1; light still low, but badges are labelled ("Left newer"…) |
| 4 | Red means "different", "newer", "removed", "deleted" and "conflict"; in Folder Compare's split view a red folder row sits over a red removed line | `--different` / `--del-fg` | 1.6 | 2.9 → 8.5 | Kept on purpose (established language). Shapes carry the meaning: ≠ and bold for different/newer in folders, **− glyph** for removed text (new), state icons and ⚠ for merge conflicts |
| 5 | Removed vs added row tints; changed vs removed tints | `--del-bg` / `--add-bg`, `--chg-bg` / `--del-bg` | 1.1, 2.1 | 0.7, 1.7 | Inherent to low-alpha red/green/amber. **Fixed by the change glyphs** (+ − ~ ≈) rather than by colour |
| 6 | Text: sync point vs moved block (both purple); sync dot vs bookmark (blue) | `--tc-sync` / `--tc-moved-fg`, `--bookmark` | 1.5, 1.0 | 9.5, 3.8 | Not changed: different shapes (dot and dashed link vs tinted rows with a "moved" tag, flag for bookmarks). Candidate follow-up |
| 7 | Text: changed (amber) vs edit marker (orange) | `--chg-fg` / `--tc-edited` | 3.0 | 1.7 | Not changed: related meanings, different shapes (row tint and ~ vs a bar) |
| 8 | Text: moved-block tag vs keyword colour; Merge: auto-merged vs added green | `--tc-moved-fg` / `--syn-kw`, `--ok` / `--add-fg` | 2.7, 4.0 | 4.6, 0.3 | Not changed: the tag is a bordered pill, and both greens mean "taken in" |

Contrast notes: line numbers use `--faint` (2.6:1 dark, 2.5:1 light), which is quiet on purpose so that changes stand out. Light `--tc-sync` is 4.2:1, enough for a dot (graphics need 3:1). Every change glyph is at least 5.1:1 in both themes.

## Token changes

| Token | Theme | Before | After | Why |
| --- | --- | --- | --- | --- |
| `--unimportant` | dark | `#7fa6d9` | `#9aadc4` | a quieter slate: unimportant reads as de-emphasised, and its lightness now differs from the orphan purple |
| `--orphan` | dark | `#c38cf5` | `#aa79e5` | a deeper violet (5.2:1), still purple |
| `--orphan` | light | `#8b3fd0` | `#8a3ee0` | a touch bluer, 5.5:1 |
| `--different` | light | `#d0383b` | `#b32c3e` | a deeper red (6.3:1) that no longer has the same lightness as `--warn` |
| `--tc-band-unimp` | dark | from the old unimportant | from the new one | the linked-view band follows `--unimportant` |
| `--chg-glyph`, `--del-glyph`, `--add-glyph` (new) | both | — | row tint made opaque, mixed 70/30 with `--glyph-ink` (white / black) | glyphs follow custom diff colours; they fall back to `--chg-fg` / `--del-fg` / `--add-fg` without relative colour syntax |

No meanings moved: red is still different/removed, amber changed, green added, purple orphans, blue-grey unimportant.

## Change glyphs

Text Compare draws a sign in a 10px slot at the inner edge of each line-number gutter: **+** added (right only), **−** removed (left only), **~** changed, **≈** unimportant (the same sign Folder Compare uses for similar files). Moved lines keep their + or −, in the moved-block colour. The setting is *Settings → Colors → Change glyphs* (or the **…** menu in Text Compare) and is on by default. It works with padding off (linked lines), with the right-side gutter for the left pane, and with blame.
