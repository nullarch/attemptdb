//! Opening the writer and importing pending spool files.

use crate::config::DeviceRecord;
use crate::locator::Locator;
use crate::{Result, io_at};
use attemptdb_storage::{Database, IngestReport, OpenOptions};

/// Open (or create) the database the locator points at, as the writer.
pub fn open_writer(locator: &Locator, create: bool) -> Result<Database> {
    let mut opts = OpenOptions { create, ..Default::default() };
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
    Ok(Database::open(&locator.db_dir, OpenOptions { read_only: true, ..Default::default() })?)
}

/// Open for reading after importing whatever the hooks spooled. Falls back
/// to a read-only view when another writer holds the lock.
pub fn open_fresh(locator: &Locator, create: bool) -> Result<(Database, Option<IngestReport>, bool)> {
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
