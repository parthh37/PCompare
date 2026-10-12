//! Byte-level alignment of two binaries, so an inserted or deleted run shows
//! as a gap on the other side instead of making everything after it differ.
//!
//! Files up to a size limit are aligned in memory: matching 32-byte blocks
//! found with a rolling hash anchor the alignment, and the short stretches
//! between anchors are refined with Myers. Larger files fall back to a
//! streamed offset-by-offset compare. Either way the result is a list of
//! [`Segment`]s that tile both files, laid out on a shared "display" axis
//! where each segment is as wide as its longer side.

use crate::hex_edit::{ByteSource, IO_CHUNK};
use crate::Result;
use serde::{Deserialize, Serialize};
use similar::{Algorithm, DiffTag};
use std::ops::Range;
use std::time::{Duration, Instant};

/// Default per-side size limit for byte alignment.
pub const DEFAULT_ALIGN_LIMIT: u64 = 64 << 20;
/// Anchor block size; also the shortest match that can anchor a jump.
const BLOCK: usize = 32;
/// Hash candidates checked per right-side position.
const CANDIDATES: usize = 4;
/// Gaps between anchors up to this many bytes (both sides together) are
/// refined with Myers; longer ones are shown as one changed block.
const MYERS_WINDOW: usize = 256 * 1024;
const MYERS_DEADLINE: Duration = Duration::from_millis(100);
/// Equal runs shorter than this inside a changed stretch are folded into it,
/// so random data doesn't align on coincidental single bytes.
const MIN_EQUAL: usize = 3;
/// Offset compare: differences closer than this merge into one block.
const OFFSET_MERGE_GAP: u64 = 16;
/// Offset compare: past this many segments, the rest of the file is one block.
const MAX_SEGMENTS: usize = 2_000_000;

/// A run that is equal on both sides, or a changed run whose sides may have
/// different lengths. In a changed run the sides are paired byte by byte from
/// the start and the shorter side shows a gap at the end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Segment {
    pub equal: bool,
    pub start: [u64; 2],
    pub len: [u64; 2],
    /// Display positions in this segment where the sides differ.
    pub diff: u64,
}

impl Segment {
    pub fn equal(l: u64, r: u64, len: u64) -> Segment {
        Segment {
            equal: true,
            start: [l, r],
            len: [len, len],
            diff: 0,
        }
    }

    /// A changed run; `diff` starts as an upper bound until counted.
    pub fn change(l: u64, r: u64, llen: u64, rlen: u64) -> Segment {
        Segment {
            equal: false,
            start: [l, r],
            len: [llen, rlen],
            diff: llen.max(rlen),
        }
    }

    pub fn width(&self) -> u64 {
        self.len[0].max(self.len[1])
    }

    fn end(&self, side: usize) -> u64 {
        self.start[side] + self.len[side]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum AlignMode {
    /// Insertions and deletions are aligned.
    Aligned,
    /// Too large to align; bytes are compared at equal offsets.
    Offset,
}

/// Which rows a view shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RowFilter {
    All,
    Differences,
    /// Differences plus `context` rows around each.
    Context,
}

/// Merges neighbours of the same kind and drops empty segments. Two changed
/// runs merge into one whose sides are paired from the start; the diff count
/// stays exact when the first run has no gap, else it becomes an upper bound.
pub fn normalize(segs: Vec<Segment>) -> Vec<Segment> {
    let mut out: Vec<Segment> = Vec::with_capacity(segs.len());
    for s in segs {
        if s.width() == 0 {
            continue;
        }
        match out.last_mut() {
            Some(last) if last.equal && s.equal => {
                last.len[0] += s.len[0];
                last.len[1] += s.len[1];
            }
            Some(last) if !last.equal && !s.equal => {
                let exact = last.len[0] == last.len[1];
                last.len[0] += s.len[0];
                last.len[1] += s.len[1];
                last.diff = if exact { last.diff + s.diff } else { last.width() };
            }
            _ => out.push(s),
        }
    }
    out
}

fn common_prefix(a: &[u8], b: &[u8]) -> usize {
    a.iter().zip(b).take_while(|(x, y)| x == y).count()
}

fn common_suffix(a: &[u8], b: &[u8]) -> usize {
    a.iter().rev().zip(b.iter().rev()).take_while(|(x, y)| x == y).count()
}

const HASH_MUL: u64 = 0x100_0000_01b3;

fn block_hash(block: &[u8]) -> u64 {
    block
        .iter()
        .fold(0u64, |h, &b| h.wrapping_mul(HASH_MUL).wrapping_add(b as u64 + 1))
}

/// `HASH_MUL^(BLOCK-1)`, to drop the outgoing byte when rolling.
fn hash_out_factor() -> u64 {
    (1..BLOCK).fold(1u64, |f, _| f.wrapping_mul(HASH_MUL))
}

fn mix(h: u64) -> u64 {
    h.wrapping_mul(0x9E37_79B9_7F4A_7C15)
}

/// Block hashes of `a` at block-aligned positions, bucketed by hash for
/// O(1) lookup, plus a bitmap that rules out most misses with one read.
struct BlockIndex {
    /// `(hash, position)`, sorted by bucket, then hash, then position.
    entries: Vec<(u64, usize)>,
    /// `entries[starts[b]..starts[b + 1]]` is bucket `b`.
    starts: Vec<u32>,
    bucket_bits: u32,
    filter: Vec<u64>,
    /// log2 of the filter's bit count, sized so few lookups pass by chance.
    filter_bits: u32,
}

impl BlockIndex {
    fn new(a: &[u8], range: Range<usize>) -> BlockIndex {
        let mut entries: Vec<(u64, usize)> = (range.start..range.end.saturating_sub(BLOCK - 1))
            .step_by(BLOCK)
            .map(|p| (block_hash(&a[p..p + BLOCK]), p))
            .collect();
        let n = entries.len() as u64;
        let bucket_bits = n.next_power_of_two().trailing_zeros().clamp(4, 26);
        // About 32 bits per block keeps chance filter hits near 3%.
        let filter_bits = (n * 32).next_power_of_two().trailing_zeros().clamp(12, 28);
        let bucket = |h: u64| (mix(h) >> (64 - bucket_bits)) as usize;
        entries.sort_unstable_by_key(|&(h, p)| (bucket(h), h, p));
        let mut starts = vec![0u32; (1 << bucket_bits) + 1];
        for &(h, _) in &entries {
            starts[bucket(h) + 1] += 1;
        }
        for i in 1..starts.len() {
            starts[i] += starts[i - 1];
        }
        let mut filter = vec![0u64; 1 << (filter_bits - 6)];
        for &(h, _) in &entries {
            let slot = (mix(h).rotate_left(32) >> (64 - filter_bits)) as usize;
            filter[slot >> 6] |= 1 << (slot & 63);
        }
        BlockIndex {
            entries,
            starts,
            bucket_bits,
            filter,
            filter_bits,
        }
    }

    /// The position in `a` (at or after `min`, ending by `max_end`) whose
    /// block equals `block`, nearest to `expected`.
    fn lookup(&self, h: u64, block: &[u8], a: &[u8], min: usize, max_end: usize, expected: usize) -> Option<usize> {
        let m = mix(h);
        let slot = (m.rotate_left(32) >> (64 - self.filter_bits)) as usize;
        if self.filter[slot >> 6] & (1 << (slot & 63)) == 0 {
            return None;
        }
        let b = (m >> (64 - self.bucket_bits)) as usize;
        let bucket = &self.entries[self.starts[b] as usize..self.starts[b + 1] as usize];
        let lo = bucket.partition_point(|e| e.0 < h);
        let hi = lo + bucket[lo..].partition_point(|e| e.0 == h);
        let same = &bucket[lo..hi];
        let first = same.partition_point(|e| e.1 < min);
        let mid = first + same[first..].partition_point(|e| e.1 < expected);
        let near = same[first..mid]
            .iter()
            .rev()
            .take(CANDIDATES)
            .chain(same[mid..].iter().take(CANDIDATES));
        near.map(|e| e.1)
            .filter(|&p| p + BLOCK <= max_end && &a[p..p + BLOCK] == block)
            .min_by_key(|&p| p.abs_diff(expected))
    }
}

/// Matching runs `(left, right, len)`, ascending on both sides.
fn find_anchors(a: &[u8], b: &[u8]) -> Vec<(usize, usize, usize)> {
    let mut out = Vec::new();
    let prefix = common_prefix(a, b);
    if prefix > 0 {
        out.push((0, 0, prefix));
    }
    let suffix = common_suffix(&a[prefix..], &b[prefix..]);
    let (a_end, b_end) = (a.len() - suffix, b.len() - suffix);
    let (mut li, mut ri) = (prefix, prefix);
    if a_end - li >= BLOCK && b_end - ri >= BLOCK {
        let index = BlockIndex::new(a, li..a_end);
        let out_factor = hash_out_factor();
        let mut r = ri;
        let mut h = block_hash(&b[r..r + BLOCK]);
        while r + BLOCK <= b_end {
            let block = &b[r..r + BLOCK];
            // Cheapest and most common: the bytes continue on the same diagonal.
            let expected = li + (r - ri);
            let found = if expected + BLOCK <= a_end && &a[expected..expected + BLOCK] == block {
                Some(expected)
            } else {
                index.lookup(h, block, a, li, a_end, expected)
            };
            if let Some(mut l) = found {
                let mut r0 = r;
                while l > li && r0 > ri && a[l - 1] == b[r0 - 1] {
                    l -= 1;
                    r0 -= 1;
                }
                let len = common_prefix(&a[l..a_end], &b[r0..b_end]);
                out.push((l, r0, len));
                li = l + len;
                ri = r0 + len;
                r = ri;
                if r + BLOCK <= b_end {
                    h = block_hash(&b[r..r + BLOCK]);
                }
            } else {
                if r + BLOCK < b_end {
                    let outgoing = (b[r] as u64 + 1).wrapping_mul(out_factor);
                    h = h
                        .wrapping_sub(outgoing)
                        .wrapping_mul(HASH_MUL)
                        .wrapping_add(b[r + BLOCK] as u64 + 1);
                }
                r += 1;
            }
        }
    }
    if suffix > 0 {
        out.push((a_end, b_end, suffix));
    }
    out
}

/// Aligns the unmatched stretch between two anchors.
fn fill_gap(a: &[u8], b: &[u8], lr: Range<usize>, rr: Range<usize>, out: &mut Vec<Segment>) {
    let (n, m) = (lr.len(), rr.len());
    if n == 0 && m == 0 {
        return;
    }
    // Same length: a straight substitution reads best (and is what BC shows).
    if n == 0 || m == 0 || n == m || n + m > MYERS_WINDOW {
        out.push(Segment::change(lr.start as u64, rr.start as u64, n as u64, m as u64));
        return;
    }
    let deadline = Instant::now() + MYERS_DEADLINE;
    let ops = similar::capture_diff_slices_deadline(Algorithm::Myers, &a[lr.clone()], &b[rr.clone()], Some(deadline));
    for op in ops {
        let (tag, o, n) = op.as_tag_tuple();
        let (l, r) = ((lr.start + o.start) as u64, (rr.start + n.start) as u64);
        if tag == DiffTag::Equal && o.len() >= MIN_EQUAL {
            out.push(Segment::equal(l, r, o.len() as u64));
        } else {
            out.push(Segment::change(l, r, o.len() as u64, n.len() as u64));
        }
    }
}

fn count_diff(a: &[u8], b: &[u8], s: &Segment) -> u64 {
    let (l, r) = (s.start[0] as usize, s.start[1] as usize);
    let paired = s.len[0].min(s.len[1]) as usize;
    let mismatched = a[l..l + paired]
        .iter()
        .zip(&b[r..r + paired])
        .filter(|(x, y)| x != y)
        .count();
    mismatched as u64 + (s.width() - paired as u64)
}

/// Aligns two in-memory files byte by byte.
pub fn align_bytes(a: &[u8], b: &[u8]) -> Vec<Segment> {
    let mut segs = Vec::new();
    let (mut li, mut ri) = (0, 0);
    for (l, r, len) in find_anchors(a, b) {
        fill_gap(a, b, li..l, ri..r, &mut segs);
        segs.push(Segment::equal(l as u64, r as u64, len as u64));
        li = l + len;
        ri = r + len;
    }
    fill_gap(a, b, li..a.len(), ri..b.len(), &mut segs);
    let mut segs = normalize(segs);
    for s in segs.iter_mut().filter(|s| !s.equal) {
        s.diff = count_diff(a, b, s);
    }
    segs
}

/// Compares two sources offset by offset, streaming, for files too large to
/// align. Nearby differences merge into one block.
pub fn offset_segments(a: &mut dyn ByteSource, b: &mut dyn ByteSource) -> Result<Vec<Segment>> {
    let (la, lb) = (a.len(), b.len());
    let common = la.min(lb);
    let mut segs: Vec<Segment> = Vec::new();
    // The open changed run, as `start..end` plus its differing byte count.
    let mut run: Option<(u64, u64, u64)> = None;
    let mut eq_start = 0u64;
    let (mut ab, mut bb) = (vec![0u8; IO_CHUNK], vec![0u8; IO_CHUNK]);
    let mut offset = 0u64;
    while offset < common {
        let want = ((common - offset) as usize).min(IO_CHUNK);
        let na = a.read_at(offset, &mut ab[..want])?;
        let nb = b.read_at(offset, &mut bb[..want])?;
        let n = na.min(nb);
        if n == 0 {
            break;
        }
        if ab[..n] != bb[..n] {
            for i in (0..n).filter(|&i| ab[i] != bb[i]) {
                let p = offset + i as u64;
                let gap = if segs.len() >= MAX_SEGMENTS {
                    u64::MAX
                } else {
                    OFFSET_MERGE_GAP
                };
                match &mut run {
                    Some((_, end, diff)) if p - *end < gap => {
                        *end = p + 1;
                        *diff += 1;
                    }
                    _ => {
                        if let Some((s, e, d)) = run.take() {
                            segs.push(Segment::equal(eq_start, eq_start, s - eq_start));
                            segs.push(Segment {
                                diff: d,
                                ..Segment::change(s, s, e - s, e - s)
                            });
                            eq_start = e;
                        }
                        run = Some((p, p + 1, 1));
                    }
                }
            }
        }
        offset += n as u64;
    }
    if let Some((s, e, d)) = run {
        segs.push(Segment::equal(eq_start, eq_start, s - eq_start));
        segs.push(Segment {
            diff: d,
            ..Segment::change(s, s, e - s, e - s)
        });
        eq_start = e;
    }
    segs.push(Segment::equal(eq_start, eq_start, common - eq_start));
    segs.push(Segment::change(common, common, la - common, lb - common));
    Ok(normalize(segs))
}

/// The segments of a compare laid out on the display axis.
#[derive(Debug, Clone)]
pub struct Alignment {
    pub mode: AlignMode,
    segs: Vec<Segment>,
    /// Display position where each segment starts.
    display: Vec<u64>,
    total: u64,
    sizes: [u64; 2],
    differing: u64,
    blocks: u64,
}

/// One display position: each side's byte offset, or `None` for a gap.
pub type Cell = [Option<u64>; 2];

impl Alignment {
    pub fn new(mode: AlignMode, segs: Vec<Segment>) -> Alignment {
        let mut a = Alignment {
            mode,
            segs,
            display: Vec::new(),
            total: 0,
            sizes: [0, 0],
            differing: 0,
            blocks: 0,
        };
        a.reindex();
        a
    }

    fn reindex(&mut self) {
        self.display.clear();
        self.display.reserve(self.segs.len());
        let mut at = 0;
        self.differing = 0;
        self.blocks = 0;
        for s in &self.segs {
            self.display.push(at);
            at += s.width();
            self.differing += s.diff;
            self.blocks += u64::from(!s.equal);
        }
        self.total = at;
        self.sizes = match self.segs.last() {
            Some(s) => [s.end(0), s.end(1)],
            None => [0, 0],
        };
    }

    pub fn segments(&self) -> &[Segment] {
        &self.segs
    }

    /// Length of the display axis.
    pub fn display_len(&self) -> u64 {
        self.total
    }

    pub fn differing_bytes(&self) -> u64 {
        self.differing
    }

    /// Number of changed blocks.
    pub fn blocks(&self) -> u64 {
        self.blocks
    }

    fn seg_at(&self, d: u64) -> usize {
        self.display.partition_point(|&s| s <= d).saturating_sub(1)
    }

    /// Each side's offset of the next byte at or after display position `d`.
    pub fn positions_at(&self, d: u64) -> [u64; 2] {
        if d >= self.total {
            return self.sizes;
        }
        let i = self.seg_at(d);
        let (s, k) = (&self.segs[i], d - self.display[i]);
        [s.start[0] + k.min(s.len[0]), s.start[1] + k.min(s.len[1])]
    }

    /// Display position of byte `offset` on `side` (the end of the axis for
    /// the end of the file).
    pub fn locate(&self, side: usize, offset: u64) -> u64 {
        if offset >= self.sizes[side] {
            return self.total;
        }
        let i = self.segs.partition_point(|s| s.end(side) <= offset);
        self.display[i] + (offset - self.segs[i].start[side])
    }

    /// Cells for display positions `d0..d1`.
    pub fn cells(&self, d0: u64, d1: u64) -> Vec<Cell> {
        let d1 = d1.min(self.total);
        let mut out = Vec::with_capacity(d1.saturating_sub(d0) as usize);
        if d0 >= d1 {
            return out;
        }
        let mut i = self.seg_at(d0);
        let mut d = d0;
        while d < d1 && i < self.segs.len() {
            let s = &self.segs[i];
            let end = (self.display[i] + s.width()).min(d1);
            for k in (d - self.display[i])..(end - self.display[i]) {
                out.push([0, 1].map(|side| (k < s.len[side]).then(|| s.start[side] + k)));
            }
            d = end;
            i += 1;
        }
        out
    }

    /// Display start of the next (or previous) changed block relative to
    /// display position `from`; `None` means before the start.
    pub fn next_change(&self, from: Option<u64>, forward: bool) -> Option<u64> {
        let changes = self.segs.iter().zip(&self.display).filter(|(s, _)| !s.equal);
        match (from, forward) {
            (None, true) => changes.map(|(_, &d)| d).next(),
            (None, false) => None,
            (Some(from), true) => changes.map(|(_, &d)| d).find(|&d| d > from),
            (Some(from), false) => changes.rev().find(|(s, &d)| d + s.width() <= from).map(|(_, &d)| d),
        }
    }

    /// Updates the layout after `removed` bytes at `offset` on `side` were
    /// replaced by `inserted` bytes, without re-reading either file: the
    /// touched segments become one changed block. A full re-align later
    /// refines it.
    pub fn patch(&mut self, side: usize, offset: u64, removed: u64, inserted: u64) {
        let other = 1 - side;
        let end = offset + removed;
        // Segments touching `offset..=end` on `side` (contiguous, since both
        // sides are tiled in order).
        let i = self.segs.partition_point(|s| s.end(side) < offset);
        let j = self.segs.partition_point(|s| s.start[side] <= end).max(i);
        let mut repl = Vec::with_capacity(3);
        // The block replacing them covers `us..ue` on `side` before the edit
        // and `os..oe` on the other side. Equal runs keep their untouched ends.
        let (mut us, mut ue) = (offset, end);
        let (mut os, mut oe) = (self.sizes[other], self.sizes[other]);
        let mut suffix = None;
        if i < j {
            let (first, last) = (self.segs[i], self.segs[j - 1]);
            if first.equal && first.start[side] < offset {
                let k = offset - first.start[side];
                repl.push(Segment::equal(first.start[0], first.start[1], k));
                os = first.start[other] + k;
            } else {
                us = first.start[side];
                os = first.start[other];
            }
            if last.equal && last.end(side) > end {
                let k = end - last.start[side];
                oe = last.start[other] + k;
                suffix = Some((oe, last.len[side] - k));
            } else {
                ue = last.end(side);
                oe = last.end(other);
            }
        }
        let new_len = ue - us - removed + inserted;
        let mut block = Segment::change(0, 0, 0, 0);
        block.start[side] = us;
        block.start[other] = os;
        block.len[side] = new_len;
        block.len[other] = oe - os;
        block.diff = block.width();
        repl.push(block);
        if let Some((ostart, len)) = suffix {
            let mut s = Segment::equal(0, 0, len);
            s.start[side] = us + new_len;
            s.start[other] = ostart;
            repl.push(s);
        }
        let tail = self.segs.split_off(j);
        self.segs.truncate(i);
        self.segs.extend(repl);
        self.segs.extend(tail.into_iter().map(|mut s| {
            s.start[side] = s.start[side] - removed + inserted;
            s
        }));
        self.segs = normalize(std::mem::take(&mut self.segs));
        self.reindex();
    }

    /// Which display rows a view shows.
    pub fn row_map(&self, bytes_per_row: u64, filter: RowFilter, context: u64) -> RowMap {
        let bpr = bytes_per_row.max(1);
        let total_rows = self.total.div_ceil(bpr);
        let mut ranges: Vec<(u64, u64)> = Vec::new();
        if filter == RowFilter::All {
            if total_rows > 0 {
                ranges.push((0, total_rows));
            }
        } else {
            let ctx = if filter == RowFilter::Context { context } else { 0 };
            for (s, &d) in self.segs.iter().zip(&self.display) {
                if s.equal {
                    continue;
                }
                let r0 = (d / bpr).saturating_sub(ctx);
                let r1 = ((d + s.width() - 1) / bpr + 1 + ctx).min(total_rows);
                match ranges.last_mut() {
                    Some(last) if r0 <= last.1 => last.1 = last.1.max(r1),
                    _ => ranges.push((r0, r1)),
                }
            }
        }
        let mut before = Vec::with_capacity(ranges.len());
        let mut count = 0;
        for &(a, b) in &ranges {
            before.push(count);
            count += b - a;
        }
        RowMap { ranges, before, count }
    }
}

/// Maps rows of a filtered view to display rows.
#[derive(Debug, Clone)]
pub struct RowMap {
    /// Display row ranges shown, ascending.
    ranges: Vec<(u64, u64)>,
    /// Visible rows before each range.
    before: Vec<u64>,
    pub count: u64,
}

impl RowMap {
    /// The display row of visible row `v`, and whether it starts a range
    /// that isn't the first (the view hides rows above it).
    pub fn display_row(&self, v: u64) -> Option<(u64, bool)> {
        if v >= self.count {
            return None;
        }
        let i = self.before.partition_point(|&b| b <= v) - 1;
        let k = v - self.before[i];
        Some((self.ranges[i].0 + k, k == 0 && (i > 0 || self.ranges[i].0 > 0)))
    }

    /// The visible row showing display row `row`, if the view shows it.
    pub fn visible_row(&self, row: u64) -> Option<u64> {
        let i = self.ranges.partition_point(|r| r.1 <= row);
        let (a, b) = *self.ranges.get(i)?;
        (row >= a && row < b).then(|| self.before[i] + row - a)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Renders an alignment as two strings with `_` for gaps.
    fn render(a: &[u8], b: &[u8]) -> (String, String) {
        let al = Alignment::new(AlignMode::Aligned, align_bytes(a, b));
        let cells = al.cells(0, al.display_len());
        let side = |s: usize, src: &[u8]| {
            cells
                .iter()
                .map(|c| c[s].map_or('_', |o| src[o as usize] as char))
                .collect::<String>()
        };
        (side(0, a), side(1, b))
    }

    fn noise(seed: u64, len: usize) -> Vec<u8> {
        let mut x = seed;
        (0..len)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                (x >> 24) as u8
            })
            .collect()
    }

    #[test]
    fn identical_files_are_one_equal_segment() {
        let segs = align_bytes(b"same bytes", b"same bytes");
        assert_eq!(segs, vec![Segment::equal(0, 0, 10)]);
        assert!(align_bytes(b"", b"").is_empty());
    }

    #[test]
    fn small_insertion_shows_as_gap() {
        let (l, r) = render(b"hello world", b"hello, world");
        assert_eq!(l, "hello_ world");
        assert_eq!(r, "hello, world");
    }

    #[test]
    fn substitution_is_not_shifted() {
        let (l, r) = render(b"abcdefgh", b"abXdeYgh");
        assert_eq!(l, "abcdefgh");
        assert_eq!(r, "abXdeYgh");
        let segs = align_bytes(b"abcdefgh", b"abXdeYgh");
        let al = Alignment::new(AlignMode::Aligned, segs);
        assert_eq!(al.differing_bytes(), 2);
    }

    #[test]
    fn large_insertion_in_the_middle_realigns() {
        let base = noise(7, 50_000);
        let mut right = base[..20_000].to_vec();
        right.extend(noise(99, 3_000));
        right.extend_from_slice(&base[20_000..]);
        let al = Alignment::new(AlignMode::Aligned, align_bytes(&base, &right));
        assert_eq!(al.blocks(), 1);
        assert_eq!(al.differing_bytes(), 3_000);
        assert_eq!(al.display_len(), 53_000);
        // Bytes after the insertion line up again.
        let d = al.locate(0, 30_000);
        assert_eq!(al.positions_at(d), [30_000, 33_000]);
    }

    #[test]
    fn deletions_and_moves_far_apart() {
        let base = noise(3, 200_000);
        let mut right = base.clone();
        right.drain(150_000..150_500);
        right.drain(10_000..10_100);
        right[100_000] ^= 0xFF;
        let al = Alignment::new(AlignMode::Aligned, align_bytes(&base, &right));
        assert_eq!(al.blocks(), 3);
        assert_eq!(al.differing_bytes(), 601);
    }

    #[test]
    fn offset_compare_merges_nearby_differences() {
        use crate::hex_edit::ByteSource;
        struct Mem(Vec<u8>);
        impl ByteSource for Mem {
            fn len(&self) -> u64 {
                self.0.len() as u64
            }
            fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<usize> {
                let src = self.0.get(offset as usize..).unwrap_or_default();
                let n = src.len().min(buf.len());
                buf[..n].copy_from_slice(&src[..n]);
                Ok(n)
            }
        }
        let a = vec![0u8; 1000];
        let mut b = a.clone();
        b[10] = 1;
        b[14] = 1;
        b[500] = 1;
        b.extend([7, 7]);
        let segs = offset_segments(&mut Mem(a), &mut Mem(b)).unwrap();
        let al = Alignment::new(AlignMode::Offset, segs);
        assert_eq!(al.blocks(), 3);
        assert_eq!(al.differing_bytes(), 5);
        assert_eq!(al.next_change(None, true), Some(10));
        assert_eq!(al.next_change(Some(10), true), Some(500));
        assert_eq!(al.next_change(Some(500), true), Some(1000));
        assert_eq!(al.next_change(Some(1000), false), Some(500));
        assert_eq!(al.next_change(Some(10), false), None);
    }

    #[test]
    fn patch_keeps_layout_consistent() {
        let a = b"0123456789abcdef".to_vec();
        let mut al = Alignment::new(AlignMode::Aligned, align_bytes(&a, &a));
        // Insert three bytes on the left at offset 4.
        al.patch(0, 4, 0, 3);
        assert_eq!(
            al.segments(),
            &[
                Segment::equal(0, 0, 4),
                Segment::change(4, 4, 3, 0),
                Segment::equal(7, 4, 12)
            ]
        );
        // Delete two bytes on the right inside the trailing equal run.
        al.patch(1, 10, 2, 0);
        assert_eq!(al.positions_at(al.display_len()), [19, 14]);
        assert_eq!(al.blocks(), 2);
        // Undo both: the inverse patches leave no gaps (diffs stay pending a re-align).
        al.patch(1, 10, 0, 2);
        al.patch(0, 4, 3, 0);
        assert_eq!(al.positions_at(al.display_len()), [16, 16]);
        assert!(al.segments().iter().all(|s| s.len[0] == s.len[1]));
        // Editing empty files.
        let mut empty = Alignment::new(AlignMode::Aligned, Vec::new());
        empty.patch(1, 0, 0, 5);
        assert_eq!(empty.segments(), &[Segment::change(0, 0, 0, 5)]);
    }

    #[test]
    fn row_maps_filter_differences() {
        let a = vec![0u8; 160];
        let mut b = a.clone();
        b[40] = 1;
        b[120] = 1;
        let al = Alignment::new(AlignMode::Aligned, align_bytes(&a, &b));
        let all = al.row_map(16, RowFilter::All, 0);
        assert_eq!(all.count, 10);
        let diffs = al.row_map(16, RowFilter::Differences, 0);
        assert_eq!(diffs.count, 2);
        assert_eq!(diffs.display_row(0), Some((2, true)));
        assert_eq!(diffs.display_row(1), Some((7, true)));
        assert_eq!(diffs.visible_row(7), Some(1));
        assert_eq!(diffs.visible_row(3), None);
        let ctx = al.row_map(16, RowFilter::Context, 1);
        assert_eq!(ctx.count, 6);
        assert_eq!(ctx.display_row(3), Some((6, true)));
    }
}
