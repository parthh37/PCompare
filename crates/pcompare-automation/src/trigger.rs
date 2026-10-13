//! When a task runs: the trigger kinds and their next-fire arithmetic, in
//! local time.
//!
//! DST rules: a wall-clock time that doesn't exist (spring forward) fires
//! at the first instant after the gap; one that occurs twice (fall back)
//! fires once, at the earlier instant. That holds for daily times and for
//! cron patterns with a fixed second, minute and hour. Cron patterns that
//! repeat within the day (`*/15 * * * *`) follow real time instead: they
//! skip what the gap skips and keep firing through a repeated hour, so
//! "every 15 minutes" stays every 15 minutes.
//!
//! Interval and real-time rescan triggers are counted in absolute time from
//! the end of the previous run, so DST doesn't affect them at all.
//!
//! The arithmetic is written against any [`TimeZone`] so it can be tested
//! in fixed zones; the public functions use the machine's local time.

use std::str::FromStr;

use chrono::{DateTime, Datelike, Duration, Local, NaiveDate, NaiveDateTime, NaiveTime, TimeZone};
use croner::Cron;
use pcompare_index::RunTrigger;
use serde::{Deserialize, Serialize};

/// Longest accepted interval (31 days); daily and cron triggers cover longer
/// periods better.
const MAX_INTERVAL_MINUTES: u32 = 31 * 24 * 60;
/// Limits on real-time settle time, maximum delay and the startup delay.
const MAX_DEBOUNCE_SECS: u32 = 3600;
const MAX_DELAY_SECS: u32 = 24 * 3600;
const MAX_STARTUP_DELAY_SECS: u32 = 24 * 3600;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum Trigger {
    /// Every N minutes, counted from the end of the previous run (so a
    /// long run never queues up behind itself). Minimum 1.
    #[serde(rename_all = "camelCase")]
    Interval { every_minutes: u32 },
    /// At fixed wall-clock times ("HH:MM", 24h) on the given weekdays
    /// (0 = Monday … 6 = Sunday; empty = every day).
    #[serde(rename_all = "camelCase")]
    Daily {
        times: Vec<String>,
        weekdays: Vec<u8>,
    },
    /// A cron expression: 5 fields (min hour dom month dow) or 6 with
    /// seconds first; `@hourly`/`@daily`/`@weekly`/`@monthly` too.
    #[serde(rename_all = "camelCase")]
    Cron { expr: String },
    /// Watch the source side(s) for changes and sync once they settle.
    /// Which sides are watched follows the mode: left for left→right modes,
    /// right for right→left, both for two-way and update-both.
    #[serde(rename_all = "camelCase")]
    Realtime {
        /// Wait for this many seconds without changes before syncing.
        debounce_secs: u32,
        /// Sync anyway after this long under constant churn.
        max_delay_secs: u32,
        /// Also do a full rescan this often, to catch events the OS dropped
        /// (and as the only trigger for sides that can't be watched, such
        /// as remote profiles). 0 = never.
        rescan_minutes: u32,
    },
    /// When the app starts (including at login), after a delay.
    #[serde(rename_all = "camelCase")]
    OnStartup { delay_secs: u32 },
}

impl Trigger {
    /// The history label for runs this trigger starts.
    pub fn run_trigger(&self) -> RunTrigger {
        match self {
            Trigger::Interval { .. } => RunTrigger::Interval,
            Trigger::Daily { .. } => RunTrigger::Daily,
            Trigger::Cron { .. } => RunTrigger::Cron,
            Trigger::Realtime { .. } => RunTrigger::Realtime,
            Trigger::OnStartup { .. } => RunTrigger::Startup,
        }
    }

    /// Whether this trigger fires on the clock (and so can be missed).
    pub fn is_scheduled(&self) -> bool {
        matches!(
            self,
            Trigger::Interval { .. } | Trigger::Daily { .. } | Trigger::Cron { .. }
        )
    }
}

/// Checks a trigger's fields (time formats, weekday range, cron syntax,
/// minimums). The message is for the user.
pub fn validate(trigger: &Trigger) -> Result<(), String> {
    match trigger {
        Trigger::Interval { every_minutes } => {
            if *every_minutes < 1 {
                return Err("The interval must be at least 1 minute.".into());
            }
            if *every_minutes > MAX_INTERVAL_MINUTES {
                return Err("The interval can be at most 31 days; use a daily or cron trigger for longer periods.".into());
            }
        }
        Trigger::Daily { times, weekdays } => {
            if times.is_empty() {
                return Err("Add at least one time of day.".into());
            }
            for t in times {
                parse_time(t).ok_or_else(|| {
                    format!(
                        "\"{t}\" isn't a valid time; use 24-hour HH:MM, such as 09:00 or 17:30."
                    )
                })?;
            }
            if let Some(d) = weekdays.iter().find(|&&d| d > 6) {
                return Err(format!(
                    "Weekday {d} is out of range (0 = Monday … 6 = Sunday)."
                ));
            }
        }
        Trigger::Cron { expr } => {
            let cron = parse_cron(expr)?;
            if cron.find_next_occurrence(&Local::now(), false).is_err() {
                return Err(format!(
                    "The cron expression \"{}\" never matches a real date.",
                    expr.trim()
                ));
            }
        }
        Trigger::Realtime {
            debounce_secs,
            max_delay_secs,
            ..
        } => {
            if *debounce_secs < 1 {
                return Err("The settle time must be at least 1 second.".into());
            }
            if *debounce_secs > MAX_DEBOUNCE_SECS {
                return Err("The settle time can be at most 1 hour.".into());
            }
            if max_delay_secs < debounce_secs {
                return Err("The maximum delay can't be shorter than the settle time.".into());
            }
            if *max_delay_secs > MAX_DELAY_SECS {
                return Err("The maximum delay can be at most 24 hours.".into());
            }
        }
        Trigger::OnStartup { delay_secs } => {
            if *delay_secs > MAX_STARTUP_DELAY_SECS {
                return Err("The startup delay can be at most 24 hours.".into());
            }
        }
    }
    Ok(())
}

/// The next time a clock trigger fires strictly after `now`. `last_run` is
/// when the task's previous run ended (the anchor for `Interval`; `None`:
/// fire at `now`). Real-time rescans are clock triggers too: they fire
/// `rescan_minutes` after the last run. `OnStartup` and a real-time trigger
/// with `rescan_minutes == 0` return `None`.
///
/// An anchored trigger whose time has already passed (the app was closed)
/// returns the next point on its grid (`last_run + k × every`) after `now`;
/// [`missed`] says whether one was skipped.
pub fn next_fire(
    trigger: &Trigger,
    last_run: Option<DateTime<Local>>,
    now: DateTime<Local>,
) -> Option<DateTime<Local>> {
    next_fire_in(trigger, last_run, now)
}

/// Whether a scheduled time fell in `(last_run, now]` — a run was missed
/// while the app was closed or the machine slept. Only
/// [`Trigger::is_scheduled`] triggers can be missed.
pub fn missed(trigger: &Trigger, last_run: DateTime<Local>, now: DateTime<Local>) -> bool {
    missed_in(trigger, last_run, now)
}

/// The next `count` fire times across `triggers` after `now`, merged and
/// sorted, for the editor's "Next runs" preview. Clock triggers only.
///
/// Runs are assumed to take no time, so interval triggers count from the
/// previous fire time. Two triggers firing at the same instant are one run
/// (the first trigger's label is shown).
pub fn upcoming(
    triggers: &[Trigger],
    last_run: Option<DateTime<Local>>,
    now: DateTime<Local>,
    count: usize,
) -> Vec<(DateTime<Local>, RunTrigger)> {
    upcoming_in(triggers, last_run, now, count)
}

/// A short human description: "Every 15 minutes", "Weekdays at 09:00,
/// 17:30", "Cron 0 */2 * * *", "Real-time (5 s settle, rescan hourly)".
pub fn describe(trigger: &Trigger) -> String {
    match trigger {
        Trigger::Interval { every_minutes } => describe_every(*every_minutes),
        Trigger::Daily { times, weekdays } => {
            let mut parsed: Vec<(u32, u32)> = times.iter().filter_map(|t| parse_time(t)).collect();
            parsed.sort_unstable();
            parsed.dedup();
            let at = parsed
                .iter()
                .map(|(h, m)| format!("{h:02}:{m:02}"))
                .collect::<Vec<_>>()
                .join(", ");
            format!("{} at {at}", describe_days(weekdays))
        }
        Trigger::Cron { expr } => format!(
            "Cron {}",
            expr.split_whitespace().collect::<Vec<_>>().join(" ")
        ),
        Trigger::Realtime {
            debounce_secs,
            rescan_minutes,
            ..
        } => {
            let rescan = match *rescan_minutes {
                0 => "no rescan".to_owned(),
                60 => "rescan hourly".to_owned(),
                1440 => "rescan daily".to_owned(),
                m => format!("rescan {}", describe_every(m).to_lowercase()),
            };
            format!("Real-time ({debounce_secs} s settle, {rescan})")
        }
        Trigger::OnStartup { delay_secs: 0 } => "At startup".to_owned(),
        Trigger::OnStartup { delay_secs } => {
            format!("At startup, after {}", describe_secs(*delay_secs))
        }
    }
}

fn describe_every(minutes: u32) -> String {
    let (n, unit) = if minutes >= 1440 && minutes.is_multiple_of(1440) {
        (minutes / 1440, "day")
    } else if minutes >= 60 && minutes.is_multiple_of(60) {
        (minutes / 60, "hour")
    } else {
        (minutes, "minute")
    };
    if n == 1 {
        format!("Every {unit}")
    } else {
        format!("Every {n} {unit}s")
    }
}

fn describe_secs(secs: u32) -> String {
    if secs >= 60 && secs.is_multiple_of(60) {
        let m = secs / 60;
        if m == 1 {
            "1 minute".into()
        } else {
            format!("{m} minutes")
        }
    } else {
        format!("{secs} s")
    }
}

fn describe_days(weekdays: &[u8]) -> String {
    const NAMES: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
    let mut days: Vec<u8> = weekdays.iter().copied().filter(|&d| d <= 6).collect();
    days.sort_unstable();
    days.dedup();
    match days.as_slice() {
        [] | [0, 1, 2, 3, 4, 5, 6] => "Daily".into(),
        [0, 1, 2, 3, 4] => "Weekdays".into(),
        [5, 6] => "Weekends".into(),
        _ => days
            .iter()
            .map(|&d| NAMES[d as usize])
            .collect::<Vec<_>>()
            .join(", "),
    }
}

/// "H:MM" or "HH:MM", 24-hour.
fn parse_time(text: &str) -> Option<(u32, u32)> {
    let (h, m) = text.trim().split_once(':')?;
    let digits = |s: &str, len: std::ops::RangeInclusive<usize>| {
        len.contains(&s.len()) && s.bytes().all(|b| b.is_ascii_digit())
    };
    if !digits(h, 1..=2) || !digits(m, 2..=2) {
        return None;
    }
    let (h, m) = (h.parse().ok()?, m.parse().ok()?);
    (h < 24 && m < 60).then_some((h, m))
}

/// Parses a cron expression: 5 fields, 6 with seconds first, or one of the
/// `@` shorthands.
fn parse_cron(expr: &str) -> Result<Cron, String> {
    let expr = expr.trim();
    if expr.is_empty() {
        return Err("Enter a cron expression, such as 0 2 * * * for 02:00 every day.".into());
    }
    if expr.starts_with('@') {
        let known = [
            "@hourly",
            "@daily",
            "@weekly",
            "@monthly",
            "@yearly",
            "@annually",
        ];
        if !known.iter().any(|k| k.eq_ignore_ascii_case(expr)) {
            return Err(format!(
                "\"{expr}\" isn't a known shorthand; use @hourly, @daily, @weekly or @monthly."
            ));
        }
    } else {
        let fields = expr.split_whitespace().count();
        if !(5..=6).contains(&fields) {
            return Err(format!(
                "A cron expression has 5 fields (minute hour day month weekday) or 6 with seconds first; \"{expr}\" has {fields}."
            ));
        }
    }
    Cron::from_str(expr).map_err(|e| format!("The cron expression \"{expr}\" isn't valid: {e}."))
}

fn next_fire_in<Tz: TimeZone>(
    trigger: &Trigger,
    last_run: Option<DateTime<Tz>>,
    now: DateTime<Tz>,
) -> Option<DateTime<Tz>> {
    match trigger {
        Trigger::Interval { every_minutes } => Some(anchored_next(*every_minutes, last_run, now)),
        Trigger::Realtime {
            rescan_minutes: 0, ..
        }
        | Trigger::OnStartup { .. } => None,
        Trigger::Realtime { rescan_minutes, .. } => {
            Some(anchored_next(*rescan_minutes, last_run, now))
        }
        Trigger::Daily { times, weekdays } => daily_next(times, weekdays, &now),
        Trigger::Cron { expr } => parse_cron(expr)
            .ok()?
            .find_next_occurrence(&now, false)
            .ok(),
    }
}

/// `last_run + k × every` for the smallest `k ≥ 1` that lands after `now`;
/// `now` itself without a previous run. A `last_run` in the future (the
/// clock was set back) counts as `now`.
fn anchored_next<Tz: TimeZone>(
    every_minutes: u32,
    last_run: Option<DateTime<Tz>>,
    now: DateTime<Tz>,
) -> DateTime<Tz> {
    let Some(last) = last_run else { return now };
    let anchor = if last > now { now.clone() } else { last };
    let every = Duration::minutes(i64::from(every_minutes.max(1)));
    let elapsed = now.clone() - anchor.clone();
    let k = elapsed.num_milliseconds() / every.num_milliseconds() + 1;
    anchor + every * k as i32
}

fn daily_next<Tz: TimeZone>(
    times: &[String],
    weekdays: &[u8],
    now: &DateTime<Tz>,
) -> Option<DateTime<Tz>> {
    let times: Vec<NaiveTime> = times
        .iter()
        .filter_map(|t| parse_time(t))
        .filter_map(|(h, m)| NaiveTime::from_hms_opt(h, m, 0))
        .collect();
    let today = now.naive_local().date();
    // Eight days covers every weekday; one more absorbs a DST shift.
    (0..=8)
        .filter_map(|d| today.checked_add_days(chrono::Days::new(d)))
        .filter(|date| weekday_selected(weekdays, *date))
        .flat_map(|date| times.iter().map(move |t| date.and_time(*t)))
        .filter_map(|naive| resolve(&now.timezone(), naive))
        .filter(|t| t > now)
        .min()
}

fn weekday_selected(weekdays: &[u8], date: NaiveDate) -> bool {
    weekdays.is_empty() || weekdays.contains(&(date.weekday().num_days_from_monday() as u8))
}

/// A wall-clock time as an instant, per the DST rules in the module docs.
fn resolve<Tz: TimeZone>(tz: &Tz, naive: NaiveDateTime) -> Option<DateTime<Tz>> {
    if let Some(t) = tz.from_local_datetime(&naive).earliest() {
        return Some(t);
    }
    // In a gap: the first wall-clock minute that exists again is the instant
    // the clocks jumped. Gaps are at most a few hours.
    (1..=24 * 60).find_map(|m| {
        tz.from_local_datetime(&(naive + Duration::minutes(m)))
            .earliest()
    })
}

fn missed_in<Tz: TimeZone>(trigger: &Trigger, last_run: DateTime<Tz>, now: DateTime<Tz>) -> bool {
    trigger.is_scheduled()
        && last_run < now
        && next_fire_in(trigger, Some(last_run.clone()), last_run).is_some_and(|t| t <= now)
}

fn upcoming_in<Tz: TimeZone>(
    triggers: &[Trigger],
    last_run: Option<DateTime<Tz>>,
    now: DateTime<Tz>,
    count: usize,
) -> Vec<(DateTime<Tz>, RunTrigger)> {
    let mut all: Vec<(DateTime<Tz>, usize, RunTrigger)> = Vec::new();
    for (i, trigger) in triggers.iter().enumerate() {
        let label = match trigger {
            Trigger::Realtime { .. } => RunTrigger::Rescan,
            t => t.run_trigger(),
        };
        let (mut last, mut at) = (last_run.clone(), now.clone());
        for _ in 0..count {
            let Some(next) = next_fire_in(trigger, last.clone(), at.clone()) else {
                break;
            };
            all.push((next.clone(), i, label));
            // An anchored trigger counts from this (instant) run.
            last = Some(next.clone());
            at = next;
        }
    }
    all.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    all.dedup_by(|b, a| a.0 == b.0);
    all.into_iter()
        .take(count)
        .map(|(t, _, label)| (t, label))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{FixedOffset, Utc};
    use chrono_tz::America::New_York;
    use chrono_tz::Tz;

    fn ny(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Tz> {
        New_York
            .with_ymd_and_hms(y, mo, d, h, mi, 0)
            .earliest()
            .unwrap()
    }

    fn utc(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn fixed(s: &str) -> DateTime<FixedOffset> {
        DateTime::parse_from_rfc3339(s).unwrap()
    }

    fn daily(times: &[&str], weekdays: &[u8]) -> Trigger {
        Trigger::Daily {
            times: times.iter().map(|t| t.to_string()).collect(),
            weekdays: weekdays.to_vec(),
        }
    }

    fn cron(expr: &str) -> Trigger {
        Trigger::Cron { expr: expr.into() }
    }

    #[test]
    fn times_parse_strictly() {
        assert_eq!(parse_time("9:05"), Some((9, 5)));
        assert_eq!(parse_time("09:05"), Some((9, 5)));
        assert_eq!(parse_time(" 23:59 "), Some((23, 59)));
        for bad in [
            "24:00", "12:60", "9:5", "123:00", "", "9", "ab:cd", "-1:00", "+9:00", "09:05:00",
        ] {
            assert_eq!(parse_time(bad), None, "{bad}");
        }
    }

    #[test]
    fn validation_messages() {
        assert!(validate(&Trigger::Interval { every_minutes: 1 }).is_ok());
        assert!(validate(&Trigger::Interval { every_minutes: 0 })
            .unwrap_err()
            .contains("at least 1 minute"));
        assert!(validate(&Trigger::Interval {
            every_minutes: MAX_INTERVAL_MINUTES + 1
        })
        .is_err());
        assert!(validate(&daily(&["9:00", "17:30"], &[0, 6])).is_ok());
        assert!(validate(&daily(&[], &[])).is_err());
        assert!(validate(&daily(&["25:00"], &[]))
            .unwrap_err()
            .contains("\"25:00\""));
        assert!(validate(&daily(&["09:00"], &[7]))
            .unwrap_err()
            .contains("Weekday 7"));
        for ok in [
            "0 2 * * *",
            "*/15 * * * *",
            "30 0 2 * * MON-FRI",
            "@hourly",
            "@Daily",
            "@weekly",
            "@monthly",
        ] {
            assert!(validate(&cron(ok)).is_ok(), "{ok}");
        }
        assert!(validate(&cron("")).is_err());
        assert!(validate(&cron("* * * *")).unwrap_err().contains("has 4"));
        assert!(validate(&cron("0 0 1 1 * 2030")).is_err(), "7 fields");
        assert!(validate(&cron("@reboot"))
            .unwrap_err()
            .contains("shorthand"));
        assert!(validate(&cron("61 * * * *"))
            .unwrap_err()
            .contains("isn't valid"));
        assert!(validate(&cron("0 0 30 2 *"))
            .unwrap_err()
            .contains("never matches"));
        let rt = |d, m| Trigger::Realtime {
            debounce_secs: d,
            max_delay_secs: m,
            rescan_minutes: 60,
        };
        assert!(validate(&rt(1, 1)).is_ok());
        assert!(validate(&rt(0, 10)).is_err());
        assert!(validate(&rt(10, 5)).unwrap_err().contains("maximum delay"));
        assert!(validate(&Trigger::OnStartup { delay_secs: 0 }).is_ok());
    }

    #[test]
    fn interval_counts_from_the_last_run() {
        let every = Trigger::Interval { every_minutes: 15 };
        let now = utc("2026-05-01T10:00:00Z");
        assert_eq!(next_fire_in(&every, None, now), Some(now), "never ran: now");
        let last = utc("2026-05-01T09:50:00Z");
        assert_eq!(
            next_fire_in(&every, Some(last), now),
            Some(utc("2026-05-01T10:05:00Z"))
        );
        // Overdue: the next grid point, and `missed` reports the skipped one.
        let last = utc("2026-05-01T08:00:00Z");
        assert_eq!(
            next_fire_in(&every, Some(last), now),
            Some(utc("2026-05-01T10:15:00Z"))
        );
        assert!(missed_in(&every, last, now));
        assert!(!missed_in(&every, utc("2026-05-01T09:46:00Z"), now));
        assert!(
            missed_in(&every, utc("2026-05-01T09:45:00Z"), now),
            "inclusive of now"
        );
        // The clock went back: count from now, not from the future.
        let future = utc("2026-05-01T12:00:00Z");
        assert_eq!(
            next_fire_in(&every, Some(future), now),
            Some(utc("2026-05-01T10:15:00Z"))
        );
    }

    #[test]
    fn realtime_rescans_and_startup_are_not_scheduled_fires() {
        let rt = Trigger::Realtime {
            debounce_secs: 5,
            max_delay_secs: 60,
            rescan_minutes: 60,
        };
        let now = utc("2026-05-01T10:00:00Z");
        assert_eq!(
            next_fire_in(&rt, Some(utc("2026-05-01T09:30:00Z")), now),
            Some(utc("2026-05-01T10:30:00Z"))
        );
        assert!(
            !missed_in(&rt, utc("2026-05-01T07:00:00Z"), now),
            "rescans aren't caught up"
        );
        let none = Trigger::Realtime {
            debounce_secs: 5,
            max_delay_secs: 60,
            rescan_minutes: 0,
        };
        assert_eq!(next_fire_in(&none, None, now), None);
        assert_eq!(
            next_fire_in(&Trigger::OnStartup { delay_secs: 5 }, None, now),
            None
        );
    }

    #[test]
    fn daily_times_and_weekdays() {
        // 2026-05-01 is a Friday.
        let now = fixed("2026-05-01T10:00:00+02:00");
        let t = daily(&["17:30", "9:00"], &[]);
        assert_eq!(
            next_fire_in(&t, None, now),
            Some(fixed("2026-05-01T17:30:00+02:00"))
        );
        let at_time = fixed("2026-05-01T17:30:00+02:00");
        assert_eq!(
            next_fire_in(&t, None, at_time),
            Some(fixed("2026-05-02T09:00:00+02:00")),
            "strictly after"
        );
        // Weekdays only (0 = Monday): Friday evening → Monday morning.
        let weekdays = daily(&["09:00"], &[0, 1, 2, 3, 4]);
        assert_eq!(
            next_fire_in(&weekdays, None, fixed("2026-05-01T18:00:00+02:00")),
            Some(fixed("2026-05-04T09:00:00+02:00"))
        );
        // Sunday only, from a Sunday after the time: a week later.
        let sunday = daily(&["08:00"], &[6]);
        assert_eq!(
            next_fire_in(&sunday, None, fixed("2026-05-03T09:00:00+02:00")),
            Some(fixed("2026-05-10T08:00:00+02:00"))
        );
        assert!(
            missed_in(&t, fixed("2026-04-30T18:00:00+02:00"), now),
            "09:00 today was missed"
        );
        assert!(
            !missed_in(&t, fixed("2026-05-01T09:00:00+02:00"), now),
            "09:00 already ran"
        );
    }

    #[test]
    fn daily_time_in_spring_forward_gap_fires_right_after_it() {
        // New York, 2026-03-08: 02:00 EST jumps to 03:00 EDT.
        let t = daily(&["02:30"], &[]);
        let next = next_fire_in(&t, None, ny(2026, 3, 8, 0, 0)).unwrap();
        assert_eq!(
            next.with_timezone(&Utc),
            utc("2026-03-08T07:00:00Z"),
            "03:00 EDT"
        );
        // And the day after it's back to 02:30.
        let after = next_fire_in(&t, None, next).unwrap();
        assert_eq!(after, ny(2026, 3, 9, 2, 30));
        // A time not in the gap is unaffected.
        let t = daily(&["03:30"], &[]);
        assert_eq!(
            next_fire_in(&t, None, ny(2026, 3, 8, 0, 0)).unwrap(),
            ny(2026, 3, 8, 3, 30)
        );
    }

    #[test]
    fn daily_time_in_fall_back_overlap_fires_once_at_the_earlier_instant() {
        // New York, 2026-11-01: 02:00 EDT falls back to 01:00 EST; 01:30 happens twice.
        let t = daily(&["01:30"], &[]);
        let first = next_fire_in(&t, None, ny(2026, 11, 1, 0, 0)).unwrap();
        assert_eq!(
            first.with_timezone(&Utc),
            utc("2026-11-01T05:30:00Z"),
            "01:30 EDT"
        );
        // From just after it (and from the repeated hour), the next is tomorrow.
        let tomorrow = ny(2026, 11, 2, 1, 30);
        assert_eq!(
            next_fire_in(&t, None, first + Duration::minutes(1)).unwrap(),
            tomorrow
        );
        let in_repeat = New_York.from_utc_datetime(&utc("2026-11-01T06:10:00Z").naive_utc()); // 01:10 EST
        assert_eq!(next_fire_in(&t, None, in_repeat).unwrap(), tomorrow);
        assert!(
            !missed_in(&t, first, in_repeat + Duration::minutes(30)),
            "not missed a second time"
        );
    }

    #[test]
    fn fixed_time_cron_follows_the_same_dst_rules() {
        let gap = cron("30 2 * * *");
        let next = next_fire_in(&gap, None, ny(2026, 3, 8, 0, 0)).unwrap();
        assert_eq!(next.with_timezone(&Utc), utc("2026-03-08T07:00:00Z"));
        let overlap = cron("30 1 * * *");
        let first = next_fire_in(&overlap, None, ny(2026, 11, 1, 0, 0)).unwrap();
        assert_eq!(first.with_timezone(&Utc), utc("2026-11-01T05:30:00Z"));
        assert_eq!(
            next_fire_in(&overlap, None, first).unwrap(),
            ny(2026, 11, 2, 1, 30)
        );
    }

    #[test]
    fn repeating_cron_keeps_real_time_through_a_repeated_hour() {
        let every_half_hour = cron("*/30 * * * *");
        let mut at = ny(2026, 11, 1, 0, 50);
        let mut fires = Vec::new();
        for _ in 0..6 {
            at = next_fire_in(&every_half_hour, None, at).unwrap();
            fires.push(at.with_timezone(&Utc).format("%H:%M").to_string());
        }
        assert_eq!(
            fires,
            ["05:00", "05:30", "06:00", "06:30", "07:00", "07:30"]
        );
    }

    #[test]
    fn cron_shorthands_and_seconds() {
        let now = utc("2026-05-01T10:20:00Z");
        assert_eq!(
            next_fire_in(&cron("@hourly"), None, now),
            Some(utc("2026-05-01T11:00:00Z"))
        );
        assert_eq!(
            next_fire_in(&cron("@daily"), None, now),
            Some(utc("2026-05-02T00:00:00Z"))
        );
        assert_eq!(
            next_fire_in(&cron("15 0 2 * * *"), None, now),
            Some(utc("2026-05-02T02:00:15Z"))
        );
        assert_eq!(next_fire_in(&cron("not cron"), None, now), None);
        assert!(missed_in(
            &cron("0 2 * * *"),
            utc("2026-04-30T03:00:00Z"),
            now
        ));
        assert!(!missed_in(
            &cron("0 2 * * *"),
            utc("2026-05-01T03:00:00Z"),
            now
        ));
    }

    #[test]
    fn upcoming_merges_triggers() {
        let now = utc("2026-05-01T08:50:00Z");
        let triggers = [
            Trigger::Interval { every_minutes: 30 },
            daily(&["09:00", "10:00"], &[]),
            Trigger::OnStartup { delay_secs: 0 },
            Trigger::Realtime {
                debounce_secs: 1,
                max_delay_secs: 1,
                rescan_minutes: 0,
            },
        ];
        let last = Some(utc("2026-05-01T08:40:00Z"));
        let next: Vec<(String, RunTrigger)> = upcoming_in(&triggers, last, now, 5)
            .into_iter()
            .map(|(t, k)| (t.format("%H:%M").to_string(), k))
            .collect();
        assert_eq!(
            next,
            [
                ("09:00".into(), RunTrigger::Daily),
                ("09:10".into(), RunTrigger::Interval),
                ("09:40".into(), RunTrigger::Interval),
                ("10:00".into(), RunTrigger::Daily),
                ("10:10".into(), RunTrigger::Interval),
            ]
        );
        // Two triggers at the same instant are one run; a never-run interval starts now.
        let same = [
            Trigger::Interval { every_minutes: 60 },
            daily(&["09:50"], &[]),
        ];
        let next = upcoming_in(&same, None, now, 3);
        assert_eq!(next.len(), 3);
        assert_eq!(next[0], (now, RunTrigger::Interval));
        assert_eq!(next[1], (utc("2026-05-01T09:50:00Z"), RunTrigger::Interval));
        let rescans = [Trigger::Realtime {
            debounce_secs: 1,
            max_delay_secs: 1,
            rescan_minutes: 60,
        }];
        assert_eq!(upcoming_in(&rescans, last, now, 1)[0].1, RunTrigger::Rescan);
        assert!(upcoming_in(&[], last, now, 3).is_empty());
    }

    #[test]
    fn local_time_wrappers_agree() {
        let now = Local::now();
        let t = Trigger::Interval { every_minutes: 5 };
        assert_eq!(
            next_fire(&t, Some(now), now),
            Some(now + Duration::minutes(5))
        );
        assert!(missed(&t, now - Duration::minutes(6), now));
        assert_eq!(upcoming(&[t], Some(now), now, 2).len(), 2);
    }

    #[test]
    fn descriptions() {
        assert_eq!(
            describe(&Trigger::Interval { every_minutes: 1 }),
            "Every minute"
        );
        assert_eq!(
            describe(&Trigger::Interval { every_minutes: 15 }),
            "Every 15 minutes"
        );
        assert_eq!(
            describe(&Trigger::Interval { every_minutes: 120 }),
            "Every 2 hours"
        );
        assert_eq!(
            describe(&Trigger::Interval {
                every_minutes: 1440
            }),
            "Every day"
        );
        assert_eq!(
            describe(&daily(&["17:30", "9:00"], &[0, 1, 2, 3, 4])),
            "Weekdays at 09:00, 17:30"
        );
        assert_eq!(describe(&daily(&["09:00"], &[])), "Daily at 09:00");
        assert_eq!(describe(&daily(&["09:00"], &[6, 5])), "Weekends at 09:00");
        assert_eq!(
            describe(&daily(&["09:00"], &[0, 2, 4])),
            "Mon, Wed, Fri at 09:00"
        );
        assert_eq!(describe(&cron(" 0  */2 * * * ")), "Cron 0 */2 * * *");
        let rt = |rescan| Trigger::Realtime {
            debounce_secs: 5,
            max_delay_secs: 60,
            rescan_minutes: rescan,
        };
        assert_eq!(describe(&rt(60)), "Real-time (5 s settle, rescan hourly)");
        assert_eq!(describe(&rt(0)), "Real-time (5 s settle, no rescan)");
        assert_eq!(
            describe(&rt(30)),
            "Real-time (5 s settle, rescan every 30 minutes)"
        );
        assert_eq!(
            describe(&Trigger::OnStartup { delay_secs: 0 }),
            "At startup"
        );
        assert_eq!(
            describe(&Trigger::OnStartup { delay_secs: 120 }),
            "At startup, after 2 minutes"
        );
        assert_eq!(
            describe(&Trigger::OnStartup { delay_secs: 30 }),
            "At startup, after 30 s"
        );
    }

    #[test]
    fn serde_shape_matches_the_ts_mirror() {
        let t: Trigger = serde_json::from_str(
            r#"{"kind":"realtime","debounceSecs":5,"maxDelaySecs":60,"rescanMinutes":0}"#,
        )
        .unwrap();
        assert_eq!(
            t,
            Trigger::Realtime {
                debounce_secs: 5,
                max_delay_secs: 60,
                rescan_minutes: 0
            }
        );
        let json = serde_json::to_value(Trigger::OnStartup { delay_secs: 3 }).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"kind": "onStartup", "delaySecs": 3})
        );
        let json = serde_json::to_value(Trigger::Interval { every_minutes: 3 }).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"kind": "interval", "everyMinutes": 3})
        );
    }
}
