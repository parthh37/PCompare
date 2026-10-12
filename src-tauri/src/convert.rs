//! Tauri commands for file format rules (document → text conversion before
//! Text Compare) and external comparison tools. Both rule lists live in
//! `file-formats.json` in the app config folder.

use crate::CmdResult;
use pcompare_convert::{ConvertConfig, ExternalTool, FileFormat};
use pcompare_core::CoreError;
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use tauri::{AppHandle, Manager};

const CONFIG_FILE: &str = "file-formats.json";

fn config_path(app: &AppHandle) -> pcompare_core::Result<PathBuf> {
    let dir = app
        .path()
        .app_config_dir()
        .map_err(|e| CoreError::Other(format!("no config folder: {e}")))?;
    Ok(dir.join(CONFIG_FILE))
}

/// The saved rules. A damaged rules file must not stop every text compare,
/// so loading for a compare falls back to the defaults (the File formats
/// dialog reports the error when opened).
pub fn rules(app: &AppHandle) -> ConvertConfig {
    config_path(app)
        .and_then(|p| ConvertConfig::load(&p))
        .unwrap_or_else(|e| {
            eprintln!("file format rules: {e}");
            ConvertConfig::default()
        })
}

/// What a file format rule did to one Text Compare side.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Conversion {
    /// The rule's name.
    pub rule: String,
    /// What converted the text ("PDF", "Word", "external command"), if it was converted.
    pub converted_from: Option<String>,
    pub read_only: bool,
    /// Grammar id the rule forces, if any.
    pub grammar: Option<String>,
}

static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Applies the first matching rule to a side whose content is at `local` and
/// whose path (for mask matching and messages) is `display`. Returns `None`
/// when no rule matches; otherwise the rule's effect and the converted text
/// (`None` when the content passes through unconverted, e.g. a text DFM).
pub fn apply_format(
    config: &ConvertConfig,
    local: &Path,
    display: &str,
) -> pcompare_core::Result<Option<(Conversion, Option<String>)>> {
    let Some(rule) = config.format_for(display) else {
        return Ok(None);
    };
    let (text, from) = convert_with(rule, local, display)?;
    let conversion = Conversion {
        rule: rule.name.clone(),
        // Only converted text is protected by a converting rule (document
        // text always: saving it would overwrite the document); a rule
        // without a converter makes the file itself read-only if asked to.
        read_only: (rule.read_only || (rule.command.trim().is_empty() && rule.converter.is_lossy()))
            && (text.is_some() || !rule.converts()),
        converted_from: text.as_ref().map(|_| from),
        grammar: Some(rule.grammar.trim().to_string()).filter(|g| !g.is_empty()),
    };
    Ok(Some((conversion, text)))
}

fn convert_with(rule: &FileFormat, local: &Path, display: &str) -> pcompare_core::Result<(Option<String>, String)> {
    let command = rule.command.trim();
    if command.is_empty() {
        let text = pcompare_convert::convert_file(rule.converter, local).map_err(|e| match e {
            // Report the user's path, not a download-cache copy.
            CoreError::Other(m) => CoreError::Other(m.replace(&local.display().to_string(), display)),
            e => e,
        })?;
        return Ok((text, rule.converter.label().to_string()));
    }
    let dir = std::env::temp_dir().join("pcompare-convert");
    std::fs::create_dir_all(&dir).map_err(|e| CoreError::io(&dir, e))?;
    let out = dir.join(format!(
        "{}-{}.txt",
        std::process::id(),
        TEMP_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let text = pcompare_convert::run_conversion_command(command, local, &out)
        .map_err(|e| CoreError::Other(format!("{display}: {e}")))?;
    Ok((Some(text), "external command".into()))
}

#[tauri::command]
pub async fn convert_config_load(app: AppHandle) -> CmdResult<ConvertConfig> {
    crate::blocking(move || ConvertConfig::load(&config_path(&app)?)).await
}

#[tauri::command]
pub async fn convert_config_save(app: AppHandle, config: ConvertConfig) -> CmdResult<()> {
    crate::blocking(move || config.save(&config_path(&app)?)).await
}

/// The stock rules, for "Reset to defaults".
#[tauri::command]
pub fn convert_config_defaults() -> ConvertConfig {
    ConvertConfig::default()
}

/// Enabled external tools whose mask matches both sides.
#[tauri::command]
pub async fn convert_tools_for(app: AppHandle, left: String, right: String) -> CmdResult<Vec<ExternalTool>> {
    crate::blocking(move || Ok(rules(&app).tools_for(&left, &right).cloned().collect())).await
}

/// Starts an external comparison tool on two files (remote sides are
/// downloaded first). The tool runs on its own; this doesn't wait for it.
#[tauri::command]
pub async fn convert_launch_tool(app: AppHandle, command: String, left: String, right: String) -> CmdResult<()> {
    crate::blocking(move || {
        let local = |r: &pcompare_remote::RemoteManager, p: &str| -> pcompare_core::Result<String> {
            if p.is_empty() {
                return Ok(String::new());
            }
            Ok(r.local_copy(p, true)?.display().to_string())
        };
        let (l, r) = crate::with_remote(&app, [&left, &right], |r| Ok((local(r, &left)?, local(r, &right)?)))?;
        pcompare_convert::expand_tool_command(&command, &l, &r)?.spawn_detached()
    })
    .await
}

/// A file's original text, unconverted (the HTML rendered view of a side
/// whose text came through a converter).
#[tauri::command]
pub async fn convert_source_text(app: AppHandle, path: String) -> CmdResult<String> {
    crate::blocking(move || {
        let local = crate::with_remote(&app, [&path, ""], |r| r.local_copy(&path, true))?;
        Ok(pcompare_core::textfile::load_text(&local)?.text)
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use pcompare_convert::Converter;

    fn rule(mask: &str, converter: Converter) -> FileFormat {
        FileFormat {
            name: "R".into(),
            mask: mask.into(),
            enabled: true,
            converter,
            command: String::new(),
            read_only: true,
            grammar: String::new(),
        }
    }

    #[test]
    fn applies_first_matching_rule() {
        let dir = std::env::temp_dir().join(format!("pcompare-convert-cmd-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let json = dir.join("a.json");
        std::fs::write(&json, r#"{"b":1,"a":2}"#).unwrap();
        let mut cfg = ConvertConfig {
            formats: vec![rule("*.json", Converter::Json)],
            external_tools: Vec::new(),
        };
        let (conv, text) = apply_format(&cfg, &json, "shown/a.json").unwrap().unwrap();
        assert_eq!(text.as_deref(), Some("{\n  \"a\": 2,\n  \"b\": 1\n}\n"));
        assert!(conv.read_only);
        assert_eq!(conv.converted_from.as_deref(), Some("JSON"));
        assert!(apply_format(&cfg, &json, "a.txt").unwrap().is_none());

        // A rule without a converter only sets the grammar / read-only flag.
        cfg.formats[0] = FileFormat {
            converter: Converter::None,
            grammar: " sql ".into(),
            read_only: false,
            ..rule("*.json", Converter::None)
        };
        let (conv, text) = apply_format(&cfg, &json, "a.json").unwrap().unwrap();
        assert_eq!(text, None);
        assert_eq!(conv.grammar.as_deref(), Some("sql"));
        assert!(!conv.read_only && conv.converted_from.is_none());

        // Pass-through (a text DFM) stays editable under a converting rule.
        let dfm = dir.join("f.dfm");
        std::fs::write(&dfm, "object F: TF\nend\n").unwrap();
        cfg.formats[0] = rule("*.dfm", Converter::Dfm);
        let (conv, text) = apply_format(&cfg, &dfm, "f.dfm").unwrap().unwrap();
        assert!(text.is_none() && !conv.read_only);

        // Converted document text stays read-only even if the rule allows saving.
        let rtf = dir.join("a.rtf");
        std::fs::write(&rtf, r"{\rtf1 Hi\par}").unwrap();
        cfg.formats[0] = FileFormat {
            read_only: false,
            ..rule("*.rtf", Converter::Rtf)
        };
        let (conv, text) = apply_format(&cfg, &rtf, "a.rtf").unwrap().unwrap();
        assert_eq!(text.as_deref(), Some("Hi\n"));
        assert!(conv.read_only);
        // Reformatted text (JSON) can be saved back when the rule allows it.
        cfg.formats[0] = FileFormat {
            read_only: false,
            ..rule("*.json", Converter::Json)
        };
        std::fs::write(&json, "[1]").unwrap();
        assert!(!apply_format(&cfg, &json, "a.json").unwrap().unwrap().0.read_only);

        // Errors name the user's path, not the local copy.
        std::fs::write(&json, "{").unwrap();
        cfg.formats[0] = rule("*.json", Converter::Json);
        let err = apply_format(&cfg, &json, "remote://p/a.json").unwrap_err().to_string();
        assert!(err.starts_with("remote://p/a.json: cannot convert"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
