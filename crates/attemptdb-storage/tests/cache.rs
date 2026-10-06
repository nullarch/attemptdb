//! `ScanCache`: a refreshing reader decodes each segment once.

use attemptdb_core::event::Provider;
use attemptdb_core::{CaptureMode, DeviceId, Event, EventKind, ProjectRef};
use attemptdb_storage::{CompactionPolicy, Database, OpenOptions, ScanCache, ScanFilter};
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

/// After a compaction the cache decodes the one merged segment and forgets
/// the inputs; what it serves is still exactly what `Database::scan` serves.
#[test]
fn refresh_after_compaction_decodes_the_merged_segment_and_drops_the_inputs() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("db");
    let mut db = writer(&root);
    let device = db.device_id();
    for b in 0..5 {
        db.ingest(events(device, 20, &format!("s{b}"))).unwrap();
        db.flush().unwrap();
    }
    db.ingest(events(device, 4, "wal")).unwrap();
    drop(db);

    let mut cache = ScanCache::new();
    let db = reader(&root);
    let r = cache.refresh(&db).unwrap();
    assert_eq!(cache.decodes, 5);
    assert_eq!(r.new_segments.len(), 5);
    let inputs: HashSet<uuid::Uuid> = r.new_segments.iter().copied().collect();
    let scanned_before = db.scan(&ScanFilter::default()).unwrap();
    assert_eq!(r.scan(&ScanFilter::default()).unwrap(), scanned_before);
    drop(db);

    let mut db = writer(&root);
    let report = db
        .compact(&CompactionPolicy {
            max_segments: 1,
            small_segment_bytes: u64::MAX,
            min_inputs: 2,
            ..Default::default()
        })
        .unwrap()
        .expect("five small segments merge");
    assert_eq!(report.inputs.len(), 5);
    drop(db);

    let db = reader(&root);
    let r = cache.refresh(&db).unwrap();
    assert_eq!(cache.decodes, 6, "exactly the merged segment was decoded");
    assert_eq!(r.new_segments, vec![report.output_segment.segment_id]);
    assert_eq!(
        r.dropped_segments.iter().copied().collect::<HashSet<_>>(),
        inputs,
        "every input was forgotten"
    );
    assert_eq!(cache.segment_count(), 1);
    assert_eq!(r.segments.len(), 1);
    assert_eq!(r.memtable.len(), 4);
    assert_eq!(r.event_count(), 104);
    let scanned = db.scan(&ScanFilter::default()).unwrap();
    assert_eq!(scanned, scanned_before, "compaction changed nothing");
    assert_eq!(r.scan(&ScanFilter::default()).unwrap(), scanned);
    let filter = ScanFilter {
        limit: Some(7),
        ..Default::default()
    };
    assert_eq!(r.scan(&filter).unwrap(), db.scan(&filter).unwrap());
    let rows: usize = r.batches().unwrap().iter().map(|b| b.num_rows()).sum();
    assert_eq!(rows, 104);
    drop(db);

    // Nothing changed since: no decode.
    let db = reader(&root);
    let r = cache.refresh(&db).unwrap();
    assert_eq!(cache.decodes, 6);
    assert!(r.new_segments.is_empty() && r.dropped_segments.is_empty());
}

// ---------------------------------------------------------------------------
// Lazy listing, column projection, scoped reads, vanished segments
// ---------------------------------------------------------------------------

use attemptdb_core::Timestamp;
use attemptdb_storage::segment::BATCH_ROWS;
use std::sync::Arc;

fn stamped(
    device: DeviceId,
    n: usize,
    project: &str,
    session: &str,
    at_micros: i64,
    kind: EventKind,
) -> Vec<Event> {
    (0..n)
        .map(|i| {
            let mut ev = Event::new(
                device,
                Provider::ClaudeCode,
                "PostToolUse",
                kind,
                ProjectRef::derive(project, None, &device),
                session.to_string(),
                CaptureMode::LocalSemantic,
                "cache-test/0.1",
            );
            ev.observed_at = Timestamp::from_micros(at_micros + i as i64);
            ev.captured_at = ev.observed_at;
            ev
        })
        .collect()
}

/// One segment of three batches where project `b` sits in the middle batch
/// only, plus a second segment holding nothing of `b`.
fn two_project_db(root: &std::path::Path) -> (Database, DeviceId) {
    let mut db = writer(root);
    let device = db.device_id();
    db.ingest(stamped(
        device,
        BATCH_ROWS,
        "/home/dev/a",
        "sa1",
        1_000_000,
        EventKind::ToolCallFinished,
    ))
    .unwrap();
    db.ingest(stamped(
        device,
        100,
        "/home/dev/b",
        "sb1",
        10_000_000,
        EventKind::ToolCallStarted,
    ))
    .unwrap();
    db.ingest(stamped(
        device,
        BATCH_ROWS - 100,
        "/home/dev/a",
        "sa2",
        11_000_000,
        EventKind::ToolCallFinished,
    ))
    .unwrap();
    db.ingest(stamped(
        device,
        BATCH_ROWS,
        "/home/dev/a",
        "sa3",
        20_000_000,
        EventKind::ToolCallFinished,
    ))
    .unwrap();
    db.flush().unwrap();
    db.ingest(stamped(
        device,
        10,
        "/home/dev/a",
        "sa4",
        30_000_000,
        EventKind::ToolCallFinished,
    ))
    .unwrap();
    db.flush().unwrap();
    (db, device)
}

fn event_ids(events: &[Event]) -> Vec<attemptdb_core::EventId> {
    events.iter().map(|e| e.event_id).collect()
}

#[test]
fn list_decodes_nothing_and_reads_on_demand() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("db");
    let (db, device) = two_project_db(&root);
    drop(db);
    let project_b = ProjectRef::derive("/home/dev/b", None, &device).project_id;

    let db = reader(&root);
    let mut cache = ScanCache::new();
    let r = cache.list(&db).unwrap();
    assert_eq!(r.segments.len(), 2);
    assert_eq!(r.new_segments.len(), 2);
    assert_eq!(cache.total_decodes(), 0, "listing reads no segment");
    assert!(r.segments.iter().all(|s| !s.is_resident()));
    assert_eq!(r.event_count(), 3 * BATCH_ROWS + 10);

    // A scoped read decodes only the rows of its scope and keeps nothing.
    let filter = ScanFilter {
        project_id: Some(project_b),
        ..Default::default()
    };
    let scoped = r.filtered_batches(&filter).unwrap();
    assert_eq!(scoped.iter().map(|b| b.num_rows()).sum::<usize>(), 100);
    assert_eq!(scoped.len(), 1, "only the batch that holds the project");
    assert_eq!(cache.total_decodes(), 0);
    assert!(r.segments.iter().all(|s| !s.is_resident()));
    assert_eq!(
        event_ids(&r.scan(&filter).unwrap()),
        event_ids(&db.scan(&filter).unwrap())
    );

    // A column read decodes only those columns.
    let mut seen = 0;
    r.segments[0]
        .read_columns(&["event_id", "project_id", "no_such_column"], &mut |b| {
            assert_eq!(b.num_columns(), 2);
            assert_eq!(b.schema().field(0).name(), "event_id");
            seen += b.num_rows();
            Ok(true)
        })
        .unwrap();
    assert_eq!(seen, 3 * BATCH_ROWS);
    assert_eq!(cache.total_decodes(), 0);

    // Asking for everything decodes once and keeps it.
    let all = r.batches().unwrap();
    assert_eq!(
        all.iter().map(|b| b.num_rows()).sum::<usize>(),
        3 * BATCH_ROWS + 10
    );
    assert_eq!(cache.total_decodes(), 2);
    assert!(r.segments.iter().all(|s| s.is_resident()));
    r.batches().unwrap();
    assert_eq!(cache.total_decodes(), 2, "decoded once");
    // A resident segment answers the same scoped read from memory.
    let again = r.filtered_batches(&filter).unwrap();
    assert_eq!(again.iter().map(|b| b.num_rows()).sum::<usize>(), 100);
}

/// The lazy two-pass read keeps exactly the rows `Database::scan` keeps,
/// for every kind of condition a filter has.
#[test]
fn scoped_reads_match_scan_for_every_condition() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("db");
    let (db, device) = two_project_db(&root);
    let a = ProjectRef::derive("/home/dev/a", None, &device).project_id;
    let b = ProjectRef::derive("/home/dev/b", None, &device).project_id;
    let sb1 = attemptdb_core::SessionId::derive(&["claude_code", "sb1"]);
    let mut excluded = db.scan(&ScanFilter::default()).unwrap();
    excluded.truncate(5);
    drop(db);
    let db = reader(&root);
    let filters = vec![
        ScanFilter {
            project_id: Some(b),
            ..Default::default()
        },
        ScanFilter {
            project_id: Some(a),
            since: Some(Timestamp::from_micros(10_500_000)),
            until: Some(Timestamp::from_micros(25_000_000)),
            ..Default::default()
        },
        ScanFilter {
            session_id: Some(sb1),
            ..Default::default()
        },
        ScanFilter {
            kinds: vec![EventKind::ToolCallStarted],
            ..Default::default()
        },
        ScanFilter {
            providers: vec!["claude_code".into()],
            project_id: Some(b),
            ..Default::default()
        },
        ScanFilter {
            providers: vec!["codex".into()],
            ..Default::default()
        },
        ScanFilter {
            captured_only: true,
            project_id: Some(b),
            ..Default::default()
        },
        ScanFilter {
            exclude_events: event_ids(&excluded),
            project_id: Some(a),
            ..Default::default()
        },
        ScanFilter {
            exclude_sessions: vec![sb1],
            ..Default::default()
        },
        ScanFilter {
            project_id: Some(a),
            limit: Some(7),
            ..Default::default()
        },
        ScanFilter::default(),
    ];
    for (i, f) in filters.iter().enumerate() {
        let mut lazy = ScanCache::new();
        let r = lazy.list(&db).unwrap();
        let mut eager = ScanCache::new();
        let e = eager.refresh(&db).unwrap();
        let want = db.scan(f).unwrap();
        assert_eq!(r.scan(f).unwrap(), want, "lazy scan, filter {i}: {f:?}");
        assert_eq!(e.scan(f).unwrap(), want, "eager scan, filter {i}");
        if f.limit.is_none() {
            let rows = |batches: Vec<arrow::array::RecordBatch>| -> usize {
                batches.iter().map(|b| b.num_rows()).sum()
            };
            assert_eq!(
                rows(r.filtered_batches(f).unwrap()),
                want.len(),
                "lazy rows, filter {i}"
            );
            assert_eq!(
                rows(e.filtered_batches(f).unwrap()),
                want.len(),
                "resident rows, filter {i}"
            );
        }
    }
}

/// A reader that read the manifest, then lost a segment to a compaction
/// whose garbage collection ran in between, starts over from a fresh
/// manifest and still gets the whole database (REPORT.md §4.3).
#[test]
fn a_segment_deleted_after_the_manifest_was_read_is_retried_from_a_fresh_manifest() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("db");
    let mut db = writer(&root);
    let device = db.device_id();
    for b in 0..4 {
        db.ingest(events(device, 20, &format!("s{b}"))).unwrap();
        db.flush().unwrap();
    }
    drop(db);

    // The reader's handle: it lists the four segments.
    let stale = reader(&root);
    assert_eq!(stale.manifest().segments.len(), 4);
    let want = stale.scan(&ScanFilter::default()).unwrap();

    // A compaction merges them, and the next flush collects the inputs.
    let mut db = writer(&root);
    db.compact(&CompactionPolicy {
        max_segments: 1,
        small_segment_bytes: u64::MAX,
        min_inputs: 2,
        ..Default::default()
    })
    .unwrap()
    .expect("four small segments merge");
    db.ingest(events(device, 1, "after")).unwrap();
    db.flush().unwrap();
    drop(db);
    let gone = segment_files(&root);
    assert!(
        stale
            .manifest()
            .segments
            .iter()
            .all(|s| !gone.contains(&s.file)),
        "the inputs are physically gone"
    );

    // Eager: the refresh itself starts over from a fresh manifest.
    let mut cache = ScanCache::new();
    let r = cache
        .refresh(&stale)
        .expect("the retry reads the new manifest");
    let rows = r
        .batches()
        .unwrap()
        .iter()
        .map(|b| b.num_rows())
        .sum::<usize>();
    assert_eq!(rows, 81, "all events, once");
    let mut got: Vec<_> = r.scan(&ScanFilter::default()).unwrap();
    assert_eq!(got.len(), 81);
    got.retain(|e| want.iter().any(|w| w.event_id == e.event_id));
    assert_eq!(
        got.len(),
        80,
        "everything the stale handle saw is still there"
    );

    // Lazy: a listing is a lease on files, so a read after the files are gone
    // fails with an error the caller can recognise, and `retry_vanished`
    // runs the whole read again from a fresh open.
    let mut cache = ScanCache::new();
    let listed = cache.list(&stale).unwrap();
    let err = listed.batches().expect_err("the listed inputs are gone");
    assert!(err.is_segment_gone(), "{err}");
    let mut attempts = 0;
    let rows = attemptdb_storage::cache::retry_vanished(|attempt| {
        attempts += 1;
        let mut cache = ScanCache::new();
        // The first attempt lists through the stale manifest.
        let r = if attempt == 0 {
            cache.list(&stale)?
        } else {
            cache.list(&reader(&root))?
        };
        Ok(r.batches()?.iter().map(|b| b.num_rows()).sum::<usize>())
    })
    .unwrap();
    assert_eq!((rows, attempts), (81, 2));
}

fn segment_files(root: &std::path::Path) -> HashSet<String> {
    std::fs::read_dir(root.join("segments"))
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect()
}

/// A segment the manifest lists and that is simply missing is not a race: the
/// retry gives up with a message that says so.
#[test]
fn a_segment_that_stays_missing_fails_with_a_clear_error() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("db");
    let mut db = writer(&root);
    let device = db.device_id();
    db.ingest(events(device, 5, "a")).unwrap();
    db.flush().unwrap();
    let file = db.manifest().segments[0].file.clone();
    drop(db);
    let db = reader(&root);
    std::fs::remove_file(root.join("segments").join(&file)).unwrap();
    // Eager: the refresh stops with a message that says what happened.
    let mut cache = ScanCache::new();
    let err = cache
        .refresh(&db)
        .map(|_| ())
        .expect_err("a missing segment is an error");
    let text = err.to_string();
    assert!(text.contains("no newer generation replaced it"), "{text}");
    assert!(text.contains(&file), "{text}");
    // Lazy: the read of the listed segment names the file.
    let mut cache = ScanCache::new();
    let r = cache.list(&db).unwrap();
    let err = r.segments[0].batches().map(|_| ()).expect_err("gone");
    assert!(err.is_segment_gone(), "{err}");
    // The helper gives up after its attempts, not before, and says so.
    let started = std::time::Instant::now();
    let err = attemptdb_storage::cache::retry_vanished(|_| r.segments[0].batches().map(|_| ()))
        .expect_err("still gone");
    assert!(err.to_string().contains("still is after 5 reads"), "{err}");
    assert!(started.elapsed() >= std::time::Duration::from_millis(100));
}

/// An event iterator cannot return an error per item, but a segment that fails
/// to decode no longer vanishes from it unannounced.
#[test]
fn a_segment_that_fails_to_decode_is_reported_not_dropped() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("db");
    let mut db = writer(&root);
    let device = db.device_id();
    db.ingest(events(device, 5, "a")).unwrap();
    db.flush().unwrap();
    db.ingest(events(device, 3, "b")).unwrap();
    db.flush().unwrap();
    let broken = db.manifest().segments[0].file.clone();
    drop(db);
    let db = reader(&root);
    let mut cache = ScanCache::new();
    let r = cache.list(&db).unwrap();
    // Damage the first segment after it was listed.
    let path = root.join("segments").join(&broken);
    let bytes = std::fs::read(&path).unwrap();
    std::fs::write(&path, &bytes[..bytes.len() / 2]).unwrap();

    assert_eq!(r.events().count(), 3, "the intact segment still reads");
    let failures = r.decode_failures();
    assert_eq!(failures.len(), 1, "{failures:?}");
    assert!(failures[0].contains(&broken), "{failures:?}");
    assert!(r.try_events().is_err());
    assert!(r.scan(&ScanFilter::default()).is_err());
    assert!(r.batches().is_err());
    let _ = Arc::strong_count(&r.segments[0]);
}
