//! Decoded-segment cache for readers that refresh.
//!
//! A segment is immutable once published, so its Arrow batches can be kept
//! across opens and reused until the manifest stops listing it. A reader
//! that polls a live database then pays only for the segments published
//! since its last refresh plus the WAL replay — not for decompressing the
//! whole history again (item 7 of `docs/benchmarks.md`).
//!
//! Only the batches are kept. Decoded `Event`s cost about 3.5 KiB each on
//! top of the ~0.8 KiB their Arrow form takes (measured over 200 k
//! metadata-only events), so a resident `Vec<Event>` per segment was most
//! of a reader's memory. Callers that need events decode them on demand
//! through [`Refreshed::events`], segment by segment; the query layer
//! derives what it keeps (projection observations, id maps, facts) from
//! the columns and the transient decode.
//!
//! The cache is owned by the caller (a UI or MCP store, a server), not by
//! the database: databases are opened per request, the cache outlives them.

use crate::Result;
use crate::blobs::{BlobReader, BlobStore, KeyProvider};
use crate::db::Database;
use crate::manifest::SegmentMeta;
use crate::segment;
use arrow::array::RecordBatch;
use attemptdb_core::Event;
use std::collections::HashMap;
use std::sync::Arc;
use uuid::Uuid;

/// One cached segment: its manifest entry and its batches, canonical
/// schema, blob refs resolved when the database held the key.
#[derive(Debug)]
pub struct CachedSegment {
    pub segment_id: Uuid,
    pub meta: SegmentMeta,
    pub batches: Vec<RecordBatch>,
}

impl CachedSegment {
    pub fn row_count(&self) -> usize {
        self.batches.iter().map(RecordBatch::num_rows).sum()
    }

    /// Decode the segment's events. `reader` resolves encrypted content;
    /// without one, `content`/`raw` of format 2 rows come back `None`.
    pub fn decode(&self, reader: Option<&BlobReader<'_>>) -> Result<Vec<Event>> {
        let mut out = Vec::with_capacity(self.row_count());
        for b in &self.batches {
            out.extend(segment::batch_to_events_with(b, reader)?);
        }
        Ok(out)
    }
}

/// Decoded-segment cache by id, plus counters so tests (and `attempt
/// status`) can see what a refresh actually cost.
#[derive(Debug, Default)]
pub struct ScanCache {
    segments: HashMap<Uuid, Arc<CachedSegment>>,
    /// Segments decoded from disk over the cache's lifetime.
    pub decodes: u64,
    /// Refreshes served.
    pub refreshes: u64,
}

/// What one refresh produced: every segment in manifest order (shared with
/// the cache), the WAL's events, which segments were new or gone, and what
/// an on-demand decode needs to resolve content.
#[derive(Debug)]
pub struct Refreshed {
    pub segments: Vec<Arc<CachedSegment>>,
    pub memtable: Vec<Event>,
    pub new_segments: Vec<Uuid>,
    pub dropped_segments: Vec<Uuid>,
    blobs: BlobStore,
    keys: Option<Arc<dyn KeyProvider>>,
}

impl Refreshed {
    /// A blob reader over the database's key, for decoding segments one
    /// at a time ([`CachedSegment::decode`]).
    pub fn reader(&self) -> BlobReader<'_> {
        BlobReader::new(&self.blobs, self.keys.as_deref())
    }

    /// Every event, segments in manifest order then the WAL, decoded as the
    /// iterator advances (one segment at a time is resident). Not globally
    /// sorted; callers that need stream order sort by `(hlc, source_seq)`.
    /// A segment that fails to decode ends the iteration early; use
    /// [`Self::try_events`] to see the error.
    pub fn events(&self) -> impl Iterator<Item = Event> + '_ {
        self.decoded(self.segments.iter().map(Arc::as_ref))
    }

    /// As [`Self::events`], surfacing decode errors.
    pub fn try_events(&self) -> Result<Vec<Event>> {
        let reader = self.reader();
        let mut out = Vec::with_capacity(self.event_count());
        for s in &self.segments {
            out.extend(s.decode(Some(&reader))?);
        }
        out.extend(self.memtable.iter().cloned());
        Ok(out)
    }

    /// Events a projector has not seen before this refresh: those of the
    /// new segments plus the WAL. A projector that ignores duplicate ids can
    /// be fed this after every refresh.
    pub fn fresh_events(&self) -> impl Iterator<Item = Event> + '_ {
        let new: std::collections::HashSet<Uuid> = self.new_segments.iter().copied().collect();
        self.decoded(
            self.segments
                .iter()
                .map(Arc::as_ref)
                .filter(move |s| new.contains(&s.segment_id)),
        )
    }

    fn decoded<'a>(
        &'a self,
        segments: impl Iterator<Item = &'a CachedSegment> + 'a,
    ) -> impl Iterator<Item = Event> + 'a {
        let reader = self.reader();
        segments
            .flat_map(move |s| s.decode(Some(&reader)).unwrap_or_default())
            .chain(self.memtable.iter().cloned())
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
    /// segments the filter rules out are not decoded, rows are filtered,
    /// sorted by `(hlc, source_seq)`, then limited to the newest `limit`.
    pub fn scan(&self, filter: &crate::ScanFilter) -> Vec<Event> {
        let reader = self.reader();
        let mut out: Vec<Event> = Vec::new();
        for s in &self.segments {
            if !filter.segment_may_match(&s.meta) {
                continue;
            }
            out.extend(
                s.decode(Some(&reader))
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|e| filter.matches(e)),
            );
        }
        out.extend(self.memtable.iter().filter(|e| filter.matches(e)).cloned());
        out.sort_by_key(|a| (a.hlc, a.source_seq));
        if let Some(limit) = filter.limit
            && out.len() > limit
        {
            out.drain(..out.len() - limit);
        }
        out
    }

    pub fn event_count(&self) -> usize {
        self.segments.iter().map(|s| s.row_count()).sum::<usize>() + self.memtable.len()
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
            segments: Vec::with_capacity(manifest.segments.len()),
            memtable: Vec::new(),
            new_segments: Vec::new(),
            dropped_segments: dropped,
            blobs: db.blob_store().clone(),
            keys: db.key_provider().cloned(),
        };
        for seg in &manifest.segments {
            if let Some(cached) = self.segments.get(&seg.segment_id) {
                out.segments.push(Arc::clone(cached));
                continue;
            }
            let path = segment::segments_dir(db.root()).join(&seg.file);
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
                meta: seg.clone(),
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
