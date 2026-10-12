//! Tauri commands for the Registry, Version and MP3 Compare sessions.

use crate::CmdResult;
use pcompare_core::CoreError;
use pcompare_meta::audio::{self, AudioCompareResult};
use pcompare_meta::registry::{
    self, EditableSource, RegEdit, RegTree, RegistryCompareResult, Side, SideEdit, SourceKind,
};
use pcompare_meta::version::{self, VersionCompareResult};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use tauri::AppHandle;

/// Live keys read recently, so replaying queued `.reg` edits against a live
/// other side doesn't export the key again on every edit.
static LIVE_CACHE: Mutex<Vec<(String, RegTree)>> = Mutex::new(Vec::new());
const LIVE_CACHE_CAPACITY: usize = 4;

/// Engine errors name the file they read; for remote paths that is the
/// download cache, so swap in what the user actually typed.
fn relabel(e: CoreError, local: &Path, shown: &str) -> CoreError {
    let local = local.to_string_lossy();
    if local == shown {
        return e;
    }
    CoreError::Other(e.to_string().replace(local.as_ref(), shown))
}

fn local_pair(
    app: &AppHandle,
    left: &str,
    right: &str,
) -> pcompare_core::Result<(PathBuf, PathBuf)> {
    crate::with_remote(app, [left, right], |r| {
        Ok((r.local_copy(left, true)?, r.local_copy(right, true)?))
    })
}

fn live_spec(spec: &str) -> Option<registry::LiveSpec> {
    (!pcompare_remote::is_remote(spec))
        .then(|| registry::classify_source(spec))
        .flatten()
}

/// Live key specs (`HKLM\...`, `\\PC\HKLM\...`) are read from the registry;
/// anything else is a `.reg` file, possibly on a remote profile. `reload`
/// re-reads live keys and re-downloads remote files instead of reusing
/// what an earlier call read.
fn load_registry(
    app: &AppHandle,
    spec: &str,
    reload: bool,
) -> pcompare_core::Result<EditableSource> {
    match live_spec(spec) {
        Some(key) => {
            let kind = if key.machine.is_some() {
                SourceKind::Remote
            } else {
                SourceKind::Live
            };
            let mut cache = LIVE_CACHE.lock().unwrap_or_else(|e| e.into_inner());
            let cached = cache.iter().position(|(s, _)| s == spec);
            let tree = match cached {
                Some(i) if !reload => cache[i].1.clone(),
                _ => {
                    // Don't hold the lock while reg.exe runs.
                    drop(cache);
                    let tree = registry::load_live(&key)?;
                    cache = LIVE_CACHE.lock().unwrap_or_else(|e| e.into_inner());
                    cache.retain(|(s, _)| s != spec);
                    cache.insert(0, (spec.to_owned(), tree.clone()));
                    cache.truncate(LIVE_CACHE_CAPACITY);
                    tree
                }
            };
            Ok(EditableSource::live(spec.to_owned(), kind, tree))
        }
        None => {
            let local = crate::with_remote(app, [spec, ""], |r| r.local_copy(spec, reload))?;
            let bytes = std::fs::read(&local)
                .map_err(|e| relabel(CoreError::io(&local, e), &local, spec))?;
            EditableSource::file(spec.to_owned(), &bytes)
                .map_err(|e| CoreError::Other(format!("{spec}: {e}")))
        }
    }
}

/// Loads both sides and replays the queued `.reg` edits onto them.
fn load_edited(
    app: &AppHandle,
    left: &str,
    right: &str,
    edits: &[SideEdit],
    reload: bool,
) -> pcompare_core::Result<(EditableSource, EditableSource)> {
    let mut l = load_registry(app, left, reload)?;
    let mut r = load_registry(app, right, reload)?;
    registry::apply_edits(&mut l, &mut r, edits)?;
    Ok((l, r))
}

/// Compares two registry sources after replaying `edits` (unsaved `.reg`
/// changes, oldest first). `reload` (default true) re-reads live keys.
#[tauri::command]
pub async fn compare_registry(
    app: AppHandle,
    left: String,
    right: String,
    edits: Option<Vec<SideEdit>>,
    reload: Option<bool>,
) -> CmdResult<RegistryCompareResult> {
    crate::blocking(move || {
        let (l, r) = load_edited(
            &app,
            &left,
            &right,
            &edits.unwrap_or_default(),
            reload.unwrap_or(true),
        )?;
        Ok(registry::compare_sources(l.into_loaded(), r.into_loaded()))
    })
    .await
}

/// Writes the `.reg` files of `sides` with the queued edits applied, each
/// in its original format (REGEDIT4 / version 5, encoding, line endings).
/// All sides are computed from one replay before anything is written.
#[tauri::command]
pub async fn registry_save(
    app: AppHandle,
    left: String,
    right: String,
    edits: Vec<SideEdit>,
    sides: Vec<Side>,
) -> CmdResult<()> {
    crate::blocking(move || {
        for spec in sides.iter().map(|s| match s {
            Side::Left => &left,
            Side::Right => &right,
        }) {
            if pcompare_remote::is_remote(spec) {
                return Err(CoreError::Other(format!(
                    "{spec}: remote files are read-only"
                )));
            }
        }
        let (l, r) = load_edited(&app, &left, &right, &edits, false)?;
        let mut writes = Vec::new();
        for side in &sides {
            let source = match side {
                Side::Left => &l,
                Side::Right => &r,
            };
            let doc = source.document().ok_or_else(|| {
                CoreError::Other(format!(
                    "{} is a live registry key, not a .reg file",
                    source.spec
                ))
            })?;
            let bytes = doc
                .to_bytes()
                .map_err(|e| CoreError::Other(format!("{}: {e}", source.spec)))?;
            writes.push((source.spec.clone(), bytes));
        }
        for (spec, bytes) in writes {
            std::fs::write(&spec, bytes).map_err(|e| CoreError::io(spec.as_str(), e))?;
        }
        Ok(())
    })
    .await
}

/// The `reg.exe` steps that would apply `edit` to the live key on `side`
/// (after replaying the queued `.reg` edits, which copies may read from).
fn live_steps(
    app: &AppHandle,
    left: &str,
    right: &str,
    edits: &[SideEdit],
    side: Side,
    edit: &RegEdit,
) -> pcompare_core::Result<Vec<Vec<String>>> {
    let (l, r) = load_edited(app, left, right, edits, false)?;
    let (target, source) = match side {
        Side::Left => (l, r),
        Side::Right => (r, l),
    };
    let spec = live_spec(&target.spec)
        .filter(|_| target.document().is_none())
        .ok_or_else(|| CoreError::Other(format!("{} is not a live registry key", target.spec)))?;
    registry::live_plan(&spec, target.tree(), source.tree(), edit)
}

/// Previews a live-registry edit as `reg …` command lines for confirmation.
#[tauri::command]
pub async fn registry_live_plan(
    app: AppHandle,
    left: String,
    right: String,
    edits: Vec<SideEdit>,
    side: Side,
    edit: RegEdit,
) -> CmdResult<Vec<String>> {
    crate::blocking(move || {
        let steps = live_steps(&app, &left, &right, &edits, side, &edit)?;
        Ok(steps.iter().map(|s| registry::format_command(s)).collect())
    })
    .await
}

/// Applies an edit to a live key with `reg add` / `reg delete` (Windows only).
#[tauri::command]
pub async fn registry_apply_live(
    app: AppHandle,
    left: String,
    right: String,
    edits: Vec<SideEdit>,
    side: Side,
    edit: RegEdit,
) -> CmdResult<()> {
    crate::blocking(move || {
        let steps = live_steps(&app, &left, &right, &edits, side, &edit)?;
        registry::run_live_plan(&steps)
    })
    .await
}

#[tauri::command]
pub async fn compare_versions(
    app: AppHandle,
    left: String,
    right: String,
) -> CmdResult<VersionCompareResult> {
    crate::blocking(move || {
        let (lp, rp) = local_pair(&app, &left, &right)?;
        version::compare_executables(&lp, &rp)
            .map_err(|e| relabel(relabel(e, &lp, &left), &rp, &right))
    })
    .await
}

#[tauri::command]
pub async fn compare_audio(
    app: AppHandle,
    left: String,
    right: String,
) -> CmdResult<AudioCompareResult> {
    crate::blocking(move || {
        let (lp, rp) = local_pair(&app, &left, &right)?;
        audio::compare_audio(&lp, &rp).map_err(|e| relabel(relabel(e, &lp, &left), &rp, &right))
    })
    .await
}
