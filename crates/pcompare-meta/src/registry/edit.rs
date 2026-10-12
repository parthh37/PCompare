//! Editing registry sources. Edits to `.reg` files are queued by the UI and
//! replayed onto a fresh [`RegDocument`] for every compare (so undo is just
//! dropping the last one) and written back on save. Live keys are changed
//! immediately through `reg add` / `reg delete` commands built by
//! [`live_plan`].

use super::reg_doc::RegDocument;
use super::{
    canonical_root, LiveSpec, LoadedSource, RegKey, RegTree, RegType, RegValue, SourceKind,
    ValueData,
};
use pcompare_core::{CoreError, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Side {
    Left,
    Right,
}

/// One change to one side. Key paths are full (`HKEY_…\…`), as reported in
/// `RegKeyDiff::left_path` / `right_path`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum RegEdit {
    /// Adds or replaces a value; `data` is in the value editor's text form
    /// (see [`parse_value_input`]). An empty `name` is the default value.
    SetValue {
        key: String,
        name: String,
        value_type: String,
        data: String,
    },
    DeleteValue {
        key: String,
        name: String,
    },
    AddKey {
        key: String,
    },
    /// Deletes the key and everything below it.
    DeleteKey {
        key: String,
    },
    /// Makes `to` on this side a recursive copy of `from` on the other side
    /// (deleting it when `from` doesn't exist there).
    CopyKey {
        from: String,
        to: String,
    },
    /// Makes value `name` of `to_key` equal to the other side's value of
    /// `from_key` (deleting it when the other side has none).
    CopyValue {
        from_key: String,
        to_key: String,
        name: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SideEdit {
    pub side: Side,
    pub edit: RegEdit,
}

fn split_path(key: &str) -> Result<Vec<String>> {
    let parts: Vec<String> = key
        .split('\\')
        .filter(|p| !p.is_empty())
        .map(str::to_owned)
        .collect();
    if parts.is_empty() {
        return Err(CoreError::Other("empty registry key path".into()));
    }
    Ok(parts)
}

// ---------- value input ----------

fn parse_type(name: &str) -> Option<RegType> {
    let name = name.trim();
    if let Some(kind) = RegType::from_name(&name.to_ascii_uppercase()) {
        return Some(kind);
    }
    let code = name.strip_prefix("hex(")?.strip_suffix(')')?;
    u32::from_str_radix(code, 16).ok().map(RegType::from_code)
}

fn parse_number(text: &str, max: u64) -> std::result::Result<u64, String> {
    let t = text.trim();
    let parsed = match t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        Some(hex) => u64::from_str_radix(hex, 16),
        None => t.parse::<u64>(),
    };
    let value =
        parsed.map_err(|_| format!("\"{t}\" is not a number (use decimal or 0x-prefixed hex)"))?;
    if value > max {
        return Err(format!("{t} is too large (maximum {max})"));
    }
    Ok(value)
}

fn parse_hex_bytes(text: &str) -> std::result::Result<Vec<u8>, String> {
    let mut out = Vec::new();
    for token in text
        .split(|c: char| c.is_whitespace() || c == ',')
        .filter(|t| !t.is_empty())
    {
        if token.len() > 2 && !token.len().is_multiple_of(2) {
            return Err(format!("\"{token}\" is not a whole number of hex bytes"));
        }
        for chunk in token.as_bytes().chunks(2) {
            let s = std::str::from_utf8(chunk)
                .map_err(|_| format!("invalid hex byte in \"{token}\""))?;
            out.push(u8::from_str_radix(s, 16).map_err(|_| format!("\"{s}\" is not a hex byte"))?);
        }
    }
    Ok(out)
}

/// Reads value data typed in the editor: strings as is, multi-strings one
/// item per line (blank lines dropped), numbers in decimal or `0x` hex, and
/// everything else as hex bytes separated by spaces or commas.
pub fn parse_value_input(type_name: &str, text: &str) -> Result<(RegType, ValueData)> {
    let kind = parse_type(type_name)
        .filter(|k| *k != RegType::Delete)
        .ok_or_else(|| CoreError::Other(format!("unknown value type \"{type_name}\"")))?;
    let data = match kind {
        RegType::Sz | RegType::ExpandSz | RegType::Link => Ok(ValueData::Str(text.to_owned())),
        RegType::MultiSz => Ok(ValueData::Multi(
            text.split('\n')
                .map(|l| l.strip_suffix('\r').unwrap_or(l))
                .filter(|l| !l.is_empty())
                .map(str::to_owned)
                .collect(),
        )),
        RegType::Dword | RegType::DwordBigEndian => {
            parse_number(text, u64::from(u32::MAX)).map(|v| ValueData::U32(v as u32))
        }
        RegType::Qword => parse_number(text, u64::MAX).map(ValueData::U64),
        _ => parse_hex_bytes(text).map(ValueData::Bytes),
    };
    Ok((
        kind,
        data.map_err(|e| CoreError::Other(format!("{}: {e}", kind.name())))?,
    ))
}

// ---------- editable sources ----------

/// A loaded side that edits can be applied to: `.reg` files keep their
/// document; live keys only their tree.
pub struct EditableSource {
    pub spec: String,
    pub kind: SourceKind,
    doc: Option<RegDocument>,
    tree: RegTree,
}

impl EditableSource {
    /// A `.reg` file's raw bytes.
    pub fn file(spec: String, bytes: &[u8]) -> std::result::Result<Self, String> {
        let doc = RegDocument::parse(bytes)?;
        let tree = doc.tree()?;
        Ok(EditableSource {
            spec,
            kind: SourceKind::File,
            doc: Some(doc),
            tree,
        })
    }

    pub fn live(spec: String, kind: SourceKind, tree: RegTree) -> Self {
        EditableSource {
            spec,
            kind,
            doc: None,
            tree,
        }
    }

    pub fn tree(&self) -> &RegTree {
        &self.tree
    }

    /// The edited `.reg` document; `None` for live keys.
    pub fn document(&self) -> Option<&RegDocument> {
        self.doc.as_ref()
    }

    pub fn into_loaded(self) -> LoadedSource {
        LoadedSource {
            spec: self.spec,
            kind: self.kind,
            tree: self.tree,
        }
    }
}

/// Replays queued edits in order. Copies read the other side as it is at
/// that point (including its own earlier edits).
pub fn apply_edits(
    left: &mut EditableSource,
    right: &mut EditableSource,
    edits: &[SideEdit],
) -> Result<()> {
    for e in edits {
        match e.side {
            Side::Left => apply_one(left, right, &e.edit),
            Side::Right => apply_one(right, left, &e.edit),
        }?;
    }
    Ok(())
}

fn apply_one(target: &mut EditableSource, source: &EditableSource, edit: &RegEdit) -> Result<()> {
    let Some(doc) = target.doc.as_mut() else {
        return Err(CoreError::Other(format!(
            "{} is a live registry key; its changes are applied immediately, not queued",
            target.spec
        )));
    };
    match edit {
        RegEdit::SetValue {
            key,
            name,
            value_type,
            data,
        } => {
            let (kind, data) = parse_value_input(value_type, data)?;
            let value = RegValue {
                name: name.clone(),
                kind,
                data,
            };
            doc.set_value(&split_path(key)?, &value);
        }
        RegEdit::DeleteValue { key, name } => {
            doc.delete_value(&split_path(key)?, name);
        }
        RegEdit::AddKey { key } => doc.add_key(&split_path(key)?),
        RegEdit::DeleteKey { key } => {
            doc.delete_key(&split_path(key)?);
        }
        RegEdit::CopyKey { from, to } => {
            doc.replace_key(&split_path(to)?, source.tree.get(&split_path(from)?));
        }
        RegEdit::CopyValue {
            from_key,
            to_key,
            name,
        } => {
            let to = split_path(to_key)?;
            match source
                .tree
                .get(&split_path(from_key)?)
                .and_then(|k| k.value(name))
            {
                Some(v) => doc.set_value(&to, v),
                None => {
                    doc.delete_value(&to, name);
                }
            }
        }
    }
    target.tree = doc.tree().map_err(CoreError::Other)?;
    Ok(())
}

// ---------- live keys ----------

fn live_key(spec: &LiveSpec, path: &[String]) -> Result<String> {
    let root = canonical_root(&path[0])
        .ok_or_else(|| CoreError::Other(format!("\"{}\" is not a registry hive", path[0])))?;
    let mut full = vec![root.to_owned()];
    full.extend(path[1..].iter().cloned());
    Ok(LiveSpec {
        machine: spec.machine.clone(),
        path: full,
    }
    .reg_arg())
}

fn name_args(name: &str) -> Vec<String> {
    if name.is_empty() {
        vec!["/ve".into()]
    } else {
        vec!["/v".into(), name.to_owned()]
    }
}

/// Candidate separators for `reg add /s` with REG_MULTI_SZ data.
const MULTI_SEPARATORS: [char; 6] = ['|', '~', '^', '`', '#', '¦'];

/// `reg add` arguments writing `value` to `key`.
fn add_value_args(key: &str, value: &RegValue) -> Result<Vec<String>> {
    let mut args = vec!["add".to_owned(), key.to_owned()];
    args.extend(name_args(&value.name));
    let type_name = value.kind.name();
    let mut separator = None;
    let data = match (value.kind, &value.data) {
        (RegType::Sz | RegType::ExpandSz, ValueData::Str(s)) => s.clone(),
        (RegType::MultiSz, ValueData::Multi(items)) => {
            let sep = MULTI_SEPARATORS
                .into_iter()
                .find(|c| items.iter().all(|i| !i.contains(*c)))
                .ok_or_else(|| {
                    CoreError::Other(format!(
                        "\"{}\": every reg.exe separator character occurs in the data",
                        value.name
                    ))
                })?;
            separator = Some(sep.to_string());
            items.join(&sep.to_string())
        }
        (RegType::Dword | RegType::DwordBigEndian, ValueData::U32(v)) => v.to_string(),
        (RegType::Qword, ValueData::U64(v)) => v.to_string(),
        (RegType::Binary | RegType::None, ValueData::Bytes(b)) => {
            b.iter().map(|x| format!("{x:02X}")).collect()
        }
        _ => {
            return Err(CoreError::Other(format!(
                "\"{}\": reg.exe cannot write {type_name} values",
                value.name
            )))
        }
    };
    args.extend(["/t".to_owned(), type_name]);
    if let Some(sep) = separator {
        args.extend(["/s".to_owned(), sep]);
    }
    args.extend(["/d".to_owned(), data, "/f".to_owned()]);
    Ok(args)
}

fn delete_value_args(key: &str, name: &str) -> Vec<String> {
    let mut args = vec!["delete".to_owned(), key.to_owned()];
    args.extend(name_args(name));
    args.push("/f".into());
    args
}

/// Adds `key` and its values, then its children; `[-KEY]` markers (from a
/// `.reg` source) mean "absent" and are skipped.
fn copy_tree_args(
    spec: &LiveSpec,
    key: &RegKey,
    path: Vec<String>,
    out: &mut Vec<Vec<String>>,
) -> Result<()> {
    if !key.deleted {
        let arg = live_key(spec, &path)?;
        out.push(vec!["add".into(), arg.clone(), "/f".into()]);
        for v in key.values.iter().filter(|v| v.kind != RegType::Delete) {
            out.push(add_value_args(&arg, v)?);
        }
    }
    for child in key.children.values() {
        let mut child_path = path.clone();
        child_path.push(child.name.clone());
        copy_tree_args(spec, child, child_path, out)?;
    }
    Ok(())
}

/// The `reg.exe` invocations (argument lists, without `reg` itself) that
/// apply `edit` to the live side `spec`, whose current contents are
/// `target`; copies read from `source`, the other side.
pub fn live_plan(
    spec: &LiveSpec,
    target: &RegTree,
    source: &RegTree,
    edit: &RegEdit,
) -> Result<Vec<Vec<String>>> {
    let mut plan = Vec::new();
    match edit {
        RegEdit::SetValue {
            key,
            name,
            value_type,
            data,
        } => {
            let (kind, data) = parse_value_input(value_type, data)?;
            let value = RegValue {
                name: name.clone(),
                kind,
                data,
            };
            plan.push(add_value_args(&live_key(spec, &split_path(key)?)?, &value)?);
        }
        RegEdit::DeleteValue { key, name } => {
            plan.push(delete_value_args(&live_key(spec, &split_path(key)?)?, name));
        }
        RegEdit::AddKey { key } => {
            plan.push(vec![
                "add".into(),
                live_key(spec, &split_path(key)?)?,
                "/f".into(),
            ]);
        }
        RegEdit::DeleteKey { key } => {
            plan.push(vec![
                "delete".into(),
                live_key(spec, &split_path(key)?)?,
                "/f".into(),
            ]);
        }
        RegEdit::CopyKey { from, to } => {
            let to = split_path(to)?;
            if target.get(&to).is_some() {
                plan.push(vec!["delete".into(), live_key(spec, &to)?, "/f".into()]);
            }
            if let Some(src) = source.get(&split_path(from)?) {
                copy_tree_args(spec, src, to, &mut plan)?;
            }
        }
        RegEdit::CopyValue {
            from_key,
            to_key,
            name,
        } => {
            let to = split_path(to_key)?;
            let key = live_key(spec, &to)?;
            let src = source
                .get(&split_path(from_key)?)
                .and_then(|k| k.value(name))
                .filter(|v| v.kind != RegType::Delete);
            match src {
                Some(v) => plan.push(add_value_args(&key, v)?),
                None if target.get(&to).and_then(|k| k.value(name)).is_some() => {
                    plan.push(delete_value_args(&key, name))
                }
                None => {}
            }
        }
    }
    Ok(plan)
}

/// A `reg …` command line for showing a plan step to the user.
pub fn format_command(args: &[String]) -> String {
    let mut out = String::from("reg");
    for a in args {
        out.push(' ');
        if a.is_empty() || a.contains(char::is_whitespace) || a.contains('"') {
            out.push('"');
            out.push_str(&a.replace('"', "\\\""));
            out.push('"');
        } else {
            out.push_str(a);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::super::{compare_trees, parse_live_spec, parse_reg};
    use super::*;

    const LEFT: &str = "Windows Registry Editor Version 5.00\r\n\r\n\
        [HKEY_CURRENT_USER\\Software\\Vendor]\r\n\
        \"Name\"=\"left\"\r\n\
        \"Only\"=dword:00000002\r\n\r\n\
        [HKEY_CURRENT_USER\\Software\\Vendor\\Sub]\r\n\
        \"Deep\"=hex:01,02\r\n\r\n";
    const RIGHT: &str = "REGEDIT4\n\n[HKEY_CURRENT_USER\\Software\\Vendor]\n\"Name\"=\"right\"\n\n\
        [HKEY_CURRENT_USER\\Software\\Vendor\\Extra]\n\"x\"=\"1\"\n\n";

    fn sources() -> (EditableSource, EditableSource) {
        (
            EditableSource::file("left.reg".into(), LEFT.as_bytes()).unwrap(),
            EditableSource::file("right.reg".into(), RIGHT.as_bytes()).unwrap(),
        )
    }

    fn edit(side: Side, edit: RegEdit) -> SideEdit {
        SideEdit { side, edit }
    }

    const VENDOR: &str = "HKEY_CURRENT_USER\\Software\\Vendor";

    #[test]
    fn value_input() {
        assert_eq!(
            parse_value_input("REG_SZ", "a b").unwrap().1,
            ValueData::Str("a b".into())
        );
        assert_eq!(
            parse_value_input("reg_dword", "42").unwrap().1,
            ValueData::U32(42)
        );
        assert_eq!(
            parse_value_input("REG_DWORD", " 0x2A ").unwrap().1,
            ValueData::U32(42)
        );
        assert!(parse_value_input("REG_DWORD", "4294967296").is_err());
        assert!(parse_value_input("REG_DWORD", "-1").is_err());
        assert_eq!(
            parse_value_input("REG_QWORD", "0xffffffffffffffff")
                .unwrap()
                .1,
            ValueData::U64(u64::MAX)
        );
        assert_eq!(
            parse_value_input("REG_MULTI_SZ", "a\r\n\nb\n").unwrap().1,
            ValueData::Multi(vec!["a".into(), "b".into()])
        );
        assert_eq!(
            parse_value_input("REG_BINARY", "01 ab,CD\n0102").unwrap().1,
            ValueData::Bytes(vec![1, 0xab, 0xcd, 1, 2])
        );
        assert!(parse_value_input("REG_BINARY", "abc").is_err());
        assert!(parse_value_input("REG_BINARY", "zz").is_err());
        let (kind, data) = parse_value_input("hex(20)", "ff").unwrap();
        assert_eq!(
            (kind, data),
            (RegType::Other(0x20), ValueData::Bytes(vec![0xff]))
        );
        assert!(parse_value_input("REG_BOGUS", "").is_err());
        assert!(parse_value_input("(delete)", "").is_err());

        // The editor's starting text reads back to the same data.
        for data in [
            ValueData::U32(7),
            ValueData::U64(1 << 50),
            ValueData::Bytes(vec![0, 255, 16]),
            ValueData::Multi(vec!["x".into(), "y z".into()]),
        ] {
            let kind = match data {
                ValueData::U32(_) => "REG_DWORD",
                ValueData::U64(_) => "REG_QWORD",
                ValueData::Bytes(_) => "REG_BINARY",
                _ => "REG_MULTI_SZ",
            };
            assert_eq!(parse_value_input(kind, &data.edit_text()).unwrap().1, data);
        }
    }

    #[test]
    fn queued_edits_and_undo_by_replay() {
        let (mut l, mut r) = sources();
        let edits = vec![
            edit(
                Side::Left,
                RegEdit::SetValue {
                    key: VENDOR.into(),
                    name: "Name".into(),
                    value_type: "REG_SZ".into(),
                    data: "right".into(),
                },
            ),
            edit(
                Side::Left,
                RegEdit::DeleteValue {
                    key: VENDOR.into(),
                    name: "only".into(),
                },
            ),
            edit(
                Side::Right,
                RegEdit::AddKey {
                    key: format!("{VENDOR}\\Sub"),
                },
            ),
            edit(
                Side::Right,
                RegEdit::SetValue {
                    key: format!("{VENDOR}\\Sub"),
                    name: "Deep".into(),
                    value_type: "REG_BINARY".into(),
                    data: "01 02".into(),
                },
            ),
            edit(
                Side::Left,
                RegEdit::AddKey {
                    key: format!("{VENDOR}\\Extra"),
                },
            ),
            edit(
                Side::Left,
                RegEdit::SetValue {
                    key: format!("{VENDOR}\\Extra"),
                    name: "x".into(),
                    value_type: "REG_SZ".into(),
                    data: "1".into(),
                },
            ),
        ];
        apply_edits(&mut l, &mut r, &edits).unwrap();
        let diff = compare_trees(l.tree(), r.tree());
        assert_eq!(
            diff.stats.values.different
                + diff.stats.values.left_only
                + diff.stats.values.right_only,
            0
        );
        assert_eq!(diff.stats.keys.left_only + diff.stats.keys.right_only, 0);

        // Undo = replay one edit fewer on freshly loaded sources.
        let (mut l, mut r) = sources();
        apply_edits(&mut l, &mut r, &edits[..5]).unwrap();
        assert_eq!(compare_trees(l.tree(), r.tree()).stats.values.right_only, 1);

        // The saved bytes keep each file's format.
        let left_bytes = l.document().unwrap().to_bytes().unwrap();
        assert!(String::from_utf8(left_bytes)
            .unwrap()
            .contains("\r\n[HKEY_CURRENT_USER\\Software\\Vendor\\Extra]\r\n"));
        assert!(r.document().unwrap().is_v4());
    }

    #[test]
    fn copy_key_and_value_between_sides() {
        let (mut l, mut r) = sources();
        let edits = vec![
            // Edit the left first: the copy must see it.
            edit(
                Side::Left,
                RegEdit::SetValue {
                    key: format!("{VENDOR}\\Sub"),
                    name: "Added".into(),
                    value_type: "REG_EXPAND_SZ".into(),
                    data: "%TEMP%".into(),
                },
            ),
            edit(
                Side::Right,
                RegEdit::CopyKey {
                    from: VENDOR.into(),
                    to: VENDOR.into(),
                },
            ),
        ];
        apply_edits(&mut l, &mut r, &edits).unwrap();
        let diff = compare_trees(l.tree(), r.tree());
        assert_eq!(
            diff.roots[0].status,
            crate::DiffStatus::Same,
            "{:?}",
            diff.stats
        );
        let text = r.document().unwrap().to_text();
        assert!(!text.contains("Extra"), "{text}");
        assert!(
            text.contains("\"Added\"=hex(2):25,54,45,4d,50,25,00\n"),
            "ANSI for REGEDIT4: {text}"
        );

        // Copying a value the source lacks deletes it on the target.
        let (mut l, mut r) = sources();
        let edits = vec![
            edit(
                Side::Right,
                RegEdit::CopyValue {
                    from_key: VENDOR.into(),
                    to_key: VENDOR.into(),
                    name: "Only".into(),
                },
            ),
            edit(
                Side::Left,
                RegEdit::CopyValue {
                    from_key: format!("{VENDOR}\\Extra"),
                    to_key: format!("{VENDOR}\\Sub"),
                    name: "Deep".into(),
                },
            ),
            edit(
                Side::Left,
                RegEdit::CopyKey {
                    from: format!("{VENDOR}\\Missing"),
                    to: format!("{VENDOR}\\Sub"),
                },
            ),
        ];
        apply_edits(&mut l, &mut r, &edits).unwrap();
        let vendor: Vec<String> = VENDOR.split('\\').map(str::to_owned).collect();
        assert_eq!(
            r.tree().get(&vendor).unwrap().value("only").unwrap().data,
            ValueData::U32(2)
        );
        assert!(l.tree().get(&vendor).unwrap().child("sub").is_none());
    }

    #[test]
    fn live_sides_reject_queued_edits() {
        let (_, mut r) = sources();
        let mut live = EditableSource::live(
            "HKCU\\Software".into(),
            SourceKind::Live,
            RegTree::default(),
        );
        let err = apply_edits(
            &mut live,
            &mut r,
            &[edit(
                Side::Left,
                RegEdit::AddKey {
                    key: "HKCU\\Software\\X".into(),
                },
            )],
        )
        .unwrap_err();
        assert!(err.to_string().contains("applied immediately"));
    }

    #[test]
    fn live_plans() {
        let spec = parse_live_spec("HKCU\\Software\\Vendor").unwrap();
        let target = parse_reg(RIGHT).unwrap();
        let source = parse_reg(LEFT).unwrap();
        let plan = |e: RegEdit| {
            live_plan(&spec, &target, &source, &e)
                .unwrap()
                .iter()
                .map(|a| format_command(a))
                .collect::<Vec<_>>()
        };

        assert_eq!(
            plan(RegEdit::SetValue {
                key: VENDOR.into(),
                name: String::new(),
                value_type: "REG_SZ".into(),
                data: "C:\\Program Files\\".into(),
            }),
            vec!["reg add HKCU\\Software\\Vendor /ve /t REG_SZ /d \"C:\\Program Files\\\" /f"]
        );
        assert_eq!(
            plan(RegEdit::SetValue {
                key: VENDOR.into(),
                name: "Flags".into(),
                value_type: "REG_DWORD".into(),
                data: "0x10".into(),
            }),
            vec!["reg add HKCU\\Software\\Vendor /v Flags /t REG_DWORD /d 16 /f"]
        );
        assert_eq!(
            plan(RegEdit::SetValue {
                key: VENDOR.into(),
                name: "List".into(),
                value_type: "REG_MULTI_SZ".into(),
                data: "a|b\nc".into(),
            }),
            vec!["reg add HKCU\\Software\\Vendor /v List /t REG_MULTI_SZ /s ~ /d a|b~c /f"]
        );
        assert_eq!(
            plan(RegEdit::DeleteKey {
                key: format!("{VENDOR}\\Extra"),
            }),
            vec!["reg delete HKCU\\Software\\Vendor\\Extra /f"]
        );
        assert_eq!(
            plan(RegEdit::CopyKey {
                from: VENDOR.into(),
                to: VENDOR.into(),
            }),
            vec![
                "reg delete HKCU\\Software\\Vendor /f",
                "reg add HKCU\\Software\\Vendor /f",
                "reg add HKCU\\Software\\Vendor /v Name /t REG_SZ /d left /f",
                "reg add HKCU\\Software\\Vendor /v Only /t REG_DWORD /d 2 /f",
                "reg add HKCU\\Software\\Vendor\\Sub /f",
                "reg add HKCU\\Software\\Vendor\\Sub /v Deep /t REG_BINARY /d 0102 /f",
            ]
        );
        // The target has no "Only" and the source has no "Missing": nothing to do.
        assert!(plan(RegEdit::CopyValue {
            from_key: VENDOR.into(),
            to_key: VENDOR.into(),
            name: "Missing".into(),
        })
        .is_empty());
        assert_eq!(
            plan(RegEdit::CopyValue {
                from_key: format!("{VENDOR}\\Sub"),
                to_key: VENDOR.into(),
                name: "Name".into(),
            }),
            vec!["reg delete HKCU\\Software\\Vendor /v Name /f"]
        );

        let remote = parse_live_spec("\\\\PC01\\HKLM\\Software").unwrap();
        let p = live_plan(
            &remote,
            &target,
            &source,
            &RegEdit::AddKey {
                key: "HKEY_LOCAL_MACHINE\\Software\\New Key".into(),
            },
        )
        .unwrap();
        assert_eq!(
            format_command(&p[0]),
            "reg add \"\\\\PC01\\HKLM\\Software\\New Key\" /f"
        );

        let unsupported = live_plan(
            &spec,
            &target,
            &source,
            &RegEdit::SetValue {
                key: VENDOR.into(),
                name: "R".into(),
                value_type: "REG_RESOURCE_LIST".into(),
                data: "00".into(),
            },
        );
        assert!(unsupported
            .unwrap_err()
            .to_string()
            .contains("cannot write"));
    }
}
