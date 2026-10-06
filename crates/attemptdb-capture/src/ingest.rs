//! Opening the writer and importing pending spool files.

use crate::config::DeviceRecord;
use crate::locator::Locator;
use crate::{Result, io_at};
use attemptdb_storage::{Database, IngestReport, OpenOptions};

/// Open (or create) the database the locator points at, as the writer.
pub fn open_writer(locator: &Locator, create: bool) -> Result<Database> {
    let mut opts = OpenOptions {
        create,
        keys: crate::keys::provider_for_db(locator, &locator.db_dir),
        ..Default::default()
    };
    if create && !Database::exists(&locator.db_dir) {
        let device = DeviceRecord::load_or_create(&locator.paths.data_dir)?;
        opts.device_id = Some(device.device_id);
        if let Some(parent) = locator.db_dir.parent() {
            std::fs::create_dir_all(parent).map_err(|e| io_at(parent, e))?;
        }
    }
    Ok(Database::open(&locator.db_dir, opts)?)
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
    match open_writer(locator, create) {
        Ok(mut db) => {
            let report = db.import_spool()?;
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
pub fn write_events(locator: &Locator, events: Vec<attemptdb_core::Event>) -> Result<IngestReport> {
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
    let mut db = open_writer(locator, false)?;
    db.import_spool()?;
    let report = db.ingest(events)?;
    db.flush()?;
    Ok(report)
}
