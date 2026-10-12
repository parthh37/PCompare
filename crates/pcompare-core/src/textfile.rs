//! Loading and saving text files with encoding, BOM and line-ending handling.

use crate::{CoreError, Result};
use encoding_rs::{Encoding, UTF_16BE, UTF_16LE, UTF_8, WINDOWS_1252};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// How many leading bytes are sniffed when deciding whether a file is binary.
const BINARY_SNIFF_LEN: usize = 8000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum LineEnding {
    Lf,
    Crlf,
    Cr,
    Mixed,
    None,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TextFileInfo {
    pub path: String,
    pub encoding: String,
    pub has_bom: bool,
    pub line_ending: LineEnding,
    pub size: u64,
}

#[derive(Debug, Clone)]
pub struct LoadedText {
    pub text: String,
    pub info: TextFileInfo,
}

/// Heuristic used by git and most diff tools: NUL bytes never occur in text
/// (UTF-16 is handled earlier via its BOM).
pub fn is_probably_binary(bytes: &[u8]) -> bool {
    let sniff = &bytes[..bytes.len().min(BINARY_SNIFF_LEN)];
    if Encoding::for_bom(sniff).is_some() {
        return false;
    }
    sniff.contains(&0)
}

/// Counts `(lf, crlf, cr)` line breaks.
fn count_line_endings(text: &str) -> (usize, usize, usize) {
    let (mut lf, mut crlf, mut cr) = (0usize, 0usize, 0usize);
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\r' if bytes.get(i + 1) == Some(&b'\n') => {
                crlf += 1;
                i += 1;
            }
            b'\r' => cr += 1,
            b'\n' => lf += 1,
            _ => {}
        }
        i += 1;
    }
    (lf, crlf, cr)
}

pub fn detect_line_ending(text: &str) -> LineEnding {
    let (lf, crlf, cr) = count_line_endings(text);
    match (lf > 0, crlf > 0, cr > 0) {
        (false, false, false) => LineEnding::None,
        (true, false, false) => LineEnding::Lf,
        (false, true, false) => LineEnding::Crlf,
        (false, false, true) => LineEnding::Cr,
        _ => LineEnding::Mixed,
    }
}

/// Decodes raw bytes: BOM first, then strict UTF-8, then Windows-1252 as the
/// classic fallback for legacy Windows files.
pub fn decode(bytes: &[u8]) -> (String, &'static Encoding, bool) {
    if let Some((encoding, bom_len)) = Encoding::for_bom(bytes) {
        let (text, _) = encoding.decode_without_bom_handling(&bytes[bom_len..]);
        return (text.into_owned(), encoding, true);
    }
    match std::str::from_utf8(bytes) {
        Ok(text) => (text.to_owned(), UTF_8, false),
        Err(_) => {
            let (text, _) = WINDOWS_1252.decode_without_bom_handling(bytes);
            (text.into_owned(), WINDOWS_1252, false)
        }
    }
}

pub fn load_text(path: impl AsRef<Path>) -> Result<LoadedText> {
    let path = path.as_ref();
    let bytes = std::fs::read(path).map_err(|e| CoreError::io(path, e))?;
    let (text, encoding, has_bom) = decode(&bytes);
    let info = TextFileInfo {
        path: path.display().to_string(),
        encoding: encoding.name().to_owned(),
        has_bom,
        line_ending: detect_line_ending(&text),
        size: bytes.len() as u64,
    };
    Ok(LoadedText { text, info })
}

/// The platform's conventional line ending.
pub const NATIVE_LINE_ENDING: LineEnding = if cfg!(windows) {
    LineEnding::Crlf
} else {
    LineEnding::Lf
};

/// The line ending to use when writing `text` back: its own style when
/// consistent, the most frequent style when mixed (ties prefer CRLF, then LF),
/// and [`NATIVE_LINE_ENDING`] when the text has no line breaks at all.
/// Always one of `Lf`, `Crlf` or `Cr`.
pub fn save_line_ending(text: &str) -> LineEnding {
    let (lf, crlf, cr) = count_line_endings(text);
    if lf + crlf + cr == 0 {
        NATIVE_LINE_ENDING
    } else if crlf >= lf && crlf >= cr {
        LineEnding::Crlf
    } else if lf >= cr {
        LineEnding::Lf
    } else {
        LineEnding::Cr
    }
}

/// Everything needed to write text back in the same on-disk format it was
/// read in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TextFormat {
    /// WHATWG encoding label, e.g. `UTF-8`, `UTF-16LE`, `windows-1252`.
    pub encoding: String,
    pub has_bom: bool,
    /// Line breaks are normalized to this style on save; `Mixed` and `None`
    /// keep the text's line breaks untouched.
    pub line_ending: LineEnding,
}

impl LoadedText {
    /// The format to save this text in; see [`save_line_ending`] for how
    /// mixed or missing line endings are resolved.
    pub fn format(&self) -> TextFormat {
        TextFormat {
            encoding: self.info.encoding.clone(),
            has_bom: self.info.has_bom,
            line_ending: save_line_ending(&self.text),
        }
    }
}

/// Rewrites every line break (`\r\n`, `\r` or `\n`) as `eol`.
fn normalize_line_endings(text: &str, eol: &str) -> String {
    let mut out = String::with_capacity(text.len() + text.len() / 16);
    let mut rest = text;
    while let Some(pos) = rest.find(['\r', '\n']) {
        out.push_str(&rest[..pos]);
        out.push_str(eol);
        let skip = if rest[pos..].starts_with("\r\n") { 2 } else { 1 };
        rest = &rest[pos + skip..];
    }
    out.push_str(rest);
    out
}

/// Encodes `text` in `format`: line breaks normalized, BOM prepended when
/// requested. Fails rather than silently substituting characters the target
/// encoding cannot represent.
pub fn encode_text(text: &str, format: &TextFormat) -> Result<Vec<u8>> {
    let encoding = Encoding::for_label(format.encoding.as_bytes())
        .ok_or_else(|| CoreError::Other(format!("unsupported encoding `{}`", format.encoding)))?;
    let text = match format.line_ending {
        LineEnding::Lf => normalize_line_endings(text, "\n"),
        LineEnding::Crlf => normalize_line_endings(text, "\r\n"),
        LineEnding::Cr => normalize_line_endings(text, "\r"),
        LineEnding::Mixed | LineEnding::None => text.to_owned(),
    };

    // encoding_rs only decodes UTF-16 (its encoder emits UTF-8), so encode it by hand.
    if encoding == UTF_16LE || encoding == UTF_16BE {
        let le = encoding == UTF_16LE;
        let mut out = Vec::with_capacity(text.len() * 2 + 2);
        let mut push = |unit: u16| out.extend_from_slice(&if le { unit.to_le_bytes() } else { unit.to_be_bytes() });
        if format.has_bom {
            push(0xFEFF);
        }
        text.encode_utf16().for_each(&mut push);
        return Ok(out);
    }

    let mut out = Vec::with_capacity(text.len() + 3);
    if encoding == UTF_8 {
        if format.has_bom {
            out.extend_from_slice(&[0xEF, 0xBB, 0xBF]);
        }
        out.extend_from_slice(text.as_bytes());
        return Ok(out);
    }
    let (bytes, used, unmappable) = encoding.encode(&text);
    if unmappable || used != encoding {
        return Err(CoreError::Other(format!(
            "the text contains characters that cannot be saved as {}",
            encoding.name()
        )));
    }
    out.extend_from_slice(&bytes);
    Ok(out)
}

/// Writes `text` to `path` in `format` and returns the info a fresh load of
/// the written file would report.
pub fn save_text(path: impl AsRef<Path>, text: &str, format: &TextFormat) -> Result<TextFileInfo> {
    let path = path.as_ref();
    let bytes = encode_text(text, format)?;
    std::fs::write(path, &bytes).map_err(|e| CoreError::io(path, e))?;
    let (written, encoding, has_bom) = decode(&bytes);
    Ok(TextFileInfo {
        path: path.display().to_string(),
        encoding: encoding.name().to_owned(),
        has_bom,
        line_ending: detect_line_ending(&written),
        size: bytes.len() as u64,
    })
}

/// Reads only the sniff window, so callers can route large binaries to the
/// hex view without loading them fully.
pub fn file_is_binary(path: impl AsRef<Path>) -> Result<bool> {
    use std::io::Read;
    let path = path.as_ref();
    let mut file = std::fs::File::open(path).map_err(|e| CoreError::io(path, e))?;
    let mut buf = vec![0u8; BINARY_SNIFF_LEN];
    let mut filled = 0;
    while filled < buf.len() {
        match file.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) => return Err(CoreError::io(path, e)),
        }
    }
    Ok(is_probably_binary(&buf[..filled]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_line_endings() {
        assert_eq!(detect_line_ending("a\nb\n"), LineEnding::Lf);
        assert_eq!(detect_line_ending("a\r\nb\r\n"), LineEnding::Crlf);
        assert_eq!(detect_line_ending("a\r\nb\n"), LineEnding::Mixed);
        assert_eq!(detect_line_ending("abc"), LineEnding::None);
    }

    #[test]
    fn decodes_utf16_with_bom() {
        let bytes = [0xFF, 0xFE, b'h', 0, b'i', 0];
        let (text, enc, bom) = decode(&bytes);
        assert_eq!(text, "hi");
        assert_eq!(enc.name(), "UTF-16LE");
        assert!(bom);
        assert!(!is_probably_binary(&bytes));
    }

    #[test]
    fn falls_back_to_windows_1252() {
        let (text, enc, _) = decode(&[b'c', b'a', b'f', 0xE9]);
        assert_eq!(text, "café");
        assert_eq!(enc.name(), "windows-1252");
    }

    fn format(encoding: &str, has_bom: bool, line_ending: LineEnding) -> TextFormat {
        TextFormat {
            encoding: encoding.to_owned(),
            has_bom,
            line_ending,
        }
    }

    /// Saves `text`, reloads it and checks text, encoding, BOM and line endings survive.
    fn round_trip(original: &[u8]) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        std::fs::write(&path, original).unwrap();
        let loaded = load_text(&path).unwrap();
        let info = save_text(&path, &loaded.text, &loaded.format()).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), original);
        assert_eq!(info.encoding, loaded.info.encoding);
        assert_eq!(info.has_bom, loaded.info.has_bom);
        assert_eq!(info.line_ending, loaded.info.line_ending);
        assert_eq!(info.size, original.len() as u64);
    }

    #[test]
    fn round_trips_utf8_bom() {
        round_trip(b"\xEF\xBB\xBFcaf\xC3\xA9\nline 2\n");
    }

    #[test]
    fn round_trips_utf16le_bom_crlf() {
        let mut bytes = vec![0xFF, 0xFE];
        bytes.extend("h\u{e9}\r\n\u{1F600}\r\n".encode_utf16().flat_map(u16::to_le_bytes));
        round_trip(&bytes);
    }

    #[test]
    fn round_trips_utf16be_bom() {
        let mut bytes = vec![0xFE, 0xFF];
        bytes.extend("one\ntwo".encode_utf16().flat_map(u16::to_be_bytes));
        round_trip(&bytes);
    }

    #[test]
    fn round_trips_windows_1252() {
        round_trip(b"caf\xE9 \x80 \x81\r\nna\xEFve\r\n");
    }

    #[test]
    fn round_trips_crlf_and_cr() {
        round_trip(b"a\r\nb\r\nc");
        round_trip(b"a\rb\r");
    }

    #[test]
    fn normalizes_edited_text_to_file_style() {
        let bytes = encode_text("a\nb\r\nc\rd\n", &format("UTF-8", false, LineEnding::Crlf)).unwrap();
        assert_eq!(bytes, b"a\r\nb\r\nc\r\nd\r\n");
        let kept = encode_text("a\nb\r\n", &format("UTF-8", false, LineEnding::Mixed)).unwrap();
        assert_eq!(kept, b"a\nb\r\n");
    }

    #[test]
    fn mixed_files_save_with_dominant_ending() {
        assert_eq!(save_line_ending("a\r\nb\r\nc\n"), LineEnding::Crlf);
        assert_eq!(save_line_ending("a\nb\nc\r\n"), LineEnding::Lf);
        assert_eq!(save_line_ending("a\rb\rc\n"), LineEnding::Cr);
        assert_eq!(save_line_ending("a\r\nb\n"), LineEnding::Crlf);
        assert_eq!(save_line_ending("abc"), NATIVE_LINE_ENDING);
    }

    #[test]
    fn rejects_unmappable_characters() {
        let err = encode_text("snowman \u{2603}", &format("windows-1252", false, LineEnding::Lf)).unwrap_err();
        assert!(err.to_string().contains("windows-1252"));
        assert!(encode_text("x", &format("no-such-encoding", false, LineEnding::Lf)).is_err());
    }

    #[test]
    fn nul_bytes_mean_binary() {
        assert!(is_probably_binary(b"PK\x03\x04\x00\x00"));
        assert!(!is_probably_binary(b"plain text"));
    }
}
