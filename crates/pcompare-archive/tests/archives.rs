//! Archives, snapshots and the expanding wrapper against generated files.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use pcompare_archive::compare::StoredChecksums;
use pcompare_archive::snapshot::{self, SnapshotOptions};
use pcompare_archive::{ArchiveKind, Archives, ContainerKind, CrcSource};
use pcompare_core::folder::{
    self, ContentMethod, EntryStatus, FolderCompareOptions, FolderEntry, ScanControl,
};
use pcompare_core::vfs::{LocalFs, Vfs};

const FILES: &[(&str, &[u8])] = &[
    ("readme.txt", b"hello archive\n"),
    ("src/main.rs", b"fn main() {}\n"),
    ("src/deep/data.bin", b"\x00\x01\x02\x03binary"),
];

fn write_zip(path: &Path, files: &[(&str, &[u8])]) {
    let mut w = zip::ZipWriter::new(File::create(path).unwrap());
    for (i, (name, data)) in files.iter().enumerate() {
        // Mix storage methods: stored, deflated and bzip2 (decoded by the zip crate).
        let method = match i % 3 {
            0 => zip::CompressionMethod::Deflated,
            1 => zip::CompressionMethod::Stored,
            _ => zip::CompressionMethod::Bzip2,
        };
        let opts = zip::write::SimpleFileOptions::default().compression_method(method);
        w.start_file(*name, opts).unwrap();
        w.write_all(data).unwrap();
    }
    w.finish().unwrap();
}

fn tar_bytes(files: &[(&str, &[u8])]) -> Vec<u8> {
    let mut b = tar::Builder::new(Vec::new());
    for (name, data) in files {
        let mut h = tar::Header::new_gnu();
        h.set_size(data.len() as u64);
        h.set_mode(0o640);
        h.set_mtime(1_700_000_000);
        h.set_cksum();
        b.append_data(&mut h, name, *data).unwrap();
    }
    b.into_inner().unwrap()
}

fn read_all(fs: &dyn Vfs, path: &str) -> Vec<u8> {
    let mut v = Vec::new();
    fs.open(path).unwrap().read_to_end(&mut v).unwrap();
    v
}

fn names(fs: &dyn Vfs, dir: &str) -> Vec<String> {
    let mut n: Vec<_> = fs
        .read_dir(dir)
        .unwrap()
        .into_iter()
        .map(|e| e.name)
        .collect();
    n.sort();
    n
}

fn s(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

fn setup() -> (tempfile::TempDir, Arc<Archives>) {
    let dir = tempfile::tempdir().unwrap();
    let archives = Arc::new(Archives::new(dir.path().join("cache")));
    (dir, archives)
}

/// Lists and reads every sample file through a resolved archive path.
fn check_contents(archives: &Archives, archive: &Path, kind: ArchiveKind) {
    let root = archives.resolve(&s(archive), true).unwrap().unwrap();
    assert_eq!(root.kind, ContainerKind::Archive(kind));
    assert_eq!(root.path, "");
    assert_eq!(names(&*root.fs, ""), ["readme.txt", "src"]);
    assert_eq!(names(&*root.fs, "src"), ["deep", "main.rs"]);
    for (name, data) in FILES {
        // Nested paths resolve inside the archive, with either separator.
        let full = format!("{}/{}", s(archive), name.replace('/', "\\"));
        let r = archives.resolve(&full, false).unwrap().unwrap();
        assert_eq!(r.path, *name);
        assert_eq!(read_all(&*r.fs, &r.path), *data, "{kind:?} {name}");
        assert_eq!(r.fs.stat(&r.path).unwrap().unwrap().size, data.len() as u64);
        // The viewers get an extracted copy.
        let copy = archives.local_copy(&full).unwrap().unwrap();
        assert_eq!(fs::read(&copy).unwrap(), *data);
    }
    // The bare archive is still a file where a file is expected.
    assert!(archives.resolve(&s(archive), false).unwrap().is_none());
}

#[test]
fn zip_listing_reading_and_crcs() {
    let (dir, archives) = setup();
    let zip = dir.path().join("a.zip");
    write_zip(&zip, FILES);
    check_contents(&archives, &zip, ArchiveKind::Zip);
    let r = archives.resolve(&s(&zip), true).unwrap().unwrap();
    let crcs = r.crcs.unwrap();
    assert_eq!(
        crcs.stored_crc("readme.txt"),
        Some(crc32fast::hash(FILES[0].1))
    );
    assert!(r.read_only.is_none());
}

#[test]
fn tar_family_listing_and_reading() {
    let (dir, archives) = setup();
    let tar = tar_bytes(FILES);

    let plain = dir.path().join("a.tar");
    fs::write(&plain, &tar).unwrap();
    check_contents(&archives, &plain, ArchiveKind::Tar);

    let gz = dir.path().join("a.tar.gz");
    let mut e =
        flate2::write::GzEncoder::new(File::create(&gz).unwrap(), flate2::Compression::default());
    e.write_all(&tar).unwrap();
    e.finish().unwrap();
    check_contents(&archives, &gz, ArchiveKind::TarGz);

    let bz = dir.path().join("a.tbz2");
    let mut e =
        bzip2::write::BzEncoder::new(File::create(&bz).unwrap(), bzip2::Compression::default());
    e.write_all(&tar).unwrap();
    e.finish().unwrap();
    check_contents(&archives, &bz, ArchiveKind::TarBz2);

    let xz = dir.path().join("a.tar.xz");
    let mut e = lzma_rust2::XzWriter::new(
        File::create(&xz).unwrap(),
        lzma_rust2::XzOptions::with_preset(1),
    )
    .unwrap();
    e.write_all(&tar).unwrap();
    e.finish().unwrap();
    check_contents(&archives, &xz, ArchiveKind::TarXz);

    // Tar metadata comes through.
    let r = archives
        .resolve(&format!("{}/readme.txt", s(&plain)), false)
        .unwrap()
        .unwrap();
    let meta = r.fs.stat(&r.path).unwrap().unwrap();
    assert_eq!(
        (meta.modified_ms, meta.mode),
        (Some(1_700_000_000_000), Some(0o640))
    );
    // Tar is read-only, and says so.
    let err = r.fs.remove_file(&r.path).unwrap_err();
    assert!(
        err.to_string()
            .contains("only zip archives can be modified"),
        "{err}"
    );
}

#[test]
fn single_gzip_file() {
    let (dir, archives) = setup();
    let gz = dir.path().join("log.txt.gz");
    let mut e =
        flate2::write::GzEncoder::new(File::create(&gz).unwrap(), flate2::Compression::default());
    e.write_all(b"line 1\nline 2\n").unwrap();
    e.finish().unwrap();
    let r = archives.resolve(&s(&gz), true).unwrap().unwrap();
    assert_eq!(names(&*r.fs, ""), ["log.txt"]);
    assert_eq!(read_all(&*r.fs, "log.txt"), b"line 1\nline 2\n");
    assert_eq!(r.fs.stat("log.txt").unwrap().unwrap().size, 14);
}

#[test]
fn sevenz_listing_and_reading() {
    let (dir, archives) = setup();
    let path = dir.path().join("a.7z");
    let mut w = sevenz_rust2::ArchiveWriter::new(File::create(&path).unwrap()).unwrap();
    w.push_archive_entry::<&[u8]>(sevenz_rust2::ArchiveEntry::new_directory("src"), None)
        .unwrap();
    for (name, data) in FILES {
        w.push_archive_entry(sevenz_rust2::ArchiveEntry::new_file(name), Some(*data))
            .unwrap();
    }
    w.finish().unwrap();
    check_contents(&archives, &path, ArchiveKind::SevenZ);
    let r = archives.resolve(&s(&path), true).unwrap().unwrap();
    assert_eq!(
        r.crcs.unwrap().stored_crc("src/main.rs"),
        Some(crc32fast::hash(FILES[1].1))
    );
}

#[test]
fn nested_archives_resolve_and_are_read_only() {
    let (dir, archives) = setup();
    let inner = dir.path().join("inner.zip");
    write_zip(&inner, &[("deep.txt", b"nested!")]);
    let outer = dir.path().join("outer.tar");
    fs::write(
        &outer,
        tar_bytes(&[("lib/inner.zip", &fs::read(&inner).unwrap())]),
    )
    .unwrap();

    let path = format!("{}/lib/inner.zip/deep.txt", s(&outer));
    let r = archives.resolve(&path, false).unwrap().unwrap();
    assert_eq!(r.kind, ContainerKind::Archive(ArchiveKind::Zip));
    assert_eq!(r.path, "deep.txt");
    assert_eq!(read_all(&*r.fs, &r.path), b"nested!");
    assert!(r.read_only.unwrap().contains("inside other archives"));
    // The nested zip opened as a folder.
    let r = archives
        .resolve(&format!("{}/lib/inner.zip", s(&outer)), true)
        .unwrap()
        .unwrap();
    assert_eq!(names(&*r.fs, &r.path), ["deep.txt"]);
}

#[test]
fn zip_write_delete_rename_round_trip() {
    let (dir, archives) = setup();
    let zip = dir.path().join("w.zip");
    write_zip(&zip, FILES);
    let r = archives.resolve(&s(&zip), true).unwrap().unwrap();
    let fs_ = r.fs;
    assert!(fs_.caps().rename);

    // Replace, add, and add into a new folder.
    fs_.write("readme.txt", 0, &mut &b"replaced"[..], &mut |_| Ok(()))
        .unwrap();
    fs_.create_dir_all("new/empty").unwrap();
    let mut seen = 0;
    let n = fs_
        .write("new/file.txt", 0, &mut &b"brand new"[..], &mut |b| {
            seen += b;
            Ok(())
        })
        .unwrap();
    assert_eq!((n, seen), (9, 9));
    assert_eq!(read_all(&*fs_, "readme.txt"), b"replaced");
    assert_eq!(names(&*fs_, "new"), ["empty", "file.txt"]);

    // Untouched entries keep their bytes (copied without recompressing).
    assert_eq!(read_all(&*fs_, "src/deep/data.bin"), FILES[2].1);

    fs_.set_modified("new/file.txt", 1_600_000_000_000).unwrap();
    let t = fs_
        .stat("new/file.txt")
        .unwrap()
        .unwrap()
        .modified_ms
        .unwrap();
    assert!((t - 1_600_000_000_000).abs() <= 2000, "{t}");

    fs_.rename("src", "code").unwrap();
    assert_eq!(names(&*fs_, ""), ["code", "new", "readme.txt"]);
    assert_eq!(read_all(&*fs_, "code/main.rs"), FILES[1].1);

    fs_.remove_file("readme.txt").unwrap();
    fs_.remove_file("readme.txt").unwrap(); // already gone is fine
    fs_.remove_dir_all("code").unwrap();
    assert_eq!(names(&*fs_, ""), ["new"]);

    // Another tool sees a valid zip with what we wrote.
    let mut z = zip::ZipArchive::new(File::open(&zip).unwrap()).unwrap();
    let mut s = String::new();
    z.by_name("new/file.txt")
        .unwrap()
        .read_to_string(&mut s)
        .unwrap();
    assert_eq!(s, "brand new");
    assert!(z.by_name("new/empty/").is_ok());
}

#[test]
fn zip_takes_writes_from_several_threads() {
    // A copy into an archive has its walk looking at the archive while a
    // worker writes; nothing may keep the old archive open while a write
    // swaps the new one in (Windows: "Access is denied").
    let (dir, archives) = setup();
    let zip = dir.path().join("many.zip");
    write_zip(&zip, FILES);
    let r = archives.resolve(&s(&zip), true).unwrap().unwrap();
    let fs_ = &*r.fs;
    let done = AtomicBool::new(false);
    std::thread::scope(|scope| {
        scope.spawn(|| {
            while !done.load(Ordering::Relaxed) {
                assert!(fs_.stat("readme.txt").unwrap().is_some());
            }
        });
        let writers: Vec<_> = (0..4)
            .map(|t| {
                scope.spawn(move || {
                    for i in 0..5 {
                        fs_.create_dir_all(&format!("t{t}")).unwrap();
                        let body = format!("{t}.{i}");
                        fs_.write(
                            &format!("t{t}/{i}.txt"),
                            0,
                            &mut body.as_bytes(),
                            &mut |_| Ok(()),
                        )
                        .unwrap();
                    }
                })
            })
            .collect();
        for w in writers {
            w.join().unwrap();
        }
        done.store(true, Ordering::Relaxed);
    });
    assert_eq!(
        names(fs_, ""),
        ["readme.txt", "src", "t0", "t1", "t2", "t3"]
    );
    for t in 0..4 {
        for i in 0..5 {
            assert_eq!(
                read_all(fs_, &format!("t{t}/{i}.txt")),
                format!("{t}.{i}").as_bytes()
            );
        }
    }
}

#[test]
fn one_zip_under_two_spellings_takes_one_write_at_a_time() {
    // `a.zip/v1` beside `./a.zip/v2` in a script: each spelling has its own
    // label, but both must queue on one lock, or two rewrites race and the
    // later swap drops the other's change.
    let (dir, archives) = setup();
    let zip = dir.path().join("a.zip");
    write_zip(&zip, FILES);
    let plain = s(&zip);
    let dotted = format!("{}/./a.zip", s(dir.path()));
    let mut spellings = vec![plain.clone(), dotted.clone()];
    // A case-insensitive disk (macOS, Windows) reaches it as `A.ZIP` too.
    let upper = format!("{}/A.ZIP", s(dir.path()));
    if Path::new(&upper).is_file() {
        spellings.push(upper);
    }
    #[cfg(unix)]
    {
        let link = dir.path().join("link.zip");
        std::os::unix::fs::symlink(&zip, &link).unwrap();
        spellings.push(s(&link));
    }
    let views: Vec<_> = spellings
        .iter()
        .map(|p| archives.resolve(p, true).unwrap().unwrap().fs)
        .collect();

    // Errors name the entry under the path as it was written.
    for (view, label) in views.iter().zip(&spellings) {
        let err = view.open("missing.txt").err().unwrap().to_string();
        assert!(err.contains(&format!("{label}/missing.txt")), "{err}");
    }

    std::thread::scope(|scope| {
        for (t, view) in views.iter().enumerate() {
            scope.spawn(move || {
                for i in 0..6 {
                    let body = format!("{t}.{i}");
                    view.write(
                        &format!("t{t}/{i}.txt"),
                        0,
                        &mut body.as_bytes(),
                        &mut |_| Ok(()),
                    )
                    .unwrap();
                }
            });
        }
    });
    // Nothing lost, and every spelling sees every write.
    for view in &views {
        for t in 0..spellings.len() {
            for i in 0..6 {
                assert_eq!(
                    read_all(&**view, &format!("t{t}/{i}.txt")),
                    format!("{t}.{i}").as_bytes()
                );
            }
        }
    }
    // The symlink is still a symlink: the rewrite replaced the zip it points at.
    #[cfg(unix)]
    assert!(fs::symlink_metadata(dir.path().join("link.zip"))
        .unwrap()
        .file_type()
        .is_symlink());
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

#[test]
fn zip_versus_zip_compares_by_header_crcs() {
    let (dir, archives) = setup();
    let (a, b) = (dir.path().join("a.zip"), dir.path().join("b.zip"));
    write_zip(&a, FILES);
    let mut changed = FILES.to_vec();
    changed[1] = ("src/main.rs", b"fn main() { 1 }\n");
    write_zip(&b, &changed);
    let (l, r) = (
        archives.resolve(&s(&a), true).unwrap().unwrap(),
        archives.resolve(&s(&b), true).unwrap().unwrap(),
    );
    let opts = FolderCompareOptions {
        content: ContentMethod::Crc,
        compare_timestamp: false,
        ..Default::default()
    };
    let cache = StoredChecksums {
        sides: [l.crcs.clone(), r.crcs.clone()],
        inner: None,
    };
    let control = ScanControl {
        cache: Some(&cache),
        ..Default::default()
    };
    let res = folder::compare_folders_controlled(&*l.fs, &l.path, &*r.fs, &r.path, &opts, control)
        .unwrap();
    assert_eq!(find(&res.entries, "readme.txt").status, EntryStatus::Same);
    assert_eq!(
        find(&res.entries, "src/main.rs").status,
        EntryStatus::Different
    );
    // Every checksum came from the headers.
    assert_eq!(res.stats.checksums_reused, 6);
}

#[test]
fn expanding_archives_inside_a_folder_tree() {
    let (dir, archives) = setup();
    let (left, right) = (dir.path().join("L"), dir.path().join("R"));
    fs::create_dir_all(&left).unwrap();
    fs::create_dir_all(&right).unwrap();
    write_zip(&left.join("pkg.jar"), FILES);
    let mut changed = FILES.to_vec();
    changed[0] = ("readme.txt", b"other text\n");
    write_zip(&right.join("pkg.jar"), &changed);

    let lfs = archives.expand(Arc::new(LocalFs), true, None);
    let rfs = archives.expand(Arc::new(LocalFs), true, None);
    let opts = FolderCompareOptions {
        content: ContentMethod::Binary,
        compare_timestamp: false,
        ..Default::default()
    };
    let res = folder::compare_folders_with(&lfs, &s(&left), &rfs, &s(&right), &opts).unwrap();
    let jar = find(&res.entries, "pkg.jar");
    assert!(jar.is_dir);
    assert_eq!(
        find(&res.entries, "pkg.jar/readme.txt").status,
        EntryStatus::Different
    );
    assert_eq!(
        find(&res.entries, "pkg.jar/src/deep/data.bin").status,
        EntryStatus::Same
    );

    // Paths built from the result open through the archive resolver.
    let p = format!("{}/pkg.jar/src/main.rs", s(&left));
    assert_eq!(
        read_all(
            &lfs,
            &LocalFs.join(&LocalFs.join(&s(&left), "pkg.jar"), "src/main.rs")
        ),
        FILES[1].1
    );
    assert_eq!(
        fs::read(archives.local_copy(&p).unwrap().unwrap()).unwrap(),
        FILES[1].1
    );
    assert_eq!(
        lfs.stored_crc(&LocalFs.join(&s(&left), "pkg.jar/readme.txt")),
        Some(crc32fast::hash(FILES[0].1))
    );

    // Without expansion the archives are plain files.
    let res =
        folder::compare_folders_with(&LocalFs, &s(&left), &LocalFs, &s(&right), &opts).unwrap();
    assert!(!find(&res.entries, "pkg.jar").is_dir);
}

fn sample_tree(root: &Path) -> PathBuf {
    for (name, data) in FILES {
        let p = root.join(name);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, data).unwrap();
    }
    root.to_path_buf()
}

#[test]
fn snapshot_save_load_and_compare() {
    let (dir, archives) = setup();
    let src = sample_tree(&dir.path().join("src-tree"));
    let out = dir.path().join("before.pcsnap");
    let (snap, sum) = snapshot::take(
        &LocalFs,
        &s(&src),
        &s(&src),
        SnapshotOptions { include_crcs: true },
        None,
        None,
    )
    .unwrap();
    assert_eq!(
        (sum.files, sum.folders, sum.bytes),
        (3, 2, FILES.iter().map(|f| f.1.len() as u64).sum())
    );
    snapshot::save(&snap, &out).unwrap();
    // Gzip-compressed on disk.
    assert_eq!(&fs::read(&out).unwrap()[..2], &[0x1f, 0x8b]);

    // Change the folder after the snapshot.
    fs::write(src.join("src/main.rs"), b"fn main() { changed(); }\n").unwrap();
    fs::write(src.join("new.txt"), b"new").unwrap();

    let snap_side = archives.resolve(&s(&out), true).unwrap().unwrap();
    assert_eq!(snap_side.kind, ContainerKind::Snapshot);
    assert_eq!(names(&*snap_side.fs, ""), ["readme.txt", "src"]);
    // No contents: reading reports it instead of failing obscurely.
    let err = snap_side.fs.open("readme.txt").err().unwrap().to_string();
    assert!(err.contains(snapshot::CONTENT_UNAVAILABLE), "{err}");
    assert!(archives
        .local_copy(&format!("{}/readme.txt", s(&out)))
        .is_err());
    assert!(snap_side
        .fs
        .write("x", 0, &mut &b""[..], &mut |_| Ok(()))
        .is_err());

    let base = FolderCompareOptions {
        content: ContentMethod::Binary,
        compare_timestamp: false,
        ..Default::default()
    };
    let (opts, notice) =
        pcompare_archive::compare::options_for_snapshots(&base, [Some(true), None]);
    assert!(notice.is_some());
    let cache = StoredChecksums {
        sides: [snap_side.crcs.clone(), None],
        inner: None,
    };
    let control = ScanControl {
        cache: Some(&cache),
        ..Default::default()
    };
    let res =
        folder::compare_folders_controlled(&*snap_side.fs, "", &LocalFs, &s(&src), &opts, control)
            .unwrap();
    assert_eq!(find(&res.entries, "readme.txt").status, EntryStatus::Same);
    assert_eq!(
        find(&res.entries, "src/main.rs").status,
        EntryStatus::Different
    );
    assert_eq!(find(&res.entries, "new.txt").status, EntryStatus::RightOnly);
    assert_eq!(res.stats.errors, 0);

    // Without CRCs: size and date only, still no errors.
    let (snap, _) = snapshot::take(
        &LocalFs,
        &s(&src),
        &s(&src),
        SnapshotOptions::default(),
        None,
        None,
    )
    .unwrap();
    assert!(snap.entries.iter().all(|e| e.crc.is_none()));
    let plain = dir.path().join("plain.pcsnap");
    snapshot::save(&snap, &plain).unwrap();
    let side = archives.resolve(&s(&plain), true).unwrap().unwrap();
    let (opts, _) = pcompare_archive::compare::options_for_snapshots(&base, [Some(false), None]);
    let res = folder::compare_folders_with(&*side.fs, "", &LocalFs, &s(&src), &opts).unwrap();
    assert_eq!(res.stats.errors, 0);
    assert_eq!(find(&res.entries, "readme.txt").status, EntryStatus::Same);
}
