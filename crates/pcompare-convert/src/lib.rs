//! Converters that turn documents into comparable text, and the "File
//! formats" / "External tools" rules that decide which files get converted
//! or handed to a third-party diff utility.
//!
//! Built-in converters: Word (.docx and Word 97-2003 .doc), PDF, Rich Text,
//! OpenDocument text, Delphi binary forms (TPF0 → textual DFM), HTML → plain
//! text, and JSON / XML pretty-printers. A rule can instead run an external
//! command (`conv.exe %s %t`) whose output becomes the compared text.

mod dfm;
mod external;
mod html;
mod office;
mod pdf;
mod pretty;
mod rtf;
mod rules;

use pcompare_core::{CoreError, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

pub use external::{expand_tool_command, run_conversion_command, split_command_line, ToolCommand};
pub use rules::{mask_matches, ConvertConfig, ExternalTool, FileFormat};

/// A built-in converter, as chosen in a file format rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Converter {
    /// No conversion: the file is compared as it is (the rule may still set
    /// read-only or a grammar).
    #[default]
    None,
    /// Microsoft Word: .docx (Office Open XML) or .doc (Word 97-2003).
    Word,
    Pdf,
    Rtf,
    /// OpenDocument text (.odt).
    Odt,
    /// Delphi forms: binary (TPF0) forms become textual DFM; text forms pass through.
    Dfm,
    /// HTML with the markup stripped.
    Html,
    /// JSON pretty-printed with sorted keys.
    Json,
    /// XML pretty-printed.
    Xml,
}

impl Converter {
    /// Whether saving the converted text over the original would destroy it
    /// (a document format rather than reformatted text); such text is always read-only.
    pub fn is_lossy(self) -> bool {
        matches!(
            self,
            Converter::Word | Converter::Pdf | Converter::Rtf | Converter::Odt | Converter::Dfm
        )
    }

    /// Short name shown in banners ("Converted from PDF").
    pub fn label(self) -> &'static str {
        match self {
            Converter::None => "none",
            Converter::Word => "Word",
            Converter::Pdf => "PDF",
            Converter::Rtf => "RTF",
            Converter::Odt => "OpenDocument",
            Converter::Dfm => "Delphi form",
            Converter::Html => "HTML",
            Converter::Json => "JSON",
            Converter::Xml => "XML",
        }
    }
}

/// Converts `bytes` (the content of `name`, used in messages) to text.
/// Returns `None` when the content needs no conversion (a text DFM, or
/// [`Converter::None`]) and should be loaded as an ordinary text file.
pub fn convert_bytes(converter: Converter, bytes: &[u8], name: &str) -> Result<Option<String>> {
    let fail = |e: String| CoreError::Other(format!("{name}: cannot convert from {}: {e}", converter.label()));
    let text = match converter {
        Converter::None => return Ok(None),
        Converter::Word => office::word_to_text(bytes),
        Converter::Odt => office::odt_to_text(bytes),
        Converter::Pdf => pdf::pdf_to_text(bytes),
        Converter::Rtf => rtf::rtf_to_text(bytes),
        Converter::Dfm => {
            if !dfm::is_binary_dfm(bytes) {
                return Ok(None);
            }
            dfm::dfm_to_text(bytes)
        }
        Converter::Html => Ok(html::html_to_text(&decode_text(bytes))),
        Converter::Json => pretty::json_pretty(&decode_text(bytes)),
        Converter::Xml => pretty::xml_pretty(&decode_text(bytes)),
    };
    text.map(|t| Some(normalize(&t))).map_err(fail)
}

/// [`convert_bytes`] on a file.
pub fn convert_file(converter: Converter, path: &Path) -> Result<Option<String>> {
    if converter == Converter::None {
        return Ok(None);
    }
    let bytes = std::fs::read(path).map_err(|e| CoreError::io(path, e))?;
    convert_bytes(converter, &bytes, &path.display().to_string())
}

/// Text from bytes of unknown encoding: BOM, then UTF-8, then a statistical guess.
pub(crate) fn decode_text(bytes: &[u8]) -> String {
    pcompare_core::textfile::decode_auto(bytes).text
}

/// Converted text with `\n` line breaks, no trailing spaces, at most one
/// blank line in a row, and a single final newline.
fn normalize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut blanks = 0;
    for line in text.replace("\r\n", "\n").replace('\r', "\n").split('\n') {
        let line = line.trim_end();
        if line.is_empty() {
            blanks += 1;
            if blanks > 1 || out.is_empty() {
                continue;
            }
        } else {
            blanks = 0;
        }
        out.push_str(line);
        out.push('\n');
    }
    while out.ends_with("\n\n") {
        out.pop();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_line_breaks_and_blank_runs() {
        assert_eq!(normalize("\n\na  \r\n\r\n\r\nb\rc\n\n"), "a\n\nb\nc\n");
        assert_eq!(normalize(""), "");
    }

    #[test]
    fn decodes_bom_utf8_and_ansi() {
        assert_eq!(decode_text(b"\xEF\xBB\xBFhi"), "hi");
        assert_eq!(decode_text(b"\xFF\xFEh\0i\0"), "hi");
        assert_eq!(decode_text(b"caf\xE9"), "café");
        assert_eq!(decode_text("café".as_bytes()), "café");
    }

    #[test]
    fn none_and_text_dfm_pass_through() {
        assert_eq!(convert_bytes(Converter::None, b"x", "f").unwrap(), None);
        assert_eq!(
            convert_bytes(Converter::Dfm, b"object Form1: TForm1\nend\n", "f.dfm").unwrap(),
            None
        );
    }

    /// Each side converts on its own, so documents compare against plain text.
    fn diff_with_text(converter: Converter, bytes: &[u8], plain: &str) -> pcompare_core::text::TextDiffResult {
        let text = convert_bytes(converter, bytes, "doc").unwrap().unwrap();
        pcompare_core::text::diff_text(&text, plain, &Default::default()).unwrap()
    }

    #[test]
    fn different_formats_compare_as_text() {
        let docx = office::tests::docx("<w:p><w:r><w:t>Alpha</w:t></w:r></w:p><w:p><w:r><w:t>Beta</w:t></w:r></w:p>");
        assert!(diff_with_text(Converter::Word, &docx, "Alpha\nBeta\n").identical);
        let res = diff_with_text(Converter::Word, &docx, "Alpha\nGamma\n");
        assert_eq!((res.stats.equal, res.stats.changed), (1, 1));

        let doc = office::tests::doc(&[("Alpha\rBeta\r", true)]);
        assert!(diff_with_text(Converter::Word, &doc, "Alpha\nBeta\n").identical);

        let rtf = br"{\rtf1\ansi Alpha\par Beta\par}";
        assert!(diff_with_text(Converter::Rtf, rtf, "Alpha\nBeta\n").identical);

        let pdf = pdf::tests::pdf(&["Alpha", "Beta"]);
        let text = convert_bytes(Converter::Pdf, &pdf, "a.pdf").unwrap().unwrap();
        let words: Vec<&str> = text.split_whitespace().collect();
        assert_eq!(words, ["Alpha", "Beta"]);
        let opts = pcompare_core::text::TextDiffOptions {
            whitespace: pcompare_core::text::WhitespaceMode::IgnoreAll,
            ..Default::default()
        };
        let res = pcompare_core::text::diff_text(&text, "Alpha\nBeta\n", &opts).unwrap();
        assert_eq!(
            res.stats.changed + res.stats.left_only + res.stats.right_only,
            0,
            "{text:?}"
        );

        let dfm = dfm::tests::sample();
        let text = convert_bytes(Converter::Dfm, &dfm, "f.dfm").unwrap().unwrap();
        assert!(text.starts_with("object Form1: TForm1\n  Left = 300\n"));
    }

    #[test]
    fn errors_name_the_file_and_format() {
        let err = convert_bytes(Converter::Word, b"not a document", "a.docx").unwrap_err();
        assert!(err.to_string().starts_with("a.docx: cannot convert from Word"), "{err}");
    }
}
