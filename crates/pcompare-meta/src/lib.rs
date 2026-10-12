//! Registry, executable version and audio tag comparison for PCompare.
//!
//! * [`registry`] parses `.reg` exports (and, on Windows, live registry keys)
//!   into key trees and diffs them.
//! * [`version`] reads PE version resources and header facts.
//! * [`audio`] reads audio tags, embedded pictures and stream properties.
//!
//! Version and audio results share the flat [`PropertyRow`] model.

pub mod audio;
pub mod property;
pub mod registry;
pub mod version;

pub use property::{
    compare_properties, DiffStatus, Property, PropertyCompareResult, PropertyRow, PropertyStats,
};

/// Formats a byte count as `1,234,567 bytes`.
pub(crate) fn format_bytes(n: u64) -> String {
    format!("{} bytes", group_thousands(n))
}

pub(crate) fn group_thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Formats seconds since the Unix epoch as `YYYY-MM-DD HH:MM:SS UTC`.
pub(crate) fn format_unix_utc(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // Howard Hinnant's civil-from-days algorithm.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02} UTC",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thousands() {
        assert_eq!(group_thousands(0), "0");
        assert_eq!(group_thousands(999), "999");
        assert_eq!(group_thousands(1000), "1,000");
        assert_eq!(group_thousands(1234567), "1,234,567");
    }

    #[test]
    fn unix_dates() {
        assert_eq!(format_unix_utc(0), "1970-01-01 00:00:00 UTC");
        assert_eq!(format_unix_utc(951_782_400), "2000-02-29 00:00:00 UTC");
        assert_eq!(format_unix_utc(1_700_000_000), "2023-11-14 22:13:20 UTC");
    }
}
