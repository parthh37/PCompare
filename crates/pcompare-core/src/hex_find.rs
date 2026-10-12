//! Byte-pattern search over edited content, streamed in chunks so it works
//! on files of any size. Text searches arrive already encoded as bytes.

use crate::hex_edit::{ByteSource, IO_CHUNK};
use crate::Result;
use memchr::memmem;

/// Most matches "Replace all" collects in one go.
pub const MAX_MATCHES: usize = 1_000_000;

fn fold(bytes: &mut [u8], fold_case: bool) {
    if fold_case {
        bytes.make_ascii_lowercase();
    }
}

/// The first match at or after `from` (forward), or the last match starting
/// before `from` (backward). `fold_case` ignores ASCII letter case.
pub fn find(
    src: &mut dyn ByteSource,
    pattern: &[u8],
    from: u64,
    forward: bool,
    fold_case: bool,
) -> Result<Option<u64>> {
    let plen = pattern.len() as u64;
    let len = src.len();
    if pattern.is_empty() || plen > len {
        return Ok(None);
    }
    let mut pat = pattern.to_vec();
    fold(&mut pat, fold_case);
    let overlap = plen - 1;
    if forward {
        let finder = memmem::Finder::new(&pat);
        let mut pos = from;
        while pos + plen <= len {
            let mut window = src.read_vec(pos, IO_CHUNK + overlap as usize)?;
            fold(&mut window, fold_case);
            if let Some(i) = finder.find(&window) {
                return Ok(Some(pos + i as u64));
            }
            if window.len() < plen as usize {
                break;
            }
            pos += window.len() as u64 - overlap;
        }
    } else {
        let finder = memmem::FinderRev::new(&pat);
        // Matches must start before `from`, so may end by `from + overlap`.
        let mut end = (from.min(len) + overlap).min(len);
        while end >= plen {
            let start = end.saturating_sub(IO_CHUNK as u64 + overlap);
            let mut window = src.read_vec(start, (end - start) as usize)?;
            fold(&mut window, fold_case);
            if let Some(i) = finder.rfind(&window) {
                return Ok(Some(start + i as u64));
            }
            if start == 0 {
                break;
            }
            end = start + overlap;
        }
    }
    Ok(None)
}

/// Non-overlapping matches from the start, up to `limit`.
pub fn find_all(src: &mut dyn ByteSource, pattern: &[u8], fold_case: bool, limit: usize) -> Result<Vec<u64>> {
    let plen = pattern.len() as u64;
    let len = src.len();
    let mut out = Vec::new();
    if pattern.is_empty() || plen > len {
        return Ok(out);
    }
    let mut pat = pattern.to_vec();
    fold(&mut pat, fold_case);
    let finder = memmem::Finder::new(&pat);
    let overlap = plen - 1;
    let mut pos = 0u64;
    while pos + plen <= len && out.len() < limit {
        let mut window = src.read_vec(pos, IO_CHUNK + overlap as usize)?;
        fold(&mut window, fold_case);
        // Next window starts where an unfinished match could begin, or after the last match.
        let mut next = pos + (window.len() as u64).saturating_sub(overlap).max(1);
        for i in finder.find_iter(&window) {
            let at = pos + i as u64;
            if out.last().is_some_and(|&last| at < last + plen) {
                continue;
            }
            out.push(at);
            next = next.max(at + plen);
            if out.len() >= limit {
                break;
            }
        }
        if window.len() < plen as usize {
            break;
        }
        pos = next;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn finds_forward_and_backward() {
        let mut m = Mem(b"abc DEF abc def".to_vec());
        assert_eq!(find(&mut m, b"abc", 0, true, false).unwrap(), Some(0));
        assert_eq!(find(&mut m, b"abc", 1, true, false).unwrap(), Some(8));
        assert_eq!(find(&mut m, b"abc", 9, true, false).unwrap(), None);
        assert_eq!(find(&mut m, b"abc", 15, false, false).unwrap(), Some(8));
        assert_eq!(find(&mut m, b"abc", 8, false, false).unwrap(), Some(0));
        assert_eq!(find(&mut m, b"abc", 0, false, false).unwrap(), None);
        assert_eq!(find(&mut m, b"def", 0, true, false).unwrap(), Some(12));
        assert_eq!(find(&mut m, b"def", 0, true, true).unwrap(), Some(4));
        assert_eq!(find(&mut m, b"", 0, true, false).unwrap(), None);
    }

    #[test]
    fn matches_across_chunk_boundaries() {
        let mut data = vec![0u8; IO_CHUNK * 2 + 10];
        let at = IO_CHUNK - 2;
        data[at..at + 4].copy_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);
        let tail = data.len() - 4;
        data[tail..].copy_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);
        let mut m = Mem(data);
        let pat = [0xDE, 0xAD, 0xBE, 0xEF];
        assert_eq!(find(&mut m, &pat, 0, true, false).unwrap(), Some(at as u64));
        assert_eq!(
            find(&mut m, &pat, at as u64 + 1, true, false).unwrap(),
            Some(tail as u64)
        );
        assert_eq!(find(&mut m, &pat, tail as u64, false, false).unwrap(), Some(at as u64));
        assert_eq!(find_all(&mut m, &pat, false, 10).unwrap(), vec![at as u64, tail as u64]);
    }

    #[test]
    fn find_all_skips_overlaps_and_honours_limit() {
        let mut m = Mem(b"aaaaa".to_vec());
        assert_eq!(find_all(&mut m, b"aa", false, 10).unwrap(), vec![0, 2]);
        assert_eq!(find_all(&mut m, b"a", false, 3).unwrap(), vec![0, 1, 2]);
        let mut m = Mem(b"Hello hello HELLO".to_vec());
        assert_eq!(find_all(&mut m, b"hello", true, 10).unwrap(), vec![0, 6, 12]);
    }
}
