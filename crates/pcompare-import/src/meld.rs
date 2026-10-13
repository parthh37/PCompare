//! Meld's recent comparisons (`.meldcmp`), INI files Meld keeps in
//! `~/.local/share/meld` (`%LOCALAPPDATA%\meld` on Windows):
//!
//! ```ini
//! [Comparison]
//! type = File
//! uris = file:///home/u/a.txt;file:///home/u/b.txt
//! ```
//!
//! `type` is File, Folder, Merge (three files) or Version control; `uris`
//! lists the panes left to right. A folder of them reads as one source.

use crate::{format_err, ImportedSession, Parsed, Result, Skipped};
use std::path::Path;

/// `file:///C:/x%20y` → `C:\x y`; `file:///home/u` → `/home/u`.
fn uri_to_path(uri: &str) -> String {
    let Some(rest) = uri.strip_prefix("file://") else {
        return uri.to_owned();
    };
    let decoded = percent_decode(rest);
    let b = decoded.as_bytes();
    // `/C:/…` is a Windows drive path.
    if b.len() >= 3 && b[0] == b'/' && b[1].is_ascii_alphabetic() && b[2] == b':' {
        decoded[1..].replace('/', "\\")
    } else {
        decoded
    }
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let hex = std::str::from_utf8(&b[i + 1..i + 3])
                .ok()
                .and_then(|h| u8::from_str_radix(h, 16).ok());
            if let Some(v) = hex {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn leaf(p: &str) -> &str {
    let t = p.trim_end_matches(['\\', '/']);
    t.rsplit(['\\', '/'])
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(t)
}

/// One `.meldcmp` file → a session, or why it can't be one.
fn comparison(text: &str) -> std::result::Result<ImportedSession, Skipped> {
    let mut in_section = false;
    let (mut kind, mut uris) = (None, None);
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            in_section = line.eq_ignore_ascii_case("[Comparison]");
        } else if let (true, Some((k, v))) = (in_section, line.split_once('=')) {
            match k.trim() {
                "type" => kind = Some(v.trim().to_owned()),
                "uris" => uris = Some(v.trim().to_owned()),
                _ => {}
            }
        }
    }
    let paths: Vec<String> = uris
        .unwrap_or_default()
        .split(';')
        .filter(|u| !u.is_empty())
        .map(uri_to_path)
        .collect();
    let name = paths
        .iter()
        .map(|p| leaf(p))
        .collect::<Vec<_>>()
        .join(" ↔ ");
    let kind = kind.unwrap_or_default();
    let skip = |reason: &str| Skipped {
        name: if name.is_empty() {
            "Meld comparison".into()
        } else {
            name.clone()
        },
        reason: reason.into(),
    };
    let mapped = match (kind.as_str(), paths.len()) {
        ("Version control", _) => {
            return Err(skip("version-control views have no PCompare equivalent"))
        }
        ("Folder", 2) => "folder",
        ("Folder", 3) => "folderMerge",
        ("File" | "Merge", 2) => "text",
        ("File" | "Merge", 3) => "merge",
        _ => return Err(skip("not a two- or three-pane comparison")),
    };
    // Meld's three panes are left, middle (the base when merging) and right.
    let (left, right, base) = match &paths[..] {
        [l, r] => (l.clone(), r.clone(), None),
        [l, m, r] => (l.clone(), r.clone(), Some(m.clone())),
        _ => unreachable!("lengths checked above"),
    };
    let mut s = ImportedSession::new(name, mapped, left, right);
    s.base = base;
    s.file_kind_by_extension = mapped == "text";
    Ok(s)
}

pub(crate) fn parse(text: &str) -> Result<Parsed> {
    if !text
        .lines()
        .any(|l| l.trim().eq_ignore_ascii_case("[Comparison]"))
    {
        return Err(format_err("not a Meld comparison file"));
    }
    let mut out = Parsed {
        label: "Meld comparison".into(),
        ..Default::default()
    };
    match comparison(text) {
        Ok(s) => out.sessions.push(s),
        Err(skip) => out.skipped.push(skip),
    }
    Ok(out)
}

/// Every `.meldcmp` in a folder (Meld's recent-comparisons store), newest first.
pub(crate) fn parse_dir(dir: &Path) -> Result<Parsed> {
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .map_err(|source| crate::ImportError::Io {
            path: dir.display().to_string(),
            source,
        })?
        .flatten()
        .filter(|e| {
            e.path()
                .extension()
                .is_some_and(|x| x.eq_ignore_ascii_case("meldcmp"))
        })
        .map(|e| (e.metadata().and_then(|m| m.modified()).ok(), e.path()))
        .collect();
    if files.is_empty() {
        return Err(format_err(
            "this folder has no Meld comparisons (.meldcmp files)",
        ));
    }
    files.sort_by_key(|f| std::cmp::Reverse(f.0));
    let mut out = Parsed {
        label: "Meld recent comparisons".into(),
        ..Default::default()
    };
    for (_, file) in files {
        let Ok(bytes) = std::fs::read(&file) else {
            continue;
        };
        match comparison(&crate::decode(&bytes)) {
            // Meld keeps these automatically, like BC's auto-saved sessions.
            Ok(mut s) => {
                s.auto_saved = true;
                if !out.sessions.iter().any(|x: &ImportedSession| {
                    (&x.left, &x.right, &x.base) == (&s.left, &s.right, &s.base)
                }) {
                    out.sessions.push(s);
                }
            }
            Err(skip) => out.skipped.push(skip),
        }
    }
    Ok(out)
}

/// Meld's store for this user, if it exists.
pub(crate) fn location(
    local_appdata: Option<std::path::PathBuf>,
    home: Option<std::path::PathBuf>,
) -> Vec<std::path::PathBuf> {
    local_appdata
        .map(|d| d.join("meld"))
        .into_iter()
        .chain(home.map(|h| h.join(".local").join("share").join("meld")))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse_bytes;

    #[test]
    fn reads_file_folder_and_merge_comparisons() {
        let file =
            "[Comparison]\ntype = File\nuris = file:///home/u/a%20b.txt;file:///home/u/c.txt\n";
        let p = parse_bytes(file.as_bytes(), Path::new("recent-x.meldcmp")).unwrap();
        let s = &p.sessions[0];
        assert_eq!(
            (s.kind, s.left.as_str(), s.right.as_str()),
            ("text", "/home/u/a b.txt", "/home/u/c.txt")
        );
        assert!(s.file_kind_by_extension);

        let merge = "[Comparison]\ntype = Merge\nuris = file:///C:/m/mine.c;file:///C:/m/base.c;file:///C:/m/theirs.c\n";
        let s = &parse_bytes(merge.as_bytes(), Path::new("m.meldcmp"))
            .unwrap()
            .sessions[0];
        assert_eq!(
            (s.kind, s.left.as_str(), s.base.as_deref()),
            ("merge", r"C:\m\mine.c", Some(r"C:\m\base.c"))
        );

        let vc = "[Comparison]\ntype = Version control\nuris = file:///repo\n";
        assert_eq!(
            parse_bytes(vc.as_bytes(), Path::new("v.meldcmp"))
                .unwrap()
                .skipped
                .len(),
            1
        );
    }

    #[test]
    fn a_folder_of_recent_comparisons_reads_as_one_source() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("recent-1.meldcmp"),
            "[Comparison]\ntype = Folder\nuris = file:///a;file:///b\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("recent-2.meldcmp"),
            "[Comparison]\ntype = Folder\nuris = file:///a;file:///b\n",
        )
        .unwrap();
        let p = parse_dir(dir.path()).unwrap();
        assert_eq!(p.sessions.len(), 1, "duplicates collapse");
        assert!(p.sessions[0].auto_saved);
        assert_eq!(p.sessions[0].kind, "folder");
    }
}
