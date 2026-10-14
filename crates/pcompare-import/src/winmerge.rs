//! WinMerge project files (`.WinMerge`): one `<paths>` block per comparison.
//!
//! ```xml
//! <project>
//!   <paths>
//!     <left>C:\v1</left> <middle>…</middle> <right>C:\v2</right>
//!     <filter>*.*</filter> <subfolders>1</subfolders>
//!     <left-readonly>0</left-readonly> <right-readonly>1</right-readonly>
//!   </paths>
//! </project>
//! ```
//!
//! Paths may be relative to the project file.

use crate::{resolve, ImportedSession, Parsed, Result, Skipped};
use roxmltree::Node;
use std::path::Path;

fn child_text<'a>(node: Node<'a, '_>, name: &str) -> Option<&'a str> {
    node.children()
        .find(|c| c.has_tag_name(name))
        .and_then(|c| c.text())
        .map(str::trim)
}

fn flag(node: Node, name: &str) -> bool {
    child_text(node, name).is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))
}

/// The last path component, for naming a session after its sides.
fn leaf(p: &str) -> &str {
    let t = p.trim_end_matches(['\\', '/']);
    t.rsplit(['\\', '/'])
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(t)
}

pub(crate) fn parse(root: Node, base_dir: &Path) -> Result<Parsed> {
    let mut out = Parsed {
        label: "WinMerge project".into(),
        ..Default::default()
    };
    let blocks: Vec<Node> = root
        .children()
        .filter(|c| c.has_tag_name("paths"))
        .collect();
    for (i, paths) in blocks.iter().copied().enumerate() {
        let side = |n| {
            child_text(paths, n)
                .map(|p| resolve(base_dir, p))
                .unwrap_or_default()
        };
        let (left, right, middle) = (side("left"), side("right"), side("middle"));
        let name = match (left.is_empty(), right.is_empty()) {
            (false, false) => format!("{} ↔ {}", leaf(&left), leaf(&right)),
            _ if blocks.len() > 1 => format!("WinMerge project {}", i + 1),
            _ => "WinMerge project".to_owned(),
        };
        if left.is_empty() && right.is_empty() {
            out.skipped.push(Skipped {
                name,
                reason: "no paths".into(),
            });
            continue;
        }
        // WinMerge picks the viewer when the project opens: folders if the
        // sides are folders (checked here when they exist) or the project
        // carries folder options; otherwise files, whose viewer the app then
        // picks by extension.
        let folders = Path::new(&left).is_dir()
            || child_text(paths, "subfolders").is_some()
            || child_text(paths, "filter").is_some_and(|f| f != "*.*");
        let three_way = !middle.is_empty();
        // `window-type` pins the viewer: 0/-1 automatic, 1 text, 2 table,
        // 3 binary, 4 image, 5 webpage, 6 folder.
        let window = child_text(paths, "window-type")
            .and_then(|v| v.parse::<i32>().ok())
            .unwrap_or(-1);
        let kind = match (three_way, folders || window == 6, window) {
            (true, true, _) => "folderMerge",
            (true, false, _) => "merge",
            (false, true, _) => "folder",
            (false, false, 2) => "table",
            (false, false, 3) => "hex",
            (false, false, 4) => "picture",
            (false, false, _) => "text",
        };
        let mut s = ImportedSession::new(name, kind, left, right);
        s.file_kind_by_extension = kind == "text" && !matches!(window, 1 | 5);
        apply_options(&mut s, paths);
        if three_way {
            s.base = Some(middle);
        }
        if let Some(filter) = child_text(paths, "filter").filter(|f| !f.is_empty() && *f != "*.*") {
            apply_filter(&mut s, filter);
        }
        if child_text(paths, "subfolders") == Some("0") {
            s.notes.push(
                "WinMerge compared only the top level; PCompare compares subfolders too.".into(),
            );
        }
        if flag(paths, "left-readonly") || flag(paths, "right-readonly") {
            s.notes
                .push("Read-only sides aren't kept in saved sessions.".into());
        }
        for (tag, what) in [
            ("unpacker", "unpacker plugin"),
            ("prediffer", "prediffer plugin"),
        ] {
            if child_text(paths, tag).is_some_and(|v| !v.is_empty()) {
                s.notes
                    .push(format!("WinMerge {what} isn't available in PCompare."));
            }
        }
        out.sessions.push(s);
    }
    Ok(out)
}

/// Comparison options a WinMerge project can carry.
fn apply_options(s: &mut ImportedSession, paths: Node) {
    let num = |tag| child_text(paths, tag).and_then(|v| v.parse::<i32>().ok());
    let mut rules = serde_json::Map::new();
    match num("white-spaces") {
        Some(1) => drop(rules.insert("whitespace".into(), "ignoreChanges".into())),
        Some(2) => drop(rules.insert("whitespace".into(), "ignoreAll".into())),
        _ => {}
    }
    if child_text(paths, "ignore-case").is_some() {
        rules.insert("ignoreCase".into(), flag(paths, "ignore-case").into());
    }
    if child_text(paths, "ignore-comment-diff").is_some() {
        rules.insert(
            "ignoreComments".into(),
            flag(paths, "ignore-comment-diff").into(),
        );
    }
    // Text sessions keep rules under `options`, folder sessions under `rules`.
    let key = match s.kind {
        "folder" | "folderMerge" => Some("rules"),
        "text" | "merge" => Some("options"),
        _ => None,
    };
    if let (Some(key), false) = (key, rules.is_empty()) {
        s.set(key, serde_json::Value::Object(rules));
    }
    if matches!(s.kind, "folder" | "folderMerge") {
        // 0 full contents, 1 quick contents, 2 binary, 3 date, 4 date and size, 5 size, 6 existence.
        let (size, time, content) = match num("compare-method") {
            Some(0) => (true, true, "rules"),
            Some(1 | 2) => (true, true, "binary"),
            Some(3) => (false, true, "off"),
            Some(5) => (true, false, "off"),
            Some(6) => (false, false, "off"),
            _ => return,
        };
        s.set("compareSize", size);
        s.set("compareTimestamp", time);
        s.set("content", content);
    }
}

/// WinMerge filters are either a mask list (`*.cpp;*.h`, `!` to exclude) or
/// the name of a saved filter file, which can't come along.
fn apply_filter(s: &mut ImportedSession, filter: &str) {
    let masks: Vec<&str> = filter
        .split([';', ','])
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .collect();
    let is_mask = |m: &&str| m.contains('*') || m.contains('?') || m.starts_with('!');
    if masks.is_empty() || !masks.iter().all(is_mask) {
        s.notes.push(format!(
            "WinMerge filter `{filter}` is a named filter file; set a filter in PCompare instead."
        ));
        return;
    }
    let (exclude, include): (Vec<&str>, Vec<&str>) =
        masks.into_iter().partition(|m| m.starts_with('!'));
    let include: Vec<&str> = include
        .into_iter()
        .filter(|m| *m != "*.*" && *m != "*")
        .collect();
    if !include.is_empty() {
        s.set("includeFiles", include.join(", "));
    }
    if !exclude.is_empty() {
        let exclude: Vec<&str> = exclude.iter().map(|m| m.trim_start_matches('!')).collect();
        s.set("excludeFiles", exclude.join(", "));
    }
}

#[cfg(test)]
mod tests {
    use crate::parse_bytes;
    use std::path::Path;

    #[test]
    fn reads_two_and_three_way_projects() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<project>
  <paths>
    <left>C:\src\v1</left>
    <right>C:\src\v2</right>
    <filter>*.cpp;*.h;!*.tmp</filter>
    <subfolders>1</subfolders>
    <left-readonly>0</left-readonly>
    <right-readonly>1</right-readonly>
  </paths>
  <paths>
    <left>mine.txt</left>
    <middle>base.txt</middle>
    <right>theirs.txt</right>
  </paths>
</project>"#;
        let p = parse_bytes(xml.as_bytes(), Path::new("/proj/x.WinMerge")).unwrap();
        assert_eq!(p.sessions.len(), 2);
        let folder = &p.sessions[0];
        assert_eq!((folder.kind, folder.name.as_str()), ("folder", "v1 ↔ v2"));
        assert_eq!(folder.settings["includeFiles"], "*.cpp, *.h");
        assert_eq!(folder.settings["excludeFiles"], "*.tmp");
        assert!(folder.notes.iter().any(|n| n.contains("Read-only")));
        let merge = &p.sessions[1];
        assert_eq!(merge.kind, "merge");
        assert_eq!(
            merge.left,
            Path::new("/proj").join("mine.txt").display().to_string()
        );
        assert_eq!(
            merge.base.as_deref(),
            Some(
                Path::new("/proj")
                    .join("base.txt")
                    .display()
                    .to_string()
                    .as_str()
            )
        );
    }

    #[test]
    fn window_type_and_options_carry_over() {
        let xml = r#"<project>
  <paths><left>C:\a.csv</left><right>C:\b.csv</right><window-type>2</window-type></paths>
  <paths><left>C:\a.dat</left><right>C:\b.dat</right><window-type>1</window-type><white-spaces>2</white-spaces><ignore-case>1</ignore-case></paths>
  <paths><left>C:\x</left><right>C:\y</right><subfolders>1</subfolders><compare-method>5</compare-method><white-spaces>1</white-spaces></paths>
</project>"#;
        let p = parse_bytes(xml.as_bytes(), Path::new("p.WinMerge")).unwrap();
        let (table, text, folder) = (&p.sessions[0], &p.sessions[1], &p.sessions[2]);
        assert_eq!(table.kind, "table");
        assert!(!table.file_kind_by_extension);
        // Forced to Text: the extension mustn't send it to Hex.
        assert_eq!((text.kind, text.file_kind_by_extension), ("text", false));
        assert_eq!(text.settings["options"]["whitespace"], "ignoreAll");
        assert_eq!(text.settings["options"]["ignoreCase"], true);
        assert_eq!(folder.settings["rules"]["whitespace"], "ignoreChanges");
        assert_eq!(
            (
                folder.settings["compareTimestamp"].as_bool(),
                folder.settings["content"].as_str()
            ),
            (Some(false), Some("off"))
        );
    }

    #[test]
    fn named_filters_are_reported_not_guessed() {
        let xml = "<project><paths><left>C:\\a</left><right>C:\\b</right><filter>Source code</filter></paths></project>";
        let p = parse_bytes(xml.as_bytes(), Path::new("p.WinMerge")).unwrap();
        assert!(p.sessions[0].notes[0].contains("named filter"));
    }
}
