//! Word (.docx, .doc) and OpenDocument (.odt) text extraction.
//!
//! .docx and .odt are zip packages; their body XML is walked for text runs,
//! with paragraphs as lines and tabs / breaks kept. Word 97-2003 .doc files
//! are OLE compound files; their text is read through the piece table.

use quick_xml::events::Event;
use quick_xml::Reader;
use std::io::{Cursor, Read};

/// OLE compound file signature (.doc, .xls, .msg...).
const OLE_MAGIC: &[u8] = b"\xD0\xCF\x11\xE0\xA1\xB1\x1A\xE1";

/// Word text from a .docx package or a Word 97-2003 .doc file.
pub fn word_to_text(bytes: &[u8]) -> Result<String, String> {
    if bytes.starts_with(OLE_MAGIC) {
        return doc_to_text(bytes);
    }
    let xml = zip_part(bytes, "word/document.xml")?;
    xml_text(&xml, &DOCX)
}

/// OpenDocument text from an .odt package.
pub fn odt_to_text(bytes: &[u8]) -> Result<String, String> {
    let xml = zip_part(bytes, "content.xml")?;
    xml_text(&xml, &ODT)
}

/// Reads one member of a zip package as UTF-8.
fn zip_part(bytes: &[u8], name: &str) -> Result<String, String> {
    let mut zip = zip::ZipArchive::new(Cursor::new(bytes)).map_err(|e| format!("not a valid package ({e})"))?;
    let mut part = zip.by_name(name).map_err(|_| format!("the package has no {name}"))?;
    let mut xml = String::new();
    part.read_to_string(&mut xml).map_err(|e| format!("{name}: {e}"))?;
    Ok(xml)
}

/// Which elements of a document XML dialect carry text and structure
/// (matched on local names, so prefixes don't matter).
struct Dialect {
    /// Elements whose text content is document text.
    text: &'static [&'static str],
    /// Elements that end a line when they close.
    block: &'static [&'static str],
    /// Elements (usually empty) that stand for a tab.
    tab: &'static [&'static str],
    /// Elements (usually empty) that stand for a line break.
    line_break: &'static [&'static str],
    /// Elements whose whole subtree is skipped.
    skip: &'static [&'static str],
    /// An element that stands for N spaces (`text:s text:c="N"`).
    spaces: Option<&'static str>,
}

const DOCX: Dialect = Dialect {
    text: &["t"],
    block: &["p", "tr"],
    tab: &["tab", "ptab"],
    line_break: &["br", "cr"],
    // Deleted revisions, field codes, and alternate (fallback) content.
    skip: &["del", "delText", "instrText", "Fallback"],
    spaces: None,
};

const ODT: Dialect = Dialect {
    // In ODF, text sits directly inside paragraphs and spans.
    text: &["p", "h", "span", "a"],
    block: &["p", "h"],
    tab: &["tab"],
    line_break: &["line-break"],
    skip: &["annotation", "tracked-changes", "note-citation"],
    spaces: Some("s"),
};

fn xml_text(xml: &str, d: &Dialect) -> Result<String, String> {
    let mut reader = Reader::from_str(xml);
    let mut out = String::new();
    // Depth inside text-bearing elements, and inside a skipped subtree.
    let mut in_text = 0usize;
    let mut skip = 0usize;
    loop {
        let event = reader.read_event().map_err(|e| format!("bad XML: {e}"))?;
        match event {
            Event::Start(e) => {
                let name = e.local_name();
                let name = std::str::from_utf8(name.as_ref()).unwrap_or("");
                if skip > 0 || d.skip.contains(&name) {
                    skip += 1;
                } else if d.text.contains(&name) {
                    in_text += 1;
                }
            }
            Event::End(e) => {
                let name = e.local_name();
                let name = std::str::from_utf8(name.as_ref()).unwrap_or("");
                if skip > 0 {
                    skip -= 1;
                    continue;
                }
                if d.text.contains(&name) {
                    in_text = in_text.saturating_sub(1);
                }
                if d.block.contains(&name) {
                    out.push('\n');
                }
            }
            Event::Empty(e) if skip == 0 => {
                let name = e.local_name();
                let name = std::str::from_utf8(name.as_ref()).unwrap_or("");
                if d.tab.contains(&name) {
                    out.push('\t');
                } else if d.line_break.contains(&name) || d.block.contains(&name) {
                    out.push('\n');
                } else if d.spaces == Some(name) {
                    let n = e
                        .attributes()
                        .flatten()
                        .find(|a| a.key.local_name().as_ref() == b"c")
                        .and_then(|a| std::str::from_utf8(&a.value).ok()?.parse::<usize>().ok())
                        .unwrap_or(1);
                    out.extend(std::iter::repeat_n(' ', n.min(1000)));
                }
            }
            Event::Text(t) if skip == 0 && in_text > 0 => {
                out.push_str(&t.decode().map_err(|e| e.to_string())?);
            }
            Event::CData(t) if skip == 0 && in_text > 0 => {
                out.push_str(&t.decode().map_err(|e| e.to_string())?);
            }
            Event::GeneralRef(r) if skip == 0 && in_text > 0 => match r.resolve_char_ref() {
                Ok(Some(c)) => out.push(c),
                _ => {
                    let name = r.decode().map_err(|e| e.to_string())?;
                    out.push_str(quick_xml::escape::resolve_predefined_entity(&name).unwrap_or(""));
                }
            },
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(out)
}

// --- Word 97-2003 ([MS-DOC]) ---

fn u16_at(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(at..at + 2)?.try_into().ok()?))
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

/// The main document text of a .doc file, via the FIB and the piece table (Clx).
fn doc_to_text(bytes: &[u8]) -> Result<String, String> {
    let mut cfb = cfb::CompoundFile::open(Cursor::new(bytes)).map_err(|e| format!("not a valid OLE file ({e})"))?;
    let read = |cfb: &mut cfb::CompoundFile<Cursor<&[u8]>>, name: &str| -> Result<Vec<u8>, String> {
        let mut s = cfb
            .open_stream(name)
            .map_err(|_| format!("no {name} stream (not a Word document)"))?;
        let mut v = Vec::new();
        s.read_to_end(&mut v).map_err(|e| e.to_string())?;
        Ok(v)
    };
    let word = read(&mut cfb, "/WordDocument")?;
    let bad = || "damaged Word header".to_string();
    if u16_at(&word, 0) != Some(0xA5EC) {
        return Err("not a Word 97-2003 document".into());
    }
    let flags = u16_at(&word, 0x0A).ok_or_else(bad)?;
    if flags & 0x0100 != 0 {
        return Err("the document is password-protected".into());
    }
    let table = read(&mut cfb, if flags & 0x0200 != 0 { "/1Table" } else { "/0Table" })?;

    // FibBase (32 bytes), then counted arrays: fibRgW, fibRgLw, fibRgFcLcb.
    let csw = u16_at(&word, 32).ok_or_else(bad)? as usize;
    let lw = 34 + csw * 2;
    let cslw = u16_at(&word, lw).ok_or_else(bad)? as usize;
    let ccp_text = u32_at(&word, lw + 2 + 12).ok_or_else(bad)? as usize;
    let fc_lcb = lw + 2 + cslw * 4 + 2;
    // fcClx / lcbClx are the 34th pair of FibRgFcLcb97.
    let fc_clx = u32_at(&word, fc_lcb + 33 * 8).ok_or_else(bad)? as usize;
    let lcb_clx = u32_at(&word, fc_lcb + 33 * 8 + 4).ok_or_else(bad)? as usize;
    let clx = table.get(fc_clx..fc_clx + lcb_clx).ok_or_else(bad)?;

    // Skip any Prc (formatting) entries to reach the Pcdt.
    let mut at = 0;
    while clx.get(at) == Some(&0x01) {
        let cb = u16_at(clx, at + 1).ok_or_else(bad)? as usize;
        at += 3 + cb;
    }
    if clx.get(at) != Some(&0x02) {
        return Err(bad());
    }
    let lcb = u32_at(clx, at + 1).ok_or_else(bad)? as usize;
    let plc = clx.get(at + 5..at + 5 + lcb).ok_or_else(bad)?;
    // n+1 character positions, then n 8-byte piece descriptors.
    let n = lcb.checked_sub(4).ok_or_else(bad)? / 12;
    let mut text = String::new();
    for i in 0..n {
        let cp0 = u32_at(plc, i * 4).ok_or_else(bad)? as usize;
        let cp1 = u32_at(plc, (i + 1) * 4).ok_or_else(bad)? as usize;
        if cp0 >= ccp_text {
            break;
        }
        let count = cp1.min(ccp_text).saturating_sub(cp0);
        let fc = u32_at(plc, (n + 1) * 4 + i * 8 + 2).ok_or_else(bad)?;
        if fc & 0x4000_0000 != 0 {
            // "Compressed": one Windows-1252 byte per character.
            let start = ((fc & !0x4000_0000) / 2) as usize;
            let raw = word.get(start..start + count).ok_or_else(bad)?;
            text.push_str(&encoding_rs::WINDOWS_1252.decode_without_bom_handling(raw).0);
        } else {
            let start = fc as usize;
            let raw = word.get(start..start + count * 2).ok_or_else(bad)?;
            let units: Vec<u16> = raw.as_chunks::<2>().0.iter().map(|&c| u16::from_le_bytes(c)).collect();
            text.push_str(&String::from_utf16_lossy(&units));
        }
    }
    Ok(clean_doc_text(&text))
}

/// Maps Word's control characters to plain text: paragraph and cell marks
/// become breaks / tabs, field codes are dropped (their results kept), and
/// object anchors vanish.
fn clean_doc_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    // Nesting of fields still in their code part (between 0x13 and 0x14).
    let mut code: Vec<bool> = Vec::new();
    for c in text.chars() {
        match c {
            '\u{13}' => code.push(true),
            '\u{14}' => {
                if let Some(top) = code.last_mut() {
                    *top = false;
                }
            }
            '\u{15}' => {
                code.pop();
            }
            _ if code.iter().any(|&in_code| in_code) => {}
            '\r' | '\u{0B}' | '\u{0C}' => out.push('\n'),
            '\u{07}' => out.push('\t'),
            '\u{1E}' => out.push('-'),
            '\u{1F}' | '\u{01}' | '\u{08}' | '\u{05}' => {}
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::Write;

    /// A zip package with the given members.
    pub fn package(parts: &[(&str, &str)]) -> Vec<u8> {
        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (name, body) in parts {
            let opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
            zip.start_file(*name, opts).unwrap();
            zip.write_all(body.as_bytes()).unwrap();
        }
        zip.finish().unwrap().into_inner()
    }

    pub fn docx(body: &str) -> Vec<u8> {
        package(&[
            ("[Content_Types].xml", "<Types/>"),
            (
                "word/document.xml",
                &format!(
                    r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body>{body}</w:body></w:document>"#
                ),
            ),
        ])
    }

    #[test]
    fn docx_paragraphs_tabs_breaks_and_entities() {
        let bytes = docx(
            r#"<w:p><w:r><w:t>Hello</w:t></w:r><w:r><w:t xml:space="preserve"> world &amp; co</w:t></w:r></w:p>
<w:p><w:r><w:t>a</w:t><w:tab/><w:t>b</w:t><w:br/><w:t>c&#233;</w:t></w:r></w:p>
<w:p><w:del><w:r><w:delText>gone</w:delText></w:r></w:del><w:r><w:instrText>PAGE</w:instrText><w:t>kept</w:t></w:r></w:p>
<w:tbl><w:tr><w:tc><w:p><w:r><w:t>cell</w:t></w:r></w:p></w:tc></w:tr></w:tbl>"#,
        );
        assert_eq!(
            word_to_text(&bytes).unwrap(),
            "Hello world & co\na\tb\ncé\nkept\ncell\n\n"
        );
    }

    #[test]
    fn odt_spaces_tabs_and_headings() {
        let bytes = package(&[(
            "content.xml",
            r#"<office:document-content xmlns:office="o" xmlns:text="t"><office:body><office:text>
<text:h>Title</text:h><text:p>a<text:s text:c="3"/>b<text:tab/>c<text:span>d</text:span></text:p>
<text:p>x<office:annotation><text:p>note</text:p></office:annotation><text:line-break/>y</text:p>
</office:text></office:body></office:document-content>"#,
        )]);
        assert_eq!(odt_to_text(&bytes).unwrap(), "Title\na   b\tcd\nx\ny\n");
    }

    #[test]
    fn missing_part_and_garbage_fail() {
        assert!(word_to_text(&package(&[("x", "y")]))
            .unwrap_err()
            .contains("word/document.xml"));
        assert!(odt_to_text(b"garbage").is_err());
    }

    /// A minimal Word 97 file: FIB + one compressed and one UTF-16 piece.
    pub fn doc(pieces: &[(&str, bool)]) -> Vec<u8> {
        let mut word = vec![0u8; 0x200];
        word[0..2].copy_from_slice(&0xA5ECu16.to_le_bytes());
        word[0x0A..0x0C].copy_from_slice(&0x0200u16.to_le_bytes()); // 1Table
        word[32..34].copy_from_slice(&14u16.to_le_bytes()); // csw
        let lw = 34 + 28;
        word[lw..lw + 2].copy_from_slice(&22u16.to_le_bytes()); // cslw
        let fc_lcb = lw + 2 + 88 + 2;
        word[fc_lcb - 2..fc_lcb].copy_from_slice(&93u16.to_le_bytes());
        // Piece text after the header.
        let mut cps = vec![0u32];
        let mut fcs = Vec::new();
        for (text, compressed) in pieces {
            let start = word.len() as u32;
            let n = text.encode_utf16().count() as u32;
            if *compressed {
                word.extend(text.bytes());
                fcs.push((start * 2) | 0x4000_0000);
            } else {
                word.extend(text.encode_utf16().flat_map(u16::to_le_bytes));
                fcs.push(start);
            }
            cps.push(cps.last().unwrap() + n);
        }
        let ccp_text = *cps.last().unwrap();
        word[lw + 2 + 12..lw + 2 + 16].copy_from_slice(&ccp_text.to_le_bytes());
        // Clx: a Prc to skip, then the Pcdt.
        let mut plc = Vec::new();
        cps.iter().for_each(|c| plc.extend(c.to_le_bytes()));
        for fc in &fcs {
            plc.extend([0, 0]);
            plc.extend(fc.to_le_bytes());
            plc.extend([0, 0]);
        }
        let mut clx = vec![0x01, 2, 0, 0xAA, 0xBB, 0x02];
        clx.extend((plc.len() as u32).to_le_bytes());
        clx.extend(plc);
        let mut table = vec![0u8; 16];
        let fc_clx = table.len() as u32;
        table.extend(&clx);
        word[fc_lcb + 33 * 8..fc_lcb + 33 * 8 + 4].copy_from_slice(&fc_clx.to_le_bytes());
        word[fc_lcb + 33 * 8 + 4..fc_lcb + 33 * 8 + 8].copy_from_slice(&(clx.len() as u32).to_le_bytes());

        let mut cfb = cfb::CompoundFile::create(Cursor::new(Vec::new())).unwrap();
        cfb.create_stream("/WordDocument").unwrap().write_all(&word).unwrap();
        cfb.create_stream("/1Table").unwrap().write_all(&table).unwrap();
        cfb.flush().unwrap();
        cfb.into_inner().into_inner()
    }

    #[test]
    fn doc_piece_table_text() {
        let bytes = doc(&[
            ("First para\rTab\u{7}cell\r", true),
            ("Ünïcode \u{13}PAGE\u{14}3\u{15} end\r", false),
        ]);
        assert_eq!(word_to_text(&bytes).unwrap(), "First para\nTab\tcell\nÜnïcode 3 end\n");
    }

    #[test]
    fn doc_rejects_other_ole_files() {
        let mut cfb = cfb::CompoundFile::create(Cursor::new(Vec::new())).unwrap();
        cfb.create_stream("/Workbook").unwrap().write_all(b"x").unwrap();
        cfb.flush().unwrap();
        let bytes = cfb.into_inner().into_inner();
        assert!(word_to_text(&bytes).unwrap_err().contains("WordDocument"));
    }
}
