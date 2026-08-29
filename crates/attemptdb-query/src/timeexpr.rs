//! Timestamp expressions: absolute (RFC 3339, date, epoch), `now`, relative
//! (`-15m`, `-2h`, `-1d`, `-1w`), `today` and `yesterday` (UTC midnight).

use attemptdb_core::Timestamp;
use chrono::{DateTime, Utc};

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

    /// Resolve to an absolute timestamp given the current time.
    pub fn resolve(&self, now: Timestamp) -> Timestamp {
        match self {
            TimeExpr::Absolute(t) => *t,
            TimeExpr::Now => now,
            TimeExpr::Relative { amount, unit } => {
                let delta = amount.saturating_mul(unit_micros(*unit));
                Timestamp::from_micros(now.as_micros().saturating_sub(delta))
            }
            TimeExpr::Today => start_of_day(now),
            TimeExpr::Yesterday => {
                Timestamp::from_micros(start_of_day(now).as_micros() - MICROS_PER_DAY)
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

fn start_of_day(t: Timestamp) -> Timestamp {
    DateTime::<Utc>::from_timestamp_micros(t.as_micros())
        .and_then(|dt| dt.date_naive().and_hms_opt(0, 0, 0))
        .map(|naive| Timestamp::from_micros(naive.and_utc().timestamp_micros()))
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
        let today = TimeExpr::Today.resolve(now);
        assert_eq!(today.to_rfc3339(), "2026-08-28T00:00:00.000000Z");
        let yesterday = TimeExpr::Yesterday.resolve(now);
        assert_eq!(yesterday.to_rfc3339(), "2026-08-27T00:00:00.000000Z");
    }
}
