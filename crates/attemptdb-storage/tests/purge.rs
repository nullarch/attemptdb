//! `Database::purge`: rewrite segments without the rows a rule refuses.
//!
//! What must hold:
//!
//! - the WAL is flushed first, so a refused row that had not reached a
//!   segment is refused too;
//! - a segment with nothing to refuse is not touched (same id, same file);
//! - a segment with nothing to keep leaves the manifest;
//! - every kept row survives unchanged and the scan is the kept rows in
//!   order; a second purge changes nothing;
//! - a reopen sees the same database (the rewrite is durable).

use attemptdb_core::event::Provider;
use attemptdb_core::{CaptureMode, DeviceId, Event, EventKind, ProjectRef};
use attemptdb_storage::{Database, OpenOptions, ScanFilter};
use serde_json::{Value, json};
use std::path::Path;

fn open(root: &Path) -> Database {
    Database::open(
        root,
        OpenOptions {
            create: true,
            device_id: Some(DeviceId::derive(&["purge-test"])),
            flush_events: usize::MAX,
            flush_bytes: usize::MAX,
            ..Default::default()
        },
    )
    .unwrap()
}

fn event(dev: DeviceId, project: &ProjectRef, session: &str, span: bool) -> Event {
    let mut e = Event::new(
        dev,
        Provider::Codex,
        if span { "receiving" } else { "PostToolUse" },
        if span {
            EventKind::Unknown
        } else {
            EventKind::ToolCallFinished
        },
        project.clone(),
        session,
        CaptureMode::MetadataOnly,
        "purge-test/0",
    );
    if span {
        e.attrs.insert("source".into(), json!("otel"));
        e.attrs.insert("x_otel_record_type".into(), json!("span"));
        e.attrs
            .insert("x_otel_session_attributed".into(), json!(false));
    }
    e
}

fn is_span(e: &Event) -> bool {
    e.attrs.get("x_otel_record_type") == Some(&json!("span"))
}

#[test]
fn purge_rewrites_only_the_segments_with_refused_rows_and_is_durable() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("db.attemptdb");
    let dev = DeviceId::derive(&["purge-test"]);
    let project = ProjectRef::derive("/home/dev/example/project", None, &dev);
    let mut db = open(&root);
    // Segment 1: three hooks and two spans. Segment 2: two hooks, clean.
    // Segment 3: spans only. WAL: one hook and one span, not yet flushed.
    db.ingest(vec![
        event(dev, &project, "s1", false),
        event(dev, &project, "s1", true),
        event(dev, &project, "s1", false),
        event(dev, &project, "s2", true),
        event(dev, &project, "s2", false),
    ])
    .unwrap();
    db.flush().unwrap();
    db.ingest(vec![
        event(dev, &project, "s3", false),
        event(dev, &project, "s3", false),
    ])
    .unwrap();
    db.flush().unwrap();
    db.ingest(vec![
        event(dev, &project, "s4", true),
        event(dev, &project, "s4", true),
    ])
    .unwrap();
    db.flush().unwrap();
    db.ingest(vec![
        event(dev, &project, "s5", false),
        event(dev, &project, "s5", true),
    ])
    .unwrap();
    let clean = db.manifest().segments[1].clone();
    let generation = db.manifest().generation;
    let before: Vec<Event> = db.scan(&ScanFilter::default()).unwrap();
    assert_eq!(before.len(), 11);

    let report = db.purge(&|e| !is_span(e)).unwrap();
    assert_eq!(report.events_dropped, 5);
    assert_eq!(report.events_kept, 6);
    assert_eq!(
        report.segments_rewritten, 2,
        "segment 1 and the flushed WAL"
    );
    assert_eq!(report.segments_removed, 1, "the spans-only segment");
    // Flush (+1) and one generation per rewritten or removed segment (+3).
    assert_eq!(report.generation, generation + 4);
    assert_eq!(db.manifest().generation, report.generation);
    let after: Vec<Event> = db.scan(&ScanFilter::default()).unwrap();
    assert_eq!(after.len(), 6);
    assert!(after.iter().all(|e| !is_span(e)));
    let kept_before: Vec<&Event> = before.iter().filter(|e| !is_span(e)).collect();
    for (a, b) in kept_before.iter().zip(&after) {
        assert_eq!(a.event_id, b.event_id);
        assert_eq!(a.source_seq, b.source_seq);
        assert_eq!(a.attrs, b.attrs);
    }
    assert_eq!(db.manifest().segments.len(), 3);
    assert_eq!(
        db.manifest().segments[1].segment_id,
        clean.segment_id,
        "a clean segment is not rewritten"
    );
    assert_eq!(db.manifest().segments[1].file, clean.file);

    // Nothing left to refuse: the second purge is a no-op.
    let again = db.purge(&|e| !is_span(e)).unwrap();
    assert_eq!(
        (
            again.events_dropped,
            again.segments_rewritten,
            again.segments_removed
        ),
        (0, 0, 0)
    );
    assert_eq!(again.events_kept, 6);
    assert_eq!(again.generation, report.generation);
    drop(db);

    // A reopen reads the rewritten generation.
    let db = open(&root);
    let reopened: Vec<Event> = db.scan(&ScanFilter::default()).unwrap();
    assert_eq!(
        reopened.iter().map(|e| e.event_id).collect::<Vec<_>>(),
        after.iter().map(|e| e.event_id).collect::<Vec<_>>()
    );
    let _: Value = json!(db.stats().segments);
}

#[test]
fn a_large_input_becomes_several_outputs_of_the_chunk_size_in_order() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("db.attemptdb");
    let dev = DeviceId::derive(&["purge-test"]);
    let project = ProjectRef::derive("/home/dev/example/project", None, &dev);
    let mut db = open(&root);
    // Seven rows, two of them spans: five kept, in chunks of two → 2+2+1.
    db.ingest(
        (0..7)
            .map(|i| event(dev, &project, &format!("s{i}"), i == 2 || i == 5))
            .collect(),
    )
    .unwrap();
    db.flush().unwrap();
    let before: Vec<Event> = db.scan(&ScanFilter::default()).unwrap();
    let report = db.purge_chunked(&|e| !is_span(e), 2).unwrap();
    assert_eq!(report.events_dropped, 2);
    assert_eq!(report.events_kept, 5);
    assert_eq!(report.segments_rewritten, 1);
    assert_eq!(report.segments_written, 3);
    let segments = db.manifest().segments.clone();
    assert_eq!(segments.len(), 3);
    assert_eq!(
        segments.iter().map(|s| s.rows).collect::<Vec<_>>(),
        vec![2, 2, 1]
    );
    // Sequence order across the outputs is the input's.
    let seqs: Vec<u64> = segments
        .iter()
        .flat_map(|s| [s.min_source_seq, s.max_source_seq])
        .collect();
    assert!(seqs.windows(2).all(|w| w[0] <= w[1]));
    let after: Vec<Event> = db.scan(&ScanFilter::default()).unwrap();
    assert_eq!(
        after.iter().map(|e| e.event_id).collect::<Vec<_>>(),
        before
            .iter()
            .filter(|e| !is_span(e))
            .map(|e| e.event_id)
            .collect::<Vec<_>>()
    );
    // Deduplication still knows every kept id after the rewrite.
    let again = db.ingest(before.clone()).unwrap();
    assert_eq!(again.accepted, 2, "only the two refused rows are new again");
}
