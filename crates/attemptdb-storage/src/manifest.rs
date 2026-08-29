//! Manifest generations.
//!
//! Every generation is a complete, self-checksummed JSON document. Writers
//! never overwrite: they write `gen-NNNNNN.json` atomically for the next
//! number. Readers pick the highest generation whose checksum verifies and
//! whose segment files exist, so a torn write of the newest file simply
//! falls back to the previous generation.

use crate::format::{MANIFEST_DIR, MANIFEST_FORMAT_VERSION};
use crate::{IoAt, Result, StorageError};
use attemptdb_core::{DeviceId, EventId, Hlc, ProjectId, Timestamp};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use uuid::Uuid;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SegmentMeta {
    pub segment_id: Uuid,
    /// File name relative to `segments/`.
    pub file: String,
    pub rows: u64,
    pub bytes: u64,
    pub min_observed_at: Timestamp,
    pub max_observed_at: Timestamp,
    pub min_hlc: Hlc,
    pub max_hlc: Hlc,
    pub min_source_seq: u64,
    pub max_source_seq: u64,
    pub min_event_id: EventId,
    pub max_event_id: EventId,
    #[serde(default)]
    pub providers: Vec<String>,
    #[serde(default)]
    pub project_ids: Vec<ProjectId>,
    #[serde(default)]
    pub session_count: u64,
    pub sha256: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Tombstone {
    pub file: String,
    pub since_generation: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct WalState {
    /// Number of the WAL file that was active when this generation was
    /// written. Every event in lower-numbered files is contained in segments.
    pub active_file: u64,
    /// Byte offset in `active_file` up to which events are contained in
    /// segments (0 = none).
    pub checkpoint_offset: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Manifest {
    pub format_version: u16,
    pub generation: u64,
    pub db_id: Uuid,
    pub device_id: DeviceId,
    pub created_at: Timestamp,
    pub last_hlc: Hlc,
    pub last_source_seq: u64,
    pub wal: WalState,
    #[serde(default)]
    pub segments: Vec<SegmentMeta>,
    #[serde(default)]
    pub tombstones: Vec<Tombstone>,
    /// CRC32C of the canonical JSON of this document with `checksum` absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checksum: Option<u32>,
    #[serde(flatten, default)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl Manifest {
    pub fn initial(db_id: Uuid, device_id: DeviceId) -> Self {
        Self {
            format_version: MANIFEST_FORMAT_VERSION,
            generation: 0,
            db_id,
            device_id,
            created_at: Timestamp::now(),
            last_hlc: Hlc::default(),
            last_source_seq: 0,
            wal: WalState::default(),
            segments: Vec::new(),
            tombstones: Vec::new(),
            checksum: None,
            extra: Default::default(),
        }
    }

    pub fn dir(root: &Path) -> PathBuf {
        root.join(MANIFEST_DIR)
    }

    fn file_name(generation: u64) -> String {
        format!("gen-{generation:06}.json")
    }

    /// Canonical bytes used for the checksum: compact JSON with `checksum`
    /// removed. Key order is the struct order (serde_json preserves it).
    fn canonical_bytes(&self) -> Result<Vec<u8>> {
        let mut clone = self.clone();
        clone.checksum = None;
        Ok(serde_json::to_vec(&clone)?)
    }

    fn compute_checksum(&self) -> Result<u32> {
        Ok(crc32c::crc32c(&self.canonical_bytes()?))
    }

    /// Write this manifest as the next generation (`self.generation` must
    /// already be set by the caller). Returns the path written.
    pub fn write(&mut self, root: &Path) -> Result<PathBuf> {
        let dir = Self::dir(root);
        std::fs::create_dir_all(&dir).at(&dir)?;
        self.checksum = Some(self.compute_checksum()?);
        let path = dir.join(Self::file_name(self.generation));
        let tmp = dir.join(format!("{}.tmp", Self::file_name(self.generation)));
        let bytes = serde_json::to_vec_pretty(self)?;
        write_atomically(&tmp, &path, &bytes)?;
        Ok(path)
    }

    /// Load the newest valid generation. Returns `None` when no manifest
    /// exists yet (fresh database).
    pub fn load_latest(root: &Path) -> Result<Option<(Manifest, Vec<String>)>> {
        let dir = Self::dir(root);
        if !dir.exists() {
            return Ok(None);
        }
        let mut gens: Vec<(u64, PathBuf)> = Vec::new();
        for entry in std::fs::read_dir(&dir).at(&dir)? {
            let entry = entry.at(&dir)?;
            let name = entry.file_name().to_string_lossy().to_string();
            if let Some(num) = name.strip_prefix("gen-").and_then(|s| s.strip_suffix(".json"))
                && let Ok(n) = num.parse::<u64>() {
                    gens.push((n, entry.path()));
                }
        }
        gens.sort_by(|a, b| b.0.cmp(&a.0));
        let mut warnings = Vec::new();
        for (n, path) in gens {
            match Self::load_file(&path, root) {
                Ok(m) => return Ok(Some((m, warnings))),
                Err(e) => warnings.push(format!("manifest generation {n} rejected: {e}")),
            }
        }
        if warnings.is_empty() {
            Ok(None)
        } else {
            Err(StorageError::Corrupt {
                what: "manifest",
                path: dir,
                detail: warnings.join("; "),
            })
        }
    }

    fn load_file(path: &Path, root: &Path) -> Result<Manifest> {
        let bytes = std::fs::read(path).at(path)?;
        let m: Manifest = serde_json::from_slice(&bytes).map_err(|e| StorageError::Corrupt {
            what: "manifest",
            path: path.to_path_buf(),
            detail: e.to_string(),
        })?;
        if m.format_version != MANIFEST_FORMAT_VERSION {
            return Err(StorageError::UnsupportedFormat {
                what: "manifest",
                found: m.format_version,
                supported: MANIFEST_FORMAT_VERSION,
            });
        }
        let expected = m.compute_checksum()?;
        if m.checksum != Some(expected) {
            return Err(StorageError::Corrupt {
                what: "manifest",
                path: path.to_path_buf(),
                detail: format!("checksum mismatch (stored {:?}, computed {expected})", m.checksum),
            });
        }
        for seg in &m.segments {
            let p = root.join(crate::format::SEGMENTS_DIR).join(&seg.file);
            if !p.exists() {
                return Err(StorageError::Corrupt {
                    what: "manifest",
                    path: path.to_path_buf(),
                    detail: format!("references missing segment {}", seg.file),
                });
            }
        }
        Ok(m)
    }
}

/// Write `bytes` to `tmp`, fsync, rename over `target`, fsync the directory.
pub fn write_atomically(tmp: &Path, target: &Path, bytes: &[u8]) -> Result<()> {
    {
        let mut f = std::fs::File::create(tmp).at(tmp)?;
        std::io::Write::write_all(&mut f, bytes).at(tmp)?;
        f.sync_all().at(tmp)?;
    }
    #[cfg(windows)]
    {
        // Windows cannot rename over an existing file atomically with
        // std::fs::rename on all filesystems; remove first (documented
        // non-atomic window, mitigated by generation-based recovery).
        if target.exists() {
            std::fs::remove_file(target).at(target)?;
        }
    }
    std::fs::rename(tmp, target).at(target)?;
    if let Some(dir) = target.parent() {
        crate::wal::sync_dir(dir)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newest_valid_generation_wins_and_corrupt_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let mut m = Manifest::initial(Uuid::now_v7(), DeviceId::new());
        m.generation = 1;
        m.write(root).unwrap();
        m.generation = 2;
        m.last_source_seq = 42;
        m.write(root).unwrap();
        // Corrupt generation 3: truncated JSON.
        std::fs::write(Manifest::dir(root).join("gen-000003.json"), b"{\"format_version\":1,").unwrap();
        let (loaded, warnings) = Manifest::load_latest(root).unwrap().unwrap();
        assert_eq!(loaded.generation, 2);
        assert_eq!(loaded.last_source_seq, 42);
        assert_eq!(warnings.len(), 1);
    }

    #[test]
    fn checksum_detects_tampering() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let mut m = Manifest::initial(Uuid::now_v7(), DeviceId::new());
        m.generation = 1;
        let path = m.write(root).unwrap();
        let s = std::fs::read_to_string(&path).unwrap().replace("\"last_source_seq\": 0", "\"last_source_seq\": 7");
        std::fs::write(&path, s).unwrap();
        assert!(Manifest::load_latest(root).is_err());
    }
}
