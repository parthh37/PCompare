//! The persistent checksum cache behind [`ChecksumCache`].
//!
//! A folder scan with CRC comparison reads every file it compares. Most of
//! them haven't changed since the last scan, so their CRC is remembered,
//! keyed by full path and valid only while size and modification time are
//! exactly what they were.
//!
//! Scans are parallel and hit the cache once per file, so per-lookup SQLite
//! queries would serialize them on the connection lock. Instead the rows
//! under the scanned roots are loaded into a `HashMap` up front (one indexed
//! range query per root), lookups are plain map reads, and new checksums are
//! buffered and written in one transaction by [`CachedChecksums::flush`].
//!
//! One trap: a file modified within the same clock tick as the scan can be
//! modified again without its mtime changing, so caching its CRC could hide
//! the second change forever (git's "racy clean" problem). Files whose mtime
//! is within [`RACY_WINDOW_MS`] of now, or in the future, are never cached.

use std::collections::HashMap;
use std::sync::Mutex;

use pcompare_core::folder::{ChecksumCache, EntryMeta, Side};
use pcompare_core::store::now_ms;

use crate::{normalize_key, Index, Result};

/// Files modified this recently are too fresh to trust their mtime.
pub const RACY_WINDOW_MS: i64 = 3_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Cached {
    size: u64,
    mtime_ms: i64,
    crc: u32,
}

/// A [`ChecksumCache`] preloaded from the index for one comparison.
///
/// Checksums put during the scan are not visible to `get` until a later
/// [`Index::checksum_cache`] call; a scan computes each file's CRC at most
/// once, so it never needs them back.
pub struct CachedChecksums<'a> {
    index: &'a Index,
    /// Normalized root per side; `None` disables caching for that side.
    roots: [Option<String>; 2],
    known: HashMap<String, Cached>,
    pending: Mutex<HashMap<String, Cached>>,
}

impl Index {
    /// Preloads cached checksums for files under `roots` (left, right) and
    /// returns a cache for one comparison. `None` disables a side, e.g. a
    /// remote one whose mtimes can't be trusted. Call
    /// [`CachedChecksums::flush`] when the scan ends; dropping the cache
    /// also tries to flush.
    pub fn checksum_cache(&self, roots: [Option<&str>; 2]) -> Result<CachedChecksums<'_>> {
        let roots = roots.map(|r| r.map(normalize_key));
        let mut known = HashMap::new();
        {
            let conn = self.conn();
            let mut stmt = conn.prepare_cached(
                "SELECT path, size, mtime_ms, crc FROM checksums WHERE path >= ?1 AND path < ?2",
            )?;
            let mut done: Vec<&str> = Vec::new();
            for root in roots.iter().flatten() {
                if done.contains(&root.as_str()) {
                    continue;
                }
                done.push(root);
                let (lo, hi) = prefix_range(root);
                let rows = stmt.query_map([&lo, &hi], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        Cached {
                            size: r.get::<_, i64>(1)? as u64,
                            mtime_ms: r.get(2)?,
                            crc: r.get::<_, i64>(3)? as u32,
                        },
                    ))
                })?;
                for row in rows {
                    let (path, c) = row?;
                    known.insert(path, c);
                }
            }
        }
        Ok(CachedChecksums {
            index: self,
            roots,
            known,
            pending: Mutex::new(HashMap::new()),
        })
    }

    /// Empties the checksum cache; returns how many entries were removed.
    pub fn clear_checksums(&self) -> Result<usize> {
        Ok(self.conn().execute("DELETE FROM checksums", [])?)
    }
}

impl CachedChecksums<'_> {
    /// Entries loaded from the index.
    pub fn preloaded(&self) -> usize {
        self.known.len()
    }

    /// Writes buffered checksums in one transaction; returns how many. On
    /// failure they stay buffered for the next attempt.
    pub fn flush(&self) -> Result<usize> {
        let batch = std::mem::take(&mut *self.lock_pending());
        if batch.is_empty() {
            return Ok(0);
        }
        match self.write(&batch) {
            Ok(()) => Ok(batch.len()),
            Err(e) => {
                let mut pending = self.lock_pending();
                for (k, v) in batch {
                    pending.entry(k).or_insert(v);
                }
                Err(e)
            }
        }
    }

    fn write(&self, batch: &HashMap<String, Cached>) -> Result<()> {
        let mut conn = self.index.conn();
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT OR REPLACE INTO checksums (path, size, mtime_ms, crc, seen_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            let now = now_ms();
            for (path, c) in batch {
                stmt.execute(rusqlite::params![
                    path,
                    c.size as i64,
                    c.mtime_ms,
                    c.crc as i64,
                    now
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    fn root(&self, side: Side) -> Option<&str> {
        self.roots[match side {
            Side::Left => 0,
            Side::Right => 1,
        }]
        .as_deref()
    }

    fn lock_pending(&self) -> std::sync::MutexGuard<'_, HashMap<String, Cached>> {
        self.pending.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl ChecksumCache for CachedChecksums<'_> {
    fn get(&self, side: Side, path: &str, meta: &EntryMeta) -> Option<u32> {
        self.root(side)?;
        let mtime_ms = meta.modified_ms?;
        let c = self.known.get(&normalize_key(path))?;
        (c.size == meta.size && c.mtime_ms == mtime_ms).then_some(c.crc)
    }

    fn put(&self, side: Side, path: &str, meta: &EntryMeta, crc: u32) {
        if self.root(side).is_none() || meta.is_dir {
            return;
        }
        let Some(mtime_ms) = meta.modified_ms else {
            return;
        };
        if mtime_ms > now_ms() - RACY_WINDOW_MS {
            return;
        }
        let key = normalize_key(path);
        let entry = Cached {
            size: meta.size,
            mtime_ms,
            crc,
        };
        if self.known.get(&key) == Some(&entry) {
            return;
        }
        self.lock_pending().insert(key, entry);
    }
}

impl Drop for CachedChecksums<'_> {
    fn drop(&mut self) {
        if let Err(e) = self.flush() {
            eprintln!("pcompare-index: cannot save checksums: {e}");
        }
    }
}

/// Half-open key range `[lo, hi)` covering every path strictly under
/// `root`. Plain `>=`/`<` comparisons let SQLite use the primary key index,
/// which `LIKE` (case-insensitive by default) would not. The upper bound is
/// the separator's successor, so `/a/b` covers `/a/b/x` but not `/a/bc`.
fn prefix_range(root: &str) -> (String, String) {
    let sep = if cfg!(windows) { '\\' } else { '/' };
    let base = root.strip_suffix(['/', '\\']).unwrap_or(root);
    let next = char::from(sep as u8 + 1);
    (format!("{base}{sep}"), format!("{base}{next}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: i64 = 86_400_000;

    fn meta(size: u64, modified_ms: Option<i64>) -> EntryMeta {
        EntryMeta {
            size,
            modified_ms,
            is_dir: false,
        }
    }

    fn p(parts: &[&str]) -> String {
        let sep = if cfg!(windows) { "\\" } else { "/" };
        let root = if cfg!(windows) { "c:" } else { "" };
        format!("{root}{sep}{}", parts.join(sep))
    }

    #[test]
    fn hit_only_when_size_and_mtime_match() {
        let index = Index::open_in_memory().unwrap();
        let old = now_ms() - DAY;
        let file = p(&["a", "f.txt"]);
        {
            let cache = index.checksum_cache([Some(&p(&["a"])), None]).unwrap();
            assert_eq!(cache.get(Side::Left, &file, &meta(5, Some(old))), None);
            cache.put(Side::Left, &file, &meta(5, Some(old)), 0xdead);
            assert_eq!(cache.flush().unwrap(), 1);
        }
        let cache = index.checksum_cache([Some(&p(&["a"])), None]).unwrap();
        assert_eq!(cache.preloaded(), 1);
        assert_eq!(
            cache.get(Side::Left, &file, &meta(5, Some(old))),
            Some(0xdead)
        );
        assert_eq!(
            cache.get(Side::Left, &file, &meta(6, Some(old))),
            None,
            "size changed"
        );
        assert_eq!(
            cache.get(Side::Left, &file, &meta(5, Some(old + 1))),
            None,
            "mtime changed"
        );
        assert_eq!(
            cache.get(Side::Left, &file, &meta(5, None)),
            None,
            "no mtime"
        );
        assert_eq!(
            cache.get(Side::Right, &file, &meta(5, Some(old))),
            None,
            "side disabled"
        );
    }

    #[test]
    fn racy_and_untimed_files_are_not_cached() {
        let index = Index::open_in_memory().unwrap();
        let root = p(&["r"]);
        let now = now_ms();
        {
            let cache = index.checksum_cache([Some(&root), Some(&root)]).unwrap();
            cache.put(
                Side::Left,
                &p(&["r", "fresh"]),
                &meta(1, Some(now - 1_000)),
                1,
            );
            cache.put(
                Side::Left,
                &p(&["r", "future"]),
                &meta(1, Some(now + DAY)),
                2,
            );
            cache.put(Side::Left, &p(&["r", "untimed"]), &meta(1, None), 3);
            cache.put(
                Side::Right,
                &p(&["r", "settled"]),
                &meta(1, Some(now - 10_000)),
                4,
            );
        }
        assert_eq!(index.stats().unwrap().checksums, 1, "flushed on drop");
        let cache = index.checksum_cache([Some(&root), Some(&root)]).unwrap();
        assert_eq!(
            cache.get(
                Side::Right,
                &p(&["r", "settled"]),
                &meta(1, Some(now - 10_000))
            ),
            Some(4)
        );
    }

    #[test]
    fn preload_is_limited_to_the_root_folder() {
        let index = Index::open_in_memory().unwrap();
        let old = now_ms() - DAY;
        {
            let cache = index.checksum_cache([Some("/"), None]).unwrap();
            for (i, f) in [
                p(&["a", "b", "x"]),
                p(&["a", "bc", "x"]),
                p(&["a", "b"]),
                p(&["a", "b", "y", "z"]),
            ]
            .iter()
            .enumerate()
            {
                cache.put(Side::Left, f, &meta(1, Some(old)), i as u32);
            }
        }
        assert_eq!(index.stats().unwrap().checksums, 4);
        let root = format!(
            "{}{}",
            p(&["a", "b"]),
            if cfg!(windows) { "\\" } else { "/" }
        );
        let cache = index.checksum_cache([None, Some(&root)]).unwrap();
        assert_eq!(cache.preloaded(), 2, "only /a/b/x and /a/b/y/z");
        assert_eq!(
            cache.get(Side::Right, &p(&["a", "b", "y", "z"]), &meta(1, Some(old))),
            Some(3)
        );
        assert_eq!(
            cache.get(Side::Right, &p(&["a", "bc", "x"]), &meta(1, Some(old))),
            None
        );
        assert_eq!(
            cache.get(Side::Left, &p(&["a", "b", "x"]), &meta(1, Some(old))),
            None
        );

        assert_eq!(index.clear_checksums().unwrap(), 4);
    }

    #[test]
    fn prefix_range_bounds() {
        let (lo, hi) = prefix_range("/a/b");
        if !cfg!(windows) {
            assert_eq!((lo.as_str(), hi.as_str()), ("/a/b/", "/a/b0"));
            assert_eq!(prefix_range("/"), ("/".into(), "0".into()));
        }
    }
}
