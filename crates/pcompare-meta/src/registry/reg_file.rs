//! Parser for regedit `.reg` exports (`Windows Registry Editor Version 5.00`
//! and the older ANSI `REGEDIT4`).

use super::{RegTree, RegType, RegValue, ValueData};

const HEADER_V5: &str = "Windows Registry Editor Version 5.00";
const HEADER_V4: &str = "REGEDIT4";

/// Parses the decoded text of a `.reg` file. Malformed lines are skipped and
/// reported in [`RegTree::warnings`]; only a missing header is fatal.
pub fn parse_reg(text: &str) -> Result<RegTree, String> {
    let mut lines = text.lines().enumerate();
    let utf16 = loop {
        match lines.next() {
            Some((_, l)) if l.trim().is_empty() => continue,
            Some((_, l)) if l.trim() == HEADER_V5 => break true,
            Some((_, l)) if l.trim() == HEADER_V4 => break false,
            _ => {
                return Err(format!(
                    "not a registry export (expected \"{HEADER_V5}\" or \"{HEADER_V4}\")"
                ))
            }
        }
    };

    let mut tree = RegTree::default();
    let mut current: Option<Vec<String>> = None;
    let mut logical = String::new();
    let mut start = 0;
    for (i, raw) in lines {
        if logical.is_empty() {
            start = i + 1;
            logical.push_str(raw);
        } else {
            logical.push_str(raw.trim_start());
        }
        if continues(&logical) {
            // Drop the trailing backslash (and any whitespace before it).
            let keep = logical.trim_end().len() - 1;
            logical.truncate(keep);
            continue;
        }
        parse_line(&mut tree, &mut current, logical.trim(), start, utf16);
        logical.clear();
    }
    if !logical.is_empty() {
        parse_line(&mut tree, &mut current, logical.trim(), start, utf16);
    }
    Ok(tree)
}

/// A line continues when it ends in `\` outside of a quoted string
/// (hex data is wrapped at 80 columns this way).
fn continues(line: &str) -> bool {
    let line = line.trim_end();
    if !line.ends_with('\\') {
        return false;
    }
    let mut in_quote = false;
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => in_quote = !in_quote,
            '\\' if in_quote => {
                chars.next();
            }
            _ => {}
        }
    }
    !in_quote
}

fn parse_line(
    tree: &mut RegTree,
    current: &mut Option<Vec<String>>,
    line: &str,
    lineno: usize,
    utf16: bool,
) {
    if line.is_empty() || line.starts_with(';') {
        return;
    }
    if let Some(rest) = line.strip_prefix('[') {
        let Some(end) = rest.rfind(']') else {
            tree.warn(format!("line {lineno}: unterminated key name"));
            *current = None;
            return;
        };
        let inner = &rest[..end];
        let (deleted, path) = match inner.strip_prefix('-') {
            Some(p) => (true, p),
            None => (false, inner),
        };
        let parts: Vec<&str> = path.split('\\').filter(|p| !p.is_empty()).collect();
        if parts.is_empty() {
            tree.warn(format!("line {lineno}: empty key name"));
            *current = None;
            return;
        }
        let key = tree.key_mut(&parts);
        key.declared = true;
        key.deleted = deleted;
        // Values cannot follow a deletion; they would be ignored on import.
        *current = (!deleted).then(|| parts.iter().map(|p| p.to_string()).collect());
        return;
    }
    if line.starts_with('"') || line.starts_with('@') {
        let Some(path) = current.as_ref() else {
            tree.warn(format!("line {lineno}: value outside of a key"));
            return;
        };
        match parse_value(line, utf16) {
            Ok(value) => {
                let parts: Vec<&str> = path.iter().map(String::as_str).collect();
                tree.key_mut(&parts).set_value(value);
            }
            Err(e) => tree.warn(format!("line {lineno}: {e}")),
        }
        return;
    }
    tree.warn(format!("line {lineno}: unrecognised line"));
}

/// Reads a `"…"` string with `\\` and `\"` escapes; returns it and the rest.
fn quoted(s: &str) -> Result<(String, &str), String> {
    let body = s.strip_prefix('"').ok_or("expected '\"'")?;
    let mut out = String::new();
    let mut chars = body.char_indices();
    while let Some((i, c)) = chars.next() {
        match c {
            '"' => return Ok((out, &body[i + 1..])),
            '\\' => match chars.next() {
                Some((_, e @ ('\\' | '"'))) => out.push(e),
                Some((_, e)) => {
                    out.push('\\');
                    out.push(e);
                }
                None => out.push('\\'),
            },
            c => out.push(c),
        }
    }
    Err("unterminated string".into())
}

fn parse_value(line: &str, utf16: bool) -> Result<RegValue, String> {
    let (name, rest) = match line.strip_prefix('@') {
        Some(rest) => (String::new(), rest),
        None => quoted(line)?,
    };
    let data = rest
        .trim_start()
        .strip_prefix('=')
        .ok_or("expected '='")?
        .trim();

    let (kind, data) = if data == "-" {
        (RegType::Delete, ValueData::Bytes(Vec::new()))
    } else if data.starts_with('"') {
        (RegType::Sz, ValueData::Str(quoted(data)?.0))
    } else if let Some(hex) = strip_prefix_ci(data, "dword:") {
        let v =
            u32::from_str_radix(hex.trim(), 16).map_err(|_| format!("invalid dword \"{hex}\""))?;
        (RegType::Dword, ValueData::U32(v))
    } else if let Some(rest) = strip_prefix_ci(data, "hex") {
        let (code, bytes) = match rest.strip_prefix('(') {
            Some(r) => {
                let (code, after) = r.split_once(')').ok_or("unterminated hex(…) type")?;
                let code = u32::from_str_radix(code.trim(), 16)
                    .map_err(|_| format!("invalid value type \"{code}\""))?;
                (code, after)
            }
            None => (3, rest),
        };
        let bytes = bytes.strip_prefix(':').ok_or("expected ':' after hex")?;
        let kind = RegType::from_code(code);
        (kind, ValueData::from_bytes(kind, hex_list(bytes)?, utf16))
    } else {
        return Err(format!(
            "unrecognised value data \"{}\"",
            truncate(data, 40)
        ));
    };
    Ok(RegValue { name, kind, data })
}

fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    let head = s.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| &s[prefix.len()..])
}

fn hex_list(s: &str) -> Result<Vec<u8>, String> {
    s.split(',')
        .map(str::trim)
        .filter(|b| !b.is_empty())
        .map(|b| {
            u8::from_str_radix(b, 16)
                .map_err(|_| format!("invalid hex byte \"{}\"", truncate(b, 8)))
        })
        .collect()
}

fn truncate(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utf16_hex(s: &str) -> String {
        s.encode_utf16()
            .chain([0])
            .flat_map(|u| u.to_le_bytes())
            .map(|b| format!("{b:02x}"))
            .collect::<Vec<_>>()
            .join(",")
    }

    fn values(tree: &RegTree, path: &[&str]) -> Vec<RegValue> {
        let path: Vec<String> = path.iter().map(|p| p.to_string()).collect();
        tree.get(&path).unwrap().values().to_vec()
    }

    #[test]
    fn requires_header() {
        assert!(parse_reg("[HKEY_CURRENT_USER\\A]\n").is_err());
        assert!(parse_reg("").is_err());
        assert!(parse_reg("\n\nREGEDIT4\n").is_ok());
    }

    #[test]
    fn parses_v5_value_types() {
        let text = format!(
            "Windows Registry Editor Version 5.00\r\n\
             \r\n\
             ; a comment\r\n\
             [HKEY_LOCAL_MACHINE\\SOFTWARE\\Test]\r\n\
             @=\"default\"\r\n\
             \"Path\"=\"C:\\\\Program Files\\\\App\\\\\"\r\n\
             \"Quote\"=\"say \\\"hi\\\"\"\r\n\
             \"Count\"=dword:0000002a\r\n\
             \"Blob\"=hex:01,02,\\\r\n  ab,cd\r\n\
             \"Expand\"=hex(2):{}\r\n\
             \"Multi\"=hex(7):{}\r\n\
             \"Big\"=hex(b):00,00,00,00,01,00,00,00\r\n\
             \"None\"=hex(0):\r\n\
             \"Gone\"=-\r\n",
            utf16_hex("%SystemRoot%\\x"),
            utf16_hex("one\0two\0"),
        );
        let tree = parse_reg(&text).unwrap();
        assert!(tree.warnings.is_empty(), "{:?}", tree.warnings);
        let v = values(&tree, &["HKLM", "SOFTWARE", "test"]);
        let get = |n: &str| v.iter().find(|x| x.name == n).unwrap().clone();

        assert_eq!(get("").data, ValueData::Str("default".into()));
        assert_eq!(
            get("Path").data,
            ValueData::Str("C:\\Program Files\\App\\".into())
        );
        assert_eq!(get("Quote").data, ValueData::Str("say \"hi\"".into()));
        assert_eq!(get("Count").data, ValueData::U32(42));
        assert_eq!(get("Blob").data, ValueData::Bytes(vec![1, 2, 0xab, 0xcd]));
        assert_eq!(get("Expand").kind, RegType::ExpandSz);
        assert_eq!(get("Expand").data, ValueData::Str("%SystemRoot%\\x".into()));
        assert_eq!(
            get("Multi").data,
            ValueData::Multi(vec!["one".into(), "two".into()])
        );
        assert_eq!(get("Big").data, ValueData::U64(1 << 32));
        assert_eq!(get("None").kind, RegType::None);
        assert_eq!(get("None").data, ValueData::Bytes(vec![]));
        assert_eq!(get("Gone").kind, RegType::Delete);
    }

    #[test]
    fn regedit4_strings_are_ansi() {
        let tree =
            parse_reg("REGEDIT4\n\n[HKEY_CURRENT_USER\\A]\n\"S\"=hex(2):25,50,41,54,48,25,00\n")
                .unwrap();
        let v = values(&tree, &["HKEY_CURRENT_USER", "A"]);
        assert_eq!(v[0].data, ValueData::Str("%PATH%".into()));
    }

    #[test]
    fn keys_and_deletions() {
        let tree = parse_reg(
            "REGEDIT4\n\n[HKEY_CURRENT_USER\\A]\n\"x\"=\"1\"\n\n[-HKEY_CURRENT_USER\\A\\Old]\n\"ignored\"=\"v\"\n\n\
             [HKEY_CURRENT_USER\\a]\n\"X\"=\"2\"\n",
        )
        .unwrap();
        assert_eq!(tree.warnings.len(), 1, "value after a deletion is reported");
        let a = tree.get(&["HKEY_CURRENT_USER".into(), "A".into()]).unwrap();
        assert_eq!(
            a.values().len(),
            1,
            "same key/value merged case-insensitively"
        );
        assert_eq!(a.values()[0].data, ValueData::Str("2".into()));
        assert!(a.child("old").unwrap().deleted);
        assert_eq!(
            tree.base_path(),
            vec!["HKEY_CURRENT_USER".to_string(), "A".to_string()]
        );
    }

    #[test]
    fn malformed_lines_are_warnings() {
        let tree = parse_reg(
            "REGEDIT4\n\"orphan\"=\"v\"\n[HKEY_CURRENT_USER\\A]\n\"bad\"=dword:zz\n\"u\"=\"unterminated\ngarbage\n\"ok\"=\"1\"\n",
        )
        .unwrap();
        assert_eq!(tree.warnings.len(), 4, "{:?}", tree.warnings);
        assert_eq!(values(&tree, &["HKCU", "A"]).len(), 1);
    }

    #[test]
    fn continuation_detection() {
        assert!(continues("\"b\"=hex:01,02,\\"));
        assert!(continues("\"b\"=hex:01,02,\\  "));
        assert!(!continues("\"p\"=\"C:\\\\\""));
        assert!(!continues("\"p\"=\"ends in backslash\\\\\""));
    }
}
