//! Cell values read from any engine, in one canonical form.
//!
//! Drivers convert native types into [`Value`] so rows from SQL Server and
//! MySQL can be compared with each other, and dialects turn them back into
//! SQL literals for data sync scripts.
//!
//! Canonical text formats (drivers must produce exactly these):
//! - `Decimal`: plain digits, `-` sign, `.` separator, no exponent, trailing
//!   fractional zeros removed (`12.50` → `12.5`, `3.000` → `3`).
//! - `Date`: `YYYY-MM-DD`; `Time`: `HH:MM:SS[.fraction]`.
//! - `DateTime`: `YYYY-MM-DD HH:MM:SS[.fraction]` (no zone).
//! - `DateTimeTz`: `YYYY-MM-DD HH:MM:SS[.fraction]+HH:MM`.
//!   Fractions have trailing zeros removed and are omitted when zero.
//! - `Uuid`: lower-case hyphenated.

use serde::{Serialize, Serializer};
use std::cmp::Ordering;
use std::fmt::{self, Write};

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    /// Unsigned values that don't fit in `i64` (MySQL `bigint unsigned`).
    UInt(u64),
    Float(f64),
    Decimal(String),
    Text(String),
    Bytes(Vec<u8>),
    Date(String),
    Time(String),
    DateTime(String),
    DateTimeTz(String),
    Uuid(String),
    /// JSON / XML documents, compared as text.
    Document(String),
}

impl Value {
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    /// The value as a number, for cross-type numeric comparison.
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Bool(b) => Some(*b as u8 as f64),
            Value::Int(i) => Some(*i as f64),
            Value::UInt(u) => Some(*u as f64),
            Value::Float(f) => Some(*f),
            Value::Decimal(d) => d.parse().ok(),
            _ => None,
        }
    }

    /// Exact integer view, so large keys compare without float rounding.
    pub fn as_i128(&self) -> Option<i128> {
        match self {
            Value::Bool(b) => Some(*b as i128),
            Value::Int(i) => Some(*i as i128),
            Value::UInt(u) => Some(*u as i128),
            Value::Decimal(d) if !d.contains('.') => d.parse().ok(),
            _ => None,
        }
    }

    /// Hex rendering used for display and keys: `0x0aff`.
    pub fn hex(bytes: &[u8]) -> String {
        let mut s = String::with_capacity(2 + bytes.len() * 2);
        s.push_str("0x");
        for b in bytes {
            let _ = write!(s, "{b:02x}");
        }
        s
    }

    /// Strips trailing fractional zeros: `"12.500"` → `"12.5"`, `"3.0"` → `"3"`.
    pub fn canonical_decimal(s: &str) -> String {
        let s = s.trim();
        if !s.contains('.') {
            return if s == "-0" { "0".into() } else { s.into() };
        }
        let t = s.trim_end_matches('0').trim_end_matches('.');
        match t {
            "" | "-" | "-0" => "0".into(),
            _ => t.into(),
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => f.write_str("NULL"),
            Value::Bool(b) => f.write_str(if *b { "true" } else { "false" }),
            Value::Int(i) => write!(f, "{i}"),
            Value::UInt(u) => write!(f, "{u}"),
            Value::Float(x) => write!(f, "{x}"),
            Value::Bytes(b) => f.write_str(&Value::hex(b)),
            Value::Decimal(s)
            | Value::Text(s)
            | Value::Date(s)
            | Value::Time(s)
            | Value::DateTime(s)
            | Value::DateTimeTz(s)
            | Value::Uuid(s)
            | Value::Document(s) => f.write_str(s),
        }
    }
}

/// The UI receives `null` or display text (strings keep 64-bit integers exact in JS).
impl Serialize for Value {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Value::Null => s.serialize_none(),
            v => s.serialize_str(&v.to_string()),
        }
    }
}

/// How values are matched in Data Compare.
#[derive(Debug, Clone, Default, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ValueRules {
    pub ignore_case: bool,
    /// Ignore leading/trailing whitespace (CHAR padding differs between engines).
    pub trim_whitespace: bool,
    /// Numbers within this absolute difference are equal; `None` = exact.
    pub numeric_tolerance: Option<f64>,
    /// Compare date-times only to this many fractional-second digits
    /// (SQL Server `datetime` keeps ~3, MySQL/PostgreSQL 6); `None` = all.
    pub time_precision: Option<u8>,
}

/// Decides whether two cells hold the same value.
///
/// This is the heart of Data Compare: rows that differ only in ways this
/// function forgives are reported as identical, and no sync statement is
/// generated for them.
///
/// The default is deliberately strict apart from numbers (which compare by
/// value across types): case, padding and time precision only count as equal
/// when the user opts in through [`ValueRules`].
pub fn values_equal(a: &Value, b: &Value, rules: &ValueRules) -> bool {
    use Value::*;
    match (a, b) {
        (Null, Null) => true,
        (Null, _) | (_, Null) => false,
        _ => {
            // Numbers compare by value across Int/Decimal/Float/Bool, so
            // `bit` = 1 matches `tinyint(1)` = 1 and `12.50` matches `12.5`.
            if let (Some(x), Some(y)) = (a.as_i128(), b.as_i128()) {
                return x == y;
            }
            if let (Some(x), Some(y)) = (a.as_f64(), b.as_f64()) {
                let tol = rules.numeric_tolerance.unwrap_or(0.0);
                return (x - y).abs() <= tol || x == y;
            }
            let (l, r) = (a.to_string(), b.to_string());
            let (l, r) = if rules.trim_whitespace { (l.trim(), r.trim()) } else { (l.as_str(), r.as_str()) };
            if let (Some(p), true) = (rules.time_precision, is_temporal(a) && is_temporal(b)) {
                return truncate_fraction(l, p) == truncate_fraction(r, p);
            }
            if rules.ignore_case {
                l.to_lowercase() == r.to_lowercase()
            } else {
                l == r
            }
        }
    }
}

fn is_temporal(v: &Value) -> bool {
    matches!(v, Value::Time(_) | Value::DateTime(_) | Value::DateTimeTz(_))
}

/// `"10:00:00.123456"` at precision 3 → `"10:00:00.123"`; zone suffixes are kept.
fn truncate_fraction(s: &str, digits: u8) -> String {
    let Some(dot) = s.find('.') else { return s.to_owned() };
    let frac_end = s[dot + 1..]
        .find(|c: char| !c.is_ascii_digit())
        .map_or(s.len(), |i| dot + 1 + i);
    let keep = (dot + 1 + digits as usize).min(frac_end);
    let mut frac = s[dot + 1..keep].trim_end_matches('0').to_owned();
    if !frac.is_empty() {
        frac.insert(0, '.');
    }
    format!("{}{}{}", &s[..dot], frac, &s[frac_end..])
}

/// Total order used to sort keys for display (numbers numerically, then text).
pub fn compare_values(a: &Value, b: &Value) -> Ordering {
    match (a, b) {
        (Value::Null, Value::Null) => Ordering::Equal,
        (Value::Null, _) => Ordering::Less,
        (_, Value::Null) => Ordering::Greater,
        _ => match (a.as_i128(), b.as_i128()) {
            (Some(x), Some(y)) => x.cmp(&y),
            _ => match (a.as_f64(), b.as_f64()) {
                (Some(x), Some(y)) => x.partial_cmp(&y).unwrap_or(Ordering::Equal),
                _ => a.to_string().cmp(&b.to_string()),
            },
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Value::*;

    #[test]
    fn canonical_decimal_strips_zeros() {
        assert_eq!(Value::canonical_decimal("12.500"), "12.5");
        assert_eq!(Value::canonical_decimal("3.000"), "3");
        assert_eq!(Value::canonical_decimal("-0.00"), "0");
        assert_eq!(Value::canonical_decimal("100"), "100");
    }

    #[test]
    fn values_equal_numbers_across_types() {
        let r = ValueRules::default();
        assert!(values_equal(&Int(1), &Bool(true), &r));
        assert!(values_equal(&Decimal("12.5".into()), &Float(12.5), &r));
        assert!(values_equal(&UInt(u64::MAX), &Decimal(u64::MAX.to_string()), &r));
        assert!(!values_equal(&Int(1), &Int(2), &r));
        let tol = ValueRules { numeric_tolerance: Some(0.01), ..r };
        assert!(values_equal(&Float(1.0), &Float(1.005), &tol));
    }

    #[test]
    fn values_equal_nulls() {
        let r = ValueRules::default();
        assert!(values_equal(&Null, &Null, &r));
        assert!(!values_equal(&Null, &Text(String::new()), &r));
    }

    #[test]
    fn values_equal_text_rules() {
        let r = ValueRules::default();
        assert!(!values_equal(&Text("abc ".into()), &Text("abc".into()), &r));
        let trim = ValueRules { trim_whitespace: true, ..r.clone() };
        assert!(values_equal(&Text("abc ".into()), &Text("abc".into()), &trim));
        let case = ValueRules { ignore_case: true, ..r };
        assert!(values_equal(&Text("ABC".into()), &Text("abc".into()), &case));
    }

    #[test]
    fn values_equal_time_precision() {
        let r = ValueRules { time_precision: Some(3), ..Default::default() };
        let a = DateTime("2024-01-02 10:00:00.123456".into());
        let b = DateTime("2024-01-02 10:00:00.123".into());
        assert!(values_equal(&a, &b, &r));
        assert!(!values_equal(&a, &b, &ValueRules::default()));
        assert_eq!(truncate_fraction("10:00:00.100+02:00", 3), "10:00:00.1+02:00");
    }
}
