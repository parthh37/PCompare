//! Tauri commands for the hex compare session. Register each command in main.rs.
//!
//! The stateless `hex_summary` / `hex_chunk` / `hex_next_difference` page
//! through two files by offset. The editable, aligned view keeps a
//! [`HexSession`] per open tab (keyed by an id the UI picks) so edits, undo
//! history and the alignment live here and only visible rows cross the IPC.

use crate::{blocking, with_remote, CmdResult};
use pcompare_core::hex::{self, HexChunk, HexEditAt, HexRow, HexSession, HexState, HexSummary, HexTarget, HexView};
use pcompare_core::backups::BackupOrigin;
use pcompare_core::hex_align::DEFAULT_ALIGN_LIMIT;
use pcompare_core::CoreError;
use serde::Serialize;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tauri::AppHandle;

/// Local paths for a hex session; remote files are downloaded on `refresh`
/// (opening the session) and reused for every subsequent page.
fn hex_paths(app: &AppHandle, left: &str, right: &str, refresh: bool) -> pcompare_core::Result<(PathBuf, PathBuf)> {
    with_remote(app, [left, right], |r| {
        Ok((r.local_copy(left, refresh)?, r.local_copy(right, refresh)?))
    })
}

#[tauri::command]
pub async fn hex_summary(app: AppHandle, left: String, right: String) -> CmdResult<HexSummary> {
    blocking(move || {
        let (l, r) = hex_paths(&app, &left, &right, true)?;
        hex::summarize(&l, &r)
    })
    .await
}

#[tauri::command]
pub async fn hex_chunk(app: AppHandle, left: String, right: String, offset: u64, len: usize) -> CmdResult<HexChunk> {
    let len = len.min(1 << 20); // never ship more than 1 MiB per IPC call
    blocking(move || {
        let (l, r) = hex_paths(&app, &left, &right, false)?;
        hex::read_chunk(&l, &r, offset, len)
    })
    .await
}

#[tauri::command]
pub async fn hex_next_difference(app: AppHandle, left: String, right: String, from: u64) -> CmdResult<Option<u64>> {
    blocking(move || {
        let (l, r) = hex_paths(&app, &left, &right, false)?;
        hex::next_difference(&l, &r, from)
    })
    .await
}

// ---------- editable sessions ----------

struct Open {
    session: HexSession,
    /// The user's paths; remote sides are edited in the download cache and
    /// can't be saved.
    remote: [bool; 2],
    /// The local files open on each side.
    paths: [PathBuf; 2],
}

type Shared = Arc<Mutex<Open>>;

static SESSIONS: Mutex<Option<HashMap<String, Shared>>> = Mutex::new(None);

/// Alignment limits the UI may pick, in bytes.
const MIN_LIMIT: u64 = 1 << 20;
const MAX_LIMIT: u64 = 1 << 30;
/// Most rows one `hex_rows` call returns.
const MAX_ROWS: u64 = 1024;

fn limit(mb: Option<u64>) -> u64 {
    mb.map_or(DEFAULT_ALIGN_LIMIT, |mb| mb.saturating_mul(1 << 20))
        .clamp(MIN_LIMIT, MAX_LIMIT)
}

fn get(id: &str) -> pcompare_core::Result<Shared> {
    // A poisoned lock only means another command panicked; the map is still valid.
    let map = SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
    map.as_ref()
        .and_then(|m| m.get(id).cloned())
        .ok_or_else(|| CoreError::Other("hex session is not open; refresh to reopen it".into()))
}

/// Runs `f` on an open session off the IPC thread.
async fn with_session<T, F>(id: String, f: F) -> CmdResult<T>
where
    T: Send + 'static,
    F: FnOnce(&mut Open) -> pcompare_core::Result<T> + Send + 'static,
{
    blocking(move || {
        let shared = get(&id)?;
        let mut open = shared.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut open)
    })
    .await
}

fn side(side: usize) -> pcompare_core::Result<usize> {
    if side < 2 {
        Ok(side)
    } else {
        Err(CoreError::Other(format!("bad side {side}")))
    }
}

fn editable(open: &Open, s: usize) -> pcompare_core::Result<usize> {
    let s = side(s)?;
    if open.remote[s] {
        return Err(CoreError::Other("remote files are read-only".into()));
    }
    Ok(s)
}

/// Opens (or reopens) both files for an editable, aligned compare.
/// `limit_mb` caps the size aligned byte by byte; larger files are compared
/// offset by offset.
#[tauri::command]
pub async fn hex_open(
    app: AppHandle,
    id: String,
    left: String,
    right: String,
    limit_mb: Option<u64>,
    view: HexView,
) -> CmdResult<HexState> {
    blocking(move || {
        let (l, r) = hex_paths(&app, &left, &right, true)?;
        let mut session = HexSession::open(&l, &r, limit(limit_mb))?;
        let state = session.state(&view);
        let open = Open {
            session,
            // files inside archives open from an extracted copy: read-only too
            remote: [&left, &right].map(|p| pcompare_remote::is_remote(p) || pcompare_remote::is_inside_container(p)),
            paths: [l.clone(), r.clone()],
        };
        let mut map = SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
        map.get_or_insert_with(HashMap::new)
            .insert(id, Arc::new(Mutex::new(open)));
        Ok(state)
    })
    .await
}

#[tauri::command]
pub fn hex_close(id: String) {
    let mut map = SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(m) = map.as_mut() {
        m.remove(&id);
    }
}

#[tauri::command]
pub async fn hex_state(id: String, view: HexView) -> CmdResult<HexState> {
    with_session(id, move |o| Ok(o.session.state(&view))).await
}

#[tauri::command]
pub async fn hex_rows(id: String, view: HexView, first: u64, count: u64) -> CmdResult<Vec<HexRow>> {
    with_session(id, move |o| o.session.rows(&view, first, count.min(MAX_ROWS))).await
}

/// Replaces `remove` bytes at `offset` with `insert` (overwrite, insert and
/// delete are all this).
#[tauri::command]
pub async fn hex_edit(
    id: String,
    side: usize,
    offset: u64,
    remove: u64,
    insert: Vec<u8>,
    view: HexView,
) -> CmdResult<HexState> {
    with_session(id, move |o| {
        let s = editable(o, side)?;
        o.session.edit(s, offset, remove, &insert);
        Ok(o.session.state(&view))
    })
    .await
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HexUndo {
    state: HexState,
    /// Where the change happened, or `None` if there was nothing to do.
    at: Option<HexEditAt>,
}

#[tauri::command]
pub async fn hex_undo(id: String, view: HexView) -> CmdResult<HexUndo> {
    with_session(id, move |o| {
        let at = o.session.undo();
        Ok(HexUndo {
            state: o.session.state(&view),
            at,
        })
    })
    .await
}

#[tauri::command]
pub async fn hex_redo(id: String, view: HexView) -> CmdResult<HexUndo> {
    with_session(id, move |o| {
        let at = o.session.redo();
        Ok(HexUndo {
            state: o.session.state(&view),
            at,
        })
    })
    .await
}

/// Recompares the current (edited) content in full. The session stays
/// usable meanwhile; if it is edited before the compare finishes, the
/// result is dropped and the returned state is still `stale`.
#[tauri::command]
pub async fn hex_realign(id: String, limit_mb: Option<u64>, view: HexView) -> CmdResult<HexState> {
    blocking(move || {
        let shared = get(&id)?;
        let job = shared
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .session
            .align_job(limit(limit_mb))?;
        let outcome = job.run()?;
        let mut open = shared.lock().unwrap_or_else(|e| e.into_inner());
        open.session.apply(outcome);
        Ok(open.session.state(&view))
    })
    .await
}

/// The next (or previous) difference from display position `from` (`None`:
/// before the start).
#[tauri::command]
pub async fn hex_navigate(id: String, view: HexView, from: Option<u64>, forward: bool) -> CmdResult<Option<HexTarget>> {
    with_session(id, move |o| Ok(o.session.next_difference(&view, from, forward))).await
}

#[tauri::command]
pub async fn hex_locate(id: String, view: HexView, side: usize, offset: u64) -> CmdResult<HexTarget> {
    with_session(id, move |o| {
        let s = self::side(side)?;
        Ok(o.session.locate(&view, s, offset))
    })
    .await
}

#[tauri::command]
pub async fn hex_find(
    id: String,
    side: usize,
    pattern: Vec<u8>,
    from: u64,
    forward: bool,
    fold_case: bool,
) -> CmdResult<Option<u64>> {
    with_session(id, move |o| {
        let s = self::side(side)?;
        o.session.find(s, &pattern, from, forward, fold_case)
    })
    .await
}

#[tauri::command]
pub async fn hex_read(id: String, side: usize, offset: u64, len: usize) -> CmdResult<Vec<u8>> {
    with_session(id, move |o| {
        let s = self::side(side)?;
        o.session.read(s, offset, len.min(1 << 20))
    })
    .await
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HexReplaced {
    state: HexState,
    count: usize,
}

#[tauri::command]
pub async fn hex_replace_all(
    id: String,
    side: usize,
    pattern: Vec<u8>,
    replacement: Vec<u8>,
    fold_case: bool,
    view: HexView,
) -> CmdResult<HexReplaced> {
    with_session(id, move |o| {
        let s = editable(o, side)?;
        let count = o.session.replace_all(s, &pattern, &replacement, fold_case)?;
        Ok(HexReplaced {
            state: o.session.state(&view),
            count,
        })
    })
    .await
}

/// Writes one side back to disk. The version it replaces goes to the app's
/// backups when they are on, else to `<name>.bak` when `backup` is set.
#[tauri::command]
pub async fn hex_save(
    app: AppHandle,
    id: String,
    side: usize,
    backup: bool,
    view: HexView,
    origin: Option<BackupOrigin>,
) -> CmdResult<HexState> {
    with_session(id, move |o| {
        let s = editable(o, side)?;
        let backups = crate::backups::settings(&app).enabled;
        if backups {
            let path = o.paths[s].clone();
            let origin = origin.unwrap_or_else(|| crate::backups::save_origin("Hex Compare", &path.to_string_lossy()));
            crate::backups::keep_before_save(&app, &path, origin)?;
        }
        o.session.save(s, backup && !backups)?;
        Ok(o.session.state(&view))
    })
    .await
}

/// Which sides changed on disk since they were opened or last saved.
#[tauri::command]
pub async fn hex_disk_changed(id: String) -> CmdResult<[bool; 2]> {
    with_session(id, |o| Ok(o.session.changed_on_disk())).await
}
