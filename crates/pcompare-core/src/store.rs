//! Small helpers for the JSON files PCompare keeps in its config directory.

use crate::{CoreError, Result};
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// `[year, month, day, hour, minute, second]` in UTC for seconds since the
/// Unix epoch.
pub fn utc_civil(secs: i64) -> [i64; 6] {
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Civil-from-days (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    [year, month, day, rem / 3600, rem % 3600 / 60, rem % 60]
}

/// A short id that is unique within this machine: millisecond clock plus a
/// process-wide counter, so two ids minted in the same millisecond differ.
pub fn new_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{:x}{:04x}", now_ms(), (n ^ std::process::id() as u64) & 0xffff)
}

/// Reads a JSON file, returning `T::default()` when it does not exist yet.
///
/// A file that fails to parse is moved aside (never overwritten) so a bad
/// edit or a newer app version can't silently destroy the user's data.
pub fn read_json<T: DeserializeOwned + Default>(path: &Path) -> Result<T> {
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(T::default()),
        Err(e) => return Err(CoreError::io(path, e)),
    };
    match serde_json::from_str(&text) {
        Ok(v) => Ok(v),
        Err(_) => {
            let aside = path.with_extension(format!("corrupt-{}.json", now_ms()));
            fs::rename(path, &aside).map_err(|e| CoreError::io(path, e))?;
            Ok(T::default())
        }
    }
}

/// Writes JSON via a temp file + rename so a crash never leaves half a file.
pub fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| CoreError::io(dir, e))?;
    }
    let json = serde_json::to_vec_pretty(value).map_err(|e| CoreError::Other(e.to_string()))?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, json).map_err(|e| CoreError::io(&tmp, e))?;
    fs::rename(&tmp, path).map_err(|e| CoreError::io(path, e))
}
