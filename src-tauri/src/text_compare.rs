//! Tauri commands for Text Compare: loading with a chosen or detected
//! encoding, diffing edited text with grammars and per-line terminators, and
//! saving with automatic backups.

use crate::CmdResult;
use pcompare_core::grammar;
use pcompare_core::text::{self, SideInput, TextDiffOptions, TextDiffResult};
use pcompare_core::textfile::{self, BackupOptions, EncodingChoice, LineEnding, TextFileInfo, TextFormat};
use pcompare_core::CoreError;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use tauri::AppHandle;

/// A text file split into lines, plus what is needed to save it back as-is.
/// A superset of `merge.rs`'s document (same field names), so the UI can
/// treat both alike.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TextDocument {
    info: TextFileInfo,
    /// Encoding, BOM and line-ending style used when saving; mixed or missing
    /// line endings resolve to the dominant or native style.
    format: TextFormat,
    /// Empty for binary files.
    lines: Vec<String>,
    /// Each line's terminator as read (`none` for an unterminated last line).
    eols: Vec<LineEnding>,
    trailing_newline: bool,
    /// The file looks binary, so `lines` were not loaded.
    binary: bool,
    /// The encoding was guessed statistically (no BOM, not UTF-8).
    encoding_guessed: bool,
    /// bc5:converters — the file format rule applied to this side, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    conversion: Option<crate::convert::Conversion>,
}

/// Loads `local` (possibly a download-cache copy) as `encoding` (an
/// encoding-picker id, or `None` to detect) and reports `display` as its path.
fn load_document(local: &Path, display: String, encoding: Option<&str>) -> pcompare_core::Result<TextDocument> {
    let encoding = encoding.map(str::trim).filter(|e| !e.is_empty());
    let wide = match encoding {
        Some(e) => textfile::parse_encoding_choice(e)?.0.is_wide(),
        None => false,
    };
    // UTF-16/32 without a BOM is full of NULs; trust an explicit choice.
    if !wide && textfile::file_is_binary(local)? {
        let size = std::fs::metadata(local).map_err(|e| CoreError::io(local, e))?.len();
        return Ok(TextDocument {
            info: TextFileInfo {
                path: display,
                encoding: String::new(),
                has_bom: false,
                line_ending: LineEnding::None,
                size,
            },
            format: TextFormat {
                encoding: "UTF-8".into(),
                has_bom: false,
                line_ending: textfile::NATIVE_LINE_ENDING,
            },
            lines: Vec::new(),
            eols: Vec::new(),
            trailing_newline: false,
            binary: true,
            encoding_guessed: false,
            conversion: None, // bc5:converters
        });
    }
    let loaded = textfile::load_text_as(local, encoding)?;
    let mut format = loaded.format();
    if let Some(choice) = encoding {
        // An explicit "… BOM" choice writes a BOM even if the file had none.
        format.has_bom |= textfile::parse_encoding_choice(choice)?.1;
    }
    let (lines, eols) = textfile::split_lines_with_endings(&loaded.text);
    let trailing_newline = eols.last().is_some_and(|e| *e != LineEnding::None);
    let lines = lines.into_iter().map(str::to_owned).collect();
    let mut info = loaded.info;
    info.path = display;
    Ok(TextDocument {
        info,
        format,
        lines,
        eols,
        trailing_newline,
        binary: false,
        encoding_guessed: loaded.guessed,
        conversion: None, // bc5:converters
    })
}

// bc5:converters — loading through the first matching file format rule.
/// Loads a side like [`load_document`], unless a file format rule converts
/// it (Word, PDF, external command...) or marks it read-only / sets a grammar.
fn load_side(
    rules: &pcompare_convert::ConvertConfig,
    local: &Path,
    display: String,
    encoding: Option<&str>,
) -> pcompare_core::Result<TextDocument> {
    let Some((conversion, text)) = crate::convert::apply_format(rules, local, &display)? else {
        return load_document(local, display, encoding);
    };
    let mut doc = match text {
        Some(text) => converted_document(local, display, &text)?,
        None => load_document(local, display, encoding)?,
    };
    doc.conversion = Some(conversion);
    Ok(doc)
}

/// A side holding converted text (UTF-8, LF), sized as the original file.
fn converted_document(local: &Path, display: String, text: &str) -> pcompare_core::Result<TextDocument> {
    let size = std::fs::metadata(local).map_err(|e| CoreError::io(local, e))?.len();
    let (lines, eols) = textfile::split_lines_with_endings(text);
    Ok(TextDocument {
        info: TextFileInfo {
            path: display,
            encoding: "UTF-8".into(),
            has_bom: false,
            line_ending: LineEnding::Lf,
            size,
        },
        format: TextFormat {
            encoding: "UTF-8".into(),
            has_bom: false,
            line_ending: LineEnding::Lf,
        },
        trailing_newline: eols.last().is_some_and(|e| *e != LineEnding::None),
        lines: lines.into_iter().map(str::to_owned).collect(),
        eols,
        binary: false,
        encoding_guessed: false,
        conversion: None,
    })
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TextPair {
    left: TextDocument,
    right: TextDocument,
}

/// Loads both sides of a Text Compare session; each side is detected unless
/// an encoding is given for it.
#[tauri::command]
pub async fn text_load_pair(
    app: AppHandle,
    left: String,
    right: String,
    left_encoding: Option<String>,
    right_encoding: Option<String>,
) -> CmdResult<TextPair> {
    crate::blocking(move || {
        let (lp, rp) = crate::with_remote(&app, [&left, &right], |r| {
            Ok((r.local_copy(&left, true)?, r.local_copy(&right, true)?))
        })?;
        let rules = crate::convert::rules(&app); // bc5:converters
        Ok(TextPair {
            // bc5:converters — file format rules apply per side.
            left: load_side(&rules, &lp, left, left_encoding.as_deref())?,
            right: load_side(&rules, &rp, right, right_encoding.as_deref())?,
        })
    })
    .await
}

/// Loads one side, e.g. after the user picked a different encoding.
#[tauri::command]
pub async fn text_load(app: AppHandle, path: String, encoding: Option<String>) -> CmdResult<TextDocument> {
    crate::blocking(move || {
        let local = crate::with_remote(&app, [&path, ""], |r| r.local_copy(&path, true))?;
        // bc5:converters — file format rules apply.
        load_side(&crate::convert::rules(&app), &local, path, encoding.as_deref())
    })
    .await
}

/// One side of [`text_diff`]: the (possibly edited) lines, their terminators
/// when known, and the path (for picking the grammar).
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffSide {
    lines: Vec<String>,
    #[serde(default)]
    eols: Option<Vec<LineEnding>>,
    #[serde(default)]
    path: Option<String>,
}

/// Diffs text held by the UI with every Text Compare rule.
#[tauri::command]
pub async fn text_diff(left: DiffSide, right: DiffSide, options: TextDiffOptions) -> CmdResult<TextDiffResult> {
    crate::blocking(move || {
        let l: Vec<&str> = left.lines.iter().map(String::as_str).collect();
        let r: Vec<&str> = right.lines.iter().map(String::as_str).collect();
        text::diff_sides(
            SideInput {
                lines: &l,
                eols: left.eols.as_deref(),
                path: left.path.as_deref(),
            },
            SideInput {
                lines: &r,
                eols: right.eols.as_deref(),
                path: right.path.as_deref(),
            },
            &options,
        )
    })
    .await
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SavedText {
    info: TextFileInfo,
    /// Where the previous version was backed up, if a backup was made.
    backup: Option<String>,
}

/// Files already backed up in this app session (see [`BackupOptions::every_save`]).
fn backed_up() -> &'static Mutex<HashSet<PathBuf>> {
    static SET: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
    SET.get_or_init(Default::default)
}

/// Saves, backing up the existing file per `backup` — once per file and app
/// session unless [`BackupOptions::every_save`] is set.
fn save_with_backup(
    path: &str,
    text: &str,
    format: &TextFormat,
    backup: Option<BackupOptions>,
) -> pcompare_core::Result<SavedText> {
    let key = std::fs::canonicalize(path).unwrap_or_else(|_| PathBuf::from(path));
    let backup = backup.filter(|b| b.every_save || !backed_up().lock().is_ok_and(|set| set.contains(&key)));
    let (info, made) = textfile::save_text_with_backup(path, text, format, backup.as_ref())?;
    if backup.is_some() {
        if let Ok(mut set) = backed_up().lock() {
            set.insert(key);
        }
    }
    Ok(SavedText {
        info,
        backup: made.map(|p| p.display().to_string()),
    })
}

/// Writes `text` to a local file in `format`, first backing up the existing
/// file per `backup`. Remote paths are rejected: they are read-only.
#[tauri::command]
pub async fn text_save(
    path: String,
    text: String,
    format: TextFormat,
    backup: Option<BackupOptions>,
) -> CmdResult<SavedText> {
    if pcompare_remote::is_remote(&path) {
        return Err(format!("{path}: remote files are read-only"));
    }
    if pcompare_remote::is_inside_container(&path) {
        return Err(pcompare_remote::read_only_inside(&path)); // bc5:archives
    }
    crate::blocking(move || save_with_backup(&path, &text, &format, backup)).await
}

/// Encodings offered by the per-side encoding picker.
#[tauri::command]
pub fn text_encodings() -> &'static [EncodingChoice] {
    textfile::ENCODINGS
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GrammarInfo {
    id: &'static str,
    name: &'static str,
    extensions: &'static [&'static str],
}

/// The built-in language grammars, for the rules dialog.
#[tauri::command]
pub fn text_grammars() -> Vec<GrammarInfo> {
    grammar::GRAMMARS
        .iter()
        .map(|g| GrammarInfo {
            id: g.id,
            name: g.name,
            extensions: g.extensions,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pcompare_core::textfile::BackupMode;

    /// A fresh scratch directory (removed when dropped).
    struct Scratch(PathBuf);
    impl Scratch {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("pcompare-text-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Scratch(dir)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn loads_lines_terminators_and_manual_encoding() {
        let dir = Scratch::new("load");
        let path = dir.path().join("a.txt");
        std::fs::write(&path, b"caf\xE9\r\nb\n").unwrap();
        let doc = load_document(&path, "shown".into(), None).unwrap();
        assert_eq!(doc.lines, vec!["café", "b"]);
        assert_eq!(doc.eols, vec![LineEnding::Crlf, LineEnding::Lf]);
        assert!(doc.trailing_newline && doc.encoding_guessed && !doc.binary);
        assert_eq!(doc.info.path, "shown");

        // UTF-16 without a BOM looks binary unless chosen explicitly.
        let wide = dir.path().join("w.txt");
        std::fs::write(
            &wide,
            "hi\n".encode_utf16().flat_map(u16::to_le_bytes).collect::<Vec<u8>>(),
        )
        .unwrap();
        assert!(load_document(&wide, String::new(), None).unwrap().binary);
        let doc = load_document(&wide, String::new(), Some("UTF-16LE BOM")).unwrap();
        assert_eq!(doc.lines, vec!["hi"]);
        assert_eq!(doc.format.encoding, "UTF-16LE");
        assert!(doc.format.has_bom, "a BOM choice writes one on save");
        assert!(load_document(&wide, String::new(), Some("nope")).is_err());
    }

    #[test]
    fn backs_up_once_per_session_unless_every_save() {
        let dir = Scratch::new("backup");
        let path = dir.path().join("f.txt");
        let p = path.to_str().unwrap();
        std::fs::write(&path, "v1\n").unwrap();
        let format = TextFormat {
            encoding: "UTF-8".into(),
            has_bom: false,
            line_ending: LineEnding::Lf,
        };
        let bak = BackupOptions::default();
        assert_eq!(bak.mode, BackupMode::Bak);
        let first = save_with_backup(p, "v2\n", &format, Some(bak.clone())).unwrap();
        assert!(first.backup.unwrap().ends_with("f.txt.bak"));
        let second = save_with_backup(p, "v3\n", &format, Some(bak.clone())).unwrap();
        assert_eq!(second.backup, None);
        // The .bak still holds the version from before the session's first save.
        assert_eq!(std::fs::read_to_string(dir.path().join("f.txt.bak")).unwrap(), "v1\n");

        let every = BackupOptions {
            every_save: true,
            ..bak
        };
        save_with_backup(p, "v4\n", &format, Some(every)).unwrap();
        assert_eq!(std::fs::read_to_string(dir.path().join("f.txt.bak")).unwrap(), "v3\n");
    }
}
