//! "File formats" rules (which files are converted, made read-only or given
//! a grammar) and "External tools" rules (which files open in a third-party
//! diff utility), stored together in one JSON file.

use crate::Converter;
use pcompare_core::{CoreError, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

fn yes() -> bool {
    true
}

/// One file format rule. The first enabled rule whose mask matches a file
/// applies to it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileFormat {
    pub name: String,
    /// File name masks separated by `;` (e.g. `*.htm;*.html`). A mask with a
    /// path separator matches the whole path instead of the file name.
    pub mask: String,
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default)]
    pub converter: Converter,
    /// External conversion command; when set it replaces `converter`. `%s` is
    /// the input file and `%t` the output file the command writes (without
    /// `%t`, its standard output is the text).
    #[serde(default)]
    pub command: String,
    /// Converted text can't be saved back over the original.
    #[serde(default = "yes")]
    pub read_only: bool,
    /// Grammar id (see `pcompare_core::grammar`) used instead of the one the
    /// file extension picks; empty for automatic.
    #[serde(default)]
    pub grammar: String,
}

impl FileFormat {
    pub fn matches(&self, path: &str) -> bool {
        self.enabled && mask_matches(&self.mask, path)
    }

    /// Whether this rule converts the file's content at all.
    pub fn converts(&self) -> bool {
        !self.command.trim().is_empty() || self.converter != Converter::None
    }
}

/// One external comparison tool rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExternalTool {
    pub name: String,
    pub mask: String,
    #[serde(default = "yes")]
    pub enabled: bool,
    /// Command line with `%1` / `%2` for the left / right file, e.g.
    /// `"C:\Tools\Diff.exe" %1 %2`. Without placeholders both paths are appended.
    pub command: String,
    /// Pairs matching this rule always open in the tool instead of a built-in viewer.
    #[serde(default)]
    pub always: bool,
}

impl ExternalTool {
    /// Every given (non-empty) side must match the mask.
    pub fn matches_pair(&self, left: &str, right: &str) -> bool {
        let sides: Vec<&str> = [left, right].into_iter().filter(|p| !p.is_empty()).collect();
        self.enabled
            && !self.command.trim().is_empty()
            && !sides.is_empty()
            && sides.iter().all(|p| mask_matches(&self.mask, p))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConvertConfig {
    #[serde(default)]
    pub formats: Vec<FileFormat>,
    #[serde(default)]
    pub external_tools: Vec<ExternalTool>,
}

impl Default for ConvertConfig {
    /// The built-in document formats, as stock rules.
    fn default() -> Self {
        let rule = |name: &str, mask: &str, converter| FileFormat {
            name: name.into(),
            mask: mask.into(),
            enabled: true,
            converter,
            command: String::new(),
            read_only: true,
            grammar: String::new(),
        };
        ConvertConfig {
            formats: vec![
                rule("Microsoft Word", "*.docx;*.docm;*.dotx;*.doc;*.dot", Converter::Word),
                rule("Adobe PDF", "*.pdf", Converter::Pdf),
                rule("Rich Text", "*.rtf", Converter::Rtf),
                rule("OpenDocument Text", "*.odt;*.ott", Converter::Odt),
                rule("Delphi Forms", "*.dfm;*.xfm;*.fmx", Converter::Dfm),
            ],
            external_tools: Vec::new(),
        }
    }
}

impl ConvertConfig {
    /// Loads the rules file; a missing file means the defaults.
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text)
                .map_err(|e| CoreError::Other(format!("{}: invalid file format rules: {e}", path.display()))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(CoreError::io(path, e)),
        }
    }

    /// Writes the rules file (via a temporary file, so a crash can't truncate it).
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| CoreError::io(dir, e))?;
        }
        let json = serde_json::to_string_pretty(self).map_err(|e| CoreError::Other(e.to_string()))?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json).map_err(|e| CoreError::io(&tmp, e))?;
        std::fs::rename(&tmp, path).map_err(|e| CoreError::io(path, e))
    }

    /// The first enabled file format rule matching `path`.
    pub fn format_for(&self, path: &str) -> Option<&FileFormat> {
        self.formats.iter().find(|f| f.matches(path))
    }

    /// Enabled external tools that accept both sides.
    pub fn tools_for<'a>(&'a self, left: &'a str, right: &'a str) -> impl Iterator<Item = &'a ExternalTool> + 'a {
        self.external_tools.iter().filter(move |t| t.matches_pair(left, right))
    }
}

/// Whether `path` matches any of the `;`-separated wildcard masks (`*`, `?`;
/// case-insensitive). Masks without a path separator match the file name.
pub fn mask_matches(masks: &str, path: &str) -> bool {
    let path = path.trim_end_matches(['/', '\\']);
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    masks
        .split([';', ','])
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .any(|mask| {
            if mask.contains(['/', '\\']) {
                wildcard(
                    &mask.replace('\\', "/").to_lowercase(),
                    &path.replace('\\', "/").to_lowercase(),
                )
            } else {
                wildcard(&mask.to_lowercase(), &name.to_lowercase())
            }
        })
}

/// `*` / `?` matching with backtracking on the last `*`.
fn wildcard(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some((pi, ti));
            pi += 1;
        } else if let Some((sp, st)) = star {
            pi = sp + 1;
            ti = st + 1;
            star = Some((sp, st + 1));
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|&c| c == '*')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks() {
        assert!(mask_matches("*.docx;*.doc", r"C:\Docs\Report.DOCX"));
        assert!(mask_matches("*.doc", "remote://p/a/b.doc"));
        assert!(!mask_matches("*.doc", "b.docx"));
        assert!(mask_matches("a?c.*", "abc.txt"));
        assert!(mask_matches("*", "anything"));
        assert!(mask_matches("*/build/*.json", "/src/build/out.json"));
        assert!(!mask_matches("*/build/*.json", "/src/out.json"));
        assert!(!mask_matches("", "x"));
        assert!(mask_matches("*a*b*c", "xxaYYbZZc"));
    }

    #[test]
    fn first_matching_enabled_rule_wins() {
        let mut cfg = ConvertConfig::default();
        assert_eq!(cfg.format_for("x.PDF").unwrap().converter, Converter::Pdf);
        assert_eq!(cfg.format_for("x.doc").unwrap().converter, Converter::Word);
        assert!(cfg.format_for("x.txt").is_none());
        cfg.formats.insert(
            0,
            FileFormat {
                name: "Off".into(),
                mask: "*.pdf".into(),
                enabled: false,
                converter: Converter::Json,
                command: String::new(),
                read_only: false,
                grammar: String::new(),
            },
        );
        assert_eq!(cfg.format_for("x.pdf").unwrap().name, "Adobe PDF");
    }

    #[test]
    fn tools_need_every_side_to_match() {
        let tool = ExternalTool {
            name: "Diff".into(),
            mask: "*.psd".into(),
            enabled: true,
            command: "diff %1 %2".into(),
            always: true,
        };
        assert!(tool.matches_pair("a.psd", "b.PSD"));
        assert!(tool.matches_pair("a.psd", ""));
        assert!(!tool.matches_pair("a.psd", "b.txt"));
        assert!(!tool.matches_pair("", ""));
    }

    #[test]
    fn load_save_round_trip_and_defaults() {
        let dir = std::env::temp_dir().join(format!("pcompare-convert-rules-{}", std::process::id()));
        let path = dir.join("file-formats.json");
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(ConvertConfig::load(&path).unwrap(), ConvertConfig::default());
        let mut cfg = ConvertConfig::default();
        cfg.external_tools.push(ExternalTool {
            name: "Tool".into(),
            mask: "*.bin".into(),
            enabled: true,
            command: "\"C:\\Tools\\Diff.exe\" %1 %2".into(),
            always: false,
        });
        cfg.save(&path).unwrap();
        assert_eq!(ConvertConfig::load(&path).unwrap(), cfg);
        // Omitted fields take their defaults.
        std::fs::write(
            &path,
            r#"{"formats":[{"name":"J","mask":"*.json","converter":"json"}]}"#,
        )
        .unwrap();
        let cfg = ConvertConfig::load(&path).unwrap();
        assert!(cfg.formats[0].enabled && cfg.formats[0].read_only && cfg.external_tools.is_empty());
        std::fs::write(&path, "{").unwrap();
        assert!(ConvertConfig::load(&path).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
