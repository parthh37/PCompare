//! Storage for sync [`Baseline`]s, one per folder pair.
//!
//! A baseline is replaced wholesale after every successful sync, so saving
//! is delete-and-reinsert in a single transaction: a crash mid-save leaves
//! the previous baseline intact, never a mix of old and new. Pair roots are
//! keys made with [`normalize_key`]; relative paths are stored verbatim.

use std::collections::HashMap;

use pcompare_core::store::now_ms;
use pcompare_sync::baseline::{Baseline, BaselineEntry, Snap};
use rusqlite::{params, OptionalExtension, TransactionBehavior};

use crate::{normalize_key, Index, Result};

impl Index {
    /// The baseline stored for `left` ↔ `right`, empty if there is none.
    pub fn load_baseline(&self, left: &str, right: &str) -> Result<Baseline> {
        let conn = self.conn();
        let (left, right) = (normalize_key(left), normalize_key(right));
        let pair: Option<i64> = conn
            .prepare_cached("SELECT id FROM pairs WHERE left = ?1 AND right = ?2")?
            .query_row([&left, &right], |r| r.get(0))
            .optional()?;
        let Some(pair) = pair else {
            return Ok(Baseline::default());
        };
        let count: i64 = conn
            .prepare_cached("SELECT COUNT(*) FROM baseline WHERE pair_id = ?1")?
            .query_row([pair], |r| r.get(0))?;
        let mut entries = HashMap::with_capacity(count as usize);
        let mut stmt = conn.prepare_cached(
            "SELECT rel_path, is_dir, l_size, l_mtime, r_size, r_mtime FROM baseline WHERE pair_id = ?1",
        )?;
        let rows = stmt.query_map([pair], |r| {
            Ok((
                r.get::<_, String>(0)?,
                BaselineEntry {
                    is_dir: r.get(1)?,
                    left: Snap {
                        size: r.get::<_, i64>(2)? as u64,
                        modified_ms: r.get(3)?,
                    },
                    right: Snap {
                        size: r.get::<_, i64>(4)? as u64,
                        modified_ms: r.get(5)?,
                    },
                },
            ))
        })?;
        for row in rows {
            let (rel, entry) = row?;
            entries.insert(rel, entry);
        }
        Ok(Baseline { entries })
    }

    /// Replaces the baseline for `left` ↔ `right` atomically.
    pub fn save_baseline(&self, left: &str, right: &str, b: &Baseline) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        {
            let pair: i64 = tx
                .prepare_cached(
                    "INSERT INTO pairs (left, right, updated_ms) VALUES (?1, ?2, ?3)
                     ON CONFLICT (left, right) DO UPDATE SET updated_ms = excluded.updated_ms
                     RETURNING id",
                )?
                .query_row(
                    params![normalize_key(left), normalize_key(right), now_ms()],
                    |r| r.get(0),
                )?;
            tx.prepare_cached("DELETE FROM baseline WHERE pair_id = ?1")?
                .execute([pair])?;
            let mut insert = tx.prepare_cached(
                "INSERT INTO baseline (pair_id, rel_path, is_dir, l_size, l_mtime, r_size, r_mtime)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?;
            for (rel, e) in &b.entries {
                insert.execute(params![
                    pair,
                    rel,
                    e.is_dir,
                    e.left.size as i64,
                    e.left.modified_ms,
                    e.right.size as i64,
                    e.right.modified_ms,
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Drops the baseline for `left` ↔ `right`; the next two-way sync of the
    /// pair starts over as if it had never been synced.
    pub fn forget_baseline(&self, left: &str, right: &str) -> Result<()> {
        self.conn()
            .prepare_cached("DELETE FROM pairs WHERE left = ?1 AND right = ?2")?
            .execute([normalize_key(left), normalize_key(right)])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(is_dir: bool, l: (u64, Option<i64>), r: (u64, Option<i64>)) -> BaselineEntry {
        BaselineEntry {
            is_dir,
            left: Snap {
                size: l.0,
                modified_ms: l.1,
            },
            right: Snap {
                size: r.0,
                modified_ms: r.1,
            },
        }
    }

    #[test]
    fn round_trip_replace_and_forget() {
        let index = Index::open_in_memory().unwrap();
        assert!(index.load_baseline("/l", "/r").unwrap().is_empty());

        let mut b = Baseline::default();
        b.entries
            .insert("dir".into(), entry(true, (0, None), (0, None)));
        b.entries.insert(
            "dir/f.txt".into(),
            entry(false, (10, Some(1_000)), (10, None)),
        );
        b.entries.insert(
            "Ünïcode name.txt".into(),
            entry(false, (u32::MAX as u64 * 4, Some(-5)), (1, Some(2))),
        );
        index.save_baseline("/l/", "/r", &b).unwrap();
        assert_eq!(
            index.load_baseline("/l", "/r/").unwrap(),
            b,
            "keys normalized"
        );
        assert!(
            index.load_baseline("/r", "/l").unwrap().is_empty(),
            "pair is ordered"
        );

        let mut next = Baseline::default();
        next.entries
            .insert("other".into(), entry(false, (1, Some(1)), (1, Some(1))));
        index.save_baseline("/l", "/r", &next).unwrap();
        assert_eq!(
            index.load_baseline("/l", "/r").unwrap(),
            next,
            "replaced, not merged"
        );
        let stats = index.stats().unwrap();
        assert_eq!((stats.baselines, stats.baseline_entries), (1, 1));

        index.forget_baseline("/l", "/r").unwrap();
        assert!(index.load_baseline("/l", "/r").unwrap().is_empty());
        assert_eq!(
            index.stats().unwrap().baseline_entries,
            0,
            "entries cascade"
        );
    }

    #[test]
    fn large_baseline_saves_in_one_transaction() {
        let index = Index::open_in_memory().unwrap();
        let mut b = Baseline::default();
        for i in 0..100_000 {
            b.entries.insert(
                format!("d{}/f{i}", i % 100),
                entry(false, (i, Some(i as i64)), (i, Some(i as i64))),
            );
        }
        index.save_baseline("/l", "/r", &b).unwrap();
        assert_eq!(
            index.load_baseline("/l", "/r").unwrap().entries.len(),
            100_000
        );
    }
}
