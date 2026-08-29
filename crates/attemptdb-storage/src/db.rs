//! The database handle: single writer, many readers.
//!
//! Life of an event:
//!
//! 1. A hook process appends it to the spool (or hands it to the daemon).
//! 2. The writer assigns `source_seq`, `hlc`, `ingested_at`, appends it to
//!    the WAL and syncs according to the durability policy. The event is
//!    now acknowledged.
//! 3. The event sits in the memtable until a flush writes an immutable
//!    segment, publishes a new manifest generation, and truncates the WAL.
//!
//! Recovery on open replays the manifest (newest valid generation) and the
//! WAL; ingestion is idempotent by event id, so replaying a WAL whose events
//! already reached a segment is harmless.

use crate::failpoint;
use crate::format::{BLOBS_DIR, IDENTITY_FILE, LOCK_FILE, MANIFEST_DIR, SEGMENTS_DIR};
use crate::identity::Identity;
use crate::manifest::{Manifest, SegmentMeta, Tombstone, WalState};
use crate::memtable::MemTable;
use crate::segment;
use crate::spool::SpoolReader;
use crate::wal::Wal;
use crate::{IoAt, Result, StorageError};
use arrow::array::RecordBatch;
use attemptdb_core::clock::HlcGenerator;
use attemptdb_core::{DeviceId, Event, EventId, EventKind, Hlc, ProjectId, SessionId, Timestamp};
use std::collections::{HashMap, HashSet};
use std::fs::OpenOptions as FsOpenOptions;
use std::path::{Path, PathBuf};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum DurabilityPolicy {
    /// fsync the WAL before acknowledging every ingest call (group commit
    /// happens naturally because callers batch spool imports).
    #[default]
    Strict,
    /// Do not fsync per ingest; sync on flush and close. Faster, and only
    /// weaker across a power loss (not across a process crash).
    Relaxed,
}

#[derive(Clone, Debug)]
pub struct OpenOptions {
    pub create: bool,
    pub read_only: bool,
    pub durability: DurabilityPolicy,
    /// Flush the memtable to a segment once it holds this many events.
    pub flush_events: usize,
    /// ... or this many encoded bytes.
    pub flush_bytes: usize,
    /// Device id to use when creating a new database.
    pub device_id: Option<DeviceId>,
}

impl Default for OpenOptions {
    fn default() -> Self {
        Self {
            create: false,
            read_only: false,
            durability: DurabilityPolicy::Strict,
            flush_events: 5_000,
            flush_bytes: 8 * 1024 * 1024,
            device_id: None,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct IngestReport {
    pub accepted: usize,
    pub duplicates: usize,
    pub bytes: usize,
    pub flushed_segments: usize,
    pub spool_files: usize,
    pub undecodable: usize,
}

impl IngestReport {
    fn merge(&mut self, o: IngestReport) {
        self.accepted += o.accepted;
        self.duplicates += o.duplicates;
        self.bytes += o.bytes;
        self.flushed_segments += o.flushed_segments;
        self.spool_files += o.spool_files;
        self.undecodable += o.undecodable;
    }
}

/// Filter for `Database::scan`. All conditions are ANDed.
#[derive(Clone, Debug, Default)]
pub struct ScanFilter {
    pub project_id: Option<ProjectId>,
    pub session_id: Option<SessionId>,
    pub since: Option<Timestamp>,
    pub until: Option<Timestamp>,
    pub providers: Vec<String>,
    pub kinds: Vec<EventKind>,
    /// Keep only the newest `limit` events (by hlc/seq) after filtering.
    pub limit: Option<usize>,
    /// Drop events reconstructed from transcripts (`attrs.reconstructed`),
    /// keeping only what hooks captured.
    pub captured_only: bool,
}

impl ScanFilter {
    fn matches(&self, ev: &Event) -> bool {
        if self.project_id.is_some_and(|p| ev.project.project_id != p) {
            return false;
        }
        if self.session_id.is_some_and(|s| ev.session_id != s) {
            return false;
        }
        if self.since.is_some_and(|t| ev.observed_at < t) {
            return false;
        }
        if self.until.is_some_and(|t| ev.observed_at > t) {
            return false;
        }
        if !self.providers.is_empty() && !self.providers.iter().any(|p| p == ev.provider.as_str()) {
            return false;
        }
        if !self.kinds.is_empty() && !self.kinds.contains(&ev.kind) {
            return false;
        }
        if self.captured_only && ev.attrs.get("reconstructed").and_then(|v| v.as_bool()) == Some(true) {
            return false;
        }
        true
    }

    fn segment_may_match(&self, seg: &SegmentMeta) -> bool {
        if self.since.is_some_and(|t| seg.max_observed_at < t) {
            return false;
        }
        if self.until.is_some_and(|t| seg.min_observed_at > t) {
            return false;
        }
        if self.project_id.is_some_and(|p| !seg.project_ids.is_empty() && !seg.project_ids.contains(&p)) {
            return false;
        }
        if !self.providers.is_empty()
            && !seg.providers.is_empty()
            && !seg.providers.iter().any(|p| self.providers.contains(p))
        {
            return false;
        }
        true
    }
}

#[derive(Clone, Debug, Default)]
pub struct DbStats {
    pub generation: u64,
    pub segments: usize,
    pub segment_rows: u64,
    pub segment_bytes: u64,
    pub memtable_rows: usize,
    pub wal_bytes: u64,
    pub last_source_seq: u64,
    pub last_hlc: Hlc,
    pub spool_pending: bool,
}

pub struct Database {
    root: PathBuf,
    identity: Identity,
    manifest: Manifest,
    wal: Option<Wal>,
    memtable: MemTable,
    hlc: HlcGenerator,
    next_seq: u64,
    opts: OpenOptions,
    _lock: Option<std::fs::File>,
    /// Lazily loaded id sets per segment, used for deduplication.
    segment_ids: HashMap<Uuid, HashSet<EventId>>,
    /// Recovery notes worth surfacing to the user.
    pub warnings: Vec<String>,
}

impl Database {
    pub fn exists(root: &Path) -> bool {
        Identity::path(root).exists()
    }

    /// Create a new empty database directory.
    pub fn create(root: &Path, device_id: DeviceId) -> Result<()> {
        std::fs::create_dir_all(root).at(root)?;
        if Self::exists(root) {
            return Err(StorageError::Other(format!(
                "database already exists at {}",
                root.display()
            )));
        }
        for d in [SEGMENTS_DIR, BLOBS_DIR, crate::format::WAL_DIR, crate::format::MANIFEST_DIR, crate::format::SPOOL_DIR] {
            let p = root.join(d);
            std::fs::create_dir_all(&p).at(&p)?;
        }
        let identity = Identity::new(device_id);
        let mut manifest = Manifest::initial(identity.db_id, device_id);
        manifest.generation = 1;
        manifest.write(root)?;
        identity.write(root)?;
        crate::wal::sync_dir(root)?;
        Ok(())
    }

    pub fn open(root: &Path, opts: OpenOptions) -> Result<Self> {
        if !Self::exists(root) {
            if opts.create {
                let device_id = opts.device_id.unwrap_or_default();
                Self::create(root, device_id)?;
            } else {
                return Err(StorageError::NotADatabase(root.to_path_buf()));
            }
        }
        let identity = Identity::load(root)?;
        let lock = if opts.read_only {
            None
        } else {
            let lock_path = root.join(LOCK_FILE);
            let f = FsOpenOptions::new()
                .create(true)
                .write(true)
                .truncate(false)
                .open(&lock_path)
                .at(&lock_path)?;
            match f.try_lock() {
                Ok(()) => {}
                Err(std::fs::TryLockError::WouldBlock) => {
                    return Err(StorageError::Locked(root.to_path_buf()));
                }
                Err(std::fs::TryLockError::Error(e)) => return Err(StorageError::io(&lock_path, e)),
            }
            Some(f)
        };
        let mut warnings = Vec::new();
        if !opts.read_only {
            // Holding the lock, so nobody is mid-publish: every `.tmp` left
            // in the database is the remains of an interrupted atomic write.
            remove_stale_temp_files(root, &mut warnings)?;
        }
        let manifest = match Manifest::load_latest(root)? {
            Some((m, w)) => {
                warnings.extend(w);
                m
            }
            None => {
                let mut m = Manifest::initial(identity.db_id, identity.device_id);
                m.generation = 1;
                m
            }
        };
        note_unreferenced_segments(root, &manifest, &mut warnings)?;

        let mut db = Self {
            root: root.to_path_buf(),
            identity,
            hlc: HlcGenerator::resume_from(manifest.last_hlc),
            next_seq: manifest.last_source_seq + 1,
            manifest,
            wal: None,
            memtable: MemTable::new(),
            opts,
            _lock: lock,
            segment_ids: HashMap::new(),
            warnings,
        };

        // Replay the WAL. Read-only readers replay too (into memory) so they
        // see acknowledged events, but never truncate, rotate, or create
        // files: they scan without opening a writer.
        let (wal, recovery) = if db.opts.read_only {
            (None, Wal::scan(root)?)
        } else {
            let (w, r) = Wal::open(root)?;
            (Some(w), r)
        };
        if !recovery.truncated_files.is_empty() {
            db.warnings.push(format!(
                "recovered {} WAL file(s) with a torn tail",
                recovery.truncated_files.len()
            ));
        }
        if recovery.undecodable_records > 0 {
            db.warnings.push(format!(
                "{} WAL record(s) could not be decoded and were skipped",
                recovery.undecodable_records
            ));
        }
        for ev in recovery.events {
            if db.is_known(&ev.event_id)? {
                continue;
            }
            if ev.source_seq >= db.next_seq {
                db.next_seq = ev.source_seq + 1;
            }
            if ev.hlc > db.hlc.last() {
                db.hlc = HlcGenerator::resume_from(ev.hlc);
            }
            let len = attemptdb_core::codec::encode_event(&ev)?.len();
            db.memtable.push(ev, len);
        }
        db.wal = wal;
        Ok(db)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn identity(&self) -> &Identity {
        &self.identity
    }

    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    pub fn device_id(&self) -> DeviceId {
        self.identity.device_id
    }

    pub fn is_read_only(&self) -> bool {
        self.opts.read_only
    }

    fn require_writer(&self) -> Result<()> {
        if self.opts.read_only {
            return Err(StorageError::Other("database opened read-only".into()));
        }
        Ok(())
    }

    /// Whether an event id is already stored (memtable or any segment).
    pub fn is_known(&mut self, id: &EventId) -> Result<bool> {
        if self.memtable.contains(id) {
            return Ok(true);
        }
        let candidates: Vec<(Uuid, String)> = self
            .manifest
            .segments
            .iter()
            .filter(|s| s.min_event_id <= *id && *id <= s.max_event_id)
            .map(|s| (s.segment_id, s.file.clone()))
            .collect();
        for (seg_id, file) in candidates {
            if !self.segment_ids.contains_key(&seg_id) {
                let path = segment::segments_dir(&self.root).join(&file);
                let ids = segment::read_segment_event_ids(&path)?;
                self.segment_ids.insert(seg_id, ids.into_iter().collect());
            }
            if self.segment_ids[&seg_id].contains(id) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Ingest events: dedupe, assign ordering, append to the WAL, sync per
    /// policy, and flush when thresholds are crossed.
    pub fn ingest(&mut self, events: Vec<Event>) -> Result<IngestReport> {
        self.require_writer()?;
        let mut report = IngestReport::default();
        let mut batch: Vec<Event> = Vec::with_capacity(events.len());
        let now = Timestamp::now();
        let mut seen_in_batch = HashSet::new();
        for mut ev in events {
            if !seen_in_batch.insert(ev.event_id) || self.is_known(&ev.event_id)? {
                report.duplicates += 1;
                continue;
            }
            ev.source_seq = self.next_seq;
            self.next_seq += 1;
            ev.hlc = self.hlc.next(now);
            ev.ingested_at = Some(now);
            ev.apply_capture_mode();
            batch.push(ev);
        }
        if batch.is_empty() {
            return Ok(report);
        }
        let wal = self.wal.as_mut().expect("writer has a WAL");
        report.bytes = match wal.append(&batch) {
            Ok(bytes) => bytes,
            Err(e) => {
                // Nothing of this batch reached the file (the frame writer
                // discards a partially written batch), so the sequence
                // numbers it consumed are handed out again by the next
                // successful ingest instead of leaving a gap.
                self.next_seq = batch[0].source_seq;
                return Err(e);
            }
        };
        if self.opts.durability == DurabilityPolicy::Strict {
            wal.sync()?;
        }
        report.accepted = batch.len();
        let per_event = report.bytes / batch.len().max(1);
        for ev in batch {
            self.memtable.push(ev, per_event);
        }
        if (self.memtable.len() >= self.opts.flush_events || self.memtable.approx_bytes() >= self.opts.flush_bytes)
            && self.flush()?.is_some() {
                report.flushed_segments += 1;
            }
        Ok(report)
    }

    /// Claim every pending spool file, ingest it, and delete it once the
    /// events are durable.
    pub fn import_spool(&mut self) -> Result<IngestReport> {
        self.require_writer()?;
        let reader = SpoolReader::new(&self.root)?;
        let mut report = IngestReport::default();
        for claimed in reader.claim()? {
            report.spool_files += 1;
            report.undecodable += claimed.undecodable;
            if claimed.truncated {
                self.warnings.push(format!(
                    "spool file {} had a torn tail; valid prefix imported",
                    claimed.path.display()
                ));
            }
            let r = self.ingest(claimed.events.clone())?;
            report.merge(r);
            // Everything accepted is in the WAL now; make sure it is synced
            // before the spool file disappears even under Relaxed durability.
            if let Some(w) = self.wal.as_mut() {
                w.sync()?;
            }
            reader.release(&claimed)?;
        }
        Ok(report)
    }

    /// Flush the memtable into a new segment and publish a manifest
    /// generation. Returns the new segment's metadata, or `None` when there
    /// was nothing to flush.
    pub fn flush(&mut self) -> Result<Option<SegmentMeta>> {
        self.require_writer()?;
        if self.memtable.is_empty() {
            return Ok(None);
        }
        let wal = self.wal.as_mut().expect("writer has a WAL");
        wal.sync()?;
        // Work on a copy: the memtable keeps serving reads and deduplication
        // until the new generation is durable, so a failed segment or
        // manifest write (disk full) leaves the database exactly as it was.
        let mut events = self.memtable.events().to_vec();
        events.sort_by_key(|e| e.source_seq);
        let meta = segment::write_segment(&self.root, &events)?;

        // Rotate the WAL first so the manifest can record that every event
        // in files below the new active number is contained in segments.
        wal.rotate()?;
        let mut next = self.manifest.clone();
        next.generation += 1;
        next.created_at = Timestamp::now();
        next.last_hlc = self.hlc.last();
        next.last_source_seq = self.next_seq - 1;
        next.wal = WalState { active_file: wal.active_number(), checkpoint_offset: 0 };
        next.segments.push(meta.clone());
        next.write(&self.root)?;
        self.manifest = next;
        self.segment_ids.insert(meta.segment_id, events.iter().map(|e| e.event_id).collect());
        self.memtable.drain();
        failpoint::hit(failpoint::FLUSH_AFTER_MANIFEST_BEFORE_WAL_TRUNCATE);

        // Only now is it safe to drop the older WAL files.
        wal.truncate_before(wal.active_number())?;
        Ok(Some(meta))
    }

    /// Scan events matching `filter`, sorted by `(hlc, source_seq)`.
    pub fn scan(&self, filter: &ScanFilter) -> Result<Vec<Event>> {
        let mut out = Vec::new();
        for seg in &self.manifest.segments {
            if !filter.segment_may_match(seg) {
                continue;
            }
            let path = segment::segments_dir(&self.root).join(&seg.file);
            for ev in segment::read_segment_events(&path)? {
                if filter.matches(&ev) {
                    out.push(ev);
                }
            }
        }
        for ev in self.memtable.events() {
            if filter.matches(ev) {
                out.push(ev.clone());
            }
        }
        out.sort_by(|a, b| (a.hlc, a.source_seq).cmp(&(b.hlc, b.source_seq)));
        if let Some(limit) = filter.limit
            && out.len() > limit {
                out.drain(..out.len() - limit);
            }
        Ok(out)
    }

    /// All events as Arrow batches (segments pruned by `filter`, memtable as
    /// one trailing batch). Row-level filtering is left to the query engine.
    pub fn batches(&self, filter: &ScanFilter) -> Result<Vec<RecordBatch>> {
        let mut out = Vec::new();
        for seg in &self.manifest.segments {
            if !filter.segment_may_match(seg) {
                continue;
            }
            let path = segment::segments_dir(&self.root).join(&seg.file);
            out.extend(segment::read_segment_batches(&path)?);
        }
        if !self.memtable.is_empty() {
            out.push(segment::events_to_batch(self.memtable.events())?);
        }
        Ok(out)
    }

    pub fn stats(&self) -> DbStats {
        DbStats {
            generation: self.manifest.generation,
            segments: self.manifest.segments.len(),
            segment_rows: self.manifest.segments.iter().map(|s| s.rows).sum(),
            segment_bytes: self.manifest.segments.iter().map(|s| s.bytes).sum(),
            memtable_rows: self.memtable.len(),
            wal_bytes: self.wal.as_ref().map(|w| w.active_len()).unwrap_or(0),
            last_source_seq: self.next_seq.saturating_sub(1),
            last_hlc: self.hlc.last(),
            spool_pending: SpoolReader::new(&self.root).map(|r| r.has_pending()).unwrap_or(false),
        }
    }

    /// Verify manifests, segments, and WAL. Returns human-readable problems.
    pub fn verify(&self) -> Result<Vec<String>> {
        let mut problems = Vec::new();
        for seg in &self.manifest.segments {
            if let Err(e) = segment::verify_segment(&self.root, seg) {
                problems.push(e.to_string());
            }
        }
        let recovery = Wal::scan(&self.root)?;
        for f in recovery.truncated_files {
            problems.push(format!("WAL file {} has a torn tail", f.display()));
        }
        if recovery.undecodable_records > 0 {
            problems.push(format!("{} undecodable WAL record(s)", recovery.undecodable_records));
        }
        Ok(problems)
    }

    /// Remove tombstoned files whose generation is older than the current
    /// one (no readers can reference them any more once the newer
    /// generation is durable and the process holding them has closed).
    pub fn collect_garbage(&mut self) -> Result<usize> {
        self.require_writer()?;
        let current = self.manifest.generation;
        let mut removed = 0;
        let mut keep: Vec<Tombstone> = Vec::new();
        for t in self.manifest.tombstones.clone() {
            if t.since_generation < current {
                let p = segment::segments_dir(&self.root).join(&t.file);
                if p.exists() {
                    std::fs::remove_file(&p).at(&p)?;
                }
                removed += 1;
            } else {
                keep.push(t);
            }
        }
        if removed > 0 {
            self.manifest.tombstones = keep;
        }
        Ok(removed)
    }

    /// Flush and release the lock.
    pub fn close(mut self) -> Result<()> {
        if !self.opts.read_only {
            self.flush()?;
        }
        Ok(())
    }
}

/// Delete the `.tmp` files an interrupted atomic write leaves in the
/// segment and manifest directories (and the identity temp file). They are
/// never referenced by anything: a manifest only names a segment after the
/// rename, and a generation file only exists after its own rename.
fn remove_stale_temp_files(root: &Path, warnings: &mut Vec<String>) -> Result<()> {
    for dir in [root.join(SEGMENTS_DIR), root.join(MANIFEST_DIR)] {
        if !dir.exists() {
            continue;
        }
        for entry in std::fs::read_dir(&dir).at(&dir)? {
            let path = entry.at(&dir)?.path();
            if path.extension().and_then(|e| e.to_str()) == Some("tmp") && path.is_file() {
                std::fs::remove_file(&path).at(&path)?;
                warnings.push(format!("removed stale temp file {}", path.display()));
            }
        }
    }
    let identity_tmp = root.join(format!("{IDENTITY_FILE}.tmp"));
    if identity_tmp.is_file() {
        std::fs::remove_file(&identity_tmp).at(&identity_tmp)?;
        warnings.push(format!("removed stale temp file {}", identity_tmp.display()));
    }
    Ok(())
}

/// Warn about segment files the selected generation does not reference.
/// They come from a crash between publishing a segment and publishing the
/// manifest that names it (harmless: the WAL still holds those events), or
/// from a newer generation that was rejected as corrupt (then they hold
/// events no longer visible). Either way they are left in place for repair.
fn note_unreferenced_segments(root: &Path, manifest: &Manifest, warnings: &mut Vec<String>) -> Result<()> {
    let dir = segment::segments_dir(root);
    if !dir.exists() {
        return Ok(());
    }
    let referenced: HashSet<&str> = manifest
        .segments
        .iter()
        .map(|s| s.file.as_str())
        .chain(manifest.tombstones.iter().map(|t| t.file.rsplit('/').next().unwrap_or(&t.file)))
        .collect();
    let mut orphans = Vec::new();
    for entry in std::fs::read_dir(&dir).at(&dir)? {
        let name = entry.at(&dir)?.file_name().to_string_lossy().to_string();
        if name.starts_with("seg-") && name.ends_with(".arrow") && !referenced.contains(name.as_str()) {
            orphans.push(name);
        }
    }
    orphans.sort();
    for name in orphans {
        warnings.push(format!(
            "unreferenced segment file {name} (not in manifest generation {}); left in place",
            manifest.generation
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spool::SpoolWriter;
    use attemptdb_core::event::Provider;
    use attemptdb_core::{CaptureMode, ProjectRef};

    fn ev(dev: DeviceId, i: u32) -> Event {
        let mut e = Event::new(
            dev,
            Provider::ClaudeCode,
            "PostToolUse",
            EventKind::ToolCallFinished,
            ProjectRef::derive("/p", None, &dev),
            "s1",
            CaptureMode::LocalSemantic,
            "t",
        );
        e.attrs.insert("i".into(), serde_json::json!(i));
        e
    }

    #[test]
    fn create_ingest_flush_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("db.attemptdb");
        let dev = DeviceId::new();
        let mut db = Database::open(
            &root,
            OpenOptions { create: true, device_id: Some(dev), flush_events: 100, ..Default::default() },
        )
        .unwrap();
        let events: Vec<Event> = (0..10).map(|i| ev(dev, i)).collect();
        let r = db.ingest(events.clone()).unwrap();
        assert_eq!(r.accepted, 10);
        // Re-ingest = duplicates.
        let r = db.ingest(events.clone()).unwrap();
        assert_eq!(r.duplicates, 10);
        assert_eq!(r.accepted, 0);
        let all = db.scan(&ScanFilter::default()).unwrap();
        assert_eq!(all.len(), 10);
        assert!(all.iter().all(Event::is_ingested));
        assert_eq!(all[0].source_seq, 1);
        assert_eq!(all[9].source_seq, 10);
        let meta = db.flush().unwrap().unwrap();
        assert_eq!(meta.rows, 10);
        assert_eq!(db.stats().segments, 1);
        assert_eq!(db.stats().memtable_rows, 0);
        // Duplicate detection against the segment works after flush.
        let r = db.ingest(events.clone()).unwrap();
        assert_eq!(r.duplicates, 10);
        // Ingest more (left in WAL) then drop without flushing.
        db.ingest((10..15).map(|i| ev(dev, i)).collect()).unwrap();
        drop(db);
        let db = Database::open(&root, OpenOptions::default()).unwrap();
        let all = db.scan(&ScanFilter::default()).unwrap();
        assert_eq!(all.len(), 15);
        assert_eq!(db.stats().last_source_seq, 15);
        assert_eq!(db.stats().memtable_rows, 5);
        assert!(db.verify().unwrap().is_empty());
    }

    #[test]
    fn spool_import_is_idempotent_and_crash_safe() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("db.attemptdb");
        let dev = DeviceId::new();
        Database::create(&root, dev).unwrap();
        let writer = SpoolWriter::new(&root).unwrap();
        let events: Vec<Event> = (0..5).map(|i| ev(dev, i)).collect();
        writer.append(&events[..3]).unwrap();
        writer.append(&events[3..]).unwrap();
        // Simulate a crash after import but before release: import twice.
        let mut db = Database::open(&root, OpenOptions::default()).unwrap();
        let r = db.import_spool().unwrap();
        assert_eq!(r.accepted, 5);
        assert_eq!(r.spool_files, 1);
        writer.append(&events).unwrap(); // hooks re-sent the same events
        let r = db.import_spool().unwrap();
        assert_eq!(r.accepted, 0);
        assert_eq!(r.duplicates, 5);
        assert!(!db.stats().spool_pending);
    }

    #[test]
    fn second_writer_is_rejected_but_reader_is_fine() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("db.attemptdb");
        let _w = Database::open(&root, OpenOptions { create: true, ..Default::default() }).unwrap();
        assert!(matches!(
            Database::open(&root, OpenOptions::default()),
            Err(StorageError::Locked(_))
        ));
        let r = Database::open(&root, OpenOptions { read_only: true, ..Default::default() }).unwrap();
        assert!(r.is_read_only());
    }

    #[test]
    fn scan_filters_and_limit() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("db.attemptdb");
        let dev = DeviceId::new();
        let mut db = Database::open(&root, OpenOptions { create: true, device_id: Some(dev), ..Default::default() }).unwrap();
        let mut events: Vec<Event> = (0..6).map(|i| ev(dev, i)).collect();
        events[0].kind = EventKind::PromptSubmitted;
        events[1].provider = Provider::Codex;
        db.ingest(events).unwrap();
        db.flush().unwrap();
        let prompts = db.scan(&ScanFilter { kinds: vec![EventKind::PromptSubmitted], ..Default::default() }).unwrap();
        assert_eq!(prompts.len(), 1);
        let codex = db.scan(&ScanFilter { providers: vec!["codex".into()], ..Default::default() }).unwrap();
        assert_eq!(codex.len(), 1);
        let last2 = db.scan(&ScanFilter { limit: Some(2), ..Default::default() }).unwrap();
        assert_eq!(last2.len(), 2);
        assert_eq!(last2[1].source_seq, 6);
        let batches = db.batches(&ScanFilter::default()).unwrap();
        assert_eq!(batches.iter().map(|b| b.num_rows()).sum::<usize>(), 6);
    }

    #[test]
    fn torn_wal_recovers_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("db.attemptdb");
        let dev = DeviceId::new();
        {
            let mut db = Database::open(&root, OpenOptions { create: true, device_id: Some(dev), ..Default::default() }).unwrap();
            db.ingest((0..4).map(|i| ev(dev, i)).collect()).unwrap();
        }
        let wal = root.join("wal").join("000001.wal");
        let len = std::fs::metadata(&wal).unwrap().len();
        std::fs::OpenOptions::new().write(true).open(&wal).unwrap().set_len(len - 5).unwrap();
        let db = Database::open(&root, OpenOptions::default()).unwrap();
        assert_eq!(db.scan(&ScanFilter::default()).unwrap().len(), 3);
        assert!(!db.warnings.is_empty());
    }
}
