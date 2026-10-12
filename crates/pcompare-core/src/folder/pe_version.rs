//! Version resources of Windows executables, for the folder compare's
//! "exe/dll version" content method: two builds with the same version info
//! count as the same even though their bytes differ (timestamps, signing).
//!
//! A minimal reader on pelite (pure Rust); the full property view lives in
//! `pcompare-meta`.

use super::read_full;
use crate::vfs::Vfs;
use crate::{CoreError, Result};
use pelite::image::VS_FIXEDFILEINFO;
use pelite::resources::version_info::Visit;
use pelite::PeFile;

/// Executables larger than this aren't loaded; they fall back to binary.
pub const VERSION_MAX_BYTES: u64 = 256 * 1024 * 1024;

/// Everything in a version resource that identifies a build.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct VersionInfo {
    /// File and product version, flags, OS, type, subtype and date of the
    /// fixed file info.
    pub fixed: Option<[u64; 7]>,
    /// `(language, key, value)` of every string-table entry, in file order.
    pub strings: Vec<(String, String, String)>,
    pub translations: Vec<u16>,
}

fn version(v: &pelite::image::VS_VERSION) -> u64 {
    (u64::from(v.Major) << 48)
        | (u64::from(v.Minor) << 32)
        | (u64::from(v.Patch) << 16)
        | u64::from(v.Build)
}

#[derive(Default)]
struct Collector {
    info: VersionInfo,
    lang: String,
}

impl<'a> Visit<'a> for Collector {
    fn version_info(&mut self, _key: &'a [u16], fixed: Option<&'a VS_FIXEDFILEINFO>) -> bool {
        self.info.fixed = fixed.map(|f| {
            [
                version(&f.dwFileVersion),
                version(&f.dwProductVersion),
                u64::from(f.dwFileFlags & f.dwFileFlagsMask),
                u64::from(f.dwFileOS),
                u64::from(f.dwFileType),
                u64::from(f.dwFileSubtype),
                (u64::from(f.dwFileDateMS) << 32) | u64::from(f.dwFileDateLS),
            ]
        });
        true
    }
    fn string_table(&mut self, lang: &'a [u16]) -> bool {
        self.lang = String::from_utf16_lossy(lang).to_lowercase();
        true
    }
    fn string(&mut self, key: &'a [u16], value: &'a [u16]) {
        let value = String::from_utf16_lossy(value);
        self.info.strings.push((
            self.lang.clone(),
            String::from_utf16_lossy(key),
            value.trim_end_matches('\0').to_owned(),
        ));
    }
    fn var(&mut self, key: &'a [u16], value: &'a [u16]) {
        if String::from_utf16_lossy(key) == "Translation" {
            self.info.translations = value.to_vec();
        }
    }
}

/// Parses the version resource of a PE image held in memory.
pub fn parse(image: &[u8]) -> Option<VersionInfo> {
    let pe = PeFile::from_bytes(image).ok()?;
    let info = pe.resources().ok()?.version_info().ok()?;
    let mut c = Collector::default();
    info.visit(&mut c);
    Some(c.info)
}

/// Reads a file's version resource. `None` when it isn't a PE file, has no
/// version resource or is too large to load.
pub fn read(fs: &dyn Vfs, path: &str, size: u64) -> Result<Option<VersionInfo>> {
    if !(64..=VERSION_MAX_BYTES).contains(&size) {
        return Ok(None);
    }
    let mut reader = fs.open(path)?;
    let len = size as usize;
    // pelite reads headers in place, which needs 8-byte alignment.
    let mut buf = vec![0u8; len + 8];
    let off = buf.as_ptr().align_offset(8).min(8);
    let image = &mut buf[off..off + len];
    let io = |e| CoreError::io(path, e);
    // Check the "MZ" signature before reading the rest of a large file.
    if read_full(&mut reader, &mut image[..2]).map_err(io)? < 2 || &image[..2] != b"MZ" {
        return Ok(None);
    }
    let n = 2 + read_full(&mut reader, &mut image[2..]).map_err(io)?;
    Ok(parse(&image[..n]))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn utf16z(s: &str) -> Vec<u8> {
        s.encode_utf16()
            .chain([0])
            .flat_map(u16::to_le_bytes)
            .collect()
    }

    fn pad4(v: &mut Vec<u8>) {
        while !v.len().is_multiple_of(4) {
            v.push(0);
        }
    }

    /// One version-info block: header, key, value and children, with the
    /// length fields filled in.
    fn block(key: &str, value: &[u8], text: bool, children: &[Vec<u8>]) -> Vec<u8> {
        let mut b = vec![0; 6];
        b.extend(utf16z(key));
        pad4(&mut b);
        b.extend_from_slice(value);
        for child in children {
            pad4(&mut b);
            b.extend_from_slice(child);
        }
        let value_len = if text { value.len() / 2 } else { value.len() };
        let len = b.len() as u16;
        b[0..2].copy_from_slice(&len.to_le_bytes());
        b[2..4].copy_from_slice(&(value_len as u16).to_le_bytes());
        b[4..6].copy_from_slice(&u16::from(text).to_le_bytes());
        b
    }

    fn version_resource(file_version: [u16; 4], product_name: &str) -> Vec<u8> {
        let [a, b, c, d] = file_version;
        let ms = (u32::from(a) << 16) | u32::from(b);
        let ls = (u32::from(c) << 16) | u32::from(d);
        let fixed: Vec<u8> = [
            0xFEEF_04BD,
            0x0001_0000,
            ms,
            ls,
            ms,
            ls,
            0x3F,
            0,
            0x4_0004,
            1,
            0,
            0,
            0,
        ]
        .iter()
        .flat_map(|v: &u32| v.to_le_bytes())
        .collect();
        let strings = block(
            "StringFileInfo",
            &[],
            true,
            &[block(
                "040904b0",
                &[],
                true,
                &[
                    block("ProductName", &utf16z(product_name), true, &[]),
                    block(
                        "FileVersion",
                        &utf16z(&format!("{a}.{b}.{c}.{d}")),
                        true,
                        &[],
                    ),
                ],
            )],
        );
        let vars = block(
            "VarFileInfo",
            &[],
            true,
            &[block("Translation", &[0x09, 0x04, 0xB0, 0x04], false, &[])],
        );
        block("VS_VERSION_INFO", &fixed, false, &[strings, vars])
    }

    /// A minimal PE32 DLL whose only section holds a version resource.
    pub(crate) fn pe_with_version(
        file_version: [u16; 4],
        product_name: &str,
        padding: u8,
    ) -> Vec<u8> {
        const SECTION_RVA: u32 = 0x1000;
        let res = version_resource(file_version, product_name);

        // Resource tree: type 16 (RT_VERSION) → id 1 → language 0x409 → data.
        let mut rsrc = Vec::new();
        let dir = |rsrc: &mut Vec<u8>, id: u32, target: u32| {
            rsrc.extend([0u8; 12]);
            rsrc.extend(0u16.to_le_bytes()); // named entries
            rsrc.extend(1u16.to_le_bytes()); // id entries
            rsrc.extend(id.to_le_bytes());
            rsrc.extend(target.to_le_bytes());
        };
        dir(&mut rsrc, 16, 0x8000_0000 | 24);
        dir(&mut rsrc, 1, 0x8000_0000 | 48);
        dir(&mut rsrc, 0x409, 72);
        let data_rva = SECTION_RVA + 88;
        rsrc.extend(data_rva.to_le_bytes());
        rsrc.extend((res.len() as u32).to_le_bytes());
        rsrc.extend([0u8; 8]); // code page, reserved
        rsrc.extend(&res);
        // Bytes outside the version resource: differ them to model a rebuild.
        rsrc.extend([padding; 16]);
        let raw_size = rsrc.len().div_ceil(0x200) * 0x200;
        rsrc.resize(raw_size, 0);

        let mut f = vec![0u8; 0x200];
        f[0..2].copy_from_slice(b"MZ");
        f[0x3C..0x40].copy_from_slice(&0x40u32.to_le_bytes());
        f[0x40..0x44].copy_from_slice(b"PE\0\0");
        // COFF header.
        let coff = 0x44;
        f[coff..coff + 2].copy_from_slice(&0x14Cu16.to_le_bytes()); // i386
        f[coff + 2..coff + 4].copy_from_slice(&1u16.to_le_bytes()); // sections
        f[coff + 16..coff + 18].copy_from_slice(&0xE0u16.to_le_bytes()); // optional header size
        f[coff + 18..coff + 20].copy_from_slice(&0x2102u16.to_le_bytes()); // DLL | 32-bit | executable
                                                                           // Optional header (PE32).
        let opt = coff + 20;
        let put32 =
            |f: &mut Vec<u8>, at: usize, v: u32| f[at..at + 4].copy_from_slice(&v.to_le_bytes());
        f[opt..opt + 2].copy_from_slice(&0x10Bu16.to_le_bytes());
        put32(&mut f, opt + 28, 0x1000_0000); // image base
        put32(&mut f, opt + 32, 0x1000); // section alignment
        put32(&mut f, opt + 36, 0x200); // file alignment
        f[opt + 40..opt + 42].copy_from_slice(&6u16.to_le_bytes()); // OS version
        f[opt + 48..opt + 50].copy_from_slice(&6u16.to_le_bytes()); // subsystem version
        let image_size = SECTION_RVA + (raw_size as u32).div_ceil(0x1000) * 0x1000;
        put32(&mut f, opt + 56, image_size);
        put32(&mut f, opt + 60, 0x200); // headers size
        f[opt + 68..opt + 70].copy_from_slice(&2u16.to_le_bytes()); // GUI subsystem
        put32(&mut f, opt + 72, 0x10_0000); // stack reserve
        put32(&mut f, opt + 76, 0x1000);
        put32(&mut f, opt + 80, 0x10_0000); // heap reserve
        put32(&mut f, opt + 84, 0x1000);
        put32(&mut f, opt + 92, 16); // data directories
        let resource_dir = opt + 96 + 2 * 8;
        put32(&mut f, resource_dir, SECTION_RVA);
        put32(&mut f, resource_dir + 4, rsrc.len() as u32);
        // Section header.
        let sec = opt + 0xE0;
        f[sec..sec + 8].copy_from_slice(b".rsrc\0\0\0");
        put32(&mut f, sec + 8, rsrc.len() as u32);
        put32(&mut f, sec + 12, SECTION_RVA);
        put32(&mut f, sec + 16, raw_size as u32);
        put32(&mut f, sec + 20, 0x200);
        put32(&mut f, sec + 36, 0x4000_0040); // initialized data, readable
        f.extend(rsrc);
        f
    }

    #[test]
    fn reads_the_version_resource_of_a_pe_image() {
        let image = pe_with_version([1, 2, 3, 4], "Widget", 0);
        let info = parse(&image).expect("version resource");
        let fixed = info.fixed.expect("fixed file info");
        assert_eq!(fixed[0], (1 << 48) | (2 << 32) | (3 << 16) | 4);
        assert!(info
            .strings
            .contains(&("040904b0".into(), "ProductName".into(), "Widget".into())));
        assert!(info.strings.contains(&(
            "040904b0".into(),
            "FileVersion".into(),
            "1.2.3.4".into()
        )));
        assert_eq!(info.translations, vec![0x409, 0x4B0]);

        // A rebuild with other bytes but the same version info reads the same.
        assert_eq!(
            parse(&pe_with_version([1, 2, 3, 4], "Widget", 0xAA)),
            Some(info.clone())
        );
        assert_ne!(
            parse(&pe_with_version([1, 2, 3, 5], "Widget", 0)),
            Some(info.clone())
        );
        assert_ne!(
            parse(&pe_with_version([1, 2, 3, 4], "Gadget", 0)),
            Some(info)
        );
    }

    #[test]
    fn non_pe_files_have_no_version() {
        assert_eq!(
            parse(b"MZ but not really a PE file at all, just text padding......"),
            None
        );
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.txt");
        std::fs::write(&p, vec![b'x'; 100]).unwrap();
        let path = p.to_string_lossy();
        assert_eq!(read(&crate::vfs::LocalFs, &path, 100).unwrap(), None);
    }
}
