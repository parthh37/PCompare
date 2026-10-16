//! Path completion: what's on disk under a half-typed path, for the path
//! fields' dropdown. Only absolute local paths are listed; anything else
//! (remote, archive and VCS paths, a bare folder name) is left to the
//! recent-path history.

use serde::Serialize;
use std::fs;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PathCompletion {
    /// The folder listed, as typed and ending with its separator (`C:\Users\`).
    pub dir: String,
    /// The matches, folders first, then by name.
    pub entries: Vec<CompletionEntry>,
    /// Matches left out past the limit.
    pub more: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompletionEntry {
    pub name: String,
    /// `dir` + `name`: what the field holds once this entry is picked.
    pub path: String,
    pub is_dir: bool,
}

/// Splits typed text into the folder to list (as typed, up to and including
/// its last separator) and the start of a name after it. `None` unless the
/// text is an absolute local path: a drive (`C:\`, or a bare `C:`) or a UNC
/// share on Windows, `/…` elsewhere. Quotes around it, as Explorer's "Copy as
/// path" adds, are ignored.
pub fn split_typed(text: &str, windows: bool) -> Option<(String, &str)> {
    let text = text.trim_start().trim_start_matches('"').trim_end_matches('"');
    let is_sep = |c: char| c == '/' || (windows && c == '\\');
    if windows {
        let mut chars = text.chars();
        let (a, b, c) = (chars.next(), chars.next(), chars.next());
        let drive = a.is_some_and(|a| a.is_ascii_alphabetic()) && b == Some(':');
        if drive && c.is_none() {
            return Some((format!("{text}\\"), ""));
        }
        let rooted = if drive { c.is_some_and(is_sep) } else { a.is_some_and(is_sep) && b.is_some_and(is_sep) };
        if !rooted {
            return None;
        }
    } else if !text.starts_with('/') {
        return None;
    }
    let cut = text.rfind(is_sep)? + 1;
    Some((text[..cut].to_owned(), &text[cut..]))
}

/// The entries of the folder `text` points into whose names start with what
/// follows its last separator (ignoring case), folders first. Folders always
/// come along, as the way to reach deeper paths; `files` says which files to
/// offer (a folder field takes only archives and snapshots). `None` when the
/// text isn't an absolute local path or its folder can't be read.
pub fn complete_path(text: &str, limit: usize, files: impl Fn(&str) -> bool) -> Option<PathCompletion> {
    let (dir, partial) = split_typed(text, cfg!(windows))?;
    let want = partial.to_lowercase();
    let mut found = Vec::new();
    for entry in fs::read_dir(&dir).ok()?.flatten() {
        // a name that isn't valid Unicode can't be typed back into the field
        let Ok(name) = entry.file_name().into_string() else { continue };
        if !name.to_lowercase().starts_with(&want) || (is_hidden(&entry, &name) && !offer_hidden(&name, partial)) {
            continue;
        }
        let is_dir = match entry.file_type() {
            // a link to a folder leads somewhere just like the folder
            Ok(t) if t.is_symlink() => fs::metadata(entry.path()).is_ok_and(|m| m.is_dir()),
            Ok(t) => t.is_dir(),
            Err(_) => continue,
        };
        if is_dir || files(&name) {
            found.push(CompletionEntry { path: format!("{dir}{name}"), name, is_dir });
        }
    }
    found.sort_by_cached_key(|e| (!e.is_dir, e.name.to_lowercase()));
    let more = found.len().saturating_sub(limit);
    found.truncate(limit);
    Some(PathCompletion { dir, entries: found, more })
}

/// Dot files everywhere, plus the hidden attribute on Windows (`AppData`,
/// `ProgramData`, `$Recycle.Bin`…).
#[cfg(windows)]
fn is_hidden(entry: &fs::DirEntry, name: &str) -> bool {
    use std::os::windows::fs::MetadataExt;
    name.starts_with('.') || entry.metadata().is_ok_and(|m| m.file_attributes() & crate::folder::attr::HIDDEN != 0)
}

#[cfg(not(windows))]
fn is_hidden(_entry: &fs::DirEntry, name: &str) -> bool {
    name.starts_with('.')
}

/// Whether a hidden entry is offered anyway. `partial` is what's typed of its
/// name so far (the name already starts with it, ignoring case); it's empty
/// right after a separator, when the whole folder is being shown.
// TODO(you): pick the policy. The working default hides hidden entries from
// the whole-folder list but offers them once any of the name is typed, so
// `C:\Users\me\A` reaches AppData and `~/.` shows dot files. Alternatives:
// wait for 2+ characters (less clutter under `C:\P`, where ProgramData sits
// beside Program Files), only on a leading `.` (shell-style, but Windows'
// hidden folders could then never be suggested), or never offer
// hidden+system entries like `System Volume Information`, which can't be
// opened anyway (that needs the attribute passed in here).
fn offer_hidden(_name: &str, partial: &str) -> bool {
    !partial.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::MAIN_SEPARATOR as SEP;

    #[test]
    fn splits_windows_paths() {
        let s = |t| split_typed(t, true);
        assert_eq!(s(r"C:\"), Some((r"C:\".into(), "")));
        assert_eq!(s(r"C:\ad1 backup\SO"), Some((r"C:\ad1 backup\".into(), "SO")));
        assert_eq!(s("c:/Users/pa"), Some(("c:/Users/".into(), "pa")));
        assert_eq!(s("D:"), Some((r"D:\".into(), "")));
        assert_eq!(s(r"\\server\share\Fo"), Some((r"\\server\share\".into(), "Fo")));
        assert_eq!(s(r#""C:\Program Files""#), Some((r"C:\".into(), "Program Files")));
        assert_eq!(s(r"  C:\x"), Some((r"C:\".into(), "x")));
        // relative, bare names and other schemes are left to the history
        for t in ["", "C", "SOFT", r"docs\x", "C:x", r"\Windows", "remote://p/x", r"git:C:\r@HEAD"] {
            assert_eq!(s(t), None, "{t}");
        }
    }

    #[test]
    fn splits_posix_paths() {
        let s = |t| split_typed(t, false);
        assert_eq!(s("/"), Some(("/".into(), "")));
        assert_eq!(s("/Users/me/Dow"), Some(("/Users/me/".into(), "Dow")));
        // a backslash is just a character in a POSIX name
        assert_eq!(s(r"/tmp/a\b"), Some(("/tmp/".into(), r"a\b")));
        assert_eq!(s("Users/me"), None);
        assert_eq!(s(r"C:\Users"), None);
    }

    fn tree() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        for d in ["Alpha", "alps", "beta", ".git"] {
            fs::create_dir(tmp.path().join(d)).unwrap();
        }
        for f in ["alpine.txt", "Album.zip", ".env"] {
            fs::write(tmp.path().join(f), "x").unwrap();
        }
        tmp
    }

    fn names(c: &PathCompletion) -> Vec<&str> {
        c.entries.iter().map(|e| e.name.as_str()).collect()
    }

    #[test]
    fn lists_matches_folders_first() {
        let tmp = tree();
        let dir = format!("{}{SEP}", tmp.path().display());
        let c = complete_path(&format!("{dir}al"), 50, |_| true).unwrap();
        assert_eq!(c.dir, dir);
        assert_eq!(names(&c), ["Alpha", "alps", "Album.zip", "alpine.txt"]);
        assert_eq!(c.entries[0].path, format!("{dir}Alpha"));
        assert!(c.entries[0].is_dir && !c.entries[2].is_dir);
        assert_eq!(c.more, 0);
    }

    #[test]
    fn folder_fields_get_folders_and_chosen_files() {
        let tmp = tree();
        let c = complete_path(&format!("{}{SEP}", tmp.path().display()), 50, |n| n.ends_with(".zip")).unwrap();
        assert_eq!(names(&c), ["Alpha", "alps", "beta", "Album.zip"]);
    }

    #[test]
    fn hidden_entries_wait_for_a_typed_name() {
        let tmp = tree();
        let dir = format!("{}{SEP}", tmp.path().display());
        let all = complete_path(&dir, 50, |_| true).unwrap();
        assert!(!names(&all).iter().any(|n| n.starts_with('.')));
        let dotted = complete_path(&format!("{dir}."), 50, |_| true).unwrap();
        assert_eq!(names(&dotted), [".git", ".env"]);
    }

    #[test]
    fn limits_and_counts_the_rest() {
        let tmp = tree();
        let c = complete_path(&format!("{}{SEP}", tmp.path().display()), 2, |_| true).unwrap();
        assert_eq!(names(&c), ["Alpha", "alps"]);
        assert_eq!(c.more, 3);
    }

    #[test]
    fn unreadable_or_relative_gives_nothing() {
        let tmp = tree();
        assert_eq!(complete_path(&format!("{}{SEP}missing{SEP}x", tmp.path().display()), 50, |_| true), None);
        assert_eq!(complete_path("Alpha", 50, |_| true), None);
        // a file is not a folder to list
        assert_eq!(complete_path(&format!("{}{SEP}alpine.txt{SEP}", tmp.path().display()), 50, |_| true), None);
    }

    #[cfg(unix)]
    #[test]
    fn links_to_folders_count_as_folders() {
        let tmp = tree();
        std::os::unix::fs::symlink(tmp.path().join("beta"), tmp.path().join("beta-link")).unwrap();
        let c = complete_path(&format!("{}/beta", tmp.path().display()), 50, |_| false).unwrap();
        assert_eq!(names(&c), ["beta", "beta-link"]);
        assert!(c.entries.iter().all(|e| e.is_dir));
    }
}
