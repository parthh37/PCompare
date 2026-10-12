use pcompare_core::folder::{
    compare_folders, pair_passes, ContentMethod, EntryMeta, EntryStatus, FolderCompareOptions, FolderEntry, FolderFilters,
};
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
        content: ContentMethod::Binary,
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

#[test]
fn rules_based_treats_line_endings_and_ignored_text_as_similar() {
    let (l, r) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    write(l.path(), "eol.txt", b"one\ntwo\n");
    write(r.path(), "eol.txt", b"one\r\ntwo\r\n");
    write(l.path(), "stamp.txt", b"built 2026-01-01\nbody\n");
    write(r.path(), "stamp.txt", b"built 2026-10-12\nbody\n");
    write(l.path(), "real.txt", b"alpha\n");
    write(r.path(), "real.txt", b"beta\n");
    write(l.path(), "same.bin", b"\0\x01\x02");
    write(r.path(), "same.bin", b"\0\x01\x02");
    let mut opts = FolderCompareOptions {
        content: ContentMethod::Rules,
        ..content_opts()
    };
    opts.rules.ignore_patterns = vec![r"\d{4}-\d{2}-\d{2}".into()];

    let res = compare_folders(l.path(), r.path(), &opts).unwrap();
    let e = &res.entries;
    assert_eq!(find(e, "eol.txt").status, EntryStatus::Similar);
    assert_eq!(find(e, "stamp.txt").status, EntryStatus::Similar);
    assert_eq!(find(e, "real.txt").status, EntryStatus::Different);
    assert_eq!(find(e, "same.bin").status, EntryStatus::Same);
    assert_eq!(find(e, "eol.txt").content_checked, Some(ContentMethod::Rules));
    assert_eq!(res.stats.similar, 2);
}

#[test]
fn crc_reports_checksums() {
    let (l, r) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    write(l.path(), "a.txt", b"hello");
    write(r.path(), "a.txt", b"hellp");
    let opts = FolderCompareOptions {
        content: ContentMethod::Crc,
        ..content_opts()
    };
    let res = compare_folders(l.path(), r.path(), &opts).unwrap();
    let a = find(&res.entries, "a.txt");
    assert_eq!(a.status, EntryStatus::Different);
    let [lc, rc] = a.crc.expect("crc computed");
    assert_eq!(lc, 0x3610a686); // CRC-32 of "hello"
    assert_ne!(lc, rc);
}

#[test]
fn quick_tests_without_override_keep_timestamp_differences() {
    let (l, r) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    write(l.path(), "a.txt", b"same bytes");
    write(r.path(), "a.txt", b"same bytes");
    let old = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000_000);
    fs::File::options().write(true).open(l.path().join("a.txt")).unwrap().set_modified(old).unwrap();

    let overriding = compare_folders(l.path(), r.path(), &content_opts()).unwrap();
    assert_eq!(overriding.entries[0].status, EntryStatus::Same, "content overrides the quick test");

    let strict = FolderCompareOptions {
        override_quick_tests: false,
        ..content_opts()
    };
    let res = compare_folders(l.path(), r.path(), &strict).unwrap();
    assert_eq!(res.entries[0].status, EntryStatus::Different);
    assert_eq!(res.entries[0].content_checked, Some(ContentMethod::Binary));
}

#[test]
fn name_filters_split_files_and_folders() {
    let (l, r) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    for root in [l.path(), r.path()] {
        write(root, "src/main.rs", b"fn main(){}");
        write(root, "src/notes.md", b"#");
        write(root, "target/debug/app.rs", b"x");
        write(root, "README.MD", b"#");
    }
    let opts = FolderCompareOptions {
        filters: FolderFilters {
            include_files: vec!["*.rs".into()],
            exclude_folders: vec!["target".into()],
            ..Default::default()
        },
        ..content_opts()
    };
    let res = compare_folders(l.path(), r.path(), &opts).unwrap();
    let names: Vec<_> = res.entries.iter().map(|e| e.rel_path.as_str()).collect();
    assert_eq!(names, vec!["src"]);
    let src: Vec<_> = res.entries[0].children.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(src, vec!["main.rs"]);
}

#[test]
fn size_filter_hides_small_pairs() {
    let (l, r) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    write(l.path(), "tiny.txt", b"1");
    write(r.path(), "tiny.txt", b"2");
    write(l.path(), "big.txt", &[b'a'; 4096]);
    write(r.path(), "big.txt", &[b'a'; 4096]);
    let opts = FolderCompareOptions {
        filters: FolderFilters {
            min_size: Some(1024),
            ..Default::default()
        },
        ..content_opts()
    };
    let res = compare_folders(l.path(), r.path(), &opts).unwrap();
    let names: Vec<_> = res.entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, vec!["big.txt"]);
}

#[test]
fn size_and_date_filters_keep_a_pair_when_either_side_matches() {
    let file = |size, modified_ms| EntryMeta { size, modified_ms, is_dir: false, ..Default::default() };
    let size = FolderFilters { max_size: Some(100), ..Default::default() };
    // Grew past the limit on one side only: still listed.
    assert!(pair_passes(Some(&file(50, None)), Some(&file(500, None)), &size));
    assert!(!pair_passes(Some(&file(500, None)), Some(&file(600, None)), &size));
    // Orphans are judged by the side that exists.
    assert!(pair_passes(None, Some(&file(10, None)), &size));
    assert!(!pair_passes(Some(&file(500, None)), None, &size));

    let date = FolderFilters { modified_after_ms: Some(1_000), ..Default::default() };
    assert!(!pair_passes(Some(&file(1, Some(500))), Some(&file(1, Some(999))), &date));
    assert!(pair_passes(Some(&file(1, Some(500))), Some(&file(1, Some(1_000))), &date));
    // Unknown timestamps can't be proven out of range.
    assert!(pair_passes(Some(&file(1, None)), None, &date));
}

#[test]
fn folder_compare_lists_a_single_side() {
    use pcompare_core::folder::compare_folders_with;
    use pcompare_core::vfs::{EmptyFs, LocalFs};
    let l = tempfile::tempdir().unwrap();
    write(l.path(), "a.txt", b"1");
    write(l.path(), "sub/b.txt", b"2");
    let dir = l.path().to_string_lossy();

    let res = compare_folders_with(&LocalFs, &dir, &EmptyFs, "", &content_opts()).unwrap();
    assert_eq!(find(&res.entries, "a.txt").status, EntryStatus::LeftOnly);
    assert_eq!(find(&res.entries, "sub/b.txt").status, EntryStatus::LeftOnly);
    assert_eq!(res.stats.left_only, 2);

    let res = compare_folders_with(&EmptyFs, "", &LocalFs, &dir, &content_opts()).unwrap();
    assert_eq!(find(&res.entries, "a.txt").status, EntryStatus::RightOnly);
}
