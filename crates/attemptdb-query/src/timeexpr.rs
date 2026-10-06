//! Timestamp expressions: absolute (RFC 3339, date, epoch), `now`, relative
//! (`-15m`, `-2h`, `-1d`, `-1w`), `today` and `yesterday`.
//!
//! `today` and `yesterday` are midnights in the machine's local time zone
//! (the day a person means), not UTC midnight. A bare `YYYY-MM-DD` is UTC
//! midnight, as in every other timestamp the database prints; write an RFC
//! 3339 timestamp with an offset (`2026-10-06T00:00:00+09:00`) for another
//! zone.

use attemptdb_core::Timestamp;
use chrono::{DateTime, Duration, Local, LocalResult, TimeZone, Utc};

const MICROS_PER_SECOND: i64 = 1_000_000;
const MICROS_PER_DAY: i64 = 86_400 * MICROS_PER_SECOND;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TimeExpr {
    Absolute(Timestamp),
    Now,
    /// `amount` units before now; `unit` is one of `s m h d w`.
    Relative {
        amount: i64,
        unit: char,
    },
    Today,
    Yesterday,
}

impl TimeExpr {
    /// Parse the text of a timestamp literal (quotes already removed).
    pub fn parse_literal(s: &str) -> Option<TimeExpr> {
        let t = s.trim();
        let lower = t.to_ascii_lowercase();
        match lower.as_str() {
            "now" => return Some(TimeExpr::Now),
            "today" => return Some(TimeExpr::Today),
            "yesterday" => return Some(TimeExpr::Yesterday),
            _ => {}
        }
        if let Some(rel) = parse_relative(&lower) {
            return Some(rel);
        }
        Timestamp::parse(t).map(TimeExpr::Absolute)
    }

    /// Resolve to an absolute timestamp given the current time; `today` and
    /// `yesterday` are midnights in the machine's local time zone.
    pub fn resolve(&self, now: Timestamp) -> Timestamp {
        self.resolve_in(now, &Local)
    }

    /// [`Self::resolve`] with the day boundary taken in `tz`.
    pub fn resolve_in<Tz: TimeZone>(&self, now: Timestamp, tz: &Tz) -> Timestamp {
        match self {
            TimeExpr::Absolute(t) => *t,
            TimeExpr::Now => now,
            TimeExpr::Relative { amount, unit } => {
                let delta = amount.saturating_mul(unit_micros(*unit));
                Timestamp::from_micros(now.as_micros().saturating_sub(delta))
            }
            TimeExpr::Today => start_of_day(now, tz),
            TimeExpr::Yesterday => {
                // The midnight before today's, in `tz` (a day is not always
                // 24 hours there): the start of the day that contains the
                // instant just before today began.
                let today = start_of_day(now, tz);
                start_of_day(Timestamp::from_micros(today.as_micros() - 1), tz)
            }
        }
    }

    /// Human-readable form for notes and explanations.
    pub fn describe(&self) -> String {
        match self {
            TimeExpr::Absolute(t) => t.to_rfc3339(),
            TimeExpr::Now => "now".to_string(),
            TimeExpr::Relative { amount, unit } => format!("-{amount}{unit}"),
            TimeExpr::Today => "today".to_string(),
            TimeExpr::Yesterday => "yesterday".to_string(),
        }
    }
}

/// `-15m`, `-2h`, `-1d`, `-1w`, `-30s`.
pub fn parse_relative(s: &str) -> Option<TimeExpr> {
    let body = s.strip_prefix('-')?;
    let unit = body.chars().last()?;
    if !matches!(unit, 's' | 'm' | 'h' | 'd' | 'w') {
        return None;
    }
    let digits = &body[..body.len() - 1];
    if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let amount: i64 = digits.parse().ok()?;
    Some(TimeExpr::Relative { amount, unit })
}

fn unit_micros(unit: char) -> i64 {
    match unit {
        's' => MICROS_PER_SECOND,
        'm' => 60 * MICROS_PER_SECOND,
        'h' => 3_600 * MICROS_PER_SECOND,
        'd' => MICROS_PER_DAY,
        'w' => 7 * MICROS_PER_DAY,
        _ => 0,
    }
}

/// The first instant of `t`'s calendar day in `tz`.
fn start_of_day<Tz: TimeZone>(t: Timestamp, tz: &Tz) -> Timestamp {
    let Some(utc) = DateTime::<Utc>::from_timestamp_micros(t.as_micros()) else {
        return t;
    };
    let Some(midnight) = utc.with_timezone(tz).date_naive().and_hms_opt(0, 0, 0) else {
        return t;
    };
    let local = |naive| match tz.from_local_datetime(&naive) {
        LocalResult::Single(dt) => Some(dt),
        // A clock set back across midnight: the first of the two.
        LocalResult::Ambiguous(first, _) => Some(first),
        LocalResult::None => None,
    };
    // A clock set forward across midnight has no 00:00; the day starts when
    // the clock resumes.
    local(midnight)
        .or_else(|| local(midnight + Duration::hours(1)))
        .map(|dt| Timestamp::from_micros(dt.timestamp_micros()))
        .unwrap_or(t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_all_forms() {
        assert_eq!(TimeExpr::parse_literal("now"), Some(TimeExpr::Now));
        assert_eq!(TimeExpr::parse_literal("NOW"), Some(TimeExpr::Now));
        assert_eq!(
            TimeExpr::parse_literal("-15m"),
            Some(TimeExpr::Relative {
                amount: 15,
                unit: 'm'
            })
        );
        assert_eq!(
            TimeExpr::parse_literal("yesterday"),
            Some(TimeExpr::Yesterday)
        );
        assert!(matches!(
            TimeExpr::parse_literal("2026-08-28"),
            Some(TimeExpr::Absolute(_))
        ));
        assert!(matches!(
            TimeExpr::parse_literal("2026-08-28T08:00:00Z"),
            Some(TimeExpr::Absolute(_))
        ));
        assert_eq!(TimeExpr::parse_literal("-15x"), None);
        assert_eq!(TimeExpr::parse_literal("soon"), None);
    }

    #[test]
    fn relative_resolution() {
        let now = Timestamp::from_micros(1_787_904_000_000_000);
        let t = TimeExpr::Relative {
            amount: 2,
            unit: 'h',
        }
        .resolve(now);
        assert_eq!(now.as_micros() - t.as_micros(), 7_200 * MICROS_PER_SECOND);
        let today = TimeExpr::Today.resolve_in(now, &Utc);
        assert_eq!(today.to_rfc3339(), "2026-08-28T00:00:00.000000Z");
        let yesterday = TimeExpr::Yesterday.resolve_in(now, &Utc);
        assert_eq!(yesterday.to_rfc3339(), "2026-08-27T00:00:00.000000Z");
    }

    #[test]
    fn today_and_yesterday_are_local_midnights() {
        use chrono::FixedOffset;
        let kst = FixedOffset::east_opt(9 * 3600).unwrap();
        // 08:00Z is 17:00 on the 28th in Seoul.
        let now = Timestamp::from_micros(1_787_904_000_000_000);
        assert_eq!(
            TimeExpr::Today.resolve_in(now, &kst).to_rfc3339(),
            "2026-08-27T15:00:00.000000Z"
        );
        assert_eq!(
            TimeExpr::Yesterday.resolve_in(now, &kst).to_rfc3339(),
            "2026-08-26T15:00:00.000000Z"
        );
        // 20:00Z on the 28th is already the 29th in Seoul: the UTC date and
        // the local date differ, and `today` follows the local one.
        let late = Timestamp::from_micros(now.as_micros() + 12 * 3_600 * MICROS_PER_SECOND);
        assert_eq!(
            TimeExpr::Today.resolve_in(late, &kst).to_rfc3339(),
            "2026-08-28T15:00:00.000000Z"
        );
        // West of UTC, early UTC hours still belong to the previous day.
        let pdt = FixedOffset::west_opt(7 * 3600).unwrap();
        let early = Timestamp::from_micros(now.as_micros() - 6 * 3_600 * MICROS_PER_SECOND);
        assert_eq!(
            TimeExpr::Today.resolve_in(early, &pdt).to_rfc3339(),
            "2026-08-27T07:00:00.000000Z"
        );
        // `resolve` uses the machine's zone: whatever it is, `today` is
        // within a day before now and never after it.
        let today = TimeExpr::Today.resolve(now);
        assert!(
            today <= now && now.as_micros() - today.as_micros() < 25 * 3_600 * MICROS_PER_SECOND
        );
    }
}
