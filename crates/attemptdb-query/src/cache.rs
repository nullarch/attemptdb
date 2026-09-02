//! Decoded segments plus an incremental projection, kept across refreshes.
//!
//! A reader that serves a live database — the local UI, the MCP server, a
//! hosted tenant — pays for a refresh, not for a reload: [`EngineCache`]
//! keeps every decoded segment (`attemptdb_storage::ScanCache`) and the
//! per-session projection state (`attemptdb_project::IncrementalProjector`)
//! between refreshes, so a refresh after new events decodes only the newly
//! listed segments and re-finalises only the sessions they touched. The
//! caller builds the engine from the parts with [`crate::QueryEngine::from_parts`].
//!
//! The cache is owned by the caller and outlives any `Database` handle: a
//! database is opened per refresh (or held by a server), the cache is not.

use crate::parts::SegmentParts;
use crate::{QueryEngine, Result};
use attemptdb_core::Timestamp;
use attemptdb_project::{IncrementalProjector, Projection};
use attemptdb_storage::{Database, Refreshed, ScanCache};
use std::collections::HashMap;
use std::sync::Arc;
use uuid::Uuid;

/// What the cache has cost and holds so far.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CacheStats {
    /// Segments decoded from disk over the cache's lifetime.
    pub decodes: u64,
    /// Refreshes served.
    pub refreshes: u64,
    /// Segments currently held.
    pub segments: usize,
    /// Events held by the projector (duplicates excluded).
    pub events: usize,
    /// Sessions the next snapshot will rebuild.
    pub pending_sessions: usize,
}

/// Decoded segments and the incremental projection of one database.
#[derive(Debug, Default)]
pub struct EngineCache {
    scan: ScanCache,
    projector: IncrementalProjector,
    /// What the query layer derived from each listed segment (readable
    /// columns, id maps); kept as long as the segment is listed.
    parts: HashMap<Uuid, Arc<SegmentParts>>,
    /// Which database (or snapshot) the cache describes.
    source: String,
}

impl EngineCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// The source the cache was last refreshed from (empty before the
    /// first refresh).
    pub fn source(&self) -> &str {
        &self.source
    }

    /// Bring the cache in line with `db`. `source` names the database; a
    /// different source (another directory, a snapshot) clears everything
    /// first. Segments the manifest newly lists are decoded and their
    /// events pushed into the projector along with the WAL's; a segment
    /// that left the manifest (repair, restore) restarts the projector
    /// from the cache, because a projector cannot forget events.
    pub fn refresh(&mut self, db: &Database, source: &str) -> Result<Refreshed> {
        if self.source != source {
            self.scan.clear();
            self.projector = IncrementalProjector::new();
            self.parts.clear();
            self.source = source.to_string();
        }
        let refreshed = self.scan.refresh(db)?;
        for id in &refreshed.dropped_segments {
            self.parts.remove(id);
        }
        for seg in &refreshed.segments {
            self.parts
                .entry(seg.segment_id)
                .or_insert_with(|| Arc::new(SegmentParts::from_batches(seg.batches.clone())));
        }
        if refreshed.dropped_segments.is_empty() {
            for ev in refreshed.fresh_events() {
                self.projector.push(ev);
            }
        } else {
            self.projector = IncrementalProjector::new();
            for ev in refreshed.events() {
                self.projector.push(ev);
            }
        }
        Ok(refreshed)
    }

    /// The projection of everything refreshed so far, rebuilding only the
    /// sessions touched since the last snapshot.
    pub fn snapshot(&mut self) -> Projection {
        self.projector.snapshot()
    }

    /// An engine over everything `refreshed` holds: the segments' derived
    /// parts are shared with this cache (nothing is re-derived), the WAL's
    /// are built here for this engine, and the projection is the
    /// incremental snapshot. `refreshed` must be what the last
    /// [`Self::refresh`] returned.
    pub fn engine(&mut self, refreshed: &Refreshed) -> Result<QueryEngine> {
        let projection = self.projector.snapshot();
        self.engine_with(refreshed, projection)
    }

    /// As [`Self::engine`], with a projection the caller already took
    /// (for example judged at another time with [`Self::snapshot_at`]).
    pub fn engine_with(
        &mut self,
        refreshed: &Refreshed,
        projection: Projection,
    ) -> Result<QueryEngine> {
        let mut parts: Vec<Arc<SegmentParts>> = Vec::with_capacity(refreshed.segments.len() + 1);
        for seg in &refreshed.segments {
            let part = self
                .parts
                .entry(seg.segment_id)
                .or_insert_with(|| Arc::new(SegmentParts::from_batches(seg.batches.clone())));
            parts.push(Arc::clone(part));
        }
        if !refreshed.memtable.is_empty() {
            let batches = attemptdb_storage::segment::events_to_batches(&refreshed.memtable)?;
            parts.push(Arc::new(SegmentParts::from_batches_and_events(
                batches,
                refreshed.memtable.iter(),
            )));
        }
        Ok(QueryEngine::over(parts, projection))
    }

    /// As [`Self::snapshot`], judged against `now` instead of the stream's
    /// latest timestamp.
    pub fn snapshot_at(&mut self, now: Timestamp) -> Projection {
        self.projector.snapshot_at(now)
    }

    pub fn projector(&self) -> &IncrementalProjector {
        &self.projector
    }

    pub fn stats(&self) -> CacheStats {
        CacheStats {
            decodes: self.scan.decodes,
            refreshes: self.scan.refreshes,
            segments: self.scan.segment_count(),
            events: self.projector.len(),
            pending_sessions: self.projector.pending_sessions(),
        }
    }

    /// Forget everything; the next refresh starts from scratch.
    pub fn clear(&mut self) {
        self.scan.clear();
        self.projector = IncrementalProjector::new();
        self.parts.clear();
        self.source.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use attemptdb_core::event::Provider;
    use attemptdb_core::{CaptureMode, DeviceId, Event, EventKind, ProjectRef};
    use attemptdb_storage::OpenOptions;

    fn events(dev: DeviceId, n: usize, tag: &str) -> Vec<Event> {
        (0..n)
            .map(|_| {
                Event::new(
                    dev,
                    Provider::ClaudeCode,
                    "PostToolUse",
                    EventKind::ToolCallFinished,
                    ProjectRef::derive("/home/dev/example/project", None, &dev),
                    format!("session-{tag}"),
                    CaptureMode::MetadataOnly,
                    "cache-test/0",
                )
            })
            .collect()
    }

    #[test]
    fn refresh_decodes_new_segments_only_and_projects_incrementally() {
        let tmp = tempfile::tempdir().unwrap();
        let dev = DeviceId::derive(&["cache-test"]);
        let mut db = Database::open(
            tmp.path(),
            OpenOptions {
                create: true,
                device_id: Some(dev),
                ..Default::default()
            },
        )
        .unwrap();
        db.ingest(events(dev, 3, "a")).unwrap();
        db.flush().unwrap();

        let mut cache = EngineCache::new();
        let r = cache.refresh(&db, "db").unwrap();
        assert_eq!(r.event_count(), 3);
        assert_eq!(cache.stats().decodes, 1);
        assert_eq!(cache.stats().events, 3);
        assert_eq!(cache.snapshot().sessions.len(), 1);

        // WAL only: no decode, the projector grows by the new events.
        db.ingest(events(dev, 2, "b")).unwrap();
        let r = cache.refresh(&db, "db").unwrap();
        assert_eq!(r.event_count(), 5);
        let s = cache.stats();
        assert_eq!((s.decodes, s.refreshes, s.events), (1, 2, 5));
        assert_eq!(s.pending_sessions, 1, "only the new session is dirty");
        assert_eq!(cache.snapshot().sessions.len(), 2);
        assert_eq!(cache.stats().pending_sessions, 0);

        // Flushed into a second segment: one more decode, nothing counted twice.
        db.flush().unwrap();
        cache.refresh(&db, "db").unwrap();
        let s = cache.stats();
        assert_eq!((s.decodes, s.segments, s.events), (2, 2, 5));

        // Another source clears the cache.
        cache.refresh(&db, "elsewhere").unwrap();
        assert_eq!(cache.source(), "elsewhere");
        assert_eq!(cache.stats().decodes, 4);
        cache.clear();
        assert_eq!(cache.stats().events, 0);
    }
}
