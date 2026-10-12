//! Folder Compare against containers: checksums the containers already
//! store, and what a content comparison can do when a side (a snapshot) has
//! no contents at all.

use std::sync::Arc;

use pcompare_core::folder::{ChecksumCache, ContentMethod, EntryMeta, FolderCompareOptions, Side};

use crate::CrcSource;

/// A [`ChecksumCache`] that answers from the CRC-32s stored in zip / 7z
/// headers and snapshots, so a CRC comparison of two zips reads nothing and
/// a binary one stops early on differing checksums. Sides without a
/// container fall through to `inner` (the scan database's cache).
pub struct StoredChecksums<'a> {
    pub sides: [Option<Arc<dyn CrcSource>>; 2],
    pub inner: Option<&'a dyn ChecksumCache>,
}

fn index(side: Side) -> usize {
    match side {
        Side::Left => 0,
        Side::Right => 1,
    }
}

impl ChecksumCache for StoredChecksums<'_> {
    fn get(&self, side: Side, path: &str, meta: &EntryMeta) -> Option<u32> {
        match &self.sides[index(side)] {
            Some(src) if src.covers(path) => src.stored_crc(path),
            _ => self.inner?.get(side, path, meta),
        }
    }

    fn put(&self, side: Side, path: &str, meta: &EntryMeta, crc: u32) {
        // Container paths are relative to the container: never cache them.
        if !self.sides[index(side)]
            .as_ref()
            .is_some_and(|s| s.covers(path))
        {
            if let Some(inner) = self.inner {
                inner.put(side, path, meta, crc);
            }
        }
    }
}

/// Content comparison settings for a compare where `snapshots[i]` says
/// whether side `i` is a snapshot and, if so, whether it holds CRCs.
/// Snapshot files can't be read, so a content method becomes CRC (using the
/// stored checksums) or, without them, falls back to size and date — with a
/// notice for the user instead of a "content unavailable" error per file.
pub fn options_for_snapshots(
    opts: &FolderCompareOptions,
    snapshots: [Option<bool>; 2],
) -> (FolderCompareOptions, Option<String>) {
    let mut out = opts.clone();
    if snapshots.iter().all(Option::is_none) || opts.content == ContentMethod::Off {
        return (out, None);
    }
    if snapshots.iter().flatten().all(|has_crcs| *has_crcs) {
        let notice = (opts.content != ContentMethod::Crc).then(|| {
            "Snapshots hold no file contents: contents are compared by the CRC-32s stored in the snapshot.".to_owned()
        });
        out.content = ContentMethod::Crc;
        (out, notice)
    } else {
        out.content = ContentMethod::Off;
        (
            out,
            Some(
                "Content unavailable: the snapshot was saved without CRCs, so files are compared by size and date only."
                    .to_owned(),
            ),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixed(u32);

    impl CrcSource for Fixed {
        fn stored_crc(&self, path: &str) -> Option<u32> {
            (path == "a").then_some(self.0)
        }
    }

    #[test]
    fn stored_checksums_answer_for_containers_only() {
        let c = StoredChecksums {
            sides: [Some(Arc::new(Fixed(7))), None],
            inner: None,
        };
        let m = EntryMeta::default();
        assert_eq!(c.get(Side::Left, "a", &m), Some(7));
        assert_eq!(c.get(Side::Left, "b", &m), None);
        assert_eq!(c.get(Side::Right, "a", &m), None);
    }

    #[test]
    fn snapshot_sides_change_the_content_method() {
        let binary = FolderCompareOptions {
            content: ContentMethod::Binary,
            ..Default::default()
        };
        let (o, note) = options_for_snapshots(&binary, [None, None]);
        assert_eq!((o.content, note), (ContentMethod::Binary, None));
        let (o, note) = options_for_snapshots(&binary, [Some(true), None]);
        assert_eq!(o.content, ContentMethod::Crc);
        assert!(note.is_some());
        let (o, note) = options_for_snapshots(&binary, [None, Some(false)]);
        assert_eq!(o.content, ContentMethod::Off);
        assert!(note.unwrap().starts_with("Content unavailable"));
        let (o, note) =
            options_for_snapshots(&FolderCompareOptions::default(), [Some(false), None]);
        assert_eq!((o.content, note), (ContentMethod::Off, None));
    }
}
