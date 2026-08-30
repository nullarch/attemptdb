//! A flush larger than one Arrow batch must produce a readable segment.
//!
//! Segments are written as several IPC batches; the file format allows one
//! dictionary per field across all of them. Before the shared vocabulary,
//! a chunk whose tool names differed from the first chunk's failed the
//! whole flush with "Dictionary replacement detected" — which a busy
//! writer with the default 20 000-row memtable would hit in production.

use attemptdb_core::event::{Provider, ToolCategory, ToolRef};
use attemptdb_core::{CaptureMode, Event, EventKind, ProjectRef};
use attemptdb_storage::{Database, OpenOptions, ScanFilter};

#[test]
fn a_flush_of_many_batches_with_differing_dictionaries_round_trips() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("db");
    let mut db = Database::open(
        &root,
        OpenOptions {
            create: true,
            flush_events: usize::MAX,
            flush_bytes: usize::MAX,
            ..Default::default()
        },
    )
    .unwrap();
    let device = db.device_id();
    let n = 10_000usize; // > 2 × BATCH_ROWS
    let events: Vec<Event> = (0..n)
        .map(|i| {
            let mut ev = Event::new(
                device,
                Provider::ClaudeCode,
                "PostToolUse",
                EventKind::ToolCallFinished,
                ProjectRef::derive("/home/dev/example/project", None, &device),
                format!("session-{}", i / 500),
                CaptureMode::MetadataOnly,
                "large-flush/0.1",
            );
            // A tool name that changes every 300 events: each 4 096-row
            // chunk sees a different dictionary unless they share one.
            ev.tool = Some(ToolRef {
                name: format!("Tool{}", i / 300),
                category: ToolCategory::Other,
                call_id: None,
            });
            ev
        })
        .collect();
    db.ingest(events).unwrap();
    let meta = db.flush().unwrap().expect("something to flush");
    assert_eq!(meta.rows as usize, n);
    drop(db);

    let db = Database::open(
        &root,
        OpenOptions {
            read_only: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(db.warnings.is_empty(), "{:?}", db.warnings);
    let back = db.scan(&ScanFilter::default()).unwrap();
    assert_eq!(back.len(), n);
    let last = back.iter().max_by_key(|e| e.source_seq).unwrap();
    assert_eq!(
        last.tool.as_ref().unwrap().name,
        format!("Tool{}", (n - 1) / 300),
        "dictionary values survive across chunks"
    );
    let rows: usize = db
        .batches(&ScanFilter::default())
        .unwrap()
        .iter()
        .map(|b| b.num_rows())
        .sum();
    assert_eq!(rows, n);
    assert!(db.verify().unwrap().is_empty());
}
