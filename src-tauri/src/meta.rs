//! Tauri commands for the Registry, Version and MP3 Compare sessions.

use crate::CmdResult;
use pcompare_core::CoreError;
use pcompare_meta::audio::{self, AudioCompareResult};
use pcompare_meta::registry::{self, LoadedSource, RegistryCompareResult, SourceKind};
use pcompare_meta::version::{self, VersionCompareResult};
use std::path::{Path, PathBuf};
use tauri::AppHandle;

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

/// Live key specs (`HKLM\...`, `\\PC\HKLM\...`) are read from the registry;
/// anything else is a `.reg` file, possibly on a remote profile.
fn load_registry(app: &AppHandle, spec: &str) -> pcompare_core::Result<LoadedSource> {
    let live = (!pcompare_remote::is_remote(spec))
        .then(|| registry::classify_source(spec))
        .flatten();
    let (kind, tree) = match live {
        Some(key) => {
            let kind = if key.machine.is_some() {
                SourceKind::Remote
            } else {
                SourceKind::Live
            };
            (kind, registry::load_live(&key)?)
        }
        None => {
            let local = crate::with_remote(app, [spec, ""], |r| r.local_copy(spec, true))?;
            let tree = registry::load_file(&local).map_err(|e| relabel(e, &local, spec))?;
            (SourceKind::File, tree)
        }
    };
    Ok(LoadedSource {
        spec: spec.to_owned(),
        kind,
        tree,
    })
}

#[tauri::command]
pub async fn compare_registry(
    app: AppHandle,
    left: String,
    right: String,
) -> CmdResult<RegistryCompareResult> {
    crate::blocking(move || {
        let l = load_registry(&app, &left)?;
        let r = load_registry(&app, &right)?;
        Ok(registry::compare_sources(l, r))
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
