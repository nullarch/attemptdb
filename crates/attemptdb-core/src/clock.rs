//! Hybrid logical clock (HLC).
//!
//! An HLC timestamp is `(wall_ms, logical)` packed into a single `u64`:
//! the upper 48 bits carry milliseconds since the Unix epoch, the lower
//! 16 bits carry a logical counter. Timestamps are totally ordered, never go
//! backwards on a single writer even when the wall clock does, and stay close
//! to physical time so they remain human-interpretable.
//!
//! One `HlcGenerator` exists per database writer. Values are assigned at
//! ingestion time (not at capture time), because capture happens in many
//! short-lived hook processes that share no state.

use crate::time::Timestamp;
use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default)]
#[serde(transparent)]
pub struct Hlc(pub u64);

const LOGICAL_BITS: u32 = 16;
const LOGICAL_MASK: u64 = (1 << LOGICAL_BITS) - 1;

impl Hlc {
    pub const fn new(wall_ms: u64, logical: u16) -> Self {
        Self((wall_ms << LOGICAL_BITS) | logical as u64)
    }

    pub const fn wall_ms(self) -> u64 {
        self.0 >> LOGICAL_BITS
    }

    pub const fn logical(self) -> u16 {
        (self.0 & LOGICAL_MASK) as u16
    }

    pub const fn as_u64(self) -> u64 {
        self.0
    }

    pub fn to_timestamp(self) -> Timestamp {
        Timestamp::from_millis(self.wall_ms() as i64)
    }
}

impl fmt::Debug for Hlc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Hlc({}:{})", self.wall_ms(), self.logical())
    }
}

impl fmt::Display for Hlc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{:05}", self.wall_ms(), self.logical())
    }
}

/// Monotonic HLC generator for a single writer.
#[derive(Debug)]
pub struct HlcGenerator {
    last: Hlc,
}

impl HlcGenerator {
    /// Start from a previously persisted value so a restarted writer never
    /// re-issues an earlier timestamp.
    pub fn resume_from(last: Hlc) -> Self {
        Self { last }
    }

    pub fn new() -> Self {
        Self { last: Hlc(0) }
    }

    /// Issue the next timestamp given the current wall clock.
    pub fn next(&mut self, now: Timestamp) -> Hlc {
        let now_ms = now.as_millis().max(0) as u64;
        let next = if now_ms > self.last.wall_ms() {
            Hlc::new(now_ms, 0)
        } else if self.last.logical() < u16::MAX {
            Hlc::new(self.last.wall_ms(), self.last.logical() + 1)
        } else {
            Hlc::new(self.last.wall_ms() + 1, 0)
        };
        self.last = next;
        next
    }

    /// Merge a timestamp received from another writer (Lamport receive rule).
    pub fn observe(&mut self, remote: Hlc, now: Timestamp) -> Hlc {
        let now_ms = now.as_millis().max(0) as u64;
        let wall = now_ms.max(self.last.wall_ms()).max(remote.wall_ms());
        let logical = if wall == self.last.wall_ms() && wall == remote.wall_ms() {
            self.last.logical().max(remote.logical()).saturating_add(1)
        } else if wall == self.last.wall_ms() {
            self.last.logical().saturating_add(1)
        } else if wall == remote.wall_ms() {
            remote.logical().saturating_add(1)
        } else {
            0
        };
        self.last = Hlc::new(wall, logical);
        self.last
    }

    pub fn last(&self) -> Hlc {
        self.last
    }
}

impl Default for HlcGenerator {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monotonic_under_clock_regression() {
        let mut g = HlcGenerator::new();
        let a = g.next(Timestamp::from_millis(1000));
        let b = g.next(Timestamp::from_millis(900)); // clock went backwards
        let c = g.next(Timestamp::from_millis(900));
        let d = g.next(Timestamp::from_millis(1001));
        assert!(a < b && b < c && c < d);
        assert_eq!(b.wall_ms(), 1000);
        assert_eq!(b.logical(), 1);
        assert_eq!(d.wall_ms(), 1001);
        assert_eq!(d.logical(), 0);
    }

    #[test]
    fn logical_overflow_advances_wall() {
        let mut g = HlcGenerator::resume_from(Hlc::new(5, u16::MAX));
        let n = g.next(Timestamp::from_millis(5));
        assert_eq!(n, Hlc::new(6, 0));
    }
}
