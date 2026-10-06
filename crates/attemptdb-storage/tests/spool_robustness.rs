//! The spool and the framed files under attack and under version skew:
//! nothing the importer cannot use is deleted, one bad file does not stop the
//! rest, a planted symlink is never written through, a record the reader
//! would call corrupt is refused before it is acknowledged, and an unknown
//! enum value from a newer build survives the trip into the database.

use attemptdb_core::event::{EventContent, Provider};
use attemptdb_core::{CaptureMode, DeviceId, Event, EventKind, ProjectRef};
use attemptdb_storage::format::{
    FILE_HEADER_LEN, MAGIC_SPOOL, MAGIC_WAL, MAX_RECORD_PAYLOAD, record_type,
};
use attemptdb_storage::frame::{FileHeader, FrameReader, FrameWriter, Record};
use attemptdb_storage::spool::{QUARANTINE_DIR, QUARANTINE_MAX_FILES};
use attemptdb_storage::{
    Database, OpenOptions, ScanFilter, SpoolReader, SpoolWriter, StorageError,
};
use std::path::{Path, PathBuf};

fn temp_root() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("db.attemptdb");
    (dir, root)
}

fn writer() -> OpenOptions {
    OpenOptions {
        create: true,
        flush_events: usize::MAX,
        flush_bytes: usize::MAX,
        ..Default::default()
    }
}

fn make_events(device: DeviceId, n: usize, tag: &str) -> Vec<Event> {
    (0..n)
        .map(|i| {
            let mut ev = Event::new(
                device,
                Provider::ClaudeCode,
                "PostToolUse",
                EventKind::ToolCallFinished,
                ProjectRef::derive("/home/dev/example/project", None, &device),
                format!("session-{tag}"),
                CaptureMode::LocalSemantic,
                "spool-robustness/0.1",
            );
            ev.attrs.insert("x_test_tag".into(), serde_json::json!(tag));
            ev.attrs.insert("x_test_index".into(), serde_json::json!(i));
            ev
        })
        .collect()
}

fn spool_dir(root: &Path) -> PathBuf {
    root.join("spool")
}

fn spool_files(root: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(spool_dir(root))
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("spool"))
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

fn quarantined(root: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(spool_dir(root).join(QUARANTINE_DIR))
        .map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.path()).collect())
        .unwrap_or_default();
    v.sort();
    v
}

/// A record that passes its CRC but is not an event.
fn junk_record() -> Record {
    Record {
        record_type: record_type::EVENT,
        codec: 1,
        flags: 0,
        payload: b"{this is not an event".to_vec(),
        offset: 0,
    }
}

/// A record type this build has never heard of (a newer build wrote it).
fn future_record() -> Record {
    Record {
        record_type: 9,
        codec: 1,
        flags: 0,
        payload: b"{\"from\":\"the future\"}".to_vec(),
        offset: 0,
    }
}

fn write_spool(path: &Path, records: &[Record]) {
    let mut w = FrameWriter::open(path, MAGIC_SPOOL).unwrap();
    w.append(records).unwrap();
    w.sync_all().unwrap();
}

fn stored(db: &Database) -> Vec<Event> {
    let mut v = db.scan(&ScanFilter::default()).unwrap();
    v.sort_by_key(|e| e.source_seq);
    v
}

// ---------------------------------------------------------------------------
// Undecodable records are kept, never deleted with their file
// ---------------------------------------------------------------------------

#[test]
fn an_undecodable_record_is_quarantined_before_its_file_is_released() {
    let (_dir, root) = temp_root();
    let mut db = Database::open(&root, writer()).unwrap();
    let device = db.device_id();
    let good = make_events(device, 2, "mixed");
    let pending = spool_dir(&root).join("pending-mixed.spool");
    std::fs::create_dir_all(spool_dir(&root)).unwrap();
    write_spool(
        &pending,
        &[
            Record::event(&good[0]).unwrap(),
            junk_record(),
            Record::event(&good[1]).unwrap(),
            future_record(),
        ],
    );

    let r = db.import_spool().unwrap();
    assert_eq!(r.accepted, 2);
    assert_eq!(r.undecodable, 2);
    assert_eq!(r.quarantined, 2, "{r:?}");
    assert!(
        !pending.exists(),
        "the file is released once its records are saved"
    );

    // The records are kept byte for byte, in a file that is itself a valid
    // spool file (rename it to `.spool` and move it back to retry).
    let q = quarantined(&root);
    assert_eq!(q.len(), 1, "{q:?}");
    assert_eq!(q[0].extension().and_then(|e| e.to_str()), Some("rec"));
    let kept = FrameReader::scan(&q[0], MAGIC_SPOOL).unwrap();
    assert_eq!(kept.truncated_at, None);
    assert_eq!(kept.records.len(), 2);
    assert_eq!(kept.records[0].payload, junk_record().payload);
    assert_eq!(kept.records[1].record_type, 9);
    assert_eq!(kept.records[1].payload, future_record().payload);

    // The quarantine is not a spool: a second import finds nothing.
    let again = db.import_spool().unwrap();
    assert_eq!((again.spool_files, again.quarantined), (0, 0));
    assert_eq!(stored(&db).len(), 2);
}

#[test]
fn when_the_quarantine_cannot_be_written_the_file_stays() {
    let (_dir, root) = temp_root();
    let mut db = Database::open(&root, writer()).unwrap();
    let device = db.device_id();
    let good = make_events(device, 1, "blocked");
    let pending = spool_dir(&root).join("pending-blocked.spool");
    write_spool(&pending, &[Record::event(&good[0]).unwrap(), junk_record()]);
    // A plain file where the quarantine directory must go.
    let blocker = spool_dir(&root).join(QUARANTINE_DIR);
    std::fs::write(&blocker, b"in the way").unwrap();

    let r = db.import_spool().unwrap();
    assert_eq!(r.accepted, 1, "the good event still imports");
    assert_eq!(r.quarantined, 0);
    assert!(pending.exists(), "an unsaved record must keep its file");
    assert!(
        db.warnings.iter().any(|w| w.contains("not released")),
        "{:?}",
        db.warnings
    );

    // Retried (and deduplicated) until the quarantine works again.
    let r = db.import_spool().unwrap();
    assert_eq!((r.accepted, r.duplicates), (0, 1));
    assert!(pending.exists());

    std::fs::remove_file(&blocker).unwrap();
    let r = db.import_spool().unwrap();
    assert_eq!((r.accepted, r.duplicates, r.quarantined), (0, 1, 1));
    assert!(!pending.exists());
    assert_eq!(quarantined(&root).len(), 1);
    assert_eq!(stored(&db).len(), 1);
}

#[test]
fn a_full_quarantine_never_discards() {
    let (_dir, root) = temp_root();
    let mut db = Database::open(&root, writer()).unwrap();
    let pending = spool_dir(&root).join("pending-overflow.spool");
    write_spool(&pending, &[junk_record()]);
    let qdir = spool_dir(&root).join(QUARANTINE_DIR);
    std::fs::create_dir_all(&qdir).unwrap();
    for i in 0..QUARANTINE_MAX_FILES {
        std::fs::write(qdir.join(format!("old-{i}.rec")), b"x").unwrap();
    }
    let r = db.import_spool().unwrap();
    assert_eq!(r.quarantined, 0);
    assert!(pending.exists(), "no room: the file must stay");
    assert!(
        db.warnings.iter().any(|w| w.contains("is full")),
        "{:?}",
        db.warnings
    );
    // Making room lets it through.
    std::fs::remove_file(qdir.join("old-0.rec")).unwrap();
    let r = db.import_spool().unwrap();
    assert_eq!(r.quarantined, 1);
    assert!(!pending.exists());
}

// ---------------------------------------------------------------------------
// One bad file must not block the others
// ---------------------------------------------------------------------------

#[test]
fn a_corrupt_or_newer_spool_file_is_moved_aside_and_the_rest_import() {
    let (_dir, root) = temp_root();
    let mut db = Database::open(&root, writer()).unwrap();
    let device = db.device_id();
    let dir = spool_dir(&root);

    // Bad magic.
    let mut junk = b"JUNK".to_vec();
    junk.resize(64, 0x42);
    std::fs::write(dir.join("claimed-aaaa.spool"), &junk).unwrap();
    // A frame format newer than this build scans.
    let mut newer = FileHeader::new(MAGIC_SPOOL).encode().to_vec();
    newer[4..6].copy_from_slice(&2u16.to_le_bytes());
    newer.extend_from_slice(&[7u8; 40]);
    std::fs::write(dir.join("claimed-bbbb.spool"), &newer).unwrap();
    // The inbox itself is garbage too.
    std::fs::write(dir.join("inbox.spool"), &junk).unwrap();
    // And two good files.
    let events = make_events(device, 4, "good");
    write_spool(
        &dir.join("pending-1.spool"),
        &[
            Record::event(&events[0]).unwrap(),
            Record::event(&events[1]).unwrap(),
        ],
    );
    write_spool(
        &dir.join("pending-2.spool"),
        &[
            Record::event(&events[2]).unwrap(),
            Record::event(&events[3]).unwrap(),
        ],
    );

    let r = db.import_spool().unwrap();
    assert_eq!(r.accepted, 4, "{r:?}");
    assert_eq!(r.quarantined, 3, "{r:?}");
    assert!(spool_files(&root).is_empty(), "{:?}", spool_files(&root));
    assert!(!SpoolReader::new(&root).unwrap().has_pending());
    let q = quarantined(&root);
    assert_eq!(q.len(), 3, "{q:?}");
    // Moved whole: contents untouched.
    let mut sizes: Vec<u64> = q
        .iter()
        .map(|p| std::fs::metadata(p).unwrap().len())
        .collect();
    sizes.sort();
    assert_eq!(sizes, vec![64, 64, (FILE_HEADER_LEN + 40) as u64]);
    assert!(
        db.warnings
            .iter()
            .filter(|w| w.contains("moved to"))
            .count()
            >= 3,
        "{:?}",
        db.warnings
    );
    // The next sweep is clean.
    let again = db.import_spool().unwrap();
    assert_eq!((again.spool_files, again.accepted), (0, 0));
    assert_eq!(stored(&db).len(), 4);
}

// ---------------------------------------------------------------------------
// Planted symlinks
// ---------------------------------------------------------------------------

#[cfg(unix)]
mod symlinks {
    use super::*;
    use std::os::unix::fs::symlink;

    fn victim(dir: &Path) -> PathBuf {
        let v = dir.join("victim.txt");
        std::fs::write(&v, b"precious").unwrap();
        v
    }

    #[test]
    fn a_planted_committed_tmp_link_does_not_truncate_its_target() {
        let (dir, root) = temp_root();
        let w = SpoolWriter::new(&root).unwrap();
        let victim = victim(dir.path());
        // The path `write_committed` used to hand to `std::fs::write`.
        symlink(&victim, spool_dir(&root).join("inbox.spool.committed.tmp")).unwrap();

        let device = DeviceId::nil();
        w.append(&make_events(device, 2, "planted-tmp")).unwrap();
        w.append(&make_events(device, 1, "planted-tmp-2")).unwrap();

        assert_eq!(std::fs::read(&victim).unwrap(), b"precious");
        let mut db = Database::open(&root, writer()).unwrap();
        assert_eq!(db.import_spool().unwrap().accepted, 3);
    }

    #[test]
    fn a_planted_inbox_link_is_not_opened_and_no_event_is_lost() {
        let (dir, root) = temp_root();
        let w = SpoolWriter::new(&root).unwrap();
        // Small enough that the old `set_len(0)` path would have started it over.
        let victim = victim(dir.path());
        symlink(&victim, spool_dir(&root).join("inbox.spool")).unwrap();

        let ev = make_events(DeviceId::nil(), 1, "planted-inbox");
        let id = ev[0].event_id;
        let path = w.append(&ev).unwrap();
        assert!(
            path.file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("pending-"),
            "{path:?}"
        );
        assert_eq!(std::fs::read(&victim).unwrap(), b"precious");

        // The importer moves the link aside (it never reads through it) and
        // imports the private file.
        let mut db = Database::open(&root, writer()).unwrap();
        let r = db.import_spool().unwrap();
        assert_eq!(r.accepted, 1, "{r:?}");
        assert!(db.is_known(&id).unwrap());
        assert_eq!(std::fs::read(&victim).unwrap(), b"precious");
    }

    #[test]
    fn a_planted_lock_link_costs_nothing() {
        let (dir, root) = temp_root();
        let w = SpoolWriter::new(&root).unwrap();
        let victim = victim(dir.path());
        symlink(&victim, spool_dir(&root).join("inbox.lock")).unwrap();
        let ev = make_events(DeviceId::nil(), 1, "planted-lock");
        w.append(&ev).unwrap();
        assert_eq!(std::fs::read(&victim).unwrap(), b"precious");
        let mut db = Database::open(&root, writer()).unwrap();
        assert_eq!(db.import_spool().unwrap().accepted, 1);
        assert_eq!(std::fs::read(&victim).unwrap(), b"precious");
    }

    #[test]
    fn a_symlink_named_like_a_spool_file_is_moved_aside_unread() {
        let (dir, root) = temp_root();
        let mut db = Database::open(&root, writer()).unwrap();
        let victim = victim(dir.path());
        symlink(&victim, spool_dir(&root).join("claimed-evil.spool")).unwrap();
        let r = db.import_spool().unwrap();
        assert_eq!((r.spool_files, r.quarantined, r.accepted), (1, 1, 0));
        assert_eq!(std::fs::read(&victim).unwrap(), b"precious");
        assert!(spool_files(&root).is_empty());
        assert_eq!(quarantined(&root).len(), 1);
    }

    #[test]
    fn a_planted_wal_link_is_refused_and_not_truncated() {
        let (dir, root) = temp_root();
        {
            let mut db = Database::open(&root, writer()).unwrap();
            let device = db.device_id();
            db.ingest(make_events(device, 3, "wal")).unwrap();
        }
        // Replace the WAL with a link to a tiny file the old code would have
        // truncated and re-initialised as its own.
        let wal = root.join("wal").join("000001.wal");
        std::fs::remove_file(&wal).unwrap();
        let victim = victim(dir.path());
        symlink(&victim, &wal).unwrap();
        let err = Database::open(&root, writer())
            .err()
            .expect("open must refuse");
        assert!(
            matches!(err, StorageError::Io { .. }),
            "unexpected error: {err}"
        );
        assert_eq!(std::fs::read(&victim).unwrap(), b"precious");
    }

    #[test]
    fn frame_writer_refuses_a_link_and_a_directory() {
        let dir = tempfile::tempdir().unwrap();
        let victim = victim(dir.path());
        let link = dir.path().join("link.spool");
        symlink(&victim, &link).unwrap();
        assert!(FrameWriter::open(&link, MAGIC_SPOOL).is_err());
        assert!(FrameWriter::open(&link, MAGIC_WAL).is_err());
        assert_eq!(std::fs::read(&victim).unwrap(), b"precious");
        let sub = dir.path().join("sub.spool");
        std::fs::create_dir(&sub).unwrap();
        assert!(FrameWriter::open(&sub, MAGIC_SPOOL).is_err());
    }
}

// ---------------------------------------------------------------------------
// Write/read cap symmetry
// ---------------------------------------------------------------------------

fn oversized_event(device: DeviceId) -> Event {
    let mut ev = make_events(device, 1, "huge").remove(0);
    ev.content = Some(EventContent {
        prompt: Some("x".repeat(MAX_RECORD_PAYLOAD as usize + 1024)),
        ..Default::default()
    });
    ev
}

#[test]
fn an_oversized_record_is_refused_at_append_time() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("big.wal");
    let device = DeviceId::nil();
    let small = make_events(device, 2, "small");
    let mut w = FrameWriter::open(&path, MAGIC_WAL).unwrap();
    w.append(&[Record::event(&small[0]).unwrap()]).unwrap();
    let len_before = w.len();

    // Record::event refuses the encoded event...
    let err = Record::event(&oversized_event(device)).expect_err("refused");
    assert!(matches!(err, StorageError::RecordTooLarge { .. }), "{err}");
    // ...and the writer refuses a hand-built record over the cap, before
    // writing a byte.
    let huge = Record {
        record_type: record_type::EVENT,
        codec: 1,
        flags: 0,
        payload: vec![b' '; MAX_RECORD_PAYLOAD as usize + 1],
        offset: 0,
    };
    let err = w
        .append(&[Record::event(&small[1]).unwrap(), huge])
        .expect_err("refused");
    assert!(matches!(err, StorageError::RecordTooLarge { .. }), "{err}");
    assert_eq!(w.len(), len_before, "nothing of the batch was written");
    drop(w);
    assert_eq!(std::fs::metadata(&path).unwrap().len(), len_before);

    // The log is intact: nothing was acknowledged that the scan would drop.
    let scan = FrameReader::scan(&path, MAGIC_WAL).unwrap();
    assert_eq!(scan.records.len(), 1);
    assert_eq!(scan.truncated_at, None);
}

#[test]
fn the_spool_refuses_an_oversized_event_without_touching_the_inbox() {
    let (_dir, root) = temp_root();
    let w = SpoolWriter::new(&root).unwrap();
    let err = w
        .append(&[oversized_event(DeviceId::nil())])
        .expect_err("refused");
    assert!(matches!(err, StorageError::RecordTooLarge { .. }), "{err}");
    assert!(spool_files(&root).is_empty());
}

#[test]
fn ingest_rejects_only_the_oversized_event() {
    let (_dir, root) = temp_root();
    let mut db = Database::open(&root, writer()).unwrap();
    let device = db.device_id();
    let before = make_events(device, 2, "before");
    let after = make_events(device, 2, "after");
    let huge = oversized_event(device);
    let huge_id = huge.event_id;
    let batch = vec![
        before[0].clone(),
        before[1].clone(),
        huge,
        after[0].clone(),
        after[1].clone(),
    ];
    let r = db.ingest(batch).unwrap();
    assert_eq!(r.accepted, 4, "{r:?}");
    assert_eq!(r.rejected, 1);
    assert_eq!(r.rejected_ids, vec![huge_id]);
    assert!(!db.is_known(&huge_id).unwrap());
    // No gap in the sequence: the rejected event handed its number back.
    let seqs: Vec<u64> = stored(&db).iter().map(|e| e.source_seq).collect();
    assert_eq!(seqs, vec![1, 2, 3, 4]);
    db.flush().unwrap();
    drop(db);

    // Everything acknowledged survives a restart; the WAL was never poisoned.
    let db = Database::open(&root, OpenOptions::default()).unwrap();
    assert_eq!(stored(&db).len(), 4);
    assert!(db.warnings.is_empty(), "{:?}", db.warnings);
}

// ---------------------------------------------------------------------------
// Version skew: unknown enum values through the real spool -> database path
// ---------------------------------------------------------------------------

#[test]
fn unknown_enum_values_from_a_newer_hook_import_instead_of_being_dropped() {
    let (_dir, root) = temp_root();
    let mut db = Database::open(&root, writer()).unwrap();
    let device = db.device_id();

    let mut ev = make_events(device, 1, "skew").remove(0);
    ev.provider_event_name = "BrandNewHook".into();
    ev.tool = Some(attemptdb_core::event::ToolRef {
        name: "Teleport".into(),
        category: attemptdb_core::event::ToolCategory::Shell,
        call_id: None,
    });
    ev.outcome = Some(attemptdb_core::event::Outcome::success());
    ev.content = Some(EventContent {
        prompt: Some("a prompt that must not be kept under an unknown mode".into()),
        ..Default::default()
    });
    let id = ev.event_id;
    let mut v = serde_json::to_value(&ev).unwrap();
    v["kind"] = "kind_from_the_future".into();
    v["capture_mode"] = "mode_from_the_future".into();
    v["tool"]["category"] = "category_from_the_future".into();
    v["outcome"]["status"] = "status_from_the_future".into();
    let raw = Record {
        record_type: record_type::EVENT,
        codec: 1,
        flags: 0,
        payload: serde_json::to_vec(&v).unwrap(),
        offset: 0,
    };
    std::fs::create_dir_all(spool_dir(&root)).unwrap();
    write_spool(&spool_dir(&root).join("pending-skew.spool"), &[raw]);

    let r = db.import_spool().unwrap();
    assert_eq!(
        (r.accepted, r.undecodable, r.quarantined),
        (1, 0, 0),
        "{r:?}"
    );
    assert!(spool_files(&root).is_empty());
    assert!(quarantined(&root).is_empty());

    let check = |db: &Database| {
        let got = stored(db);
        assert_eq!(got.len(), 1);
        let e = &got[0];
        assert_eq!(e.event_id, id);
        assert_eq!(e.kind, EventKind::Unknown);
        assert_eq!(e.provider_event_name, "BrandNewHook");
        assert_eq!(
            e.tool.as_ref().unwrap().category,
            attemptdb_core::event::ToolCategory::Other
        );
        assert_eq!(
            e.outcome.as_ref().unwrap().status,
            attemptdb_core::event::OutcomeStatus::Unknown
        );
        // Fail closed: the unknown mode reads as the most restrictive one
        // and the content it would have governed is not stored.
        assert_eq!(e.capture_mode, CaptureMode::MetadataOnly);
        assert!(e.content.is_none());
    };
    check(&db);
    // And through a segment (the on-disk encodings are unchanged).
    db.flush().unwrap();
    drop(db);
    let db = Database::open(&root, OpenOptions::default()).unwrap();
    check(&db);
}
