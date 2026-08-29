//! Spool: the crash-safe inbox written by short-lived hook processes.
//!
//! Hook invocations run concurrently (subagents, parallel tool calls) and
//! must finish in milliseconds, so they never open the database. They append
//! one framed record to `spool/inbox.spool` under a short advisory lock. The
//! database writer later claims the inbox by renaming it and imports it.
//! Ingestion is idempotent by event id, so a crash between import and delete
//! cannot duplicate events.

use crate::format::{MAGIC_SPOOL, SPOOL_DIR};
use crate::frame::{FrameReader, FrameWriter, Record};
use crate::{IoAt, Result};
use attemptdb_core::Event;
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};

pub const INBOX_FILE: &str = "inbox.spool";

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

    /// Append events durably to the inbox. Holds the spool lock for the
    /// duration of the write so concurrent hooks never interleave frames.
    pub fn append(&self, events: &[Event]) -> Result<PathBuf> {
        let lock_path = self.dir.join("inbox.lock");
        let lock = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .at(&lock_path)?;
        lock.lock().at(&lock_path)?;
        let path = self.dir.join(INBOX_FILE);
        let result = (|| {
            let mut w = FrameWriter::open(&path, MAGIC_SPOOL)?;
            let records = events.iter().map(Record::event).collect::<Result<Vec<_>>>()?;
            w.append(&records)?;
            w.sync()?;
            Ok(())
        })();
        let _ = lock.unlock();
        result.map(|_| path)
    }
}

pub struct SpoolReader {
    dir: PathBuf,
}

/// A claimed spool file ready for import.
pub struct ClaimedSpool {
    pub path: PathBuf,
    pub events: Vec<Event>,
    pub undecodable: usize,
    pub truncated: bool,
}

impl SpoolReader {
    pub fn new(root: &Path) -> Result<Self> {
        let dir = SpoolWriter::dir(root);
        std::fs::create_dir_all(&dir).at(&dir)?;
        Ok(Self { dir })
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
    /// then read every claimed file. Files that were claimed earlier but not
    /// deleted (crash mid-import) are picked up too.
    pub fn claim(&self) -> Result<Vec<ClaimedSpool>> {
        let lock_path = self.dir.join("inbox.lock");
        let lock = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .at(&lock_path)?;
        lock.lock().at(&lock_path)?;
        let inbox = self.dir.join(INBOX_FILE);
        if inbox.exists() {
            let claimed = self
                .dir
                .join(format!("claimed-{}.spool", uuid::Uuid::now_v7().simple()));
            std::fs::rename(&inbox, &claimed).at(&inbox)?;
        }
        let _ = lock.unlock();
        let mut out = Vec::new();
        for path in self.list_files()? {
            if path.file_name().and_then(|n| n.to_str()) == Some(INBOX_FILE) {
                continue; // a new inbox created after our rename
            }
            let scan = FrameReader::scan(&path, MAGIC_SPOOL)?;
            let mut events = Vec::with_capacity(scan.records.len());
            let mut undecodable = 0;
            for r in scan.records {
                if r.record_type == crate::format::record_type::EVENT {
                    match r.decode_event() {
                        Ok(ev) => events.push(ev),
                        Err(_) => undecodable += 1,
                    }
                }
            }
            out.push(ClaimedSpool { path, events, undecodable, truncated: scan.truncated_at.is_some() });
        }
        Ok(out)
    }

    /// Remove a claimed file after its events are durable in the database.
    pub fn release(&self, claimed: &ClaimedSpool) -> Result<()> {
        std::fs::remove_file(&claimed.path).at(&claimed.path)
    }
}

