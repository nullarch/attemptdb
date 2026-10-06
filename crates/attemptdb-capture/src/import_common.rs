//! Plumbing shared by the history importers (Claude Code transcripts, Codex
//! rollouts): where the events are written, and how a run is bounded.
//!
//! **Where.** An importer used to open the database as the writer and fail
//! with a generic error when the daemon already held the writer lock. It now
//! asks [`open_import_target`] instead: the writer when it is free (events
//! are ingested and flushed into a segment, with exact accepted/duplicate
//! counts), and otherwise the **spool**, exactly like a hook: the daemon
//! that holds the lock imports it within seconds, ids make a repeat a no-op
//! there, and the importer reports the events as *queued* rather than
//! stored. A spool file is read back into memory whole by whoever imports
//! it, so [`SpoolSink`] paces itself: once the inbox passes a high-water
//! mark it waits (bounded) for the daemon to claim it before writing more.
//!
//! **How much.** [`BudgetOptions`] / [`pick_within_budget`] choose which
//! files a bounded run (`attempt setup`'s history step, `--days`,
//! `--max-bytes`) reads: only files modified inside the window, newest
//! first, as many as fit the byte budget. Both use file metadata only.

use crate::config::DeviceRecord;
use crate::ingest;
use crate::locator::Locator;
use crate::{CaptureError, Result};
use attemptdb_core::{DeviceId, Event, Timestamp};
use attemptdb_storage::spool::INBOX_FILE;
use attemptdb_storage::{Database, SpoolWriter, StorageError};
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Events per `ingest` call / spool append.
pub const INGEST_BATCH: usize = 500;

/// Approximate encoded bytes per batch before it is written anyway.
pub const BATCH_BYTES: usize = 8 * 1024 * 1024;

/// Inbox size above which a [`SpoolSink`] waits for the daemon to claim it.
pub const SPOOL_HIGH_WATER: u64 = 32 * 1024 * 1024;

/// Longest a [`SpoolSink`] waits for the inbox to drain, once. After that it
/// stops waiting for the rest of the run: nobody is draining.
pub const SPOOL_DRAIN_WAIT: Duration = Duration::from_secs(20);

/// What one write did. `queued` events went to the spool: the receiver
/// counts accepted and duplicates when it imports them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Written {
    pub accepted: usize,
    pub duplicates: usize,
    pub queued: usize,
}

/// Where an import writes events.
pub trait EventSink {
    /// Store (or queue) a batch.
    fn write(&mut self, events: Vec<Event>) -> Result<Written>;

    /// Make the run durable and visible: flush the memtable into a segment.
    /// A spool has nothing to flush: the importer of the spool does it.
    fn finish(&mut self) -> Result<()>;

    /// Whether events are queued rather than stored.
    fn is_spool(&self) -> bool;
}

/// Events written since the last flush after which a [`DbSink`] flushes the
/// memtable into a segment. Below the engine's own threshold (20 000 events)
/// on purpose: a flush copies the memtable, and imported events carry whole
/// tool outputs, so a smaller memtable keeps an import's memory flat. The
/// segments are merged by compaction later.
pub const FLUSH_EVERY_EVENTS: usize = 5_000;

/// An open database writer.
pub struct DbSink<'a> {
    db: &'a mut Database,
    since_flush: usize,
}

impl<'a> DbSink<'a> {
    pub fn new(db: &'a mut Database) -> Self {
        Self { db, since_flush: 0 }
    }
}

/// Ingest a batch, flushing once enough has accumulated.
fn write_direct(db: &mut Database, since_flush: &mut usize, events: Vec<Event>) -> Result<Written> {
    let r = db.ingest(events)?;
    *since_flush += r.accepted;
    if *since_flush >= FLUSH_EVERY_EVENTS {
        db.flush()?;
        *since_flush = 0;
    }
    Ok(Written {
        accepted: r.accepted,
        duplicates: r.duplicates,
        queued: 0,
    })
}

impl EventSink for DbSink<'_> {
    fn write(&mut self, events: Vec<Event>) -> Result<Written> {
        write_direct(self.db, &mut self.since_flush, events)
    }

    fn finish(&mut self) -> Result<()> {
        self.db.flush()?;
        Ok(())
    }

    fn is_spool(&self) -> bool {
        false
    }
}

/// The spool the daemon (or the next CLI command) drains.
pub struct SpoolSink {
    writer: SpoolWriter,
    inbox: PathBuf,
    high_water: u64,
    drain_wait: Duration,
    given_up: bool,
}

impl SpoolSink {
    pub fn new(locator: &Locator) -> Result<Self> {
        Self::with_limits(locator, SPOOL_HIGH_WATER, SPOOL_DRAIN_WAIT)
    }

    pub fn with_limits(locator: &Locator, high_water: u64, drain_wait: Duration) -> Result<Self> {
        let writer = SpoolWriter::new(&locator.db_dir)?;
        let inbox = SpoolWriter::dir(&locator.db_dir).join(INBOX_FILE);
        Ok(Self {
            writer,
            inbox,
            high_water,
            drain_wait,
            given_up: false,
        })
    }

    fn inbox_len(&self) -> u64 {
        std::fs::metadata(&self.inbox).map(|m| m.len()).unwrap_or(0)
    }

    /// After a large append, give the daemon a moment to claim the inbox so
    /// it never has to read the whole run into memory at once.
    fn pace(&mut self) {
        if self.given_up {
            return;
        }
        let before = self.inbox_len();
        if before < self.high_water {
            return;
        }
        let deadline = Instant::now() + self.drain_wait;
        while Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
            if self.inbox_len() < before {
                return;
            }
        }
        self.given_up = true;
    }
}

impl EventSink for SpoolSink {
    fn write(&mut self, events: Vec<Event>) -> Result<Written> {
        let queued = events.len();
        if queued == 0 {
            return Ok(Written::default());
        }
        // Not fsynced, like a hook: the spool is a transport, and the WAL of
        // whoever imports it is the durability boundary.
        self.writer.append_with(&events, false)?;
        self.pace();
        Ok(Written {
            queued,
            ..Written::default()
        })
    }

    fn finish(&mut self) -> Result<()> {
        Ok(())
    }

    fn is_spool(&self) -> bool {
        true
    }
}

/// The target of one import run: the writer when it is free, the spool when
/// another process holds it.
pub enum ImportTarget {
    Direct {
        db: Box<Database>,
        since_flush: usize,
    },
    Spool(SpoolSink),
}

impl ImportTarget {
    pub fn is_spool(&self) -> bool {
        matches!(self, ImportTarget::Spool(_))
    }
}

impl EventSink for ImportTarget {
    fn write(&mut self, events: Vec<Event>) -> Result<Written> {
        match self {
            ImportTarget::Direct { db, since_flush } => write_direct(db, since_flush, events),
            ImportTarget::Spool(s) => s.write(events),
        }
    }

    fn finish(&mut self) -> Result<()> {
        match self {
            ImportTarget::Direct { db, .. } => DbSink::new(db).finish(),
            ImportTarget::Spool(s) => s.finish(),
        }
    }

    fn is_spool(&self) -> bool {
        ImportTarget::is_spool(self)
    }
}

/// Open the place an import writes: the database writer, or the spool when
/// the writer lock is held (the daemon is running). The database must exist.
/// Anything else that stops the writer from opening is an error.
pub fn open_import_target(locator: &Locator) -> Result<ImportTarget> {
    match ingest::open_writer(locator, false) {
        Ok(mut db) => {
            // Hooks may have spooled events while nobody held the lock; take
            // them first so source order stays arrival order.
            db.import_spool()?;
            Ok(ImportTarget::Direct {
                db: Box::new(db),
                since_flush: 0,
            })
        }
        Err(CaptureError::Storage(StorageError::Locked(_))) => {
            Ok(ImportTarget::Spool(SpoolSink::new(locator)?))
        }
        Err(e) => Err(e),
    }
}

/// The device id events must carry: the writer's own, or — when only the
/// spool is available — the database's, read without taking the lock.
pub fn import_device(locator: &Locator, target: &ImportTarget) -> Result<DeviceId> {
    match target {
        ImportTarget::Direct { db, .. } => Ok(db.device_id()),
        ImportTarget::Spool(_) => match ingest::open_reader(locator) {
            Ok(db) => Ok(db.device_id()),
            Err(_) => Ok(DeviceRecord::load_or_create(&locator.paths.data_dir)?.device_id),
        },
    }
}

/// Collects events and writes them to a sink in bounded batches (by count
/// and by approximate size), adding up what the sink reports.
pub struct Batcher<'a> {
    sink: &'a mut dyn EventSink,
    batch: Vec<Event>,
    bytes: usize,
    pub total: Written,
}

impl<'a> Batcher<'a> {
    pub fn new(sink: &'a mut dyn EventSink) -> Self {
        Self {
            sink,
            batch: Vec::with_capacity(INGEST_BATCH),
            bytes: 0,
            total: Written::default(),
        }
    }

    pub fn push(&mut self, event: Event) -> Result<()> {
        self.bytes += approx_bytes(&event);
        self.batch.push(event);
        if self.batch.len() >= INGEST_BATCH || self.bytes >= BATCH_BYTES {
            self.flush()?;
        }
        Ok(())
    }

    /// Write what is pending.
    pub fn flush(&mut self) -> Result<()> {
        self.bytes = 0;
        if self.batch.is_empty() {
            return Ok(());
        }
        let w = self.sink.write(std::mem::take(&mut self.batch))?;
        self.total.accepted += w.accepted;
        self.total.duplicates += w.duplicates;
        self.total.queued += w.queued;
        Ok(())
    }
}

/// A cheap size estimate of an event's encoded form: metadata plus the text
/// it carries (the tool input is bounded by the parsers).
fn approx_bytes(ev: &Event) -> usize {
    let mut n = 1024 + ev.provider_event_name.len();
    if let Some(c) = &ev.content {
        for s in [&c.prompt, &c.command, &c.message, &c.error]
            .into_iter()
            .flatten()
        {
            n += s.len();
        }
        if let Some(v) = &c.tool_output {
            n += v.as_str().map_or(256, str::len);
        }
        if let Some(v) = &c.tool_input {
            n += v.to_string().len();
        }
        for v in c.extra.values() {
            n += v.as_str().map_or(64, str::len);
        }
    }
    n
}

// ---------------------------------------------------------------------------
// Bounding a run
// ---------------------------------------------------------------------------

/// How a bounded run picks files: modified at or after `since`, newest
/// first, within `max_bytes` in total. `None` means unbounded.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BudgetOptions {
    pub since: Option<Timestamp>,
    pub max_bytes: Option<u64>,
}

/// The files a [`BudgetOptions`] kept and what it left out.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Picked<T> {
    /// Newest first.
    pub files: Vec<T>,
    /// Older than the window.
    pub skipped_old: usize,
    /// Newer than the window's start but not fitting the byte budget.
    pub skipped_over_budget: usize,
}

/// Choose files for a bounded run. `meta` gives a file's modification time
/// and size (file metadata, nothing is read). A file without a modification
/// time counts as old when a window is set. Files that do not fit the
/// remaining budget are skipped and the scan continues with smaller, older
/// ones, so one huge file cannot starve the run.
pub fn pick_within_budget<T>(
    mut files: Vec<T>,
    meta: impl Fn(&T) -> (Option<Timestamp>, u64),
    options: &BudgetOptions,
) -> Picked<T> {
    files.sort_by(|a, b| meta(b).0.cmp(&meta(a).0));
    let mut picked = Picked {
        files: Vec::new(),
        skipped_old: 0,
        skipped_over_budget: 0,
    };
    let mut used = 0u64;
    for f in files {
        let (modified, bytes) = meta(&f);
        if let Some(since) = options.since
            && modified.is_none_or(|m| m < since)
        {
            picked.skipped_old += 1;
            continue;
        }
        if let Some(max) = options.max_bytes
            && used.saturating_add(bytes) > max
        {
            picked.skipped_over_budget += 1;
            continue;
        }
        used = used.saturating_add(bytes);
        picked.files.push(f);
    }
    picked
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(days_ago: i64) -> Timestamp {
        Timestamp::from_micros(1_787_904_000_000_000 - days_ago * 86_400_000_000)
    }

    #[test]
    fn the_budget_keeps_the_newest_files_inside_the_window() {
        let files = vec![
            ("old", Some(ts(40)), 10u64),
            ("huge", Some(ts(1)), 900),
            ("newest", Some(ts(0)), 100),
            ("mid", Some(ts(5)), 300),
            ("small", Some(ts(10)), 50),
            ("undated", None, 5),
        ];
        let picked = pick_within_budget(
            files,
            |f| (f.1, f.2),
            &BudgetOptions {
                since: Some(ts(30)),
                max_bytes: Some(500),
            },
        );
        let names: Vec<&str> = picked.files.iter().map(|f| f.0).collect();
        assert_eq!(names, vec!["newest", "mid", "small"]);
        assert_eq!(picked.skipped_old, 2, "older than the window, and undated");
        assert_eq!(picked.skipped_over_budget, 1, "the huge file does not fit");

        let all = pick_within_budget(
            vec![("a", Some(ts(400)), 1u64), ("b", None, 1)],
            |f| (f.1, f.2),
            &BudgetOptions::default(),
        );
        assert_eq!(
            all.files.len(),
            2,
            "no bounds: everything, undated included"
        );
    }
}
