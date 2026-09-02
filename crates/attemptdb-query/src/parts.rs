//! What the query layer derives from a segment's batches, once.
//!
//! A segment is immutable, so everything an engine needs from it can be
//! computed the first time the segment is seen and reused by every engine
//! built afterwards: its `events` table columns with readable ids, and its
//! id maps (every event id in order, the set for membership, event ids per
//! session). The WAL — the events not yet in a segment — is derived the
//! same way but per view, because it changes with every ingest.
//!
//! [`crate::EngineCache`] keeps one [`SegmentParts`] per listed segment;
//! [`crate::QueryEngine`] holds `Arc`s to them plus one for the WAL.

use crate::Result;
use crate::tables;
use attemptdb_core::{Event, EventId, SessionId};
use attemptdb_storage::segment::col;
use datafusion::arrow::array::{Array, AsArray, RecordBatch};
use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

/// Id maps of one slice of the stream, in stream order.
#[derive(Debug, Default)]
pub(crate) struct IdMaps {
    pub event_ids: Vec<EventId>,
    pub set: HashSet<EventId>,
    pub session_events: HashMap<SessionId, Vec<EventId>>,
}

impl IdMaps {
    pub fn from_events<'a>(events: impl IntoIterator<Item = &'a Event>) -> Self {
        let mut m = Self::default();
        for e in events {
            m.push(e.event_id, e.session_id);
        }
        m
    }

    /// Straight from the `event_id` / `session_id` columns: no `Event` is
    /// decoded.
    pub fn from_batches(batches: &[RecordBatch]) -> Self {
        let mut m = Self::default();
        for b in batches {
            let (Some(ev), Some(ses)) = (
                b.column_by_name(col::EVENT_ID)
                    .and_then(|c| c.as_fixed_size_binary_opt()),
                b.column_by_name(col::SESSION_ID)
                    .and_then(|c| c.as_fixed_size_binary_opt()),
            ) else {
                continue;
            };
            for row in 0..b.num_rows() {
                if ev.is_null(row) || ev.value(row).len() != 16 {
                    continue;
                }
                let mut id = [0u8; 16];
                id.copy_from_slice(ev.value(row));
                let sid = if ses.is_null(row) || ses.value(row).len() != 16 {
                    SessionId::from_bytes([0u8; 16])
                } else {
                    let mut s = [0u8; 16];
                    s.copy_from_slice(ses.value(row));
                    SessionId::from_bytes(s)
                };
                m.push(EventId::from_bytes(id), sid);
            }
        }
        m
    }

    /// Every row counts, duplicates included: `event_count` reports what
    /// was loaded, and a segment never holds an id twice anyway.
    fn push(&mut self, id: EventId, sid: SessionId) {
        self.set.insert(id);
        self.event_ids.push(id);
        self.session_events.entry(sid).or_default().push(id);
    }
}

/// One slice of the stream as the query layer sees it.
#[derive(Debug)]
pub(crate) struct SegmentParts {
    /// The storage batches, canonical schema (`Arc`-backed clones).
    pub batches: Vec<RecordBatch>,
    /// The `events` table columns for those batches, minus the `retracted`
    /// flag (which depends on the projection): built on the first SQL
    /// statement and kept for every later engine over this segment.
    readable: OnceLock<std::result::Result<Vec<RecordBatch>, String>>,
    pub ids: IdMaps,
}

impl SegmentParts {
    pub fn from_batches(batches: Vec<RecordBatch>) -> Self {
        let ids = IdMaps::from_batches(&batches);
        Self {
            batches,
            readable: OnceLock::new(),
            ids,
        }
    }

    /// For callers that already hold the decoded events (the legacy
    /// `from_events` path): ids come from them, not from the columns.
    pub fn from_batches_and_events<'a>(
        batches: Vec<RecordBatch>,
        events: impl IntoIterator<Item = &'a Event>,
    ) -> Self {
        Self {
            batches,
            readable: OnceLock::new(),
            ids: IdMaps::from_events(events),
        }
    }

    /// The readable batches (no `retracted` column yet).
    pub fn readable(&self) -> Result<&[RecordBatch]> {
        self.readable
            .get_or_init(|| {
                self.batches
                    .iter()
                    .map(tables::readable_columns)
                    .collect::<Result<Vec<_>>>()
                    .map_err(|e| e.to_string())
            })
            .as_ref()
            .map(Vec::as_slice)
            .map_err(|m| crate::QueryError::Exec(m.clone()))
    }
}
