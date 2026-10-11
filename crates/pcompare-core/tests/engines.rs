use pcompare_core::folder::{compare_folders, CompareCriteria, EntryStatus, FolderCompareOptions, FolderEntry};
use pcompare_core::hex;
use std::fs;
use std::path::Path;

fn write(root: &Path, rel: &str, contents: &[u8]) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, contents).unwrap();
}

fn find<'a>(entries: &'a [FolderEntry], rel: &str) -> &'a FolderEntry {
    for e in entries {
        if e.rel_path == rel {
            return e;
        }
        if rel.starts_with(&format!("{}/", e.rel_path)) {
            return find(&e.children, rel);
        }
    }
    panic!("{rel} not found");
}

fn content_opts() -> FolderCompareOptions {
    FolderCompareOptions {
        criteria: CompareCriteria::Content,
        ignore_name_case: false,
        ..Default::default()
    }
}

#[test]
fn folder_compare_classifies_entries() {
    let (l, r) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    write(l.path(), "same.txt", b"hello");
    write(r.path(), "same.txt", b"hello");
    write(l.path(), "diff.txt", b"aaaa");
    write(r.path(), "diff.txt", b"aaab");
    write(l.path(), "only-left.txt", b"x");
    write(r.path(), "only-right/nested.txt", b"y");
    write(l.path(), "sub/a.txt", b"1");
    write(r.path(), "sub/a.txt", b"1");

    let res = compare_folders(l.path(), r.path(), &content_opts()).unwrap();
    let e = &res.entries;
    assert_eq!(find(e, "same.txt").status, EntryStatus::Same);
    assert_eq!(find(e, "diff.txt").status, EntryStatus::Different);
    assert_eq!(find(e, "only-left.txt").status, EntryStatus::LeftOnly);
    assert_eq!(find(e, "only-right").status, EntryStatus::RightOnly);
    assert_eq!(find(e, "only-right/nested.txt").status, EntryStatus::RightOnly);
    assert_eq!(find(e, "sub").status, EntryStatus::Same);

    assert_eq!(res.stats.same, 2);
    assert_eq!(res.stats.different, 1);
    assert_eq!(res.stats.left_only, 1);
    assert_eq!(res.stats.right_only, 1);
    // Folders sort before files.
    assert!(e[0].is_dir);
}

#[test]
fn folder_status_bubbles_up() {
    let (l, r) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    write(l.path(), "a/b/c.txt", b"one");
    write(r.path(), "a/b/c.txt", b"two");
    let res = compare_folders(l.path(), r.path(), &content_opts()).unwrap();
    assert_eq!(find(&res.entries, "a").status, EntryStatus::Different);
    assert_eq!(find(&res.entries, "a/b").status, EntryStatus::Different);
}

#[test]
fn exclude_patterns_and_hidden_files() {
    let (l, r) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    write(l.path(), "node_modules/x.js", b"1");
    write(l.path(), "build.tmp", b"1");
    write(l.path(), ".env", b"1");
    write(l.path(), "keep.rs", b"1");
    write(r.path(), "keep.rs", b"1");
    let opts = FolderCompareOptions {
        exclude: vec!["node_modules".into(), "*.tmp".into()],
        include_hidden: false,
        ..content_opts()
    };
    let res = compare_folders(l.path(), r.path(), &opts).unwrap();
    let names: Vec<_> = res.entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, vec!["keep.rs"]);
}

#[test]
fn bad_glob_is_reported() {
    let d = tempfile::tempdir().unwrap();
    let opts = FolderCompareOptions {
        exclude: vec!["[unclosed".into()],
        ..Default::default()
    };
    assert!(compare_folders(d.path(), d.path(), &opts).is_err());
}

#[test]
fn case_insensitive_pairing() {
    let (l, r) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    write(l.path(), "Readme.md", b"x");
    write(r.path(), "README.md", b"x");
    let opts = FolderCompareOptions {
        ignore_name_case: true,
        ..content_opts()
    };
    let res = compare_folders(l.path(), r.path(), &opts).unwrap();
    assert_eq!(res.entries.len(), 1);
    assert_eq!(res.entries[0].status, EntryStatus::Same);
}

#[test]
fn hex_summary_and_navigation() {
    let d = tempfile::tempdir().unwrap();
    let (a, b) = (d.path().join("a.bin"), d.path().join("b.bin"));
    fs::write(&a, [0u8, 1, 2, 3, 4, 5]).unwrap();
    fs::write(&b, [0u8, 1, 9, 3, 4, 5, 6, 7]).unwrap();

    let s = hex::summarize(&a, &b).unwrap();
    assert_eq!(s.first_difference, Some(2));
    assert_eq!(s.differing_bytes, 3); // one changed byte + two extra bytes
    assert!(!s.identical);

    assert_eq!(hex::next_difference(&a, &b, 3).unwrap(), Some(6));
    assert_eq!(hex::next_difference(&a, &a, 0).unwrap(), None);

    let chunk = hex::read_chunk(&a, &b, 4, 16).unwrap();
    assert_eq!(chunk.left, vec![4, 5]);
    assert_eq!(chunk.right, vec![4, 5, 6, 7]);
}
