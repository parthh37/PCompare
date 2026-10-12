//! bc5:archives — archive paths through the RemoteManager routing, and
//! Folder Sync writing into a zip archive.

use std::io::Read;
use std::path::Path;
use std::sync::atomic::AtomicBool;

use pcompare_core::folder::{
    compare_folders_with, ContentMethod, EntryStatus, FolderCompareOptions,
};
use pcompare_remote::profile::MemorySecrets;
use pcompare_remote::RemoteManager;
use pcompare_sync::exec::{execute_on, ExecOptions};
use pcompare_sync::fsops::PermanentDeleter;
use pcompare_sync::journal::NullJournal;
use pcompare_sync::remote::Root;
use pcompare_sync::sync::{plan_sync_with, PlanOptions, SyncMode};

/// An empty zip: just the end-of-central-directory record.
const EMPTY_ZIP: [u8; 22] = [
    0x50, 0x4b, 0x05, 0x06, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
];

fn manager(dir: &Path) -> RemoteManager {
    RemoteManager::new(dir, dir.join("cache"), Box::<MemorySecrets>::default())
}

fn write(root: &Path, rel: &str, data: &[u8]) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, data).unwrap();
}

#[test]
fn folder_sync_mirrors_into_a_zip_and_paths_route_inside_it() {
    let tmp = tempfile::tempdir().unwrap();
    let m = manager(tmp.path());
    let src = tmp.path().join("src");
    write(&src, "a.txt", b"one");
    write(&src, "sub/b.txt", b"two");
    write(&src, "sub/deeper/c.bin", &[0, 1, 2, 3]);
    let zip = tmp.path().join("out.zip");
    std::fs::write(&zip, EMPTY_ZIP).unwrap();
    let (src_s, zip_s) = (
        src.to_string_lossy().into_owned(),
        zip.to_string_lossy().into_owned(),
    );

    // A bare archive is a file, except where a folder is expected.
    let (_, p) = m.target(&zip_s).unwrap();
    assert_eq!(p, zip_s);
    assert!(m.target_write(&zip_s).unwrap().is_none());
    assert!(pcompare_remote::is_container_path(&zip_s));
    assert!(!pcompare_remote::is_inside_container(&zip_s));
    assert!(!pcompare_remote::is_container_path(&src_s));

    let (zfs, zrel) = m.target_write_folder(&zip_s).unwrap().unwrap();
    assert_eq!(zrel, "");
    let (lfs, lrel) = m.target_folder(&src_s).unwrap();
    let opts = FolderCompareOptions {
        content: ContentMethod::Binary,
        ..Default::default()
    };
    let plan = plan_sync_with(
        &*lfs,
        &lrel,
        &*zfs,
        &zrel,
        SyncMode::MirrorLeftToRight,
        &opts,
        &PlanOptions::default(),
        None,
    )
    .unwrap();
    assert!(!plan.ops.is_empty());
    let report = execute_on(
        Root::Local(&src),
        Root::Remote {
            fs: &*zfs,
            path: &zrel,
            label: &zip_s,
        },
        &plan.ops,
        &ExecOptions {
            verify: true,
            ..Default::default()
        },
        &PermanentDeleter,
        &mut NullJournal,
        &AtomicBool::new(false),
        &mut |_| {},
    );
    assert_eq!(report.failed, 0, "{:?}", report.results);

    // The zip now matches the folder, by content and by date.
    let (zfs, zrel) = m.target_folder(&zip_s).unwrap();
    let res = compare_folders_with(&*lfs, &lrel, &*zfs, &zrel, &opts).unwrap();
    assert_eq!(
        res.stats.different + res.stats.left_only + res.stats.right_only,
        0,
        "{:?}",
        res.entries
    );
    let by_date =
        compare_folders_with(&*lfs, &lrel, &*zfs, &zrel, &FolderCompareOptions::default()).unwrap();
    assert_eq!(by_date.stats.different, 0, "{:?}", by_date.entries);

    // Paths inside the zip route to it: listing, reading, viewer copies.
    let inner = format!("{zip_s}/sub/b.txt");
    assert!(pcompare_remote::is_inside_container(&inner));
    let (fs, rel) = m.target(&inner).unwrap();
    let mut s = String::new();
    fs.open(&rel).unwrap().read_to_string(&mut s).unwrap();
    assert_eq!(s, "two");
    assert_eq!(
        std::fs::read(m.local_copy(&inner, true).unwrap()).unwrap(),
        b"two"
    );
    let names: Vec<_> = m
        .list(&format!("{zip_s}/sub"))
        .unwrap()
        .into_iter()
        .map(|e| e.name)
        .collect();
    assert_eq!(names, ["deeper", "b.txt"]);

    // Delete through the remote-style API too.
    m.delete(&format!("{zip_s}/sub/deeper")).unwrap();
    let (zfs, zrel) = m.target_folder(&zip_s).unwrap();
    let res = compare_folders_with(&*lfs, &lrel, &*zfs, &zrel, &opts).unwrap();
    assert_eq!(res.stats.left_only, 1);
    let deeper = res.entries.iter().find(|e| e.name == "sub").unwrap();
    assert_eq!(
        deeper
            .children
            .iter()
            .find(|e| e.name == "deeper")
            .unwrap()
            .status,
        EntryStatus::LeftOnly
    );
}
