//! One unreadable segment must not make every open and every ingest fail:
//! the duplicate check skips it with a warning and still deduplicates
//! against the readable ones.

use attemptdb_core::event::Provider;
use attemptdb_core::{CaptureMode, DeviceId, Event, EventKind, ProjectRef};
use attemptdb_storage::{Database, OpenOptions, ScanFilter};
use std::time::Duration;

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
                "unreadable-segment/0.1",
            );
            ev.attrs.insert("x_test_index".into(), serde_json::json!(i));
            ev
        })
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

#[test]
fn a_corrupt_segment_is_skipped_with_a_warning_and_readable_ones_still_dedupe() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("db.attemptdb");
    let (a, b);
    {
        let mut db = Database::open(&root, writer()).unwrap();
        let device = db.device_id();
        a = make_events(device, 3, "a");
        db.ingest(a.clone()).unwrap();
        db.flush().unwrap();
        // Keep the two segments' id ranges apart (v7 ids order by time).
        std::thread::sleep(Duration::from_millis(5));
        b = make_events(device, 3, "b");
        db.ingest(b.clone()).unwrap();
        let seg_b = db.flush().unwrap().unwrap();
        let path = root.join("segments").join(&seg_b.file);
        // Damage the second segment beyond reading.
        std::fs::write(&path, b"this is no longer an arrow file").unwrap();
    }

    // Both kinds of open succeed.
    let ro = Database::open(
        &root,
        OpenOptions {
            read_only: true,
            ..Default::default()
        },
    )
    .unwrap();
    drop(ro);
    let mut db = Database::open(&root, OpenOptions::default()).unwrap();
    let device = db.device_id();

    // A duplicate of an event in the readable segment is still a duplicate.
    assert!(db.is_known(&a[1].event_id).unwrap());
    // An event that only the unreadable segment could vouch for is accepted
    // again (its stored copy cannot be read either way), with a warning.
    let fresh = make_events(device, 1, "fresh");
    let r = db
        .ingest(vec![a[2].clone(), b[1].clone(), fresh[0].clone()])
        .expect("one unreadable segment must not fail the ingest");
    assert_eq!(r.duplicates, 1, "{r:?}");
    assert_eq!(r.accepted, 2, "{r:?}");
    let warned: Vec<&String> = db
        .warnings
        .iter()
        .filter(|w| w.contains("is unreadable"))
        .collect();
    assert_eq!(
        warned.len(),
        1,
        "one warning, not one per lookup: {:?}",
        db.warnings
    );

    // The corrupt file is read once, not once per candidate id.
    let again = db.ingest(vec![b[1].clone(), b[2].clone()]).unwrap();
    assert_eq!(
        again.duplicates, 1,
        "b[1] is in the memtable now: {again:?}"
    );
    assert_eq!(
        db.warnings
            .iter()
            .filter(|w| w.contains("is unreadable"))
            .count(),
        1
    );
    // Reading the damaged rows is still an error: that is `attempt repair`'s
    // job, not the duplicate check's.
    assert!(db.scan(&ScanFilter::default()).is_err());
}
