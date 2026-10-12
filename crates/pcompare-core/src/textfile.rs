//! Loading and saving text files with encoding, BOM and line-ending handling.

use crate::{CoreError, Result};
use encoding_rs::{Encoding, UTF_16BE, UTF_16LE, UTF_8, WINDOWS_1252};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

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
    /// The encoding was guessed statistically rather than known from a BOM
    /// or valid UTF-8.
    pub guessed: bool,
}

/// Heuristic used by git and most diff tools: NUL bytes never occur in text
/// (UTF-16 is handled earlier via its BOM).
pub fn is_probably_binary(bytes: &[u8]) -> bool {
    let sniff = &bytes[..bytes.len().min(BINARY_SNIFF_LEN)];
    if sniff_bom(sniff).is_some() {
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
/// classic fallback for legacy Windows files. Used by engines that only need
/// the text; [`decode_auto`] adds UTF-32 and multi-byte detection.
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

/// A text encoding: anything `encoding_rs` knows plus UTF-32, which the
/// WHATWG set (and so `encoding_rs`) leaves out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    Whatwg(&'static Encoding),
    Utf32 { le: bool },
}

const UTF32LE_BOM: [u8; 4] = [0xFF, 0xFE, 0x00, 0x00];
const UTF32BE_BOM: [u8; 4] = [0x00, 0x00, 0xFE, 0xFF];

impl Codec {
    /// Resolves an encoding label (`UTF-8`, `utf-16le`, `UTF-32BE`,
    /// `shift_jis`, `latin1`, ...).
    pub fn for_label(label: &str) -> Option<Codec> {
        match label.trim().to_ascii_lowercase().as_str() {
            "utf-32" | "utf-32le" | "utf32" | "utf32le" => Some(Codec::Utf32 { le: true }),
            "utf-32be" | "utf32be" => Some(Codec::Utf32 { le: false }),
            l => Encoding::for_label(l.as_bytes()).map(Codec::Whatwg),
        }
    }

    /// The canonical name, as reported in [`TextFileInfo::encoding`].
    pub fn name(self) -> &'static str {
        match self {
            Codec::Whatwg(e) => e.name(),
            Codec::Utf32 { le: true } => "UTF-32LE",
            Codec::Utf32 { le: false } => "UTF-32BE",
        }
    }

    /// Wide encodings are full of NUL bytes, so the binary sniff must not
    /// run on them.
    pub fn is_wide(self) -> bool {
        matches!(self, Codec::Utf32 { .. }) || self == Codec::Whatwg(UTF_16LE) || self == Codec::Whatwg(UTF_16BE)
    }

    fn bom(self) -> &'static [u8] {
        match self {
            Codec::Utf32 { le: true } => &UTF32LE_BOM,
            Codec::Utf32 { le: false } => &UTF32BE_BOM,
            Codec::Whatwg(e) if e == UTF_8 => &[0xEF, 0xBB, 0xBF],
            Codec::Whatwg(e) if e == UTF_16LE => &[0xFF, 0xFE],
            Codec::Whatwg(e) if e == UTF_16BE => &[0xFE, 0xFF],
            Codec::Whatwg(_) => &[],
        }
    }

    /// Decodes `bytes` (BOM already removed) with replacement characters for
    /// malformed sequences.
    fn decode(self, bytes: &[u8]) -> String {
        match self {
            Codec::Whatwg(e) => e.decode_without_bom_handling(bytes).0.into_owned(),
            Codec::Utf32 { le } => {
                let mut out = String::with_capacity(bytes.len() / 4);
                let (units, rest) = bytes.as_chunks::<4>();
                for c in units {
                    let unit = if le {
                        u32::from_le_bytes(*c)
                    } else {
                        u32::from_be_bytes(*c)
                    };
                    out.push(char::from_u32(unit).unwrap_or(char::REPLACEMENT_CHARACTER));
                }
                if !rest.is_empty() {
                    out.push(char::REPLACEMENT_CHARACTER);
                }
                out
            }
        }
    }
}

/// Recognizes a byte-order mark, including UTF-32 ones (checked first, as
/// the UTF-32LE BOM starts with the UTF-16LE one).
fn sniff_bom(bytes: &[u8]) -> Option<(Codec, usize)> {
    if bytes.starts_with(&UTF32LE_BOM) {
        return Some((Codec::Utf32 { le: true }, 4));
    }
    if bytes.starts_with(&UTF32BE_BOM) {
        return Some((Codec::Utf32 { le: false }, 4));
    }
    Encoding::for_bom(bytes).map(|(e, len)| (Codec::Whatwg(e), len))
}

/// The result of decoding a file's bytes.
#[derive(Debug, Clone)]
pub struct Decoded {
    pub text: String,
    pub codec: Codec,
    pub has_bom: bool,
    /// The encoding was guessed statistically (no BOM, not valid UTF-8).
    pub guessed: bool,
}

/// Decodes with automatic detection: BOM (UTF-8/16/32), then strict UTF-8,
/// then a statistical guess among legacy single- and multi-byte encodings
/// (Windows-125x, Shift_JIS, EUC-JP, GBK, Big5, EUC-KR, KOI8, ...).
pub fn decode_auto(bytes: &[u8]) -> Decoded {
    if let Some((codec, bom_len)) = sniff_bom(bytes) {
        return Decoded {
            text: codec.decode(&bytes[bom_len..]),
            codec,
            has_bom: true,
            guessed: false,
        };
    }
    if let Ok(text) = std::str::from_utf8(bytes) {
        return Decoded {
            text: text.to_owned(),
            codec: Codec::Whatwg(UTF_8),
            has_bom: false,
            guessed: false,
        };
    }
    let mut detector = chardetng::EncodingDetector::new();
    detector.feed(bytes, true);
    let codec = Codec::Whatwg(detector.guess(None, false));
    Decoded {
        text: codec.decode(bytes),
        codec,
        has_bom: false,
        guessed: true,
    }
}

/// Decodes `bytes` as the encoding chosen by the user (an [`ENCODINGS`] id
/// or any label). A BOM of that encoding is stripped and reported; no
/// detection happens.
pub fn decode_as(bytes: &[u8], choice: &str) -> Result<Decoded> {
    let (codec, _) = parse_encoding_choice(choice)?;
    let bom = codec.bom();
    let has_bom = !bom.is_empty() && bytes.starts_with(bom);
    let body = if has_bom { &bytes[bom.len()..] } else { bytes };
    Ok(Decoded {
        text: codec.decode(body),
        codec,
        has_bom,
        guessed: false,
    })
}

/// An entry of the encoding picker.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EncodingChoice {
    /// What the UI passes back: an encoding name, with ` BOM` appended for
    /// the variants that write a byte-order mark.
    pub id: &'static str,
    /// Human-readable label.
    pub name: &'static str,
    /// Picker section.
    pub group: &'static str,
}

macro_rules! enc {
    ($id:expr, $name:expr, $group:expr) => {
        EncodingChoice {
            id: $id,
            name: $name,
            group: $group,
        }
    };
}

/// Every encoding offered for manual selection.
pub const ENCODINGS: &[EncodingChoice] = &[
    enc!("UTF-8", "UTF-8", "Unicode"),
    enc!("UTF-8 BOM", "UTF-8 with BOM", "Unicode"),
    enc!("UTF-16LE BOM", "UTF-16 LE with BOM", "Unicode"),
    enc!("UTF-16BE BOM", "UTF-16 BE with BOM", "Unicode"),
    enc!("UTF-16LE", "UTF-16 LE", "Unicode"),
    enc!("UTF-16BE", "UTF-16 BE", "Unicode"),
    enc!("UTF-32LE BOM", "UTF-32 LE with BOM", "Unicode"),
    enc!("UTF-32BE BOM", "UTF-32 BE with BOM", "Unicode"),
    enc!("UTF-32LE", "UTF-32 LE", "Unicode"),
    enc!("UTF-32BE", "UTF-32 BE", "Unicode"),
    enc!("windows-1252", "Western (Windows-1252 / Latin-1)", "Western"),
    enc!("ISO-8859-15", "Western (ISO-8859-15)", "Western"),
    enc!("macintosh", "Western (Mac Roman)", "Western"),
    enc!("windows-1250", "Central European (Windows-1250)", "European"),
    enc!("ISO-8859-2", "Central European (ISO-8859-2)", "European"),
    enc!("windows-1257", "Baltic (Windows-1257)", "European"),
    enc!("ISO-8859-4", "Baltic (ISO-8859-4)", "European"),
    enc!("ISO-8859-13", "Baltic (ISO-8859-13)", "European"),
    enc!("ISO-8859-3", "South European (ISO-8859-3)", "European"),
    enc!("ISO-8859-10", "Nordic (ISO-8859-10)", "European"),
    enc!("ISO-8859-16", "South-Eastern European (ISO-8859-16)", "European"),
    enc!("windows-1251", "Cyrillic (Windows-1251)", "Cyrillic"),
    enc!("KOI8-R", "Cyrillic (KOI8-R)", "Cyrillic"),
    enc!("KOI8-U", "Cyrillic (KOI8-U)", "Cyrillic"),
    enc!("ISO-8859-5", "Cyrillic (ISO-8859-5)", "Cyrillic"),
    enc!("IBM866", "Cyrillic (DOS 866)", "Cyrillic"),
    enc!("windows-1253", "Greek (Windows-1253)", "Greek & Turkish"),
    enc!("ISO-8859-7", "Greek (ISO-8859-7)", "Greek & Turkish"),
    enc!("windows-1254", "Turkish (Windows-1254 / ISO-8859-9)", "Greek & Turkish"),
    enc!("windows-1255", "Hebrew (Windows-1255)", "Middle Eastern"),
    enc!("ISO-8859-8", "Hebrew (ISO-8859-8)", "Middle Eastern"),
    enc!("windows-1256", "Arabic (Windows-1256)", "Middle Eastern"),
    enc!("ISO-8859-6", "Arabic (ISO-8859-6)", "Middle Eastern"),
    enc!("windows-874", "Thai (Windows-874)", "Asian"),
    enc!("windows-1258", "Vietnamese (Windows-1258)", "Asian"),
    enc!("Shift_JIS", "Japanese (Shift_JIS)", "Asian"),
    enc!("EUC-JP", "Japanese (EUC-JP)", "Asian"),
    enc!("ISO-2022-JP", "Japanese (ISO-2022-JP)", "Asian"),
    enc!("GBK", "Chinese Simplified (GBK)", "Asian"),
    enc!("gb18030", "Chinese Simplified (GB18030)", "Asian"),
    enc!("Big5", "Chinese Traditional (Big5)", "Asian"),
    enc!("EUC-KR", "Korean (EUC-KR)", "Asian"),
];

/// Splits an encoding choice like `UTF-16LE BOM` into its codec and whether
/// a BOM is written.
pub fn parse_encoding_choice(choice: &str) -> Result<(Codec, bool)> {
    let choice = choice.trim();
    let (label, bom) = match choice.strip_suffix(" BOM") {
        Some(label) => (label, true),
        None => (choice, false),
    };
    let codec = Codec::for_label(label).ok_or_else(|| CoreError::Other(format!("unsupported encoding `{choice}`")))?;
    Ok((codec, bom))
}

pub fn load_text(path: impl AsRef<Path>) -> Result<LoadedText> {
    load_text_as(path, None)
}

/// Loads a text file, decoding it as `encoding` (an [`ENCODINGS`] id) when
/// given, or with [`decode_auto`] otherwise.
pub fn load_text_as(path: impl AsRef<Path>, encoding: Option<&str>) -> Result<LoadedText> {
    let path = path.as_ref();
    let bytes = std::fs::read(path).map_err(|e| CoreError::io(path, e))?;
    let decoded = match encoding {
        Some(choice) => decode_as(&bytes, choice)?,
        None => decode_auto(&bytes),
    };
    let info = TextFileInfo {
        path: path.display().to_string(),
        encoding: decoded.codec.name().to_owned(),
        has_bom: decoded.has_bom,
        line_ending: detect_line_ending(&decoded.text),
        size: bytes.len() as u64,
    };
    Ok(LoadedText {
        text: decoded.text,
        info,
        guessed: decoded.guessed,
    })
}

/// Splits text into lines and the terminator ending each one: `\r\n`, `\n`
/// and a lone `\r` all break lines. The last line's terminator is
/// [`LineEnding::None`] unless the text ends with a line break, in which case
/// no empty trailing line is produced (like [`str::lines`]).
pub fn split_lines_with_endings(text: &str) -> (Vec<&str>, Vec<LineEnding>) {
    let (mut lines, mut endings) = (Vec::new(), Vec::new());
    let mut rest = text;
    while let Some(pos) = rest.find(['\r', '\n']) {
        lines.push(&rest[..pos]);
        let (eol, skip) = if rest[pos..].starts_with("\r\n") {
            (LineEnding::Crlf, 2)
        } else if rest.as_bytes()[pos] == b'\r' {
            (LineEnding::Cr, 1)
        } else {
            (LineEnding::Lf, 1)
        };
        endings.push(eol);
        rest = &rest[pos + skip..];
    }
    if !rest.is_empty() {
        lines.push(rest);
        endings.push(LineEnding::None);
    }
    (lines, endings)
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
    let codec = Codec::for_label(&format.encoding)
        .ok_or_else(|| CoreError::Other(format!("unsupported encoding `{}`", format.encoding)))?;
    let text = match format.line_ending {
        LineEnding::Lf => normalize_line_endings(text, "\n"),
        LineEnding::Crlf => normalize_line_endings(text, "\r\n"),
        LineEnding::Cr => normalize_line_endings(text, "\r"),
        LineEnding::Mixed | LineEnding::None => text.to_owned(),
    };
    let mut out = Vec::with_capacity(text.len() + 4);
    if format.has_bom {
        out.extend_from_slice(codec.bom());
    }
    let encoding = match codec {
        Codec::Utf32 { le } => {
            out.reserve(text.len() * 4);
            for c in text.chars() {
                out.extend_from_slice(&if le {
                    (c as u32).to_le_bytes()
                } else {
                    (c as u32).to_be_bytes()
                });
            }
            return Ok(out);
        }
        Codec::Whatwg(e) => e,
    };

    // encoding_rs only decodes UTF-16 (its encoder emits UTF-8), so encode it by hand.
    if encoding == UTF_16LE || encoding == UTF_16BE {
        let le = encoding == UTF_16LE;
        out.reserve(text.len() * 2);
        text.encode_utf16()
            .for_each(|unit| out.extend_from_slice(&if le { unit.to_le_bytes() } else { unit.to_be_bytes() }));
        return Ok(out);
    }
    if encoding == UTF_8 {
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

/// Writes `text` to `path` in `format` and returns the file's new info
/// (encoding and BOM as written, not re-detected: a legacy encoding without
/// a BOM is not reliably detectable on reload).
pub fn save_text(path: impl AsRef<Path>, text: &str, format: &TextFormat) -> Result<TextFileInfo> {
    let path = path.as_ref();
    let bytes = encode_text(text, format)?;
    std::fs::write(path, &bytes).map_err(|e| CoreError::io(path, e))?;
    let codec = Codec::for_label(&format.encoding).unwrap_or(Codec::Whatwg(UTF_8));
    let has_bom = format.has_bom && !codec.bom().is_empty();
    let body = if has_bom {
        &bytes[codec.bom().len()..]
    } else {
        &bytes[..]
    };
    Ok(TextFileInfo {
        path: path.display().to_string(),
        encoding: codec.name().to_owned(),
        has_bom,
        line_ending: detect_line_ending(&codec.decode(body)),
        size: bytes.len() as u64,
    })
}

/// Where automatic backups go when a text file is saved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum BackupMode {
    /// No backups.
    Off,
    /// `<name>.bak` next to the file.
    #[default]
    Bak,
    /// `<name>.<UTC timestamp>.bak` in [`BackupOptions::folder`].
    Folder,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct BackupOptions {
    pub mode: BackupMode,
    /// Backup folder for [`BackupMode::Folder`]; created when missing.
    pub folder: String,
    /// Back up on every save instead of only the first save of a file in
    /// this app session (which preserves the version from before editing).
    pub every_save: bool,
}

/// `YYYYMMDD-HHMMSS` in UTC.
fn utc_stamp(time: std::time::SystemTime) -> String {
    let secs = time
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (days, rem) = (secs / 86_400, secs % 86_400);
    // Civil-from-days (Howard Hinnant's algorithm), valid for any date after 1970.
    let z = days as i64 + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}{month:02}{day:02}-{:02}{:02}{:02}",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// Where a backup of `path` taken at `time` goes, or `None` when backups are off.
pub fn backup_path(path: &Path, opts: &BackupOptions, time: std::time::SystemTime) -> Result<Option<PathBuf>> {
    let name = path
        .file_name()
        .ok_or_else(|| CoreError::Other(format!("{}: not a file path", path.display())))?
        .to_string_lossy();
    Ok(match opts.mode {
        BackupMode::Off => None,
        BackupMode::Bak => Some(path.with_file_name(format!("{name}.bak"))),
        BackupMode::Folder => {
            let folder = opts.folder.trim();
            if folder.is_empty() {
                return Err(CoreError::Other("no backup folder is set".into()));
            }
            Some(Path::new(folder).join(format!("{name}.{}.bak", utc_stamp(time))))
        }
    })
}

/// Copies the current contents of `path` to its backup location (see
/// [`backup_path`]) and returns where it went. Nothing happens when backups
/// are off or the file does not exist yet.
pub fn backup_file(path: &Path, opts: &BackupOptions) -> Result<Option<PathBuf>> {
    if !path.is_file() {
        return Ok(None);
    }
    let Some(target) = backup_path(path, opts, std::time::SystemTime::now())? else {
        return Ok(None);
    };
    if let Some(dir) = target.parent() {
        std::fs::create_dir_all(dir).map_err(|e| CoreError::io(dir, e))?;
    }
    std::fs::copy(path, &target).map_err(|e| CoreError::io(&target, e))?;
    Ok(Some(target))
}

/// [`save_text`] preceded by a backup of the existing file when `backup` is
/// set. The text is encoded first, so a save that would fail leaves no
/// stray backup behind.
pub fn save_text_with_backup(
    path: impl AsRef<Path>,
    text: &str,
    format: &TextFormat,
    backup: Option<&BackupOptions>,
) -> Result<(TextFileInfo, Option<PathBuf>)> {
    let path = path.as_ref();
    encode_text(text, format)?;
    let backed_up = match backup {
        Some(opts) => backup_file(path, opts)?,
        None => None,
    };
    Ok((save_text(path, text, format)?, backed_up))
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

    #[test]
    fn splits_lines_with_their_terminators() {
        let (lines, eols) = split_lines_with_endings("a\r\nb\nc\rd");
        assert_eq!(lines, vec!["a", "b", "c", "d"]);
        use LineEnding::*;
        assert_eq!(eols, vec![Crlf, Lf, Cr, None]);
        let (lines, eols) = split_lines_with_endings("x\n\n");
        assert_eq!(lines, vec!["x", ""]);
        assert_eq!(eols, vec![Lf, Lf]);
        assert_eq!(split_lines_with_endings("").0.len(), 0);
    }

    #[test]
    fn every_listed_encoding_resolves() {
        for choice in ENCODINGS {
            let (codec, bom) = parse_encoding_choice(choice.id).unwrap();
            assert_eq!(bom, choice.id.ends_with(" BOM"), "{}", choice.id);
            assert_eq!(codec.name(), choice.id.trim_end_matches(" BOM"), "{}", choice.id);
        }
        assert!(parse_encoding_choice("klingon").is_err());
    }

    #[test]
    fn utf32_round_trips_and_is_detected_by_bom() {
        for (choice, name) in [("UTF-32LE BOM", "UTF-32LE"), ("UTF-32BE BOM", "UTF-32BE")] {
            let (codec, bom) = parse_encoding_choice(choice).unwrap();
            let fmt = format(codec.name(), bom, LineEnding::Lf);
            let bytes = encode_text("h\u{e9}\n\u{1F600}\n", &fmt).unwrap();
            assert!(!is_probably_binary(&bytes));
            let d = decode_auto(&bytes);
            assert_eq!(d.text, "h\u{e9}\n\u{1F600}\n");
            assert_eq!(d.codec.name(), name);
            assert!(d.has_bom && !d.guessed);
        }
        // Without a BOM it must be chosen manually.
        let bytes = encode_text("ok", &format("UTF-32LE", false, LineEnding::None)).unwrap();
        assert_eq!(bytes, b"o\0\0\0k\0\0\0");
        assert_eq!(decode_as(&bytes, "UTF-32LE").unwrap().text, "ok");
    }

    #[test]
    fn legacy_encodings_round_trip() {
        let samples = [
            ("Shift_JIS", "日本語のテキスト"),
            ("EUC-JP", "日本語のテキスト"),
            ("GBK", "简体中文文本"),
            ("gb18030", "简体中文文本"),
            ("Big5", "繁體中文文本"),
            ("EUC-KR", "한국어 텍스트"),
            ("KOI8-R", "Привет, мир"),
            ("windows-1251", "Привет, мир"),
            ("ISO-8859-7", "Καλημέρα"),
            ("windows-1250", "Příliš žluťoučký kůň"),
        ];
        for (enc, text) in samples {
            let bytes = encode_text(text, &format(enc, false, LineEnding::Lf)).unwrap();
            let d = decode_as(&bytes, enc).unwrap();
            assert_eq!(d.text, text, "{enc}");
            assert_eq!(d.codec.name(), enc);
        }
    }

    #[test]
    fn detects_multibyte_legacy_encodings() {
        let text = "日本語のテキストファイルです。文字コードを自動的に判定します。これはテストです。\n";
        let (sjis, _, _) = encoding_rs::SHIFT_JIS.encode(text);
        let d = decode_auto(&sjis);
        assert!(d.guessed);
        assert_eq!(d.codec.name(), "Shift_JIS");
        assert_eq!(d.text, text);

        let text = "这是一个简体中文的文本文件，用于测试字符编码的自动检测功能。\n";
        let (gbk, _, _) = encoding_rs::GBK.encode(text);
        let d = decode_auto(&gbk);
        assert!(d.guessed);
        assert_eq!(d.text, text);

        // Valid UTF-8 is never second-guessed.
        assert!(!decode_auto("plain ascii".as_bytes()).guessed);
    }

    #[test]
    fn load_with_manual_encoding_and_save_keeps_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("k.txt");
        let (koi, _, _) = encoding_rs::KOI8_R.encode("Привет\r\n");
        std::fs::write(&path, &koi).unwrap();
        let loaded = load_text_as(&path, Some("KOI8-R")).unwrap();
        assert_eq!(loaded.text, "Привет\r\n");
        assert_eq!(loaded.info.encoding, "KOI8-R");
        let info = save_text(&path, "Пока\n", &loaded.format()).unwrap();
        assert_eq!(info.encoding, "KOI8-R");
        assert_eq!(info.line_ending, LineEnding::Crlf);
        assert_eq!(
            std::fs::read(&path).unwrap(),
            encoding_rs::KOI8_R.encode("Пока\r\n").0.as_ref()
        );
    }

    #[test]
    fn utc_stamps() {
        let t = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000_000);
        assert_eq!(utc_stamp(t), "20010909-014640");
        assert_eq!(utc_stamp(std::time::UNIX_EPOCH), "19700101-000000");
        let leap = std::time::UNIX_EPOCH + std::time::Duration::from_secs(951_782_400); // 2000-02-29
        assert_eq!(utc_stamp(leap), "20000229-000000");
    }

    #[test]
    fn backups_next_to_file_and_in_folder() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "old\n").unwrap();
        let fmt = format("UTF-8", false, LineEnding::Lf);

        let bak = BackupOptions::default();
        let (_, made) = save_text_with_backup(&path, "new\n", &fmt, Some(&bak)).unwrap();
        assert_eq!(made, Some(dir.path().join("a.txt.bak")));
        assert_eq!(std::fs::read_to_string(dir.path().join("a.txt.bak")).unwrap(), "old\n");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new\n");

        let folder = BackupOptions {
            mode: BackupMode::Folder,
            folder: dir.path().join("backups").display().to_string(),
            every_save: true,
        };
        let (_, made) = save_text_with_backup(&path, "newer\n", &fmt, Some(&folder)).unwrap();
        let made = made.unwrap();
        assert!(made.starts_with(dir.path().join("backups")));
        let name = made.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.starts_with("a.txt.") && name.ends_with(".bak"), "{name}");
        assert_eq!(std::fs::read_to_string(&made).unwrap(), "new\n");

        let off = BackupOptions {
            mode: BackupMode::Off,
            ..Default::default()
        };
        assert_eq!(save_text_with_backup(&path, "x\n", &fmt, Some(&off)).unwrap().1, None);
        // A new file has nothing to back up.
        let fresh = dir.path().join("new.txt");
        assert_eq!(save_text_with_backup(&fresh, "x\n", &fmt, Some(&bak)).unwrap().1, None);
        // An unsaveable text leaves no backup behind.
        let latin = format("windows-1252", false, LineEnding::Lf);
        std::fs::remove_file(dir.path().join("a.txt.bak")).unwrap();
        assert!(save_text_with_backup(&path, "\u{2603}", &latin, Some(&bak)).is_err());
        assert!(!dir.path().join("a.txt.bak").exists());
        // Folder mode needs a folder.
        let no_folder = BackupOptions {
            mode: BackupMode::Folder,
            ..Default::default()
        };
        assert!(backup_file(&path, &no_folder).is_err());
    }
}
