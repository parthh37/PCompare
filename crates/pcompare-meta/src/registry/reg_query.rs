//! Parser for `reg.exe query <key> /s` output, used for remote machines
//! (where `reg export` is unavailable).
//!
//! ```text
//! HKEY_LOCAL_MACHINE\SOFTWARE\Vendor
//!     (Default)    REG_SZ    hello
//!     Count    REG_DWORD    0x2a
//!
//! HKEY_LOCAL_MACHINE\SOFTWARE\Vendor\Sub
//! ```
//!
//! Columns are separated by exactly four spaces. Limitations inherited from
//! the format: the `(Default)` / `(value not set)` markers are localised by
//! Windows (only the English ones are recognised), and a value name that
//! itself contains a four-space run followed by a type name is ambiguous.

use super::{RegTree, RegType, RegValue, ValueData};

const SEP: &str = "    ";

pub fn parse_reg_query(text: &str) -> RegTree {
    let mut tree = RegTree::default();
    let mut current: Option<Vec<String>> = None;
    for (i, line) in text.lines().enumerate() {
        let lineno = i + 1;
        if line.trim().is_empty() {
            continue;
        }
        if let Some(value_line) = line.strip_prefix(SEP) {
            let Some(path) = current.as_ref() else {
                tree.warn(format!("line {lineno}: value outside of a key"));
                continue;
            };
            match parse_value(value_line) {
                Some(Some(value)) => {
                    let parts: Vec<&str> = path.iter().map(String::as_str).collect();
                    tree.key_mut(&parts).set_value(value);
                }
                Some(None) => {} // default value not set
                None => tree.warn(format!("line {lineno}: unrecognised value line")),
            }
            continue;
        }
        let key = line.trim_end();
        // Remote output may repeat the `\\MACHINE\` prefix on every key.
        let key = match key.strip_prefix("\\\\") {
            Some(rest) => rest.split_once('\\').map_or("", |(_, k)| k),
            None => key,
        };
        let parts: Vec<&str> = key.split('\\').filter(|p| !p.is_empty()).collect();
        if parts
            .first()
            .and_then(|r| super::canonical_root(r))
            .is_none()
        {
            // Trailers such as "End of search: 3 match(es) found." or errors.
            current = None;
            continue;
        }
        tree.key_mut(&parts).declared = true;
        current = Some(parts.iter().map(|p| p.to_string()).collect());
    }
    tree
}

/// `Some(None)` for an unset default value, `None` when unparseable.
fn parse_value(line: &str) -> Option<Option<RegValue>> {
    // Find the first "    REG_xxx" column that names a known type.
    let mut search = 0;
    let (name, kind, data) = loop {
        let at = search + line[search..].find(SEP)?;
        let after = &line[at + SEP.len()..];
        let (ty, data) = match after.find(SEP) {
            Some(j) => (&after[..j], &after[j + SEP.len()..]),
            None => (after.trim_end(), ""),
        };
        if let Some(kind) = RegType::from_name(ty) {
            break (&line[..at], kind, data);
        }
        search = at + 1;
    };
    let name = if name == "(Default)" { "" } else { name };
    if name.is_empty() && kind == RegType::Sz && data == "(value not set)" {
        return Some(None);
    }

    let number = |s: &str| {
        s.strip_prefix("0x")
            .and_then(|h| u64::from_str_radix(h, 16).ok())
    };
    let data = match kind {
        RegType::Sz | RegType::ExpandSz | RegType::Link => ValueData::Str(data.to_owned()),
        RegType::MultiSz => {
            let mut items: Vec<String> = data.split("\\0").map(str::to_owned).collect();
            while items.last().is_some_and(String::is_empty) {
                items.pop();
            }
            ValueData::Multi(items)
        }
        RegType::Dword | RegType::DwordBigEndian => {
            ValueData::U32(u32::try_from(number(data)?).ok()?)
        }
        RegType::Qword => ValueData::U64(number(data)?),
        _ => ValueData::Bytes(hex_string(data)?),
    };
    Some(Some(RegValue {
        name: name.to_owned(),
        kind,
        data,
    }))
}

/// `0102AB` → `[1, 2, 171]`.
fn hex_string(s: &str) -> Option<Vec<u8>> {
    let s = s.trim();
    if !s.len().is_multiple_of(2) || !s.is_ascii() {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Leading indentation is significant, so this is a raw string.
    const OUTPUT: &str = r"
HKEY_LOCAL_MACHINE\SOFTWARE\Vendor
    (Default)    REG_SZ    hello world
    Count    REG_DWORD    0x2a
    Big    REG_QWORD    0x100000000
    Blob    REG_BINARY    0102AB
    Empty    REG_BINARY    
    List    REG_MULTI_SZ    one\0two
    Path    REG_EXPAND_SZ    %SystemRoot%\x
    Name With  Spaces    REG_SZ    a    b
    NoData    REG_SZ

HKEY_LOCAL_MACHINE\SOFTWARE\Vendor\Sub
    (Default)    REG_SZ    (value not set)

End of search: 9 match(es) found.
";

    #[test]
    fn parses_query_output() {
        let tree = parse_reg_query(OUTPUT);
        assert!(tree.warnings.is_empty(), "{:?}", tree.warnings);
        let path: Vec<String> = ["HKLM", "SOFTWARE", "Vendor"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let key = tree.get(&path).unwrap();
        let get = |n: &str| {
            key.values()
                .iter()
                .find(|v| v.name == n)
                .unwrap()
                .data
                .clone()
        };
        assert_eq!(get(""), ValueData::Str("hello world".into()));
        assert_eq!(get("Count"), ValueData::U32(42));
        assert_eq!(get("Big"), ValueData::U64(1 << 32));
        assert_eq!(get("Blob"), ValueData::Bytes(vec![1, 2, 0xab]));
        assert_eq!(get("Empty"), ValueData::Bytes(vec![]));
        assert_eq!(
            get("List"),
            ValueData::Multi(vec!["one".into(), "two".into()])
        );
        assert_eq!(get("Path"), ValueData::Str("%SystemRoot%\\x".into()));
        assert_eq!(get("Name With  Spaces"), ValueData::Str("a    b".into()));
        assert_eq!(get("NoData"), ValueData::Str(String::new()));

        let sub = key.child("sub").unwrap();
        assert!(sub.values().is_empty(), "unset default is skipped");
        assert_eq!(
            tree.base_path().join("\\"),
            "HKEY_LOCAL_MACHINE\\SOFTWARE\\Vendor"
        );
    }

    #[test]
    fn strips_remote_prefix() {
        let tree = parse_reg_query(
            "\\\\PC01\\HKEY_LOCAL_MACHINE\\SOFTWARE\\X\r\n    v    REG_SZ    1\r\n",
        );
        let path: Vec<String> = ["HKEY_LOCAL_MACHINE", "SOFTWARE", "X"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(tree.get(&path).unwrap().values().len(), 1);
    }

    #[test]
    fn bad_lines_warn() {
        let tree = parse_reg_query(
            "    v    REG_SZ    1\r\nHKEY_CURRENT_USER\\A\r\n    v    REG_DWORD    nope\r\n",
        );
        assert_eq!(tree.warnings.len(), 2);
    }
}
