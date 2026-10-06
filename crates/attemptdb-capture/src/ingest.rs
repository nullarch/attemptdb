//! Opening the writer and importing pending spool files.

use crate::config::{Config, DeviceRecord};
use crate::keys::{ContentGate, KeyStoreOptions, NoticeLevel};
use crate::locator::Locator;
use crate::{Result, io_at};
use attemptdb_storage::{Database, IngestReport, OpenOptions, SpoolReader};

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
    for notice in keys.gate.take_notices() {
        if notice.level == NoticeLevel::Error {
            db.warnings.push(notice.message);
        }
    }
    if let Some(why) = &config.load_error {
        db.warnings.push(format!(
            "{why}; capturing metadata only until it is fixed (`attempt doctor`)"
        ));
    }
    Ok((db, keys.gate))
}

/// Import the spool through `gate`. While the gate is open this is
/// [`Database::import_spool`]. While it withholds content, the spool files
/// are claimed here instead, so that the events lose their content before
/// they reach the write-ahead log; the storage engine would store them as
/// they are. Under `Relaxed` durability the WAL is synced by the flush
/// that follows rather than before a spool file is deleted.
pub fn import_spool(db: &mut Database, gate: &ContentGate) -> Result<IngestReport> {
    if !gate.is_withholding() {
        return Ok(db.import_spool()?);
    }
    let reader = SpoolReader::new(db.root())?;
    let mut report = IngestReport::default();
    for mut claimed in reader.claim()? {
        report.spool_files += 1;
        report.undecodable += claimed.undecodable;
        if claimed.truncated {
            db.warnings.push(format!(
                "spool file {} had a torn tail; valid prefix imported",
                claimed.path.display()
            ));
        }
        let mut events = std::mem::take(&mut claimed.events);
        gate.apply(&mut events);
        let r = db.ingest(events)?;
        report.accepted += r.accepted;
        report.duplicates += r.duplicates;
        report.bytes += r.bytes;
        report.flushed_segments += r.flushed_segments;
        report.redactions += r.redactions;
        reader.release(&claimed)?;
    }
    Ok(report)
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
