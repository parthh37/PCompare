//! Lenient date / time parsing for "ignore differences within a date tolerance".
//!
//! Cells are read as a point in time (seconds since 1970-01-01, with no time
//! zone treated as UTC) so that two spellings of nearly the same moment can be
//! compared numerically. Accepted shapes:
//!
//! * ISO: `2024-03-05`, `2024/03/05`, `2024-03-05T14:30:00.250Z`,
//!   `2024-03-05 14:30+02:00` (what spreadsheet date cells are rendered as)
//! * numeric day/month/year: `/` reads as US month-first (`3/5/2024`), `.` and
//!   `-` as day-first (`05.03.2024`, `05-03-2024`); a first field over 12 (or a
//!   second field over 12) settles the order either way. Two-digit years are
//!   only accepted with `/` (`3/5/24`), so version strings like `1.2.10` stay text
//! * month names: `5 Mar 2024`, `05-Mar-2024`, `March 5, 2024`, with an
//!   optional leading weekday (`Tue, 5 Mar 2024 14:30:00 +0000`)
//! * a time on its own (`14:30`, `2:30:15 PM`) counts from midnight
//!
//! A time may carry `AM`/`PM` and a `Z` / `UTC` / `GMT` / `±HH[:MM]` zone.
//! Plain numbers are never dates.

const MONTHS: [&str; 12] = [
    "january",
    "february",
    "march",
    "april",
    "may",
    "june",
    "july",
    "august",
    "september",
    "october",
    "november",
    "december",
];
const WEEKDAYS: [&str; 7] = [
    "monday",
    "tuesday",
    "wednesday",
    "thursday",
    "friday",
    "saturday",
    "sunday",
];

/// Parses `s` as a date, date-time or time of day; returns seconds since the
/// Unix epoch (a bare time is seconds since midnight of 1970-01-01).
pub fn parse_datetime(s: &str) -> Option<f64> {
    let s = s.trim();
    // Cheap rejections: dates are short and always contain a digit.
    if s.len() < 3 || s.len() > 64 || !s.bytes().any(|b| b.is_ascii_digit()) {
        return None;
    }
    let lower = s.to_ascii_lowercase();
    let (date, time) = split_date_time(&lower);
    let days = if date.is_empty() { 0 } else { parse_date(date)? };
    let secs = match time {
        Some(t) => parse_time(t)?,
        None if date.is_empty() => return None,
        None => 0.0,
    };
    Some(days as f64 * 86_400.0 + secs)
}

/// Splits at the time of day: the digit run right before the first `:`.
fn split_date_time(s: &str) -> (&str, Option<&str>) {
    let Some(colon) = s.find(':') else {
        return (s, None);
    };
    let start = s[..colon].rfind(|c: char| !c.is_ascii_digit()).map_or(0, |i| i + 1);
    let date = s[..start].trim_end_matches(|c: char| c == 't' || c == ',' || c.is_whitespace());
    // `2024-03-05T14:30` needs the `T` gone; a lone `t` in front of a time is not a date.
    (date, Some(&s[start..]))
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        2 if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

fn ymd(y: i64, m: u32, d: u32) -> Option<i64> {
    ((1..=9999).contains(&y) && (1..=12).contains(&m) && d >= 1 && d <= days_in_month(y, m))
        .then(|| days_from_civil(y, m, d))
}

#[derive(Debug, PartialEq)]
enum Tok<'a> {
    /// Value and digit count.
    Num(u32, usize),
    Word(&'a str),
    Sep(char),
}

fn tokenize(s: &str) -> Option<Vec<Tok<'_>>> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let start = i;
        let c = b[i];
        if c.is_ascii_digit() {
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
            if i - start > 4 {
                return None;
            }
            out.push(Tok::Num(s[start..i].parse().ok()?, i - start));
        } else if c.is_ascii_alphabetic() {
            while i < b.len() && b[i].is_ascii_alphabetic() {
                i += 1;
            }
            out.push(Tok::Word(&s[start..i]));
        } else if matches!(c, b'-' | b'/' | b'.' | b',' | b' ' | b'\t') {
            while i < b.len() && b[i] == b' ' && c == b' ' {
                i += 1;
            }
            if i == start {
                i += 1;
            }
            out.push(Tok::Sep(if c == b'\t' { ' ' } else { c as char }));
        } else {
            return None;
        }
    }
    Some(out)
}

/// A month name or its 3+ letter abbreviation (`sep` / `sept` included).
fn month(word: &str) -> Option<u32> {
    if word.len() < 3 {
        return None;
    }
    MONTHS
        .iter()
        .position(|m| m.starts_with(word) || (word == "sept" && *m == "september"))
        .map(|i| i as u32 + 1)
}

fn is_weekday(word: &str) -> bool {
    word.len() >= 3 && WEEKDAYS.iter().any(|d| d.starts_with(word))
}

fn year(value: u32, digits: usize) -> Option<i64> {
    match digits {
        4 => Some(value as i64),
        2 => Some(if value < 50 {
            2000 + value as i64
        } else {
            1900 + value as i64
        }),
        _ => None,
    }
}

fn parse_date(s: &str) -> Option<i64> {
    let mut toks = tokenize(s)?;
    // Drop a leading weekday and the separators after it.
    if matches!(toks.first(), Some(Tok::Word(w)) if is_weekday(w)) {
        toks.remove(0);
        while matches!(toks.first(), Some(Tok::Sep(_))) {
            toks.remove(0);
        }
    }
    let seps: Vec<char> = toks
        .iter()
        .filter_map(|t| match t {
            Tok::Sep(c) => Some(*c),
            _ => None,
        })
        .collect();
    let parts: Vec<&Tok> = toks.iter().filter(|t| !matches!(t, Tok::Sep(_))).collect();
    let [a, b, c] = parts[..] else {
        return None;
    };
    match (a, b, c) {
        (Tok::Num(a, an), Tok::Num(b, _), Tok::Num(c, cn)) => {
            // All-numeric dates need exactly two identical separators.
            let [s1, s2] = seps[..] else {
                return None;
            };
            if s1 != s2 || s1 == ' ' || s1 == ',' {
                return None;
            }
            if *an == 4 {
                return ymd(*a as i64, *b, *c);
            }
            if *an > 2 || (*cn == 2 && s1 != '/') {
                return None;
            }
            let y = year(*c, *cn)?;
            let month_first = if *a > 12 {
                false
            } else if *b > 12 {
                true
            } else {
                s1 == '/'
            };
            if month_first {
                ymd(y, *a, *b)
            } else {
                ymd(y, *b, *a)
            }
        }
        (Tok::Num(d, dn), Tok::Word(m), Tok::Num(y, yn)) if *dn <= 2 => ymd(year(*y, *yn)?, month(m)?, *d),
        (Tok::Word(m), Tok::Num(d, dn), Tok::Num(y, yn)) if *dn <= 2 => ymd(year(*y, *yn)?, month(m)?, *d),
        (Tok::Num(y, 4), Tok::Word(m), Tok::Num(d, dn)) if *dn <= 2 => ymd(*y as i64, month(m)?, *d),
        _ => None,
    }
}

/// `H:MM[:SS[.fff]] [AM|PM] [Z|UTC|GMT|±HH[:MM]]` → seconds (zone applied).
fn parse_time(s: &str) -> Option<f64> {
    let b = s.as_bytes();
    let mut i = 0;
    /// Reads `min..=max` digits at `*i`.
    fn field(s: &str, i: &mut usize, min: usize, max: usize) -> Option<u32> {
        let b = s.as_bytes();
        let start = *i;
        while *i < b.len() && b[*i].is_ascii_digit() && *i - start < max {
            *i += 1;
        }
        if *i - start < min {
            return None;
        }
        s[start..*i].parse().ok()
    }
    let h = field(s, &mut i, 1, 2)?;
    if b.get(i) != Some(&b':') {
        return None;
    }
    i += 1;
    let m = field(s, &mut i, 2, 2)?;
    let mut sec = 0.0;
    if b.get(i) == Some(&b':') {
        i += 1;
        sec = field(s, &mut i, 2, 2)? as f64;
        if b.get(i) == Some(&b'.') {
            i += 1;
            let start = i;
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
            if i == start {
                return None;
            }
            sec += format!("0.{}", &s[start..i]).parse::<f64>().ok()?;
        }
    }
    let mut rest = s[i..].trim_start();
    let mut hour = h;
    for (suffix, pm) in [("am", false), ("a.m.", false), ("pm", true), ("p.m.", true)] {
        if let Some(r) = rest.strip_prefix(suffix) {
            if !(1..=12).contains(&h) {
                return None;
            }
            hour = h % 12 + if pm { 12 } else { 0 };
            rest = r.trim_start();
            break;
        }
    }
    if hour > 24 || m > 59 || sec >= 61.0 || (hour == 24 && (m > 0 || sec > 0.0)) {
        return None;
    }
    let offset = parse_zone(rest)?;
    Some(hour as f64 * 3600.0 + m as f64 * 60.0 + sec - offset as f64)
}

/// Zone offset east of UTC in seconds; empty means UTC.
fn parse_zone(s: &str) -> Option<i64> {
    let s = s.trim();
    if s.is_empty() || matches!(s, "z" | "utc" | "gmt") {
        return Some(0);
    }
    let s = s.strip_prefix("utc").or_else(|| s.strip_prefix("gmt")).unwrap_or(s);
    let (sign, digits) = match s.as_bytes().first()? {
        b'+' => (1, &s[1..]),
        b'-' => (-1, &s[1..]),
        _ => return None,
    };
    let digits: String = digits.chars().filter(|&c| c != ':').collect();
    if !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let (h, m) = match digits.len() {
        1 | 2 => (digits.parse::<i64>().ok()?, 0),
        4 => (digits[..2].parse::<i64>().ok()?, digits[2..].parse::<i64>().ok()?),
        _ => return None,
    };
    (h <= 14 && m < 60).then_some(sign * (h * 3600 + m * 60))
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: f64 = 86_400.0;

    fn p(s: &str) -> f64 {
        parse_datetime(s).unwrap_or_else(|| panic!("{s:?} should parse"))
    }

    #[test]
    fn iso_dates_and_times() {
        assert_eq!(p("1970-01-01"), 0.0);
        assert_eq!(p("1970-01-02"), DAY);
        assert_eq!(p("2000-03-01"), 11_017.0 * DAY);
        assert_eq!(p("2024-02-29"), p("2024/02/29"));
        assert_eq!(p("2024-03-05T14:30:00"), p("2024-03-05") + 14.5 * 3600.0);
        assert_eq!(p("2024-03-05 14:30"), p("2024-03-05T14:30:00Z"));
        assert_eq!(p("2024-03-05T14:30:00.250"), p("2024-03-05T14:30:00") + 0.25);
        assert_eq!(p("2024-03-05T16:30:00+02:00"), p("2024-03-05T14:30:00Z"));
        assert_eq!(p("2024-03-05T09:30:00-0500"), p("2024-03-05 14:30"));
        assert!(parse_datetime("2023-02-29").is_none(), "not a leap year");
        assert!(parse_datetime("2024-13-01").is_none());
        assert!(parse_datetime("2024-01-01 25:00").is_none());
    }

    #[test]
    fn numeric_day_month_orders() {
        let mar5 = p("2024-03-05");
        assert_eq!(p("3/5/2024"), mar5, "slash is month-first");
        assert_eq!(p("03/05/24"), mar5, "two-digit year with slash");
        assert_eq!(p("05.03.2024"), mar5, "dot is day-first");
        assert_eq!(p("05-03-2024"), mar5, "dash is day-first");
        assert_eq!(p("25/12/2024"), p("2024-12-25"), "day > 12 settles the order");
        assert_eq!(p("12.25.2024"), p("2024-12-25"), "month > 12 settles the order");
        assert_eq!(p("3/5/2024 2:30 PM"), mar5 + 14.5 * 3600.0);
        assert_eq!(p("3/5/2024 12:05 am"), mar5 + 300.0);
    }

    #[test]
    fn month_names_and_weekdays() {
        let mar5 = p("2024-03-05");
        for s in [
            "5 Mar 2024",
            "05-Mar-2024",
            "March 5, 2024",
            "mar 5 2024",
            "2024 Mar 05",
            "Tue, 5 Mar 2024",
        ] {
            assert_eq!(p(s), mar5, "{s}");
        }
        assert_eq!(p("Tue, 05 Mar 2024 14:30:00 +0000"), mar5 + 14.5 * 3600.0);
        assert_eq!(p("5 Sept 2024"), p("2024-09-05"));
        assert!(parse_datetime("5 Ma 2024").is_none(), "abbreviations need 3 letters");
    }

    #[test]
    fn times_alone() {
        assert_eq!(p("14:30"), 14.5 * 3600.0);
        assert_eq!(p("2:30:15 pm"), 14.0 * 3600.0 + 30.0 * 60.0 + 15.0);
        assert!(parse_datetime("13:00 PM").is_none());
        assert!(parse_datetime("1:2").is_none());
    }

    #[test]
    fn rejects_non_dates() {
        for s in [
            "",
            "2024",
            "12.5",
            "-3",
            "1.2.10",
            "1.2.3",
            "abc",
            "v1/2/3",
            "2024-01",
            "100/200/2024",
            "a:b",
        ] {
            assert!(parse_datetime(s).is_none(), "{s:?} is not a date");
        }
    }
}
