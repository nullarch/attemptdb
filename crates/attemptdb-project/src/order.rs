//! Defensive ordering of the input stream.
//!
//! Callers are expected to pass events sorted by `(hlc, source_seq)`. The
//! projector re-sorts anyway so that the output is a pure function of the
//! *set* of events. Two modes exist:
//!
//! - **HLC order** `(observed_at, hlc, source_seq, event_id)` when every
//!   event has been ingested (non-zero HLC). `observed_at` leads because
//!   history can be *reconstructed* later (transcript import): such events
//!   are ingested long after the live events of the same session, so their
//!   HLC says nothing about when they happened. Within equal timestamps the
//!   writer's HLC/sequence keeps ingestion causality.
//! - **Wall order** `(observed_at, captured_at, event_id)` when at least one
//!   event has not been ingested. Mixing the two axes inside one stream would
//!   be ill-defined, so the whole stream falls back together.

use attemptdb_core::{Event, EventId};
use std::cmp::Ordering;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct OrderKey {
    pub hlc: u64,
    pub source_seq: u64,
    pub observed_at: i64,
    pub captured_at: i64,
    pub event_id: EventId,
}

impl OrderKey {
    pub fn from_event(ev: &Event) -> Self {
        Self {
            hlc: ev.hlc.as_u64(),
            source_seq: ev.source_seq,
            observed_at: ev.observed_at.as_micros(),
            captured_at: ev.captured_at.as_micros(),
            event_id: ev.event_id,
        }
    }

    pub fn compare(&self, other: &Self, mode: OrderMode) -> Ordering {
        match mode {
            OrderMode::Hlc => (self.observed_at, self.hlc, self.source_seq, self.event_id).cmp(&(
                other.observed_at,
                other.hlc,
                other.source_seq,
                other.event_id,
            )),
            OrderMode::Wall => (self.observed_at, self.captured_at, self.event_id).cmp(&(
                other.observed_at,
                other.captured_at,
                other.event_id,
            )),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OrderMode {
    Hlc,
    Wall,
}

/// HLC order only when every key carries a non-zero HLC.
pub(crate) fn choose_mode<'a>(keys: impl IntoIterator<Item = &'a OrderKey>) -> OrderMode {
    let mut any = false;
    for k in keys {
        any = true;
        if k.hlc == 0 {
            return OrderMode::Wall;
        }
    }
    if any { OrderMode::Hlc } else { OrderMode::Wall }
}
