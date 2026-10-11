//! Binary comparison with random access, so the UI can page through files of
//! any size without loading them into memory.

use crate::{CoreError, Result};
use serde::Serialize;
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
