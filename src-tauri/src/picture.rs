//! Tauri commands for the picture session(s). Register each command in main.rs.

use crate::CmdResult;
use pcompare_core::CoreError;
use pcompare_picture::{
    DiffOptions, PictureComparison, PicturePair, PictureSources, PixelProbe, Transform, DEFAULT_MAX_DISPLAY_DIM,
};
use serde::Deserialize;
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

/// Rotation, flips and scaling of both sides, applied before comparing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PairTransform {
    left: Transform,
    right: Transform,
}

struct Cached {
    paths: (String, String),
    stamps: [Stamp; 2],
    pair: Arc<PicturePair>,
    /// The most recently requested transformed variant of `pair`.
    transformed: Option<(PairTransform, Arc<PicturePair>)>,
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
        transformed: None,
    });
    cache.truncate(CACHE_CAPACITY);
    Ok(pair)
}

/// Like [`pair`], with both sides rotated, flipped and scaled as requested.
/// The last transformed variant is cached alongside the decoded pair.
fn transformed_pair(
    app: &AppHandle,
    left: &str,
    right: &str,
    reload: bool,
    transforms: PairTransform,
) -> pcompare_core::Result<Arc<PicturePair>> {
    let base = pair(app, left, right, reload)?;
    if transforms == PairTransform::default() {
        return Ok(base);
    }
    let lookup = |cache: &VecDeque<Cached>| {
        cache
            .iter()
            .find(|c| Arc::ptr_eq(&c.pair, &base))
            .and_then(|c| c.transformed.as_ref())
            .filter(|(t, _)| *t == transforms)
            .map(|(_, p)| p.clone())
    };
    if let Some(hit) = lookup(&CACHE.lock().unwrap_or_else(|e| e.into_inner())) {
        return Ok(hit);
    }
    let Some(variant) = base.transformed(&transforms.left, &transforms.right, DEFAULT_MAX_DISPLAY_DIM)? else {
        return Ok(base);
    };
    let variant = Arc::new(variant);
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(entry) = cache.iter_mut().find(|c| Arc::ptr_eq(&c.pair, &base)) {
        entry.transformed = Some((transforms, variant.clone()));
    }
    Ok(variant)
}

/// Returns both images' info and display images, after `transforms`.
/// `reload` re-reads both files from disk (set it for a fresh load or a
/// refresh; leave it unset when only the transforms changed). Call
/// [`picture_diff`] afterwards for the comparison.
#[tauri::command]
pub async fn picture_load(
    app: AppHandle,
    left: String,
    right: String,
    transforms: Option<PairTransform>,
    reload: Option<bool>,
) -> CmdResult<PictureSources> {
    crate::blocking(move || {
        transformed_pair(
            &app,
            &left,
            &right,
            reload.unwrap_or(true),
            transforms.unwrap_or_default(),
        )?
        .sources()
    })
    .await
}

/// Compares the (transformed) pictures under `options`, reusing the decoded
/// images from [`picture_load`] when the files haven't changed.
#[tauri::command]
pub async fn picture_diff(
    app: AppHandle,
    left: String,
    right: String,
    transforms: Option<PairTransform>,
    options: DiffOptions,
) -> CmdResult<PictureComparison> {
    crate::blocking(move || {
        transformed_pair(&app, &left, &right, false, transforms.unwrap_or_default())?.compare(&options)
    })
    .await
}

/// Full-resolution pixels of both sides around canvas position (`x`, `y`),
/// for the loupe.
#[allow(clippy::too_many_arguments)]
#[tauri::command]
pub async fn picture_probe(
    app: AppHandle,
    left: String,
    right: String,
    transforms: Option<PairTransform>,
    options: DiffOptions,
    x: u32,
    y: u32,
    radius: u32,
) -> CmdResult<PixelProbe> {
    crate::blocking(move || {
        Ok(transformed_pair(&app, &left, &right, false, transforms.unwrap_or_default())?.probe(x, y, radius, &options))
    })
    .await
}
