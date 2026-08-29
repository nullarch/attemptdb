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
use attemptdb_core::Timestamp;
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

/// How [`restore`] treats the destination directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RestoreMode {
    /// The destination must not exist or must be an empty directory.
    IntoEmptyDir,
    /// Move the current database directory to `backup_to` (same filesystem,
    /// a rename) before putting the restored copy in its place.
    ReplaceExisting { backup_to: PathBuf },
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct RestoreReport {
    /// Rows across every restored segment.
    pub events: u64,
    pub segments: usize,
    /// Where the previous database went (`ReplaceExisting` only).
    pub backup: Option<PathBuf>,
}

/// Restore a snapshot into `dest` as a live, writable database.
///
/// Every entry's CRC is verified before anything on disk is touched. The
/// snapshot is extracted into a staging directory next to `dest` and then
/// swapped in with renames, so a failure half-way leaves both the existing
/// database and the backup intact. Refuses when a writer holds the lock of
/// the database being replaced.
pub fn restore(snapshot: &Path, dest: &Path, mode: RestoreMode) -> Result<RestoreReport> {
    // Verifies every entry checksum; nothing below runs on a bad container.
    inspect(snapshot)?;
    let dest_exists = dest.exists();
    let dest_is_empty_dir = dest.is_dir()
        && std::fs::read_dir(dest)
            .map(|mut d| d.next().is_none())
            .unwrap_or(false);
    let replacing = dest_exists && !dest_is_empty_dir;
    let mut backup_to: Option<PathBuf> = None;
    match &mode {
        RestoreMode::IntoEmptyDir => {
            if replacing {
                return Err(StorageError::Other(format!(
                    "destination {} is not empty; use ReplaceExisting to back it up and replace it",
                    dest.display()
                )));
            }
        }
        RestoreMode::ReplaceExisting { backup_to: b } => {
            if b.exists() {
                return Err(StorageError::Other(format!(
                    "backup path {} already exists",
                    b.display()
                )));
            }
            if replacing {
                if !dest.is_dir() {
                    return Err(StorageError::Other(format!(
                        "destination {} is not a directory",
                        dest.display()
                    )));
                }
                backup_to = Some(b.clone());
            }
        }
    }
    // A writer must not be replaced underneath itself.
    let lock = if replacing {
        Some(crate::repair::try_writer_lock(dest)?)
    } else {
        None
    };

    let parent = dest
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent).at(parent)?;
    let name = dest
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "db".into());
    let staging = parent.join(format!(".{name}.restore-{}", Uuid::now_v7().simple()));
    let staged = (|| -> Result<RestoreReport> {
        extract(snapshot, &staging)?;
        for d in [
            crate::format::WAL_DIR,
            crate::format::SPOOL_DIR,
            crate::format::BLOBS_DIR,
        ] {
            let p = staging.join(d);
            std::fs::create_dir_all(&p).at(&p)?;
        }
        let (manifest, _) =
            Manifest::load_latest(&staging)?.ok_or_else(|| StorageError::Corrupt {
                what: "snapshot",
                path: snapshot.to_path_buf(),
                detail: "extracted database has no manifest".into(),
            })?;
        Ok(RestoreReport {
            events: manifest.segments.iter().map(|s| s.rows).sum(),
            segments: manifest.segments.len(),
            backup: None,
        })
    })();
    let mut report = match staged {
        Ok(r) => r,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(e);
        }
    };

    // Swap. The lock file moves with the directory; release our handle
    // first so the rename cannot fail on platforms that refuse to move a
    // directory with open files inside.
    drop(lock);
    if dest_exists {
        match &backup_to {
            Some(b) => {
                if let Err(e) = std::fs::rename(dest, b) {
                    let _ = std::fs::remove_dir_all(&staging);
                    return Err(if e.kind() == std::io::ErrorKind::CrossesDevices {
                        StorageError::Other(format!(
                            "backup path {} must be on the same filesystem as {}",
                            b.display(),
                            dest.display()
                        ))
                    } else {
                        StorageError::io(dest, e)
                    });
                }
                report.backup = Some(b.clone());
            }
            None => {
                if let Err(e) = std::fs::remove_dir(dest) {
                    let _ = std::fs::remove_dir_all(&staging);
                    return Err(StorageError::io(dest, e));
                }
            }
        }
    }
    if let Err(e) = std::fs::rename(&staging, dest) {
        // Put the previous database back where it was before giving up.
        if let Some(b) = &backup_to {
            let _ = std::fs::rename(b, dest);
        }
        let _ = std::fs::remove_dir_all(&staging);
        return Err(StorageError::io(dest, e));
    }
    crate::wal::sync_dir(parent)?;
    Ok(report)
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

/// What a sanitised export removes or rewrites. Every option is on by
/// default; the result is meant to be published.
#[derive(Clone, Debug)]
pub struct SanitizePolicy {
    /// Drop `content` and `raw` (prompts, commands, tool output, payloads).
    pub drop_content: bool,
    /// Drop `unknown` (fields from newer schemas that we cannot vet).
    pub drop_unknown: bool,
    /// Rewrite absolute paths so the project root becomes `/<project name>`.
    pub relativize_paths: bool,
    /// Remove `attrs.cwd`, `attrs.previous_cwd`, `attrs.worktree_path`
    /// instead of rewriting them.
    pub drop_cwd_attrs: bool,
    /// Drop the git remote URL.
    pub drop_remote: bool,
    /// Replace provider session ids with a stable hash (keeps sessions
    /// distinct without exposing the original id).
    pub hash_session_ids: bool,
}

impl Default for SanitizePolicy {
    fn default() -> Self {
        Self {
            drop_content: true,
            drop_unknown: true,
            relativize_paths: true,
            drop_cwd_attrs: true,
            drop_remote: false,
            hash_session_ids: false,
        }
    }
}

/// Apply the policy to one event (in place).
pub fn sanitize_event(ev: &mut attemptdb_core::Event, policy: &SanitizePolicy) {
    use attemptdb_core::PortablePath;
    let root = ev.project.root.clone();
    let alias = format!("/{}", ev.project.name.rsplit('/').next().unwrap_or("project"));
    let rewrite = |s: &str| -> String {
        if !root.is_empty() && s.starts_with(&root) {
            format!("{alias}{}", &s[root.len()..])
        } else if s.starts_with('/') || s.get(1..3) == Some(":/") {
            // Absolute path outside the project: keep only the file name.
            format!("<outside>/{}", s.rsplit('/').next().unwrap_or(""))
        } else {
            s.to_string()
        }
    };
    if policy.drop_content {
        ev.content = None;
        ev.raw = None;
    }
    if policy.drop_unknown {
        ev.unknown.clear();
    }
    if policy.relativize_paths {
        for p in &mut ev.paths {
            let logical = rewrite(&p.logical);
            let mut np = PortablePath::from_raw(&logical, Some(&alias));
            np.repo_relative = p.repo_relative.clone().or(np.repo_relative);
            *p = np;
        }
        for key in ["cwd", "previous_cwd", "worktree_path"] {
            if let Some(v) = ev.attrs.get(key).and_then(|v| v.as_str()).map(str::to_string) {
                if policy.drop_cwd_attrs {
                    ev.attrs.remove(key);
                } else {
                    ev.attrs.insert(key.into(), serde_json::Value::String(rewrite(&v)));
                }
            }
        }
        ev.project.root = alias.clone();
    } else if policy.drop_cwd_attrs {
        for key in ["cwd", "previous_cwd", "worktree_path"] {
            ev.attrs.remove(key);
        }
    }
    if policy.drop_remote {
        ev.project.repo_remote = None;
    }
    if policy.hash_session_ids {
        let h = attemptdb_core::codec::content_hash(ev.provider_session_id.as_bytes());
        ev.provider_session_id = format!("anon-{}", &h[..16]);
    }
}

/// Export a filtered (and optionally sanitised) copy of the database as a
/// snapshot. Events keep their original ordering fields. Returns the
/// snapshot info and the number of events exported.
pub fn export_filtered(
    db: &Database,
    out: &Path,
    filter: &crate::db::ScanFilter,
    policy: Option<&SanitizePolicy>,
) -> Result<(SnapshotInfo, usize)> {
    let mut events = db.scan(filter)?;
    if let Some(p) = policy {
        for ev in &mut events {
            sanitize_event(ev, p);
        }
    }
    let count = events.len();
    let staging = tempdir_for(out)?;
    let root = staging.join("staging.attemptdb");
    Database::create(&root, db.device_id())?;
    let mut manifest = Manifest::initial(db.identity().db_id, db.device_id());
    manifest.generation = 2;
    for chunk in events.chunks(50_000) {
        if chunk.is_empty() {
            continue;
        }
        manifest.segments.push(crate::segment::write_segment(&root, chunk)?);
    }
    manifest.last_source_seq = events.iter().map(|e| e.source_seq).max().unwrap_or(0);
    manifest.last_hlc = events.iter().map(|e| e.hlc).max().unwrap_or_default();
    manifest.write(&root)?;
    let staged = Database::open(&root, OpenOptions { read_only: true, ..Default::default() })?;
    let result = export(&staged, out);
    drop(staged);
    let _ = std::fs::remove_dir_all(&staging);
    result.map(|(info, _)| (info, count))
}

fn tempdir_for(out: &Path) -> Result<PathBuf> {
    let parent = out.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let dir = parent.join(format!(".attemptdb-export-{}", Uuid::now_v7().simple()));
    std::fs::create_dir_all(&dir).at(&dir)?;
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::ScanFilter;
    use attemptdb_core::event::Provider;
    use attemptdb_core::{CaptureMode, DeviceId, Event, EventKind, ProjectRef};

    #[test]
    fn sanitized_filtered_export_strips_content_and_paths() {
        use attemptdb_core::event::{EventContent, ToolCategory, ToolRef};
        use attemptdb_core::PortablePath;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("db.attemptdb");
        let dev = DeviceId::new();
        let mut db = Database::open(&root, OpenOptions { create: true, device_id: Some(dev), ..Default::default() }).unwrap();
        let proj = ProjectRef::derive("/Users/someone/code/attemptdb", Some("git@github.com:o/attemptdb.git"), &dev);
        let other = ProjectRef::derive("/Users/someone/code/other", None, &dev);
        let mut ev = Event::new(dev, Provider::ClaudeCode, "PostToolUse", EventKind::ToolCallFinished, proj.clone(), "s1", CaptureMode::LocalSemantic, "t");
        ev.tool = Some(ToolRef { name: "Edit".into(), category: ToolCategory::FileEdit, call_id: None });
        ev.paths.push(PortablePath::from_raw("/Users/someone/code/attemptdb/src/lib.rs", Some(&proj.root)));
        ev.paths.push(PortablePath::from_raw("/Users/someone/.secret/keys", Some(&proj.root)));
        ev.attrs.insert("cwd".into(), serde_json::json!("/Users/someone/code/attemptdb"));
        ev.content = Some(EventContent { command: Some("echo SECRET".into()), ..Default::default() });
        ev.raw = Some(serde_json::json!({"prompt": "SECRET"}));
        ev.unknown.insert("future".into(), serde_json::json!("SECRET"));
        let mut other_ev = Event::new(dev, Provider::Codex, "Stop", EventKind::TurnStopped, other, "s2", CaptureMode::LocalSemantic, "t");
        other_ev.content = Some(EventContent { message: Some("SECRET".into()), ..Default::default() });
        db.ingest(vec![ev, other_ev]).unwrap();
        db.flush().unwrap();
        let out = dir.path().join("public.atdb");
        let filter = ScanFilter { project_id: Some(proj.project_id), ..Default::default() };
        let (_, n) = export_filtered(&db, &out, &filter, Some(&SanitizePolicy::default())).unwrap();
        assert_eq!(n, 1);
        let (ro, _) = open_read_only(&out, &dir.path().join("cache")).unwrap();
        let events = ro.scan(&ScanFilter::default()).unwrap();
        assert_eq!(events.len(), 1);
        let e = &events[0];
        assert!(e.content.is_none() && e.raw.is_none() && e.unknown.is_empty());
        assert_eq!(e.project.root, "/attemptdb");
        assert_eq!(e.paths[0].logical, "/attemptdb/src/lib.rs");
        assert_eq!(e.paths[0].repo_relative.as_deref(), Some("src/lib.rs"));
        assert_eq!(e.paths[1].logical, "<outside>/keys");
        assert!(e.attrs.get("cwd").is_none());
        assert!(e.source_seq > 0, "ordering fields preserved");
        // Segments are compressed, so scan the decoded events rather than
        // the raw bytes when looking for leaks.
        let dump = serde_json::to_string(&events).unwrap();
        assert!(!dump.contains("SECRET"), "{dump}");
        assert!(!dump.contains("someone"), "{dump}");
        assert!(!dir.path().read_dir().unwrap().any(|e| e.unwrap().file_name().to_string_lossy().starts_with(".attemptdb-export")));
    }

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
