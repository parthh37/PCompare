//! Version information and header facts of Windows executables (PE files:
//! .exe, .dll, .sys, .ocx, …). Parsing is pure Rust, so it works on any OS.

use crate::property::{Property, PropertyCompareResult};
use crate::{format_bytes, format_unix_utc};
use pcompare_core::{CoreError, Result};
use pelite::image::{self, VS_FIXEDFILEINFO};
use pelite::resources::version_info::Visit;
use pelite::{PeFile, Wrap};
use serde::Serialize;
use std::path::Path;

pub const GROUP_VERSION: &str = "Version";
pub const GROUP_STRINGS: &str = "Strings";
pub const GROUP_IMAGE: &str = "Image";
pub const GROUP_FILE: &str = "File";

/// Headline facts shown above the property table.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VersionSummary {
    pub file_version: Option<String>,
    pub product_version: Option<String>,
    pub description: Option<String>,
    pub architecture: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ExecutableInfo {
    pub summary: VersionSummary,
    pub properties: Vec<Property>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VersionCompareResult {
    #[serde(flatten)]
    pub compare: PropertyCompareResult,
    pub left: VersionSummary,
    pub right: VersionSummary,
}

pub fn compare_executables(left: &Path, right: &Path) -> Result<VersionCompareResult> {
    let l = read_executable(left)?;
    let r = read_executable(right)?;
    Ok(VersionCompareResult {
        compare: PropertyCompareResult::new(&l.properties, &r.properties),
        left: l.summary,
        right: r.summary,
    })
}

fn not_pe(path: &Path, why: impl std::fmt::Display) -> CoreError {
    CoreError::Other(format!(
        "{}: not a Windows executable (PE) file ({why})",
        path.display()
    ))
}

pub fn read_executable(path: &Path) -> Result<ExecutableInfo> {
    let size = std::fs::metadata(path)
        .map_err(|e| CoreError::io(path, e))?
        .len();
    // Smaller than a DOS header; also avoids mapping an empty file.
    if size < 64 {
        return Err(not_pe(path, "file too small"));
    }
    let map = pelite::FileMap::open(path).map_err(|e| CoreError::io(path, e))?;
    let pe = PeFile::from_bytes(&map).map_err(|e| not_pe(path, e))?;

    let mut props = Vec::new();
    let mut summary = VersionSummary::default();
    version_properties(pe, &mut props, &mut summary);
    image_properties(pe, &mut props, &mut summary);
    props.push(Property::new(GROUP_FILE, "Size", format_bytes(size)));
    Ok(ExecutableInfo {
        summary,
        properties: props,
    })
}

// ---------- version resource ----------

/// Collects the version resource in file order (pelite's own `file_info`
/// uses hash maps, which would scramble the string order).
#[derive(Default)]
struct Collector {
    fixed: Option<VS_FIXEDFILEINFO>,
    tables: Vec<(String, Vec<(String, String)>)>,
    translations: Vec<(u16, u16)>,
}

impl<'a> Visit<'a> for Collector {
    fn version_info(&mut self, _key: &'a [u16], fixed: Option<&'a VS_FIXEDFILEINFO>) -> bool {
        self.fixed = fixed.copied();
        true
    }
    fn string_table(&mut self, lang: &'a [u16]) -> bool {
        self.tables
            .push((String::from_utf16_lossy(lang), Vec::new()));
        true
    }
    fn string(&mut self, key: &'a [u16], value: &'a [u16]) {
        if let Some((_, strings)) = self.tables.last_mut() {
            strings.push((
                String::from_utf16_lossy(key),
                String::from_utf16_lossy(value),
            ));
        }
    }
    fn var(&mut self, key: &'a [u16], value: &'a [u16]) {
        if String::from_utf16_lossy(key) == "Translation" {
            self.translations = value.as_chunks::<2>().0.iter().map(|&[lang, codepage]| (lang, codepage)).collect();
        }
    }
}

fn version_properties(pe: PeFile<'_>, props: &mut Vec<Property>, summary: &mut VersionSummary) {
    let info = pe.resources().ok().and_then(|r| r.version_info().ok());
    let Some(info) = info else {
        props.push(Property::new(
            GROUP_VERSION,
            "Version resource",
            "Not present",
        ));
        return;
    };
    let mut c = Collector::default();
    info.visit(&mut c);

    if let Some(f) = &c.fixed {
        let file_version = f.dwFileVersion.to_string();
        let product_version = f.dwProductVersion.to_string();
        props.push(Property::new(GROUP_VERSION, "File version", &file_version));
        props.push(Property::new(
            GROUP_VERSION,
            "Product version",
            &product_version,
        ));
        summary.file_version = Some(file_version);
        summary.product_version = Some(product_version);
        props.push(Property::new(
            GROUP_VERSION,
            "File flags",
            file_flags(f.dwFileFlags & f.dwFileFlagsMask),
        ));
        props.push(Property::new(GROUP_VERSION, "File OS", file_os(f.dwFileOS)));
        props.push(Property::new(
            GROUP_VERSION,
            "File type",
            file_type(f.dwFileType),
        ));
        if let Some(sub) = file_subtype(f.dwFileType, f.dwFileSubtype) {
            props.push(Property::new(GROUP_VERSION, "File subtype", sub));
        }
        let filetime = (u64::from(f.dwFileDateMS) << 32) | u64::from(f.dwFileDateLS);
        if filetime != 0 {
            let unix = (filetime / 10_000_000) as i64 - 11_644_473_600;
            props.push(Property::new(
                GROUP_VERSION,
                "File date",
                format_unix_utc(unix),
            ));
        }
    } else {
        props.push(Property::new(
            GROUP_VERSION,
            "Fixed file info",
            "Not present",
        ));
    }
    if !c.translations.is_empty() {
        let list: Vec<String> = c
            .translations
            .iter()
            .map(|&(l, cp)| language_label(l, cp))
            .collect();
        props.push(Property::new(
            GROUP_VERSION,
            "Translations",
            list.join("; "),
        ));
    }

    // The table matching the first translation is the primary one and is
    // grouped as plain "Strings" so files with different languages still align.
    let primary = c
        .translations
        .first()
        .and_then(|&(l, cp)| {
            c.tables
                .iter()
                .position(|(k, _)| k.eq_ignore_ascii_case(&format!("{l:04x}{cp:04x}")))
        })
        .unwrap_or(0);
    for (i, (key, strings)) in c.tables.iter().enumerate() {
        let label =
            parse_lang_key(key).map_or_else(|| key.clone(), |(l, cp)| language_label(l, cp));
        let group = if i == primary {
            GROUP_STRINGS.to_owned()
        } else {
            format!("{GROUP_STRINGS} ({label})")
        };
        props.push(Property::new(&group, "Language", label));
        for (name, value) in strings {
            props.push(Property::new(&group, name, value.trim_end_matches('\0')));
        }
        if i == primary {
            summary.description = strings
                .iter()
                .find(|(k, v)| k == "FileDescription" && !v.trim().is_empty())
                .map(|(_, v)| v.trim_end_matches('\0').to_owned());
        }
    }
}

fn parse_lang_key(key: &str) -> Option<(u16, u16)> {
    if key.len() != 8 || !key.is_ascii() {
        return None;
    }
    Some((
        u16::from_str_radix(&key[..4], 16).ok()?,
        u16::from_str_radix(&key[4..], 16).ok()?,
    ))
}

fn language_label(lang: u16, codepage: u16) -> String {
    let name = match lang {
        0x0000 => "Language neutral",
        0x007f => "Invariant",
        0x0401 => "Arabic",
        0x0402 => "Bulgarian",
        0x0403 => "Catalan",
        0x0404 => "Chinese (Traditional)",
        0x0405 => "Czech",
        0x0406 => "Danish",
        0x0407 => "German",
        0x0408 => "Greek",
        0x0409 => "English (United States)",
        0x040a => "Spanish (Traditional)",
        0x040b => "Finnish",
        0x040c => "French",
        0x040d => "Hebrew",
        0x040e => "Hungarian",
        0x0410 => "Italian",
        0x0411 => "Japanese",
        0x0412 => "Korean",
        0x0413 => "Dutch",
        0x0414 => "Norwegian (Bokmål)",
        0x0415 => "Polish",
        0x0416 => "Portuguese (Brazil)",
        0x0418 => "Romanian",
        0x0419 => "Russian",
        0x041a => "Croatian",
        0x041b => "Slovak",
        0x041d => "Swedish",
        0x041e => "Thai",
        0x041f => "Turkish",
        0x0421 => "Indonesian",
        0x0422 => "Ukrainian",
        0x0424 => "Slovenian",
        0x042a => "Vietnamese",
        0x0804 => "Chinese (Simplified)",
        0x0807 => "German (Switzerland)",
        0x0809 => "English (United Kingdom)",
        0x080a => "Spanish (Mexico)",
        0x0816 => "Portuguese (Portugal)",
        0x0c0a => "Spanish",
        0x0c0c => "French (Canada)",
        _ => "",
    };
    let cp = match codepage {
        0 => "7-bit ASCII".to_owned(),
        1200 => "Unicode".to_owned(),
        932 => "Shift-JIS".to_owned(),
        936 => "GB2312".to_owned(),
        949 => "Korean".to_owned(),
        950 => "Big5".to_owned(),
        n @ 1250..=1258 => format!("Windows-{n}"),
        n => format!("code page {n}"),
    };
    if name.is_empty() {
        format!("0x{lang:04X}, {cp}")
    } else {
        format!("{name}, {cp}")
    }
}

fn file_flags(flags: u32) -> String {
    const NAMES: &[(u32, &str)] = &[
        (0x01, "Debug"),
        (0x02, "Pre-release"),
        (0x04, "Patched"),
        (0x08, "Private build"),
        (0x10, "Info inferred"),
        (0x20, "Special build"),
    ];
    let set: Vec<&str> = NAMES
        .iter()
        .filter(|(bit, _)| flags & bit != 0)
        .map(|(_, n)| *n)
        .collect();
    if set.is_empty() {
        "None".into()
    } else {
        set.join(", ")
    }
}

fn file_os(os: u32) -> String {
    match os {
        0x0000_0000 => "Unknown".into(),
        0x0000_0001 => "16-bit Windows".into(),
        0x0000_0004 => "32-bit Windows".into(),
        0x0001_0000 => "MS-DOS".into(),
        0x0001_0001 => "16-bit Windows on MS-DOS".into(),
        0x0001_0004 => "32-bit Windows on MS-DOS".into(),
        0x0004_0000 => "Windows NT".into(),
        0x0004_0004 => "Windows NT (Win32)".into(),
        n => format!("0x{n:08X}"),
    }
}

fn file_type(t: u32) -> String {
    match t {
        0 => "Unknown".into(),
        1 => "Application".into(),
        2 => "DLL".into(),
        3 => "Driver".into(),
        4 => "Font".into(),
        5 => "Virtual device".into(),
        7 => "Static library".into(),
        n => format!("0x{n:X}"),
    }
}

fn file_subtype(t: u32, sub: u32) -> Option<String> {
    let name = match (t, sub) {
        (3 | 4, 0) => "Unknown",
        (3, 1) => "Printer",
        (3, 2) => "Keyboard",
        (3, 3) => "Language",
        (3, 4) => "Display",
        (3, 5) => "Mouse",
        (3, 6) => "Network",
        (3, 7) => "System",
        (3, 8) => "Installable",
        (3, 9) => "Sound",
        (3, 10) => "Communications",
        (3, 12) => "Versioned printer",
        (4, 1) => "Raster",
        (4, 2) => "Vector",
        (4, 3) => "TrueType",
        (5, id) => return Some(format!("Device ID 0x{id:X}")),
        (3 | 4, n) => return Some(format!("0x{n:X}")),
        _ => return None,
    };
    Some(name.into())
}

// ---------- PE headers ----------

fn machine(m: u16) -> String {
    match m {
        0x014c => "x86".into(),
        0x8664 => "x64".into(),
        0xaa64 => "ARM64".into(),
        0xa641 => "ARM64EC".into(),
        0xa64e => "ARM64X".into(),
        0x01c0 => "ARM".into(),
        0x01c4 => "ARMv7 (Thumb-2)".into(),
        0x0200 => "Itanium (IA-64)".into(),
        0x0ebc => "EFI byte code".into(),
        0x5032 => "RISC-V 32".into(),
        0x5064 => "RISC-V 64".into(),
        n => format!("0x{n:04X}"),
    }
}

fn subsystem(s: u16) -> String {
    match s {
        0 => "Unknown".into(),
        1 => "Native".into(),
        2 => "Windows GUI".into(),
        3 => "Windows console".into(),
        5 => "OS/2 console".into(),
        7 => "POSIX console".into(),
        8 => "Native Win9x driver".into(),
        9 => "Windows CE".into(),
        10 => "EFI application".into(),
        11 => "EFI boot service driver".into(),
        12 => "EFI runtime driver".into(),
        13 => "EFI ROM".into(),
        14 => "Xbox".into(),
        16 => "Windows boot application".into(),
        n => format!("{n}"),
    }
}

fn yes_no(b: bool) -> &'static str {
    if b {
        "Yes"
    } else {
        "No"
    }
}

/// Header fields that differ only in width between PE32 and PE32+.
struct Optional {
    format: &'static str,
    linker: (u8, u8),
    os: (u16, u16),
    image_version: (u16, u16),
    subsystem_version: (u16, u16),
    subsystem: u16,
    dll_characteristics: u16,
    size_of_image: u32,
    image_base: u64,
    entry_point: u32,
    checksum: u32,
}

fn optional(pe: PeFile<'_>) -> Optional {
    macro_rules! read {
        ($h:expr, $format:expr) => {
            Optional {
                format: $format,
                linker: ($h.LinkerVersion.Major, $h.LinkerVersion.Minor),
                os: (
                    $h.OperatingSystemVersion.Major,
                    $h.OperatingSystemVersion.Minor,
                ),
                image_version: ($h.ImageVersion.Major, $h.ImageVersion.Minor),
                subsystem_version: ($h.SubsystemVersion.Major, $h.SubsystemVersion.Minor),
                subsystem: $h.Subsystem,
                dll_characteristics: $h.DllCharacteristics,
                size_of_image: $h.SizeOfImage,
                image_base: u64::from($h.ImageBase),
                entry_point: $h.AddressOfEntryPoint,
                checksum: $h.CheckSum,
            }
        };
    }
    match pe.optional_header() {
        Wrap::T32(h) => read!(h, "PE32 (32-bit)"),
        Wrap::T64(h) => read!(h, "PE32+ (64-bit)"),
    }
}

fn image_properties(pe: PeFile<'_>, props: &mut Vec<Property>, summary: &mut VersionSummary) {
    let fh = pe.file_header();
    let oh = optional(pe);
    let arch = machine(fh.Machine);
    summary.architecture = Some(arch.clone());

    let mut add = |name: &str, value: String| props.push(Property::new(GROUP_IMAGE, name, value));
    add("Architecture", arch);
    add("Format", oh.format.into());
    let kind = if fh.Characteristics & image::IMAGE_FILE_DLL != 0 {
        "DLL"
    } else if fh.Characteristics & 0x0002 != 0 {
        "Executable"
    } else {
        "Object (not executable)"
    };
    add("Image type", kind.into());
    add("Subsystem", subsystem(oh.subsystem));
    add(
        "Subsystem version",
        format!("{}.{}", oh.subsystem_version.0, oh.subsystem_version.1),
    );
    add("OS version", format!("{}.{}", oh.os.0, oh.os.1));
    add(
        "Image version",
        format!("{}.{}", oh.image_version.0, oh.image_version.1),
    );
    add("Linker version", format!("{}.{}", oh.linker.0, oh.linker.1));
    add(
        "Link timestamp",
        format_unix_utc(i64::from(fh.TimeDateStamp)),
    );
    add("Sections", fh.NumberOfSections.to_string());
    add("Image size", format_bytes(u64::from(oh.size_of_image)));
    add("Image base", format!("0x{:X}", oh.image_base));
    add("Entry point", format!("0x{:08X}", oh.entry_point));
    add("Checksum", format!("0x{:08X}", oh.checksum));

    let dc = oh.dll_characteristics;
    let has = |bit: u16| yes_no(dc & bit != 0).to_owned();
    add(
        "ASLR (dynamic base)",
        has(image::IMAGE_DLLCHARACTERISTICS_DYNAMIC_BASE),
    );
    add(
        "High-entropy ASLR",
        has(image::IMAGE_DLLCHARACTERISTICS_HIGH_ENTROPY_VA),
    );
    add(
        "DEP (NX compatible)",
        has(image::IMAGE_DLLCHARACTERISTICS_NX_COMPAT),
    );
    add(
        "Control Flow Guard",
        has(image::IMAGE_DLLCHARACTERISTICS_GUARD_CF),
    );
    add(
        "Force integrity",
        has(image::IMAGE_DLLCHARACTERISTICS_FORCE_INTEGRITY),
    );
    add(
        "AppContainer",
        has(image::IMAGE_DLLCHARACTERISTICS_APPCONTAINER),
    );
    add("No SEH", has(image::IMAGE_DLLCHARACTERISTICS_NO_SEH));
    add(
        "Terminal Server aware",
        has(image::IMAGE_DLLCHARACTERISTICS_TERMINAL_SERVER_AWARE),
    );

    let dirs = pe.data_directory();
    let present = |i: usize| dirs.get(i).filter(|d| d.VirtualAddress != 0 && d.Size != 0);
    add(
        ".NET assembly",
        yes_no(present(image::IMAGE_DIRECTORY_ENTRY_COM_DESCRIPTOR).is_some()).into(),
    );
    add(
        "Digital signature",
        match present(image::IMAGE_DIRECTORY_ENTRY_SECURITY) {
            Some(d) => format!("Present ({})", format_bytes(u64::from(d.Size))),
            None => "Not present".into(),
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::property::DiffStatus;
    use std::path::PathBuf;

    /// Real PE files that happen to be on disk (cargo registry sources).
    /// Tests depending on them are skipped when none are found.
    fn fixture(rel: &str) -> Option<PathBuf> {
        let home = std::env::var_os("CARGO_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cargo")))?;
        let src = home.join("registry").join("src");
        for index in std::fs::read_dir(src).ok()?.flatten() {
            let Ok(crates) = std::fs::read_dir(index.path()) else {
                continue;
            };
            for krate in crates.flatten() {
                if krate
                    .file_name()
                    .to_string_lossy()
                    .starts_with("webview2-com-sys-")
                {
                    let p = krate.path().join(rel);
                    if p.is_file() {
                        return Some(p);
                    }
                }
            }
        }
        None
    }

    #[test]
    fn rejects_non_pe() {
        let dir = tempfile::tempdir().unwrap();
        let small = dir.path().join("small.exe");
        std::fs::write(&small, b"MZ").unwrap();
        assert!(read_executable(&small)
            .unwrap_err()
            .to_string()
            .contains("not a Windows executable"));

        let text = dir.path().join("text.dll");
        std::fs::write(
            &text,
            "hello world, this is definitely not a portable executable file!!!",
        )
        .unwrap();
        assert!(read_executable(&text)
            .unwrap_err()
            .to_string()
            .contains("not a Windows executable"));

        assert!(read_executable(&dir.path().join("missing.exe")).is_err());
    }

    #[test]
    fn labels() {
        assert_eq!(
            language_label(0x0409, 1200),
            "English (United States), Unicode"
        );
        assert_eq!(language_label(0x0c07, 1252), "0x0C07, Windows-1252");
        assert_eq!(parse_lang_key("040904B0"), Some((0x0409, 0x04b0)));
        assert_eq!(parse_lang_key("bogus"), None);
        assert_eq!(file_flags(0x3), "Debug, Pre-release");
        assert_eq!(file_flags(0), "None");
        assert_eq!(file_subtype(3, 7).as_deref(), Some("System"));
        assert_eq!(file_subtype(1, 0), None);
        assert_eq!(machine(0x8664), "x64");
    }

    #[test]
    fn reads_real_dll_if_available() {
        let (Some(x64), Some(x86)) = (
            fixture("x64/WebView2Loader.dll"),
            fixture("x86/WebView2Loader.dll"),
        ) else {
            eprintln!("skipping: no WebView2Loader.dll fixtures found");
            return;
        };
        let info = read_executable(&x64).unwrap();
        assert_eq!(info.summary.architecture.as_deref(), Some("x64"));
        assert!(
            info.summary.file_version.is_some(),
            "WebView2Loader has a version resource"
        );
        let get = |n: &str| {
            info.properties
                .iter()
                .find(|p| p.name == n)
                .map(|p| p.value.clone())
        };
        assert_eq!(get("Format").as_deref(), Some("PE32+ (64-bit)"));
        assert_eq!(get("Image type").as_deref(), Some("DLL"));
        assert!(get("CompanyName").is_some());

        let cmp = compare_executables(&x64, &x86).unwrap();
        let arch = cmp
            .compare
            .rows
            .iter()
            .find(|r| r.name == "Architecture")
            .unwrap();
        assert_eq!(arch.status, DiffStatus::Different);
        assert_eq!(arch.right.as_deref(), Some("x86"));
        let same = compare_executables(&x64, &x64).unwrap();
        assert_eq!(
            same.compare.stats.different
                + same.compare.stats.left_only
                + same.compare.stats.right_only,
            0
        );
    }
}
