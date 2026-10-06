//! Opening the writer and importing pending spool files.

use crate::config::{Config, DeviceRecord};
use crate::keys::{ContentGate, KeyStoreOptions, NoticeLevel};
use crate::locator::Locator;
use crate::{Result, io_at};
use attemptdb_storage::{Database, IngestReport, OpenOptions};

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
pub fn import_spool(db: &mut Database, gate: &ContentGate) -> Result<IngestReport> {
    Ok(db.import_spool_with(|events| {
        gate.apply(events);
    })?)
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
}

/// Import whatever the hooks spooled, if the writer lock is free, and let go
/// of it again. Never waits: a held lock means the daemon (or another CLI) is
/// the writer and is importing, so the caller reads without importing.
///
/// A read has no business holding the writer lock. With the lock held for
/// the length of the read — seconds to minutes on a large database — the
/// daemon could not start and a second CLI read fell back to a degraded
/// view (REPORT.md §4.1, §7.5). The import is the only part of a read that
/// writes, and it is done when this returns; open the database for reading
/// with [`open_reader`] afterwards.
pub fn import_pending(locator: &Locator) -> Result<PendingImport> {
    match open_writer(locator, false) {
        Ok(mut db) => {
            let report = db.import_spool()?;
            // Dropping the handle releases the lock; the events are in the WAL
            // (synced by `import_spool`) and a reader replays it.
            drop(db);
            Ok(PendingImport {
                report: Some(report),
                writer_busy: false,
            })
        }
        Err(crate::CaptureError::Storage(attemptdb_storage::StorageError::Locked(_))) => {
            Ok(PendingImport {
                report: None,
                writer_busy: true,
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
    let db = open_reader(locator)?;
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
            Err(e) => {
                // Fall through to the direct path; if the daemon really holds
                // the lock the open below reports it clearly.
                eprintln!("daemon did not accept the events ({e}); writing directly");
            }
        }
    }
    let (mut db, gate) = open_writer_guarded(locator, false)?;
    import_spool(&mut db, &gate)?;
    gate.apply(&mut events);
    let report = db.ingest(events)?;
    db.flush()?;
    Ok(report)
}
