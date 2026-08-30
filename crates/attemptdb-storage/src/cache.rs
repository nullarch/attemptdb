//! Decoded-segment cache for readers that refresh.
//!
//! A segment is immutable once published, so its decoded events and Arrow
//! batches can be kept across opens and reused until the manifest stops
//! listing it. A reader that polls a live database then pays only for the
//! segments published since its last refresh plus the WAL replay — not for
//! decompressing the whole history again (item 7 of `docs/benchmarks.md`).
//!
//! The cache is owned by the caller (a UI or MCP store, a server), not by
//! the database: databases are opened per request, the cache outlives them.

use crate::db::Database;
use crate::segment;
use crate::{Result, blobs::BlobReader};
use arrow::array::RecordBatch;
use attemptdb_core::Event;
use std::collections::HashMap;
use std::sync::Arc;
use uuid::Uuid;

/// One decoded segment.
#[derive(Debug)]
pub struct CachedSegment {
    pub segment_id: Uuid,
    pub events: Vec<Event>,
    pub batches: Vec<RecordBatch>,
}

/// Decoded segments by id, plus counters so tests (and `attempt status`)
/// can see what a refresh actually cost.
#[derive(Debug, Default)]
pub struct ScanCache {
    segments: HashMap<Uuid, Arc<CachedSegment>>,
    /// Segments decoded from disk over the cache's lifetime.
    pub decodes: u64,
    /// Refreshes served.
    pub refreshes: u64,
}

/// What one refresh produced: every segment in manifest order (shared with
/// the cache), the WAL's events, and which segments were new or gone.
#[derive(Debug, Default)]
pub struct Refreshed {
    pub segments: Vec<Arc<CachedSegment>>,
    pub memtable: Vec<Event>,
    pub new_segments: Vec<Uuid>,
    pub dropped_segments: Vec<Uuid>,
}

impl Refreshed {
    /// Every event, segments in manifest order then the WAL. Not globally
    /// sorted; callers that need stream order sort by `(hlc, source_seq)`.
    pub fn events(&self) -> impl Iterator<Item = &Event> {
        self.segments
            .iter()
            .flat_map(|s| s.events.iter())
            .chain(self.memtable.iter())
    }

    /// Events a projector has not seen before this refresh: those of the
    /// new segments plus the WAL. A projector that ignores duplicate ids can
    /// be fed this after every refresh.
    pub fn fresh_events(&self) -> impl Iterator<Item = &Event> {
        let new: std::collections::HashSet<Uuid> = self.new_segments.iter().copied().collect();
        self.segments
            .iter()
            .filter(move |s| new.contains(&s.segment_id))
            .flat_map(|s| s.events.iter())
            .chain(self.memtable.iter())
    }

    /// All Arrow batches: segments in manifest order, then the WAL as one
    /// trailing batch — the same layout `Database::batches` produces.
    pub fn batches(&self) -> Result<Vec<RecordBatch>> {
        let mut out: Vec<RecordBatch> = self
            .segments
            .iter()
            .flat_map(|s| s.batches.iter().cloned())
            .collect();
        if !self.memtable.is_empty() {
            out.extend(segment::events_to_batches(&self.memtable)?);
        }
        Ok(out)
    }

    /// The events `Database::scan(filter)` would return, from the cache:
    /// row-filtered, sorted by `(hlc, source_seq)`, then limited to the
    /// newest `limit`.
    pub fn scan(&self, filter: &crate::ScanFilter) -> Vec<Event> {
        let mut out: Vec<Event> = self
            .events()
            .filter(|e| filter.matches(e))
            .cloned()
            .collect();
        out.sort_by_key(|a| (a.hlc, a.source_seq));
        if let Some(limit) = filter.limit
            && out.len() > limit
        {
            out.drain(..out.len() - limit);
        }
        out
    }

    pub fn event_count(&self) -> usize {
        self.segments.iter().map(|s| s.events.len()).sum::<usize>() + self.memtable.len()
    }
}

impl ScanCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn segment_count(&self) -> usize {
        self.segments.len()
    }

    /// Bring the cache in line with `db`'s manifest: decode segments it has
    /// not seen, forget segments the manifest no longer lists, and read the
    /// WAL. Content of format-2 segments is resolved from the blob store
    /// with the database's key, exactly as `scan` and `batches` do.
    pub fn refresh(&mut self, db: &Database) -> Result<Refreshed> {
        self.refreshes += 1;
        let reader = BlobReader::new(db.blob_store(), db.key_provider().map(|k| k.as_ref()));
        let manifest = db.manifest();
        let listed: std::collections::HashSet<Uuid> =
            manifest.segments.iter().map(|s| s.segment_id).collect();
        let dropped: Vec<Uuid> = self
            .segments
            .keys()
            .filter(|id| !listed.contains(id))
            .copied()
            .collect();
        for id in &dropped {
            self.segments.remove(id);
        }
        let mut out = Refreshed {
            dropped_segments: dropped,
            ..Default::default()
        };
        for seg in &manifest.segments {
            if let Some(cached) = self.segments.get(&seg.segment_id) {
                out.segments.push(Arc::clone(cached));
                continue;
            }
            let path = segment::segments_dir(db.root()).join(&seg.file);
            let events = segment::read_segment_events_with(&path, Some(&reader))?;
            let mut batches = Vec::new();
            for batch in segment::read_segment_batches(&path)? {
                batches.push(if db.key_provider().is_some() {
                    segment::resolve_batch(&batch, &reader)?
                } else {
                    batch
                });
            }
            self.decodes += 1;
            let cached = Arc::new(CachedSegment {
                segment_id: seg.segment_id,
                events,
                batches,
            });
            self.segments.insert(seg.segment_id, Arc::clone(&cached));
            out.new_segments.push(seg.segment_id);
            out.segments.push(cached);
        }
        db.record_notes(reader.notes());
        out.memtable = db.memtable_events().to_vec();
        Ok(out)
    }

    /// Drop everything (a different database, or a snapshot).
    pub fn clear(&mut self) {
        self.segments.clear();
    }
}
