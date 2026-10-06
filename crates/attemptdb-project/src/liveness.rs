//! Whether a session is still going, judged against an explicit instant.
//!
//! The projection stays a pure function of `(events, as_of)`: nothing here
//! reads the wall clock. A caller that wants "now" passes it
//! ([`crate::Projector::finish_at`], [`crate::IncrementalProjector::snapshot_at`],
//! [`crate::project_at`]); a caller that passes nothing gets the latest
//! timestamp in the stream, which is what makes tests and replays
//! reproducible and what makes a stream's most recent session never look
//! idle.
//!
//! Rules (RFC 0003 §5.1):
//!
//! - a session whose end was observed (and not resumed afterwards) is
//!   `closed`;
//! - otherwise it is `open` while its last activity is no older than
//!   [`STALE_AFTER_US`] before the instant, `stale` after that;
//! - a session that is waiting on a human (an uncleared blocking signal)
//!   stays `open` for the longer [`PENDING_STALE_AFTER_US`]: nothing happens
//!   in a session that is waiting for an approval, so its silence is the
//!   expected state, not evidence that it died.

use crate::model::{Session, SessionStatus};
use attemptdb_core::Timestamp;

/// Silence after which a session with no end event counts as `stale`
/// (RFC 0003 §5.1: thirty minutes).
pub const STALE_AFTER_US: i64 = 30 * 60 * 1_000_000;

/// Silence after which a session that is waiting on a human counts as
/// `stale` (twelve hours: a night away from the keyboard is plausible, a
/// week is not). A documented choice: the RFC does not give a number.
pub const PENDING_STALE_AFTER_US: i64 = 12 * 60 * 60 * 1_000_000;

/// The state of a session at `as_of`. `awaiting_human` is whether an
/// uncleared blocking signal is pending in it.
pub(crate) fn judge(s: &Session, awaiting_human: bool, as_of: Timestamp) -> SessionStatus {
    if s.ended_at.is_some() {
        return SessionStatus::Closed;
    }
    if is_stale(s.last_activity_at, awaiting_human, as_of) {
        SessionStatus::Stale
    } else {
        SessionStatus::Open
    }
}

/// Whether silence since `last_activity` exceeds the staleness threshold at
/// `as_of`.
pub(crate) fn is_stale(last_activity: Timestamp, awaiting_human: bool, as_of: Timestamp) -> bool {
    let threshold = if awaiting_human {
        PENDING_STALE_AFTER_US
    } else {
        STALE_AFTER_US
    };
    as_of.as_micros() - last_activity.as_micros() > threshold
}
