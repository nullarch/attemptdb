//! Timestamps are microseconds since the Unix epoch, UTC, stored as `i64`.
//! Never persisted as a platform `SystemTime`.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default)]
#[serde(transparent)]
pub struct Timestamp(pub i64);

impl Timestamp {
    pub const fn from_micros(us: i64) -> Self {
        Self(us)
    }

    pub const fn from_millis(ms: i64) -> Self {
        Self(ms * 1_000)
    }

    pub fn now() -> Self {
        let d = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        Self(d.as_micros() as i64)
    }

    pub const fn as_micros(self) -> i64 {
        self.0
    }

    pub const fn as_millis(self) -> i64 {
        self.0 / 1_000
    }

    /// RFC 3339 rendering with microsecond precision, always UTC (`Z`).
    pub fn to_rfc3339(self) -> String {
        chrono::DateTime::<chrono::Utc>::from_timestamp_micros(self.0)
            .map(|d| d.to_rfc3339_opts(chrono::SecondsFormat::Micros, true))
            .unwrap_or_else(|| format!("invalid({})", self.0))
    }

    /// Parse RFC 3339 / ISO 8601 input. Accepts a bare `YYYY-MM-DD`, a naive
    /// datetime (interpreted as UTC), or a full offset datetime.
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim();
        if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
            return Some(Self(dt.timestamp_micros()));
        }
        if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f") {
            return Some(Self(naive.and_utc().timestamp_micros()));
        }
        if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S%.f") {
            return Some(Self(naive.and_utc().timestamp_micros()));
        }
        if let Ok(date) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
            return Some(Self(
                date.and_hms_opt(0, 0, 0)?.and_utc().timestamp_micros(),
            ));
        }
        // Unix epoch seconds or milliseconds as a convenience.
        if let Ok(n) = s.parse::<i64>() {
            if n > 10_000_000_000 {
                return Some(Self::from_millis(n));
            }
            return Some(Self(n * 1_000_000));
        }
        None
    }
}

impl fmt::Debug for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_rfc3339())
    }
}

impl fmt::Display for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_rfc3339())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_rfc3339() {
        let t = Timestamp::from_micros(1_756_368_000_123_456);
        let s = t.to_rfc3339();
        assert_eq!(Timestamp::parse(&s), Some(t));
    }

    #[test]
    fn parses_date_only() {
        let expected = chrono::NaiveDate::from_ymd_opt(2026, 8, 20)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc()
            .timestamp_micros();
        assert_eq!(
            Timestamp::parse("2026-08-20"),
            Some(Timestamp::from_micros(expected))
        );
    }
}
