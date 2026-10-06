//! What opening a database and deduplicating cost in segment reads.
//!
//! Events from OTel and from the newer hooks carry deterministic UUIDv5 ids,
//! which spread over the whole id space: every segment's
//! `min_event_id..max_event_id` then contains every id, and a duplicate check
//! that prunes by id range alone reads the `event_id` column of ALL segments
//! (about 130 MB and 1.2 to 2 s of CPU on 4 million events). A daemon that
//! polls the database (`sync`) paid that on every tick for a WAL holding two
//! events. A WAL event numbered past the manifest's `last_source_seq` cannot
//! be in a segment; one that is not is checked against only the segments
//! whose sequence range contains it.

use attemptdb_core::event::Provider;
use attemptdb_core::{CaptureMode, DeviceId, Event, EventId, EventKind, ProjectRef};
use attemptdb_storage::manifest::Manifest;
use attemptdb_storage::{Database, OpenOptions, ScanFilter};
use std::path::Path;

/// An event with a UUIDv5 id: deterministic, and not time-ordered.
fn event(device: DeviceId, name: &str) -> Event {
    let mut ev = Event::new(
        device,
        Provider::ClaudeCode,
        "PostToolUse",
        EventKind::ToolCallFinished,
        ProjectRef::derive("/home/dev/example/project", None, &device),
        "session-a",
        CaptureMode::LocalSemantic,
        "dedup-loads/0.1",
    );
    ev.event_id = EventId::derive(&["dedup-loads", name]);
    ev
}

fn batch(device: DeviceId, tag: &str, n: usize) -> Vec<Event> {
    (0..n)
        .map(|i| event(device, &format!("{tag}-{i}")))
        .collect()
}

fn writer() -> OpenOptions {
    OpenOptions {
        create: true,
        flush_events: usize::MAX,
        flush_bytes: usize::MAX,
        ..Default::default()
    }
}

fn read_only() -> OpenOptions {
    OpenOptions {
        read_only: true,
        ..Default::default()
    }
}

/// `segments` segments of `per` events each, named `s<seg>-<i>`, then
/// `wal` more events left in the WAL (named `wal-<i>`). Returns the ids.
fn build(root: &Path, segments: usize, per: usize, wal: usize) -> Vec<Event> {
    let mut db = Database::open(root, writer()).unwrap();
    let device = db.device_id();
    let mut all = Vec::new();
    for s in 0..segments {
        let events = batch(device, &format!("s{s}"), per);
        all.extend(events.clone());
        db.ingest(events).unwrap();
        db.flush().unwrap().unwrap();
    }
    let events = batch(device, "wal", wal);
    all.extend(events.clone());
    db.ingest(events).unwrap();
    all
}

#[test]
fn the_fixture_really_spans_the_id_space() {
    // If this stops holding the other tests prove nothing: with id ranges
    // that already prune, the loads would be zero for the wrong reason.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("db");
    build(&root, 6, 200, 0);
    let db = Database::open(&root, read_only()).unwrap();
    let probe = EventId::derive(&["dedup-loads", "wal-0"]);
    let spanning = db
        .manifest()
        .segments
        .iter()
        .filter(|s| s.min_event_id <= probe && probe <= s.max_event_id)
        .count();
    assert_eq!(
        spanning, 6,
        "every segment's id range contains an unrelated id"
    );
}

#[test]
fn opening_with_wal_events_loads_no_segment_ids() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("db");
    build(&root, 6, 200, 2);

    let ro = Database::open(&root, read_only()).unwrap();
    assert_eq!(ro.memtable_events().len(), 2, "the WAL events are replayed");
    assert_eq!(
        ro.segment_id_loads(),
        0,
        "a read-only open of 2 WAL events read segment ids"
    );
    assert_eq!(ro.scan(&ScanFilter::default()).unwrap().len(), 1202);
    drop(ro);

    let rw = Database::open(&root, writer()).unwrap();
    assert_eq!(rw.memtable_events().len(), 2);
    assert_eq!(rw.segment_id_loads(), 0, "nor did the writer's open");
}

#[test]
fn an_event_that_is_in_a_segment_is_still_a_duplicate() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("db");
    let all = build(&root, 6, 200, 2);
    let mut db = Database::open(&root, writer()).unwrap();
    let device = db.device_id();

    // A re-import of an event stored in a segment, one in the WAL, and a new one.
    let again_segment = event(device, "s3-17");
    let again_wal = event(device, "wal-1");
    let fresh = event(device, "never-seen");
    assert!(all.iter().any(|e| e.event_id == again_segment.event_id));
    let report = db
        .ingest(vec![again_segment, again_wal, fresh.clone()])
        .unwrap();
    assert_eq!((report.accepted, report.duplicates), (1, 2));
    assert!(db.is_known(&fresh.event_id).unwrap());

    // Reopening still sees each event once.
    drop(db);
    let db = Database::open(&root, read_only()).unwrap();
    let events = db.scan(&ScanFilter::default()).unwrap();
    assert_eq!(events.len(), 1203);
    let mut ids: Vec<_> = events.iter().map(|e| e.event_id).collect();
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), 1203);
}

#[test]
fn a_wal_file_that_outlived_its_flush_is_checked_against_one_segment() {
    // The crash window of a flush: the manifest naming the new segment is
    // durable, the WAL file holding the same events was not yet deleted.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("db");
    let wal_copy = dir.path().join("000003.wal.keep");
    let mut last_wal = std::path::PathBuf::new();
    {
        let mut db = Database::open(&root, writer()).unwrap();
        let device = db.device_id();
        for s in 0..4 {
            db.ingest(batch(device, &format!("s{s}"), 50)).unwrap();
            if s == 3 {
                // The WAL file that is about to be flushed and deleted.
                let wal_dir = root.join("wal");
                let mut files: Vec<_> = std::fs::read_dir(&wal_dir)
                    .unwrap()
                    .flatten()
                    .map(|e| e.path())
                    .collect();
                files.sort();
                last_wal = files.pop().unwrap();
                std::fs::copy(&last_wal, &wal_copy).unwrap();
            }
            db.flush().unwrap().unwrap();
        }
    }
    // Put the already-flushed WAL file back, as a crash would have left it.
    std::fs::copy(&wal_copy, &last_wal).unwrap();

    let db = Database::open(&root, writer()).unwrap();
    assert!(
        db.memtable_events().is_empty(),
        "the replayed events are already in a segment and must not be stored twice"
    );
    assert_eq!(db.scan(&ScanFilter::default()).unwrap().len(), 200);
    // Four segments, but only the one whose sequence range holds the events
    // can contain them.
    assert_eq!(db.manifest().segments.len(), 4);
    assert_eq!(db.segment_id_loads(), 1);
}

#[test]
fn a_manifest_that_disagrees_with_its_segments_still_deduplicates() {
    // `last_source_seq` is a promise the writer keeps; if a generation ever
    // understates it, the shortcut must not trust it over the segments.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("db");
    let wal_copy = dir.path().join("wal.keep");
    let wal_file;
    {
        let mut db = Database::open(&root, writer()).unwrap();
        let device = db.device_id();
        db.ingest(batch(device, "a", 20)).unwrap();
        let wal_dir = root.join("wal");
        let mut files: Vec<_> = std::fs::read_dir(&wal_dir)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .collect();
        files.sort();
        wal_file = files.pop().unwrap();
        std::fs::copy(&wal_file, &wal_copy).unwrap();
        db.flush().unwrap().unwrap();
    }
    std::fs::copy(&wal_copy, &wal_file).unwrap();
    // Understate last_source_seq in a new generation.
    let (mut manifest, _) = Manifest::load_latest(&root).unwrap().unwrap();
    manifest.last_source_seq = 0;
    manifest.generation += 1;
    manifest.write(&root).unwrap();

    let db = Database::open(&root, writer()).unwrap();
    assert!(
        db.memtable_events().is_empty(),
        "no duplicates from the WAL file"
    );
    assert_eq!(db.scan(&ScanFilter::default()).unwrap().len(), 20);
}
