//! FreeFileSync configurations (`.ffs_gui`, `.ffs_batch`): every folder pair
//! becomes a Folder Sync session.
//!
//! ```xml
//! <FreeFileSync XmlType="GUI" XmlFormat="23">
//!   <Compare><Variant>TimeAndSize</Variant></Compare>
//!   <Synchronize>
//!     <Differences LeftOnly="right" LeftNewer="right" RightNewer="right" RightOnly="right"/>
//!     <DeletionPolicy>RecycleBin</DeletionPolicy>
//!   </Synchronize>
//!   <Filter><Include><Item>*</Item></Include><Exclude><Item>*\thumbs.db</Item></Exclude></Filter>
//!   <FolderPairs><Pair><Left>D:\src</Left><Right>\\nas\src</Right></Pair></FolderPairs>
//! </FreeFileSync>
//! ```
//!
//! XmlFormat 21+ (FreeFileSync 13+) stores sync directions as `Differences`
//! or `Changes`; older files name a `Variant`. A pair may override the
//! global `Compare`, `Synchronize` and (additively) `Filter`.

use crate::{resolve, ImportedSession, Parsed, Result, Skipped};
use roxmltree::Node;
use serde_json::json;
use std::path::{Path, PathBuf};

fn child<'a, 'i>(node: Node<'a, 'i>, name: &str) -> Option<Node<'a, 'i>> {
    node.children().find(|c| c.has_tag_name(name))
}

fn text<'a>(node: Node<'a, '_>, name: &str) -> Option<&'a str> {
    child(node, name).and_then(|c| c.text()).map(str::trim)
}

/// Directions for one side's changes: (create, update, delete), each
/// "left", "right" or "none" — where that change is copied to.
type Dirs<'a> = (&'a str, &'a str, &'a str);

fn dirs<'a>(node: Option<Node<'a, '_>>) -> Option<Dirs<'a>> {
    let n = node?;
    Some((
        n.attribute("Create")?,
        n.attribute("Update")?,
        n.attribute("Delete")?,
    ))
}

/// The PCompare sync mode for a `<Synchronize>` block, or `None` for
/// FreeFileSync's Custom directions, which have no single equivalent.
fn sync_mode(sync: Node) -> Option<&'static str> {
    // XmlFormat < 21: a named variant.
    if let Some(variant) = text(sync, "Variant") {
        return match variant {
            "TwoWay" => Some("twoWay"),
            "Mirror" => Some("mirrorLeftToRight"),
            "Update" => Some("updateLeftToRight"),
            _ => None,
        };
    }
    // XmlFormat 21+: Differences (by category) or Changes (by side).
    if let Some(d) = child(sync, "Differences") {
        let all = |dir: &str| {
            ["LeftOnly", "LeftNewer", "RightNewer", "RightOnly"]
                .iter()
                .all(|a| d.attribute(*a) == Some(dir))
        };
        return if all("right") {
            Some("mirrorLeftToRight")
        } else if all("left") {
            Some("mirrorRightToLeft")
        } else {
            None
        };
    }
    let changes = child(sync, "Changes")?;
    let (left, right) = (
        dirs(child(changes, "Left"))?,
        dirs(child(changes, "Right"))?,
    );
    match (left, right) {
        (("right", "right", "right"), ("left", "left", "left")) => Some("twoWay"),
        (("right", "right", "right"), ("none", "none", "none") | ("right", "right", "right")) => {
            Some("mirrorLeftToRight")
        }
        (("none", "none", "none") | ("left", "left", "left"), ("left", "left", "left")) => {
            Some("mirrorRightToLeft")
        }
        (("right", "right", "none"), ("none", "none", "none")) => Some("updateLeftToRight"),
        (("none", "none", "none"), ("left", "left", "none")) => Some("updateRightToLeft"),
        (("right", "right", "none"), ("left", "left", "none")) => Some("updateBoth"),
        _ => None,
    }
}

fn compare_preset(compare: Node) -> Option<&'static str> {
    match text(compare, "Variant")? {
        "TimeAndSize" => Some("sizeAndTimestamp"),
        "Content" => Some("binary"),
        "Size" => Some("size"),
        _ => None,
    }
}

/// `*\thumbs.db` → `thumbs.db`, `\System Volume Information\` → `System Volume Information`.
fn exclusion(item: &str) -> String {
    let item = item.trim().replace('\\', "/");
    let item = item.strip_prefix("*/").unwrap_or(&item);
    item.trim_matches('/').to_owned()
}

fn items<'a>(filter: Option<Node<'a, '_>>, list: &str) -> Vec<&'a str> {
    filter
        .and_then(|f| child(f, list))
        .map(|l| {
            l.children()
                .filter(|c| c.has_tag_name("Item"))
                .filter_map(|c| c.text())
                .map(str::trim)
                .collect()
        })
        .unwrap_or_default()
}

fn stem(path: &Path) -> String {
    path.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "FreeFileSync".into())
}

pub(crate) fn parse(root: Node, path: &Path) -> Result<Parsed> {
    let batch = root.attribute("XmlType") == Some("BATCH");
    let mut out = Parsed {
        label: if batch {
            "FreeFileSync batch job"
        } else {
            "FreeFileSync configuration"
        }
        .into(),
        ..Default::default()
    };
    if root.attribute("XmlType") == Some("GLOBAL") {
        return Err(crate::format_err(
            "this is FreeFileSync's GlobalSettings.xml; choose a .ffs_gui or .ffs_batch file",
        ));
    }
    let base_dir = path.parent().unwrap_or(Path::new(""));
    let (g_compare, g_sync, g_filter) = (
        child(root, "Compare"),
        child(root, "Synchronize"),
        child(root, "Filter"),
    );
    let pairs: Vec<Node> = child(root, "FolderPairs")
        .map(|p| p.children().filter(|c| c.has_tag_name("Pair")).collect())
        .unwrap_or_default();
    let config = stem(path);
    for (i, pair) in pairs.iter().copied().enumerate() {
        let name = if pairs.len() > 1 {
            format!("{config} ({})", i + 1)
        } else {
            config.clone()
        };
        let side = |n| {
            text(pair, n)
                .map(|p| resolve(base_dir, p))
                .unwrap_or_default()
        };
        let (left, right) = (side("Left"), side("Right"));
        if left.is_empty() || right.is_empty() {
            out.skipped.push(Skipped {
                name,
                reason: "a side of this folder pair is empty".into(),
            });
            continue;
        }
        let mut s = ImportedSession::new(name, "sync", left, right);
        let sync = child(pair, "Synchronize").or(g_sync);
        match sync.and_then(sync_mode) {
            Some(mode) => s.set("mode", mode),
            None => s.notes.push("Custom sync directions have no PCompare equivalent; it will Update left → right until you pick a mode.".into()),
        }
        if let Some(preset) = child(pair, "Compare")
            .or(g_compare)
            .and_then(compare_preset)
        {
            s.set("compare", preset);
        }
        // A pair's own filter adds to the global one.
        let filters = [g_filter, child(pair, "Filter")];
        let exclude: Vec<String> = filters
            .iter()
            .flat_map(|f| items(*f, "Exclude"))
            .map(exclusion)
            .filter(|e| !e.is_empty())
            .collect();
        // Set even when empty: PCompare's default exclusions (.git,
        // node_modules) would otherwise silently leave files out of the sync.
        s.set("exclude", exclude.join(", "));
        let include: Vec<&str> = filters
            .iter()
            .flat_map(|f| items(*f, "Include"))
            .filter(|i| *i != "*")
            .collect();
        if !include.is_empty() {
            s.notes.push(format!("Include filter ({}) isn't carried over; Folder Sync includes everything not excluded.", include.join(", ")));
        }
        for f in filters.iter().flatten() {
            let limited = |tag: &str| {
                child(*f, tag).is_some_and(|n| {
                    n.attribute("Unit")
                        .or(n.attribute("Type"))
                        .is_some_and(|u| u != "None")
                })
            };
            if limited("SizeMin") || limited("SizeMax") || limited("TimeSpan") {
                s.notes
                    .push("Size and date filters aren't carried over.".into());
                break;
            }
        }
        if let Some(policy) = sync.and_then(|n| text(n, "DeletionPolicy")) {
            let versioning = sync
                .and_then(|n| text(n, "VersioningFolder"))
                .filter(|f| !f.is_empty());
            let disposal = match (policy, versioning) {
                ("Permanent", _) => Some(json!({ "kind": "permanent" })),
                ("Versioning", Some(folder)) => {
                    Some(json!({ "kind": "versioning", "folder": resolve(base_dir, folder) }))
                }
                ("RecycleBin", _) => Some(json!({ "kind": "trash" })),
                _ => None,
            };
            if let Some(d) = disposal {
                s.set("exec", json!({ "disposal": d }));
            }
        }
        out.sessions.push(s);
    }
    Ok(out)
}

/// The configurations FreeFileSync lists in its sidebar (from
/// `GlobalSettings.xml`), plus the last run.
pub(crate) fn locations(appdata: Option<PathBuf>, home: Option<PathBuf>) -> Vec<(String, PathBuf)> {
    let dirs: Vec<PathBuf> = appdata
        .map(|a| a.join("FreeFileSync"))
        .into_iter()
        .chain(home.into_iter().flat_map(|h| {
            [
                h.join("Library")
                    .join("Application Support")
                    .join("FreeFileSync"),
                h.join(".config").join("FreeFileSync"),
            ]
        }))
        .collect();
    let mut out = Vec::new();
    for dir in dirs {
        let Ok(xml) = std::fs::read(dir.join("GlobalSettings.xml")) else {
            continue;
        };
        let text = crate::decode(&xml);
        if let Ok(doc) = roxmltree::Document::parse(&text) {
            for item in doc.descendants().filter(|n| {
                n.has_tag_name("Item")
                    && n.parent().is_some_and(|p| p.has_tag_name("Configurations"))
            }) {
                if let Some(cfg) = item.attribute("Config") {
                    let path = PathBuf::from(resolve(&dir, cfg));
                    out.push((format!("FreeFileSync — {}", stem(&path)), path));
                }
            }
        }
        out.push((
            "FreeFileSync — last run".into(),
            dir.join("LastRun.ffs_gui"),
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use crate::parse_bytes;
    use std::path::Path;

    #[test]
    fn reads_a_v23_batch_with_two_pairs() {
        let xml = r#"<?xml version="1.0" encoding="utf-8"?>
<FreeFileSync XmlType="BATCH" XmlFormat="23">
    <Compare><Variant>TimeAndSize</Variant><Symlinks>Exclude</Symlinks><IgnoreTimeShift/></Compare>
    <Synchronize>
        <Differences LeftOnly="right" LeftNewer="right" RightNewer="right" RightOnly="right"/>
        <DeletionPolicy>RecycleBin</DeletionPolicy>
        <VersioningFolder Style="Replace"/>
    </Synchronize>
    <Filter>
        <Include><Item>*</Item></Include>
        <Exclude><Item>\System Volume Information\</Item><Item>*\thumbs.db</Item></Exclude>
        <SizeMin Unit="None">0</SizeMin><SizeMax Unit="None">0</SizeMax><TimeSpan Type="None">0</TimeSpan>
    </Filter>
    <FolderPairs>
        <Pair><Left>D:\Users\me\source</Left><Right>\\nas\source</Right></Pair>
        <Pair>
            <Left>D:\photos</Left><Right>E:\photos</Right>
            <Compare><Variant>Content</Variant></Compare>
            <Synchronize>
                <Changes><Left Create="right" Update="right" Delete="right"/><Right Create="left" Update="left" Delete="left"/></Changes>
                <DeletionPolicy>Permanent</DeletionPolicy>
            </Synchronize>
            <Filter><Exclude><Item>*.tmp</Item></Exclude></Filter>
        </Pair>
    </FolderPairs>
</FreeFileSync>"#;
        let p = parse_bytes(xml.as_bytes(), Path::new("/x/Backup.ffs_batch")).unwrap();
        assert_eq!(p.label, "FreeFileSync batch job");
        let [a, b] = &p.sessions[..] else {
            panic!("{:?}", p.sessions)
        };
        assert_eq!((a.name.as_str(), a.kind), ("Backup (1)", "sync"));
        assert_eq!(a.settings["mode"], "mirrorLeftToRight");
        assert_eq!(a.settings["compare"], "sizeAndTimestamp");
        assert_eq!(
            a.settings["exclude"],
            "System Volume Information, thumbs.db"
        );
        assert_eq!(a.settings["exec"]["disposal"]["kind"], "trash");
        assert!(a.notes.is_empty(), "{:?}", a.notes);
        assert_eq!(b.settings["mode"], "twoWay");
        assert_eq!(b.settings["compare"], "binary");
        assert_eq!(
            b.settings["exclude"],
            "System Volume Information, thumbs.db, *.tmp"
        );
        assert_eq!(b.settings["exec"]["disposal"]["kind"], "permanent");
    }

    #[test]
    fn reads_old_variants_and_flags_custom() {
        let xml = r#"<FreeFileSync XmlType="GUI" XmlFormat="14">
            <Synchronize><Variant>Custom</Variant></Synchronize>
            <FolderPairs><Pair><Left>C:\a</Left><Right>C:\b</Right></Pair></FolderPairs>
        </FreeFileSync>"#;
        let p = parse_bytes(xml.as_bytes(), Path::new("old.ffs_gui")).unwrap();
        let s = &p.sessions[0];
        assert_eq!(s.name, "old");
        assert!(s.settings.get("mode").is_none());
        assert!(s.notes[0].contains("Custom"));
        assert_eq!(s.settings["exclude"], "");
    }

    #[test]
    fn update_changes_map_to_update_mode() {
        let xml = r#"<FreeFileSync XmlType="GUI" XmlFormat="23"><Synchronize><Changes>
            <Left Create="right" Update="right" Delete="none"/><Right Create="none" Update="none" Delete="none"/>
            </Changes></Synchronize><FolderPairs><Pair><Left>a</Left><Right>b</Right></Pair></FolderPairs></FreeFileSync>"#;
        let p = parse_bytes(xml.as_bytes(), Path::new("/cfg/u.ffs_gui")).unwrap();
        assert_eq!(p.sessions[0].settings["mode"], "updateLeftToRight");
        assert_eq!(
            p.sessions[0].left,
            Path::new("/cfg").join("a").display().to_string()
        );
    }
}
