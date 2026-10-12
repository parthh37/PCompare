//! Tauri commands for the picture session(s). Register each command in main.rs.

use crate::CmdResult;
use pcompare_core::CoreError;
use pcompare_picture::{PictureComparison, PicturePair, PictureSources, DEFAULT_MAX_DISPLAY_DIM};
use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;
use tauri::AppHandle;

/// Decoded pairs are kept so tolerance changes don't re-read and re-decode
/// both files. A 4K pair holds roughly 70 MB, so keep only a few.
const CACHE_CAPACITY: usize = 3;

/// File length and modification time, used to notice files changed on disk.
type Stamp = (u64, Option<SystemTime>);

struct Cached {
    paths: (String, String),
    stamps: [Stamp; 2],
    pair: Arc<PicturePair>,
}

static CACHE: Mutex<VecDeque<Cached>> = Mutex::new(VecDeque::new());

fn stamp(path: &Path) -> pcompare_core::Result<Stamp> {
    let meta = std::fs::metadata(path).map_err(|e| CoreError::io(path, e))?;
    Ok((meta.len(), meta.modified().ok()))
}

/// Returns the decoded pair for `left`/`right`, decoding afresh when `reload`
/// is set (remote files are downloaded again too), when it isn't cached, or
/// when either file changed since it was cached.
fn pair(app: &AppHandle, left: &str, right: &str, reload: bool) -> pcompare_core::Result<Arc<PicturePair>> {
    let (lp, rp) = crate::with_remote(app, [left, right], |r| {
        Ok((r.local_copy(left, reload)?, r.local_copy(right, reload)?))
    })?;
    let stamps = [stamp(&lp)?, stamp(&rp)?];
    let paths = (left.to_owned(), right.to_owned());
    // A poisoned lock only means another decode panicked; the cache itself is still valid.
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(i) = cache.iter().position(|c| c.paths == paths) {
        let hit = cache.remove(i).expect("index from position");
        if !reload && hit.stamps == stamps {
            let pair = hit.pair.clone();
            cache.push_front(hit);
            return Ok(pair);
        }
    }
    // Don't hold the lock while decoding; other sessions may be comparing too.
    drop(cache);
    let pair = Arc::new(PicturePair::open([&lp, &rp], [left, right], DEFAULT_MAX_DISPLAY_DIM)?);
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    cache.retain(|c| c.paths != paths);
    cache.push_front(Cached {
        paths,
        stamps,
        pair: pair.clone(),
    });
    cache.truncate(CACHE_CAPACITY);
    Ok(pair)
}

/// Decodes both images (re-reading them from disk) and returns their info
/// and display images. Call [`picture_diff`] afterwards for the comparison.
#[tauri::command]
pub async fn picture_load(app: AppHandle, left: String, right: String) -> CmdResult<PictureSources> {
    crate::blocking(move || pair(&app, &left, &right, true)?.sources()).await
}

/// Compares the pictures at `tolerance` (0–255 per channel), reusing the
/// decoded images from [`picture_load`] when the files haven't changed.
#[tauri::command]
pub async fn picture_diff(app: AppHandle, left: String, right: String, tolerance: u8) -> CmdResult<PictureComparison> {
    crate::blocking(move || pair(&app, &left, &right, false)?.compare(tolerance)).await
}
