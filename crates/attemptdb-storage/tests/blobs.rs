//! Encrypted content blobs, end to end: blob files, format 2 segments,
//! reads with and without keys, snapshots in every export mode, verify,
//! and key rotation.

use attemptdb_core::event::{EventContent, Provider};
use attemptdb_core::{CaptureMode, DeviceId, Event, EventKind, ProjectRef};
use attemptdb_storage::blobs::{self, BlobStore, DerivedKeys, KeyProvider, StaticKeyProvider};
use attemptdb_storage::format::{BLOB_HEADER_LEN, BLOB_TRAILER_LEN};
use attemptdb_storage::snapshot::{self, ExportKey, RestoreMode, SanitizePolicy};
use attemptdb_storage::{Database, OpenOptions, ScanFilter, StorageError, segment};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn temp_root() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("db.attemptdb");
    (dir, root)
}

fn master(seed: u8) -> [u8; 32] {
    let mut m = [seed; 32];
    m[0] ^= 0x5a;
    m
}

fn provider(masters: &[[u8; 32]]) -> Arc<dyn KeyProvider> {
    let mut p = StaticKeyProvider::new();
    for (i, m) in masters.iter().enumerate() {
        if i == 0 {
            p.set_current(*m);
        } else {
            p.add(*m);
        }
    }
    Arc::new(p)
}

fn open(root: &Path, keys: Option<Arc<dyn KeyProvider>>, read_only: bool) -> Database {
    Database::open(
        root,
        OpenOptions {
            create: !read_only,
            read_only,
            flush_events: usize::MAX,
            flush_bytes: usize::MAX,
            keys,
            ..Default::default()
        },
    )
    .unwrap()
}

fn events(dev: DeviceId, n: usize, tag: &str) -> Vec<Event> {
    (0..n)
        .map(|i| {
            let mut ev = Event::new(
                dev,
                Provider::ClaudeCode,
                "PostToolUse",
                EventKind::ToolCallFinished,
                ProjectRef::derive("/home/dev/proj", None, &dev),
                format!("session-{tag}"),
                CaptureMode::LocalSemantic,
                "blobs-test/0.1",
            );
            ev.attrs
                .insert("turn_index_hint".into(), serde_json::json!(i));
            ev.content = Some(EventContent {
                command: Some(format!("echo secret-{tag}-{i}")),
                ..Default::default()
            });
            ev.raw = Some(serde_json::json!({"tag": tag, "i": i}));
            ev
        })
        .collect()
}

fn command_of(ev: &Event) -> Option<&str> {
    ev.content.as_ref().and_then(|c| c.command.as_deref())
}

fn segment_versions(db: &Database) -> Vec<u16> {
    db.manifest()
        .segments
        .iter()
        .map(|s| {
            segment::segment_format_version(&segment::segments_dir(db.root()).join(&s.file))
                .unwrap()
        })
        .collect()
}

fn assert_no_plaintext_in_segments(root: &Path, needle: &str) {
    for seg in std::fs::read_dir(segment::segments_dir(root)).unwrap() {
        let p = seg.unwrap().path();
        if p.extension().and_then(|e| e.to_str()) != Some("arrow") {
            continue;
        }
        // Segments are zstd-compressed; decode the rows instead of grepping.
        for b in segment::read_segment_batches(&p).unwrap() {
            for ev in segment::batch_to_events(&b).unwrap() {
                let dump = serde_json::to_string(&ev).unwrap();
                assert!(
                    !dump.contains(needle),
                    "plaintext {needle} in {}",
                    p.display()
                );
            }
        }
    }
    for entry in walk(&root.join("blobs")) {
        let bytes = std::fs::read(&entry).unwrap();
        assert!(
            !bytes.windows(needle.len()).any(|w| w == needle.as_bytes()),
            "plaintext {needle} in blob {}",
            entry.display()
        );
    }
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if !dir.is_dir() {
        return out;
    }
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            out.extend(walk(&p));
        } else {
            out.push(p);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Blob files
// ---------------------------------------------------------------------------

#[test]
fn blob_roundtrip_and_tamper_detection() {
    let (_dir, root) = temp_root();
    let db_id = Uuid::now_v7();
    let device = DeviceId::new();
    let store = BlobStore::new(&root, db_id, device);
    let m = master(1);
    let keys = DerivedKeys::from_master(&m);
    let key_id = blobs::key_id_for(&m);
    let plaintext = b"{\"command\":\"echo secret\"}";

    let id = store.write(key_id, &keys, plaintext).unwrap();
    let path = store.path(&id);
    assert!(path.starts_with(root.join("blobs").join(id.shard())));
    assert!(path.ends_with(id.file_name()));
    let bytes = std::fs::read(&path).unwrap();
    assert_eq!(&bytes[..4], b"ATBL");
    assert_eq!(
        bytes.len(),
        BLOB_HEADER_LEN + plaintext.len() + blobs::TAG_LEN + BLOB_TRAILER_LEN
    );
    assert!(!bytes.windows(6).any(|w| w == b"secret"));
    let header = store.verify(&id).unwrap();
    assert_eq!(header.key_id, key_id);
    assert_eq!(header.plaintext_len as usize, plaintext.len());

    let p = provider(&[m]);
    assert_eq!(store.read(p.as_ref(), &id).unwrap(), plaintext);

    // Wrong key → NoKey, not corruption.
    let other = provider(&[master(2)]);
    assert!(matches!(
        store.read(other.as_ref(), &id),
        Err(StorageError::NoKey { key_id: k }) if k == key_id
    ));

    // Same key, different database identity → AAD mismatch → corrupt.
    let foreign = BlobStore::new(&root, Uuid::now_v7(), device);
    let err = foreign.read(p.as_ref(), &id).unwrap_err();
    assert!(err.to_string().contains("authentication failed"), "{err}");

    // Flip a ciphertext byte: the CRC catches it without a key.
    let mut tampered = bytes.clone();
    tampered[BLOB_HEADER_LEN + 3] ^= 0x01;
    std::fs::write(&path, &tampered).unwrap();
    let err = store.verify(&id).unwrap_err();
    assert!(err.to_string().contains("crc32c"), "{err}");
    assert!(store.read(p.as_ref(), &id).is_err());

    // Repair the CRC over the tampered body: the AEAD tag still rejects it.
    let body_len = tampered.len() - BLOB_TRAILER_LEN;
    let crc = crc32c::crc32c(&tampered[..body_len]);
    tampered[body_len..].copy_from_slice(&crc.to_le_bytes());
    std::fs::write(&path, &tampered).unwrap();
    store.verify(&id).unwrap();
    let err = store.read(p.as_ref(), &id).unwrap_err();
    assert!(err.to_string().contains("authentication failed"), "{err}");

    // Truncation.
    std::fs::write(&path, &bytes[..bytes.len() - 1]).unwrap();
    assert!(store.verify(&id).is_err());
}

#[test]
fn keyed_hash_dedupes_per_key() {
    let (_dir, root) = temp_root();
    let store = BlobStore::new(&root, Uuid::now_v7(), DeviceId::new());
    let m1 = master(1);
    let m2 = master(2);
    let k1 = DerivedKeys::from_master(&m1);
    let k2 = DerivedKeys::from_master(&m2);
    let a = store.write(blobs::key_id_for(&m1), &k1, b"same").unwrap();
    let b = store.write(blobs::key_id_for(&m1), &k1, b"same").unwrap();
    assert_eq!(a, b, "same plaintext, same key → same id");
    assert_eq!(store.list().unwrap().len(), 1);
    let c = store.write(blobs::key_id_for(&m2), &k2, b"same").unwrap();
    assert_ne!(
        a, c,
        "different key → different id (no cross-key plaintext oracle)"
    );
    assert_eq!(store.list().unwrap().len(), 2);
    assert_ne!(
        store.write(blobs::key_id_for(&m1), &k1, b"other").unwrap(),
        a
    );
    assert_eq!(store.stats().unwrap().count, 3);
    assert_eq!(
        store.all_key_ids().unwrap().len(),
        2,
        "two key ids across the store"
    );
}

// ---------------------------------------------------------------------------
// Segments
// ---------------------------------------------------------------------------

#[test]
fn format2_segment_roundtrip_and_missing_key() {
    let (_dir, root) = temp_root();
    let m = master(3);
    let dev = DeviceId::new();
    let key_id = blobs::key_id_for(&m);
    {
        let mut db = Database::open(
            &root,
            OpenOptions {
                create: true,
                device_id: Some(dev),
                keys: Some(provider(&[m])),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(db.encryption_active());
        db.ingest(events(dev, 5, "a")).unwrap();
        // Before the flush the memtable serves plaintext.
        assert_eq!(
            command_of(&db.scan(&ScanFilter::default()).unwrap()[0]),
            Some("echo secret-a-0")
        );
        db.flush().unwrap();
        assert_eq!(segment_versions(&db), vec![2]);
        assert_eq!(
            db.blob_stats().unwrap().count,
            10,
            "content + raw per event"
        );
        assert_no_plaintext_in_segments(&root, "secret-a");

        // The raw batch has null content_json and non-null content_ref.
        let seg = &db.manifest().segments[0];
        let batches =
            segment::read_segment_batches(&segment::segments_dir(&root).join(&seg.file)).unwrap();
        let schema = batches[0].schema();
        let cj = batches[0].column(schema.index_of("content_json").unwrap());
        let cr = batches[0].column(schema.index_of("content_ref").unwrap());
        assert_eq!(cj.null_count(), 5);
        assert_eq!(cr.null_count(), 0);
        assert_eq!(segment::collect_blob_refs(&batches[0]).len(), 10);

        // scan resolves; batches fill content_json from the blobs.
        let all = db.scan(&ScanFilter::default()).unwrap();
        assert_eq!(all.len(), 5);
        for (i, ev) in all.iter().enumerate() {
            assert_eq!(command_of(ev), Some(format!("echo secret-a-{i}").as_str()));
            assert_eq!(ev.raw, Some(serde_json::json!({"tag": "a", "i": i})));
        }
        let resolved = db.batches(&ScanFilter::default()).unwrap();
        let s = resolved[0].schema();
        assert_eq!(
            resolved[0]
                .column(s.index_of("content_json").unwrap())
                .null_count(),
            0
        );
        assert!(db.content_warnings().is_empty());
        assert!(db.verify().unwrap().is_empty());
    }

    // No key at all: content None, one warning naming the key id, no error.
    let db = open(&root, None, true);
    assert_eq!(db.warnings.len(), 1, "{:?}", db.warnings);
    assert_eq!(
        db.warnings[0],
        format!("encrypted content unavailable (no key for key_id {key_id})")
    );
    let all = db.scan(&ScanFilter::default()).unwrap();
    assert_eq!(all.len(), 5);
    assert!(all.iter().all(|e| e.content.is_none() && e.raw.is_none()));
    assert_eq!(all[2].attrs["turn_index_hint"], 2, "metadata is intact");
    assert_eq!(db.content_warnings(), vec![db.warnings[0].clone()]);
    let unresolved = db.batches(&ScanFilter::default()).unwrap();
    let s = unresolved[0].schema();
    assert_eq!(
        unresolved[0]
            .column(s.index_of("content_json").unwrap())
            .null_count(),
        5
    );
    assert!(db.verify().unwrap().is_empty(), "verify needs no key");

    // The wrong key behaves the same.
    let db = open(&root, Some(provider(&[master(4)])), true);
    assert_eq!(db.warnings.len(), 1);
    assert!(
        db.scan(&ScanFilter::default())
            .unwrap()
            .iter()
            .all(|e| e.content.is_none())
    );
}

#[test]
fn format1_segments_stay_readable_next_to_format2() {
    let (_dir, root) = temp_root();
    let dev = DeviceId::new();
    {
        let mut db = Database::open(
            &root,
            OpenOptions {
                create: true,
                device_id: Some(dev),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(!db.encryption_active());
        db.ingest(events(dev, 3, "plain")).unwrap();
        db.flush().unwrap();
        assert_eq!(segment_versions(&db), vec![1]);
        assert_eq!(db.blob_stats().unwrap().count, 0);
    }
    let m = master(5);
    let mut db = Database::open(
        &root,
        OpenOptions {
            keys: Some(provider(&[m])),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(db.warnings.is_empty(), "{:?}", db.warnings);
    let all = db.scan(&ScanFilter::default()).unwrap();
    assert_eq!(command_of(&all[0]), Some("echo secret-plain-0"));
    db.ingest(events(dev, 2, "enc")).unwrap();
    db.flush().unwrap();
    assert_eq!(segment_versions(&db), vec![1, 2]);
    let all = db.scan(&ScanFilter::default()).unwrap();
    assert_eq!(all.len(), 5);
    assert_eq!(command_of(&all[0]), Some("echo secret-plain-0"));
    assert_eq!(command_of(&all[4]), Some("echo secret-enc-1"));
    // The query-layer batches share one schema across both versions.
    let batches = db.batches(&ScanFilter::default()).unwrap();
    assert_eq!(batches.len(), 2);
    assert_eq!(batches[0].schema(), batches[1].schema());
    assert_eq!(batches[0].schema(), segment::events_schema());
    // Without a key the inline segment still yields content, the other does not.
    drop(db);
    let db = open(&root, None, true);
    let all = db.scan(&ScanFilter::default()).unwrap();
    assert_eq!(command_of(&all[0]), Some("echo secret-plain-0"));
    assert!(all[4].content.is_none());
    assert_eq!(db.warnings.len(), 1);
}

#[test]
fn events_without_content_write_no_blobs() {
    let (_dir, root) = temp_root();
    let dev = DeviceId::new();
    let mut db = Database::open(
        &root,
        OpenOptions {
            create: true,
            device_id: Some(dev),
            keys: Some(provider(&[master(6)])),
            ..Default::default()
        },
    )
    .unwrap();
    let mut evs = events(dev, 2, "meta");
    for ev in &mut evs {
        ev.capture_mode = CaptureMode::MetadataOnly;
    }
    db.ingest(evs).unwrap();
    db.flush().unwrap();
    assert_eq!(segment_versions(&db), vec![2]);
    assert_eq!(db.blob_stats().unwrap().count, 0);
    assert!(
        db.scan(&ScanFilter::default())
            .unwrap()
            .iter()
            .all(|e| e.content.is_none())
    );
    assert!(db.verify().unwrap().is_empty());
}

// ---------------------------------------------------------------------------
// Snapshots
// ---------------------------------------------------------------------------

fn seeded_encrypted(root: &Path, m: [u8; 32]) -> (DeviceId, Database) {
    let dev = DeviceId::new();
    let mut db = Database::open(
        root,
        OpenOptions {
            create: true,
            device_id: Some(dev),
            keys: Some(provider(&[m])),
            ..Default::default()
        },
    )
    .unwrap();
    db.ingest(events(dev, 4, "snap")).unwrap();
    db.flush().unwrap();
    (dev, db)
}

fn entry_names(info: &snapshot::SnapshotInfo) -> Vec<&str> {
    info.entries.iter().map(|e| e.name.as_str()).collect()
}

#[test]
fn snapshot_export_none_same_portable() {
    let (dir, root) = temp_root();
    let m = master(7);
    let (_dev, db) = seeded_encrypted(&root, m);
    let cache = dir.path().join("cache");

    // None: metadata only, content unavailable everywhere.
    let out = dir.path().join("none.atdb");
    let (info, unflushed) = snapshot::export_with(&db, &out, &ExportKey::None).unwrap();
    assert_eq!(unflushed, 0);
    assert!(!entry_names(&info).iter().any(|n| n.starts_with("blobs/")));
    let (ro, _) = snapshot::open_read_only_with(&out, &cache, Some(provider(&[m]))).unwrap();
    let all = ro.scan(&ScanFilter::default()).unwrap();
    assert_eq!(all.len(), 4);
    assert!(all.iter().all(|e| e.content.is_none()));
    assert!(
        ro.warnings.is_empty(),
        "no blobs at all → nothing to warn about"
    );
    // The plain `export` keeps the old signature and excludes blobs.
    let out_default = dir.path().join("default.atdb");
    let (info, _) = snapshot::export(&db, &out_default).unwrap();
    assert!(!entry_names(&info).iter().any(|n| n.starts_with("blobs/")));

    // Same: raw copy, readable with the database key only.
    let out = dir.path().join("same.atdb");
    let (info, _) = snapshot::export_with(&db, &out, &ExportKey::Same).unwrap();
    let blob_entries: Vec<&str> = entry_names(&info)
        .into_iter()
        .filter(|n| n.starts_with("blobs/"))
        .collect();
    assert_eq!(blob_entries.len(), 8);
    assert!(
        blob_entries
            .iter()
            .all(|n| n.ends_with(".blob") && n.len() == "blobs/".len() + 64 + 5)
    );
    snapshot::inspect(&out).unwrap();
    let (ro, _) = snapshot::open_read_only_with(&out, &cache, Some(provider(&[m]))).unwrap();
    assert!(ro.warnings.is_empty(), "{:?}", ro.warnings);
    let all = ro.scan(&ScanFilter::default()).unwrap();
    assert_eq!(command_of(&all[3]), Some("echo secret-snap-3"));
    let (ro, _) = snapshot::open_read_only(&out, &dir.path().join("cache2")).unwrap();
    assert_eq!(ro.warnings.len(), 1);
    assert!(
        ro.scan(&ScanFilter::default())
            .unwrap()
            .iter()
            .all(|e| e.content.is_none())
    );

    // Portable: re-wrapped under a fresh key file; opens elsewhere with it.
    let out = dir.path().join("portable.atdb");
    let key_path = dir.path().join("portable.key");
    let (info, _) =
        snapshot::export_with(&db, &out, &ExportKey::Portable(key_path.clone())).unwrap();
    assert_eq!(
        entry_names(&info)
            .iter()
            .filter(|n| n.starts_with("blobs/"))
            .count(),
        8
    );
    let portable = blobs::read_key_file(&key_path).unwrap();
    assert_ne!(portable, m);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&key_path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    let elsewhere = dir.path().join("elsewhere");
    let (ro, _) =
        snapshot::open_read_only_with(&out, &elsewhere, Some(provider(&[portable]))).unwrap();
    assert!(ro.warnings.is_empty(), "{:?}", ro.warnings);
    let all = ro.scan(&ScanFilter::default()).unwrap();
    assert_eq!(all.len(), 4);
    for (i, ev) in all.iter().enumerate() {
        assert_eq!(
            command_of(ev),
            Some(format!("echo secret-snap-{i}").as_str())
        );
        assert_eq!(ev.raw, Some(serde_json::json!({"tag": "snap", "i": i})));
    }
    assert!(ro.verify().unwrap().is_empty());
    // The database's own key does not open a portable snapshot.
    let (ro, _) =
        snapshot::open_read_only_with(&out, &dir.path().join("cache3"), Some(provider(&[m])))
            .unwrap();
    assert_eq!(ro.warnings.len(), 1);
    assert!(ro.warnings[0].contains(&blobs::key_id_for(&portable).to_string()));
    assert!(
        ro.scan(&ScanFilter::default())
            .unwrap()
            .iter()
            .all(|e| e.content.is_none())
    );
    // Refuses to overwrite an existing key file.
    assert!(
        snapshot::export_with(
            &db,
            &dir.path().join("p2.atdb"),
            &ExportKey::Portable(key_path)
        )
        .is_err()
    );

    // Portable export needs the database key.
    drop(db);
    let db = open(&root, None, true);
    let err = snapshot::export_with(
        &db,
        &dir.path().join("p3.atdb"),
        &ExportKey::Portable(dir.path().join("p3.key")),
    )
    .unwrap_err();
    assert!(err.to_string().contains("needs the database key"), "{err}");
    assert!(
        !dir.path().join("p3.key").exists(),
        "no stray key file after a failed export"
    );
}

#[test]
fn filtered_exports_follow_the_same_modes() {
    let (dir, root) = temp_root();
    let m = master(8);
    let (_dev, db) = seeded_encrypted(&root, m);
    let cache = dir.path().join("cache");
    let filter = ScanFilter::default();

    // Sanitized never has content, whatever the key mode.
    let out = dir.path().join("san.atdb");
    let key_path = dir.path().join("san.key");
    let (info, n) = snapshot::export_filtered_with(
        &db,
        &out,
        &filter,
        Some(&SanitizePolicy::default()),
        &ExportKey::Portable(key_path.clone()),
    )
    .unwrap();
    assert_eq!(n, 4);
    assert!(!entry_names(&info).iter().any(|e| e.starts_with("blobs/")));
    assert!(!key_path.exists(), "no key file for a sanitized export");

    // None on an encrypted source: content stripped.
    let out = dir.path().join("fnone.atdb");
    snapshot::export_filtered_with(&db, &out, &filter, None, &ExportKey::None).unwrap();
    let (ro, _) = snapshot::open_read_only_with(&out, &cache, Some(provider(&[m]))).unwrap();
    assert!(
        ro.scan(&filter)
            .unwrap()
            .iter()
            .all(|e| e.content.is_none() && e.raw.is_none())
    );

    // Same: encrypted under the database key.
    let out = dir.path().join("fsame.atdb");
    let (info, _) =
        snapshot::export_filtered_with(&db, &out, &filter, None, &ExportKey::Same).unwrap();
    assert_eq!(
        entry_names(&info)
            .iter()
            .filter(|e| e.starts_with("blobs/"))
            .count(),
        8
    );
    let (ro, _) = snapshot::open_read_only_with(&out, &cache, Some(provider(&[m]))).unwrap();
    assert!(ro.warnings.is_empty(), "{:?}", ro.warnings);
    assert_eq!(
        command_of(&ro.scan(&filter).unwrap()[1]),
        Some("echo secret-snap-1")
    );

    // Portable: fresh key file.
    let out = dir.path().join("fport.atdb");
    let key_path = dir.path().join("fport.key");
    snapshot::export_filtered_with(
        &db,
        &out,
        &filter,
        None,
        &ExportKey::Portable(key_path.clone()),
    )
    .unwrap();
    let portable = blobs::read_key_file(&key_path).unwrap();
    let (ro, _) = snapshot::open_read_only_with(&out, &cache, Some(provider(&[portable]))).unwrap();
    assert!(ro.warnings.is_empty(), "{:?}", ro.warnings);
    assert_eq!(
        command_of(&ro.scan(&filter).unwrap()[2]),
        Some("echo secret-snap-2")
    );
    assert!(ro.verify().unwrap().is_empty());
}

#[test]
fn restore_carries_blobs() {
    let (dir, root) = temp_root();
    let m = master(9);
    let (_dev, db) = seeded_encrypted(&root, m);
    let out = dir.path().join("backup.atdb");
    snapshot::export_with(&db, &out, &ExportKey::Same).unwrap();
    drop(db);
    let dest = dir.path().join("restored.attemptdb");
    let report = snapshot::restore(&out, &dest, RestoreMode::IntoEmptyDir).unwrap();
    assert_eq!(report.events, 4);
    let db = open(&dest, Some(provider(&[m])), false);
    assert!(db.warnings.is_empty(), "{:?}", db.warnings);
    assert_eq!(db.blob_stats().unwrap().count, 8);
    assert_eq!(
        command_of(&db.scan(&ScanFilter::default()).unwrap()[0]),
        Some("echo secret-snap-0")
    );
    assert!(db.verify().unwrap().is_empty());
}

// ---------------------------------------------------------------------------
// Verify
// ---------------------------------------------------------------------------

#[test]
fn verify_reports_corrupt_and_missing_blobs() {
    let (_dir, root) = temp_root();
    let m = master(10);
    let (_dev, db) = seeded_encrypted(&root, m);
    assert!(db.verify().unwrap().is_empty());
    let mut files = walk(&root.join("blobs"));
    files.sort();
    assert_eq!(files.len(), 8);

    // One flipped byte in the ciphertext.
    let victim = &files[0];
    let mut bytes = std::fs::read(victim).unwrap();
    bytes[BLOB_HEADER_LEN + 1] ^= 0xff;
    std::fs::write(victim, &bytes).unwrap();
    let problems = db.verify().unwrap();
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(problems[0].contains("crc32c mismatch"), "{}", problems[0]);
    // The scan degrades to None for that row and records the problem.
    let all = db.scan(&ScanFilter::default()).unwrap();
    assert_eq!(
        all.iter()
            .filter(|e| e.content.is_none() || e.raw.is_none())
            .count(),
        1
    );
    assert!(
        db.content_warnings()
            .iter()
            .any(|w| w.contains("unreadable")),
        "{:?}",
        db.content_warnings()
    );

    // A missing blob file.
    std::fs::remove_file(&files[1]).unwrap();
    let problems = db.verify().unwrap();
    assert_eq!(problems.len(), 2, "{problems:?}");
    assert!(
        problems.iter().any(|p| p.contains("missing from blobs/")),
        "{problems:?}"
    );
    // Verify needs no key.
    drop(db);
    let db = open(&root, None, true);
    assert_eq!(db.verify().unwrap().len(), 2);
}

// ---------------------------------------------------------------------------
// Rotation
// ---------------------------------------------------------------------------

#[test]
fn rotation_reencrypts_everything_and_retires_the_old_key() {
    let (_dir, root) = temp_root();
    let old = master(11);
    let new = master(12);
    let old_id = blobs::key_id_for(&old);
    let new_id = blobs::key_id_for(&new);
    let (dev, mut db) = seeded_encrypted(&root, old);
    db.ingest(events(dev, 2, "second")).unwrap();
    db.flush().unwrap();
    assert_eq!(db.blob_stats().unwrap().count, 12);
    drop(db);

    // Ring with the new key current and the old one retained.
    let ring = provider(&[new, old]);
    let db = open(&root, Some(ring.clone()), false);
    let store = db.blob_store().clone();
    let report = store.reencrypt_all(ring.as_ref(), new_id, &new).unwrap();
    assert_eq!(report.rewritten, 12);
    assert_eq!(report.skipped, 0);
    assert!(report.failed.is_empty(), "{:?}", report.failed);
    assert_eq!(
        store.all_key_ids().unwrap().into_iter().collect::<Vec<_>>(),
        vec![new_id]
    );
    assert_eq!(
        store
            .sample_key_ids()
            .unwrap()
            .into_iter()
            .collect::<Vec<_>>(),
        vec![new_id]
    );
    // Blob ids (and therefore segment refs) are unchanged.
    assert_eq!(store.list().unwrap().len(), 12);
    assert!(db.verify().unwrap().is_empty());
    // Running again is a no-op.
    let again = store.reencrypt_all(ring.as_ref(), new_id, &new).unwrap();
    assert_eq!((again.rewritten, again.skipped), (0, 12));
    drop(db);

    // Only the new key is needed now.
    let db = open(&root, Some(provider(&[new])), true);
    assert!(db.warnings.is_empty(), "{:?}", db.warnings);
    let all = db.scan(&ScanFilter::default()).unwrap();
    assert_eq!(all.len(), 6);
    assert_eq!(command_of(&all[0]), Some("echo secret-snap-0"));
    assert_eq!(command_of(&all[5]), Some("echo secret-second-1"));
    assert!(db.content_warnings().is_empty());
    drop(db);

    // The old key alone no longer opens anything.
    let db = open(&root, Some(provider(&[old])), true);
    assert_eq!(db.warnings.len(), 1);
    assert!(db.warnings[0].contains(&new_id.to_string()));
    assert!(!db.warnings[0].contains(&old_id.to_string()));
    assert!(
        db.scan(&ScanFilter::default())
            .unwrap()
            .iter()
            .all(|e| e.content.is_none())
    );

    // A ring missing the old key cannot rotate blobs still under it.
    let (_dir2, root2) = temp_root();
    let (_d, db2) = seeded_encrypted(&root2, old);
    let partial = provider(&[new]);
    let r = db2
        .blob_store()
        .reencrypt_all(partial.as_ref(), new_id, &new)
        .unwrap();
    assert_eq!(r.rewritten, 0);
    assert_eq!(r.failed.len(), 8);
    assert!(r.failed[0].contains("no key for key_id"), "{}", r.failed[0]);
}
