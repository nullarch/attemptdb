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

use crate::facts::StreamFacts;
use crate::parts::SegmentParts;
use crate::{QueryEngine, Result};
use attemptdb_core::Timestamp;
use attemptdb_project::{IncrementalProjector, Projection};
use attemptdb_storage::{Database, Refreshed, ScanCache, ScanFilter};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
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
    /// The window's start when the cache serves a window; a projector
    /// cannot forget, so the cache is rebuilt when the window moves on.
    window_since: Option<Timestamp>,
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
        self.refresh_windowed(db, source, None, Duration::ZERO)
    }

    /// As [`Self::refresh`], serving only events observed at or after
    /// `since` (segments the manifest places entirely before it are never
    /// decoded). The projector cannot forget, so when `since` has moved
    /// past the cache's window by more than `slack` everything is rebuilt
    /// from the new window — cheap, since the window is what it holds.
    pub fn refresh_windowed(
        &mut self,
        db: &Database,
        source: &str,
        since: Option<Timestamp>,
        slack: Duration,
    ) -> Result<Refreshed> {
        let moved = match (self.window_since, since) {
            (None, None) => false,
            (Some(have), Some(want)) => {
                want.as_micros() - have.as_micros() > slack.as_micros() as i64
            }
            _ => true,
        };
        if self.source != source || moved {
            self.scan.clear();
            self.projector = IncrementalProjector::new();
            self.parts.clear();
            self.source = source.to_string();
            self.window_since = since;
        }
        let refreshed = self.scan.refresh_since(db, self.window_since)?;
        for id in &refreshed.dropped_segments {
            self.parts.remove(id);
        }
        for seg in &refreshed.segments {
            self.parts
                .entry(seg.segment_id)
                .or_insert_with(|| Arc::new(SegmentParts::from_batches(seg.batches.clone())));
        }
        // The projector reads content for three kinds; every other row is
        // decoded from its columns alone (no blob is opened).
        if refreshed.dropped_segments.is_empty() {
            for ev in refreshed.fresh_events_where(&attemptdb_project::needs_content) {
                self.projector.push(&ev);
            }
        } else {
            self.projector = IncrementalProjector::new();
            for ev in refreshed.events_where(&attemptdb_project::needs_content) {
                self.projector.push(&ev);
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

    /// The facts of everything `refreshed` holds — projects, providers,
    /// sessions, devices — merged from the segments' cached facts plus
    /// the WAL's. What a reader needs to resolve a scope before it builds
    /// an engine over it.
    pub fn facts(&mut self, refreshed: &Refreshed) -> StreamFacts {
        let mut merged = StreamFacts::default();
        for seg in &refreshed.segments {
            let part = self
                .parts
                .entry(seg.segment_id)
                .or_insert_with(|| Arc::new(SegmentParts::from_batches(seg.batches.clone())));
            merged.absorb(&part.facts);
        }
        if !refreshed.memtable.is_empty() {
            merged.absorb(&StreamFacts::from_events(refreshed.memtable.iter()));
        }
        merged
    }

    /// An engine over the scope `filter` selects, projected from exactly
    /// those events (as a `Database::scan` would give), but from the cache:
    /// segments the filter rules out are skipped, rows are filtered as
    /// Arrow, and content is read only for the kinds the projector needs.
    /// A `limit` in the filter decodes the scoped events instead (the
    /// newest rows need a global order).
    pub fn engine_scoped(
        &mut self,
        refreshed: &Refreshed,
        filter: &ScanFilter,
    ) -> Result<QueryEngine> {
        if filter.is_unfiltered()
            && !filter.captured_only
            && filter.exclude_sessions.is_empty()
            && filter.exclude_events.is_empty()
        {
            return self.engine(refreshed);
        }
        if filter.limit.is_some() {
            let events = refreshed.scan(filter);
            let batches = attemptdb_storage::segment::events_to_batches(&events)?;
            let projection = attemptdb_project::project(&events);
            let part = SegmentParts::from_batches_and_events(batches, events.iter());
            return Ok(QueryEngine::over(vec![Arc::new(part)], projection, None));
        }
        let batches = refreshed.filtered_batches(filter)?;
        let reader = refreshed.reader();
        let mut projector = IncrementalProjector::new();
        for b in &batches {
            for ev in attemptdb_storage::segment::batch_to_events_where(
                b,
                Some(&reader),
                &attemptdb_project::needs_content,
            )? {
                projector.push(&ev);
            }
        }
        let projection = projector.snapshot();
        let part = SegmentParts::from_batches(batches);
        Ok(QueryEngine::over(
            vec![Arc::new(part)],
            projection,
            Some(refreshed.resolver()),
        ))
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
        Ok(QueryEngine::over(
            parts,
            projection,
            Some(refreshed.resolver()),
        ))
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

    /// The window's start, when the cache serves one.
    pub fn window_since(&self) -> Option<Timestamp> {
        self.window_since
    }

    /// Forget everything; the next refresh starts from scratch.
    pub fn clear(&mut self) {
        self.scan.clear();
        self.projector = IncrementalProjector::new();
        self.parts.clear();
        self.source.clear();
        self.window_since = None;
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
    fn a_window_skips_old_segments_and_moves_in_steps() {
        let tmp = tempfile::tempdir().unwrap();
        let dev = DeviceId::derive(&["cache-window"]);
        let mut db = Database::open(
            tmp.path(),
            OpenOptions {
                create: true,
                device_id: Some(dev),
                ..Default::default()
            },
        )
        .unwrap();
        let stamped = |n: usize, tag: &str, at: i64| -> Vec<Event> {
            events(dev, n, tag)
                .into_iter()
                .enumerate()
                .map(|(i, mut e)| {
                    e.observed_at = Timestamp::from_micros(at + i as i64);
                    e
                })
                .collect()
        };
        // Two segments a day apart, then a WAL entry newer still.
        let day = 24 * 60 * 60 * 1_000_000;
        db.ingest(stamped(3, "old", 1_000_000)).unwrap();
        db.flush().unwrap();
        db.ingest(stamped(2, "new", day + 1_000_000)).unwrap();
        db.flush().unwrap();
        db.ingest(stamped(1, "wal", 2 * day)).unwrap();

        let mut cache = EngineCache::new();
        let slack = Duration::from_secs(60 * 60);
        // Window from day 1: the old segment is neither listed nor decoded.
        let r = cache
            .refresh_windowed(&db, "db", Some(Timestamp::from_micros(day)), slack)
            .unwrap();
        assert_eq!(r.segments.len(), 1);
        assert_eq!(r.event_count(), 3, "new segment + WAL");
        assert_eq!(cache.stats().decodes, 1);
        assert_eq!(cache.snapshot().sessions.len(), 2);
        // Nudging the window by less than the slack changes nothing.
        let r = cache
            .refresh_windowed(&db, "db", Some(Timestamp::from_micros(day + 60)), slack)
            .unwrap();
        assert_eq!(r.event_count(), 3);
        assert_eq!(cache.stats().decodes, 1, "no rebuild");
        // Moving it past the slack rebuilds from the new window: the
        // second segment is gone too, only the WAL remains.
        let r = cache
            .refresh_windowed(&db, "db", Some(Timestamp::from_micros(2 * day - 1)), slack)
            .unwrap();
        assert_eq!(r.segments.len(), 0);
        assert_eq!(r.event_count(), 1);
        assert_eq!(cache.snapshot().sessions.len(), 1);
        // Dropping the window brings everything back.
        let r = cache.refresh(&db, "db").unwrap();
        assert_eq!(r.event_count(), 6);
        assert_eq!(cache.snapshot().sessions.len(), 3);
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
