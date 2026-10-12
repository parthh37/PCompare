//! Registry comparison: `.reg` exports, live local keys and remote machines.
//!
//! Every source is parsed into a [`RegTree`]; two trees are then aligned key
//! by key (case-insensitively, as the registry does) into a [`RegKeyDiff`]
//! tree with per-value differences.

mod live;
mod reg_file;
mod reg_query;

pub use live::load_live;
pub use reg_file::parse_reg;
pub use reg_query::parse_reg_query;

use crate::property::DiffStatus;
use pcompare_core::{CoreError, Result};
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::Path;

/// At most this many parse warnings are kept per source.
const MAX_WARNINGS: usize = 50;
/// Binary values longer than this are truncated for display (never for comparison).
const MAX_BINARY_DISPLAY: usize = 1024;

// ---------- values ----------

/// Registry value types, numbered as in `winnt.h` (`hex(N)` in `.reg` files).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegType {
    None,
    Sz,
    ExpandSz,
    Binary,
    Dword,
    DwordBigEndian,
    Link,
    MultiSz,
    ResourceList,
    FullResourceDescriptor,
    ResourceRequirementsList,
    Qword,
    Other(u32),
    /// `"name"=-` in a `.reg` file: the value is to be removed.
    Delete,
}

impl RegType {
    pub fn from_code(code: u32) -> Self {
        match code {
            0 => RegType::None,
            1 => RegType::Sz,
            2 => RegType::ExpandSz,
            3 => RegType::Binary,
            4 => RegType::Dword,
            5 => RegType::DwordBigEndian,
            6 => RegType::Link,
            7 => RegType::MultiSz,
            8 => RegType::ResourceList,
            9 => RegType::FullResourceDescriptor,
            10 => RegType::ResourceRequirementsList,
            11 => RegType::Qword,
            n => RegType::Other(n),
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "REG_NONE" => RegType::None,
            "REG_SZ" => RegType::Sz,
            "REG_EXPAND_SZ" => RegType::ExpandSz,
            "REG_BINARY" => RegType::Binary,
            "REG_DWORD" => RegType::Dword,
            "REG_DWORD_BIG_ENDIAN" => RegType::DwordBigEndian,
            "REG_LINK" => RegType::Link,
            "REG_MULTI_SZ" => RegType::MultiSz,
            "REG_RESOURCE_LIST" => RegType::ResourceList,
            "REG_FULL_RESOURCE_DESCRIPTOR" => RegType::FullResourceDescriptor,
            "REG_RESOURCE_REQUIREMENTS_LIST" => RegType::ResourceRequirementsList,
            "REG_QWORD" => RegType::Qword,
            _ => return None,
        })
    }

    pub fn name(self) -> String {
        match self {
            RegType::None => "REG_NONE".into(),
            RegType::Sz => "REG_SZ".into(),
            RegType::ExpandSz => "REG_EXPAND_SZ".into(),
            RegType::Binary => "REG_BINARY".into(),
            RegType::Dword => "REG_DWORD".into(),
            RegType::DwordBigEndian => "REG_DWORD_BIG_ENDIAN".into(),
            RegType::Link => "REG_LINK".into(),
            RegType::MultiSz => "REG_MULTI_SZ".into(),
            RegType::ResourceList => "REG_RESOURCE_LIST".into(),
            RegType::FullResourceDescriptor => "REG_FULL_RESOURCE_DESCRIPTOR".into(),
            RegType::ResourceRequirementsList => "REG_RESOURCE_REQUIREMENTS_LIST".into(),
            RegType::Qword => "REG_QWORD".into(),
            RegType::Other(n) => format!("hex({n:x})"),
            RegType::Delete => "(delete)".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValueData {
    Str(String),
    Multi(Vec<String>),
    U32(u32),
    U64(u64),
    Bytes(Vec<u8>),
}

impl ValueData {
    /// Interprets raw value bytes the way regedit would display them.
    /// `utf16` is false for REGEDIT4 files, whose strings are ANSI.
    pub fn from_bytes(kind: RegType, bytes: Vec<u8>, utf16: bool) -> Self {
        let text = |b: &[u8]| -> String {
            if utf16 {
                let units: Vec<u16> = b
                    .chunks_exact(2)
                    .map(|c| u16::from_le_bytes([c[0], c[1]]))
                    .collect();
                String::from_utf16_lossy(&units)
            } else {
                pcompare_core::textfile::decode(b).0
            }
        };
        match kind {
            RegType::Sz | RegType::ExpandSz | RegType::Link => {
                ValueData::Str(text(&bytes).trim_end_matches('\0').to_owned())
            }
            RegType::MultiSz => {
                let s = text(&bytes);
                let mut items: Vec<String> = s.split('\0').map(str::to_owned).collect();
                while items.last().is_some_and(String::is_empty) {
                    items.pop();
                }
                ValueData::Multi(items)
            }
            RegType::Dword if bytes.len() == 4 => {
                ValueData::U32(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
            }
            RegType::DwordBigEndian if bytes.len() == 4 => {
                ValueData::U32(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
            }
            RegType::Qword if bytes.len() == 8 => {
                let mut b = [0u8; 8];
                b.copy_from_slice(&bytes);
                ValueData::U64(u64::from_le_bytes(b))
            }
            _ => ValueData::Bytes(bytes),
        }
    }

    pub fn display(&self) -> String {
        match self {
            ValueData::Str(s) => s.clone(),
            ValueData::Multi(items) => items.join(" | "),
            ValueData::U32(v) => format!("0x{v:08x} ({v})"),
            ValueData::U64(v) => format!("0x{v:016x} ({v})"),
            ValueData::Bytes(b) => {
                let shown = &b[..b.len().min(MAX_BINARY_DISPLAY)];
                let mut s = shown
                    .iter()
                    .map(|x| format!("{x:02x}"))
                    .collect::<Vec<_>>()
                    .join(" ");
                if b.len() > shown.len() {
                    s.push_str(&format!(" … ({} bytes)", b.len()));
                }
                s
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegValue {
    /// Empty for the key's default value (`@`).
    pub name: String,
    pub kind: RegType,
    pub data: ValueData,
}

impl RegValue {
    fn display(&self) -> String {
        if self.kind == RegType::Delete {
            String::new()
        } else {
            self.data.display()
        }
    }
}

// ---------- keys ----------

#[derive(Debug, Clone, Default)]
pub struct RegKey {
    /// Original-case name as written in the source.
    pub name: String,
    /// `[-KEY]`: the key is to be removed.
    pub deleted: bool,
    /// True when the source names this key itself (not just a descendant).
    pub declared: bool,
    values: Vec<RegValue>,
    children: BTreeMap<String, RegKey>,
}

impl RegKey {
    fn new(name: &str) -> Self {
        RegKey {
            name: name.to_owned(),
            ..Default::default()
        }
    }

    pub fn values(&self) -> &[RegValue] {
        &self.values
    }

    pub fn children(&self) -> impl Iterator<Item = &RegKey> {
        self.children.values()
    }

    pub fn child(&self, name: &str) -> Option<&RegKey> {
        self.children.get(&name.to_lowercase())
    }

    /// Later assignments to the same (case-insensitive) name win, as on import.
    pub fn set_value(&mut self, value: RegValue) {
        let lower = value.name.to_lowercase();
        match self
            .values
            .iter_mut()
            .find(|v| v.name.to_lowercase() == lower)
        {
            Some(existing) => *existing = value,
            None => self.values.push(value),
        }
    }

    fn count(&self) -> (usize, usize) {
        self.children
            .values()
            .fold((1, self.values.len()), |(k, v), c| {
                let (ck, cv) = c.count();
                (k + ck, v + cv)
            })
    }
}

/// A parsed registry source. The root is virtual; its children are hives.
#[derive(Debug, Clone, Default)]
pub struct RegTree {
    root: RegKey,
    pub warnings: Vec<String>,
}

impl RegTree {
    pub fn root(&self) -> &RegKey {
        &self.root
    }

    /// Returns the key at `path`, creating missing keys on the way.
    pub fn key_mut(&mut self, path: &[&str]) -> &mut RegKey {
        let mut node = &mut self.root;
        for (i, part) in path.iter().enumerate() {
            let part = if i == 0 {
                canonical_root(part).unwrap_or(part)
            } else {
                part
            };
            node = node
                .children
                .entry(part.to_lowercase())
                .or_insert_with(|| RegKey::new(part));
        }
        node
    }

    pub fn get(&self, path: &[String]) -> Option<&RegKey> {
        let mut node = &self.root;
        for (i, part) in path.iter().enumerate() {
            let part = if i == 0 {
                canonical_root(part).unwrap_or(part)
            } else {
                part
            };
            node = node.child(part)?;
        }
        Some(node)
    }

    pub fn warn(&mut self, message: String) {
        if self.warnings.len() < MAX_WARNINGS {
            self.warnings.push(message);
        }
    }

    /// The topmost key the source is "about": the end of the chain of
    /// single-child ancestors that the source never names itself.
    pub fn base_path(&self) -> Vec<String> {
        let mut path = Vec::new();
        let mut node = &self.root;
        while !node.declared && node.values.is_empty() && node.children.len() == 1 {
            node = node.children.values().next().expect("one child");
            path.push(node.name.clone());
        }
        path
    }

    /// `(keys, values)` below the virtual root.
    pub fn counts(&self) -> (usize, usize) {
        let (k, v) = self.root.count();
        (k - 1, v)
    }
}

// ---------- key specs ----------

const ROOTS: &[(&str, &str)] = &[
    ("HKLM", "HKEY_LOCAL_MACHINE"),
    ("HKCU", "HKEY_CURRENT_USER"),
    ("HKCR", "HKEY_CLASSES_ROOT"),
    ("HKU", "HKEY_USERS"),
    ("HKCC", "HKEY_CURRENT_CONFIG"),
];

/// Expands `HKLM` etc. and canonicalises the case of full hive names.
pub fn canonical_root(name: &str) -> Option<&'static str> {
    ROOTS
        .iter()
        .find(|(short, long)| name.eq_ignore_ascii_case(short) || name.eq_ignore_ascii_case(long))
        .map(|(_, long)| *long)
}

fn short_root(long: &str) -> &str {
    ROOTS
        .iter()
        .find(|(_, l)| *l == long)
        .map_or(long, |(s, _)| s)
}

/// A live key such as `HKLM\Software\Vendor` or `\\PC01\HKLM\Software`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveSpec {
    pub machine: Option<String>,
    /// Canonical full hive name followed by the subkey path.
    pub path: Vec<String>,
}

impl LiveSpec {
    /// The key as `reg.exe` expects it: abbreviated hive, optional `\\machine\`.
    pub fn reg_arg(&self) -> String {
        let mut parts = self.path.clone();
        parts[0] = short_root(&parts[0]).to_owned();
        let key = parts.join("\\");
        match &self.machine {
            Some(m) => format!("\\\\{m}\\{key}"),
            None => key,
        }
    }

    pub fn display(&self) -> String {
        let key = self.path.join("\\");
        match &self.machine {
            Some(m) => format!("\\\\{m}\\{key}"),
            None => key,
        }
    }
}

/// Recognises live key specs. Regedit's address-bar form
/// (`Computer\HKEY_LOCAL_MACHINE\...`) is accepted too.
pub fn parse_live_spec(spec: &str) -> Option<LiveSpec> {
    let s = spec.trim().trim_end_matches('\\');
    let (machine, rest) = match s.strip_prefix("\\\\") {
        Some(r) => {
            let (m, rest) = r.split_once('\\')?;
            (Some(m.to_owned()), rest)
        }
        None => (None, s),
    };
    let mut parts: Vec<&str> = rest.split('\\').filter(|p| !p.is_empty()).collect();
    if machine.is_none() && parts.len() > 1 && parts[0].eq_ignore_ascii_case("Computer") {
        parts.remove(0);
    }
    let root = canonical_root(parts.first()?)?;
    let mut path = vec![root.to_owned()];
    path.extend(parts[1..].iter().map(|p| p.to_string()));
    Some(LiveSpec { machine, path })
}

// ---------- loading ----------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum SourceKind {
    File,
    Live,
    Remote,
}

/// Returns the live key a spec names, or `None` when it should be read as a
/// `.reg` file. An existing file always wins over the key interpretation.
pub fn classify_source(spec: &str) -> Option<LiveSpec> {
    if Path::new(spec.trim()).exists() {
        return None;
    }
    parse_live_spec(spec)
}

pub fn load_file(path: &Path) -> Result<RegTree> {
    let bytes = std::fs::read(path).map_err(|e| CoreError::io(path, e))?;
    let (text, _, _) = pcompare_core::textfile::decode(&bytes);
    parse_reg(&text).map_err(|e| CoreError::Other(format!("{}: {e}", path.display())))
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceInfo {
    pub spec: String,
    pub kind: SourceKind,
    pub keys: usize,
    pub values: usize,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RegistryCompareResult {
    #[serde(flatten)]
    pub diff: RegistryDiff,
    pub left: SourceInfo,
    pub right: SourceInfo,
}

/// A loaded side: what the user typed, how it was read, and the result.
pub struct LoadedSource {
    pub spec: String,
    pub kind: SourceKind,
    pub tree: RegTree,
}

impl LoadedSource {
    fn info(self) -> SourceInfo {
        let (keys, values) = self.tree.counts();
        SourceInfo {
            spec: self.spec,
            kind: self.kind,
            keys,
            values,
            warnings: self.tree.warnings,
        }
    }
}

pub fn compare_sources(left: LoadedSource, right: LoadedSource) -> RegistryCompareResult {
    let diff = compare_trees(&left.tree, &right.tree);
    RegistryCompareResult {
        diff,
        left: left.info(),
        right: right.info(),
    }
}

// ---------- comparison ----------

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RegValueDiff {
    /// Empty for the default value.
    pub name: String,
    pub left_type: Option<String>,
    pub right_type: Option<String>,
    pub left: Option<String>,
    pub right: Option<String>,
    pub status: DiffStatus,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RegKeyDiff {
    pub name: String,
    /// Lower-case path from the compare root; unique within the result.
    pub id: String,
    /// Status of the whole subtree.
    pub status: DiffStatus,
    /// This key's own values (or deletion marker) differ, or it is an orphan.
    pub values_differ: bool,
    pub left_deleted: bool,
    pub right_deleted: bool,
    pub values: Vec<RegValueDiff>,
    pub children: Vec<RegKeyDiff>,
}

#[derive(Debug, Clone, Copy, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffCounts {
    pub same: usize,
    pub different: usize,
    pub left_only: usize,
    pub right_only: usize,
}

impl DiffCounts {
    fn add(&mut self, status: DiffStatus) {
        match status {
            DiffStatus::Same => self.same += 1,
            DiffStatus::Different => self.different += 1,
            DiffStatus::LeftOnly => self.left_only += 1,
            DiffStatus::RightOnly => self.right_only += 1,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RegistryStats {
    /// Keys by their own status (descendants are counted separately).
    pub keys: DiffCounts,
    pub values: DiffCounts,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RegistryDiff {
    pub roots: Vec<RegKeyDiff>,
    pub stats: RegistryStats,
    /// The key each side was aligned at, e.g. `HKEY_CURRENT_USER\Software\Vendor`.
    pub left_base: String,
    pub right_base: String,
}

fn same_path(a: &[String], b: &[String]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.eq_ignore_ascii_case(y))
}

fn is_prefix(prefix: &[String], path: &[String]) -> bool {
    prefix.len() <= path.len() && same_path(prefix, &path[..prefix.len()])
}

/// Aligns two trees. Each side is rooted at its [`RegTree::base_path`]; when
/// one base contains the other both use the shorter one, so comparing an
/// export of `Software\Vendor` with one of `Software` still lines up.
pub fn compare_trees(left: &RegTree, right: &RegTree) -> RegistryDiff {
    let (mut lb, mut rb) = (left.base_path(), right.base_path());
    if is_prefix(&lb, &rb) {
        rb.truncate(lb.len());
    } else if is_prefix(&rb, &lb) {
        lb.truncate(rb.len());
    }
    // `get` cannot fail: base paths come from the trees themselves.
    let lnode = left.get(&lb);
    let rnode = right.get(&rb);
    let mut stats = RegistryStats::default();

    let roots = if lb.is_empty() {
        diff_key(lnode, rnode, String::new(), String::new(), &mut stats).children
    } else {
        let name = lb.join("\\");
        let id = name.to_lowercase();
        vec![diff_key(lnode, rnode, name, id, &mut stats)]
    };
    RegistryDiff {
        roots,
        stats,
        left_base: lb.join("\\"),
        right_base: rb.join("\\"),
    }
}

fn diff_values(
    l: Option<&RegKey>,
    r: Option<&RegKey>,
    stats: &mut RegistryStats,
) -> Vec<RegValueDiff> {
    // Default value first, then case-insensitive name order (as regedit shows them).
    let mut merged: BTreeMap<String, (Option<&RegValue>, Option<&RegValue>)> = BTreeMap::new();
    for v in l.map_or(&[][..], |k| &k.values) {
        merged.entry(v.name.to_lowercase()).or_default().0 = Some(v);
    }
    for v in r.map_or(&[][..], |k| &k.values) {
        merged.entry(v.name.to_lowercase()).or_default().1 = Some(v);
    }
    merged
        .into_values()
        .map(|(lv, rv)| {
            let status = match (lv, rv) {
                (Some(a), Some(b)) if a.kind == b.kind && a.data == b.data => DiffStatus::Same,
                (Some(_), Some(_)) => DiffStatus::Different,
                (Some(_), None) => DiffStatus::LeftOnly,
                _ => DiffStatus::RightOnly,
            };
            stats.values.add(status);
            RegValueDiff {
                name: lv.or(rv).map(|v| v.name.clone()).unwrap_or_default(),
                left_type: lv.map(|v| v.kind.name()),
                right_type: rv.map(|v| v.kind.name()),
                left: lv.map(RegValue::display),
                right: rv.map(RegValue::display),
                status,
            }
        })
        .collect()
}

fn diff_key(
    l: Option<&RegKey>,
    r: Option<&RegKey>,
    name: String,
    id: String,
    stats: &mut RegistryStats,
) -> RegKeyDiff {
    let values = diff_values(l, r, stats);

    let mut names: BTreeMap<&str, (Option<&RegKey>, Option<&RegKey>)> = BTreeMap::new();
    for (k, c) in l.into_iter().flat_map(|k| &k.children) {
        names.entry(k).or_default().0 = Some(c);
    }
    for (k, c) in r.into_iter().flat_map(|k| &k.children) {
        names.entry(k).or_default().1 = Some(c);
    }
    let children: Vec<RegKeyDiff> = names
        .into_iter()
        .map(|(lower, (lc, rc))| {
            let child_name = lc.or(rc).map(|c| c.name.clone()).unwrap_or_default();
            let child_id = if id.is_empty() {
                lower.to_owned()
            } else {
                format!("{id}\\{lower}")
            };
            diff_key(lc, rc, child_name, child_id, stats)
        })
        .collect();

    let left_deleted = l.is_some_and(|k| k.deleted);
    let right_deleted = r.is_some_and(|k| k.deleted);
    let (status, values_differ) = match (l, r) {
        (Some(_), None) => (DiffStatus::LeftOnly, true),
        (None, Some(_)) => (DiffStatus::RightOnly, true),
        _ => {
            let own = left_deleted != right_deleted
                || values.iter().any(|v| v.status != DiffStatus::Same);
            let subtree = own || children.iter().any(|c| c.status != DiffStatus::Same);
            (
                if subtree {
                    DiffStatus::Different
                } else {
                    DiffStatus::Same
                },
                own,
            )
        }
    };
    // The virtual root (empty id) is not a real key.
    if !id.is_empty() {
        stats.keys.add(match status {
            DiffStatus::Different if !values_differ => DiffStatus::Same,
            s => s,
        });
    }
    RegKeyDiff {
        name,
        id,
        status,
        values_differ,
        left_deleted,
        right_deleted,
        values,
        children,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(text: &str) -> RegTree {
        parse_reg(text).unwrap()
    }

    #[test]
    fn live_specs() {
        let s = parse_live_spec("hklm\\Software\\Vendor\\").unwrap();
        assert_eq!(s.machine, None);
        assert_eq!(s.path, vec!["HKEY_LOCAL_MACHINE", "Software", "Vendor"]);
        assert_eq!(s.reg_arg(), "HKLM\\Software\\Vendor");

        let s = parse_live_spec("\\\\PC01\\HKEY_USERS\\.DEFAULT").unwrap();
        assert_eq!(s.machine.as_deref(), Some("PC01"));
        assert_eq!(s.reg_arg(), "\\\\PC01\\HKU\\.DEFAULT");
        assert_eq!(s.display(), "\\\\PC01\\HKEY_USERS\\.DEFAULT");

        let s = parse_live_spec("Computer\\HKEY_CURRENT_USER\\Console").unwrap();
        assert_eq!(s.path, vec!["HKEY_CURRENT_USER", "Console"]);
        assert_eq!(
            parse_live_spec("HKCC").unwrap().path,
            vec!["HKEY_CURRENT_CONFIG"]
        );

        assert!(parse_live_spec("C:\\exports\\a.reg").is_none());
        assert!(parse_live_spec("/tmp/a.reg").is_none());
        assert!(parse_live_spec("").is_none());
        assert!(parse_live_spec("\\\\server\\share\\file.reg").is_none());
    }

    #[test]
    fn compares_trees() {
        let left = tree(
            "Windows Registry Editor Version 5.00\r\n\r\n\
             [HKEY_CURRENT_USER\\Software\\Vendor]\r\n\
             @=\"root\"\r\n\
             \"Same\"=dword:00000001\r\n\
             \"Changed\"=\"old\"\r\n\
             \"LeftOnly\"=\"x\"\r\n\r\n\
             [HKEY_CURRENT_USER\\Software\\Vendor\\Common]\r\n\
             \"A\"=\"1\"\r\n\r\n\
             [HKEY_CURRENT_USER\\Software\\Vendor\\Gone]\r\n",
        );
        let right = tree(
            "Windows Registry Editor Version 5.00\r\n\r\n\
             [HKEY_CURRENT_USER\\SOFTWARE\\vendor]\r\n\
             @=\"root\"\r\n\
             \"same\"=dword:00000001\r\n\
             \"Changed\"=\"new\"\r\n\r\n\
             [HKEY_CURRENT_USER\\SOFTWARE\\vendor\\COMMON]\r\n\
             \"a\"=\"1\"\r\n\r\n\
             [HKEY_CURRENT_USER\\SOFTWARE\\vendor\\Added]\r\n\
             \"B\"=hex(b):2a,00,00,00,00,00,00,00\r\n",
        );
        let diff = compare_trees(&left, &right);
        assert_eq!(diff.left_base, "HKEY_CURRENT_USER\\Software\\Vendor");
        assert_eq!(diff.right_base, "HKEY_CURRENT_USER\\SOFTWARE\\vendor");
        assert_eq!(diff.roots.len(), 1);
        let root = &diff.roots[0];
        assert_eq!(root.status, DiffStatus::Different);
        assert!(root.values_differ);

        let vals: Vec<_> = root
            .values
            .iter()
            .map(|v| (v.name.as_str(), v.status))
            .collect();
        assert_eq!(
            vals,
            vec![
                ("", DiffStatus::Same),
                ("Changed", DiffStatus::Different),
                ("LeftOnly", DiffStatus::LeftOnly),
                ("Same", DiffStatus::Same),
            ]
        );

        let kids: Vec<_> = root
            .children
            .iter()
            .map(|c| (c.name.as_str(), c.status))
            .collect();
        assert_eq!(
            kids,
            vec![
                ("Added", DiffStatus::RightOnly),
                ("Common", DiffStatus::Same),
                ("Gone", DiffStatus::LeftOnly)
            ]
        );
        let added = &root.children[0];
        assert_eq!(added.id, "hkey_current_user\\software\\vendor\\added");
        assert_eq!(added.values[0].right_type.as_deref(), Some("REG_QWORD"));
        assert_eq!(
            added.values[0].right.as_deref(),
            Some("0x000000000000002a (42)")
        );

        assert_eq!(diff.stats.keys.same, 1); // Common
        assert_eq!(diff.stats.keys.different, 1); // Vendor
        assert_eq!(diff.stats.keys.left_only, 1);
        assert_eq!(diff.stats.keys.right_only, 1);
        assert_eq!(diff.stats.values.same, 3);
        assert_eq!(diff.stats.values.different, 1);
        assert_eq!(diff.stats.values.left_only, 1);
        assert_eq!(diff.stats.values.right_only, 1);
    }

    #[test]
    fn rebases_on_containing_export() {
        let left = tree("REGEDIT4\n\n[HKEY_LOCAL_MACHINE\\SOFTWARE\\A\\B]\n\"v\"=\"1\"\n");
        let right = tree(
            "REGEDIT4\n\n[HKEY_LOCAL_MACHINE\\SOFTWARE\\A]\n\n[HKEY_LOCAL_MACHINE\\SOFTWARE\\A\\B]\n\"v\"=\"1\"\n",
        );
        let diff = compare_trees(&left, &right);
        assert_eq!(diff.left_base, "HKEY_LOCAL_MACHINE\\SOFTWARE\\A");
        assert_eq!(diff.right_base, "HKEY_LOCAL_MACHINE\\SOFTWARE\\A");
        assert_eq!(diff.roots[0].children[0].status, DiffStatus::Same);
    }

    #[test]
    fn different_bases_compare_subtrees() {
        let left = tree("REGEDIT4\n\n[HKEY_LOCAL_MACHINE\\SOFTWARE\\One]\n\"v\"=\"1\"\n");
        let right = tree("REGEDIT4\n\n[HKEY_LOCAL_MACHINE\\SOFTWARE\\Two]\n\"v\"=\"1\"\n");
        let diff = compare_trees(&left, &right);
        assert_eq!(diff.roots.len(), 1);
        assert_eq!(diff.roots[0].status, DiffStatus::Same);
        assert_eq!(diff.right_base, "HKEY_LOCAL_MACHINE\\SOFTWARE\\Two");
    }

    #[test]
    fn multiple_hives_have_no_common_base() {
        let left = tree("REGEDIT4\n\n[HKEY_CURRENT_USER\\A]\n\n[HKEY_LOCAL_MACHINE\\B]\n");
        let right = tree("REGEDIT4\n\n[HKEY_LOCAL_MACHINE\\B]\n");
        let diff = compare_trees(&left, &right);
        let roots: Vec<_> = diff
            .roots
            .iter()
            .map(|r| (r.name.as_str(), r.status))
            .collect();
        assert_eq!(
            roots,
            vec![
                ("HKEY_CURRENT_USER", DiffStatus::LeftOnly),
                ("HKEY_LOCAL_MACHINE", DiffStatus::Same)
            ]
        );
    }

    #[test]
    fn deletion_markers_differ() {
        let left = tree("REGEDIT4\n\n[HKEY_CURRENT_USER\\A]\n\n[-HKEY_CURRENT_USER\\A\\B]\n");
        let right = tree("REGEDIT4\n\n[HKEY_CURRENT_USER\\A]\n\n[HKEY_CURRENT_USER\\A\\B]\n");
        let diff = compare_trees(&left, &right);
        let b = &diff.roots[0].children[0];
        assert!(b.left_deleted && !b.right_deleted);
        assert_eq!(b.status, DiffStatus::Different);
    }

    #[test]
    fn value_display() {
        assert_eq!(ValueData::U32(42).display(), "0x0000002a (42)");
        assert_eq!(ValueData::Bytes(vec![1, 0xab]).display(), "01 ab");
        assert_eq!(
            ValueData::Multi(vec!["a".into(), "b".into()]).display(),
            "a | b"
        );
        let long = ValueData::Bytes(vec![0; MAX_BINARY_DISPLAY + 1]).display();
        assert!(long.ends_with(&format!("… ({} bytes)", MAX_BINARY_DISPLAY + 1)));
    }

    #[test]
    fn counts() {
        let t = tree("REGEDIT4\n\n[HKEY_CURRENT_USER\\A]\n\"x\"=\"1\"\n\"y\"=\"2\"\n\n[HKEY_CURRENT_USER\\A\\B]\n");
        // HKEY_CURRENT_USER, A, B
        assert_eq!(t.counts(), (3, 2));
    }
}
