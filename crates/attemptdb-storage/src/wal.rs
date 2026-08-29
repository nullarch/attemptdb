//! Write-ahead log: numbered framed files under `wal/`.
//!
//! Only one writer exists per database (guarded by `LOCK`). Events are
//! appended in ingestion order after the writer assigned `source_seq`, `hlc`
//! and `ingested_at`, so replaying the WAL reproduces the memtable exactly.

use crate::format::{MAGIC_WAL, WAL_DIR};
use crate::frame::{FrameReader, FrameWriter, Record};
use crate::{IoAt, Result};
use attemptdb_core::Event;
use std::path::{Path, PathBuf};

pub struct Wal {
    dir: PathBuf,
    active: FrameWriter,
    active_number: u64,
}

#[derive(Debug, Default)]
pub struct WalRecovery {
    pub events: Vec<Event>,
    pub files_scanned: usize,
    pub truncated_files: Vec<PathBuf>,
    pub undecodable_records: usize,
}

impl Wal {
    pub fn dir(root: &Path) -> PathBuf {
        root.join(WAL_DIR)
    }

    fn file_name(n: u64) -> String {
        format!("{n:06}.wal")
    }

    /// Open the WAL, replaying every existing file (oldest first).
    pub fn open(root: &Path) -> Result<(Self, WalRecovery)> {
        let dir = Self::dir(root);
        std::fs::create_dir_all(&dir).at(&dir)?;
        let mut numbers = list_numbers(&dir)?;
        numbers.sort_unstable();
        let mut recovery = WalRecovery::default();
        for n in &numbers {
            let path = dir.join(Self::file_name(*n));
            let scan = FrameReader::scan(&path, MAGIC_WAL)?;
            recovery.files_scanned += 1;
            if scan.truncated_at.is_some() {
                recovery.truncated_files.push(path.clone());
            }
            for r in scan.records {
                if r.record_type == crate::format::record_type::EVENT {
                    match r.decode_event() {
                        Ok(ev) => recovery.events.push(ev),
                        Err(_) => recovery.undecodable_records += 1,
                    }
                }
            }
        }
        let active_number = numbers.last().copied().unwrap_or(1);
        let active = FrameWriter::open(&dir.join(Self::file_name(active_number)), MAGIC_WAL)?;
        Ok((Self { dir, active, active_number }, recovery))
    }

    pub fn append(&mut self, events: &[Event]) -> Result<usize> {
        let records = events.iter().map(Record::event).collect::<Result<Vec<_>>>()?;
        let bytes = records.iter().map(Record::encoded_len).sum();
        self.active.append(&records)?;
        Ok(bytes)
    }

    pub fn sync(&mut self) -> Result<()> {
        self.active.sync()
    }

    pub fn active_file(&self) -> PathBuf {
        self.active.path().to_path_buf()
    }

    pub fn active_number(&self) -> u64 {
        self.active_number
    }

    pub fn active_len(&self) -> u64 {
        self.active.len()
    }

    /// Start a new WAL file. Older files remain until `truncate_before`.
    pub fn rotate(&mut self) -> Result<()> {
        let next = self.active_number + 1;
        let path = self.dir.join(Self::file_name(next));
        self.active.sync_all()?;
        self.active = FrameWriter::open(&path, MAGIC_WAL)?;
        self.active_number = next;
        sync_dir(&self.dir)?;
        Ok(())
    }

    /// Delete every WAL file numbered below `keep_from`. Called only after
    /// the manifest generation covering those events is durable.
    pub fn truncate_before(&mut self, keep_from: u64) -> Result<usize> {
        let mut removed = 0;
        for n in list_numbers(&self.dir)? {
            if n < keep_from {
                let path = self.dir.join(Self::file_name(n));
                std::fs::remove_file(&path).at(&path)?;
                removed += 1;
            }
        }
        if removed > 0 {
            sync_dir(&self.dir)?;
        }
        Ok(removed)
    }
}

fn list_numbers(dir: &Path) -> Result<Vec<u64>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).at(dir)? {
        let entry = entry.at(dir)?;
        let name = entry.file_name().to_string_lossy().to_string();
        if let Some(stem) = name.strip_suffix(".wal")
            && let Ok(n) = stem.parse::<u64>() {
                out.push(n);
            }
    }
    Ok(out)
}

/// fsync a directory so that renames/creates inside it are durable.
/// On Windows directories cannot be opened for sync; that is a documented
/// limitation of the platform, not of the format.
pub fn sync_dir(dir: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        let f = std::fs::File::open(dir).at(dir)?;
        f.sync_all().at(dir)?;
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
    }
    Ok(())
}
