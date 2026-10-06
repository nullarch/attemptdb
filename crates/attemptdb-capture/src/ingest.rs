//! Opening the writer and importing pending spool files.

use crate::config::{Config, DeviceRecord};
use crate::keys::{ContentGate, GateDecision, KeyStoreOptions, NoticeLevel};
use crate::locator::Locator;
use crate::{Result, io_at};
use attemptdb_core::Timestamp;
use attemptdb_storage::{Database, IngestReport, OpenOptions};
use serde::Serialize;
use std::path::Path;

/// Open (or create) the database the locator points at, as the writer.
/// Encryption follows `encryption` in the config (see [`crate::keys`]); use
/// [`open_writer_guarded`] when this handle will store events, so that
/// content is withheld when a required key is missing.
pub fn open_writer(locator: &Locator, create: bool) -> Result<Database> {
    Ok(open_writer_guarded(locator, create)?.0)
}

/// [`open_writer`], plus the [`ContentGate`] its events must pass through
/// ([`ContentGate::apply`], or [`import_spool`] for the spool). Problems
/// worth the person's attention (a withheld-content state, a config file
/// that could not be used) are added to the database's `warnings`, which
/// `attempt status` prints.
pub fn open_writer_guarded(locator: &Locator, create: bool) -> Result<(Database, ContentGate)> {
    let config = Config::load_or_default(&locator.paths.config_dir);
    if create && !Database::exists(&locator.db_dir) {
        let device = DeviceRecord::load_or_create(&locator.paths.data_dir)?;
        if let Some(parent) = locator.db_dir.parent() {
            std::fs::create_dir_all(parent).map_err(|e| io_at(parent, e))?;
        }
        // Created before it is opened: its keys belong to its id.
        if let Err(e) = Database::create(&locator.db_dir, device.device_id)
            && !Database::exists(&locator.db_dir)
        {
            return Err(e.into());
        }
    }
    let keys = crate::keys::writer_keys(
        locator,
        &locator.db_dir,
        config.encryption,
        KeyStoreOptions::from_env(),
    );
    let mut db = Database::open(
        &locator.db_dir,
        OpenOptions {
            create,
            keys: keys.provider,
            ..Default::default()
        },
    )?;
    let gate = keys.gate.clone().with_redaction(config.redact_secrets);
    for notice in gate.take_notices() {
        if notice.level == NoticeLevel::Error {
            db.warnings.push(notice.message);
        }
    }
    if let Some(why) = &config.load_error {
        db.warnings.push(format!(
            "{why}; capturing metadata only until it is fixed (`attempt doctor`)"
        ));
    }
    Ok((db, gate))
}

/// Import the spool through `gate`: every spooled event passes
/// [`ContentGate::apply`] (content withheld while a required key is missing,
/// secrets masked when `redact_secrets` is on) before it reaches the
/// write-ahead log. Quarantine and release are the storage engine's
/// ([`Database::import_spool_with`]).
///
/// While the gate *holds* (a key that exists cannot be read right now, see
/// [`crate::keys`]) nothing is imported and no spool file is touched: the
/// events stay where the hooks left them and are imported, with their
/// content, once the key reads. The report is then empty.
pub fn import_spool(db: &mut Database, gate: &ContentGate) -> Result<IngestReport> {
    import_spool_observing(db, gate, &mut |_| {})
}

/// [`import_spool`], showing `observe` each spool file's events after the
/// gate has been applied and before they are ingested (the daemon keeps its
/// session-to-project map current this way).
pub fn import_spool_observing(
    db: &mut Database,
    gate: &ContentGate,
    observe: &mut dyn FnMut(&[attemptdb_core::Event]),
) -> Result<IngestReport> {
    // Decided before any file is claimed. The key ring only ever gains a
    // key (a key that was read stays read), so "open" here cannot turn into
    // "closed" between this line and the events being stored.
    if gate.decision() == GateDecision::Hold {
        return Ok(IngestReport::default());
    }
    Ok(db.import_spool_with(|events| {
        gate.apply(events);
        observe(events);
    })?)
}

/// What sits in a database's spool: the regular `*.spool` files directly in
/// `spool/` (an inbox, claimed files a crashed import left, pending files
/// of contended hooks). One directory listing; nothing is read.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct SpoolUsage {
    pub files: usize,
    pub bytes: u64,
}

pub fn spool_usage(db_dir: &Path) -> SpoolUsage {
    let mut usage = SpoolUsage::default();
    let dir = db_dir.join(attemptdb_storage::format::SPOOL_DIR);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return usage;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("spool") {
            continue;
        }
        // Never through a symlink: the spool is a place hooks write, and a
        // planted link is not a file this process may size or read.
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.is_file() {
            usage.files += 1;
            usage.bytes += meta.len();
        }
    }
    usage
}

/// [`SpoolUsage`] plus how many events the spool files hold.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct SpoolWaiting {
    pub files: usize,
    pub bytes: u64,
    /// Event records found by walking the record headers (nothing is
    /// decoded or checked): what an import would find, less the records a
    /// torn tail or a bad record would cost.
    pub events: u64,
    /// The walk stopped at its budget; the real number is higher.
    pub events_at_least: bool,
}

/// How many records and how long [`spool_waiting`] walks before it reports
/// "at least".
const WAITING_MAX_RECORDS: u64 = 2_000_000;
const WAITING_MAX_TIME: std::time::Duration = std::time::Duration::from_secs(2);

/// Count the events waiting in the spool by walking record headers only
/// (12 bytes read and a seek per record), so a spool of hundreds of MiB is
/// counted in about a second, not decoded.
pub fn spool_waiting(db_dir: &Path) -> SpoolWaiting {
    use attemptdb_storage::format::{FILE_HEADER_LEN, MAGIC_SPOOL, RECORD_HEADER_LEN, record_type};
    use std::io::{BufReader, Read, Seek, SeekFrom};
    let usage = spool_usage(db_dir);
    let mut waiting = SpoolWaiting {
        files: usage.files,
        bytes: usage.bytes,
        ..Default::default()
    };
    let started = std::time::Instant::now();
    let dir = db_dir.join(attemptdb_storage::format::SPOOL_DIR);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return waiting;
    };
    let mut paths: Vec<_> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("spool"))
        .collect();
    paths.sort();
    for path in paths {
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        let len = meta.len();
        let Ok(file) = std::fs::File::open(&path) else {
            continue;
        };
        let mut reader = BufReader::with_capacity(1 << 16, file);
        let mut header = [0u8; FILE_HEADER_LEN];
        if reader.read_exact(&mut header).is_err() || header[0..4] != MAGIC_SPOOL {
            continue;
        }
        let mut offset = FILE_HEADER_LEN as u64;
        let mut head = [0u8; RECORD_HEADER_LEN];
        while reader.read_exact(&mut head).is_ok() {
            let payload = u64::from(u32::from_le_bytes([head[0], head[1], head[2], head[3]]));
            if payload > u64::from(attemptdb_storage::format::MAX_RECORD_PAYLOAD)
                || offset + RECORD_HEADER_LEN as u64 + payload > len
            {
                break;
            }
            if head[8] == record_type::EVENT {
                waiting.events += 1;
            }
            offset += RECORD_HEADER_LEN as u64 + payload;
            if reader.seek(SeekFrom::Start(offset)).is_err() {
                break;
            }
            if waiting.events >= WAITING_MAX_RECORDS || started.elapsed() > WAITING_MAX_TIME {
                waiting.events_at_least = true;
                return waiting;
            }
        }
    }
    waiting
}

/// The sentence `attempt status` and `attempt doctor` print while events
/// wait for the content key: how many, and why.
pub fn waiting_for_key_text(
    waiting: &SpoolWaiting,
    state: &crate::keys::EncryptionState,
) -> String {
    let cause = if state.problems.is_empty() {
        "no key source holds a key for this database".to_string()
    } else {
        state.problems.join("; ")
    };
    let age = Timestamp::parse(&state.since)
        .map(|t| {
            let secs = ((Timestamp::now().as_micros() - t.as_micros()) / 1_000_000).max(0);
            format!(" since {}", human_age(secs))
        })
        .unwrap_or_default();
    let count = if waiting.events == 1 && !waiting.events_at_least {
        "1 event is".to_string()
    } else {
        format!(
            "{}{} events are",
            waiting.events,
            if waiting.events_at_least { "+" } else { "" }
        )
    };
    format!(
        "{count} waiting for the content key{age} ({} in {} spool file(s)): {cause}. They stay in the spool, are not in queries yet, and are imported with their content as soon as the key can be read; after {} hours or {} MiB of spool they are stored without content instead. {}",
        human_size(waiting.bytes),
        waiting.files,
        crate::keys::HOLD_MAX_AGE.as_secs() / 3600,
        crate::keys::HOLD_MAX_SPOOL_BYTES / (1024 * 1024),
        state
            .advice
            .as_deref()
            .unwrap_or("Run `attempt keys status`."),
    )
}

fn human_age(secs: i64) -> String {
    if secs < 120 {
        format!("{secs} s ago")
    } else if secs < 7200 {
        format!("{} min ago", secs / 60)
    } else {
        format!("{} h ago", secs / 3600)
    }
}

fn human_size(bytes: u64) -> String {
    const MIB: f64 = 1024.0 * 1024.0;
    if bytes < 1024 * 1024 {
        format!("{:.0} KiB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MiB", bytes as f64 / MIB)
    }
}

/// The warning that says events wait for the content key, when the last
/// writer of this database recorded that it is holding. `None` otherwise
/// (one small file read).
pub fn waiting_for_key_warning(locator: &Locator) -> Option<String> {
    let identity = attemptdb_storage::Identity::load(&locator.db_dir).ok()?;
    let state = crate::keys::read_state(locator, identity.db_id)?;
    if state.state != "holding" {
        return None;
    }
    Some(waiting_for_key_text(
        &spool_waiting(&locator.db_dir),
        &state,
    ))
}

/// Open read-only (no lock). Fails if the database does not exist.
pub fn open_reader(locator: &Locator) -> Result<Database> {
    Ok(Database::open(
        &locator.db_dir,
        OpenOptions {
            read_only: true,
            keys: crate::keys::provider_for_db(locator, &locator.db_dir),
            ..Default::default()
        },
    )?)
}

/// Open for reading after importing whatever the hooks spooled. Falls back
/// to a read-only view when another writer holds the lock.
pub fn open_fresh(
    locator: &Locator,
    create: bool,
) -> Result<(Database, Option<IngestReport>, bool)> {
    match open_writer_guarded(locator, create) {
        Ok((mut db, gate)) => {
            let report = import_spool(&mut db, &gate)?;
            Ok((db, Some(report), false))
        }
        Err(crate::CaptureError::Storage(attemptdb_storage::StorageError::Locked(_))) => {
            let db = open_reader(locator)?;
            Ok((db, None, true))
        }
        Err(e) => Err(e),
    }
}

/// What [`import_pending`] found.
#[derive(Debug, Default)]
pub struct PendingImport {
    /// What the spool import did; `None` when another writer holds the lock
    /// (its spool is its business: the daemon imports continuously).
    pub report: Option<IngestReport>,
    /// Another process holds the single-writer lock, so this reader imported
    /// nothing and sees only what that writer has made durable.
    pub writer_busy: bool,
    /// What opening the writer had to say (a required key that is missing,
    /// a config file that could not be used): `status` prints them.
    pub warnings: Vec<String>,
}

/// Import whatever the hooks spooled, if the writer lock is free, and let go
/// of it again. Never waits: a held lock means the daemon (or another CLI) is
/// the writer and is importing, so the caller reads without importing.
///
/// A read has no business holding the writer lock. With the lock held for
/// the length of the read — seconds to minutes on a large database — the
/// daemon could not start and a second CLI read fell back to a degraded
/// view. The import is the only part of a read that
/// writes, and it is done when this returns; open the database for reading
/// with [`open_reader`] afterwards.
pub fn import_pending(locator: &Locator) -> Result<PendingImport> {
    match open_writer_guarded(locator, false) {
        Ok((mut db, gate)) => {
            // Through the gate: spooled events keep no content a required
            // key is missing for, and secrets are masked, exactly as when the
            // daemon imports them.
            let report = import_spool(&mut db, &gate)?;
            let mut warnings = std::mem::take(&mut db.warnings);
            warnings.extend(waiting_for_key_warning(locator));
            // Dropping the handle releases the lock; the events are in the WAL
            // (synced by `import_spool`) and a reader replays it.
            drop(db);
            Ok(PendingImport {
                report: Some(report),
                writer_busy: false,
                warnings,
            })
        }
        Err(crate::CaptureError::Storage(attemptdb_storage::StorageError::Locked(_))) => {
            // The daemon is the writer: whether it is holding events for
            // the content key is in the state file it keeps.
            Ok(PendingImport {
                report: None,
                writer_busy: true,
                warnings: waiting_for_key_warning(locator).into_iter().collect(),
            })
        }
        Err(e) => Err(e),
    }
}

/// [`import_pending`], then a read-only handle: what a command that only
/// reads opens. `(database, import report, writer_busy)`; the handle holds no
/// lock whichever way it came.
pub fn open_for_read(locator: &Locator) -> Result<(Database, Option<IngestReport>, bool)> {
    let pending = import_pending(locator)?;
    let mut db = open_reader(locator)?;
    db.warnings.extend(pending.warnings);
    Ok((db, pending.report, pending.writer_busy))
}

/// Store events through the running daemon when there is one, otherwise by
/// opening the writer directly (import spool, ingest, flush). Used by CLI
/// commands that write (corrections, retractions, imports) so they work
/// while the daemon holds the writer lock.
pub fn write_events(
    locator: &Locator,
    mut events: Vec<attemptdb_core::Event>,
) -> Result<IngestReport> {
    if crate::ipc::daemon_reachable(locator) {
        match crate::ipc::Client::send_events(locator, &events) {
            Ok(ack) => {
                return Ok(IngestReport {
                    accepted: ack.accepted.len(),
                    duplicates: ack.duplicate.len(),
                    ..Default::default()
                });
            }
            Err(crate::ipc::IpcError::Nack(n)) if n.code == crate::keys::KEY_UNAVAILABLE_CODE => {
                // The daemon is holding events for a content key it cannot
                // read, and the lock is its: do what a hook does and spool
                // them. They are imported with the rest once the key reads.
                return spool_held(locator, &events);
            }
            Err(e) => {
                // Fall through to the direct path; if the daemon really holds
                // the lock the open below reports it clearly.
                eprintln!("daemon did not accept the events ({e}); writing directly");
            }
        }
    }
    let (mut db, gate) = open_writer_guarded(locator, false)?;
    import_spool(&mut db, &gate)?;
    if gate.decision() == GateDecision::Hold {
        drop(db);
        return spool_held(locator, &events);
    }
    gate.apply(&mut events);
    let report = db.ingest(events)?;
    db.flush()?;
    Ok(report)
}

/// Park `events` in the spool while the content key cannot be read. Nothing
/// reaches the database yet, so the report counts none as accepted.
fn spool_held(locator: &Locator, events: &[attemptdb_core::Event]) -> Result<IngestReport> {
    attemptdb_storage::SpoolWriter::new(&locator.db_dir)?.append_with(events, true)?;
    eprintln!(
        "note: the content key cannot be read right now, so {} event(s) were put in the spool instead of the database; they are imported (with their content) as soon as it can be (`attempt status` says how many wait)",
        events.len()
    );
    Ok(IngestReport::default())
}
