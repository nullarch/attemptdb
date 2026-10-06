//! Segments plus an incremental projection, kept across refreshes.
//!
//! A reader that serves a live database — the local UI, the MCP server, a
//! hosted tenant — pays for a refresh, not for a reload: [`EngineCache`]
//! keeps what it derived from each listed segment (`attemptdb_storage::ScanCache`
//! for the decoded batches, per-segment facts, per-segment query parts) and
//! the per-session projection state (`attemptdb_project::IncrementalProjector`)
//! between refreshes, so a refresh after new events reads only the newly
//! listed segments and re-finalises only the sessions they touched. The
//! caller builds the engine from the parts with [`crate::QueryEngine::from_parts`].
//!
//! # Work is done for the answer that is asked for
//!
//! A database is mostly OpenTelemetry records the projection ignores, and
//! most commands need a small part of the rest. So nothing is decoded or
//! projected up front:
//!
//! - [`EngineCache::refresh_lazy`] lists the segments and copies the WAL;
//!   no segment file is read.
//! - [`EngineCache::facts`] reads the few columns facts are made of
//!   ([`crate::facts::FACT_COLUMNS`]) of every segment it has no facts for
//!   yet, and keeps only the facts. `attempt status`, `attempt doctor` and
//!   scope resolution stop here.
//! - [`EngineCache::engine_scoped`] with a project, session or time window
//!   reads only the rows of that scope and projects only those, leaving
//!   telemetry rows out before they are decoded into events. Nothing of the
//!   rest of the history is kept, except what the last few scopes selected
//!   of each segment: segments never change, so a reload for the same scope
//!   (the daemon writes every few seconds) reads only the segments listed
//!   since, plus the WAL.
//! - [`EngineCache::engine`] (everything) decodes every listed segment once
//!   and projects it, again without decoding the telemetry rows.
//!
//! [`EngineCache::refresh`] and its windowed forms keep the eager shape
//! (every new segment decoded and projected under the caller's lock), for the
//! server, which holds its database across the read.
//!
//! The cache is owned by the caller and outlives any `Database` handle: a
//! database is opened per refresh (or held by a server), the cache is not.

use crate::facts::{FACT_COLUMNS, StreamFacts};
use crate::parts::SegmentParts;
use crate::{QueryEngine, Result};
use attemptdb_core::Timestamp;
use attemptdb_project::{IncrementalProjector, Projection};
use attemptdb_storage::{CachedSegment, Database, Refreshed, ScanCache, ScanFilter};
use datafusion::arrow::array::RecordBatch;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

impl crate::QueryError {
    /// A segment file the manifest listed was deleted (by a compaction) before
    /// it was read; see [`attemptdb_storage::cache::retry_vanished`].
    pub fn is_segment_gone(&self) -> bool {
        matches!(self, crate::QueryError::Storage(e) if e.is_segment_gone())
    }

    /// Some file a read needed vanished underneath it (a segment, a WAL file).
    pub fn is_vanished_file(&self) -> bool {
        matches!(self, crate::QueryError::Storage(e) if e.is_not_found())
    }
}

/// What the cache has cost and holds so far.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CacheStats {
    /// Segments decoded in full from disk over the cache's lifetime.
    pub decodes: u64,
    /// Refreshes served.
    pub refreshes: u64,
    /// Segments currently listed.
    pub segments: usize,
    /// Events the projector has seen (duplicates excluded; telemetry records
    /// it ignores are counted without having been decoded).
    pub events: usize,
    /// Sessions the next snapshot will rebuild.
    pub pending_sessions: usize,
}

/// How many scopes' rows stay memoised per segment.
const SCOPES_KEPT: usize = 4;

/// Decoded segments and the incremental projection of one database.
#[derive(Debug, Default)]
pub struct EngineCache {
    scan: ScanCache,
    projector: IncrementalProjector,
    /// Segments whose rows the projector holds, with how many telemetry
    /// records of each were left out before decoding.
    fed: HashMap<Uuid, u64>,
    /// Telemetry events among the WAL's at the last feed.
    wal_telemetry: u64,
    /// What the query layer derived from each listed segment (readable
    /// columns, id maps); kept as long as the segment is listed.
    parts: HashMap<Uuid, Arc<SegmentParts>>,
    /// Facts of each listed segment, read from its fact columns alone.
    facts: HashMap<Uuid, Arc<StreamFacts>>,
    /// The rows of each listed segment that a recent scope selects, by
    /// `(segment, scope)`: a segment never changes, so a reload for the same
    /// scope reads only the segments listed since (and the WAL). The scopes
    /// are the last [`SCOPES_KEPT`] asked for; memory is their rows.
    scoped: HashMap<(Uuid, String), Arc<Vec<RecordBatch>>>,
    scope_order: Vec<String>,
    /// Segment reads a scoped engine needed (a memoised segment costs none).
    scoped_reads: u64,
    /// Which database (or snapshot) the cache describes.
    source: String,
    /// The window's start when the cache serves a window; a projector
    /// cannot forget, so the cache is rebuilt when the window moves on.
    window_since: Option<Timestamp>,
}

/// Rows of `batch` the projector reads, decoded into events and pushed. The
/// telemetry rows are filtered out as Arrow, so they cost no event decode.
/// Returns how many it left out.
fn project_batch(
    projector: &mut IncrementalProjector,
    batch: &RecordBatch,
    reader: &attemptdb_storage::blobs::BlobReader<'_>,
) -> Result<u64> {
    let (mask, skipped) = crate::facts::non_telemetry_rows(batch);
    let kept;
    let batch = match mask {
        Some(m) => {
            kept = datafusion::arrow::compute::filter_record_batch(batch, &m)?;
            &kept
        }
        None => batch,
    };
    // Content is read for three kinds; every other row is decoded from its
    // columns alone (no blob is opened).
    for ev in attemptdb_storage::segment::batch_to_events_where(
        batch,
        Some(reader),
        &attemptdb_project::needs_content,
    )? {
        projector.push(&ev);
    }
    Ok(skipped)
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
        self.refresh_bounded(db, source, since, slack, None)
    }

    /// As [`Self::refresh_windowed`], holding at most `max_rows` segment
    /// rows of the window — the newest segments, whole (see
    /// `ScanCache::refresh_within`). A segment the budget no longer covers
    /// leaves the cache like one the window moved past.
    pub fn refresh_bounded(
        &mut self,
        db: &Database,
        source: &str,
        since: Option<Timestamp>,
        slack: Duration,
        max_rows: Option<u64>,
    ) -> Result<Refreshed> {
        let refreshed = self.refresh_inner(db, source, since, slack, max_rows, true)?;
        // Eager: the projector is current when this returns.
        self.feed(&refreshed)?;
        Ok(refreshed)
    }

    /// As [`Self::refresh`], decoding nothing: the manifest is read and the
    /// WAL copied, and every later step reads only what it asks for
    /// ([`Self::facts`], [`Self::engine_scoped`], [`Self::engine`]). The
    /// returned [`Refreshed`] is a lease on segment files: a compaction may
    /// delete one before something reads it, which fails with an error
    /// [`attemptdb_storage::StorageError::is_segment_gone`] recognises; wrap
    /// the whole read in [`attemptdb_storage::cache::retry_vanished`].
    pub fn refresh_lazy(&mut self, db: &Database, source: &str) -> Result<Refreshed> {
        self.refresh_inner(db, source, None, Duration::ZERO, None, false)
    }

    fn refresh_inner(
        &mut self,
        db: &Database,
        source: &str,
        since: Option<Timestamp>,
        slack: Duration,
        max_rows: Option<u64>,
        eager: bool,
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
            self.fed.clear();
            self.wal_telemetry = 0;
            self.parts.clear();
            if self.source != source {
                self.facts.clear();
                self.scoped.clear();
                self.scope_order.clear();
            }
            self.source = source.to_string();
            self.window_since = since;
        }
        let refreshed = if eager {
            self.scan.refresh_within(db, self.window_since, max_rows)?
        } else {
            self.scan.list_within(db, self.window_since, max_rows)?
        };
        for id in &refreshed.dropped_segments {
            self.parts.remove(id);
            self.facts.remove(id);
        }
        if !refreshed.dropped_segments.is_empty() {
            let gone: HashSet<&Uuid> = refreshed.dropped_segments.iter().collect();
            self.scoped.retain(|(id, _), _| !gone.contains(id));
        }
        Ok(refreshed)
    }

    /// Push what the projector has not seen: the rows of every listed segment
    /// not fed yet (telemetry records left out before decoding) and the
    /// WAL's. A segment that left the listing restarts the projector, which
    /// cannot forget.
    fn feed(&mut self, refreshed: &Refreshed) -> Result<()> {
        let listed: HashSet<Uuid> = refreshed.segments.iter().map(|s| s.segment_id).collect();
        if self.fed.keys().any(|id| !listed.contains(id)) {
            self.projector = IncrementalProjector::new();
            self.fed.clear();
        }
        let reader = refreshed.reader();
        for seg in &refreshed.segments {
            if self.fed.contains_key(&seg.segment_id) {
                continue;
            }
            let mut skipped = 0;
            for b in seg.batches()?.iter() {
                skipped += project_batch(&mut self.projector, b, &reader)?;
            }
            self.fed.insert(seg.segment_id, skipped);
        }
        // The WAL is pushed on every feed; the projector ignores ids it has.
        self.wal_telemetry = 0;
        for ev in &refreshed.memtable {
            if ev.is_telemetry() {
                self.wal_telemetry += 1;
            } else {
                self.projector.push(ev);
            }
        }
        Ok(())
    }

    /// Telemetry records the projector was spared (they are not in
    /// `projector.len()`), so counters that count every event keep counting
    /// them.
    fn left_out(&self) -> u64 {
        self.fed.values().sum::<u64>() + self.wal_telemetry
    }

    /// `p` with the stream's event count restored: the projector never saw
    /// the telemetry records it ignores.
    fn counted(&self, mut p: Projection) -> Projection {
        p.stats.events_seen += self.left_out();
        p
    }

    /// Run `read` over `refreshed`; when a file it listed has been deleted
    /// since (a compaction published a merged segment and collected its
    /// inputs), take a fresh listing through `reopen` — a new read-only open
    /// of the database — and run it again, up to
    /// [`attemptdb_storage::cache::READ_ATTEMPTS`] times. `refreshed` is left
    /// holding the listing the successful read used. A lazy refresh is a
    /// lease on files; this is how its holder renews it.
    pub fn retrying<T>(
        &mut self,
        refreshed: &mut Refreshed,
        reopen: &mut dyn FnMut() -> Result<Database>,
        mut read: impl FnMut(&mut EngineCache, &Refreshed) -> Result<T>,
    ) -> Result<T> {
        let mut attempt = 0;
        loop {
            match read(self, refreshed) {
                Err(e)
                    if e.is_vanished_file()
                        && attempt + 1 < attemptdb_storage::cache::READ_ATTEMPTS =>
                {
                    attemptdb_storage::cache::pause_before_retry(attempt);
                    attempt += 1;
                    let db = reopen()?;
                    let source = self.source.clone();
                    *refreshed = self.refresh_lazy(&db, &source)?;
                }
                other => return other,
            }
        }
    }

    /// The projection of everything fed so far, rebuilding only the sessions
    /// touched since the last snapshot. After an eager refresh that is
    /// everything refreshed; after [`Self::refresh_lazy`] use
    /// [`Self::snapshot_for`] (or build an engine), which feeds first.
    pub fn snapshot(&mut self) -> Projection {
        let p = self.projector.snapshot();
        self.counted(p)
    }

    /// As [`Self::snapshot`] over everything `refreshed` holds, feeding the
    /// projector first (decoding the segments that need it).
    pub fn snapshot_for(&mut self, refreshed: &Refreshed) -> Result<Projection> {
        self.feed(refreshed)?;
        Ok(self.snapshot())
    }

    /// An engine over everything `refreshed` holds: the segments' derived
    /// parts are shared with this cache (nothing is re-derived), the WAL's
    /// are built here for this engine, and the projection is the
    /// incremental snapshot. `refreshed` must be what the last refresh
    /// returned.
    pub fn engine(&mut self, refreshed: &Refreshed) -> Result<QueryEngine> {
        let projection = self.snapshot_for(refreshed)?;
        self.engine_with(refreshed, projection)
    }

    /// The facts of everything `refreshed` holds — projects, providers,
    /// sessions, devices — merged from the segments' cached facts plus
    /// the WAL's. What a reader needs to resolve a scope before it builds
    /// an engine over it. A segment without facts yet is read for
    /// [`crate::facts::FACT_COLUMNS`] only (and not kept, unless it is
    /// already in memory); no event is decoded.
    pub fn facts(&mut self, refreshed: &Refreshed) -> Result<StreamFacts> {
        let mut merged = StreamFacts::default();
        for seg in &refreshed.segments {
            let seg_facts = self.segment_facts(seg)?;
            merged.absorb(&seg_facts);
        }
        if !refreshed.memtable.is_empty() {
            merged.absorb(&StreamFacts::from_events(refreshed.memtable.iter()));
        }
        Ok(merged)
    }

    fn segment_facts(&mut self, seg: &CachedSegment) -> Result<Arc<StreamFacts>> {
        if let Some(f) = self.facts.get(&seg.segment_id) {
            return Ok(Arc::clone(f));
        }
        let mut f = StreamFacts::default();
        seg.read_columns(FACT_COLUMNS, &mut |b| {
            f.push_batch(&b);
            Ok(true)
        })?;
        let f = Arc::new(f);
        self.facts.insert(seg.segment_id, Arc::clone(&f));
        Ok(f)
    }

    /// The query parts of a segment (full batches, id maps, facts), derived
    /// once and shared by every engine over the segment. Decodes the
    /// segment if it is not in memory.
    fn segment_parts(&mut self, seg: &CachedSegment) -> Result<Arc<SegmentParts>> {
        if let Some(p) = self.parts.get(&seg.segment_id) {
            return Ok(Arc::clone(p));
        }
        let facts = self.segment_facts(seg)?;
        let part = Arc::new(SegmentParts::from_batches_with_facts(
            seg.batches()?.to_vec(),
            facts,
        ));
        self.parts.insert(seg.segment_id, Arc::clone(&part));
        Ok(part)
    }

    /// An engine over the scope `filter` selects, projected from exactly
    /// those events (as a `Database::scan` would give). Unfiltered, that is
    /// [`Self::engine`]. Scoped, only the rows of the scope are read — the
    /// filter's columns of every segment the zone maps do not rule out, then
    /// every column of just the batches that hold a matching row — and only
    /// those rows are projected, with telemetry records left out before they
    /// are decoded; segments held in memory are filtered as Arrow instead.
    /// Nothing outside the scope is kept. A `limit` in the filter decodes
    /// the scoped events instead (the newest rows need a global order).
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
            let events = refreshed.scan(filter)?;
            let batches = attemptdb_storage::segment::events_to_batches(&events)?;
            let projection = attemptdb_project::project(&events);
            let part = SegmentParts::from_batches_and_events(batches, events.iter());
            return Ok(QueryEngine::over(vec![Arc::new(part)], projection, None));
        }
        let batches = self.scoped_batches(refreshed, filter)?;
        let reader = refreshed.reader();
        let mut projector = IncrementalProjector::new();
        let mut left_out = 0;
        for b in &batches {
            left_out += project_batch(&mut projector, b, &reader)?;
        }
        let mut projection = projector.snapshot();
        projection.stats.events_seen += left_out;
        let part = SegmentParts::from_batches(batches);
        Ok(QueryEngine::over(
            vec![Arc::new(part)],
            projection,
            Some(refreshed.resolver()),
        ))
    }

    /// The rows `filter` selects of every listed segment (memoised per
    /// segment for the last few scopes) and of the WAL.
    fn scoped_batches(
        &mut self,
        refreshed: &Refreshed,
        filter: &ScanFilter,
    ) -> Result<Vec<RecordBatch>> {
        let key = format!("{filter:?}");
        self.scope_order.retain(|k| k != &key);
        self.scope_order.insert(0, key.clone());
        while self.scope_order.len() > SCOPES_KEPT {
            if let Some(old) = self.scope_order.pop() {
                self.scoped.retain(|(_, k), _| k != &old);
            }
        }
        let mut out = Vec::new();
        for seg in &refreshed.segments {
            if !seg.may_match(filter) {
                continue;
            }
            let slot = (seg.segment_id, key.clone());
            let rows = match self.scoped.get(&slot) {
                Some(rows) => Arc::clone(rows),
                None => {
                    self.scoped_reads += 1;
                    let rows = Arc::new(seg.filtered_batches(filter)?);
                    self.scoped.insert(slot, Arc::clone(&rows));
                    rows
                }
            };
            out.extend(rows.iter().cloned());
        }
        out.extend(refreshed.memtable_batches(filter)?);
        Ok(out)
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
            parts.push(self.segment_parts(seg)?);
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
        let p = self.projector.snapshot_at(now);
        self.counted(p)
    }

    pub fn projector(&self) -> &IncrementalProjector {
        &self.projector
    }

    pub fn stats(&self) -> CacheStats {
        CacheStats {
            decodes: self.scan.total_decodes(),
            refreshes: self.scan.refreshes,
            segments: self.scan.segment_count(),
            events: self.projector.len() + self.left_out() as usize,
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
        self.fed.clear();
        self.wal_telemetry = 0;
        self.parts.clear();
        self.facts.clear();
        self.scoped.clear();
        self.scope_order.clear();
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
    fn a_row_budget_keeps_the_newest_segments_whole() {
        let tmp = tempfile::tempdir().unwrap();
        let dev = DeviceId::derive(&["cache-budget"]);
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
        // Three segments of 3, 2 and 4 rows, oldest first, then one WAL row.
        db.ingest(stamped(3, "a", 1_000)).unwrap();
        db.flush().unwrap();
        db.ingest(stamped(2, "b", 2_000)).unwrap();
        db.flush().unwrap();
        db.ingest(stamped(4, "c", 3_000)).unwrap();
        db.flush().unwrap();
        db.ingest(stamped(1, "wal", 4_000)).unwrap();

        let mut cache = EngineCache::new();
        // Budget 6: the newest segment (4) and the next (2) fit; the oldest
        // would go over and is neither listed nor decoded.
        let r = cache
            .refresh_bounded(&db, "db", None, Duration::ZERO, Some(6))
            .unwrap();
        assert_eq!(r.segments.len(), 2);
        assert_eq!(r.event_count(), 7, "two segments + WAL");
        assert_eq!(r.budget_since, Some(Timestamp::from_micros(2_000)));
        assert_eq!(cache.stats().decodes, 2);
        assert_eq!(cache.snapshot().sessions.len(), 3);
        // Budget 3: only the newest segment fits (4 rows over a budget of 3
        // still counts — the newest is always held), the second leaves the
        // cache, and the projection is rebuilt from what is held.
        let r = cache
            .refresh_bounded(&db, "db", None, Duration::ZERO, Some(3))
            .unwrap();
        assert_eq!(r.segments.len(), 1);
        assert_eq!(r.event_count(), 5);
        assert_eq!(r.dropped_segments.len(), 1);
        assert_eq!(r.budget_since, Some(Timestamp::from_micros(3_000)));
        assert_eq!(cache.snapshot().sessions.len(), 2);
        // No budget: everything is back, and the oldest is decoded only now.
        let r = cache.refresh(&db, "db").unwrap();
        assert_eq!(r.segments.len(), 3);
        assert_eq!(r.event_count(), 10);
        assert!(r.budget_since.is_none());
        assert_eq!(cache.stats().decodes, 4);
        assert_eq!(cache.snapshot().sessions.len(), 4);
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

    #[test]
    fn a_lazy_refresh_reads_nothing_until_something_asks() {
        let tmp = tempfile::tempdir().unwrap();
        let dev = DeviceId::derive(&["cache-lazy"]);
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
        db.ingest(events(dev, 2, "b")).unwrap();
        db.flush().unwrap();
        db.ingest(events(dev, 1, "wal")).unwrap();

        let mut cache = EngineCache::new();
        let r = cache.refresh_lazy(&db, "db").unwrap();
        assert_eq!(r.event_count(), 6);
        assert_eq!(cache.stats().decodes, 0, "nothing decoded");
        assert_eq!(cache.stats().events, 0, "nothing projected");
        // Facts read a few columns and keep no batches.
        let facts = cache.facts(&r).unwrap();
        assert_eq!(facts.events, 6);
        assert_eq!(facts.session_count(), 3);
        assert_eq!(cache.stats().decodes, 0, "facts decode no segment");
        // A scoped engine decodes only its rows.
        let pid = ProjectRef::derive("/home/dev/example/project", None, &dev).project_id;
        let filter = ScanFilter {
            project_id: Some(pid),
            ..Default::default()
        };
        let engine = cache.engine_scoped(&r, &filter).unwrap();
        assert_eq!(engine.event_count(), 6);
        assert_eq!(engine.projection().sessions.len(), 3);
        assert_eq!(
            cache.stats().decodes,
            0,
            "a scope reads rows, keeps nothing"
        );
        // Everything decodes each segment once and projects it.
        let all = cache.engine(&r).unwrap();
        assert_eq!(all.event_count(), 6);
        assert_eq!(cache.stats().decodes, 2);
        assert_eq!(cache.stats().events, 6);
        assert_eq!(all.projection().stats.events_seen, 6);
        // Facts are unchanged by the decode.
        assert_eq!(cache.facts(&r).unwrap().events, 6);
    }

    /// A lazy listing is a lease on files: a compaction that deletes them
    /// fails the read with an error `retrying` recognises, and the read is
    /// repeated from a fresh manifest (REPORT.md §4.3).
    #[test]
    fn a_listing_whose_segments_were_compacted_away_is_renewed() {
        let tmp = tempfile::tempdir().unwrap();
        let dev = DeviceId::derive(&["cache-retry"]);
        let open = |read_only: bool| {
            Database::open(
                tmp.path(),
                OpenOptions {
                    create: !read_only,
                    read_only,
                    device_id: Some(dev),
                    flush_events: usize::MAX,
                    flush_bytes: usize::MAX,
                    ..Default::default()
                },
            )
            .unwrap()
        };
        let mut db = open(false);
        for i in 0..4 {
            db.ingest(events(dev, 5, &format!("s{i}"))).unwrap();
            db.flush().unwrap();
        }
        drop(db);
        let stale = open(true);
        let mut cache = EngineCache::new();
        let mut refreshed = cache.refresh_lazy(&stale, "db").unwrap();
        assert_eq!(refreshed.segments.len(), 4);

        // A compaction merges the four, and the next flush collects them.
        let mut db = open(false);
        db.compact(&attemptdb_storage::CompactionPolicy {
            max_segments: 1,
            small_segment_bytes: u64::MAX,
            min_inputs: 2,
            ..Default::default()
        })
        .unwrap()
        .expect("merged");
        db.ingest(events(dev, 1, "after")).unwrap();
        db.flush().unwrap();
        drop(db);

        let filter = ScanFilter {
            project_id: Some(
                ProjectRef::derive("/home/dev/example/project", None, &dev).project_id,
            ),
            ..Default::default()
        };
        let err = cache
            .engine_scoped(&refreshed, &filter)
            .err()
            .expect("the listed inputs are gone");
        assert!(err.is_segment_gone(), "{err}");
        let mut reopened = 0;
        let engine = cache
            .retrying(
                &mut refreshed,
                &mut || {
                    reopened += 1;
                    Ok(open(true))
                },
                |c, r| c.engine_scoped(r, &filter),
            )
            .unwrap();
        assert_eq!(reopened, 1);
        assert_eq!(engine.event_count(), 21, "all events, once");
        assert_eq!(engine.projection().sessions.len(), 5);
        // Facts renew the same way.
        let mut cache = EngineCache::new();
        let mut stale_listing = cache.refresh_lazy(&stale, "db").unwrap();
        let facts = cache
            .retrying(&mut stale_listing, &mut || Ok(open(true)), |c, r| {
                c.facts(r)
            })
            .unwrap();
        assert_eq!(facts.events, 21);
    }

    /// A reload for the same scope reads only what is new: segments never
    /// change, so what a scope selected of each is kept (and dropped when the
    /// segment goes).
    #[test]
    fn a_reload_for_the_same_scope_reads_only_the_new_segments() {
        let tmp = tempfile::tempdir().unwrap();
        let dev = DeviceId::derive(&["cache-scope-memo"]);
        let mut db = Database::open(
            tmp.path(),
            OpenOptions {
                create: true,
                device_id: Some(dev),
                flush_events: usize::MAX,
                flush_bytes: usize::MAX,
                ..Default::default()
            },
        )
        .unwrap();
        db.ingest(events(dev, 3, "a")).unwrap();
        db.flush().unwrap();
        db.ingest(events(dev, 2, "b")).unwrap();
        db.flush().unwrap();
        let pid = ProjectRef::derive("/home/dev/example/project", None, &dev).project_id;
        let filter = ScanFilter {
            project_id: Some(pid),
            ..Default::default()
        };
        let mut cache = EngineCache::new();
        let r = cache.refresh_lazy(&db, "db").unwrap();
        let e = cache.engine_scoped(&r, &filter).unwrap();
        assert_eq!((e.event_count(), cache.scoped_reads), (5, 2));
        // The same listing again: nothing is read.
        cache.engine_scoped(&r, &filter).unwrap();
        assert_eq!(cache.scoped_reads, 2);
        // A new segment and a WAL event: one segment read, the rest memoised.
        db.ingest(events(dev, 4, "c")).unwrap();
        db.flush().unwrap();
        db.ingest(events(dev, 1, "wal")).unwrap();
        let r = cache.refresh_lazy(&db, "db").unwrap();
        let e = cache.engine_scoped(&r, &filter).unwrap();
        assert_eq!((e.event_count(), cache.scoped_reads), (10, 3));
        assert_eq!(e.projection().sessions.len(), 4);
        // Another scope reads for itself; the first stays warm.
        let other = ScanFilter {
            project_id: Some(attemptdb_core::ProjectId::derive(&["nothing"])),
            ..Default::default()
        };
        let e = cache.engine_scoped(&r, &other).unwrap();
        assert_eq!(e.event_count(), 0);
        cache.engine_scoped(&r, &filter).unwrap();
        assert_eq!(
            cache.scoped_reads, 3,
            "no segment of `other` matched, none re-read"
        );
        // Compaction replaces segments: the memo of the inputs goes with them.
        db.compact(&attemptdb_storage::CompactionPolicy {
            max_segments: 1,
            small_segment_bytes: u64::MAX,
            min_inputs: 2,
            ..Default::default()
        })
        .unwrap()
        .expect("merged");
        let r = cache.refresh_lazy(&db, "db").unwrap();
        assert_eq!(r.dropped_segments.len(), 3);
        let e = cache.engine_scoped(&r, &filter).unwrap();
        assert_eq!(e.event_count(), 10);
        assert_eq!(cache.scoped_reads, 4, "the merged segment, once");
        assert!(
            cache
                .scoped
                .keys()
                .all(|(id, _)| r.segments.iter().any(|s| s.segment_id == *id))
        );
    }
}
