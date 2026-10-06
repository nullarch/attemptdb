//! A read imports what the hooks spooled and lets go of the writer lock
//! before it reads: a long read must not keep the
//! daemon from starting, nor force a second CLI into a degraded view.

use attemptdb_capture::{Locator, ingest};
use attemptdb_core::event::Provider;
use attemptdb_core::{CaptureMode, DeviceId, Event, EventKind, ProjectRef};
use attemptdb_storage::{SpoolWriter, StorageError};

fn locator(tmp: &tempfile::TempDir) -> Locator {
    Locator::resolve(
        tmp.path(),
        Some(&tmp.path().join("data")),
        Some(&tmp.path().join("db")),
    )
}

fn events(n: usize) -> Vec<Event> {
    let device = DeviceId::derive(&["read-lock"]);
    (0..n)
        .map(|i| {
            let mut ev = Event::new(
                device,
                Provider::ClaudeCode,
                "PostToolUse",
                EventKind::ToolCallFinished,
                ProjectRef::derive("/home/dev/example/project", None, &device),
                "read-lock-session",
                CaptureMode::MetadataOnly,
                "read-lock/0",
            );
            ev.attrs.insert("x_test_index".into(), serde_json::json!(i));
            ev
        })
        .collect()
}

fn is_locked<T>(r: attemptdb_capture::Result<T>) -> bool {
    matches!(
        r,
        Err(attemptdb_capture::CaptureError::Storage(
            StorageError::Locked(_)
        ))
    )
}

#[test]
fn a_read_imports_the_spool_and_holds_no_writer_lock() {
    let tmp = tempfile::tempdir().unwrap();
    let locator = locator(&tmp);
    ingest::open_writer(&locator, true)
        .unwrap()
        .close()
        .unwrap();
    SpoolWriter::new(&locator.db_dir)
        .unwrap()
        .append(&events(5))
        .unwrap();

    // The old read path: the handle is the writer, so nothing else can write
    // while the command runs.
    {
        let (held, report, read_only) = ingest::open_fresh(&locator, false).unwrap();
        assert!(!read_only);
        assert_eq!(report.unwrap().accepted, 5);
        assert!(
            is_locked(ingest::open_writer(&locator, false)),
            "open_fresh keeps the writer lock for as long as the handle lives"
        );
        drop(held);
    }

    SpoolWriter::new(&locator.db_dir)
        .unwrap()
        .append(
            &events(3)
                .into_iter()
                .map(|mut e| {
                    e.event_id =
                        attemptdb_core::EventId::derive(&["second", &e.event_id.to_string()]);
                    e
                })
                .collect::<Vec<_>>(),
        )
        .unwrap();
    let (db, report, writer_busy) = ingest::open_for_read(&locator).unwrap();
    assert!(!writer_busy);
    assert_eq!(
        report.expect("imported").accepted,
        3,
        "the spool is imported"
    );
    assert!(db.is_read_only(), "the handle only reads");
    assert_eq!(
        db.stats().segment_rows + db.stats().memtable_rows as u64,
        8,
        "and sees what was imported"
    );
    // The point: a writer (the daemon) can start while this read goes on.
    let writer =
        ingest::open_writer(&locator, false).expect("a read holds no writer lock while it runs");
    drop(writer);
}

#[test]
fn a_read_beside_a_running_writer_reads_without_importing() {
    let tmp = tempfile::tempdir().unwrap();
    let locator = locator(&tmp);
    let mut writer = ingest::open_writer(&locator, true).unwrap();
    writer.ingest(events(4)).unwrap();
    SpoolWriter::new(&locator.db_dir)
        .unwrap()
        .append(
            &events(2)
                .into_iter()
                .map(|mut e| {
                    e.event_id =
                        attemptdb_core::EventId::derive(&["spooled", &e.event_id.to_string()]);
                    e
                })
                .collect::<Vec<_>>(),
        )
        .unwrap();

    let pending = ingest::import_pending(&locator).unwrap();
    assert!(pending.writer_busy && pending.report.is_none());
    let (db, report, writer_busy) = ingest::open_for_read(&locator).unwrap();
    assert!(writer_busy && report.is_none());
    assert_eq!(
        db.memtable_events().len(),
        4,
        "what the writer made durable"
    );
    assert!(
        db.stats().spool_pending,
        "the spool is the writer's to import"
    );
    // And the writer is undisturbed.
    writer
        .ingest(
            events(1)
                .into_iter()
                .map(|mut e| {
                    e.event_id = attemptdb_core::EventId::derive(&["later"]);
                    e
                })
                .collect(),
        )
        .unwrap();
}
