//! Segment cache for readers that refresh.
//!
//! A segment is immutable once published, so what a reader derives from it
//! can be kept across opens and reused until the manifest stops listing it.
//! A reader that polls a live database then pays only for the segments
//! published since its last refresh plus the WAL replay — not for
//! decompressing the whole history again (item 7 of `docs/benchmarks.md`).
//!
//! Listing a segment and decoding it are separate steps. [`ScanCache::list`]
//! reads the manifest and the WAL and touches no segment file; a
//! [`CachedSegment`] decodes its full Arrow batches the first time something
//! asks for them ([`CachedSegment::batches`]) and keeps them. Most readers
//! never need that: a count, a provider list or a scope check reads a few
//! columns ([`CachedSegment::read_columns`]) and a scoped query decodes only
//! the rows of its scope ([`CachedSegment::filtered_batches`]); neither
//! keeps anything but what it derived. [`ScanCache::refresh`] is the eager
//! form (every new segment decoded under the caller's lock) for callers that
//! hold the database across the read, such as the server.
//!
//! Only batches are kept, never decoded `Event`s: those cost about 3.5 KiB
//! each on top of the ~0.8 KiB their Arrow form takes (measured over 200 k
//! metadata-only events). Callers that need events decode them on demand,
//! segment by segment; the query layer derives what it keeps (projection
//! observations, id maps, facts) from the columns and the transient decode.
//!
//! # Readers and compaction
//!
//! A compaction publishes a merged segment and, a generation later, deletes
//! its inputs. A reader that read the manifest before that and opens a
//! segment after it finds the file gone ([`StorageError::is_not_found`]).
//! [`retry_vanished`] re-runs a read from a fresh manifest in that case, and
//! [`ScanCache::refresh`]/[`ScanCache::list`] do it on their own: they open
//! a fresh read-only handle and start over, up to [`READ_ATTEMPTS`] times.
//!
//! The cache is owned by the caller (a UI or MCP store, a server), not by
//! the database: databases are opened per request, the cache outlives them.

use crate::Result;
use crate::StorageError;
use crate::blobs::{BlobReader, BlobStore, KeyProvider};
use crate::db::{Database, OpenOptions};
use crate::format::SEGMENTS_DIR;
use crate::manifest::SegmentMeta;
use crate::segment;
use arrow::array::RecordBatch;
use attemptdb_core::{Event, EventKind, Timestamp};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use uuid::Uuid;

/// How many times a read starts over from a fresh manifest when a file it
/// listed was deleted by a compaction or a flush in the meantime.
pub const READ_ATTEMPTS: usize = 5;

impl StorageError {
    /// A file the read needed is not there: the manifest (or WAL listing)
    /// was read before a compaction or flush deleted it. A fresh read of the
    /// manifest usually succeeds; see [`retry_vanished`].
    pub fn is_not_found(&self) -> bool {
        matches!(self, StorageError::Io { source, .. } if source.kind() == std::io::ErrorKind::NotFound)
    }

    /// [`Self::is_not_found`] for a segment file in particular.
    pub fn is_segment_gone(&self) -> bool {
        matches!(
            self,
            StorageError::Io { source, path }
                if source.kind() == std::io::ErrorKind::NotFound
                    && path.components().any(|c| c.as_os_str() == SEGMENTS_DIR)
        )
    }
}

fn backoff(attempt: usize) -> Duration {
    // 20, 40, 80, 160 ms: a flush or compaction publishes within a few
    // milliseconds, and a reader that lost the race should not hammer.
    Duration::from_millis(20 << attempt.min(4))
}

/// Sleep as [`retry_vanished`] does between attempts (`attempt` counts from
/// 0), for a retry loop that cannot be written as a closure.
pub fn pause_before_retry(attempt: usize) {
    std::thread::sleep(backoff(attempt));
}

/// Run `read`, which is handed the attempt number (0 first), and run it
/// again from the top when a file it needed vanished underneath it, up to
/// [`READ_ATTEMPTS`] times with a short pause between. Anything else fails
/// at once; the last attempt's failure is returned with the count so a
/// listed-but-missing file (deleted by hand, not by a compaction) is not
/// mistaken for a race.
pub fn retry_vanished<T>(read: impl FnMut(usize) -> Result<T>) -> Result<T> {
    retry_when(read, StorageError::is_not_found).map_err(|e| {
        if e.is_not_found() {
            StorageError::Other(format!(
                "a file the database lists is missing, and still is after {READ_ATTEMPTS} reads of the manifest (a compaction would have settled by now; was something deleted by hand? `attempt repair` can check): {e}"
            ))
        } else {
            e
        }
    })
}

/// [`retry_vanished`] for a caller whose errors are not [`StorageError`]s:
/// `vanished` says whether an error means a file went missing. When the
/// attempts run out the last error is returned unchanged.
pub fn retry_when<T, E>(
    mut read: impl FnMut(usize) -> std::result::Result<T, E>,
    vanished: impl Fn(&E) -> bool,
) -> std::result::Result<T, E> {
    let mut attempt = 0;
    loop {
        match read(attempt) {
            Err(e) if vanished(&e) && attempt + 1 < READ_ATTEMPTS => {
                std::thread::sleep(backoff(attempt));
                attempt += 1;
            }
            other => return other,
        }
    }
}

/// What resolves a database's encrypted content outside a `Database`
/// handle: the blob directory and the key provider it was opened with.
#[derive(Clone)]
pub struct ContentResolver {
    blobs: BlobStore,
    keys: Option<Arc<dyn KeyProvider>>,
}

impl std::fmt::Debug for ContentResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ContentResolver")
            .field("keys", &self.keys.is_some())
            .finish()
    }
}

impl ContentResolver {
    pub fn reader(&self) -> BlobReader<'_> {
        BlobReader::new(&self.blobs, self.keys.as_deref())
    }

    /// Whether any content could be resolved at all (a key is held).
    pub fn has_keys(&self) -> bool {
        self.keys.is_some()
    }

    /// Fill `content_json`/`raw_json` of a batch from its blob refs.
    pub fn resolve_batch(&self, batch: &RecordBatch) -> Result<RecordBatch> {
        segment::resolve_batch(batch, &self.reader())
    }
}

/// One listed segment: its manifest entry and, once something asked for
/// them, its batches on the canonical schema (blob refs unresolved).
pub struct CachedSegment {
    pub segment_id: Uuid,
    pub meta: SegmentMeta,
    path: PathBuf,
    /// Full decode, filled eagerly by [`ScanCache::refresh`] or on the first
    /// [`Self::batches`]. A failed read is not remembered: the next call
    /// tries again.
    batches: Mutex<Option<Arc<Vec<RecordBatch>>>>,
    /// Shared with the cache that listed the segment: counts full decodes.
    decodes: Arc<AtomicU64>,
}

impl std::fmt::Debug for CachedSegment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CachedSegment")
            .field("file", &self.meta.file)
            .field("rows", &self.meta.rows)
            .field("resident", &self.is_resident())
            .finish()
    }
}

impl CachedSegment {
    fn new(
        meta: SegmentMeta,
        path: PathBuf,
        resident: Option<Vec<RecordBatch>>,
        decodes: Arc<AtomicU64>,
    ) -> Self {
        Self {
            segment_id: meta.segment_id,
            meta,
            path,
            batches: Mutex::new(resident.map(Arc::new)),
            decodes,
        }
    }

    /// Rows in the segment (the manifest's count).
    pub fn row_count(&self) -> usize {
        self.meta.rows as usize
    }

    /// Whether the segment's full decode is held in memory.
    pub fn is_resident(&self) -> bool {
        self.resident().is_some()
    }

    fn resident(&self) -> Option<Arc<Vec<RecordBatch>>> {
        self.batches.lock().ok().and_then(|g| g.clone())
    }

    /// Drop the full decode (a later [`Self::batches`] reads the file again).
    pub fn evict(&self) {
        if let Ok(mut g) = self.batches.lock() {
            *g = None;
        }
    }

    /// Every column of every row, decoded once and kept. Blob refs stay
    /// unresolved: a segment holds one blob file per content-bearing row,
    /// and most readers never look at content.
    pub fn batches(&self) -> Result<Arc<Vec<RecordBatch>>> {
        let mut slot = self
            .batches
            .lock()
            .map_err(|_| StorageError::Other("segment cache lock poisoned".into()))?;
        if let Some(b) = slot.as_ref() {
            return Ok(Arc::clone(b));
        }
        let read = Arc::new(segment::read_segment_batches(&self.path)?);
        self.decodes.fetch_add(1, Ordering::Relaxed);
        *slot = Some(Arc::clone(&read));
        Ok(read)
    }

    /// Decode the segment's events. `reader` resolves encrypted content;
    /// without one, `content`/`raw` of format 2 rows come back `None`.
    pub fn decode(&self, reader: Option<&BlobReader<'_>>) -> Result<Vec<Event>> {
        self.decode_where(reader, &|_| true)
    }

    /// As [`Self::decode`], resolving content only for kinds
    /// `wants_content` accepts (see `segment::batch_to_events_where`).
    pub fn decode_where(
        &self,
        reader: Option<&BlobReader<'_>>,
        wants_content: &dyn Fn(EventKind) -> bool,
    ) -> Result<Vec<Event>> {
        let batches = self.batches()?;
        let mut out = Vec::with_capacity(self.row_count());
        for b in batches.iter() {
            out.extend(segment::batch_to_events_where(b, reader, wants_content)?);
        }
        Ok(out)
    }

    /// Walk the segment's rows with only `columns` decoded; `sink` gets each
    /// batch (holding just the requested columns the file has, matched by
    /// name, not the canonical schema) and says whether to go on. A
    /// resident segment is projected in memory; otherwise the file is read
    /// with an Arrow IPC projection and nothing is kept.
    pub fn read_columns(
        &self,
        columns: &[&str],
        sink: &mut dyn FnMut(RecordBatch) -> Result<bool>,
    ) -> Result<()> {
        if let Some(batches) = self.resident() {
            for b in batches.iter() {
                let schema = b.schema();
                let idx: Vec<usize> = columns
                    .iter()
                    .filter_map(|c| schema.index_of(c).ok())
                    .collect();
                if !sink(b.project(&idx)?)? {
                    break;
                }
            }
            return Ok(());
        }
        segment::for_each_segment_columns(&self.path, columns, sink)
    }

    /// The rows `filter` keeps, as canonical batches. A resident segment is
    /// filtered in memory. Otherwise only the columns the filter judges by
    /// are read for every batch, and every column of just the batches that
    /// kept a row; a segment where the scope is a small share of the stream
    /// costs little, and nothing is kept in the cache.
    pub fn filtered_batches(&self, filter: &crate::ScanFilter) -> Result<Vec<RecordBatch>> {
        if let Some(batches) = self.resident() {
            let mut out = Vec::new();
            for b in batches.iter() {
                if let Some(kept) = filter.filter_batch(b)? {
                    out.push(kept);
                }
            }
            return Ok(out);
        }
        let columns = filter.mask_columns();
        if columns.is_empty() {
            return Ok(self.batches()?.to_vec());
        }
        segment::read_matching_batches(&self.path, &columns, &mut |b| filter.row_mask(b))
    }
}

/// Segment cache by id, plus counters so tests (and `attempt status`) can
/// see what a refresh actually cost.
#[derive(Debug, Default)]
pub struct ScanCache {
    segments: HashMap<Uuid, Arc<CachedSegment>>,
    /// Segments decoded by an eager refresh over the cache's lifetime (see
    /// [`Self::total_decodes`] for those decoded later, on demand).
    pub decodes: u64,
    /// Refreshes served.
    pub refreshes: u64,
    lazy_decodes: Arc<AtomicU64>,
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
    /// When a row budget left segments of the requested window out: the
    /// earliest `observed_at` the refresh does hold, so a reader can say
    /// where its history starts. `None` when the whole window is held.
    pub budget_since: Option<Timestamp>,
    blobs: BlobStore,
    keys: Option<Arc<dyn KeyProvider>>,
    /// Segments the lossy iterators ([`Self::events`] and friends) could not
    /// decode, one line each.
    failures: Arc<Mutex<Vec<String>>>,
}

impl Refreshed {
    /// A blob reader over the database's key, for decoding segments one
    /// at a time ([`CachedSegment::decode`]).
    pub fn reader(&self) -> BlobReader<'_> {
        BlobReader::new(&self.blobs, self.keys.as_deref())
    }

    /// Segments the iterators below skipped because they failed to decode
    /// (one line each, naming the segment). Empty when everything read.
    /// The iterators cannot return an error per item; a caller that must not
    /// serve a partial view checks this after consuming them, or uses
    /// [`Self::try_events`] and [`Self::scan`], which fail instead.
    pub fn decode_failures(&self) -> Vec<String> {
        self.failures.lock().map(|f| f.clone()).unwrap_or_default()
    }

    /// Every event, segments in manifest order then the WAL, decoded as the
    /// iterator advances (one segment at a time is resident). Not globally
    /// sorted; callers that need stream order sort by `(hlc, source_seq)`.
    /// A segment that fails to decode is skipped and recorded in
    /// [`Self::decode_failures`]; use [`Self::try_events`] to fail instead.
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
        self.fresh_events_where(&|_| true)
    }

    /// As [`Self::fresh_events`], resolving content only for the kinds
    /// `wants_content` accepts.
    pub fn fresh_events_where<'a>(
        &'a self,
        wants_content: &'a dyn Fn(EventKind) -> bool,
    ) -> impl Iterator<Item = Event> + 'a {
        let new: std::collections::HashSet<Uuid> = self.new_segments.iter().copied().collect();
        self.decoded_where(
            self.segments
                .iter()
                .map(Arc::as_ref)
                .filter(move |s| new.contains(&s.segment_id)),
            wants_content,
        )
    }

    /// As [`Self::events`], resolving content only for the kinds
    /// `wants_content` accepts.
    pub fn events_where<'a>(
        &'a self,
        wants_content: &'a dyn Fn(EventKind) -> bool,
    ) -> impl Iterator<Item = Event> + 'a {
        self.decoded_where(self.segments.iter().map(Arc::as_ref), wants_content)
    }

    fn decoded<'a>(
        &'a self,
        segments: impl Iterator<Item = &'a CachedSegment> + 'a,
    ) -> impl Iterator<Item = Event> + 'a {
        self.decoded_where(segments, &|_| true)
    }

    fn decoded_where<'a>(
        &'a self,
        segments: impl Iterator<Item = &'a CachedSegment> + 'a,
        wants_content: &'a dyn Fn(EventKind) -> bool,
    ) -> impl Iterator<Item = Event> + 'a {
        let reader = self.reader();
        segments
            .flat_map(
                move |s| match s.decode_where(Some(&reader), wants_content) {
                    Ok(events) => events,
                    Err(e) => {
                        if let Ok(mut f) = self.failures.lock() {
                            f.push(format!("segment {}: {e}", s.meta.file));
                        }
                        Vec::new()
                    }
                },
            )
            .chain(self.memtable.iter().cloned())
    }

    /// What resolves this database's encrypted content, for a reader that
    /// outlives the refresh (the query layer's lazy content columns).
    pub fn resolver(&self) -> ContentResolver {
        ContentResolver {
            blobs: self.blobs.clone(),
            keys: self.keys.clone(),
        }
    }

    /// All Arrow batches: segments in manifest order, then the WAL as one
    /// trailing batch. Unlike `Database::batches`, format 2 segments keep
    /// their `content_ref`/`raw_ref` columns: resolve them through
    /// [`Self::resolver`] when content is wanted. Decodes (and keeps) every
    /// listed segment.
    pub fn batches(&self) -> Result<Vec<RecordBatch>> {
        let mut out: Vec<RecordBatch> = Vec::new();
        for s in &self.segments {
            out.extend(s.batches()?.iter().cloned());
        }
        if !self.memtable.is_empty() {
            out.extend(segment::events_to_batches(&self.memtable)?);
        }
        Ok(out)
    }

    /// The events `Database::scan(filter)` would return, from the cache:
    /// segments the filter rules out are not read, rows are filtered (a
    /// segment not held in memory decodes only the rows that match), sorted
    /// by `(hlc, source_seq)`, then limited to the newest `limit`. With a
    /// `limit`, segments are visited newest first and the walk stops at the
    /// first one that cannot hold a newer event than the `limit` already
    /// collected.
    pub fn scan(&self, filter: &crate::ScanFilter) -> Result<Vec<Event>> {
        let reader = self.reader();
        let mut out: Vec<Event> = Vec::new();
        let mut candidates: Vec<&Arc<CachedSegment>> = self
            .segments
            .iter()
            .filter(|s| filter.segment_may_match(&s.meta))
            .collect();
        if filter.limit.is_some() {
            candidates.sort_by_key(|s| std::cmp::Reverse(s.meta.max_hlc));
        }
        let mut floor: Option<(attemptdb_core::Hlc, u64)> = None;
        for s in candidates {
            if let (Some(limit), Some(floor)) = (filter.limit, floor)
                && out.len() >= limit
                && s.meta.max_hlc < floor.0
            {
                break;
            }
            for b in s.filtered_batches(filter)? {
                for ev in segment::batch_to_events_where(&b, Some(&reader), &|_| true)? {
                    out.push(ev);
                }
            }
            if let Some(limit) = filter.limit
                && out.len() >= limit
            {
                out.sort_by_key(|a| std::cmp::Reverse((a.hlc, a.source_seq)));
                out.truncate(limit);
                floor = out.last().map(|e| (e.hlc, e.source_seq));
            }
        }
        out.extend(self.memtable.iter().filter(|e| filter.matches(e)).cloned());
        out.sort_by_key(|a| (a.hlc, a.source_seq));
        if let Some(limit) = filter.limit
            && out.len() > limit
        {
            out.drain(..out.len() - limit);
        }
        Ok(out)
    }

    /// The batches `scan(filter)` would decode, still as Arrow: segments
    /// the filter rules out are skipped, rows are filtered in place, and
    /// the WAL's matching events are encoded as a trailing batch. Without
    /// `limit`, this is the filtered stream; with it, callers should
    /// [`Self::scan`] instead, since the newest `limit` rows need a global
    /// order.
    pub fn filtered_batches(&self, filter: &crate::ScanFilter) -> Result<Vec<RecordBatch>> {
        let mut out = Vec::new();
        for s in &self.segments {
            if !filter.segment_may_match(&s.meta) {
                continue;
            }
            out.extend(s.filtered_batches(filter)?);
        }
        let wal: Vec<Event> = self
            .memtable
            .iter()
            .filter(|e| filter.matches(e))
            .cloned()
            .collect();
        if !wal.is_empty() {
            out.extend(segment::events_to_batches(&wal)?);
        }
        Ok(out)
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

    /// Segments decoded in full over the cache's lifetime: those an eager
    /// refresh decoded plus those decoded later on demand.
    pub fn total_decodes(&self) -> u64 {
        self.decodes + self.lazy_decodes.load(Ordering::Relaxed)
    }

    /// Bring the cache in line with `db`'s manifest: decode segments it has
    /// not seen, forget segments the manifest no longer lists, and read the
    /// WAL.
    pub fn refresh(&mut self, db: &Database) -> Result<Refreshed> {
        self.refresh_since(db, None)
    }

    /// As [`Self::refresh`], but a segment whose newest `observed_at` is
    /// before `since` is neither decoded nor listed — the manifest's zone
    /// map decides, so a year of history costs nothing to a reader that
    /// serves the last two weeks. A segment straddling `since` is kept
    /// whole. Cached segments that fell out of the window are dropped.
    pub fn refresh_since(&mut self, db: &Database, since: Option<Timestamp>) -> Result<Refreshed> {
        self.refresh_within(db, since, None)
    }

    /// As [`Self::refresh_since`], holding at most `max_rows` segment rows:
    /// the newest segments of the window, whole, until the next would go
    /// over the budget (the newest one always counts). Memory per resident
    /// row is a known constant; this makes the resident history a bound
    /// rather than a hope when one device writes far more than another.
    /// `Refreshed::budget_since` says where the held history starts.
    pub fn refresh_within(
        &mut self,
        db: &Database,
        since: Option<Timestamp>,
        max_rows: Option<u64>,
    ) -> Result<Refreshed> {
        self.refresh_impl(db, since, max_rows, true)
    }

    /// As [`Self::refresh`] without decoding anything: the manifest is read
    /// and the WAL copied, segments are listed and decode (all columns, or a
    /// few, or the rows of a scope) when something asks. For a reader that
    /// may need only counts, or only one project of many.
    pub fn list(&mut self, db: &Database) -> Result<Refreshed> {
        self.list_within(db, None, None)
    }

    /// As [`Self::list`] with the window and row budget of
    /// [`Self::refresh_within`].
    pub fn list_within(
        &mut self,
        db: &Database,
        since: Option<Timestamp>,
        max_rows: Option<u64>,
    ) -> Result<Refreshed> {
        self.refresh_impl(db, since, max_rows, false)
    }

    /// Decoding a segment that a compaction deleted after the manifest was
    /// read fails with "no such file"; the manifest has moved on. Start over
    /// from a fresh read-only handle (new manifest, new WAL copy), up to
    /// [`READ_ATTEMPTS`] times, instead of failing a reader that only lost a
    /// race.
    fn refresh_impl(
        &mut self,
        db: &Database,
        since: Option<Timestamp>,
        max_rows: Option<u64>,
        eager: bool,
    ) -> Result<Refreshed> {
        self.refreshes += 1;
        let mut reopened: Option<Database> = None;
        let mut generation = db.manifest().generation;
        let mut missing = String::new();
        retry_vanished(|attempt| {
            if attempt > 0 {
                let fresh = Database::open(
                    db.root(),
                    OpenOptions {
                        read_only: true,
                        keys: db.key_provider().cloned(),
                        ..Default::default()
                    },
                )?;
                // A compaction or flush moved the manifest on. If it did not,
                // the file is missing for another reason, and a fresh open
                // would fall back to an older generation that hides events;
                // say so instead of serving that.
                if fresh.manifest().generation <= generation {
                    return Err(StorageError::Other(format!(
                        "manifest generation {generation} lists a segment that is missing from segments/ and no newer generation replaced it (deleted by hand? `attempt repair` can check): {missing}"
                    )));
                }
                generation = fresh.manifest().generation;
                reopened = Some(fresh);
            }
            let handle = reopened.as_ref().unwrap_or(db);
            let out = self.refresh_once(handle, since, max_rows, eager);
            if let Err(e) = &out
                && e.is_not_found()
            {
                missing = e.to_string();
            }
            out
        })
    }

    fn refresh_once(
        &mut self,
        db: &Database,
        since: Option<Timestamp>,
        max_rows: Option<u64>,
        eager: bool,
    ) -> Result<Refreshed> {
        let manifest = db.manifest();
        let in_window = |s: &SegmentMeta| since.is_none_or(|t| s.max_observed_at >= t);
        let mut budget_since = None;
        let mut listed: std::collections::HashSet<Uuid> = manifest
            .segments
            .iter()
            .filter(|s| in_window(s))
            .map(|s| s.segment_id)
            .collect();
        if let Some(budget) = max_rows {
            let mut held = 0u64;
            let mut cut = false;
            // Manifest order is chronological: walk it newest first.
            for s in manifest.segments.iter().rev().filter(|s| in_window(s)) {
                if cut || (held > 0 && held + s.rows > budget) {
                    cut = true;
                    listed.remove(&s.segment_id);
                    continue;
                }
                held += s.rows;
                budget_since = Some(s.min_observed_at);
            }
            if !cut {
                budget_since = None;
            }
        }
        let dir = segment::segments_dir(db.root());
        // Read before touching the cache: a segment that vanishes mid-way
        // leaves it exactly as it was, so the retry starts clean.
        let mut decoded: HashMap<Uuid, Vec<RecordBatch>> = HashMap::new();
        if eager {
            for seg in &manifest.segments {
                if listed.contains(&seg.segment_id) && !self.segments.contains_key(&seg.segment_id)
                {
                    decoded.insert(
                        seg.segment_id,
                        segment::read_segment_batches(&dir.join(&seg.file))?,
                    );
                }
            }
        }
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
            budget_since,
            blobs: db.blob_store().clone(),
            keys: db.key_provider().cloned(),
            failures: Arc::new(Mutex::new(Vec::new())),
        };
        for seg in &manifest.segments {
            if !listed.contains(&seg.segment_id) {
                continue;
            }
            if let Some(cached) = self.segments.get(&seg.segment_id) {
                out.segments.push(Arc::clone(cached));
                continue;
            }
            let resident = decoded.remove(&seg.segment_id);
            if resident.is_some() {
                self.decodes += 1;
            }
            let cached = Arc::new(CachedSegment::new(
                seg.clone(),
                dir.join(&seg.file),
                resident,
                Arc::clone(&self.lazy_decodes),
            ));
            self.segments.insert(seg.segment_id, Arc::clone(&cached));
            out.new_segments.push(seg.segment_id);
            out.segments.push(cached);
        }
        out.memtable = db.memtable_events().to_vec();
        Ok(out)
    }

    /// Drop everything (a different database, or a snapshot).
    pub fn clear(&mut self) {
        self.segments.clear();
    }
}
