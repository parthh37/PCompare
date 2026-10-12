//! Binary comparison with random access, so the UI can page through files of
//! any size without loading them into memory.
//!
//! [`HexSession`] is the editable, aligned compare behind the hex view: two
//! [`EditBuffer`]s (piece-table overlays over the files) plus their
//! [`Alignment`], patched on every edit and fully recomputed on request.

use crate::hex_align::{self, AlignMode, Alignment, RowFilter, RowMap};
use crate::hex_edit::{self as edit, ByteSource, EditBuffer, Pieces, Snapshot};
use crate::hex_find;
use crate::{CoreError, Result};
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

const SCAN_CHUNK: usize = 256 * 1024;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HexSummary {
    pub left_size: u64,
    pub right_size: u64,
    pub first_difference: Option<u64>,
    /// Differing bytes within the common length, plus the size difference.
    pub differing_bytes: u64,
    pub identical: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HexChunk {
    pub offset: u64,
    pub left: Vec<u8>,
    pub right: Vec<u8>,
}

fn open(path: &Path) -> Result<File> {
    File::open(path).map_err(|e| CoreError::io(path, e))
}

fn read_at(file: &mut File, path: &Path, offset: u64, buf: &mut [u8]) -> Result<usize> {
    file.seek(SeekFrom::Start(offset)).map_err(|e| CoreError::io(path, e))?;
    let mut filled = 0;
    while filled < buf.len() {
        match file.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) => return Err(CoreError::io(path, e)),
        }
    }
    Ok(filled)
}

pub fn read_chunk(left: &Path, right: &Path, offset: u64, len: usize) -> Result<HexChunk> {
    let (mut lf, mut rf) = (open(left)?, open(right)?);
    let (mut lb, mut rb) = (vec![0u8; len], vec![0u8; len]);
    let ln = read_at(&mut lf, left, offset, &mut lb)?;
    let rn = read_at(&mut rf, right, offset, &mut rb)?;
    lb.truncate(ln);
    rb.truncate(rn);
    Ok(HexChunk {
        offset,
        left: lb,
        right: rb,
    })
}

/// Finds the first differing offset at or after `from` (including the point
/// where the shorter file ends).
pub fn next_difference(left: &Path, right: &Path, from: u64) -> Result<Option<u64>> {
    let (mut lf, mut rf) = (open(left)?, open(right)?);
    let (mut lb, mut rb) = (vec![0u8; SCAN_CHUNK], vec![0u8; SCAN_CHUNK]);
    let mut offset = from;
    loop {
        let ln = read_at(&mut lf, left, offset, &mut lb)?;
        let rn = read_at(&mut rf, right, offset, &mut rb)?;
        let common = ln.min(rn);
        if let Some(i) = lb[..common].iter().zip(&rb[..common]).position(|(a, b)| a != b) {
            return Ok(Some(offset + i as u64));
        }
        if ln != rn {
            return Ok(Some(offset + common as u64));
        }
        if ln == 0 {
            return Ok(None);
        }
        offset += ln as u64;
    }
}

pub fn summarize(left: &Path, right: &Path) -> Result<HexSummary> {
    let (mut lf, mut rf) = (open(left)?, open(right)?);
    let left_size = lf.metadata().map_err(|e| CoreError::io(left, e))?.len();
    let right_size = rf.metadata().map_err(|e| CoreError::io(right, e))?.len();
    let (mut lb, mut rb) = (vec![0u8; SCAN_CHUNK], vec![0u8; SCAN_CHUNK]);
    let mut offset = 0u64;
    let mut first_difference = None;
    let mut differing = 0u64;
    loop {
        let ln = read_at(&mut lf, left, offset, &mut lb)?;
        let rn = read_at(&mut rf, right, offset, &mut rb)?;
        let common = ln.min(rn);
        for (i, (a, b)) in lb[..common].iter().zip(&rb[..common]).enumerate() {
            if a != b {
                differing += 1;
                first_difference.get_or_insert(offset + i as u64);
            }
        }
        if common == 0 {
            break;
        }
        offset += common as u64;
    }
    if left_size != right_size {
        first_difference.get_or_insert(left_size.min(right_size));
        differing += left_size.abs_diff(right_size);
    }
    Ok(HexSummary {
        left_size,
        right_size,
        first_difference,
        differing_bytes: differing,
        identical: differing == 0,
    })
}

// ---------- editable, aligned compare sessions ----------

/// Undo steps kept per session.
const MAX_UNDO: usize = 1000;
/// Widest row a view may ask for.
const MAX_BYTES_PER_ROW: u64 = 256;

/// How the UI lays out rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HexView {
    pub bytes_per_row: u64,
    pub filter: RowFilter,
    /// Rows shown around each difference with [`RowFilter::Context`].
    pub context: u64,
}

impl HexView {
    fn bpr(&self) -> u64 {
        self.bytes_per_row.clamp(1, MAX_BYTES_PER_ROW)
    }
}

/// Everything the UI needs to know after opening, editing or re-aligning.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HexState {
    /// Bumped whenever content or layout changes; rows from older versions are stale.
    pub version: u64,
    pub sizes: [u64; 2],
    pub dirty: [bool; 2],
    pub can_undo: bool,
    pub can_redo: bool,
    pub mode: AlignMode,
    /// Why the files were compared offset by offset, if they were.
    pub note: Option<String>,
    /// Edits since the last full compare; differences are approximate until
    /// the UI asks for a re-align.
    pub stale: bool,
    pub display_len: u64,
    pub diff_blocks: u64,
    pub differing_bytes: u64,
    pub identical: bool,
    /// Rows in the requested view.
    pub row_count: u64,
}

/// One row of the aligned view.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HexRow {
    /// Visible row index in the view.
    pub index: u64,
    /// Row on the display axis (before filtering).
    pub row: u64,
    /// Each side's offset of the first byte at or after the row start; with
    /// the gaps (`None` cells) this gives every cell's offset.
    pub start: [u64; 2],
    pub left: Vec<Option<u8>>,
    pub right: Vec<Option<u8>>,
    /// The view skips rows above this one.
    pub break_before: bool,
}

/// A position to move the cursor to.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HexTarget {
    pub display: u64,
    /// Visible row in the view, or `None` if the view hides it.
    pub row: Option<u64>,
    /// Each side's offset at `display`.
    pub start: [u64; 2],
}

/// Where an undo or redo happened, so the cursor can follow.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HexEditAt {
    pub side: usize,
    pub offset: u64,
}

struct Step {
    side: usize,
    offset: u64,
    removed: u64,
    inserted: u64,
    /// Content of `side` to restore when this step is undone (or redone).
    pieces: Pieces,
}

/// Background re-compare of a session's current content; see
/// [`HexSession::align_job`].
pub struct AlignJob {
    edits: u64,
    limit: u64,
    sources: [Snapshot; 2],
}

/// The result of an [`AlignJob`], applied with [`HexSession::apply`].
pub struct AlignOutcome {
    edits: u64,
    alignment: Alignment,
    note: Option<String>,
}

impl AlignJob {
    pub fn run(self) -> Result<AlignOutcome> {
        let [mut l, mut r] = self.sources;
        let (alignment, note) = compute_alignment(&mut l, &mut r, self.limit)?;
        Ok(AlignOutcome {
            edits: self.edits,
            alignment,
            note,
        })
    }
}

fn compute_alignment(
    l: &mut dyn ByteSource,
    r: &mut dyn ByteSource,
    limit: u64,
) -> Result<(Alignment, Option<String>)> {
    if l.len() <= limit && r.len() <= limit {
        let (a, b) = (edit::read_all(l)?, edit::read_all(r)?);
        Ok((Alignment::new(AlignMode::Aligned, hex_align::align_bytes(&a, &b)), None))
    } else {
        let note = format!(
            "Larger than {} MB: compared offset by offset, insertions and deletions are not aligned",
            limit >> 20
        );
        let segs = hex_align::offset_segments(l, r)?;
        Ok((Alignment::new(AlignMode::Offset, segs), Some(note)))
    }
}

/// Two files opened for an aligned, editable binary compare.
pub struct HexSession {
    bufs: [EditBuffer; 2],
    align: Alignment,
    note: Option<String>,
    limit: u64,
    stale: bool,
    version: u64,
    /// Bumped on content changes only, so a re-align of older content is dropped.
    edits: u64,
    undo: Vec<Step>,
    redo: Vec<Step>,
    rows: Option<(u64, HexView, RowMap)>,
}

impl HexSession {
    /// Opens both files and aligns them; files over `limit` bytes are
    /// compared offset by offset.
    pub fn open(left: &Path, right: &Path, limit: u64) -> Result<HexSession> {
        let mut bufs = [EditBuffer::open(left)?, EditBuffer::open(right)?];
        let [l, r] = &mut bufs;
        let (align, note) = compute_alignment(l, r, limit)?;
        Ok(HexSession {
            bufs,
            align,
            note,
            limit,
            stale: false,
            version: 0,
            edits: 0,
            undo: Vec::new(),
            redo: Vec::new(),
            rows: None,
        })
    }

    fn row_map(&mut self, view: &HexView) -> &RowMap {
        let fresh = matches!(&self.rows, Some((v, cached, _)) if *v == self.version && cached == view);
        if !fresh {
            let map = self.align.row_map(view.bpr(), view.filter, view.context);
            self.rows = Some((self.version, *view, map));
        }
        &self.rows.as_ref().expect("row map just cached").2
    }

    pub fn state(&mut self, view: &HexView) -> HexState {
        let row_count = self.row_map(view).count;
        HexState {
            version: self.version,
            sizes: [self.bufs[0].len(), self.bufs[1].len()],
            dirty: [self.bufs[0].is_dirty(), self.bufs[1].is_dirty()],
            can_undo: !self.undo.is_empty(),
            can_redo: !self.redo.is_empty(),
            mode: self.align.mode,
            note: self.note.clone(),
            stale: self.stale,
            display_len: self.align.display_len(),
            diff_blocks: self.align.blocks(),
            differing_bytes: self.align.differing_bytes(),
            identical: self.align.blocks() == 0,
            row_count,
        }
    }

    /// Rows `first..first + count` of a view.
    pub fn rows(&mut self, view: &HexView, first: u64, count: u64) -> Result<Vec<HexRow>> {
        let bpr = view.bpr();
        let total = self.align.display_len();
        let map = self.row_map(view).clone();
        let wanted: Vec<(u64, u64, bool)> = (first..first.saturating_add(count).min(map.count))
            .filter_map(|v| map.display_row(v).map(|(row, brk)| (v, row, brk)))
            .collect();
        let mut out = Vec::with_capacity(wanted.len());
        // Read each run of consecutive display rows with one read per side.
        let mut i = 0;
        while i < wanted.len() {
            let mut j = i + 1;
            while j < wanted.len() && wanted[j].1 == wanted[j - 1].1 + 1 {
                j += 1;
            }
            let d0 = wanted[i].1 * bpr;
            let d1 = ((wanted[j - 1].1 + 1) * bpr).min(total);
            let (p0, p1) = (self.align.positions_at(d0), self.align.positions_at(d1));
            let bytes = [0, 1].map(|s| self.bufs[s].read_vec(p0[s], (p1[s] - p0[s]) as usize));
            let [lb, rb] = bytes;
            let (lb, rb) = (lb?, rb?);
            let cells = self.align.cells(d0, d1);
            for &(index, row, break_before) in &wanted[i..j] {
                let from = (row * bpr - d0) as usize;
                let to = (from + bpr as usize).min(cells.len());
                let pick = |side: usize, src: &[u8]| -> Vec<Option<u8>> {
                    cells[from..to]
                        .iter()
                        .map(|c| c[side].and_then(|o| src.get((o - p0[side]) as usize).copied()))
                        .collect()
                };
                out.push(HexRow {
                    index,
                    row,
                    start: self.align.positions_at(row * bpr),
                    left: pick(0, &lb),
                    right: pick(1, &rb),
                    break_before,
                });
            }
            i = j;
        }
        Ok(out)
    }

    fn target(&mut self, view: &HexView, display: u64) -> HexTarget {
        let start = self.align.positions_at(display);
        let row = self.row_map(view).visible_row(display / view.bpr());
        HexTarget { display, row, start }
    }

    /// The next (or previous) changed block relative to display position
    /// `from` (`None`: before the start).
    pub fn next_difference(&mut self, view: &HexView, from: Option<u64>, forward: bool) -> Option<HexTarget> {
        let d = self.align.next_change(from, forward)?;
        Some(self.target(view, d))
    }

    /// Where byte `offset` of `side` sits in a view.
    pub fn locate(&mut self, view: &HexView, side: usize, offset: u64) -> HexTarget {
        let d = self.align.locate(side, offset);
        self.target(view, d)
    }

    fn changed(&mut self) {
        self.version += 1;
        self.edits += 1;
        self.stale = true;
    }

    fn record(&mut self, step: Step) {
        self.undo.push(step);
        if self.undo.len() > MAX_UNDO {
            self.undo.remove(0);
        }
        self.redo.clear();
    }

    /// Replaces `remove` bytes at `offset` on `side` with `insert`
    /// (overwrite, insert and delete are all this).
    pub fn edit(&mut self, side: usize, offset: u64, remove: u64, insert: &[u8]) {
        let buf = &mut self.bufs[side];
        let offset = offset.min(buf.len());
        let remove = remove.min(buf.len() - offset);
        if remove == 0 && insert.is_empty() {
            return;
        }
        let pieces = buf.pieces();
        buf.replace(offset, remove, insert);
        self.align.patch(side, offset, remove, insert.len() as u64);
        self.record(Step {
            side,
            offset,
            removed: remove,
            inserted: insert.len() as u64,
            pieces,
        });
        self.changed();
    }

    /// Replaces every match of `pattern` on `side` (at most
    /// [`hex_find::MAX_MATCHES`]) as one undo step; returns how many.
    pub fn replace_all(&mut self, side: usize, pattern: &[u8], replacement: &[u8], fold_case: bool) -> Result<usize> {
        let buf = &mut self.bufs[side];
        let matches = hex_find::find_all(buf, pattern, fold_case, hex_find::MAX_MATCHES)?;
        let (Some(&first), Some(&last)) = (matches.first(), matches.last()) else {
            return Ok(0);
        };
        let plen = pattern.len() as u64;
        let removed = last + plen - first;
        let inserted = removed - plen * matches.len() as u64 + replacement.len() as u64 * matches.len() as u64;
        let pieces = buf.pieces();
        buf.replace_many(&matches, plen, replacement);
        self.align.patch(side, first, removed, inserted);
        self.record(Step {
            side,
            offset: first,
            removed,
            inserted,
            pieces,
        });
        self.changed();
        Ok(matches.len())
    }

    pub fn find(
        &mut self,
        side: usize,
        pattern: &[u8],
        from: u64,
        forward: bool,
        fold_case: bool,
    ) -> Result<Option<u64>> {
        hex_find::find(&mut self.bufs[side], pattern, from, forward, fold_case)
    }

    /// Reads `len` bytes of `side`'s edited content (for copy and replace checks).
    pub fn read(&mut self, side: usize, offset: u64, len: usize) -> Result<Vec<u8>> {
        self.bufs[side].read_vec(offset, len)
    }

    pub fn undo(&mut self) -> Option<HexEditAt> {
        let step = self.undo.pop()?;
        let buf = &mut self.bufs[step.side];
        let current = buf.pieces();
        buf.restore(step.pieces);
        self.align.patch(step.side, step.offset, step.inserted, step.removed);
        let at = HexEditAt {
            side: step.side,
            offset: step.offset,
        };
        self.redo.push(Step {
            pieces: current,
            ..step
        });
        self.changed();
        Some(at)
    }

    pub fn redo(&mut self) -> Option<HexEditAt> {
        let step = self.redo.pop()?;
        let buf = &mut self.bufs[step.side];
        let current = buf.pieces();
        buf.restore(step.pieces);
        self.align.patch(step.side, step.offset, step.removed, step.inserted);
        let at = HexEditAt {
            side: step.side,
            offset: step.offset + step.inserted,
        };
        self.undo.push(Step {
            pieces: current,
            ..step
        });
        self.changed();
        Some(at)
    }

    /// Writes `side` back to its file (keeping `<name>.bak` with `backup`).
    /// Its undo history goes, since it refers to the old file's bytes.
    pub fn save(&mut self, side: usize, backup: bool) -> Result<()> {
        self.bufs[side].save(backup)?;
        self.undo.retain(|s| s.side != side);
        self.redo.retain(|s| s.side != side);
        // A re-align snapshot taken before the save read the replaced file.
        self.version += 1;
        self.edits += 1;
        Ok(())
    }

    /// Which sides changed on disk since they were opened or saved.
    pub fn changed_on_disk(&self) -> [bool; 2] {
        [self.bufs[0].changed_on_disk(), self.bufs[1].changed_on_disk()]
    }

    /// Snapshots the current content for a full re-compare that runs without
    /// holding the session. `limit` replaces the alignment size limit.
    pub fn align_job(&mut self, limit: u64) -> Result<AlignJob> {
        self.limit = limit;
        Ok(AlignJob {
            edits: self.edits,
            limit,
            sources: [self.bufs[0].snapshot()?, self.bufs[1].snapshot()?],
        })
    }

    /// Installs a finished re-compare unless the content was edited since
    /// its job was taken; returns whether it was applied.
    pub fn apply(&mut self, outcome: AlignOutcome) -> bool {
        if outcome.edits != self.edits {
            return false;
        }
        self.align = outcome.alignment;
        self.note = outcome.note;
        self.stale = false;
        self.version += 1;
        true
    }

    /// The configured alignment size limit.
    pub fn limit(&self) -> u64 {
        self.limit
    }
}

#[cfg(test)]
mod session_tests {
    use super::*;

    const VIEW: HexView = HexView {
        bytes_per_row: 8,
        filter: RowFilter::All,
        context: 0,
    };

    fn session(left: &[u8], right: &[u8]) -> (tempfile::TempDir, HexSession) {
        let dir = tempfile::tempdir().unwrap();
        let (l, r) = (dir.path().join("l.bin"), dir.path().join("r.bin"));
        std::fs::write(&l, left).unwrap();
        std::fs::write(&r, right).unwrap();
        let s = HexSession::open(&l, &r, hex_align::DEFAULT_ALIGN_LIMIT).unwrap();
        (dir, s)
    }

    fn realign(s: &mut HexSession) {
        let job = s.align_job(s.limit()).unwrap();
        assert!(s.apply(job.run().unwrap()));
    }

    #[test]
    fn rows_show_gaps_for_insertions() {
        let (_d, mut s) = session(b"0123456789", b"01234xx56789");
        let st = s.state(&VIEW);
        assert_eq!((st.display_len, st.row_count, st.diff_blocks), (12, 2, 1));
        let rows = s.rows(&VIEW, 0, 10).unwrap();
        let byte = |c: Option<u8>| c.map_or('_', char::from);
        let left: String = rows.iter().flat_map(|r| r.left.iter().map(|&c| byte(c))).collect();
        assert_eq!(left, "01234__56789");
        assert_eq!(rows[1].start, [6, 8]);
        let diffs = HexView {
            filter: RowFilter::Differences,
            ..VIEW
        };
        assert_eq!(s.state(&diffs).row_count, 1);
        let next = s.next_difference(&VIEW, None, true).unwrap();
        assert_eq!((next.display, next.row, next.start), (5, Some(0), [5, 5]));
    }

    #[test]
    fn edits_patch_then_realign() {
        let (_d, mut s) = session(b"abcdefgh", b"abcdefgh");
        assert!(s.state(&VIEW).identical);
        s.edit(1, 3, 0, b"XYZ");
        let st = s.state(&VIEW);
        assert!(st.stale && st.dirty[1] && !st.dirty[0]);
        assert_eq!((st.sizes, st.diff_blocks), ([8, 11], 1));
        realign(&mut s);
        let st = s.state(&VIEW);
        assert!(!st.stale);
        assert_eq!(st.differing_bytes, 3);
        // Overwriting the same bytes back on the left makes the sides match.
        s.edit(0, 3, 0, b"XYZ");
        realign(&mut s);
        assert!(s.state(&VIEW).identical);
    }

    #[test]
    fn undo_redo_restore_content_and_layout() {
        let (_d, mut s) = session(b"hello world", b"hello world");
        s.edit(0, 0, 5, b"HI");
        s.edit(0, 2, 0, b"!");
        assert_eq!(s.read(0, 0, 20).unwrap(), b"HI! world");
        let at = s.undo().unwrap();
        assert_eq!((at.side, at.offset), (0, 2));
        assert_eq!(s.read(0, 0, 20).unwrap(), b"HI world");
        s.undo();
        let st = s.state(&VIEW);
        assert!(!st.dirty[0] && !st.can_undo && st.can_redo);
        assert_eq!(st.display_len, 11);
        s.redo();
        assert_eq!(s.read(0, 0, 20).unwrap(), b"HI world");
        assert_eq!(s.state(&VIEW).display_len, 11);
    }

    #[test]
    fn find_and_replace_all() {
        let (_d, mut s) = session(b"cat dog cat", b"cat dog cat");
        assert_eq!(s.find(0, b"cat", 1, true, false).unwrap(), Some(8));
        assert_eq!(s.find(0, b"CAT", 9, false, true).unwrap(), Some(8));
        assert_eq!(s.replace_all(0, b"cat", b"lion", false).unwrap(), 2);
        assert_eq!(s.read(0, 0, 20).unwrap(), b"lion dog lion");
        assert_eq!(s.state(&VIEW).sizes, [13, 11]);
        realign(&mut s);
        assert_eq!(s.state(&VIEW).diff_blocks, 2);
        s.undo();
        assert_eq!(s.read(0, 0, 20).unwrap(), b"cat dog cat");
    }

    #[test]
    fn stale_realign_is_dropped_and_save_clears_history() {
        let (dir, mut s) = session(b"aaaa", b"bbbb");
        let job = s.align_job(s.limit()).unwrap();
        s.edit(0, 0, 1, b"b");
        assert!(!s.apply(job.run().unwrap()));
        s.save(0, false).unwrap();
        assert_eq!(std::fs::read(dir.path().join("l.bin")).unwrap(), b"baaa");
        let st = s.state(&VIEW);
        assert!(!st.dirty[0] && !st.can_undo);
        assert_eq!(s.changed_on_disk(), [false, false]);
        realign(&mut s);
        assert_eq!(s.state(&VIEW).differing_bytes, 3);
    }

    #[test]
    fn offset_mode_beyond_the_limit() {
        let dir = tempfile::tempdir().unwrap();
        let (l, r) = (dir.path().join("l"), dir.path().join("r"));
        std::fs::write(&l, b"0123456789").unwrap();
        std::fs::write(&r, b"x0123456789").unwrap();
        let mut s = HexSession::open(&l, &r, 4).unwrap();
        let st = s.state(&VIEW);
        assert_eq!(st.mode, AlignMode::Offset);
        assert!(st.note.is_some());
        assert_eq!(st.differing_bytes, 11);
    }
}
