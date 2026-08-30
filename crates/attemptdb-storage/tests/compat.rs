//! On-disk compatibility across builds.
//!
//! The current build must (1) read a database directory and a snapshot
//! written by an earlier build, (2) keep writing to that database, and
//! (3) refuse a format it does not know with a clear error instead of
//! misreading it. This is what makes a rolling upgrade across thousands of
//! tenant databases safe: the new binary opens the old files, and an old
//! binary meeting new files stops.
//!
//! `fixtures/db/format-v2/` is a small database written by the build that
//! introduced segment format 2 (2026-08-30): one flushed segment, a WAL
//! tail that was never flushed, and the identity/manifest files as that
//! build wrote them. `fixtures/db/format-v2.snapshot` is the same database
//! exported as an `.atdb` container (the name avoids the `*.atdb`
//! gitignore rule that protects live data). `format-v2.expected.json` is
//! what that build read back from its own files.
//!
//! Regenerate ONLY together with an intended format change:
//! `UPDATE_FIXTURE=1 cargo test -p attemptdb-storage --test compat`, then
//! review the diff and update `docs/storage-format.md`.

use attemptdb_core::event::Provider;
use attemptdb_core::{CaptureMode, DeviceId, Event, EventKind, ProjectRef};
use attemptdb_storage::snapshot::{self, RestoreMode};
use attemptdb_storage::{Database, OpenOptions, ScanFilter, StorageError};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Once;

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/db")
}
fn fixture_db() -> PathBuf {
    fixtures().join("format-v2")
}
fn fixture_snapshot() -> PathBuf {
    fixtures().join("format-v2.snapshot")
}
fn fixture_expected() -> PathBuf {
    fixtures().join("format-v2.expected.json")
}

/// What the writing build read back from its own files.
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
struct Expected {
    written_by: String,
    events: usize,
    in_segments: usize,
    in_wal: usize,
    segments: usize,
    event_ids: Vec<String>,
    /// The snapshot holds only flushed segments (the WAL tail is reported
    /// back to the exporter, never silently included).
    snapshot_events: usize,
    snapshot_event_ids: Vec<String>,
}

fn device() -> DeviceId {
    DeviceId::derive(&["compat-fixture", "device"])
}

fn events(n: usize, session: &str) -> Vec<Event> {
    let dev = device();
    (0..n)
        .map(|i| {
            let mut ev = Event::new(
                dev,
                Provider::ClaudeCode,
                "PostToolUse",
                EventKind::ToolCallFinished,
                ProjectRef::derive("/home/dev/example/project", None, &dev),
                format!("session-{session}"),
                CaptureMode::LocalSemantic,
                "compat-fixture/1",
            );
            ev.attrs.insert("x_test_index".into(), serde_json::json!(i));
            ev
        })
        .collect()
}

fn copy_dir(src: &Path, dst: &Path) {
    ensure_fixture();
    std::fs::create_dir_all(dst).unwrap();
    for entry in std::fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let to = dst.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &to);
        } else {
            std::fs::copy(entry.path(), to).unwrap();
        }
    }
}

fn open_read_only(dir: &Path) -> Database {
    Database::open(
        dir,
        OpenOptions {
            read_only: true,
            ..Default::default()
        },
    )
    .unwrap()
}

fn ids(db: &Database) -> Vec<String> {
    let mut v: Vec<String> = db
        .scan(&ScanFilter::default())
        .unwrap()
        .iter()
        .map(|e| e.event_id.to_string())
        .collect();
    v.sort();
    v
}

static REGENERATE: Once = Once::new();

/// Regenerate once per process when asked, before any test reads the
/// fixture — tests run in parallel, and the reads must see the new files.
fn ensure_fixture() {
    REGENERATE.call_once(|| {
        if std::env::var("UPDATE_FIXTURE").is_ok_and(|v| v == "1") {
            regenerate();
        }
    });
}

fn expected() -> Expected {
    ensure_fixture();
    let text = std::fs::read_to_string(fixture_expected()).unwrap_or_else(|e| {
        panic!(
            "{} missing ({e}); regenerate with UPDATE_FIXTURE=1 only for an intended format change",
            fixture_expected().display()
        )
    });
    serde_json::from_str(&text).unwrap()
}

fn regenerate() {
    let dir = fixture_db();
    if dir.exists() {
        std::fs::remove_dir_all(&dir).unwrap();
    }
    let mut db = Database::open(
        &dir,
        OpenOptions {
            create: true,
            device_id: Some(device()),
            ..Default::default()
        },
    )
    .unwrap();
    db.ingest(events(6, "a")).unwrap();
    db.flush().unwrap();
    db.ingest(events(3, "b")).unwrap();
    // Snapshot from the live handle: only flushed segments go in, and the
    // unflushed tail is reported back. The directory keeps its WAL tail
    // because we drop without `close`.
    let (_, unflushed) = snapshot::export(&db, &fixture_snapshot()).unwrap();
    assert_eq!(unflushed, 3);
    drop(db);
    // Nothing a live database leaves behind belongs in a fixture.
    let _ = std::fs::remove_file(dir.join("LOCK"));

    let ro = open_read_only(&dir);
    let wal_ids: Vec<String> = ro
        .memtable_events()
        .iter()
        .map(|e| e.event_id.to_string())
        .collect();
    let in_wal = wal_ids.len();
    let all = ids(&ro);
    let snapshot_event_ids: Vec<String> = all
        .iter()
        .filter(|id| !wal_ids.contains(id))
        .cloned()
        .collect();
    let exp = Expected {
        written_by: format!("attemptdb-storage {}", env!("CARGO_PKG_VERSION")),
        events: all.len(),
        in_segments: all.len() - in_wal,
        in_wal,
        segments: ro.manifest().segments.len(),
        event_ids: all,
        snapshot_events: snapshot_event_ids.len(),
        snapshot_event_ids,
    };
    std::fs::write(
        fixture_expected(),
        serde_json::to_string_pretty(&exp).unwrap() + "\n",
    )
    .unwrap();
}

#[test]
fn fixture_present_or_regenerated() {
    ensure_fixture();
    assert!(
        fixture_db().join("ATTEMPTDB").exists(),
        "fixture database missing"
    );
    assert!(fixture_snapshot().exists(), "fixture snapshot missing");
    let exp = expected();
    assert_eq!(exp.events, exp.in_segments + exp.in_wal);
    assert!(
        exp.in_wal > 0,
        "the fixture must carry an unflushed WAL tail"
    );
    assert_eq!(exp.snapshot_events, exp.in_segments);
}

#[test]
fn current_build_reads_a_database_written_by_an_earlier_build() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("db");
    copy_dir(&fixture_db(), &dir);
    let exp = expected();
    let db = open_read_only(&dir);
    assert_eq!(ids(&db), exp.event_ids, "same events, same ids");
    assert_eq!(db.memtable_events().len(), exp.in_wal, "WAL tail replayed");
    assert_eq!(db.manifest().segments.len(), exp.segments);
    assert!(
        db.verify().unwrap().is_empty(),
        "checksums and manifest verify"
    );
    assert_eq!(db.device_id(), device());
}

#[test]
fn current_build_continues_an_earlier_database() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("db");
    copy_dir(&fixture_db(), &dir);
    let exp = expected();
    {
        let mut db = Database::open(&dir, OpenOptions::default()).unwrap();
        let report = db.ingest(events(2, "c")).unwrap();
        assert_eq!(report.accepted, 2);
        db.flush().unwrap();
    }
    let db = open_read_only(&dir);
    assert_eq!(ids(&db).len(), exp.events + 2);
    assert_eq!(
        db.manifest().segments.len(),
        exp.segments + 1,
        "the old WAL tail and the new events flushed into one new segment"
    );
    assert!(db.verify().unwrap().is_empty());
    // The fixture's own ids are all still there.
    let now = ids(&db);
    for id in &exp.event_ids {
        assert!(now.contains(id), "{id} survived the continuation");
    }
}

#[test]
fn unknown_format_versions_are_refused_loudly() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("db");
    copy_dir(&fixture_db(), &dir);
    let identity = dir.join("ATTEMPTDB");
    let mut doc: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&identity).unwrap()).unwrap();
    doc["format_version"] = serde_json::json!(99);
    std::fs::write(&identity, serde_json::to_vec_pretty(&doc).unwrap()).unwrap();
    let err = Database::open(
        &dir,
        OpenOptions {
            read_only: true,
            ..Default::default()
        },
    )
    .err()
    .expect("a newer identity format must not open");
    match &err {
        StorageError::UnsupportedFormat {
            what,
            found,
            supported,
        } => {
            assert_eq!(*what, "identity file");
            assert_eq!(*found, 99);
            assert!(*supported < 99);
        }
        other => panic!("expected UnsupportedFormat, got {other:?}"),
    }
    let text = err.to_string();
    assert!(
        text.contains("unsupported format version 99"),
        "operator-readable message: {text}"
    );
    // Nothing was rewritten while refusing.
    let after: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&identity).unwrap()).unwrap();
    assert_eq!(after["format_version"], 99);
}

#[test]
fn snapshot_written_by_an_earlier_build_restores_and_reads() {
    let exp = expected();
    let info = snapshot::inspect(&fixture_snapshot()).unwrap();
    assert!(!info.entries.is_empty());
    let tmp = tempfile::tempdir().unwrap();
    let dest = tmp.path().join("restored");
    let report = snapshot::restore(&fixture_snapshot(), &dest, RestoreMode::IntoEmptyDir).unwrap();
    assert_eq!(report.events as usize, exp.snapshot_events);
    let db = open_read_only(&dest);
    assert_eq!(ids(&db), exp.snapshot_event_ids, "flushed events only");
    assert!(db.verify().unwrap().is_empty());
}
