//! Archives, folder snapshots and Subversion repositories as Folder Compare
//! sides (bc5:archives).
//!
//! Path routing itself lives in `pcompare-remote` (`RemoteManager::target`,
//! `local_copy`, `resolve_container`), so every viewer and Folder Sync
//! already reach into `C:\x\a.zip\dir\file.txt` or `svn:https://…@12`.
//! This module adds what Folder Compare needs on top: archive and snapshot
//! files opened as folders, archives inside a tree expanded on request, the
//! checksums containers store, and saving snapshots.

use std::path::Path;
use std::sync::Arc;

use pcompare_archive::compare::{self, StoredChecksums};
use pcompare_archive::snapshot::{self, SnapshotInfo, SnapshotOptions, SnapshotSummary};
use pcompare_archive::{Archives, ContainerKind, CrcSource};
use pcompare_core::folder::{
    ChecksumCache, ContentMethod, ContentVerdict, EntryStatus, FolderCompareOptions,
};
use pcompare_core::vfs::{EmptyFs, LocalFs, Vfs};
use pcompare_core::{CoreError, PathKind};
use pcompare_remote::RemoteManager;
use serde::Serialize;
use tauri::ipc::Channel;
use tauri::AppHandle;

use crate::{blocking, with_remote, CmdResult};

/// One side of a folder comparison, resolved.
pub struct FolderSide {
    pub fs: Arc<dyn Vfs>,
    pub path: String,
    /// Checksums the side stores (zip / 7z headers, snapshots).
    pub crcs: Option<Arc<dyn CrcSource>>,
    /// For a snapshot: whether it holds CRCs.
    pub snapshot: Option<bool>,
}

/// Resolves a Folder Compare side: a folder (local or remote), or an
/// archive, snapshot or `svn:` repository opened as one. With `expand`,
/// archives inside the tree list as folders too.
pub fn folder_side(
    r: &RemoteManager,
    path: &str,
    expand: bool,
) -> pcompare_core::Result<FolderSide> {
    if path.is_empty() {
        // A side with no folder chosen yet lists as empty, so the other
        // side's contents show as soon as one folder is picked.
        return Ok(FolderSide {
            fs: Arc::new(EmptyFs),
            path: String::new(),
            crcs: None,
            snapshot: None,
        });
    }
    let resolved = r.resolve_container(path, true)?;
    let svn = resolved
        .as_ref()
        .is_some_and(|c| c.kind == ContainerKind::Svn);
    let (fs, rel, crcs, snapshot, container) = match resolved {
        Some(res) => {
            let snap = res.snapshot.as_ref().map(|s| s.include_crcs);
            (res.fs as Arc<dyn Vfs>, res.path, res.crcs, snap, true)
        }
        None => {
            let (fs, rel) = r.target(path)?;
            (fs, rel, None, None, false)
        }
    };
    // Snapshots have no archive contents to expand; svn would need a
    // checkout of every archive.
    if !expand || snapshot.is_some() || svn {
        return Ok(FolderSide {
            fs,
            path: rel,
            crcs,
            snapshot,
        });
    }
    let local = !container && !pcompare_remote::is_remote(path);
    let expanded = Arc::new(r.archives().expand(fs, local, crcs));
    Ok(FolderSide {
        crcs: Some(expanded.clone()),
        fs: expanded,
        path: rel,
        snapshot: None,
    })
}

/// The path to key the scan database's checksum cache on: containers keep
/// their own checksums (and their paths aren't files on disk), so they get
/// none.
pub fn cache_root(path: &str) -> &str {
    if pcompare_remote::is_container_path(path) {
        ""
    } else {
        path
    }
}

/// Content settings adjusted for snapshot sides (which have no contents).
pub fn options_for_sides(
    options: &FolderCompareOptions,
    l: &FolderSide,
    r: &FolderSide,
) -> FolderCompareOptions {
    compare::options_for_snapshots(options, [l.snapshot, r.snapshot]).0
}

/// The checksum cache for a comparison: stored checksums for container
/// sides, `inner` (the scan database) for the rest. `None` when neither
/// side stores checksums.
pub fn stored_checksums<'a>(
    l: &FolderSide,
    r: &FolderSide,
    inner: Option<&'a dyn ChecksumCache>,
) -> Option<StoredChecksums<'a>> {
    (l.crcs.is_some() || r.crcs.is_some()).then(|| StoredChecksums {
        sides: [l.crcs.clone(), r.crcs.clone()],
        inner,
    })
}

/// "Compare contents" on one file pair where a side is in a container that
/// stores checksums (or is a snapshot, which can only be compared that
/// way). `None` when the usual comparison applies.
#[allow(clippy::too_many_arguments)]
pub fn stored_verdict(
    r: &RemoteManager,
    left: &str,
    left_size: u64,
    right: &str,
    right_size: u64,
    method: ContentMethod,
) -> pcompare_core::Result<Option<ContentVerdict>> {
    let (lc, rc) = (
        r.resolve_container(left, false)?,
        r.resolve_container(right, false)?,
    );
    if lc.is_none() && rc.is_none() {
        return Ok(None);
    }
    stored_pair(r, left, lc, left_size, right, rc, right_size, method)
}

#[allow(clippy::too_many_arguments)]
fn stored_pair(
    r: &RemoteManager,
    left: &str,
    lc: Option<pcompare_archive::Resolved>,
    left_size: u64,
    right: &str,
    rc: Option<pcompare_archive::Resolved>,
    right_size: u64,
    method: ContentMethod,
) -> pcompare_core::Result<Option<ContentVerdict>> {
    let is_snap =
        |c: &Option<pcompare_archive::Resolved>| c.as_ref().is_some_and(|c| c.snapshot.is_some());
    let snapshots = is_snap(&lc) || is_snap(&rc);
    // Only a CRC compare can use stored checksums; snapshots can't be read
    // any other way.
    if method == ContentMethod::Off || (method != ContentMethod::Crc && !snapshots) {
        return Ok(None);
    }
    let crc = |c: &Option<pcompare_archive::Resolved>, shown: &str| -> pcompare_core::Result<u32> {
        if let Some(c) = c {
            if let Some(v) = c.crcs.as_ref().and_then(|s| s.stored_crc(&c.path)) {
                return Ok(v);
            }
            if c.snapshot.is_some() {
                return Err(CoreError::Other(format!(
                    "{shown}: {} — the snapshot was saved without CRCs",
                    snapshot::CONTENT_UNAVAILABLE
                )));
            }
            return snapshot::crc32(&*c.fs, &c.path);
        }
        let (fs, p) = r.target(shown)?;
        snapshot::crc32(&*fs, &p)
    };
    let pair = [crc(&lc, left)?, crc(&rc, right)?];
    let same = left_size == right_size && pair[0] == pair[1];
    Ok(Some(ContentVerdict {
        status: if same {
            EntryStatus::Same
        } else {
            EntryStatus::Different
        },
        crc: Some(pair),
    }))
}

// ---------- commands ----------

/// What a path is when used as a folder side, for the Folder Compare banner.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SideInfo {
    /// `None` for an ordinary folder.
    pub container: Option<ContainerKind>,
    /// Why the side can't be written to.
    pub read_only: Option<String>,
    pub snapshot: Option<SnapshotInfo>,
    /// For `svn:` sides: whether the `svn` client is installed
    /// (araxis:vcs — for `git:` / `hg:` sides, `git` / `hg`).
    pub svn_available: Option<bool>,
}

#[tauri::command]
pub async fn archive_side_info(app: AppHandle, path: String) -> CmdResult<SideInfo> {
    blocking(move || {
        if path.is_empty()
            || (!pcompare_remote::is_container_path(&path) && !pcompare_remote::is_remote(&path))
        {
            return Ok(SideInfo {
                container: None,
                read_only: None,
                snapshot: None,
                svn_available: None,
            });
        }
        let svn = pcompare_archive::is_svn(&path);
        let res = with_remote(&app, [&path, ""], |r| r.resolve_container(&path, true))?;
        Ok(SideInfo {
            container: res.as_ref().map(|r| r.kind),
            read_only: res.as_ref().and_then(|r| r.read_only.clone()),
            snapshot: res.and_then(|r| r.snapshot),
            // araxis:vcs
            svn_available: match pcompare_archive::vcs::VcsKind::of(&path) {
                Some(kind) => Some(pcompare_archive::vcs::available(kind)),
                None => svn.then(pcompare_archive::svn::available),
            },
            // araxis:vcs end
        })
    })
    .await
}

/// `path_kind` for paths into archives, snapshots and repositories.
pub async fn path_kind(app: AppHandle, path: String) -> PathKind {
    blocking(move || {
        let r = crate::state(&app);
        match r.remote.resolve_container(&path, false)? {
            None => Ok(pcompare_core::path_kind(&path)),
            Some(res) => Ok(match res.fs.stat(&res.path) {
                Ok(Some(m)) if m.is_dir => PathKind::Directory,
                Ok(Some(_)) => PathKind::File,
                _ => PathKind::Missing,
            }),
        }
    })
    .await
    .unwrap_or(PathKind::Missing)
}

/// Saves a snapshot of `folder` (any folder side: local, remote, archive)
/// to `output`, reporting progress as folders are listed.
#[tauri::command]
pub async fn snapshot_save(
    app: AppHandle,
    folder: String,
    output: String,
    include_crcs: bool,
    on_progress: Channel<SnapshotSummary>,
) -> CmdResult<SnapshotSummary> {
    blocking(move || {
        if output.trim().is_empty() {
            return Err(CoreError::Other(
                "Choose where to save the snapshot.".into(),
            ));
        }
        with_remote(&app, [&folder, ""], |r| {
            let side = folder_side(r, &folder, false)?;
            if side.snapshot.is_some() {
                return Err(CoreError::Other("That is already a snapshot.".into()));
            }
            let report = |s: &SnapshotSummary| {
                let _ = on_progress.send(s.clone());
            };
            let (snap, sum) = snapshot::take(
                &*side.fs,
                &side.path,
                &folder,
                SnapshotOptions { include_crcs },
                Some(&report),
                None,
            )?;
            snapshot::save(&snap, Path::new(&output))?;
            Ok(sum)
        })
    })
    .await
}

// ---------- command line ----------

/// `pcompare --snapshot <folder> <out.pcsnap> [--crc]` (also `/snapshot`):
/// saves a snapshot without opening a window. `None` when the arguments
/// aren't a snapshot request.
pub fn run_snapshot_cli(args: &[String]) -> Option<i32> {
    let is = |a: &String, names: &[&str]| {
        let a = a.trim_start_matches(['-', '/']).to_ascii_lowercase();
        names.contains(&a.as_str())
    };
    let flag = args
        .iter()
        .position(|a| (a.starts_with('-') || a.starts_with('/')) && is(a, &["snapshot"]))?;
    let include_crcs = args
        .iter()
        .any(|a| (a.starts_with('-') || a.starts_with('/')) && is(a, &["crc", "crcs"]));
    let paths: Vec<&String> = args[flag + 1..]
        .iter()
        .filter(|a| !a.starts_with('-'))
        .take(2)
        .collect();
    let [folder, out] = paths[..] else {
        eprintln!("usage: pcompare --snapshot <folder> <output.pcsnap> [--crc]");
        return Some(2);
    };
    Some(match save_snapshot_headless(folder, out, include_crcs) {
        Ok(s) => {
            println!(
                "Saved snapshot of {folder} to {out}: {} files, {} folders, {} bytes{}",
                s.files,
                s.folders,
                s.bytes,
                if s.errors > 0 {
                    format!(" ({} unreadable)", s.errors)
                } else {
                    String::new()
                }
            );
            0
        }
        Err(e) => {
            eprintln!("error: {e}");
            2
        }
    })
}

fn save_snapshot_headless(
    folder: &str,
    out: &str,
    include_crcs: bool,
) -> pcompare_core::Result<SnapshotSummary> {
    if pcompare_remote::is_remote(folder) {
        return Err(CoreError::Other(
            "snapshots of remote folders can be saved from the app (Folder Compare → Snapshot)"
                .into(),
        ));
    }
    let scratch = std::env::temp_dir().join(format!("pcompare-snapshot-{}", std::process::id()));
    let archives = Archives::new(scratch.clone());
    let result = (|| {
        let (fs, root): (Arc<dyn Vfs>, String) = match archives.resolve(folder, true)? {
            Some(r) => (r.fs, r.path),
            None => (Arc::new(LocalFs), folder.to_owned()),
        };
        let opts = SnapshotOptions { include_crcs };
        let (snap, sum) = snapshot::take(&*fs, &root, folder, opts, None, None)?;
        snapshot::save(&snap, Path::new(out))?;
        Ok(sum)
    })();
    let _ = std::fs::remove_dir_all(scratch);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_cli_saves_a_snapshot() {
        let dir = std::env::temp_dir().join(format!("pcompare-snapcli-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("tree/sub")).unwrap();
        std::fs::write(dir.join("tree/sub/a.txt"), "hello").unwrap();
        let out = dir.join("t.pcsnap");
        let args: Vec<String> = [
            "--snapshot",
            &dir.join("tree").to_string_lossy(),
            &out.to_string_lossy(),
            "--crc",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(run_snapshot_cli(&args), Some(0));
        let snap = snapshot::load(&out).unwrap();
        assert!(snap.include_crcs);
        assert_eq!(snap.entries[0].children[0].crc, Some(0x3610_A686));
        assert_eq!(run_snapshot_cli(&["--snapshot".to_string()]), Some(2));
        assert_eq!(run_snapshot_cli(&["a".to_string(), "b".to_string()]), None);
        let _ = std::fs::remove_dir_all(dir);
    }
}
