//! Segment compaction: many small segments become one, without changing a
//! single event, in the segment format the inputs and the key dictate.
//!
//! What must hold after `Database::compact`:
//!
//! - the scan is identical (same events, ids, `source_seq`, content);
//! - the merged segment's manifest statistics are exact (deduplication
//!   prunes segments by `min/max event_id`);
//! - the inputs are tombstoned by the new generation and deleted only once
//!   a later generation is durable, with a retry when deletion fails;
//! - blobs are never rewritten, no key is needed to merge format 2
//!   segments, and a format boundary is a barrier only without a key;
//! - a failed write (disk full) leaves the database exactly as it was.

use attemptdb_core::event::{EventContent, Provider, ToolCategory, ToolRef};
use attemptdb_core::{CaptureMode, DeviceId, Event, EventId, EventKind, ProjectRef};
use attemptdb_storage::blobs::{KeyProvider, StaticKeyProvider};
use attemptdb_storage::repair::{self, RepairAction};
use attemptdb_storage::{
    CompactionPolicy, Database, OpenOptions, ScanFilter, StorageError, failpoint, segment,
};
use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn temp_root() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("db.attemptdb");
    (dir, root)
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

fn key_provider(master: [u8; 32]) -> Arc<dyn KeyProvider> {
    let mut p = StaticKeyProvider::new();
    p.set_current(master);
    Arc::new(p)
}

/// Events with varied dictionary values, content, and a raw payload so the
/// merge exercises every column kind.
fn events(dev: DeviceId, n: usize, tag: &str) -> Vec<Event> {
    (0..n)
        .map(|i| {
            let mut ev = Event::new(
                dev,
                if i % 5 == 0 {
                    Provider::Codex
                } else {
                    Provider::ClaudeCode
                },
                "PostToolUse",
                if i % 7 == 0 {
                    EventKind::ToolCallFailed
                } else {
                    EventKind::ToolCallFinished
                },
                ProjectRef::derive("/home/dev/example/project", None, &dev),
                format!("session-{tag}-{}", i / 10),
                CaptureMode::LocalSemantic,
                "compaction-test/0.1",
            );
            ev.tool = Some(ToolRef {
                name: format!("Tool-{tag}-{}", i % 3),
                category: ToolCategory::Shell,
                call_id: Some(format!("call-{tag}-{i}")),
            });
            ev.attrs.insert("x_test_tag".into(), serde_json::json!(tag));
            ev.attrs.insert("x_test_index".into(), serde_json::json!(i));
            ev.content = Some(EventContent {
                command: Some(format!("echo {tag}-{i}")),
                tool_output: Some(serde_json::json!({"line": i, "ok": i % 7 != 0})),
                ..Default::default()
            });
            ev.raw = Some(serde_json::json!({"tag": tag, "i": i, "float": 1.5}));
            ev
        })
        .collect()
}

fn policy(max_segments: usize, min_inputs: usize) -> CompactionPolicy {
    CompactionPolicy {
        max_segments,
        small_segment_bytes: u64::MAX,
        min_inputs,
    }
}

fn scan(db: &Database) -> Vec<Event> {
    db.scan(&ScanFilter::default()).unwrap()
}

fn segment_files(root: &Path) -> BTreeSet<String> {
    std::fs::read_dir(segment::segments_dir(root))
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n.ends_with(".arrow"))
        .collect()
}

fn format_of(db: &Database, file: &str) -> u16 {
    segment::segment_format_version(&segment::segments_dir(db.root()).join(file)).unwrap()
}

/// `n` flushes of `per` events each, one segment per flush.
fn seed(db: &mut Database, n: usize, per: usize, tag: &str) {
    let dev = db.device_id();
    for b in 0..n {
        db.ingest(events(dev, per, &format!("{tag}{b}"))).unwrap();
        db.flush().unwrap();
    }
}

#[test]
fn many_small_segments_merge_into_one_with_exact_events_and_bounds() {
    let (_dir, root) = temp_root();
    let mut db = open(&root, None, false);
    seed(&mut db, 6, 30, "a");
    assert_eq!(db.manifest().generation, 7);
    let before = scan(&db);
    let inputs = db.manifest().segments.clone();
    assert_eq!(inputs.len(), 6);

    // Within the limit: nothing to do, on any handle.
    assert!(db.compact(&policy(6, 2)).unwrap().is_none());
    let plan = db.compaction_plan(&policy(2, 2)).unwrap();
    assert_eq!(plan.runs.len(), 1);
    assert_eq!(plan.runs[0].inputs.len(), 6);
    assert_eq!(plan.runs[0].rows, 180);
    assert_eq!(plan.runs[0].format_version, 1);
    assert_eq!((plan.segments_before, plan.segments_after), (6, 1));

    let report = db.compact(&policy(2, 2)).unwrap().expect("one run");
    assert_eq!(report.inputs, inputs);
    assert_eq!(report.events, 180);
    assert_eq!(report.generation, 8);
    assert_eq!(
        report.input_bytes,
        inputs.iter().map(|s| s.bytes).sum::<u64>()
    );
    assert_eq!(report.output_bytes, report.output_segment.bytes);
    assert_eq!(report.pending_deletions, 6);
    assert!(db.compact(&policy(2, 2)).unwrap().is_none(), "converged");

    // The manifest lists one segment with exact statistics.
    let m = db.manifest();
    assert_eq!(m.generation, 8);
    assert_eq!(m.segments.len(), 1);
    let out = &m.segments[0];
    assert_eq!(out, &report.output_segment);
    assert_eq!(out.rows, 180);
    assert_eq!(out.min_source_seq, 1);
    assert_eq!(out.max_source_seq, 180);
    assert_eq!(
        out.min_event_id,
        before.iter().map(|e| e.event_id).min().unwrap()
    );
    assert_eq!(
        out.max_event_id,
        before.iter().map(|e| e.event_id).max().unwrap()
    );
    assert_eq!(out.min_hlc, before.iter().map(|e| e.hlc).min().unwrap());
    assert_eq!(out.max_hlc, before.iter().map(|e| e.hlc).max().unwrap());
    assert_eq!(
        out.min_observed_at,
        before.iter().map(|e| e.observed_at).min().unwrap()
    );
    assert_eq!(
        out.max_observed_at,
        before.iter().map(|e| e.observed_at).max().unwrap()
    );
    let sessions: HashSet<_> = before.iter().map(|e| e.session_id).collect();
    assert_eq!(out.session_count as usize, sessions.len());
    let mut providers: Vec<String> = before
        .iter()
        .map(|e| e.provider.as_str().to_string())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    providers.sort();
    assert_eq!(out.providers, providers);
    assert_eq!(out.project_ids, vec![before[0].project.project_id]);
    assert_eq!(m.tombstones.len(), 6);
    assert!(m.tombstones.iter().all(|t| t.since_generation == 8));
    assert_eq!(db.stats().tombstones, 6);
    assert_eq!(db.stats().segments, 1);

    // Same events, same order, same content and raw payloads.
    assert_eq!(scan(&db), before);
    assert!(db.verify().unwrap().is_empty());
    assert!(db.warnings.is_empty(), "{:?}", db.warnings);

    // Deduplication reaches the merged segment through its exact bounds.
    let old = before[17].clone();
    assert!(db.is_known(&old.event_id).unwrap());
    let r = db.ingest(vec![old]).unwrap();
    assert_eq!((r.accepted, r.duplicates), (0, 1));

    // Inputs stay on disk until a later generation is durable.
    let on_disk = segment_files(&root);
    assert_eq!(on_disk.len(), 7);
    drop(db);
    let mut db = open(&root, None, false);
    assert!(db.warnings.is_empty(), "{:?}", db.warnings);
    assert_eq!(db.stats().tombstones, 6, "same generation: still deferred");
    assert_eq!(segment_files(&root).len(), 7);
    assert_eq!(scan(&db), before);
    let dev = db.device_id();
    db.ingest(events(dev, 3, "tail")).unwrap();
    db.flush().unwrap(); // generation 9 → the six inputs go
    assert_eq!(db.manifest().generation, 9);
    assert!(db.manifest().tombstones.is_empty());
    assert_eq!(db.stats().tombstones, 0);
    let now = segment_files(&root);
    assert_eq!(now.len(), 2);
    assert!(now.contains(&report.output_segment.file));
    for input in &inputs {
        assert!(!now.contains(&input.file), "{} not deleted", input.file);
    }
    assert!(db.verify().unwrap().is_empty());
    assert_eq!(scan(&db).len(), 183);
    drop(db);
    let db = open(&root, None, true);
    assert!(db.warnings.is_empty(), "{:?}", db.warnings);
    assert_eq!(scan(&db).len(), 183);
}

#[test]
fn read_only_handles_plan_but_cannot_compact() {
    let (_dir, root) = temp_root();
    let mut db = open(&root, None, false);
    seed(&mut db, 4, 5, "r");
    drop(db);
    let ro = open(&root, None, true);
    let plan = ro.compaction_plan(&policy(1, 2)).unwrap();
    assert_eq!(plan.runs.len(), 1);
    // `&self` planning cannot happen on a `&mut` we do not have; the
    // writer-only check is the existing read-only error.
    let mut ro = ro;
    match ro.compact(&policy(1, 2)) {
        Err(StorageError::Other(msg)) => assert!(msg.contains("read-only"), "{msg}"),
        other => panic!("expected the read-only error, got {other:?}"),
    }
    assert_eq!(ro.manifest().segments.len(), 4, "nothing changed");
}

#[test]
fn large_segments_are_barriers_and_one_run_is_merged_per_call() {
    let (_dir, root) = temp_root();
    let mut db = open(&root, None, false);
    seed(&mut db, 3, 4, "x"); // three small
    let dev = db.device_id();
    db.ingest(events(dev, 400, "big")).unwrap(); // one larger segment
    db.flush().unwrap();
    seed(&mut db, 3, 4, "y"); // three small
    let sizes: Vec<u64> = db.manifest().segments.iter().map(|s| s.bytes).collect();
    let big = sizes[3];
    assert!(sizes.iter().enumerate().all(|(i, b)| i == 3 || *b < big));
    let before = scan(&db);
    let p = CompactionPolicy {
        max_segments: 1,
        small_segment_bytes: big, // strictly below: the big one is not small
        min_inputs: 2,
    };
    let plan = db.compaction_plan(&p).unwrap();
    assert_eq!(plan.runs.len(), 2, "{plan:#?}");
    assert_eq!(plan.runs[0].first_index, 0);
    assert_eq!(plan.runs[1].first_index, 4);
    assert_eq!(plan.segments_after, 3);

    let first = db.compact(&p).unwrap().unwrap();
    assert_eq!(first.inputs.len(), 3);
    assert_eq!(db.manifest().segments.len(), 5);
    assert_eq!(db.manifest().segments[0].file, first.output_segment.file);
    assert_eq!(db.manifest().segments[1].bytes, big, "the large one kept");
    let second = db.compact(&p).unwrap().unwrap();
    assert_eq!(second.inputs.len(), 3);
    assert_eq!(db.manifest().segments.len(), 3);
    assert_eq!(db.manifest().segments[2].file, second.output_segment.file);
    // The second generation's collection deleted the first run's inputs.
    let on_disk = segment_files(&root);
    for s in &first.inputs {
        assert!(!on_disk.contains(&s.file), "{} should be gone", s.file);
    }
    for s in &second.inputs {
        assert!(on_disk.contains(&s.file), "{} deferred", s.file);
    }
    assert_eq!(db.manifest().tombstones.len(), 3);
    assert!(db.compact(&p).unwrap().is_none());
    assert_eq!(scan(&db), before);
    assert!(db.verify().unwrap().is_empty());
}

#[test]
fn merged_segment_uses_bounded_batches_with_shared_dictionaries() {
    let (_dir, root) = temp_root();
    let mut db = open(&root, None, false);
    let dev = db.device_id();
    for b in 0..3 {
        // Tool names differ per flush, so every 4 096-row chunk of the
        // merged file must share one dictionary.
        let evs: Vec<Event> = (0..3_000)
            .map(|i| {
                let mut ev = Event::new(
                    dev,
                    Provider::ClaudeCode,
                    "PostToolUse",
                    EventKind::ToolCallFinished,
                    ProjectRef::derive("/home/dev/example/project", None, &dev),
                    format!("session-{b}"),
                    CaptureMode::MetadataOnly,
                    "compaction-test/0.1",
                );
                ev.tool = Some(ToolRef {
                    name: format!("Tool{b}-{}", i / 700),
                    category: ToolCategory::Other,
                    call_id: None,
                });
                ev
            })
            .collect();
        db.ingest(evs).unwrap();
        db.flush().unwrap();
    }
    let before = scan(&db);
    let report = db.compact(&policy(1, 2)).unwrap().unwrap();
    assert_eq!(report.events, 9_000);
    let path = segment::segments_dir(&root).join(&report.output_segment.file);
    let batches = segment::read_segment_batches(&path).unwrap();
    assert_eq!(batches.len(), 3);
    assert!(batches.iter().all(|b| b.num_rows() <= segment::BATCH_ROWS));
    assert_eq!(batches.iter().map(|b| b.num_rows()).sum::<usize>(), 9_000);
    assert_eq!(scan(&db), before);
    assert!(db.verify().unwrap().is_empty());
}

#[test]
fn format2_inputs_merge_without_a_key_and_blobs_are_untouched() {
    let (_dir, root) = temp_root();
    let master = [7u8; 32];
    let mut db = open(&root, Some(key_provider(master)), false);
    seed(&mut db, 4, 6, "enc");
    let before = scan(&db);
    assert!(
        before
            .iter()
            .all(|e| e.content.is_some() && e.raw.is_some())
    );
    let inputs = db.manifest().segments.clone();
    assert!(inputs.iter().all(|s| format_of(&db, &s.file) == 2));
    drop(db);
    let blobs_before = blob_snapshot(&root);
    assert!(!blobs_before.is_empty());

    // No key at all: refs are copied, nothing is decrypted or re-encrypted.
    let mut db = open(&root, None, false);
    assert!(
        db.warnings
            .iter()
            .all(|w| w.contains("encrypted content unavailable")),
        "{:?}",
        db.warnings
    );
    let plan = db.compaction_plan(&policy(1, 2)).unwrap();
    assert_eq!(plan.runs.len(), 1);
    assert_eq!(plan.runs[0].format_version, 2);
    let report = db.compact(&policy(1, 2)).unwrap().unwrap();
    assert_eq!(report.inputs, inputs);
    assert_eq!(format_of(&db, &report.output_segment.file), 2);
    assert_eq!(blob_snapshot(&root), blobs_before, "blobs untouched");
    // Without the key the content reads as `None`, as before compaction.
    let blind = scan(&db);
    assert_eq!(blind.len(), before.len());
    assert!(blind.iter().all(|e| e.content.is_none() && e.raw.is_none()));
    assert!(db.verify().unwrap().is_empty());
    drop(db);

    // With the key everything is back, byte for byte.
    let db = open(&root, Some(key_provider(master)), true);
    assert_eq!(scan(&db), before);
    assert!(db.verify().unwrap().is_empty());
}

#[test]
fn a_format_boundary_is_a_barrier_without_a_key_and_merges_with_one() {
    let (_dir, root) = temp_root();
    let master = [9u8; 32];
    let mut db = open(&root, None, false);
    seed(&mut db, 3, 5, "plain");
    drop(db);
    let mut db = open(&root, Some(key_provider(master)), false);
    seed(&mut db, 3, 5, "enc");
    let before = scan(&db);
    let formats: Vec<u16> = db
        .manifest()
        .segments
        .iter()
        .map(|s| format_of(&db, &s.file))
        .collect();
    assert_eq!(formats, vec![1, 1, 1, 2, 2, 2]);
    drop(db);

    // Without a key: two runs, one per format; two format-preserving outputs.
    let mut db = open(&root, None, false);
    let plan = db.compaction_plan(&policy(1, 2)).unwrap();
    assert_eq!(plan.runs.len(), 2, "{plan:#?}");
    assert_eq!(plan.runs[0].format_version, 1);
    assert_eq!(plan.runs[1].format_version, 2);
    let r1 = db.compact(&policy(1, 2)).unwrap().unwrap();
    let r2 = db.compact(&policy(1, 2)).unwrap().unwrap();
    assert!(db.compact(&policy(1, 2)).unwrap().is_none());
    assert_eq!(format_of(&db, &r1.output_segment.file), 1);
    assert_eq!(format_of(&db, &r2.output_segment.file), 2);
    assert_eq!(db.manifest().segments.len(), 2);
    let blobs_mid = blob_snapshot(&root);
    drop(db);

    // With the key: one run, format 2, the inline content moves into blobs.
    let mut db = open(&root, Some(key_provider(master)), false);
    assert_eq!(scan(&db), before);
    let plan = db.compaction_plan(&policy(1, 2)).unwrap();
    assert_eq!(plan.runs.len(), 1);
    assert_eq!(plan.runs[0].inputs.len(), 2);
    assert_eq!(plan.runs[0].format_version, 2);
    let r3 = db.compact(&policy(1, 2)).unwrap().unwrap();
    assert_eq!(format_of(&db, &r3.output_segment.file), 2);
    assert_eq!(db.manifest().segments.len(), 1);
    assert_eq!(scan(&db), before);
    assert!(db.verify().unwrap().is_empty());
    let blobs_after = blob_snapshot(&root);
    assert!(blobs_mid.iter().all(|(k, v)| blobs_after.get(k) == Some(v)));
    assert!(
        blobs_after.len() > blobs_mid.len(),
        "new blobs for old content"
    );
    // No plaintext left in the segment: without a key nothing decodes.
    let path = segment::segments_dir(&root).join(&r3.output_segment.file);
    for row in segment::read_segment_rows(&path).unwrap() {
        assert!(row.content_json.is_none() && row.raw_json.is_none());
        assert!(row.content_ref.is_some() && row.raw_ref.is_some());
    }
}

/// `blobs/**` as (relative path, bytes).
fn blob_snapshot(root: &Path) -> std::collections::BTreeMap<String, Vec<u8>> {
    fn walk(dir: &Path, base: &Path, out: &mut std::collections::BTreeMap<String, Vec<u8>>) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, base, out);
            } else {
                let rel = p.strip_prefix(base).unwrap().to_string_lossy().to_string();
                out.insert(rel, std::fs::read(&p).unwrap());
            }
        }
    }
    let mut out = Default::default();
    let base = root.join("blobs");
    walk(&base, &base, &mut out);
    out
}

#[test]
fn disk_full_during_compaction_leaves_the_database_as_it_was() {
    let (_dir, root) = temp_root();
    let mut db = open(&root, None, false);
    seed(&mut db, 4, 8, "df");
    let before = scan(&db);
    let manifest = db.manifest().clone();

    // Segment write fails: a torn temp file, nothing else.
    failpoint::arm_io(failpoint::SEGMENT_WRITE);
    let err = db.compact(&policy(1, 2)).unwrap_err();
    assert!(matches!(err, StorageError::Io { .. }), "{err}");
    assert_eq!(db.manifest(), &manifest);
    assert_eq!(scan(&db), before);
    let tmps = std::fs::read_dir(segment::segments_dir(&root))
        .unwrap()
        .flatten()
        .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("tmp"))
        .count();
    assert_eq!(tmps, 1);

    // Manifest write fails: the output is published but unreferenced; the
    // previous generation stays current.
    failpoint::arm_io(failpoint::MANIFEST_WRITE);
    let err = db.compact(&policy(1, 2)).unwrap_err();
    assert!(matches!(err, StorageError::Io { .. }), "{err}");
    assert_eq!(db.manifest(), &manifest);
    assert_eq!(scan(&db), before);
    assert!(db.verify().unwrap().is_empty());
    drop(db);

    // Repair sees a leftover whose events all live elsewhere, not damage.
    let plan = repair::plan(&root).unwrap();
    assert!(plan.problems.is_empty(), "{:?}", plan.problems);
    let quarantines: Vec<&RepairAction> = plan
        .actions
        .iter()
        .filter(|a| matches!(a, RepairAction::QuarantineFile { .. }))
        .collect();
    assert_eq!(quarantines.len(), 1, "{plan:#?}");
    match quarantines[0] {
        RepairAction::QuarantineFile { reason, .. } => assert!(
            reason.contains("entirely held by live segments")
                && reason.contains("interrupted flush or compaction"),
            "{reason}"
        ),
        _ => unreachable!(),
    }
    assert!(
        plan.actions
            .iter()
            .any(|a| matches!(a, RepairAction::RemoveStaleTmp { .. })),
        "the torn temp file: {plan:#?}"
    );

    // The next writer removes the temp file, tolerates the orphan, and
    // compacts successfully.
    let mut db = open(&root, None, false);
    assert!(
        db.warnings
            .iter()
            .any(|w| w.starts_with("removed stale temp file"))
            && db
                .warnings
                .iter()
                .any(|w| w.starts_with("unreferenced segment file")),
        "{:?}",
        db.warnings
    );
    assert_eq!(scan(&db), before);
    let report = db.compact(&policy(1, 2)).unwrap().unwrap();
    assert_eq!(report.events, 32);
    assert_eq!(scan(&db), before);
    assert!(db.verify().unwrap().is_empty());
}

#[test]
fn a_tombstoned_file_that_cannot_be_deleted_is_retried_later() {
    let (_dir, root) = temp_root();
    let mut db = open(&root, None, false);
    seed(&mut db, 3, 4, "held");
    let report = db.compact(&policy(1, 2)).unwrap().unwrap();
    let held_name = report.inputs[0].file.clone();
    let held_path = segment::segments_dir(&root).join(&held_name);
    // Another process (here: another handle) keeps the file locked.
    let held = std::fs::File::open(&held_path).unwrap();
    held.lock().unwrap();
    let dev = db.device_id();
    db.ingest(events(dev, 1, "t")).unwrap();
    db.flush().unwrap(); // the collection after this generation deletes
    assert_eq!(
        db.manifest().tombstones.len(),
        1,
        "{:?}",
        db.manifest().tombstones
    );
    assert_eq!(db.manifest().tombstones[0].file, held_name);
    assert!(held_path.exists());
    assert!(
        db.warnings
            .iter()
            .any(|w| w.starts_with("tombstoned segment") && w.contains(&held_name)),
        "{:?}",
        db.warnings
    );
    for other in &report.inputs[1..] {
        assert!(!segment::segments_dir(&root).join(&other.file).exists());
    }
    held.unlock().unwrap();
    drop(held);
    assert_eq!(db.collect_garbage().unwrap(), 1);
    assert!(!held_path.exists());
    assert!(db.manifest().tombstones.is_empty());
    assert!(db.verify().unwrap().is_empty());
}

#[test]
fn ids_survive_a_reopen_after_compaction() {
    let (_dir, root) = temp_root();
    let mut db = open(&root, None, false);
    seed(&mut db, 5, 7, "ids");
    let ids: Vec<EventId> = scan(&db).iter().map(|e| e.event_id).collect();
    db.compact(&policy(1, 2)).unwrap().unwrap();
    drop(db);
    let db = open(&root, None, true);
    let again: Vec<EventId> = scan(&db).iter().map(|e| e.event_id).collect();
    assert_eq!(ids, again);
    assert_eq!(db.manifest().segments.len(), 1);
}
