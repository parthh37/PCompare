//! Backups of the files PCompare overwrites or removes (see
//! `pcompare_core::backups`). The setting lives in `prefs.json` (`backups`),
//! so background syncs follow the same choice as the UI. Every tool that
//! writes over a file asks [`begin`] for a batch first; with backups off it
//! gets `None` and keeps its own behaviour (Trash, `.bak`, versioning).

use crate::{blocking, CmdResult};
use pcompare_core::backups::{
    Backup, BackupBatch, BackupOrigin, BackupSettings, BackupStore, PruneReport, RestoreOutcome,
};
use pcompare_core::store::now_ms;
use pcompare_sync::exec::Disposal;
use pcompare_sync::fsops::{BackupDeleter, Deleter};
use pcompare_sync::sync::SyncMode;
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use tauri::{AppHandle, Emitter, Manager};

/// Sent with a backup's id whenever one is made, restored or removed.
pub const CHANGED_EVENT: &str = "backups-changed";

/// Clean-ups run at most this often (and once at startup).
const PRUNE_EVERY_MS: i64 = 60 * 60 * 1000;

pub fn settings(app: &AppHandle) -> BackupSettings {
    BackupSettings::from_prefs(&crate::workspace::prefs_value(app))
}

/// `Backups` in the app's data folder.
fn default_root(app: &AppHandle) -> PathBuf {
    app.path()
        .app_data_dir()
        .unwrap_or_else(|_| std::env::temp_dir().join("PCompare"))
        .join("Backups")
}

fn store(app: &AppHandle, settings: &BackupSettings) -> BackupStore {
    BackupStore::new(settings.root(&default_root(app)))
}

/// A batch for one operation's old versions, or `None` when backups are off.
pub fn begin(app: &AppHandle, origin: BackupOrigin) -> Option<Arc<BackupBatch>> {
    let s = settings(app);
    s.enabled.then(|| Arc::new(store(app, &s).begin(origin)))
}

/// Call once an operation is done: tells the UI about a new backup and
/// tidies the folder up now and then.
pub fn finish(app: &AppHandle, batch: &BackupBatch) {
    if let Some(id) = batch.id() {
        let _ = app.emit(CHANGED_EVENT, id);
        prune_soon(app);
    }
}

/// "Save report.txt" by `tool`, for saves whose caller didn't describe them.
pub fn save_origin(tool: &str, path: &str) -> BackupOrigin {
    let name = Path::new(path).file_name().map_or_else(|| path.to_owned(), |n| n.to_string_lossy().into_owned());
    BackupOrigin {
        tool: tool.to_owned(),
        action: format!("Save {name}"),
        left: String::new(),
        right: String::new(),
    }
}

/// A copy of one file kept before it was saved over.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KeptCopy {
    /// The backup's id (for the Backups page).
    pub id: String,
    /// Where the copy is.
    pub path: String,
}

/// Keeps a copy of `path` before a save writes over it. `None` when backups
/// are off or there is no file there yet.
pub fn keep_before_save(app: &AppHandle, path: &Path, origin: BackupOrigin) -> pcompare_core::Result<Option<KeptCopy>> {
    let Some(batch) = begin(app, origin) else {
        return Ok(None);
    };
    let kept = batch
        .keep_copy(path)
        .map_err(|e| pcompare_core::CoreError::Other(format!("couldn't back up {} first: {e}", path.display())))?;
    finish(app, &batch);
    Ok(kept.zip(batch.id()).map(|(p, id)| KeptCopy {
        id,
        path: p.to_string_lossy().into_owned(),
    }))
}

/// The deleter for a sync or merge run. With backups on, what would go to
/// the Recycle Bin / Trash is kept as a backup instead (deletions too); a
/// versioning folder or permanent deletion the user chose still applies.
/// `roots` names each local side's root for [`Deleter::retire`].
pub fn run_deleter(
    app: &AppHandle,
    disposal: &Disposal,
    stamp: &str,
    origin: BackupOrigin,
    roots: &[(&str, &Path)],
) -> (Box<dyn Deleter>, Option<Arc<BackupBatch>>) {
    if matches!(disposal, Disposal::Trash) {
        if let Some(batch) = begin(app, origin) {
            let deleter = roots
                .iter()
                .fold(BackupDeleter::new(batch.clone()), |d, (side, root)| d.with_root(side, *root));
            return (Box::new(deleter), Some(batch));
        }
    }
    (disposal.deleter(stamp), None)
}

/// A sync mode as the Backups page describes the run.
pub fn sync_action(mode: SyncMode) -> &'static str {
    match mode {
        SyncMode::UpdateLeftToRight => "Update left → right",
        SyncMode::UpdateRightToLeft => "Update right → left",
        SyncMode::UpdateBoth => "Update both sides",
        SyncMode::MirrorLeftToRight => "Mirror left → right",
        SyncMode::MirrorRightToLeft => "Mirror right → left",
        SyncMode::TwoWay => "Two-way sync",
        SyncMode::Custom => "Custom sync",
    }
}

/// Removes expired backups in the background, unless that ran recently.
pub fn prune_soon(app: &AppHandle) {
    static LAST: AtomicI64 = AtomicI64::new(0);
    let now = now_ms();
    let last = LAST.load(Ordering::Relaxed);
    if now - last < PRUNE_EVERY_MS || LAST.compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed).is_err() {
        return;
    }
    let app = app.clone();
    std::thread::spawn(move || {
        let s = settings(&app);
        if let Ok(r) = store(&app, &s).prune(&s, now_ms()) {
            if r.removed > 0 {
                let _ = app.emit(CHANGED_EVENT, "");
            }
        }
    });
}

// ---------- commands ----------

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupsInfo {
    /// Where backups go now.
    folder: String,
    /// Where they go when no folder is chosen.
    default_folder: String,
}

#[tauri::command]
pub fn backups_info(app: AppHandle) -> BackupsInfo {
    BackupsInfo {
        folder: settings(&app).root(&default_root(&app)).to_string_lossy().into_owned(),
        default_folder: default_root(&app).to_string_lossy().into_owned(),
    }
}

/// Every backup in the current folder, newest first.
#[tauri::command]
pub async fn backups_list(app: AppHandle) -> CmdResult<Vec<Backup>> {
    blocking(move || store(&app, &settings(&app)).list()).await
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RestoreReport {
    outcomes: Vec<RestoreOutcome>,
    /// The backup of what the restore replaced, so it can be undone.
    undo: Option<String>,
}

/// Puts the items of backup `id` named by `stored` (all when empty) back.
/// What is there now is backed up first, even when backups are off.
#[tauri::command]
pub async fn backups_restore(app: AppHandle, id: String, stored: Vec<String>) -> CmdResult<RestoreReport> {
    blocking(move || {
        let s = settings(&app);
        let store = store(&app, &s);
        let source = store.get(&id)?;
        let before = store.begin(BackupOrigin {
            tool: "Backups".into(),
            action: format!("Before restoring {}", restored_what(&source, &stored)),
            left: String::new(),
            right: String::new(),
        });
        let outcomes = store.restore(&id, &stored, &before)?;
        let undo = before.id();
        let _ = app.emit(CHANGED_EVENT, undo.clone().unwrap_or_default());
        Ok(RestoreReport { outcomes, undo })
    })
    .await
}

/// "report.txt", "3 files", … for the restore's own backup.
fn restored_what(b: &Backup, stored: &[String]) -> String {
    let picked: Vec<_> = b
        .items
        .iter()
        .filter(|i| stored.is_empty() || stored.contains(&i.stored))
        .collect();
    match picked.as_slice() {
        [one] => Path::new(&one.original)
            .file_name()
            .map_or_else(|| one.original.clone(), |n| n.to_string_lossy().into_owned()),
        many => format!("{} items", many.len()),
    }
}

#[tauri::command]
pub async fn backups_delete(app: AppHandle, ids: Vec<String>) -> CmdResult<()> {
    blocking(move || {
        let store = store(&app, &settings(&app));
        for id in &ids {
            store.delete(id)?;
        }
        let _ = app.emit(CHANGED_EVENT, "");
        Ok(())
    })
    .await
}

/// Applies the keep-for and size settings now.
#[tauri::command]
pub async fn backups_prune(app: AppHandle) -> CmdResult<PruneReport> {
    blocking(move || {
        let s = settings(&app);
        let r = store(&app, &s).prune(&s, now_ms())?;
        let _ = app.emit(CHANGED_EVENT, "");
        Ok(r)
    })
    .await
}
