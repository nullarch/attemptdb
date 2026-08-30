//! `ScanCache`: a refreshing reader decodes each segment once.

use attemptdb_core::event::Provider;
use attemptdb_core::{CaptureMode, DeviceId, Event, EventKind, ProjectRef};
use attemptdb_storage::{Database, OpenOptions, ScanCache, ScanFilter};
use std::collections::HashSet;

fn events(device: DeviceId, n: usize, tag: &str) -> Vec<Event> {
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
                "cache-test/0.1",
            );
            ev.attrs.insert("x_test_index".into(), serde_json::json!(i));
            ev
        })
        .collect()
}

fn writer(root: &std::path::Path) -> Database {
    Database::open(
        root,
        OpenOptions {
            create: true,
            flush_events: usize::MAX,
            flush_bytes: usize::MAX,
            ..Default::default()
        },
    )
    .unwrap()
}

fn reader(root: &std::path::Path) -> Database {
    Database::open(
        root,
        OpenOptions {
            read_only: true,
            ..Default::default()
        },
    )
    .unwrap()
}

fn ids(it: impl Iterator<Item = attemptdb_core::EventId>) -> HashSet<attemptdb_core::EventId> {
    it.collect()
}

#[test]
fn refresh_decodes_each_segment_once_and_tracks_the_wal() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("db");
    let mut db = writer(&root);
    let device = db.device_id();
    db.ingest(events(device, 50, "a")).unwrap();
    db.flush().unwrap(); // segment 1
    db.ingest(events(device, 7, "b")).unwrap(); // stays in the WAL
    drop(db);

    let mut cache = ScanCache::new();
    let db = reader(&root);
    let r = cache.refresh(&db).unwrap();
    assert_eq!(cache.decodes, 1, "one segment decoded");
    assert_eq!(r.new_segments.len(), 1);
    assert_eq!(r.memtable.len(), 7);
    assert_eq!(r.event_count(), 57);
    assert_eq!(ids(r.fresh_events().map(|e| e.event_id)).len(), 57);
    let scanned = db.scan(&ScanFilter::default()).unwrap();
    assert_eq!(
        ids(r.events().map(|e| e.event_id)),
        ids(scanned.iter().map(|e| e.event_id)),
        "cache sees exactly what scan sees"
    );
    let rows: usize = r.batches().unwrap().iter().map(|b| b.num_rows()).sum();
    assert_eq!(rows, 57);
    drop(db);

    // Nothing changed: no decode, same view.
    let db = reader(&root);
    let r = cache.refresh(&db).unwrap();
    assert_eq!(cache.decodes, 1);
    assert!(r.new_segments.is_empty());
    assert_eq!(r.fresh_events().count(), 7, "only the WAL is fresh");
    drop(db);

    // The WAL is flushed into segment 2 and more events arrive.
    let mut db = writer(&root);
    db.flush().unwrap();
    db.ingest(events(device, 3, "c")).unwrap();
    drop(db);
    let db = reader(&root);
    let r = cache.refresh(&db).unwrap();
    assert_eq!(cache.decodes, 2, "only the new segment was decoded");
    assert_eq!(r.new_segments.len(), 1);
    assert_eq!(r.memtable.len(), 3);
    assert_eq!(r.event_count(), 60);
    // The 7 WAL events now live in a segment: fresh again once (new segment),
    // which is why consumers dedupe by id.
    assert_eq!(r.fresh_events().count(), 10);
    assert_eq!(cache.segment_count(), 2);
    assert_eq!(cache.refreshes, 3);
}

#[test]
fn clear_forgets_everything() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("db");
    let mut db = writer(&root);
    let device = db.device_id();
    db.ingest(events(device, 5, "a")).unwrap();
    db.flush().unwrap();
    drop(db);
    let mut cache = ScanCache::new();
    let db = reader(&root);
    cache.refresh(&db).unwrap();
    assert_eq!(cache.segment_count(), 1);
    cache.clear();
    assert_eq!(cache.segment_count(), 0);
    let r = cache.refresh(&db).unwrap();
    assert_eq!(cache.decodes, 2);
    assert_eq!(r.event_count(), 5);
}
