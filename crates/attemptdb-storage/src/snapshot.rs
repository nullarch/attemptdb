//! `.atdb` portable snapshot container.
//!
//! ```text
//! header (32 bytes)
//!   0..4   magic "ATDB"
//!   4..6   format_version u16 LE
//!   6..8   schema_version u16 LE
//!   8..24  snapshot id (UUID bytes)
//!   24..32 created_at i64 LE micros
//! entries (repeated)
//!   u16 LE name_len, name (UTF-8), u64 LE len, u32 LE crc32c(bytes), bytes
//! footer
//!   u32 LE entry_count, u32 LE crc32c(entry table), magic "ATDB"
//! ```
//!
//! The entry table used by the footer checksum is the concatenation of every
//! entry's header fields (`name_len ‖ name ‖ len ‖ crc32c`) in file order.
//! Entries are `manifest.json` (WAL state zeroed) and `segments/<file>`.

use crate::db::{Database, OpenOptions};
use crate::format::{MAGIC_SNAPSHOT, SNAPSHOT_FORMAT_VERSION, u16_le, u32_le, u64_le};
use crate::identity::Identity;
use crate::manifest::{Manifest, WalState};
use crate::{IoAt, Result, StorageError};
use attemptdb_core::schema::CANONICAL_SCHEMA_VERSION;
use attemptdb_core::{DeviceId, Timestamp};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use uuid::Uuid;

const HEADER_LEN: usize = 32;

#[derive(Clone, Debug)]
pub struct SnapshotEntry {
    pub name: String,
    pub len: u64,
    pub crc32c: u32,
    /// Offset of the entry's bytes in the container.
    pub data_offset: u64,
}

#[derive(Clone, Debug)]
pub struct SnapshotInfo {
    pub snapshot_id: Uuid,
    pub schema_version: u16,
    pub created_at: Timestamp,
    pub entries: Vec<SnapshotEntry>,
}

/// Export a flushed database as a single `.atdb` file.
///
/// The caller must have flushed (or opened read-only after a flush): only
/// segments referenced by the manifest are exported. Events still in the WAL
/// are reported back so the caller can warn.
pub fn export(db: &Database, out: &Path) -> Result<(SnapshotInfo, usize)> {
    let unflushed = db.stats().memtable_rows;
    let mut manifest = db.manifest().clone();
    manifest.wal = WalState::default();
    manifest.tombstones.clear();
    manifest.checksum = None;
    let manifest_bytes = serde_json::to_vec_pretty(&manifest)?;

    let tmp = out.with_extension("atdb.tmp");
    let mut f = std::fs::File::create(&tmp).at(&tmp)?;
    let snapshot_id = Uuid::now_v7();
    let created_at = Timestamp::now();
    let mut header = [0u8; HEADER_LEN];
    header[0..4].copy_from_slice(&MAGIC_SNAPSHOT);
    header[4..6].copy_from_slice(&SNAPSHOT_FORMAT_VERSION.to_le_bytes());
    header[6..8].copy_from_slice(&CANONICAL_SCHEMA_VERSION.to_le_bytes());
    header[8..24].copy_from_slice(snapshot_id.as_bytes());
    header[24..32].copy_from_slice(&created_at.as_micros().to_le_bytes());
    f.write_all(&header).at(&tmp)?;

    let mut table = Vec::new();
    let mut entries = Vec::new();
    let mut offset = HEADER_LEN as u64;
    let mut write_entry = |f: &mut std::fs::File, name: &str, bytes: &[u8]| -> Result<()> {
        let crc = crc32c::crc32c(bytes);
        let mut head = Vec::new();
        head.extend_from_slice(&(name.len() as u16).to_le_bytes());
        head.extend_from_slice(name.as_bytes());
        head.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
        head.extend_from_slice(&crc.to_le_bytes());
        f.write_all(&head).at(&tmp)?;
        f.write_all(bytes).at(&tmp)?;
        table.extend_from_slice(&head);
        offset += head.len() as u64;
        entries.push(SnapshotEntry { name: name.to_string(), len: bytes.len() as u64, crc32c: crc, data_offset: offset });
        offset += bytes.len() as u64;
        Ok(())
    };
    write_entry(&mut f, "manifest.json", &manifest_bytes)?;
    for seg in &manifest.segments {
        let p = crate::segment::segments_dir(db.root()).join(&seg.file);
        let bytes = std::fs::read(&p).at(&p)?;
        write_entry(&mut f, &format!("segments/{}", seg.file), &bytes)?;
    }
    let mut footer = Vec::new();
    footer.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    footer.extend_from_slice(&crc32c::crc32c(&table).to_le_bytes());
    footer.extend_from_slice(&MAGIC_SNAPSHOT);
    f.write_all(&footer).at(&tmp)?;
    f.sync_all().at(&tmp)?;
    drop(f);
    #[cfg(windows)]
    if out.exists() {
        std::fs::remove_file(out).at(out)?;
    }
    std::fs::rename(&tmp, out).at(out)?;
    Ok((SnapshotInfo { snapshot_id, schema_version: CANONICAL_SCHEMA_VERSION, created_at, entries }, unflushed))
}

/// Read and verify the container structure (headers, CRCs, footer).
pub fn inspect(path: &Path) -> Result<SnapshotInfo> {
    let mut f = std::fs::File::open(path).at(path)?;
    let total = f.metadata().at(path)?.len();
    let mut header = [0u8; HEADER_LEN];
    f.read_exact(&mut header).at(path)?;
    if header[0..4] != MAGIC_SNAPSHOT {
        return Err(StorageError::Corrupt { what: "snapshot", path: path.to_path_buf(), detail: "bad magic".into() });
    }
    let version = u16_le(&header[4..6]);
    if version != SNAPSHOT_FORMAT_VERSION {
        return Err(StorageError::UnsupportedFormat { what: "snapshot", found: version, supported: SNAPSHOT_FORMAT_VERSION });
    }
    let schema_version = u16_le(&header[6..8]);
    let mut id = [0u8; 16];
    id.copy_from_slice(&header[8..24]);
    let created_at = Timestamp::from_micros(crate::format::i64_le(&header[24..32]));

    if total < (HEADER_LEN + 12) as u64 {
        return Err(StorageError::Corrupt { what: "snapshot", path: path.to_path_buf(), detail: "truncated".into() });
    }
    f.seek(SeekFrom::Start(total - 12)).at(path)?;
    let mut footer = [0u8; 12];
    f.read_exact(&mut footer).at(path)?;
    if footer[8..12] != MAGIC_SNAPSHOT {
        return Err(StorageError::Corrupt { what: "snapshot", path: path.to_path_buf(), detail: "bad footer".into() });
    }
    let entry_count = u32_le(&footer[0..4]) as usize;
    let table_crc = u32_le(&footer[4..8]);

    f.seek(SeekFrom::Start(HEADER_LEN as u64)).at(path)?;
    let mut entries = Vec::with_capacity(entry_count);
    let mut table = Vec::new();
    let mut pos = HEADER_LEN as u64;
    for _ in 0..entry_count {
        let mut nl = [0u8; 2];
        f.read_exact(&mut nl).at(path)?;
        let name_len = u16_le(&nl) as usize;
        let mut name = vec![0u8; name_len];
        f.read_exact(&mut name).at(path)?;
        let mut rest = [0u8; 12];
        f.read_exact(&mut rest).at(path)?;
        let len = u64_le(&rest[0..8]);
        let crc = u32_le(&rest[8..12]);
        table.extend_from_slice(&nl);
        table.extend_from_slice(&name);
        table.extend_from_slice(&rest);
        pos += 2 + name_len as u64 + 12;
        let name = String::from_utf8(name).map_err(|_| StorageError::Corrupt {
            what: "snapshot",
            path: path.to_path_buf(),
            detail: "entry name is not UTF-8".into(),
        })?;
        // Verify the entry's CRC while streaming.
        let mut remaining = len;
        let mut hasher_crc = 0u32;
        let mut buf = vec![0u8; 1 << 16];
        while remaining > 0 {
            let n = buf.len().min(remaining as usize);
            f.read_exact(&mut buf[..n]).at(path)?;
            hasher_crc = crc32c::crc32c_append(hasher_crc, &buf[..n]);
            remaining -= n as u64;
        }
        if hasher_crc != crc {
            return Err(StorageError::Corrupt { what: "snapshot", path: path.to_path_buf(), detail: format!("entry {name} crc mismatch") });
        }
        entries.push(SnapshotEntry { name, len, crc32c: crc, data_offset: pos });
        pos += len;
    }
    if crc32c::crc32c(&table) != table_crc {
        return Err(StorageError::Corrupt { what: "snapshot", path: path.to_path_buf(), detail: "entry table crc mismatch".into() });
    }
    Ok(SnapshotInfo { snapshot_id: Uuid::from_bytes(id), schema_version, created_at, entries })
}

/// Extract a snapshot into a fresh database directory that can be opened
/// read-only (or read-write: it becomes an ordinary database).
pub fn extract(path: &Path, dest: &Path) -> Result<SnapshotInfo> {
    let info = inspect(path)?;
    if Database::exists(dest) {
        return Err(StorageError::Other(format!("destination {} already holds a database", dest.display())));
    }
    std::fs::create_dir_all(dest).at(dest)?;
    let mut f = std::fs::File::open(path).at(path)?;
    let mut manifest: Option<Manifest> = None;
    for e in &info.entries {
        f.seek(SeekFrom::Start(e.data_offset)).at(path)?;
        let mut bytes = vec![0u8; e.len as usize];
        f.read_exact(&mut bytes).at(path)?;
        if e.name == "manifest.json" {
            manifest = Some(serde_json::from_slice(&bytes)?);
        } else if let Some(seg) = e.name.strip_prefix("segments/") {
            if seg.contains('/') || seg.contains('\\') || seg.contains("..") {
                return Err(StorageError::Corrupt { what: "snapshot", path: path.to_path_buf(), detail: format!("unsafe entry name {}", e.name) });
            }
            let dir = crate::segment::segments_dir(dest);
            std::fs::create_dir_all(&dir).at(&dir)?;
            let target = dir.join(seg);
            std::fs::write(&target, &bytes).at(&target)?;
        }
    }
    let mut manifest = manifest.ok_or_else(|| StorageError::Corrupt {
        what: "snapshot",
        path: path.to_path_buf(),
        detail: "missing manifest.json".into(),
    })?;
    manifest.generation = 1;
    manifest.wal = WalState::default();
    manifest.write(dest)?;
    let mut identity = Identity::new(manifest.device_id);
    identity.db_id = manifest.db_id;
    identity.extra.insert("imported_from_snapshot".into(), serde_json::json!(info.snapshot_id.to_string()));
    identity.write(dest)?;
    Ok(info)
}

/// Convenience: extract into `cache_dir/<snapshot id>` (if not already
/// present) and open read-only.
pub fn open_read_only(path: &Path, cache_dir: &Path) -> Result<(Database, PathBuf)> {
    let info = inspect(path)?;
    let dest = cache_dir.join(format!("snapshot-{}", info.snapshot_id.simple()));
    if !Database::exists(&dest) {
        extract(path, &dest)?;
    }
    let db = Database::open(&dest, OpenOptions { read_only: true, ..Default::default() })?;
    Ok((db, dest))
}

#[allow(dead_code)]
fn _device(_: DeviceId) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::ScanFilter;
    use attemptdb_core::event::Provider;
    use attemptdb_core::{CaptureMode, Event, EventKind, ProjectRef};

    #[test]
    fn export_inspect_extract_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("db.attemptdb");
        let dev = DeviceId::new();
        let mut db = Database::open(&root, OpenOptions { create: true, device_id: Some(dev), ..Default::default() }).unwrap();
        let events: Vec<Event> = (0..7)
            .map(|_| Event::new(dev, Provider::Cursor, "stop", EventKind::TurnStopped, ProjectRef::derive("/p", None, &dev), "c1", CaptureMode::MetadataOnly, "t"))
            .collect();
        db.ingest(events).unwrap();
        db.flush().unwrap();
        let out = dir.path().join("x.atdb");
        let (info, unflushed) = export(&db, &out).unwrap();
        assert_eq!(unflushed, 0);
        assert_eq!(info.entries.len(), 2);
        let inspected = inspect(&out).unwrap();
        assert_eq!(inspected.snapshot_id, info.snapshot_id);
        let (ro, _) = open_read_only(&out, &dir.path().join("cache")).unwrap();
        assert_eq!(ro.scan(&ScanFilter::default()).unwrap().len(), 7);
        // Corrupt one byte inside a segment → inspect fails.
        let mut bytes = std::fs::read(&out).unwrap();
        let mid = bytes.len() / 2;
        bytes[mid] ^= 0xff;
        std::fs::write(&out, bytes).unwrap();
        assert!(inspect(&out).is_err());
    }
}
