//! The capture daemon: the single database writer behind the IPC endpoint.
//!
//! ```text
//! start:  refuse if a live daemon answers PING
//!         open the writer (takes `.attemptdb/LOCK`)
//!         import pending spool files
//!         bind the endpoint, write <runtime_dir>/attemptdb.pid + endpoint.json
//! serve:  accept connections concurrently; every INGEST batch is queued to
//!         one writer thread (ordering + single-writer lock); ACK only after
//!         `Database::ingest` returned, i.e. after the WAL durability policy
//!         is satisfied; import the spool every 5 s; flush the memtable by
//!         threshold or every 60 s
//! stop:   SIGTERM / SIGINT / SHUTDOWN frame -> close the listener (hooks
//!         spool from now on), final spool import, flush, close the database,
//!         remove socket + pid + endpoint record
//! ```
//!
//! Logs go to `<log_dir>/daemon.log` (append, one timestamped line per
//! entry) and additionally to stderr with `--foreground`.

use crate::config::{Config, DeviceRecord};
use crate::ipc::{
    self, Connection, DaemonStatus, Endpoint, EndpointRecord, Frame, Hello, HelloAck, IngestAck,
    IpcError, Listener, MsgType, Nack, Rejected,
};
use crate::locator::Locator;
use crate::platform::canonical_display_path;
use crate::{CaptureError, Result, io_at};
use attemptdb_core::schema::{CANONICAL_SCHEMA_VERSION, MIN_READABLE_SCHEMA_VERSION};
use attemptdb_core::{DeviceId, Event, Timestamp};
use attemptdb_storage::format::FRAME_FORMAT_VERSION;
use attemptdb_storage::{Database, DurabilityPolicy, OpenOptions, StorageError};
use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::AsyncReadExt;
use tokio::sync::{Notify, mpsc, oneshot};

pub use crate::ipc::DaemonStatus as Status;

/// Log file name under the log directory.
pub const LOG_FILE: &str = "daemon.log";

/// Minimum memtable rows for a timer-driven flush.
pub const PERIODIC_FLUSH_MIN_ROWS: usize = 256;
/// Rows older than this are flushed by the timer regardless of count.
pub const PERIODIC_FLUSH_MAX_AGE: Duration = Duration::from_secs(15 * 60);

#[derive(Clone, Debug)]
pub struct DaemonOptions {
    /// Also write log lines to stderr.
    pub foreground: bool,
    /// WAL durability before an `ACK` is sent (default: `Strict`).
    pub durability: DurabilityPolicy,
    /// How often pending spool files are imported.
    pub spool_interval: Duration,
    /// Memtable flush interval; the size thresholds apply as well.
    pub flush_interval: Duration,
    /// Flush the memtable once it holds this many events.
    pub flush_events: usize,
    /// A connection that sends nothing for this long is dropped.
    pub idle_timeout: Duration,
    /// Computes the device's inference set for `attempt sync` uploads
    /// (`send_inferences`). `None` uploads facts only.
    pub inference_source: Option<crate::sync::InferenceSource>,
}

impl Default for DaemonOptions {
    fn default() -> Self {
        Self {
            foreground: false,
            durability: DurabilityPolicy::Strict,
            spool_interval: Duration::from_secs(5),
            flush_interval: Duration::from_secs(60),
            flush_events: OpenOptions::default().flush_events,
            idle_timeout: Duration::from_secs(30),
            inference_source: None,
        }
    }
}

/// `<log_dir>/daemon.log`.
pub fn log_path(locator: &Locator) -> PathBuf {
    locator.paths.log_dir.join(LOG_FILE)
}

// ---------------------------------------------------------------------------
// Client-side helpers (CLI, doctor, tests)
// ---------------------------------------------------------------------------

/// Result of probing the endpoint.
#[derive(Debug)]
pub enum Probe {
    /// Nothing is listening (one `stat`, no connection attempt).
    NotRunning,
    /// Something is at the endpoint but did not answer `PING` properly.
    Unresponsive(IpcError),
    Running(Box<DaemonStatus>),
}

pub fn probe(locator: &Locator) -> Probe {
    if !ipc::daemon_reachable(locator) {
        return Probe::NotRunning;
    }
    match ipc::Client::status(locator) {
        Ok(s) => Probe::Running(Box::new(s)),
        Err(IpcError::NotRunning) => Probe::NotRunning,
        Err(e) => Probe::Unresponsive(e),
    }
}

/// `PING` the daemon. Returns quickly (one `stat`) when nothing listens.
pub fn status(locator: &Locator) -> Option<DaemonStatus> {
    match probe(locator) {
        Probe::Running(s) => Some(*s),
        _ => None,
    }
}

/// Ask a running daemon to flush and exit. `Ok(false)` when none is running.
/// Returns once the request is acknowledged; use [`wait_until_stopped`] to
/// wait for the process to finish.
pub fn stop(locator: &Locator) -> Result<bool> {
    if !ipc::daemon_reachable(locator) {
        return Ok(false);
    }
    match ipc::Client::request_shutdown(locator) {
        Ok(()) => Ok(true),
        Err(IpcError::NotRunning) => Ok(false),
        Err(e) => Err(CaptureError::Other(format!("cannot stop the daemon: {e}"))),
    }
}

/// Poll until the socket and pid file are gone (or `timeout` elapses).
pub fn wait_until_stopped(locator: &Locator, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if !ipc::daemon_reachable(locator) && !ipc::pid_path(locator).exists() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// Poll until the daemon answers `PING` (or `timeout` elapses).
pub fn wait_until_running(locator: &Locator, timeout: Duration) -> Option<DaemonStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(s) = status(locator) {
            return Some(s);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

// ---------------------------------------------------------------------------
// Logging
// ---------------------------------------------------------------------------

/// Minimal append-only logger: `<rfc3339> <LEVEL> <message>`.
pub struct Logger {
    file: Mutex<Option<std::fs::File>>,
    stderr: bool,
}

impl Logger {
    pub fn open(path: &Path, stderr: bool) -> Self {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .ok();
        Self {
            file: Mutex::new(file),
            stderr,
        }
    }

    fn write(&self, level: &str, msg: &str) {
        let line = format!("{} {level:<5} {msg}\n", Timestamp::now().to_rfc3339());
        if self.stderr {
            eprint!("{line}");
        }
        if let Ok(mut guard) = self.file.lock()
            && let Some(f) = guard.as_mut()
        {
            let _ = f.write_all(line.as_bytes());
        }
    }

    pub fn info(&self, msg: impl AsRef<str>) {
        self.write("INFO", msg.as_ref());
    }

    pub fn warn(&self, msg: impl AsRef<str>) {
        self.write("WARN", msg.as_ref());
    }

    pub fn error(&self, msg: impl AsRef<str>) {
        self.write("ERROR", msg.as_ref());
    }
}

// ---------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Counters {
    connections: u64,
    rejected_connections: u64,
    batches: u64,
    wal_commits: u64,
    events_ingested: u64,
    duplicates: u64,
    rejected_events: u64,
    spool_files_imported: u64,
    spool_events_imported: u64,
    last_spool_import_at: Option<Timestamp>,
    spool_pending: bool,
    flushes: u64,
    last_flush_at: Option<Timestamp>,
    last_source_seq: u64,
    generation: u64,
    segments: u64,
    memtable_rows: u64,
}

struct Shared {
    locator: Locator,
    opts: DaemonOptions,
    log: Logger,
    endpoint: Endpoint,
    canonical_db_dir: PathBuf,
    db_id: uuid::Uuid,
    device_id: DeviceId,
    capture_mode: String,
    started: Instant,
    started_at: Timestamp,
    counters: Mutex<Counters>,
    shutdown: Notify,
    shutting_down: AtomicBool,
}

impl Shared {
    fn counters(&self) -> std::sync::MutexGuard<'_, Counters> {
        self.counters.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn status(&self) -> DaemonStatus {
        let c = self.counters();
        DaemonStatus {
            pid: std::process::id(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            protocol_version: ipc::PROTOCOL_VERSION,
            endpoint: self.endpoint.to_string(),
            db_dir: self.locator.db_dir.clone(),
            data_dir: self.locator.paths.data_dir.clone(),
            log_path: log_path(&self.locator),
            device_id: self.device_id,
            capture_mode: self.capture_mode.clone(),
            durability: format!("{:?}", self.opts.durability).to_lowercase(),
            started_at: self.started_at,
            uptime_secs: self.started.elapsed().as_secs(),
            connections: c.connections,
            rejected_connections: c.rejected_connections,
            batches: c.batches,
            wal_commits: c.wal_commits,
            events_ingested: c.events_ingested,
            duplicates: c.duplicates,
            rejected_events: c.rejected_events,
            spool_files_imported: c.spool_files_imported,
            spool_events_imported: c.spool_events_imported,
            last_spool_import_at: c.last_spool_import_at,
            spool_pending: c.spool_pending,
            flushes: c.flushes,
            last_flush_at: c.last_flush_at,
            last_source_seq: c.last_source_seq,
            generation: c.generation,
            segments: c.segments,
            memtable_rows: c.memtable_rows,
            extra: Default::default(),
        }
    }

    fn request_shutdown(&self) {
        self.shutting_down.store(true, Ordering::SeqCst);
        self.shutdown.notify_one();
    }

    fn hello_ack(&self) -> HelloAck {
        HelloAck {
            daemon_version: env!("CARGO_PKG_VERSION").to_string(),
            protocol_version: ipc::PROTOCOL_VERSION,
            pid: std::process::id(),
            db_id: self.db_id,
            device_id: self.device_id,
            schema_version: CANONICAL_SCHEMA_VERSION,
            format_version: FRAME_FORMAT_VERSION,
            capture_mode: self.capture_mode.clone(),
            db_dir: self.locator.db_dir.clone(),
            extra: Default::default(),
        }
    }

    /// Whether `other` names the database this daemon serves.
    fn serves(&self, other: &Path) -> bool {
        other == self.locator.db_dir || canonical_display_path(other) == self.canonical_db_dir
    }
}

// ---------------------------------------------------------------------------
// Writer thread
// ---------------------------------------------------------------------------

type IngestReply = std::result::Result<IngestAck, String>;

enum WriterCmd {
    Ingest {
        events: Vec<Event>,
        reply: oneshot::Sender<IngestReply>,
    },
    ImportSpool,
    Flush,
    Shutdown {
        reply: oneshot::Sender<()>,
    },
}

/// Upper bound on `INGEST` batches coalesced into one WAL append + fsync.
const MAX_GROUP: usize = 256;

fn writer_loop(mut db: Database, mut rx: mpsc::Receiver<WriterCmd>, shared: Arc<Shared>) {
    let mut last_periodic_flush = std::time::Instant::now();
    let mut shutdown_reply = None;
    // A command pulled off the queue while forming an ingest group, to be
    // handled next.
    let mut deferred: Option<WriterCmd> = None;
    loop {
        let cmd = match deferred.take() {
            Some(c) => c,
            None => match rx.blocking_recv() {
                Some(c) => c,
                None => break,
            },
        };
        match cmd {
            WriterCmd::Ingest { events, reply } => {
                // Group commit: every batch already queued behind this one
                // shares the WAL append and the fsync, and is acknowledged
                // right after it. Ordering is the queue order.
                let mut group = vec![(events, reply)];
                while group.len() < MAX_GROUP {
                    match rx.try_recv() {
                        Ok(WriterCmd::Ingest { events, reply }) => group.push((events, reply)),
                        Ok(other) => {
                            deferred = Some(other);
                            break;
                        }
                        Err(_) => break,
                    }
                }
                ingest_group(&mut db, &shared, group);
                continue;
            }
            WriterCmd::ImportSpool => import_spool(&mut db, &shared),
            WriterCmd::Flush => {
                // A timer-driven flush of a near-empty memtable would create
                // one tiny segment per interval (86 segments for 3k events
                // was observed). Flush on the timer only when enough rows
                // accumulated, or when the oldest rows waited long enough.
                let rows = db.stats().memtable_rows;
                if rows >= PERIODIC_FLUSH_MIN_ROWS
                    || (rows > 0 && last_periodic_flush.elapsed() >= PERIODIC_FLUSH_MAX_AGE)
                {
                    flush(&mut db, &shared, "periodic");
                    last_periodic_flush = std::time::Instant::now();
                }
            }
            WriterCmd::Shutdown { reply } => {
                import_spool(&mut db, &shared);
                flush(&mut db, &shared, "shutdown");
                refresh_stats(&db, &shared);
                shutdown_reply = Some(reply);
                break;
            }
        }
        refresh_stats(&db, &shared);
    }
    drop(rx);
    if let Err(e) = db.close() {
        shared.log.error(format!("closing the database: {e}"));
    }
    if let Some(reply) = shutdown_reply {
        let _ = reply.send(());
    }
}

/// Ingest one or more queued batches with a single WAL append, then answer
/// every batch. Deduplication is by event id across the group and against
/// the database; a batch never observes another batch's events as its own.
fn ingest_group(
    db: &mut Database,
    shared: &Shared,
    group: Vec<(Vec<Event>, oneshot::Sender<IngestReply>)>,
) {
    let mut seen = HashSet::new();
    let mut fresh: Vec<Event> = Vec::new();
    let mut acks: Vec<IngestAck> = Vec::with_capacity(group.len());
    let mut replies = Vec::with_capacity(group.len());
    let mut failure: Option<String> = None;
    for (events, reply) in group {
        let mut ack = IngestAck::default();
        for ev in events {
            if failure.is_some() {
                break;
            }
            if ev.schema_version < MIN_READABLE_SCHEMA_VERSION
                || ev.schema_version > CANONICAL_SCHEMA_VERSION + 100
            {
                ack.rejected.push(Rejected {
                    event_id: ev.event_id,
                    reason: format!("unsupported schema version {}", ev.schema_version),
                });
                continue;
            }
            if !seen.insert(ev.event_id) {
                ack.duplicate.push(ev.event_id);
                continue;
            }
            match db.is_known(&ev.event_id) {
                Ok(true) => ack.duplicate.push(ev.event_id),
                Ok(false) => {
                    ack.accepted.push(ev.event_id);
                    fresh.push(ev);
                }
                Err(e) => failure = Some(e.to_string()),
            }
        }
        acks.push(ack);
        replies.push(reply);
    }
    let mut committed = false;
    if failure.is_none() && !fresh.is_empty() {
        // Returns after the WAL append (and fsync under Strict durability).
        match db.ingest(fresh) {
            Ok(_) => committed = true,
            Err(e) => failure = Some(e.to_string()),
        }
    }
    let seq = refresh_stats(db, shared);
    if let Some(msg) = failure {
        shared.log.error(format!("ingest failed: {msg}"));
        for reply in replies {
            let _ = reply.send(Err(msg.clone()));
        }
        return;
    }
    {
        let mut c = shared.counters();
        c.batches += acks.len() as u64;
        c.wal_commits += u64::from(committed);
        for ack in &acks {
            c.events_ingested += ack.accepted.len() as u64;
            c.duplicates += ack.duplicate.len() as u64;
            c.rejected_events += ack.rejected.len() as u64;
        }
    }
    for (mut ack, reply) in acks.into_iter().zip(replies) {
        ack.durable_source_seq = seq;
        let _ = reply.send(Ok(ack));
    }
}

fn import_spool(db: &mut Database, shared: &Shared) {
    match db.import_spool() {
        Ok(r) if r.spool_files > 0 => {
            shared.log.info(format!(
                "imported {} spool file(s): {} accepted, {} duplicates, {} undecodable",
                r.spool_files, r.accepted, r.duplicates, r.undecodable
            ));
            let mut c = shared.counters();
            c.spool_files_imported += r.spool_files as u64;
            c.spool_events_imported += r.accepted as u64;
            c.duplicates += r.duplicates as u64;
            c.last_spool_import_at = Some(Timestamp::now());
        }
        Ok(_) => {}
        Err(e) => shared.log.error(format!("spool import failed: {e}")),
    }
    for w in db.warnings.drain(..) {
        shared.log.warn(w);
    }
}

fn flush(db: &mut Database, shared: &Shared, why: &str) {
    match db.flush() {
        Ok(Some(meta)) => {
            shared.log.info(format!(
                "flushed {} events to {} (generation {}, {why})",
                meta.rows,
                meta.file,
                db.manifest().generation
            ));
            let mut c = shared.counters();
            c.flushes += 1;
            c.last_flush_at = Some(Timestamp::now());
        }
        Ok(None) => {}
        Err(e) => shared.log.error(format!("flush failed ({why}): {e}")),
    }
}

/// Copy the database statistics into the shared snapshot; returns the last
/// assigned `source_seq`.
fn refresh_stats(db: &Database, shared: &Shared) -> u64 {
    let s = db.stats();
    let mut c = shared.counters();
    c.last_source_seq = s.last_source_seq;
    c.generation = s.generation;
    c.segments = s.segments as u64;
    c.memtable_rows = s.memtable_rows as u64;
    c.spool_pending = s.spool_pending;
    s.last_source_seq
}

// ---------------------------------------------------------------------------
// Connections
// ---------------------------------------------------------------------------

async fn send_nack(
    stream: &mut Box<dyn ipc::AsyncStream>,
    code: &str,
    message: impl Into<String>,
    retryable: bool,
) {
    if let Ok(frame) = Frame::json(MsgType::Nack, &Nack::new(code, message, retryable)) {
        let _ = frame.write_async(stream).await;
    }
}

async fn handle_connection(
    conn: Connection,
    shared: Arc<Shared>,
    writer: mpsc::Sender<WriterCmd>,
) -> std::result::Result<(), IpcError> {
    let mut stream = conn.stream;
    let idle = shared.opts.idle_timeout;

    let mut prelude = [0u8; ipc::PRELUDE_LEN];
    match tokio::time::timeout(idle, stream.read_exact(&mut prelude)).await {
        Ok(Ok(_)) => {}
        // A probe that connected and left (Windows `metadata` on the pipe,
        // a port scanner of our own uid): nothing to report.
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
        Ok(Err(e)) => return Err(e.into()),
        Err(_) => return Err(IpcError::Timeout),
    }
    let (version, _flags) = match ipc::decode_prelude(&prelude) {
        Ok(v) => v,
        Err(e) => {
            send_nack(&mut stream, "protocol_error", e.to_string(), false).await;
            return Err(e);
        }
    };
    if version != ipc::PROTOCOL_VERSION {
        send_nack(
            &mut stream,
            "unsupported_protocol",
            format!(
                "protocol version {version} is not supported (daemon speaks {})",
                ipc::PROTOCOL_VERSION
            ),
            false,
        )
        .await;
        return Err(IpcError::UnsupportedProtocol(version));
    }

    let mut hello: Option<Hello> = None;
    loop {
        let frame = match tokio::time::timeout(idle, Frame::read_async(&mut stream)).await {
            Ok(Ok(f)) => f,
            Ok(Err(IpcError::Closed)) => return Ok(()),
            Ok(Err(e @ (IpcError::CrcMismatch { .. } | IpcError::FrameTooLarge(_)))) => {
                send_nack(&mut stream, "protocol_error", e.to_string(), false).await;
                return Err(e);
            }
            Ok(Err(e)) => return Err(e),
            Err(_) => return Err(IpcError::Timeout),
        };
        match frame.kind() {
            Some(MsgType::Hello) => {
                let h: Hello = match frame.parse_json() {
                    Ok(h) => h,
                    Err(e) => {
                        send_nack(
                            &mut stream,
                            "invalid_payload",
                            format!("cannot decode HELLO: {e}"),
                            false,
                        )
                        .await;
                        continue;
                    }
                };
                if h.protocol_version != ipc::PROTOCOL_VERSION {
                    send_nack(
                        &mut stream,
                        "unsupported_protocol",
                        format!("protocol version {} is not supported", h.protocol_version),
                        false,
                    )
                    .await;
                    return Err(IpcError::UnsupportedProtocol(h.protocol_version));
                }
                if !shared.serves(&h.db_dir) {
                    send_nack(
                        &mut stream,
                        "wrong_database",
                        format!(
                            "this daemon serves {}, not {}",
                            shared.locator.db_dir.display(),
                            h.db_dir.display()
                        ),
                        false,
                    )
                    .await;
                    hello = None;
                    continue;
                }
                if h.spooled {
                    let _ = writer.try_send(WriterCmd::ImportSpool);
                }
                Frame::json(MsgType::HelloAck, &shared.hello_ack())?
                    .write_async(&mut stream)
                    .await?;
                hello = Some(h);
            }
            Some(MsgType::Ingest) => {
                if hello.is_none() {
                    send_nack(
                        &mut stream,
                        "hello_required",
                        "send HELLO with the database directory before INGEST",
                        false,
                    )
                    .await;
                    continue;
                }
                let events: Vec<Event> = match frame.parse_json() {
                    Ok(v) => v,
                    Err(e) => {
                        send_nack(
                            &mut stream,
                            "invalid_payload",
                            format!("cannot decode event batch: {e}"),
                            false,
                        )
                        .await;
                        continue;
                    }
                };
                if events.is_empty() {
                    let ack = IngestAck {
                        durable_source_seq: shared.counters().last_source_seq,
                        ..Default::default()
                    };
                    Frame::json(MsgType::Ack, &ack)?
                        .write_async(&mut stream)
                        .await?;
                    continue;
                }
                if shared.shutting_down.load(Ordering::SeqCst) {
                    send_nack(
                        &mut stream,
                        "shutting_down",
                        "daemon is shutting down; spool the batch",
                        true,
                    )
                    .await;
                    continue;
                }
                let (tx, rx) = oneshot::channel();
                if writer
                    .send(WriterCmd::Ingest { events, reply: tx })
                    .await
                    .is_err()
                {
                    send_nack(
                        &mut stream,
                        "shutting_down",
                        "daemon is shutting down; spool the batch",
                        true,
                    )
                    .await;
                    continue;
                }
                match rx.await {
                    Ok(Ok(ack)) => {
                        Frame::json(MsgType::Ack, &ack)?
                            .write_async(&mut stream)
                            .await?
                    }
                    Ok(Err(msg)) => send_nack(&mut stream, "ingest_failed", msg, true).await,
                    Err(_) => {
                        send_nack(
                            &mut stream,
                            "shutting_down",
                            "daemon is shutting down; spool the batch",
                            true,
                        )
                        .await
                    }
                }
            }
            Some(MsgType::Ping) => {
                Frame::json(MsgType::Pong, &shared.status())?
                    .write_async(&mut stream)
                    .await?;
            }
            Some(MsgType::Shutdown) => {
                let ack = IngestAck {
                    durable_source_seq: shared.counters().last_source_seq,
                    ..Default::default()
                };
                Frame::json(MsgType::Ack, &ack)?
                    .write_async(&mut stream)
                    .await?;
                shared.request_shutdown();
                // Let the client read the ACK and hang up first; closing
                // our end immediately would race its read.
                let _ =
                    tokio::time::timeout(Duration::from_secs(2), Frame::read_async(&mut stream))
                        .await;
                return Ok(());
            }
            Some(other) => {
                send_nack(
                    &mut stream,
                    "unexpected_message_type",
                    format!("{} is not a request a client may send", other.as_str()),
                    false,
                )
                .await;
            }
            None => {
                send_nack(
                    &mut stream,
                    "unknown_message_type",
                    format!(
                        "message type {} is not understood by protocol version {}",
                        frame.msg_type,
                        ipc::PROTOCOL_VERSION
                    ),
                    false,
                )
                .await;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Lifecycle
// ---------------------------------------------------------------------------

fn other(msg: impl Into<String>) -> CaptureError {
    CaptureError::Other(msg.into())
}

fn open_db(locator: &Locator, opts: &DaemonOptions) -> Result<Database> {
    let mut oo = OpenOptions {
        create: true,
        durability: opts.durability,
        flush_events: opts.flush_events,
        keys: crate::keys::provider_for_db(locator, &locator.db_dir),
        ..Default::default()
    };
    if !Database::exists(&locator.db_dir) {
        let device = DeviceRecord::load_or_create(&locator.paths.data_dir)?;
        oo.device_id = Some(device.device_id);
        if let Some(parent) = locator.db_dir.parent() {
            std::fs::create_dir_all(parent).map_err(|e| io_at(parent, e))?;
        }
    }
    match Database::open(&locator.db_dir, oo) {
        Ok(db) => Ok(db),
        Err(StorageError::Locked(p)) => Err(other(format!(
            "database {} is locked by another writer (a CLI command importing the spool, or another daemon); retry in a moment",
            p.display()
        ))),
        Err(e) => Err(e.into()),
    }
}

fn read_pid(path: &Path) -> Option<u32> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// Whether a process with this pid exists (Unix: `kill(pid, 0)`).
pub fn process_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        // SAFETY: signal 0 performs no action; it only checks for existence.
        let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
        rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        false
    }
}

async fn wait_for_signal() -> &'static str {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match (
            signal(SignalKind::terminate()),
            signal(SignalKind::interrupt()),
        ) {
            (Ok(mut term), Ok(mut int)) => {
                tokio::select! {
                    _ = term.recv() => "SIGTERM",
                    _ = int.recv() => "SIGINT",
                }
            }
            _ => {
                std::future::pending::<()>().await;
                "signal"
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
        "ctrl-c"
    }
}

fn human_duration(d: Duration) -> String {
    let s = d.as_secs();
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m{:02}s", s / 60, s % 60)
    } else {
        format!("{}h{:02}m", s / 3600, (s % 3600) / 60)
    }
}

/// Run the daemon on a fresh multi-thread tokio runtime until it is stopped.
pub fn run(locator: &Locator, opts: DaemonOptions) -> Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("attemptdb-daemon")
        .build()
        .map_err(|e| other(format!("cannot start the async runtime: {e}")))?;
    let result = rt.block_on(serve(locator.clone(), opts));
    rt.shutdown_timeout(Duration::from_secs(2));
    result
}

/// The daemon body; must run inside a tokio runtime. Returns after a clean
/// shutdown, or with the reason it could not start.
pub async fn serve(locator: Locator, opts: DaemonOptions) -> Result<()> {
    let endpoint = ipc::endpoint(&locator);
    let pid_path = ipc::pid_path(&locator);
    let record_path = ipc::endpoint_record_path(&locator);

    // 1. Never fight a live daemon for the endpoint.
    match probe(&locator) {
        Probe::Running(s) => {
            return Err(other(format!(
                "daemon already running (pid {}) at {}; use `attempt daemon status` or `attempt daemon stop`",
                s.pid, s.endpoint
            )));
        }
        Probe::Unresponsive(e) => {
            if let Some(pid) = read_pid(&pid_path)
                && process_alive(pid)
            {
                return Err(other(format!(
                    "pid file {} names pid {pid}, which is alive but did not answer PING ({e}); stop it before starting a new daemon",
                    pid_path.display()
                )));
            }
        }
        Probe::NotRunning => {}
    }

    let log = Logger::open(&log_path(&locator), opts.foreground);
    log.info(format!(
        "attemptdb daemon {} starting (pid {})",
        env!("CARGO_PKG_VERSION"),
        std::process::id()
    ));
    if let Some(pid) = read_pid(&pid_path) {
        if process_alive(pid) {
            return Err(other(format!(
                "pid file {} names pid {pid}, which is still alive; stop it before starting a new daemon",
                pid_path.display()
            )));
        }
        log.warn(format!(
            "removing stale pid file ({}: pid {pid} is gone)",
            pid_path.display()
        ));
        let _ = std::fs::remove_file(&pid_path);
        let _ = std::fs::remove_file(&record_path);
    }

    // 2. Take the single-writer lock and recover.
    let db = open_db(&locator, &opts)
        .inspect_err(|e| log.error(format!("cannot open the database: {e}")))?;
    let config = Config::load_or_default(&locator.paths.config_dir);
    log.info(format!(
        "database {} (device {}, {}, {:?} durability)",
        locator.db_dir.display(),
        db.device_id().short(),
        config.capture_mode,
        opts.durability
    ));
    let shared = Arc::new(Shared {
        canonical_db_dir: canonical_display_path(&locator.db_dir),
        db_id: db.identity().db_id,
        device_id: db.device_id(),
        capture_mode: config.capture_mode.as_str().to_string(),
        locator: locator.clone(),
        opts: opts.clone(),
        log,
        endpoint: endpoint.clone(),
        started: Instant::now(),
        started_at: Timestamp::now(),
        counters: Mutex::new(Counters::default()),
        shutdown: Notify::new(),
        shutting_down: AtomicBool::new(false),
    });
    let log = &shared.log;
    let mut db = db;
    import_spool(&mut db, &shared);
    refresh_stats(&db, &shared);

    // 3. Listen, then advertise.
    let mut listener = Listener::bind(&endpoint).map_err(|e| {
        log.error(format!("cannot bind {endpoint}: {e}"));
        other(format!("cannot bind {endpoint}: {e}"))
    })?;
    if let Some(dir) = pid_path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| io_at(dir, e))?;
    }
    std::fs::write(&pid_path, format!("{}\n", std::process::id()))
        .map_err(|e| io_at(&pid_path, e))?;
    let record = EndpointRecord {
        endpoint: endpoint.clone(),
        protocol_version: ipc::PROTOCOL_VERSION,
        pid: std::process::id(),
    };
    if let Ok(json) = serde_json::to_vec_pretty(&record) {
        let _ = std::fs::write(&record_path, json);
    }
    log.info(format!("listening at {endpoint}"));

    // 4. The single writer.
    let (tx, rx) = mpsc::channel::<WriterCmd>(1024);
    let writer_thread = {
        let shared = shared.clone();
        std::thread::Builder::new()
            .name("attemptdb-writer".into())
            .spawn(move || writer_loop(db, rx, shared))
            .map_err(|e| other(format!("cannot start the writer thread: {e}")))?
    };

    // 5. Periodic work.
    let spool_task = tokio::spawn(periodic(tx.clone(), opts.spool_interval, || {
        WriterCmd::ImportSpool
    }));
    let flush_task = tokio::spawn(periodic(tx.clone(), opts.flush_interval, || {
        WriterCmd::Flush
    }));
    // Sync uploader (RFC 0006 §10 client): only when `attempt sync connect`
    // has stored a configuration. Reads the database read-only, so it never
    // contends with the writer thread.
    let sync_task = match crate::sync::SyncConfig::load(&locator.paths.config_dir) {
        Ok(Some(cfg)) => {
            log.info(format!(
                "sync: uploading to {} every {}s ({})",
                cfg.url,
                cfg.interval().as_secs(),
                if cfg.send_content {
                    "content included"
                } else {
                    "metadata only"
                }
            ));
            let locator = locator.clone();
            let shared = shared.clone();
            let source = opts.inference_source.clone();
            Some(tokio::spawn(async move {
                let mut interval = tokio::time::interval(cfg.interval());
                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                interval.tick().await;
                loop {
                    interval.tick().await;
                    let (l, c, s) = (locator.clone(), cfg.clone(), source.clone());
                    match tokio::task::spawn_blocking(move || {
                        crate::sync::upload_once_with(&l, &c, s.as_ref())
                    })
                    .await
                    {
                        Ok(Ok(r)) if r.batches > 0 => {
                            shared
                                .log
                                .info(format!("sync: {}", crate::sync::describe(&r)));
                        }
                        Ok(Ok(_)) => {}
                        Ok(Err(e)) => shared.log.warn(format!("sync: {e:#}")),
                        Err(e) => shared.log.warn(format!("sync task failed: {e}")),
                    }
                }
            }))
        }
        Ok(None) => None,
        Err(e) => {
            log.warn(format!(
                "sync: configuration unreadable, uploads disabled: {e:#}"
            ));
            None
        }
    };

    // 6. Serve.
    let mut signal = Box::pin(wait_for_signal());
    let owner_uid = listener.owner_uid();
    let reason = loop {
        tokio::select! {
            biased;
            _ = shared.shutdown.notified() => break "shutdown request",
            why = &mut signal => break why,
            accepted = listener.accept() => match accepted {
                Ok(conn) => {
                    if let (Some(peer), Some(me)) = (conn.peer_uid, owner_uid)
                        && peer != me
                    {
                        shared.counters().rejected_connections += 1;
                        log.warn(format!("rejected connection from uid {peer}"));
                        continue;
                    }
                    shared.counters().connections += 1;
                    let shared = shared.clone();
                    let tx = tx.clone();
                    tokio::spawn(async move {
                        if let Err(e) = handle_connection(conn, shared.clone(), tx).await {
                            shared.log.warn(format!("connection error: {e}"));
                        }
                    });
                }
                Err(e) => {
                    log.warn(format!("accept failed: {e}"));
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            },
        }
    };

    // 7. Stop: no new connections, drain the writer, clean up.
    log.info(format!("shutdown requested ({reason})"));
    shared.shutting_down.store(true, Ordering::SeqCst);
    listener.close();
    let _ = std::fs::remove_file(&record_path);
    spool_task.abort();
    flush_task.abort();
    if let Some(t) = sync_task {
        t.abort();
    }
    let (rtx, rrx) = oneshot::channel();
    if tx.send(WriterCmd::Shutdown { reply: rtx }).await.is_ok() {
        let _ = rrx.await;
    }
    drop(tx);
    let _ = tokio::task::spawn_blocking(move || writer_thread.join()).await;
    let _ = std::fs::remove_file(&pid_path);
    let c = shared.counters();
    log.info(format!(
        "stopped after {}: {} events in {} batches ({} WAL commits), {} spool file(s) imported",
        human_duration(shared.started.elapsed()),
        c.events_ingested,
        c.batches,
        c.wal_commits,
        c.spool_files_imported
    ));
    Ok(())
}

async fn periodic(tx: mpsc::Sender<WriterCmd>, every: Duration, make: impl Fn() -> WriterCmd) {
    let mut interval = tokio::time::interval(every);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    interval.tick().await; // the first tick completes immediately
    loop {
        interval.tick().await;
        if tx.send(make()).await.is_err() {
            return;
        }
    }
}
