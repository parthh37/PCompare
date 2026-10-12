//! A `.reg` export as an editable document. Edits rewrite only the lines they
//! touch, so comments, section order, line wrapping, line endings and the
//! file's encoding (UTF-16 for version 5 exports, ANSI/UTF-8 for `REGEDIT4`)
//! survive a save.

use super::reg_file::{continues, parse_reg, quoted, HEADER_V4, HEADER_V5};
use super::{canonical_root, RegKey, RegTree, RegType, RegValue, ValueData};
use pcompare_core::textfile::{self, TextFormat};
use pcompare_core::Result;

/// regedit wraps hex data so that no line exceeds this many columns.
const WRAP_COLUMNS: usize = 80;

/// One logical line of a key section.
#[derive(Debug, Clone)]
struct Item {
    /// Lower-case value name when the line assigns a value (empty for `@`).
    value: Option<String>,
    /// Physical lines; more than one when hex data is wrapped with `\`.
    lines: Vec<String>,
}

impl Item {
    fn blank() -> Self {
        Item {
            value: None,
            lines: vec![String::new()],
        }
    }

    fn is_blank(&self) -> bool {
        self.value.is_none() && self.lines.iter().all(|l| l.trim().is_empty())
    }
}

/// A `[KEY]` (or `[-KEY]`) line and everything up to the next one.
#[derive(Debug, Clone)]
struct Section {
    /// Normalised path (see [`norm_path`]); empty for a malformed key line,
    /// which no edit ever matches.
    norm: Vec<String>,
    deleted: bool,
    head: Vec<String>,
    items: Vec<Item>,
}

impl Section {
    fn new<S: AsRef<str>>(path: &[S], deleted: bool, values: Vec<Item>) -> Self {
        let joined = path
            .iter()
            .map(AsRef::as_ref)
            .collect::<Vec<_>>()
            .join("\\");
        let mut items = values;
        items.push(Item::blank());
        Section {
            norm: norm_path(path),
            deleted,
            head: vec![format!("[{}{joined}]", if deleted { "-" } else { "" })],
            items,
        }
    }

    fn ends_blank(&self) -> bool {
        self.items.last().is_some_and(Item::is_blank)
    }
}

/// Lower-cases a key path and expands the hive abbreviation, so paths
/// compare the way the registry does.
fn norm_path<S: AsRef<str>>(path: &[S]) -> Vec<String> {
    path.iter()
        .enumerate()
        .map(|(i, p)| {
            let p = p.as_ref();
            let p = if i == 0 {
                canonical_root(p).unwrap_or(p)
            } else {
                p
            };
            p.to_lowercase()
        })
        .collect()
}

fn is_under(path: &[String], prefix: &[String]) -> bool {
    path.len() >= prefix.len() && path[..prefix.len()] == *prefix
}

#[derive(Debug, Clone)]
pub struct RegDocument {
    /// Encoding, BOM and line ending to write the file back with.
    format: TextFormat,
    /// `REGEDIT4` (ANSI strings in `hex(…)` data) rather than version 5 (UTF-16).
    v4: bool,
    eol: &'static str,
    trailing_eol: bool,
    /// The header and whatever precedes the first key.
    prelude: Vec<String>,
    sections: Vec<Section>,
}

impl RegDocument {
    /// Decodes and splits a `.reg` file; fails only when the header is missing.
    pub fn parse(bytes: &[u8]) -> std::result::Result<Self, String> {
        let (text, encoding, has_bom) = textfile::decode(bytes);
        let format = TextFormat {
            encoding: encoding.name().to_owned(),
            has_bom,
            line_ending: textfile::save_line_ending(&text),
        };
        Self::from_text(&text, format)
    }

    pub fn from_text(text: &str, format: TextFormat) -> std::result::Result<Self, String> {
        let eol = if text.contains("\r\n") { "\r\n" } else { "\n" };
        let mut lines: Vec<String> = text
            .split('\n')
            .map(|l| l.strip_suffix('\r').unwrap_or(l).to_owned())
            .collect();
        let trailing_eol = text.ends_with('\n');
        if trailing_eol {
            lines.pop();
        }

        let header = lines
            .iter()
            .position(|l| !l.trim().is_empty())
            .filter(|&i| matches!(lines[i].trim(), HEADER_V5 | HEADER_V4))
            .ok_or_else(|| {
                format!("not a registry export (expected \"{HEADER_V5}\" or \"{HEADER_V4}\")")
            })?;
        let v4 = lines[header].trim() == HEADER_V4;

        let mut doc = RegDocument {
            format,
            v4,
            eol,
            trailing_eol,
            prelude: lines[..=header].to_vec(),
            sections: Vec::new(),
        };
        let mut i = header + 1;
        while i < lines.len() {
            // Join continuation lines exactly as the parser does.
            let start = i;
            let mut logical = lines[i].clone();
            while continues(&logical) && i + 1 < lines.len() {
                let keep = logical.trim_end().len() - 1;
                logical.truncate(keep);
                i += 1;
                logical.push_str(lines[i].trim_start());
            }
            i += 1;
            let physical = lines[start..i].to_vec();
            let line = logical.trim();
            if let Some(rest) = line.strip_prefix('[') {
                doc.sections.push(section_head(rest, physical));
            } else if let Some(section) = doc.sections.last_mut() {
                let value = if line.starts_with('@') {
                    Some(String::new())
                } else if line.starts_with('"') {
                    quoted(line).ok().map(|(name, _)| name.to_lowercase())
                } else {
                    None
                };
                section.items.push(Item {
                    value,
                    lines: physical,
                });
            } else {
                doc.prelude.extend(physical);
            }
        }
        Ok(doc)
    }

    /// True for `REGEDIT4` files.
    pub fn is_v4(&self) -> bool {
        self.v4
    }

    pub fn format(&self) -> &TextFormat {
        &self.format
    }

    pub fn to_text(&self) -> String {
        let mut lines: Vec<&str> = self.prelude.iter().map(String::as_str).collect();
        for s in &self.sections {
            lines.extend(s.head.iter().map(String::as_str));
            for item in &s.items {
                lines.extend(item.lines.iter().map(String::as_str));
            }
        }
        let mut text = lines.join(self.eol);
        if self.trailing_eol {
            text.push_str(self.eol);
        }
        text
    }

    /// The file contents in its original encoding.
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        textfile::encode_text(&self.to_text(), &self.format)
    }

    /// Parses the current text into a key tree.
    pub fn tree(&self) -> std::result::Result<RegTree, String> {
        parse_reg(&self.to_text())
    }

    fn live_sections(&self, norm: &[String]) -> Vec<usize> {
        (0..self.sections.len())
            .filter(|&i| !self.sections[i].deleted && self.sections[i].norm == norm)
            .collect()
    }

    /// Sets (adds or replaces) a value. The last assignment of the name is
    /// rewritten in place, since that one wins on import; a new value goes
    /// after the key's last value; a missing key gets a new section.
    pub fn set_value(&mut self, key: &[String], value: &RegValue) {
        let norm = norm_path(key);
        let lower = value.name.to_lowercase();
        let item = Item {
            value: Some(lower.clone()),
            lines: format_value(value, self.v4),
        };
        let matching = self.live_sections(&norm);
        for &si in matching.iter().rev() {
            let items = &mut self.sections[si].items;
            if let Some(ii) = items
                .iter()
                .rposition(|it| it.value.as_deref() == Some(&lower))
            {
                items[ii] = item;
                return;
            }
        }
        if let Some(&si) = matching.last() {
            let items = &mut self.sections[si].items;
            let at = items
                .iter()
                .rposition(|it| it.value.is_some())
                .map_or(0, |i| i + 1);
            items.insert(at, item);
            return;
        }
        let at = self.insert_pos(&norm);
        self.insert_sections(at, vec![Section::new(key, false, vec![item])]);
    }

    /// Removes every assignment of a value from the key; returns how many.
    pub fn delete_value(&mut self, key: &[String], name: &str) -> usize {
        let norm = norm_path(key);
        let lower = name.to_lowercase();
        let mut removed = 0;
        for si in self.live_sections(&norm) {
            let items = &mut self.sections[si].items;
            let before = items.len();
            items.retain(|it| it.value.as_deref() != Some(&lower));
            removed += before - items.len();
        }
        removed
    }

    /// Declares the key when the file doesn't already.
    pub fn add_key(&mut self, key: &[String]) {
        let norm = norm_path(key);
        if self.live_sections(&norm).is_empty() {
            let at = self.insert_pos(&norm);
            self.insert_sections(at, vec![Section::new(key, false, Vec::new())]);
        }
    }

    /// Removes the key's sections and those of all its descendants (including
    /// `[-KEY]` markers). Returns where the first removed section was.
    pub fn delete_key(&mut self, key: &[String]) -> Option<usize> {
        let norm = norm_path(key);
        let first = self.sections.iter().position(|s| is_under(&s.norm, &norm));
        self.sections.retain(|s| !is_under(&s.norm, &norm));
        first
    }

    /// Replaces the subtree at `to` with a copy of `source` (from another
    /// tree), or deletes it when `source` is `None`.
    pub fn replace_key(&mut self, to: &[String], source: Option<&RegKey>) {
        let norm = norm_path(to);
        let at = self
            .delete_key(to)
            .unwrap_or_else(|| self.insert_pos(&norm));
        if let Some(src) = source {
            let mut out = Vec::new();
            self.emit(src, to.to_vec(), true, &mut out);
            self.insert_sections(at, out);
        }
    }

    fn emit(&self, key: &RegKey, path: Vec<String>, top: bool, out: &mut Vec<Section>) {
        if key.deleted {
            out.push(Section::new(&path, true, Vec::new()));
        } else if top || key.declared || !key.values.is_empty() {
            let values = key
                .values
                .iter()
                .map(|v| Item {
                    value: Some(v.name.to_lowercase()),
                    lines: format_value(v, self.v4),
                })
                .collect();
            out.push(Section::new(&path, false, values));
        }
        for child in key.children.values() {
            let mut child_path = path.clone();
            child_path.push(child.name.clone());
            self.emit(child, child_path, false, out);
        }
    }

    /// Where a new section for `norm` goes: among the sections sharing the
    /// longest path prefix with it, before the first that sorts after it.
    fn insert_pos(&self, norm: &[String]) -> usize {
        let common = |s: &Section| s.norm.iter().zip(norm).take_while(|(a, b)| a == b).count();
        let Some(best) = self.sections.iter().map(common).max() else {
            return 0;
        };
        let candidates: Vec<usize> = (0..self.sections.len())
            .filter(|&i| common(&self.sections[i]) == best)
            .collect();
        candidates
            .iter()
            .copied()
            .find(|&i| self.sections[i].norm.as_slice() > norm)
            .unwrap_or_else(|| candidates.last().map_or(self.sections.len(), |&i| i + 1))
    }

    /// Inserts sections, keeping a blank line between consecutive sections.
    fn insert_sections(&mut self, at: usize, new: Vec<Section>) {
        if at == 0 {
            if self.prelude.last().is_some_and(|l| !l.trim().is_empty()) {
                self.prelude.push(String::new());
            }
        } else if !self.sections[at - 1].ends_blank() {
            self.sections[at - 1].items.push(Item::blank());
        }
        self.sections.splice(at..at, new);
    }
}

/// Parses the text after `[` of a key line.
fn section_head(rest: &str, head: Vec<String>) -> Section {
    let malformed = |head| Section {
        norm: Vec::new(),
        deleted: false,
        head,
        items: Vec::new(),
    };
    let Some(end) = rest.rfind(']') else {
        return malformed(head);
    };
    let inner = &rest[..end];
    let (deleted, path) = match inner.strip_prefix('-') {
        Some(p) => (true, p),
        None => (false, inner),
    };
    let parts: Vec<&str> = path.split('\\').filter(|p| !p.is_empty()).collect();
    if parts.is_empty() {
        return malformed(head);
    }
    Section {
        norm: norm_path(&parts),
        deleted,
        head,
        items: Vec::new(),
    }
}

fn escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Formats a value assignment the way regedit exports it.
pub(super) fn format_value(value: &RegValue, v4: bool) -> Vec<String> {
    let name = if value.name.is_empty() {
        "@".to_owned()
    } else {
        format!("\"{}\"", escape(&value.name))
    };
    match (value.kind, &value.data) {
        (RegType::Delete, _) => vec![format!("{name}=-")],
        // Line breaks can't be written inside a quoted string; fall through to hex(1).
        (RegType::Sz, ValueData::Str(s)) if !s.contains(['\r', '\n', '\0']) => {
            vec![format!("{name}=\"{}\"", escape(s))]
        }
        (RegType::Dword, ValueData::U32(v)) => vec![format!("{name}=dword:{v:08x}")],
        (RegType::Binary, ValueData::Bytes(b)) => hex_lines(format!("{name}=hex:"), b),
        (kind, data) => hex_lines(
            format!("{name}=hex({:x}):", kind.code().unwrap_or(0)),
            &value_bytes(kind, data, v4),
        ),
    }
}

/// Raw registry bytes of a value; strings are UTF-16LE in version 5 files.
pub(super) fn value_bytes(kind: RegType, data: &ValueData, v4: bool) -> Vec<u8> {
    let mut out = Vec::new();
    let mut text = |s: &str| {
        if v4 {
            out.extend_from_slice(s.as_bytes());
            out.push(0);
        } else {
            s.encode_utf16()
                .chain([0])
                .for_each(|u| out.extend_from_slice(&u.to_le_bytes()));
        }
    };
    match data {
        ValueData::Str(s) => text(s),
        ValueData::Multi(items) => {
            items.iter().for_each(|i| text(i));
            text("");
        }
        ValueData::U32(v) if kind == RegType::DwordBigEndian => {
            out.extend_from_slice(&v.to_be_bytes())
        }
        ValueData::U32(v) => out.extend_from_slice(&v.to_le_bytes()),
        ValueData::U64(v) => out.extend_from_slice(&v.to_le_bytes()),
        ValueData::Bytes(b) => out.extend_from_slice(b),
    }
    out
}

/// `head` followed by comma-separated hex bytes, wrapped with `\` so no line
/// exceeds [`WRAP_COLUMNS`]; continuation lines are indented two spaces.
fn hex_lines(head: String, bytes: &[u8]) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = head;
    let mut on_line = 0;
    for (i, b) in bytes.iter().enumerate() {
        let token = if i + 1 < bytes.len() {
            format!("{b:02x},")
        } else {
            format!("{b:02x}")
        };
        if on_line > 0 && line.len() + token.len() + 1 > WRAP_COLUMNS {
            line.push('\\');
            lines.push(std::mem::replace(&mut line, "  ".to_owned()));
            on_line = 0;
        }
        line.push_str(&token);
        on_line += 1;
    }
    lines.push(line);
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    const V5: &str = "Windows Registry Editor Version 5.00\r\n\
        \r\n\
        ; exported by hand\r\n\
        [HKEY_CURRENT_USER\\Software\\Vendor]\r\n\
        @=\"root\"\r\n\
        \"Count\"=dword:00000001\r\n\
        \"Blob\"=hex:01,02,\\\r\n  03\r\n\
        \r\n\
        [HKEY_CURRENT_USER\\Software\\Vendor\\Sub]\r\n\
        \"A\"=\"1\"\r\n\
        \r\n";

    fn utf16_file(text: &str) -> Vec<u8> {
        let mut out = vec![0xFF, 0xFE];
        text.encode_utf16()
            .for_each(|u| out.extend_from_slice(&u.to_le_bytes()));
        out
    }

    fn path(p: &str) -> Vec<String> {
        p.split('\\').map(str::to_owned).collect()
    }

    fn value(tree: &RegTree, key: &str, name: &str) -> Option<RegValue> {
        tree.get(&path(key))?.value(name).cloned()
    }

    #[test]
    fn untouched_round_trip_is_byte_identical() {
        let bytes = utf16_file(V5);
        let doc = RegDocument::parse(&bytes).unwrap();
        assert!(!doc.is_v4());
        assert_eq!(doc.to_text(), V5);
        assert_eq!(doc.to_bytes().unwrap(), bytes);

        let v4 = "REGEDIT4\n\n[HKEY_LOCAL_MACHINE\\A]\n\"x\"=\"1\"\n";
        let doc = RegDocument::parse(v4.as_bytes()).unwrap();
        assert!(doc.is_v4());
        assert_eq!(doc.to_bytes().unwrap(), v4.as_bytes());

        assert!(RegDocument::parse(b"[HKEY_CURRENT_USER\\A]\n").is_err());
    }

    #[test]
    fn set_value_rewrites_in_place_and_keeps_format() {
        let mut doc = RegDocument::parse(&utf16_file(V5)).unwrap();
        let key = path("HKCU\\software\\VENDOR");
        doc.set_value(
            &key,
            &RegValue {
                name: "count".into(),
                kind: RegType::Dword,
                data: ValueData::U32(42),
            },
        );
        doc.set_value(
            &key,
            &RegValue {
                name: "Path".into(),
                kind: RegType::ExpandSz,
                data: ValueData::Str("%SystemRoot%\\x \"q\"".into()),
            },
        );
        let text = doc.to_text();
        assert!(text.contains("\"count\"=dword:0000002a\r\n"), "{text}");
        assert!(!text.contains("dword:00000001"));
        assert!(text.contains("; exported by hand\r\n"));
        // New values go after the key's last value, before the blank line.
        assert!(
            text.contains("  03\r\n\"Path\"=hex(2):25,00,53,00"),
            "{text}"
        );

        let bytes = doc.to_bytes().unwrap();
        assert_eq!(&bytes[..2], &[0xFF, 0xFE], "UTF-16LE BOM kept");
        let reparsed = RegDocument::parse(&bytes).unwrap().tree().unwrap();
        let p = value(&reparsed, "HKEY_CURRENT_USER\\Software\\Vendor", "path").unwrap();
        assert_eq!(p.data, ValueData::Str("%SystemRoot%\\x \"q\"".into()));
        let c = value(&reparsed, "HKEY_CURRENT_USER\\Software\\Vendor", "Count").unwrap();
        assert_eq!(c.data, ValueData::U32(42));
    }

    #[test]
    fn v4_strings_stay_ansi() {
        let mut doc = RegDocument::parse(b"REGEDIT4\n\n[HKEY_CURRENT_USER\\A]\n").unwrap();
        doc.set_value(
            &path("HKEY_CURRENT_USER\\A"),
            &RegValue {
                name: "M".into(),
                kind: RegType::MultiSz,
                data: ValueData::Multi(vec!["a".into(), "b".into()]),
            },
        );
        let text = doc.to_text();
        assert!(text.contains("\"M\"=hex(7):61,00,62,00,00\n"), "{text}");
        let tree = doc.tree().unwrap();
        assert_eq!(
            value(&tree, "HKEY_CURRENT_USER\\A", "m").unwrap().data,
            ValueData::Multi(vec!["a".into(), "b".into()])
        );
    }

    #[test]
    fn all_value_types_round_trip() {
        let values = [
            (RegType::Sz, ValueData::Str("plain \\ \"quoted\"".into())),
            (RegType::Sz, ValueData::Str("two\nlines".into())),
            (RegType::ExpandSz, ValueData::Str("%TEMP%".into())),
            (RegType::MultiSz, ValueData::Multi(vec![])),
            (RegType::Dword, ValueData::U32(0xdead_beef)),
            (RegType::DwordBigEndian, ValueData::U32(7)),
            (RegType::Qword, ValueData::U64(1 << 40)),
            (RegType::Binary, ValueData::Bytes((0..=200).collect())),
            (RegType::None, ValueData::Bytes(vec![])),
            (RegType::Other(0x20), ValueData::Bytes(vec![9])),
            (RegType::Delete, ValueData::Bytes(vec![])),
        ];
        for v4 in [false, true] {
            let header = if v4 { "REGEDIT4" } else { HEADER_V5 };
            let mut doc = RegDocument::from_text(
                &format!("{header}\r\n\r\n"),
                RegDocument::parse(b"REGEDIT4\n").unwrap().format.clone(),
            )
            .unwrap();
            let key = path("HKEY_LOCAL_MACHINE\\T");
            for (i, (kind, data)) in values.iter().enumerate() {
                doc.set_value(
                    &key,
                    &RegValue {
                        name: format!("v{i}"),
                        kind: *kind,
                        data: data.clone(),
                    },
                );
            }
            let text = doc.to_text();
            assert!(text.lines().all(|l| l.len() <= WRAP_COLUMNS), "{text}");
            let tree = doc.tree().unwrap();
            assert!(tree.warnings.is_empty(), "{:?}", tree.warnings);
            for (i, (kind, data)) in values.iter().enumerate() {
                let got = value(&tree, "HKEY_LOCAL_MACHINE\\T", &format!("v{i}")).unwrap();
                assert_eq!(got.kind, *kind, "v{i} (v4: {v4})");
                if *kind != RegType::Delete {
                    assert_eq!(got.data, *data, "v{i} (v4: {v4})");
                }
            }
        }
    }

    #[test]
    fn keys_are_added_in_order_and_deleted_recursively() {
        let mut doc = RegDocument::parse(V5.as_bytes()).unwrap();
        doc.add_key(&path("HKEY_CURRENT_USER\\Software\\Vendor\\Alpha"));
        doc.add_key(&path("HKEY_CURRENT_USER\\Software\\Vendor\\Sub")); // already declared
        let text = doc.to_text();
        let alpha = text
            .find("[HKEY_CURRENT_USER\\Software\\Vendor\\Alpha]")
            .unwrap();
        let sub = text
            .find("[HKEY_CURRENT_USER\\Software\\Vendor\\Sub]")
            .unwrap();
        assert!(alpha < sub, "{text}");
        assert_eq!(text.matches("\\Sub]").count(), 1);
        assert!(
            text.contains("  03\r\n\r\n[HKEY_CURRENT_USER\\Software\\Vendor\\Alpha]\r\n\r\n[HKEY")
        );

        assert_eq!(
            doc.delete_key(&path("hkcu\\software\\vendor")),
            Some(0),
            "case-insensitive, abbreviated hive"
        );
        let tree = doc.tree().unwrap();
        assert_eq!(tree.counts(), (0, 0));
        assert!(doc
            .to_text()
            .starts_with("Windows Registry Editor Version 5.00\r\n"));

        // Values on a key without a section create one.
        doc.set_value(
            &path("HKEY_CURRENT_USER\\New"),
            &RegValue {
                name: String::new(),
                kind: RegType::Sz,
                data: ValueData::Str("d".into()),
            },
        );
        assert!(doc
            .to_text()
            .ends_with("[HKEY_CURRENT_USER\\New]\r\n@=\"d\"\r\n\r\n"));
    }

    #[test]
    fn delete_value_removes_every_assignment() {
        let mut doc = RegDocument::parse(
            b"REGEDIT4\n\n[HKEY_CURRENT_USER\\A]\n\"x\"=\"1\"\n\n[HKEY_CURRENT_USER\\a]\n\"X\"=\"2\"\n\"y\"=\"3\"\n",
        )
        .unwrap();
        assert_eq!(doc.delete_value(&path("HKCU\\A"), "x"), 2);
        let tree = doc.tree().unwrap();
        assert!(value(&tree, "HKEY_CURRENT_USER\\A", "x").is_none());
        assert!(value(&tree, "HKEY_CURRENT_USER\\A", "y").is_some());
        assert_eq!(doc.delete_value(&path("HKCU\\A"), "missing"), 0);
    }

    #[test]
    fn replace_key_copies_a_subtree_across_formats() {
        let source = parse_reg(
            "REGEDIT4\n\n[HKEY_LOCAL_MACHINE\\Src]\n\"s\"=\"v\"\n\n[HKEY_LOCAL_MACHINE\\Src\\Child]\n\
             \"e\"=hex(2):25,41,25,00\n\n[-HKEY_LOCAL_MACHINE\\Src\\Gone]\n",
        )
        .unwrap();
        let src = source.get(&path("HKEY_LOCAL_MACHINE\\Src")).unwrap();

        let mut doc = RegDocument::parse(&utf16_file(
            "Windows Registry Editor Version 5.00\r\n\r\n[HKEY_LOCAL_MACHINE\\Dst]\r\n\"old\"=\"1\"\r\n\r\n\
             [HKEY_LOCAL_MACHINE\\Dst\\Stale]\r\n\r\n[HKEY_LOCAL_MACHINE\\Zed]\r\n\r\n",
        ))
        .unwrap();
        doc.replace_key(&path("HKEY_LOCAL_MACHINE\\Dst"), Some(src));
        let text = doc.to_text();
        assert!(!text.contains("Stale") && !text.contains("old"), "{text}");
        assert!(
            text.find("[HKEY_LOCAL_MACHINE\\Dst]").unwrap()
                < text.find("[HKEY_LOCAL_MACHINE\\Zed]").unwrap(),
            "copied where the old subtree was"
        );
        // REGEDIT4 ANSI data is rewritten as UTF-16 for the version 5 target.
        assert!(
            text.contains("\"e\"=hex(2):25,00,41,00,25,00,00,00"),
            "{text}"
        );
        assert!(text.contains("[-HKEY_LOCAL_MACHINE\\Dst\\Gone]"));

        let tree = RegDocument::parse(&doc.to_bytes().unwrap())
            .unwrap()
            .tree()
            .unwrap();
        let dst = tree.get(&path("HKEY_LOCAL_MACHINE\\Dst")).unwrap();
        assert_eq!(dst.value("s").unwrap().data, ValueData::Str("v".into()));
        assert_eq!(
            dst.child("child").unwrap().value("e").unwrap().data,
            ValueData::Str("%A%".into())
        );
        assert!(dst.child("gone").unwrap().deleted);

        doc.replace_key(&path("HKEY_LOCAL_MACHINE\\Dst"), None);
        assert!(doc
            .tree()
            .unwrap()
            .get(&path("HKEY_LOCAL_MACHINE\\Dst"))
            .is_none());
    }

    #[test]
    fn hex_wrapping() {
        let lines = hex_lines("\"b\"=hex:".into(), &[0xab; 40]);
        assert!(lines.len() > 1);
        assert!(lines[..lines.len() - 1].iter().all(|l| l.ends_with(",\\")));
        assert!(lines[1..].iter().all(|l| l.starts_with("  ")));
        assert!(lines.iter().all(|l| l.len() <= WRAP_COLUMNS));
        assert_eq!(hex_lines("x=hex:".into(), &[]), vec!["x=hex:".to_string()]);
    }
}
