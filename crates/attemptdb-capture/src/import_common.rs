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
//! **What is already there.** A session that hooks captured live and that is
//! later reconstructed from its transcript would otherwise be stored twice:
//! hook events get fresh ids, transcript events ids derived from the
//! transcript. Tool calls are merged by id for free (both channels derive the
//! id from the provider's call id, see `attemptdb_adapters::common`); events
//! with no provider-named id (prompts, turn ends, session starts) are
//! reconciled here, by the importer, at write time: [`StoredIndex`] reads
//! what the database already holds about the sessions of a run, and
//! [`Reconciler`] decides, event by event, which reconstructed events say
//! nothing a hook has not already said. The rules are documented on
//! [`Reconciler`] and in `docs/history-import.md`.
//!
//! **How much.** [`BudgetOptions`] / [`pick_within_budget`] choose which
//! files a bounded run (`attempt setup`'s history step, `--days`,
//! `--max-bytes`) reads: only files modified inside the window, newest
//! first, as many as fit the byte budget. Both use file metadata only.

use crate::config::DeviceRecord;
use crate::ingest;
use crate::locator::Locator;
use crate::{CaptureError, Result};
use attemptdb_core::event::Provider;
use attemptdb_core::{DeviceId, Event, EventId, EventKind, ProjectId, ProjectRef, Timestamp};
use attemptdb_storage::segment::{self, Cols, col};
use attemptdb_storage::spool::INBOX_FILE;
use attemptdb_storage::{Database, SpoolWriter, StorageError};
use std::collections::{HashMap, HashSet};
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

    /// What the database already holds about `sessions` of `provider`
    /// (provider session ids), for reconciling a reconstruction with what
    /// hooks captured. A sink that cannot look (a test sink) returns an empty
    /// index, which makes the importer write everything; a sink that could
    /// not read the database says why, and the importer reports it.
    fn stored(
        &mut self,
        _provider: &Provider,
        _sessions: &HashSet<String>,
        _window: Window,
    ) -> std::result::Result<StoredIndex, String> {
        Ok(StoredIndex::default())
    }
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

    fn stored(
        &mut self,
        provider: &Provider,
        sessions: &HashSet<String>,
        window: Window,
    ) -> std::result::Result<StoredIndex, String> {
        StoredIndex::load(self.db, provider, sessions, window).map_err(|e| e.to_string())
    }
}

/// The spool the daemon (or the next CLI command) drains.
pub struct SpoolSink {
    writer: SpoolWriter,
    /// Boxed: a `Locator` would make the spool variant of [`ImportTarget`]
    /// many times larger than the writer variant.
    locator: Box<Locator>,
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
            locator: Box::new(locator.clone()),
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

    /// The writer lock is held by the daemon, but a read-only open needs no
    /// lock and replays the WAL, so it sees everything the daemon has
    /// acknowledged. Hook events still sitting in the spool inbox are not
    /// seen yet: the daemon drains it within seconds, and a hook event that
    /// has not landed by then is a hook event this run cannot reconcile with.
    fn stored(
        &mut self,
        provider: &Provider,
        sessions: &HashSet<String>,
        window: Window,
    ) -> std::result::Result<StoredIndex, String> {
        let db = ingest::open_reader(&self.locator).map_err(|e| e.to_string())?;
        StoredIndex::load(&db, provider, sessions, window).map_err(|e| e.to_string())
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

    fn stored(
        &mut self,
        provider: &Provider,
        sessions: &HashSet<String>,
        window: Window,
    ) -> std::result::Result<StoredIndex, String> {
        match self {
            ImportTarget::Direct { db, .. } => {
                StoredIndex::load(db, provider, sessions, window).map_err(|e| e.to_string())
            }
            ImportTarget::Spool(s) => s.stored(provider, sessions, window),
        }
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
// Reconciling a reconstruction with what hooks captured
// ---------------------------------------------------------------------------

/// How far apart (by `observed_at`) a hook-captured event and a reconstructed
/// one can be and still be the same real-world event. A hook fires a few
/// milliseconds around the moment the agent writes the same fact to its
/// transcript; ten seconds is generous for a loaded machine, and far below the
/// gap between two prompts, two turn ends or two compactions of one session
/// (each takes a human keystroke or a model round trip). Matching is by order,
/// nearest first, never by text: under `metadata_only` there is none.
pub const MATCH_TOLERANCE_MICROS: i64 = 10 * 1_000_000;

/// How far before a run's earliest transcript entry the lookup reads
/// segments: a `SessionStart` hook precedes the first entry of the
/// transcript by however long the person waited before typing.
pub const WINDOW_LEAD_MICROS: i64 = 7 * 86_400 * 1_000_000;

/// How far after a transcript's last write the lookup reads segments.
pub const WINDOW_TAIL_MICROS: i64 = 3_600 * 1_000_000;

/// The time range the lookup reads segments for (segment metadata only;
/// `None` is unbounded). It only prunes: a bound that is too wide costs time,
/// never correctness.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Window {
    pub since: Option<Timestamp>,
    pub until: Option<Timestamp>,
}

impl Window {
    /// The window covering files whose first entry is at `first` and whose
    /// last write is at `last`; unbounded on a side when any file lacks it.
    pub fn around(firsts: &[Option<Timestamp>], lasts: &[Option<Timestamp>]) -> Self {
        let since = firsts
            .iter()
            .copied()
            .collect::<Option<Vec<_>>>()
            .and_then(|v| v.into_iter().min())
            .map(|t| Timestamp::from_micros(t.as_micros().saturating_sub(WINDOW_LEAD_MICROS)));
        let until = lasts
            .iter()
            .copied()
            .collect::<Option<Vec<_>>>()
            .and_then(|v| v.into_iter().max())
            .map(|t| Timestamp::from_micros(t.as_micros().saturating_add(WINDOW_TAIL_MICROS)));
        Self { since, until }
    }
}

/// Kinds with no provider-named id that a reconstruction and a hook can both
/// produce for the same real-world event: they are reconciled by order.
/// (Tool calls are not in the list: they merge by id.)
const RECONCILED_KINDS: &[EventKind] = &[
    EventKind::SessionStarted,
    EventKind::SessionEnded,
    EventKind::PromptSubmitted,
    EventKind::AgentMessage,
    EventKind::TurnStopped,
    EventKind::SubagentStarted,
    EventKind::SubagentStopped,
    EventKind::CompactionFinished,
];

fn is_tool_kind(kind: EventKind) -> bool {
    matches!(
        kind,
        EventKind::ToolCallStarted | EventKind::ToolCallFinished | EventKind::ToolCallFailed
    )
}

/// A call's start and its end are different events; finished and failed are
/// one slot (see `attemptdb_adapters::common::derive_event_id`).
fn call_slot(kind: EventKind) -> char {
    if kind == EventKind::ToolCallStarted {
        's'
    } else {
        'e'
    }
}

fn call_key(kind: EventKind, call_id: &str) -> String {
    format!("{}:{call_id}", call_slot(kind))
}

/// One stored event, reduced to what reconciliation needs.
struct Seen {
    event_id: EventId,
    kind: EventKind,
    observed_at: Timestamp,
    reconstructed: bool,
    call_id: Option<String>,
}

/// What hooks captured for one session.
#[derive(Default)]
struct SessionFacts {
    /// The project identity of the earliest hook-captured event.
    project: Option<(Timestamp, ProjectRef)>,
    /// Hook-captured events of reconciled kinds: `(observed_at, matched)`.
    hooked: HashMap<EventKind, Vec<(Timestamp, bool)>>,
    /// Hook-captured tool calls, by [`call_key`].
    calls: HashSet<String>,
}

/// What a database already holds about some sessions of one provider.
/// Read-only, built once per run by [`StoredIndex::load`]; see
/// [`Reconciler`] for how it is used.
#[derive(Default)]
pub struct StoredIndex {
    /// Ids of reconstructed tool-call events (of any import version): a
    /// parser that now derives a different id for the same call reports the
    /// old one, and finding it here means an earlier import stored the call.
    reconstructed_tool_ids: HashSet<EventId>,
    sessions: HashMap<String, SessionFacts>,
}

impl StoredIndex {
    /// Read the events of `sessions` (provider session ids) of `provider`
    /// from `db`: segments pruned by provider and `window` from their
    /// metadata, each read once, batch by batch, keeping a few fields of the
    /// rows that belong to a wanted session; then the memtable. Memory is
    /// bounded by the largest segment plus what the wanted sessions hold.
    /// No row is decoded into an `Event`, no content is read.
    pub fn load(
        db: &Database,
        provider: &Provider,
        sessions: &HashSet<String>,
        window: Window,
    ) -> Result<Self> {
        let mut index = Self::default();
        if sessions.is_empty() {
            return Ok(index);
        }
        let provider_name = provider.as_str();
        for seg in &db.manifest().segments {
            if !seg.providers.is_empty() && !seg.providers.iter().any(|p| p == provider_name) {
                continue;
            }
            if window.since.is_some_and(|t| seg.max_observed_at < t)
                || window.until.is_some_and(|t| seg.min_observed_at > t)
            {
                continue;
            }
            let path = segment::segments_dir(db.root()).join(&seg.file);
            for batch in segment::read_segment_batches(&path)? {
                let cols = Cols::new(batch)?;
                for row in 0..cols.num_rows() {
                    let Some(session) = cols.str_ref(col::PROVIDER_SESSION_ID, row) else {
                        continue;
                    };
                    if !sessions.contains(session)
                        || cols.str_ref(col::PROVIDER, row) != Some(provider_name)
                    {
                        continue;
                    }
                    let Some(kind) = cols.str_ref(col::KIND, row).and_then(EventKind::parse) else {
                        continue;
                    };
                    let reconstructed = cols
                        .str_ref(col::ATTRS_JSON, row)
                        .is_some_and(attrs_say_reconstructed);
                    let seen = Seen {
                        event_id: EventId::from_bytes(
                            cols.fsb(col::EVENT_ID, row).unwrap_or([0; 16]),
                        ),
                        kind,
                        observed_at: cols.ts(col::OBSERVED_AT, row).unwrap_or_default(),
                        reconstructed,
                        call_id: cols.s(col::TOOL_CALL_ID, row),
                    };
                    index.note(session, seen, || ProjectRef {
                        project_id: ProjectId::from_bytes(
                            cols.fsb(col::PROJECT_ID, row).unwrap_or([0; 16]),
                        ),
                        root: cols.s(col::PROJECT_ROOT, row).unwrap_or_default(),
                        name: cols.s(col::PROJECT_NAME, row).unwrap_or_default(),
                        repo_remote: cols.s(col::REPO_REMOTE, row),
                        branch: None,
                        head: None,
                    });
                }
            }
        }
        for ev in db.memtable_events() {
            if ev.provider.as_str() != provider_name
                || !sessions.contains(ev.provider_session_id.as_str())
            {
                continue;
            }
            let seen = Seen {
                event_id: ev.event_id,
                kind: ev.kind,
                observed_at: ev.observed_at,
                reconstructed: ev.attrs.get("reconstructed").and_then(|v| v.as_bool())
                    == Some(true),
                call_id: ev.tool.as_ref().and_then(|t| t.call_id.clone()),
            };
            index.note(&ev.provider_session_id, seen, || ProjectRef {
                project_id: ev.project.project_id,
                root: ev.project.root.clone(),
                name: ev.project.name.clone(),
                repo_remote: ev.project.repo_remote.clone(),
                branch: None,
                head: None,
            });
        }
        Ok(index)
    }

    fn note(&mut self, session: &str, seen: Seen, project: impl FnOnce() -> ProjectRef) {
        if seen.reconstructed {
            if is_tool_kind(seen.kind) {
                self.reconstructed_tool_ids.insert(seen.event_id);
            }
            return;
        }
        // Telemetry (OTel) and other `unknown` observations are not hook
        // lifecycle events: they say nothing about what a transcript holds.
        if seen.kind == EventKind::Unknown {
            return;
        }
        let facts = self.sessions.entry(session.to_string()).or_default();
        if facts
            .project
            .as_ref()
            .is_none_or(|(at, _)| seen.observed_at < *at)
        {
            facts.project = Some((seen.observed_at, project()));
        }
        if is_tool_kind(seen.kind) {
            if let Some(call) = &seen.call_id {
                facts.calls.insert(call_key(seen.kind, call));
            }
        } else if RECONCILED_KINDS.contains(&seen.kind) {
            facts
                .hooked
                .entry(seen.kind)
                .or_default()
                .push((seen.observed_at, false));
        }
    }

    /// How many sessions have at least one hook-captured event.
    pub fn hooked_sessions(&self) -> usize {
        self.sessions.len()
    }
}

/// `attrs_json` says `reconstructed: true` (cheap substring test first).
fn attrs_say_reconstructed(attrs_json: &str) -> bool {
    attrs_json.contains("\"reconstructed\"")
        && serde_json::from_str::<serde_json::Value>(attrs_json)
            .ok()
            .and_then(|v| v.get("reconstructed").and_then(|b| b.as_bool()))
            == Some(true)
}

/// What a [`Reconciler`] left out of a run, and why.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Skipped {
    /// A tool call a hook captured (under whatever id), or an earlier import
    /// stored under the id an older version derived.
    pub calls: usize,
    /// A prompt, turn end, session start... a hook captured: matched by
    /// order within [`MATCH_TOLERANCE_MICROS`].
    pub matched: usize,
}

impl Skipped {
    pub fn total(&self) -> usize {
        self.calls + self.matched
    }
}

/// Decides, event by event, which reconstructed events say nothing that
/// hooks have not already said. It never removes anything from the database:
/// it only declines to write a reconstructed event whose real-world
/// counterpart is already stored.
///
/// For each event of a session the index knows hook-captured events of:
///
/// 1. **A tool call** is skipped when the session already holds a
///    hook-captured event for the same call id in the same slot (start, or
///    end whichever of finished/failed), whatever its id: hook events from
///    before hooks derived natural ids have random ones. A call whose id is
///    the one this version derives is also skipped when a previous import
///    stored it under the id an older version derived (`legacy`).
/// 2. **A prompt, turn end, session start, ... ** ([`RECONCILED_KINDS`]) is
///    skipped when a hook-captured event of the same kind in the same
///    session, within [`MATCH_TOLERANCE_MICROS`] and not yet matched, exists;
///    the nearest one is consumed, so two prompts match two hook prompts in
///    order and a third, which hooks missed, is imported. A session start
///    matches the nearest hook session start at any distance: the transcript
///    has no start of its own, only its first entry. A turn end that is a
///    user interruption never matches (Claude fires no `Stop` hook for one).
///
/// An event of a session with no hook-captured events is never skipped by
/// rule 2, and a lookup that failed leaves every rule inert: the run then
/// writes everything, which is the behaviour before reconciliation existed.
#[derive(Default)]
pub struct Reconciler {
    index: StoredIndex,
    skipped: Skipped,
}

impl Reconciler {
    pub fn new(index: StoredIndex) -> Self {
        Self {
            index,
            skipped: Skipped::default(),
        }
    }

    pub fn skipped(&self) -> Skipped {
        self.skipped
    }

    pub fn index(&self) -> &StoredIndex {
        &self.index
    }

    /// The project identity hooks recorded for `provider_session_id`, if any:
    /// root, name, remote and id, with no branch or head (those belong to a
    /// moment, and the reconstruction has its own).
    pub fn hooked_project(&self, provider_session_id: &str) -> Option<ProjectRef> {
        self.index
            .sessions
            .get(provider_session_id)
            .and_then(|f| f.project.as_ref())
            .map(|(_, p)| p.clone())
    }

    /// Whether `event` should not be written. `legacy` is the id an older
    /// parser derived for the same event (tool calls only).
    pub fn should_skip(&mut self, event: &Event, legacy: Option<EventId>) -> bool {
        if let Some(legacy) = legacy
            && self.index.reconstructed_tool_ids.contains(&legacy)
        {
            self.skipped.calls += 1;
            return true;
        }
        let Some(facts) = self.index.sessions.get_mut(&event.provider_session_id) else {
            return false;
        };
        if is_tool_kind(event.kind) {
            let call = event.tool.as_ref().and_then(|t| t.call_id.as_deref());
            if call.is_some_and(|c| facts.calls.contains(&call_key(event.kind, c))) {
                self.skipped.calls += 1;
                return true;
            }
            return false;
        }
        if !RECONCILED_KINDS.contains(&event.kind) || is_interruption(event) {
            return false;
        }
        let Some(hooked) = facts.hooked.get_mut(&event.kind) else {
            return false;
        };
        let any_distance = event.kind == EventKind::SessionStarted;
        let at = event.observed_at.as_micros();
        let nearest = hooked
            .iter_mut()
            .filter(|(t, matched)| {
                !*matched && (any_distance || (t.as_micros() - at).abs() <= MATCH_TOLERANCE_MICROS)
            })
            .min_by_key(|(t, _)| (t.as_micros() - at).abs());
        match nearest {
            Some(slot) => {
                slot.1 = true;
                self.skipped.matched += 1;
                true
            }
            None => false,
        }
    }
}

/// A turn end that is the person interrupting: it has no `Stop` hook.
fn is_interruption(event: &Event) -> bool {
    event.kind == EventKind::TurnStopped
        && event.attrs.get("reason").and_then(|v| v.as_str()) == Some("user_interrupt")
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
    use attemptdb_core::CaptureMode;

    const SEC: i64 = 1_000_000;
    const T0: i64 = 1_787_904_000 * SEC;

    fn event(kind: EventKind, session: &str, at_secs: f64) -> Event {
        let mut ev = Event::new(
            DeviceId::nil(),
            Provider::ClaudeCode,
            "t",
            kind,
            ProjectRef::default(),
            session,
            CaptureMode::LocalSemantic,
            "test",
        );
        ev.observed_at = Timestamp::from_micros(T0 + (at_secs * SEC as f64) as i64);
        ev
    }

    /// An index with hook-captured events at `(kind, session, seconds)`.
    fn hooked(events: &[(EventKind, &str, f64)]) -> Reconciler {
        let mut index = StoredIndex::default();
        for (kind, session, at) in events {
            let ev = event(*kind, session, *at);
            index.note(
                session,
                Seen {
                    event_id: ev.event_id,
                    kind: *kind,
                    observed_at: ev.observed_at,
                    reconstructed: false,
                    call_id: None,
                },
                ProjectRef::default,
            );
        }
        Reconciler::new(index)
    }

    #[test]
    fn matching_is_by_order_nearest_first_and_each_hook_event_is_used_once() {
        use EventKind::PromptSubmitted as P;
        let mut r = hooked(&[(P, "s", 100.0), (P, "s", 105.0)]);
        assert!(r.should_skip(&event(P, "s", 99.0), None));
        assert!(r.should_skip(&event(P, "s", 104.0), None));
        assert!(
            !r.should_skip(&event(P, "s", 106.0), None),
            "both hook prompts are spent: this one is the transcript's own"
        );
        assert_eq!(
            r.skipped(),
            Skipped {
                calls: 0,
                matched: 2
            }
        );

        // Nearest wins over earliest: the 105 s hook belongs to the 104 s
        // transcript prompt, the 100 s one to the 101 s.
        let mut r = hooked(&[(P, "s", 100.0), (P, "s", 105.0)]);
        assert!(r.should_skip(&event(P, "s", 104.0), None));
        assert!(r.should_skip(&event(P, "s", 101.0), None));
        assert_eq!(r.skipped().matched, 2);
    }

    #[test]
    fn the_tolerance_is_ten_seconds_and_a_session_start_has_none() {
        use EventKind::{PromptSubmitted as P, SessionStarted as S};
        let mut r = hooked(&[(P, "s", 100.0)]);
        assert!(!r.should_skip(&event(P, "s", 110.5), None), "10.5 s away");
        assert!(r.should_skip(&event(P, "s", 109.9), None), "9.9 s away");

        // The transcript has no start of its own, only its first entry,
        // however long after the hook's `SessionStart` the person typed.
        let mut r = hooked(&[(S, "s", 100.0)]);
        assert!(r.should_skip(&event(S, "s", 100.0 + 3.0 * 3600.0), None));
        assert!(!r.should_skip(&event(S, "s", 100.0), None), "used once");
    }

    #[test]
    fn only_the_same_kind_of_the_same_session_matches() {
        use EventKind::{AgentMessage, Notification, PromptSubmitted as P, TurnStopped};
        let mut r = hooked(&[(P, "s", 100.0)]);
        assert!(!r.should_skip(&event(TurnStopped, "s", 100.0), None));
        assert!(!r.should_skip(&event(P, "other", 100.0), None));
        // A kind no hook produces for the same fact is never reconciled.
        let mut r = hooked(&[(Notification, "s", 100.0), (AgentMessage, "s", 100.0)]);
        assert!(!r.should_skip(&event(Notification, "s", 100.0), None));
        assert_eq!(r.skipped().total(), 0);
    }

    #[test]
    fn an_interruption_never_matches_a_stop_hook() {
        use EventKind::TurnStopped as T;
        let mut r = hooked(&[(T, "s", 100.0)]);
        let mut interrupt = event(T, "s", 100.5);
        interrupt
            .attrs
            .insert("reason".into(), "user_interrupt".into());
        assert!(!r.should_skip(&interrupt, None));
        assert!(
            r.should_skip(&event(T, "s", 100.5), None),
            "the hook is still free"
        );
    }

    #[test]
    fn tool_calls_join_on_the_call_id_and_the_end_slot_is_shared() {
        use EventKind::{ToolCallFailed, ToolCallFinished, ToolCallStarted};
        let mut index = StoredIndex::default();
        for (kind, call) in [(ToolCallStarted, "c1"), (ToolCallFinished, "c1")] {
            let ev = event(kind, "s", 1.0);
            index.note(
                "s",
                Seen {
                    event_id: ev.event_id,
                    kind,
                    observed_at: ev.observed_at,
                    reconstructed: false,
                    call_id: Some(call.into()),
                },
                ProjectRef::default,
            );
        }
        let mut r = Reconciler::new(index);
        let with_call = |kind, call: &str| {
            let mut ev = event(kind, "s", 50.0);
            ev.tool = Some(attemptdb_core::ToolRef {
                name: "Bash".into(),
                category: attemptdb_core::ToolCategory::Shell,
                call_id: Some(call.into()),
            });
            ev
        };
        assert!(r.should_skip(&with_call(ToolCallStarted, "c1"), None));
        assert!(
            r.should_skip(&with_call(ToolCallFailed, "c1"), None),
            "the hook saw the end as finished, the transcript as failed: one end"
        );
        assert!(!r.should_skip(&with_call(ToolCallStarted, "c2"), None));
        // No call id: nothing to join on.
        assert!(!r.should_skip(&event(ToolCallStarted, "s", 1.0), None));
        assert_eq!(
            r.skipped(),
            Skipped {
                calls: 2,
                matched: 0
            }
        );
    }

    #[test]
    fn a_legacy_id_already_stored_skips_the_tool_event_in_any_session() {
        let mut index = StoredIndex::default();
        let old = EventId::derive(&["old"]);
        index.note(
            "s",
            Seen {
                event_id: old,
                kind: EventKind::ToolCallStarted,
                observed_at: Timestamp::from_micros(T0),
                reconstructed: true,
                call_id: Some("c".into()),
            },
            ProjectRef::default,
        );
        // A reconstructed event is not a hook event: no project, no hooks.
        assert_eq!(index.hooked_sessions(), 0);
        let mut r = Reconciler::new(index);
        let ev = event(EventKind::ToolCallStarted, "s", 1.0);
        assert!(r.should_skip(&ev, Some(old)));
        assert!(!r.should_skip(&ev, Some(EventId::derive(&["other"]))));
        assert_eq!(r.skipped().calls, 1);
    }

    #[test]
    fn without_an_index_nothing_is_skipped_and_no_project_is_known() {
        let mut r = Reconciler::default();
        assert!(!r.should_skip(&event(EventKind::PromptSubmitted, "s", 1.0), None));
        assert!(r.hooked_project("s").is_none());
    }

    #[test]
    fn the_hooked_project_is_the_earliest_hook_events() {
        let mut index = StoredIndex::default();
        for (at, root) in [(20.0, "/later"), (10.0, "/earlier")] {
            let ev = event(EventKind::PromptSubmitted, "s", at);
            index.note(
                "s",
                Seen {
                    event_id: ev.event_id,
                    kind: ev.kind,
                    observed_at: ev.observed_at,
                    reconstructed: false,
                    call_id: None,
                },
                || ProjectRef {
                    root: root.into(),
                    ..ProjectRef::default()
                },
            );
        }
        // Telemetry says nothing about the project a session ran in.
        let otel = event(EventKind::Unknown, "s", 1.0);
        index.note(
            "s",
            Seen {
                event_id: otel.event_id,
                kind: otel.kind,
                observed_at: otel.observed_at,
                reconstructed: false,
                call_id: None,
            },
            || ProjectRef {
                root: "/telemetry".into(),
                ..ProjectRef::default()
            },
        );
        let r = Reconciler::new(index);
        assert_eq!(r.hooked_project("s").unwrap().root, "/earlier");
        assert!(r.hooked_project("nobody").is_none());
    }

    #[test]
    fn the_window_is_unbounded_when_any_file_cannot_bound_it() {
        let a = Some(Timestamp::from_micros(T0));
        let b = Some(Timestamp::from_micros(T0 + 100 * SEC));
        let w = Window::around(&[a, b], &[a, b]);
        assert_eq!(
            w.since,
            Some(Timestamp::from_micros(T0 - WINDOW_LEAD_MICROS))
        );
        assert_eq!(
            w.until,
            Some(Timestamp::from_micros(T0 + 100 * SEC + WINDOW_TAIL_MICROS))
        );
        let w = Window::around(&[a, None], &[a, b]);
        assert_eq!(w.since, None);
        assert!(w.until.is_some());
        let w = Window::around(&[a], &[None]);
        assert_eq!(w.until, None);
    }

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
