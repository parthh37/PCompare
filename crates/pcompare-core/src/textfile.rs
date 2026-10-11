//! Loading text files with encoding, BOM and line-ending detection.

use crate::{CoreError, Result};
use encoding_rs::{Encoding, UTF_8, WINDOWS_1252};
use serde::Serialize;
use std::path::Path;

/// How many leading bytes are sniffed when deciding whether a file is binary.
const BINARY_SNIFF_LEN: usize = 8000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
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

pub fn detect_line_ending(text: &str) -> LineEnding {
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

    #[test]
    fn nul_bytes_mean_binary() {
        assert!(is_probably_binary(b"PK\x03\x04\x00\x00"));
        assert!(!is_probably_binary(b"plain text"));
    }
}
