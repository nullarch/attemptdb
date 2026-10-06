//! Spool: the crash-safe inbox written by short-lived hook processes.
//!
//! Hook invocations run concurrently (subagents, parallel tool calls) and
//! must finish in milliseconds, so they never open the database. They append
//! one framed record to `spool/inbox.spool` under a short advisory lock. The
//! database writer later claims the inbox by renaming it and imports it.
//! Ingestion is idempotent by event id, so a crash between import and delete
//! cannot duplicate events.
//!
//! Nothing the reader cannot import is ever deleted. A record that passes its
//! CRC but does not decode as an event is written under `spool/quarantine/`
//! before its file is released; a file the reader cannot scan at all (bad
//! magic, a newer frame format, not a regular file) is moved there whole and
//! the other files import as usual. The quarantine is bounded
//! ([`QUARANTINE_MAX_FILES`], [`QUARANTINE_MAX_BYTES`]); when it is full the
//! file stays in the spool instead of being released.
//!
//! Every file the spool creates or opens goes through `safe_fs`: a spool
//! directory can come from a repository, so a planted symlink must not be
//! followed.

use crate::failpoint;
use crate::format::{FILE_HEADER_LEN, MAGIC_SPOOL, SPOOL_DIR};
use crate::frame::{FrameReader, FrameWriter, Record};
use crate::safe_fs;
use crate::{IoAt, Result, StorageError};
use attemptdb_core::{Event, Timestamp};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

pub const INBOX_FILE: &str = "inbox.spool";
/// Sidecar holding the inbox length after the last successful append
/// (u64 LE). Lets the next appender validate only the tail.
pub const INBOX_COMMITTED_FILE: &str = "inbox.spool.committed";
/// Subdirectory of `spool/` that receives records and files the importer
/// could not use. Nothing in it is read back automatically.
pub const QUARANTINE_DIR: &str = "quarantine";
/// The quarantine stops accepting new record files beyond this many files...
pub const QUARANTINE_MAX_FILES: usize = 512;
/// ...or this many bytes, so a stuck producer cannot fill the disk. A full
/// quarantine blocks releasing the affected spool file; it never discards.
pub const QUARANTINE_MAX_BYTES: u64 = 256 * 1024 * 1024;

pub struct SpoolWriter {
    dir: PathBuf,
}

impl SpoolWriter {
    pub fn dir(root: &Path) -> PathBuf {
        root.join(SPOOL_DIR)
    }

    pub fn new(root: &Path) -> Result<Self> {
        let dir = Self::dir(root);
        std::fs::create_dir_all(&dir).at(&dir)?;
        Ok(Self { dir })
    }

    /// Append events to the inbox. Holds the spool lock for the duration of
    /// the write so concurrent hooks never interleave frames.
    ///
    /// `sync` controls whether the append is fsynced. The spool is a
    /// transport, not the durability boundary (that is the WAL, see
    /// `docs/storage-format.md`): without `sync` the events survive a hook
    /// process crash (they are in the OS page cache) but not a power loss
    /// before the next import. That is the default because fsync dominates
    /// hook latency on most systems.
    pub fn append(&self, events: &[Event]) -> Result<PathBuf> {
        self.append_with(events, false)
    }

    pub fn append_with(&self, events: &[Event], sync: bool) -> Result<PathBuf> {
        // Encode first: an event that does not fit in a record is refused
        // before any file is touched, and without waiting for the lock.
        let records = events
            .iter()
            .map(Record::event)
            .collect::<Result<Vec<_>>>()?;
        let lock_path = self.dir.join("inbox.lock");
        // A lock file that cannot be opened safely (a planted symlink) must
        // not cost the agent its event: publish a private file instead.
        let Ok(lock) = safe_fs::open_lock(&lock_path) else {
            return self.append_private(&records, sync);
        };
        match lock.try_lock() {
            Ok(()) => {}
            // A stopped or slow writer must not hold an agent's hook hostage.
            // Publish a complete private frame file with the existing format;
            // the reader already imports every non-inbox .spool file.
            Err(std::fs::TryLockError::WouldBlock) => return self.append_private(&records, sync),
            Err(std::fs::TryLockError::Error(e)) => return Err(e).at(&lock_path),
        }
        let path = self.dir.join(INBOX_FILE);
        let committed_path = self.dir.join(INBOX_COMMITTED_FILE);
        let result = (|| {
            let committed = read_committed(&committed_path);
            let mut w = FrameWriter::open_trusted(&path, MAGIC_SPOOL, committed)?;
            w.append(&records)?;
            failpoint::hit(failpoint::SPOOL_APPEND_AFTER_WRITE);
            if sync {
                w.sync()?;
            }
            failpoint::hit(failpoint::SPOOL_COMMITTED_BEFORE_WRITE);
            write_committed(&committed_path, w.len())?;
            Ok(())
        })();
        let _ = lock.unlock();
        match result {
            Ok(()) => Ok(path),
            // The inbox cannot be appended to (corrupt, a frame format this
            // build cannot extend, or a planted link). The importer will move
            // it aside on its next sweep; until then the event goes to a
            // private file instead of being lost.
            Err(e) if inbox_unusable(&e) => self.append_private(&records, sync),
            Err(e) => Err(e),
        }
    }

    fn append_private(&self, records: &[Record], sync: bool) -> Result<PathBuf> {
        let path = self
            .dir
            .join(format!("pending-{}.spool", uuid::Uuid::now_v7().simple()));
        let tmp = path.with_extension("tmp");
        let result = (|| {
            let mut writer = FrameWriter::open_trusted(&tmp, MAGIC_SPOOL, None)?;
            writer.append(records)?;
            if sync {
                writer.sync()?;
            }
            drop(writer);
            std::fs::rename(&tmp, &path).at(&path)?;
            if sync {
                crate::wal::sync_dir(&self.dir)?;
            }
            Ok(path)
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        result
    }
}

/// Errors that mean "this file cannot be appended to", as opposed to "this
/// write failed" (disk full, permissions).
fn inbox_unusable(e: &StorageError) -> bool {
    match e {
        StorageError::Corrupt { .. } | StorageError::UnsupportedFormat { .. } => true,
        StorageError::Io { source, .. } => source.kind() == std::io::ErrorKind::InvalidInput,
        _ => false,
    }
}

pub struct SpoolReader {
    dir: PathBuf,
}

/// A claimed spool file ready for import.
pub struct ClaimedSpool {
    pub path: PathBuf,
    pub events: Vec<Event>,
    /// Records that passed their CRC but did not decode as events (or are of
    /// a record type this build does not know).
    pub undecodable: usize,
    pub truncated: bool,
    /// What the reader did or could not do (quarantine moves, failures), for
    /// the caller to surface as warnings.
    pub notes: Vec<String>,
    /// The raw undecodable records. [`SpoolReader::release`] writes them to
    /// the quarantine before it removes the file.
    undecodable_records: Vec<Record>,
    /// The whole file was moved under `quarantine/`: nothing to import and
    /// nothing to release.
    moved_to: Option<PathBuf>,
    /// The file could not be read for a reason that may pass (I/O error). It
    /// stays in the spool and is retried; `release` leaves it alone.
    skipped: bool,
}

impl ClaimedSpool {
    /// Where the whole file went, when it was unscannable and was moved to
    /// the quarantine instead of imported.
    pub fn moved_to_quarantine(&self) -> Option<&Path> {
        self.moved_to.as_deref()
    }

    /// True when the file could not be read this time and stays in the spool.
    pub fn skipped(&self) -> bool {
        self.skipped
    }

    fn empty(path: PathBuf) -> Self {
        Self {
            path,
            events: Vec::new(),
            undecodable: 0,
            truncated: false,
            notes: Vec::new(),
            undecodable_records: Vec::new(),
            moved_to: None,
            skipped: false,
        }
    }
}

impl SpoolReader {
    pub fn new(root: &Path) -> Result<Self> {
        let dir = SpoolWriter::dir(root);
        std::fs::create_dir_all(&dir).at(&dir)?;
        Ok(Self { dir })
    }

    pub fn quarantine_dir(&self) -> PathBuf {
        self.dir.join(QUARANTINE_DIR)
    }

    /// Whether any spool data is waiting.
    pub fn has_pending(&self) -> bool {
        self.list_files().map(|v| !v.is_empty()).unwrap_or(false)
    }

    fn list_files(&self) -> Result<Vec<PathBuf>> {
        let mut out = Vec::new();
        for entry in std::fs::read_dir(&self.dir).at(&self.dir)? {
            let entry = entry.at(&self.dir)?;
            let p = entry.path();
            if p.extension().and_then(|e| e.to_str()) == Some("spool") {
                out.push(p);
            }
        }
        out.sort();
        Ok(out)
    }

    /// Atomically take the inbox away from writers (rename under the lock),
    /// then list every spool file waiting. Files that were claimed earlier
    /// but not deleted (crash mid-import) are included. Nothing is read:
    /// [`SpoolReader::load`] reads one file at a time, so an importer's
    /// memory is bounded by the largest single file.
    pub fn claim_paths(&self) -> Result<Vec<PathBuf>> {
        let lock_path = self.dir.join("inbox.lock");
        let lock = match safe_fs::open_lock(&lock_path) {
            Ok(f) => f,
            // The lock file is a planted symlink or similar: the importer
            // owns this directory, so unlink the entry (never its target)
            // and make a fresh one instead of failing every sweep.
            Err(e) if e.kind() == std::io::ErrorKind::InvalidInput => {
                std::fs::remove_file(&lock_path).at(&lock_path)?;
                safe_fs::open_lock(&lock_path).at(&lock_path)?
            }
            Err(e) => return Err(e).at(&lock_path),
        };
        lock.lock().at(&lock_path)?;
        let inbox = self.dir.join(INBOX_FILE);
        // `symlink_metadata`: a dangling symlink named like the inbox still
        // has to be moved out of the writers' way.
        if std::fs::symlink_metadata(&inbox).is_ok() {
            let claimed = self
                .dir
                .join(format!("claimed-{}.spool", uuid::Uuid::now_v7().simple()));
            std::fs::rename(&inbox, &claimed).at(&inbox)?;
            let _ = std::fs::remove_file(self.dir.join(INBOX_COMMITTED_FILE));
        }
        let _ = lock.unlock();
        Ok(self
            .list_files()?
            .into_iter()
            .filter(|p| p.file_name().and_then(|n| n.to_str()) != Some(INBOX_FILE))
            .collect()) // a new inbox created after our rename is skipped
    }

    /// Claim and read every pending file at once. Convenient for tests and
    /// small tools; the importer uses [`SpoolReader::claim_paths`] and
    /// [`SpoolReader::load`] so it never holds more than one file.
    pub fn claim(&self) -> Result<Vec<ClaimedSpool>> {
        Ok(self.claim_paths()?.iter().map(|p| self.load(p)).collect())
    }

    /// Read one claimed file. Never fails the sweep: a file that cannot be
    /// scanned for good is moved into the quarantine and reported through
    /// `notes`; one that cannot be read right now is left in place
    /// (`skipped`).
    pub fn load(&self, path: &Path) -> ClaimedSpool {
        let mut out = ClaimedSpool::empty(path.to_path_buf());
        let meta = match std::fs::symlink_metadata(path) {
            Ok(m) => m,
            Err(e) => {
                out.skipped = true;
                out.notes
                    .push(format!("cannot stat spool file {}: {e}", path.display()));
                return out;
            }
        };
        if !meta.is_file() {
            // A symlink, FIFO or directory named *.spool is not something a
            // hook wrote. Never open it; move it aside.
            self.quarantine_whole(&mut out, "it is not a regular file");
            return out;
        }
        // A hook that died between creating the inbox and writing its
        // header leaves a file too short to hold a record. Import it as
        // empty-and-torn so it is reported and released, instead of
        // failing every import until someone deletes it by hand.
        if meta.len() < FILE_HEADER_LEN as u64 {
            out.truncated = true;
            return out;
        }
        let scan = match FrameReader::scan(path, MAGIC_SPOOL) {
            Ok(scan) => scan,
            Err(e @ (StorageError::Corrupt { .. } | StorageError::UnsupportedFormat { .. })) => {
                self.quarantine_whole(&mut out, &e.to_string());
                return out;
            }
            Err(e) => {
                out.skipped = true;
                out.notes
                    .push(format!("cannot read spool file {}: {e}", path.display()));
                return out;
            }
        };
        out.truncated = scan.truncated_at.is_some();
        out.events.reserve(scan.records.len());
        for r in scan.records {
            match r.record_type {
                crate::format::record_type::EVENT => match r.decode_event() {
                    Ok(ev) => out.events.push(ev),
                    Err(_) => out.undecodable_records.push(r),
                },
                // Writers never put checkpoints in a spool; tolerated.
                crate::format::record_type::CHECKPOINT => {}
                // A record type from a newer build. Keep it like any other
                // record this build cannot use.
                _ => out.undecodable_records.push(r),
            }
        }
        out.undecodable = out.undecodable_records.len();
        out
    }

    /// Release a claimed file once its events are durable in the database.
    ///
    /// Undecodable records are written to the quarantine first; if that
    /// fails the file is NOT removed and the error is returned, so no record
    /// is ever dropped silently. Returns how many records were quarantined.
    /// A file that was moved whole or skipped needs no release.
    pub fn release(&self, claimed: &ClaimedSpool) -> Result<usize> {
        if claimed.moved_to.is_some() || claimed.skipped {
            return Ok(0);
        }
        let saved = if claimed.undecodable_records.is_empty() {
            0
        } else {
            self.quarantine_records(&claimed.path, &claimed.undecodable_records)?;
            claimed.undecodable_records.len()
        };
        std::fs::remove_file(&claimed.path).at(&claimed.path)?;
        Ok(saved)
    }

    /// Move a file this build cannot scan into `quarantine/`. A rename moves
    /// no data, so it needs no budget; a failed rename leaves the file for
    /// the next sweep.
    fn quarantine_whole(&self, out: &mut ClaimedSpool, why: &str) {
        let qdir = self.quarantine_dir();
        let target = qdir.join(quarantine_name(&out.path, "spool"));
        let moved = std::fs::create_dir_all(&qdir)
            .and_then(|_| std::fs::rename(&out.path, &target))
            .map(|_| {
                let _ = crate::wal::sync_dir(&qdir);
            });
        match moved {
            Ok(()) => {
                out.notes.push(format!(
                    "spool file {} could not be imported ({why}); moved to {}",
                    out.path.display(),
                    target.display()
                ));
                out.moved_to = Some(target);
            }
            Err(e) => {
                out.skipped = true;
                out.notes.push(format!(
                    "spool file {} could not be imported ({why}) and could not be moved to {}: {e}",
                    out.path.display(),
                    qdir.display()
                ));
            }
        }
    }

    /// Write `records` as a framed spool file under `quarantine/`: the
    /// original record bytes, re-encodable by renaming the file to `.spool`
    /// and moving it back once a build that understands them is installed.
    fn quarantine_records(&self, source: &Path, records: &[Record]) -> Result<PathBuf> {
        let qdir = self.quarantine_dir();
        std::fs::create_dir_all(&qdir).at(&qdir)?;
        let need: u64 =
            FILE_HEADER_LEN as u64 + records.iter().map(|r| r.encoded_len() as u64).sum::<u64>();
        let (files, bytes) = quarantine_usage(&qdir)?;
        if files >= QUARANTINE_MAX_FILES || bytes + need > QUARANTINE_MAX_BYTES {
            return Err(StorageError::Other(format!(
                "spool quarantine {} is full ({files} files, {bytes} bytes; limits {QUARANTINE_MAX_FILES} files / {QUARANTINE_MAX_BYTES} bytes): {} stays in the spool until it is cleared",
                qdir.display(),
                source.display()
            )));
        }
        let target = qdir.join(quarantine_name(source, "rec"));
        let tmp = target.with_extension("tmp");
        let result = (|| {
            let mut w = FrameWriter::open_trusted(&tmp, MAGIC_SPOOL, None)?;
            w.append(records)?;
            w.sync_all()?;
            drop(w);
            std::fs::rename(&tmp, &target).at(&target)?;
            crate::wal::sync_dir(&qdir)
        })();
        match result {
            Ok(()) => Ok(target),
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                Err(e)
            }
        }
    }
}

/// `<stem>-<micros>.<ext>` plus a random tail, unique across retries.
fn quarantine_name(source: &Path, ext: &str) -> String {
    let stem = source
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("spool");
    let rand = &uuid::Uuid::now_v7().simple().to_string()[24..];
    format!("{stem}-{}-{rand}.{ext}", Timestamp::now().as_micros())
}

/// Files and bytes currently held by the quarantine (temp files count: they
/// are space too).
fn quarantine_usage(qdir: &Path) -> Result<(usize, u64)> {
    let mut files = 0usize;
    let mut bytes = 0u64;
    for entry in std::fs::read_dir(qdir).at(qdir)? {
        let entry = entry.at(qdir)?;
        files += 1;
        bytes += entry.metadata().map(|m| m.len()).unwrap_or(0);
    }
    Ok((files, bytes))
}

fn read_committed(path: &Path) -> Option<u64> {
    let file = safe_fs::open_read(path).ok()?;
    let mut bytes = Vec::with_capacity(9);
    file.take(9).read_to_end(&mut bytes).ok()?;
    if bytes.len() != 8 {
        return None;
    }
    Some(u64::from_le_bytes(bytes.try_into().ok()?))
}

/// Write the committed length atomically (tmp + rename) so a torn write can
/// never produce a misleading hint. A missing or unreadable sidecar simply
/// means "scan the whole inbox".
///
/// The temp file is created with `create_new` after unlinking whatever stood
/// there: a symlink planted at the temp name (`std::fs::write` would follow
/// it and truncate its target) is removed, never written through.
fn write_committed(path: &Path, len: u64) -> Result<()> {
    let tmp = path.with_extension("committed.tmp");
    let mut file = safe_fs::create_new_replacing(&tmp).at(&tmp)?;
    file.write_all(&len.to_le_bytes()).at(&tmp)?;
    drop(file);
    std::fs::rename(&tmp, path).at(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use attemptdb_core::event::Provider;
    use attemptdb_core::{CaptureMode, DeviceId, EventKind, ProjectRef};
    use std::fs::OpenOptions;

    #[test]
    fn contended_append_publishes_private_spool_without_waiting() {
        let root = tempfile::tempdir().unwrap();
        let writer = SpoolWriter::new(root.path()).unwrap();
        let lock_path = writer.dir.join("inbox.lock");
        let lock = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(lock_path)
            .unwrap();
        lock.lock().unwrap();
        let event = ev(1);
        let id = event.event_id;
        let (tx, rx) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            tx.send(writer.append_with(&[event], true)).unwrap();
        });
        let result = rx.recv_timeout(std::time::Duration::from_secs(2));
        // Release even on a regression so the failing test never hangs.
        drop(lock);
        thread.join().unwrap();
        let path = result
            .expect("hook waited for another process's spool lock")
            .unwrap();
        assert!(
            path.file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("pending-")
        );
        let reader = SpoolReader::new(root.path()).unwrap();
        let claimed = reader.claim().unwrap();
        assert_eq!(claimed.len(), 1);
        assert!(!claimed[0].truncated);
        assert_eq!(claimed[0].events[0].event_id, id);
        reader.release(&claimed[0]).unwrap();
        assert!(!reader.has_pending());
    }

    fn ev(i: u32) -> Event {
        let dev = DeviceId::nil();
        let mut e = Event::new(
            dev,
            Provider::Cursor,
            "stop",
            EventKind::TurnStopped,
            ProjectRef::derive("/p", None, &dev),
            "c",
            CaptureMode::MetadataOnly,
            "t",
        );
        e.attrs.insert("i".into(), serde_json::json!(i));
        e
    }

    #[test]
    fn append_claim_release_with_committed_hint() {
        let dir = tempfile::tempdir().unwrap();
        let w = SpoolWriter::new(dir.path()).unwrap();
        for i in 0..5 {
            w.append(&[ev(i)]).unwrap();
        }
        let committed =
            read_committed(&SpoolWriter::dir(dir.path()).join(INBOX_COMMITTED_FILE)).unwrap();
        assert_eq!(
            committed,
            std::fs::metadata(SpoolWriter::dir(dir.path()).join(INBOX_FILE))
                .unwrap()
                .len()
        );
        let r = SpoolReader::new(dir.path()).unwrap();
        assert!(r.has_pending());
        let claimed = r.claim().unwrap();
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].events.len(), 5);
        assert!(
            !SpoolWriter::dir(dir.path())
                .join(INBOX_COMMITTED_FILE)
                .exists()
        );
        // New appends after the claim start a fresh inbox.
        w.append(&[ev(9)]).unwrap();
        r.release(&claimed[0]).unwrap();
        let again = r.claim().unwrap();
        assert_eq!(again.len(), 1);
        assert_eq!(again[0].events.len(), 1);
    }
}
