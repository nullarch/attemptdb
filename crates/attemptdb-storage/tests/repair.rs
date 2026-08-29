//! `attempt repair` and `snapshot restore` scenarios.
//!
//! Every scenario is produced by a real writer (in-process ingest with the
//! same failpoints the crash suite uses) followed by the file surgery a
//! crash or a disk fault would leave behind. The contract under test: `plan`
//! names every fix and every loss, `apply` restores everything that is still
//! on disk without duplicating or silently dropping events, a second `plan`
//! is empty, and the repaired database opens cleanly.

#![cfg(unix)]

use attemptdb_core::event::{EventContent, Provider};
use attemptdb_core::{CaptureMode, DeviceId, Event, EventId, EventKind, ProjectRef};
use attemptdb_storage::format::{MAGIC_SPOOL, MAGIC_WAL};
use attemptdb_storage::frame::FrameReader;
use attemptdb_storage::repair::{self, RepairAction, RepairPlan};
use attemptdb_storage::snapshot::{self, RestoreMode};
use attemptdb_storage::{Database, OpenOptions, ScanFilter, SpoolWriter, StorageError, failpoint};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn temp_root() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("db.attemptdb");
    (dir, root)
}

fn writer_options() -> OpenOptions {
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
                "repair-test/0.1",
            );
            ev.attrs.insert("tag".into(), serde_json::json!(tag));
            ev.attrs.insert("index".into(), serde_json::json!(i));
            ev.content = Some(EventContent {
                tool_output: Some(serde_json::Value::String("output ".repeat(30 + i))),
                ..Default::default()
            });
            ev
        })
        .collect()
}

/// Three flushed segments (source_seq 1..20, 21..40, 41..60; generations
/// 2, 3, 4) plus ten events still in the WAL (61..70). Returns every
/// acknowledged event in sequence order.
fn seeded(root: &Path) -> Vec<Event> {
    let mut db = Database::open(root, writer_options()).unwrap();
    let device = db.device_id();
    for b in 0..3 {
        db.ingest(make_events(device, 20, &format!("batch-{b}")))
            .unwrap();
        db.flush().unwrap();
    }
    db.ingest(make_events(device, 10, "wal")).unwrap();
    assert_eq!(db.manifest().generation, 4);
    let mut all = db.scan(&ScanFilter::default()).unwrap();
    all.sort_by_key(|e| e.source_seq);
    assert_eq!(all.len(), 70);
    all
}

fn all_events(db: &Database) -> Vec<Event> {
    db.scan(&ScanFilter::default()).expect("scan")
}

fn ids(events: &[Event]) -> BTreeSet<EventId> {
    events.iter().map(|e| e.event_id).collect()
}

/// Open as the writer and insist on a clean recovery.
fn open_clean(root: &Path) -> Database {
    let db = Database::open(root, OpenOptions::default())
        .unwrap_or_else(|e| panic!("open after repair: {e}"));
    assert!(
        db.warnings.is_empty(),
        "warnings after repair: {:?}",
        db.warnings
    );
    let problems = db.verify().unwrap();
    assert!(problems.is_empty(), "verify after repair: {problems:?}");
    db
}

/// Every event in `expected` present exactly once and nothing else.
fn assert_exact(db: &Database, expected: &[Event]) {
    let events = all_events(db);
    assert_eq!(ids(&events).len(), events.len(), "duplicate event ids");
    assert_eq!(ids(&events), ids(expected));
    for e in &events {
        let want = expected.iter().find(|x| x.event_id == e.event_id).unwrap();
        assert_eq!(
            e.source_seq, want.source_seq,
            "event {} renumbered",
            e.event_id
        );
    }
}

fn files_with_extension(dir: &Path, ext: &str) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.is_file() && p.extension().and_then(|e| e.to_str()) == Some(ext))
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out
}

/// Segment files in creation order (UUIDv7 names sort by time).
fn segment_files(root: &Path) -> Vec<PathBuf> {
    files_with_extension(&root.join("segments"), "arrow")
}

fn manifest_files(root: &Path) -> Vec<PathBuf> {
    files_with_extension(&root.join("manifest"), "json")
}

fn flip_byte(path: &Path, offset: usize) {
    let mut bytes = std::fs::read(path).unwrap();
    bytes[offset] ^= 0xff;
    std::fs::write(path, bytes).unwrap();
}

fn file_name(p: &Path) -> String {
    p.file_name().unwrap().to_string_lossy().to_string()
}

fn quarantines(plan: &RepairPlan) -> Vec<(&PathBuf, &String)> {
    plan.actions
        .iter()
        .filter_map(|a| match a {
            RepairAction::QuarantineFile { path, reason } => Some((path, reason)),
            _ => None,
        })
        .collect()
}

fn adoptions(plan: &RepairPlan) -> Vec<&str> {
    plan.actions
        .iter()
        .filter_map(|a| match a {
            RepairAction::AdoptSegment { file, .. } => Some(file.as_str()),
            _ => None,
        })
        .collect()
}

fn assert_plan_empty(root: &Path, context: &str) {
    let again = repair::plan(root).unwrap();
    assert!(
        again.actions.is_empty(),
        "{context}: second plan still has actions: {:#?}",
        again.actions
    );
    assert!(
        again.problems.is_empty(),
        "{context}: second plan still has problems: {:#?}",
        again.problems
    );
}

fn apply_all(root: &Path, plan: &RepairPlan, context: &str) -> repair::RepairReport {
    let report =
        repair::apply(root, plan).unwrap_or_else(|e| panic!("{context}: apply failed: {e}"));
    assert!(
        report.skipped.is_empty(),
        "{context}: skipped actions: {:#?}",
        report.skipped
    );
    assert_eq!(
        report.applied.len(),
        plan.actions.len(),
        "{context}: every planned action applied"
    );
    report
}

// ---------------------------------------------------------------------------
// Healthy database
// ---------------------------------------------------------------------------

#[test]
fn healthy_database_has_an_empty_plan() {
    let (_dir, root) = temp_root();
    seeded(&root);
    let plan = repair::plan(&root).unwrap();
    assert!(plan.is_empty(), "{plan:#?}");
    let report = repair::apply(&root, &plan).unwrap();
    assert_eq!(report, repair::RepairReport::default());
    assert_eq!(
        manifest_files(&root).len(),
        4,
        "an empty plan writes no generation"
    );
}

#[test]
fn plan_refuses_a_directory_that_is_not_a_database() {
    let dir = tempfile::tempdir().unwrap();
    assert!(matches!(
        repair::plan(dir.path()),
        Err(StorageError::NotADatabase(_))
    ));
    assert!(matches!(
        repair::plan(&dir.path().join("missing")),
        Err(StorageError::NotADatabase(_))
    ));
}

// ---------------------------------------------------------------------------
// (1) Rejected newest generation → unreferenced segment → adopt
// ---------------------------------------------------------------------------

#[test]
fn adopts_the_segment_of_a_rejected_newest_generation() {
    let (_dir, root) = temp_root();
    let acked = seeded(&root);
    let hidden = file_name(segment_files(&root).last().unwrap());
    let newest = manifest_files(&root).pop().unwrap();
    let len = std::fs::metadata(&newest).unwrap().len() as usize;
    flip_byte(&newest, len / 2);

    // Before: generation 3 wins and the events of segment 3 are hidden.
    let before = Database::open(
        &root,
        OpenOptions {
            read_only: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(before.manifest().generation, 3);
    assert_eq!(all_events(&before).len(), 50);
    drop(before);

    let plan = repair::plan(&root).unwrap();
    assert_eq!(adoptions(&plan), vec![hidden.as_str()], "{plan:#?}");
    let q = quarantines(&plan);
    assert_eq!(q.len(), 1, "{plan:#?}");
    assert_eq!(q[0].0, &newest);
    assert_eq!(plan.actions.len(), 2, "{plan:#?}");
    assert!(plan.problems.is_empty(), "{:#?}", plan.problems);
    if let RepairAction::AdoptSegment {
        rows,
        min_seq,
        max_seq,
        ..
    } = &plan.actions[0]
    {
        assert_eq!((*rows, *min_seq, *max_seq), (20, 41, 60));
    } else {
        panic!("first action is the adoption: {:?}", plan.actions[0]);
    }

    let report = apply_all(&root, &plan, "adopt");
    assert_eq!(report.new_generation, Some(5));
    assert!(
        root.join("manifest")
            .join("gen-000004.json.corrupt")
            .is_file()
    );
    assert_plan_empty(&root, "adopt");

    let db = open_clean(&root);
    assert_eq!(db.manifest().generation, 5);
    assert_eq!(db.manifest().segments.len(), 3);
    assert_eq!(db.stats().last_source_seq, 70);
    assert_exact(&db, &acked);
    // The adopted entry carries statistics computed from the file.
    let adopted = db
        .manifest()
        .segments
        .iter()
        .find(|s| s.file == hidden)
        .unwrap();
    assert_eq!(
        (
            adopted.rows,
            adopted.min_source_seq,
            adopted.max_source_seq,
            adopted.session_count
        ),
        (20, 41, 60, 1)
    );
    assert_eq!(adopted.providers, vec!["claude_code".to_string()]);
    assert_eq!(adopted.project_ids.len(), 1);
    drop(db);
    // Writing continues from the recovered sequence.
    let mut db = Database::open(&root, OpenOptions::default()).unwrap();
    let device = db.device_id();
    db.ingest(make_events(device, 3, "after")).unwrap();
    assert_eq!(db.stats().last_source_seq, 73);
    db.flush().unwrap();
    assert_eq!(db.manifest().generation, 6);
}

// ---------------------------------------------------------------------------
// (2) Every generation corrupt → rebuild
// ---------------------------------------------------------------------------

#[test]
fn rebuilds_the_manifest_when_every_generation_is_corrupt() {
    let (_dir, root) = temp_root();
    let acked = seeded(&root);
    let manifests = manifest_files(&root);
    assert_eq!(manifests.len(), 4);
    // Two ways to be corrupt: a flipped byte and a truncated document.
    for (i, path) in manifests.iter().enumerate() {
        if i % 2 == 0 {
            let len = std::fs::metadata(path).unwrap().len() as usize;
            flip_byte(path, len / 2);
        } else {
            let bytes = std::fs::read(path).unwrap();
            std::fs::write(path, &bytes[..bytes.len() / 3]).unwrap();
        }
    }
    assert!(matches!(
        Database::open(&root, OpenOptions::default()),
        Err(StorageError::Corrupt {
            what: "manifest",
            ..
        })
    ));

    let plan = repair::plan(&root).unwrap();
    let rebuild: Vec<&RepairAction> = plan
        .actions
        .iter()
        .filter(|a| matches!(a, RepairAction::RebuildManifest { .. }))
        .collect();
    assert_eq!(rebuild.len(), 1, "{plan:#?}");
    if let RepairAction::RebuildManifest {
        from_generation,
        segments,
    } = rebuild[0]
    {
        assert_eq!(*from_generation, 0);
        let mut expected: Vec<String> = segment_files(&root).iter().map(|p| file_name(p)).collect();
        expected.sort();
        let mut got = segments.clone();
        got.sort();
        assert_eq!(got, expected);
    }
    assert_eq!(
        quarantines(&plan).len(),
        4,
        "every corrupt generation is quarantined: {plan:#?}"
    );
    assert!(
        adoptions(&plan).is_empty(),
        "a rebuild lists its segments instead of adopting one by one"
    );
    assert!(plan.problems.is_empty(), "{:#?}", plan.problems);

    let report = apply_all(&root, &plan, "rebuild");
    assert_eq!(report.new_generation, Some(5));
    assert_eq!(manifest_files(&root).len(), 1);
    assert_eq!(
        files_with_extension(&root.join("manifest"), "corrupt").len(),
        4
    );
    assert_plan_empty(&root, "rebuild");

    let db = open_clean(&root);
    assert_eq!(db.manifest().generation, 5);
    assert_eq!(db.manifest().segments.len(), 3);
    assert_eq!(db.stats().last_source_seq, 70);
    assert_eq!(
        db.stats().memtable_rows,
        10,
        "the WAL tail is replayed on top of the rebuilt segments"
    );
    assert_exact(&db, &acked);
    assert_eq!(db.identity().db_id, db.manifest().db_id);
}

// ---------------------------------------------------------------------------
// (3) Referenced segment corrupt → quarantine + missing range reported
// ---------------------------------------------------------------------------

#[test]
fn quarantines_a_corrupt_referenced_segment_and_reports_the_missing_range() {
    let (_dir, root) = temp_root();
    let acked = seeded(&root);
    let segments = segment_files(&root);
    let target = segments[1].clone(); // source_seq 21..40
    let len = std::fs::metadata(&target).unwrap().len() as usize;
    flip_byte(&target, len / 2);

    let plan = repair::plan(&root).unwrap();
    let q = quarantines(&plan);
    assert_eq!(q.len(), 1, "{plan:#?}");
    assert_eq!(q[0].0, &target);
    assert_eq!(plan.actions.len(), 1, "{plan:#?}");
    assert_eq!(plan.problems.len(), 1, "{:#?}", plan.problems);
    assert!(plan.problems[0].contains("21..40"), "{}", plan.problems[0]);
    assert!(
        plan.problems[0].contains(&file_name(&target)),
        "{}",
        plan.problems[0]
    );

    let report = apply_all(&root, &plan, "corrupt segment");
    assert_eq!(report.new_generation, Some(5));
    let quarantined = root
        .join("segments")
        .join("quarantine")
        .join(file_name(&target));
    assert!(
        quarantined.is_file(),
        "the damaged file is kept, not deleted"
    );
    assert!(!target.exists());
    assert_eq!(segment_files(&root).len(), 2);
    assert_plan_empty(&root, "corrupt segment");

    let db = open_clean(&root);
    assert_eq!(db.manifest().generation, 5);
    assert_eq!(db.manifest().segments.len(), 2);
    let expected: Vec<Event> = acked
        .iter()
        .filter(|e| !(21..=40).contains(&e.source_seq))
        .cloned()
        .collect();
    assert_eq!(expected.len(), 50);
    assert_exact(&db, &expected);
    assert_eq!(
        db.stats().last_source_seq,
        70,
        "the sequence is not reused for the lost range"
    );
}

/// A truncated segment (footer gone) is unreadable rather than merely
/// mismatching; same outcome.
#[test]
fn quarantines_an_unreadable_referenced_segment() {
    let (_dir, root) = temp_root();
    let acked = seeded(&root);
    let target = segment_files(&root)[0].clone(); // source_seq 1..20
    let bytes = std::fs::read(&target).unwrap();
    std::fs::write(&target, &bytes[..bytes.len() / 2]).unwrap();

    let plan = repair::plan(&root).unwrap();
    assert_eq!(quarantines(&plan).len(), 1, "{plan:#?}");
    assert!(
        plan.problems.iter().any(|p| p.contains("1..20")),
        "{:#?}",
        plan.problems
    );
    apply_all(&root, &plan, "unreadable segment");
    assert_plan_empty(&root, "unreadable segment");
    let db = open_clean(&root);
    let expected: Vec<Event> = acked
        .iter()
        .filter(|e| e.source_seq > 20)
        .cloned()
        .collect();
    assert_exact(&db, &expected);
}

// ---------------------------------------------------------------------------
// (4) Overlapping unreferenced segment → quarantined, never adopted
// ---------------------------------------------------------------------------

#[test]
fn overlapping_unreferenced_segment_is_quarantined_not_adopted() {
    let (_dir, root) = temp_root();
    let mut db = Database::open(&root, writer_options()).unwrap();
    let device = db.device_id();
    db.ingest(make_events(device, 10, "a")).unwrap();
    // The segment of this flush is published; the manifest naming it is not.
    failpoint::arm_io(failpoint::MANIFEST_WRITE);
    assert!(db.flush().is_err());
    // The retry writes a second segment with the same events.
    db.flush().unwrap();
    assert_eq!(db.manifest().generation, 2);
    db.ingest(make_events(device, 5, "b")).unwrap();
    let acked = all_events(&db);
    assert_eq!(acked.len(), 15);
    drop(db);

    let segments = segment_files(&root);
    assert_eq!(segments.len(), 2);
    let orphan = segments[0].clone();
    let referenced = file_name(&segments[1]);

    let plan = repair::plan(&root).unwrap();
    assert!(
        adoptions(&plan).is_empty(),
        "an overlapping segment must not be adopted: {plan:#?}"
    );
    let q = quarantines(&plan);
    assert_eq!(q.len(), 1, "{plan:#?}");
    assert_eq!(q[0].0, &orphan);
    assert!(
        q[0].1.contains("overlaps") && q[0].1.contains(&referenced),
        "{}",
        q[0].1
    );
    assert_eq!(plan.actions.len(), 1, "{plan:#?}");
    assert!(plan.problems.is_empty(), "{:#?}", plan.problems);

    let report = apply_all(&root, &plan, "overlap");
    assert_eq!(
        report.new_generation, None,
        "quarantining an orphan needs no new generation"
    );
    assert!(
        root.join("segments")
            .join("quarantine")
            .join(file_name(&orphan))
            .is_file()
    );
    assert_plan_empty(&root, "overlap");
    let db = open_clean(&root);
    assert_eq!(db.manifest().generation, 2);
    assert_exact(&db, &acked);
}

/// Two orphans that overlap each other (no valid generation at all): the
/// one covering more of the sequence is adopted, the other quarantined.
#[test]
fn among_overlapping_orphans_the_widest_is_adopted() {
    let (_dir, root) = temp_root();
    let mut db = Database::open(&root, writer_options()).unwrap();
    let device = db.device_id();
    db.ingest(make_events(device, 10, "a")).unwrap();
    failpoint::arm_io(failpoint::MANIFEST_WRITE);
    assert!(db.flush().is_err());
    db.ingest(make_events(device, 5, "b")).unwrap();
    db.flush().unwrap(); // second segment: 1..15
    let acked = all_events(&db);
    drop(db);
    for m in manifest_files(&root) {
        std::fs::write(&m, b"{").unwrap();
    }
    let segments = segment_files(&root);
    let plan = repair::plan(&root).unwrap();
    let rebuilt = plan.actions.iter().find_map(|a| match a {
        RepairAction::RebuildManifest { segments, .. } => Some(segments.clone()),
        _ => None,
    });
    assert_eq!(rebuilt, Some(vec![file_name(&segments[1])]), "{plan:#?}");
    let q = quarantines(&plan);
    assert!(
        q.iter()
            .any(|(p, reason)| *p == &segments[0] && reason.contains("overlaps")),
        "{plan:#?}"
    );
    apply_all(&root, &plan, "widest orphan");
    assert_plan_empty(&root, "widest orphan");
    let db = open_clean(&root);
    assert_exact(&db, &acked);
}

// ---------------------------------------------------------------------------
// (5) Stale temp files
// ---------------------------------------------------------------------------

#[test]
fn removes_stale_temp_files() {
    let (_dir, root) = temp_root();
    let mut db = Database::open(&root, writer_options()).unwrap();
    let device = db.device_id();
    db.ingest(make_events(device, 10, "a")).unwrap();
    failpoint::arm_io(failpoint::SEGMENT_WRITE);
    assert!(db.flush().is_err(), "a torn segment temp file stays behind");
    let acked = all_events(&db);
    drop(db);
    let seg_tmp = files_with_extension(&root.join("segments"), "tmp");
    assert_eq!(seg_tmp.len(), 1);
    let man_tmp = root.join("manifest").join("gen-000099.json.tmp");
    std::fs::write(&man_tmp, b"{\"format_version\":1,").unwrap();
    let id_tmp = root.join("ATTEMPTDB.tmp");
    std::fs::write(&id_tmp, b"{").unwrap();

    let plan = repair::plan(&root).unwrap();
    let mut removed: Vec<PathBuf> = plan
        .actions
        .iter()
        .filter_map(|a| match a {
            RepairAction::RemoveStaleTmp { path } => Some(path.clone()),
            _ => None,
        })
        .collect();
    removed.sort();
    let mut expected = vec![seg_tmp[0].clone(), man_tmp.clone(), id_tmp.clone()];
    expected.sort();
    assert_eq!(removed, expected, "{plan:#?}");
    assert_eq!(plan.actions.len(), 3, "{plan:#?}");
    assert!(plan.problems.is_empty(), "{:#?}", plan.problems);
    assert!(
        !plan.needs_confirmation(),
        "removing temp files is not destructive"
    );

    apply_all(&root, &plan, "stale tmp");
    assert!(!seg_tmp[0].exists() && !man_tmp.exists() && !id_tmp.exists());
    assert_plan_empty(&root, "stale tmp");
    let db = open_clean(&root);
    assert_exact(&db, &acked);
}

// ---------------------------------------------------------------------------
// (6) Bad-magic spool file
// ---------------------------------------------------------------------------

#[test]
fn quarantines_bad_magic_spool_files() {
    let (_dir, root) = temp_root();
    let acked = seeded(&root);
    let spool = root.join("spool");
    std::fs::create_dir_all(&spool).unwrap();
    let inbox = spool.join("inbox.spool");
    let claimed = spool.join("claimed-0000.spool");
    let mut junk = b"JUNK".to_vec();
    junk.resize(64, 0x42);
    std::fs::write(&inbox, &junk).unwrap();
    std::fs::write(&claimed, &junk).unwrap();
    std::fs::write(spool.join("inbox.spool.committed"), 64u64.to_le_bytes()).unwrap();

    // Hooks cannot append and the writer cannot import (the failed import
    // claims the inbox, renaming it; a new junk inbox takes its place).
    let device = acked[0].device_id;
    assert!(
        SpoolWriter::new(&root)
            .unwrap()
            .append(&make_events(device, 1, "blocked"))
            .is_err()
    );
    let mut db = Database::open(&root, OpenOptions::default()).unwrap();
    assert!(matches!(
        db.import_spool(),
        Err(StorageError::Corrupt { .. })
    ));
    drop(db);
    assert!(!inbox.exists(), "the failed import claimed the inbox");
    std::fs::write(&inbox, &junk).unwrap();
    std::fs::write(spool.join("inbox.spool.committed"), 64u64.to_le_bytes()).unwrap();
    assert_eq!(files_with_extension(&spool, "spool").len(), 3);

    let plan = repair::plan(&root).unwrap();
    let q: Vec<String> = quarantines(&plan)
        .iter()
        .map(|(p, _)| file_name(p))
        .collect();
    assert_eq!(q.len(), 3, "{plan:#?}");
    assert!(
        q.contains(&"inbox.spool".to_string()) && q.contains(&"claimed-0000.spool".to_string()),
        "{plan:#?}"
    );
    assert!(
        quarantines(&plan)
            .iter()
            .all(|(_, r)| r.contains("bad magic")),
        "{plan:#?}"
    );
    assert_eq!(plan.actions.len(), 3, "{plan:#?}");
    assert!(plan.problems.is_empty(), "{:#?}", plan.problems);
    assert!(plan.needs_confirmation());

    apply_all(&root, &plan, "bad magic spool");
    assert!(spool.join("inbox.spool.corrupt").is_file());
    assert!(spool.join("claimed-0000.spool.corrupt").is_file());
    assert_eq!(files_with_extension(&spool, "corrupt").len(), 3);
    assert!(files_with_extension(&spool, "spool").is_empty());
    assert!(
        !spool.join("inbox.spool.committed").exists(),
        "a stale committed hint is dropped with the inbox"
    );
    assert_plan_empty(&root, "bad magic spool");

    // Hooks append again and the writer imports what they wrote.
    let fresh = make_events(device, 3, "after");
    SpoolWriter::new(&root).unwrap().append(&fresh).unwrap();
    let mut db = open_clean(&root);
    let r = db.import_spool().unwrap();
    assert_eq!((r.accepted, r.spool_files), (3, 1));
    let mut expected = acked.clone();
    expected.extend(all_events(&db).into_iter().filter(|e| e.source_seq > 70));
    assert_exact(&db, &expected);
}

// ---------------------------------------------------------------------------
// (7) Identity file missing → recreated from the manifest
// ---------------------------------------------------------------------------

#[test]
fn recreates_a_missing_identity_file() {
    let (_dir, root) = temp_root();
    let acked = seeded(&root);
    let before = Database::open(
        &root,
        OpenOptions {
            read_only: true,
            ..Default::default()
        },
    )
    .unwrap();
    let (db_id, device_id) = (before.identity().db_id, before.identity().device_id);
    drop(before);
    std::fs::remove_file(root.join("ATTEMPTDB")).unwrap();
    assert!(matches!(
        Database::open(&root, OpenOptions::default()),
        Err(StorageError::NotADatabase(_))
    ));

    let plan = repair::plan(&root).unwrap();
    assert_eq!(
        plan.actions,
        vec![RepairAction::RecreateIdentity { db_id, device_id }],
        "{plan:#?}"
    );
    assert!(plan.problems.is_empty(), "{:#?}", plan.problems);
    assert!(!plan.needs_confirmation());
    let report = apply_all(&root, &plan, "identity");
    assert_eq!(report.new_generation, None);
    assert_plan_empty(&root, "identity");

    let db = open_clean(&root);
    assert_eq!(db.identity().db_id, db_id);
    assert_eq!(db.identity().device_id, device_id);
    assert!(db.identity().extra.contains_key("recreated_by_repair_at"));
    assert_exact(&db, &acked);
}

#[test]
fn quarantines_a_corrupt_identity_file_before_recreating_it() {
    let (_dir, root) = temp_root();
    let acked = seeded(&root);
    let identity = root.join("ATTEMPTDB");
    std::fs::write(&identity, b"{ not json").unwrap();
    assert!(matches!(
        Database::open(&root, OpenOptions::default()),
        Err(StorageError::Corrupt { .. })
    ));
    let plan = repair::plan(&root).unwrap();
    assert_eq!(plan.actions.len(), 2, "{plan:#?}");
    assert!(
        matches!(&plan.actions[0], RepairAction::QuarantineFile { path, .. } if path == &identity)
    );
    assert!(matches!(
        &plan.actions[1],
        RepairAction::RecreateIdentity { .. }
    ));
    apply_all(&root, &plan, "corrupt identity");
    assert!(root.join("ATTEMPTDB.corrupt").is_file());
    assert_plan_empty(&root, "corrupt identity");
    let db = open_clean(&root);
    assert_eq!(db.identity().db_id, db.manifest().db_id);
    assert_exact(&db, &acked);
}

// ---------------------------------------------------------------------------
// Torn WAL tails
// ---------------------------------------------------------------------------

#[test]
fn truncates_a_torn_wal_tail_at_the_last_good_record() {
    let (_dir, root) = temp_root();
    let acked = seeded(&root);
    let active = files_with_extension(&root.join("wal"), "wal")
        .pop()
        .unwrap();
    let scan = FrameReader::scan(&active, MAGIC_WAL).unwrap();
    assert_eq!(scan.records.len(), 10);
    let full_len = std::fs::metadata(&active).unwrap().len();
    std::fs::OpenOptions::new()
        .write(true)
        .open(&active)
        .unwrap()
        .set_len(full_len - 5)
        .unwrap();

    let plan = repair::plan(&root).unwrap();
    let cut = scan.records[9].offset;
    assert_eq!(
        plan.actions,
        vec![RepairAction::TruncateTornTail {
            path: active.clone(),
            at: cut
        }],
        "{plan:#?}"
    );
    assert_eq!(plan.problems.len(), 1, "{:#?}", plan.problems);
    assert!(
        plan.problems[0].contains(&format!("offset {cut}")),
        "{}",
        plan.problems[0]
    );
    assert!(plan.needs_confirmation());
    apply_all(&root, &plan, "torn wal");
    assert_eq!(std::fs::metadata(&active).unwrap().len(), cut);
    assert_plan_empty(&root, "torn wal");
    let db = open_clean(&root);
    assert_exact(&db, &acked[..69]);

    // A corrupt record in the middle: the cut lands after the last good
    // record before it, never earlier.
    let (_dir2, root2) = temp_root();
    let acked2 = seeded(&root2);
    let active2 = files_with_extension(&root2.join("wal"), "wal")
        .pop()
        .unwrap();
    let scan2 = FrameReader::scan(&active2, MAGIC_WAL).unwrap();
    flip_byte(&active2, scan2.records[3].offset as usize + 12 + 10);
    let plan2 = repair::plan(&root2).unwrap();
    assert_eq!(
        plan2.actions,
        vec![RepairAction::TruncateTornTail {
            path: active2.clone(),
            at: scan2.records[3].offset
        }],
        "{plan2:#?}"
    );
    apply_all(&root2, &plan2, "corrupt middle record");
    assert_eq!(
        std::fs::metadata(&active2).unwrap().len(),
        scan2.records[3].offset
    );
    let db2 = open_clean(&root2);
    assert_exact(&db2, &acked2[..63]);
}

#[test]
fn truncates_a_torn_spool_tail_and_keeps_the_good_prefix() {
    let (_dir, root) = temp_root();
    let acked = seeded(&root);
    let device = acked[0].device_id;
    let spooled = make_events(device, 4, "spooled");
    let inbox = SpoolWriter::new(&root).unwrap().append(&spooled).unwrap();
    let scan = FrameReader::scan(&inbox, MAGIC_SPOOL).unwrap();
    assert_eq!(scan.records.len(), 4);
    let full_len = std::fs::metadata(&inbox).unwrap().len();
    std::fs::OpenOptions::new()
        .write(true)
        .open(&inbox)
        .unwrap()
        .set_len(full_len - 3)
        .unwrap();
    let plan = repair::plan(&root).unwrap();
    let cut = scan.records[3].offset;
    assert_eq!(
        plan.actions,
        vec![RepairAction::TruncateTornTail {
            path: inbox.clone(),
            at: cut
        }],
        "{plan:#?}"
    );
    apply_all(&root, &plan, "torn spool");
    assert_eq!(std::fs::metadata(&inbox).unwrap().len(), cut);
    assert_plan_empty(&root, "torn spool");
    let mut db = open_clean(&root);
    let r = db.import_spool().unwrap();
    assert_eq!(r.accepted, 3);
    assert!(
        db.warnings.is_empty(),
        "the tail was already clean: {:?}",
        db.warnings
    );
}

// ---------------------------------------------------------------------------
// (8) Snapshot restore
// ---------------------------------------------------------------------------

#[test]
fn restore_into_an_empty_dir_and_replace_an_existing_database() {
    let (_dir, root) = temp_root();
    let mut db = Database::open(&root, writer_options()).unwrap();
    let device = db.device_id();
    let db_id = db.identity().db_id;
    db.ingest(make_events(device, 30, "snap")).unwrap();
    db.flush().unwrap();
    let snapshot_events = all_events(&db);
    let out = _dir.path().join("x.atdb");
    snapshot::export(&db, &out).unwrap();
    drop(db);

    // Into a directory that does not exist yet.
    let fresh = _dir.path().join("restored.attemptdb");
    let report = snapshot::restore(&out, &fresh, RestoreMode::IntoEmptyDir).unwrap();
    assert_eq!(
        (report.events, report.segments, report.backup.clone()),
        (30, 1, None)
    );
    let restored = open_clean(&fresh);
    assert_exact(&restored, &snapshot_events);
    assert_eq!(
        restored.identity().db_id,
        db_id,
        "a restored copy is the same logical database"
    );
    assert!(fresh.join("wal").is_dir() && fresh.join("spool").is_dir());
    drop(restored);

    // Into an existing empty directory works; into a non-empty one does not.
    let empty = _dir.path().join("empty");
    std::fs::create_dir_all(&empty).unwrap();
    snapshot::restore(&out, &empty, RestoreMode::IntoEmptyDir).unwrap();
    assert!(Database::exists(&empty));
    assert!(matches!(
        snapshot::restore(&out, &fresh, RestoreMode::IntoEmptyDir),
        Err(StorageError::Other(_))
    ));

    // The original moves on, then is replaced by the snapshot with a backup.
    let mut db = Database::open(
        &root,
        OpenOptions {
            flush_events: usize::MAX,
            flush_bytes: usize::MAX,
            ..Default::default()
        },
    )
    .unwrap();
    db.ingest(make_events(device, 5, "later")).unwrap();
    db.flush().unwrap();
    let later_events = all_events(&db);
    assert_eq!(later_events.len(), 35);
    drop(db);
    let backup = _dir.path().join("db.attemptdb.bak");
    let report = snapshot::restore(
        &out,
        &root,
        RestoreMode::ReplaceExisting {
            backup_to: backup.clone(),
        },
    )
    .unwrap();
    assert_eq!(report.backup.as_deref(), Some(backup.as_path()));
    assert_eq!(report.events, 30);
    let replaced = open_clean(&root);
    assert_exact(&replaced, &snapshot_events);
    drop(replaced);
    let old = open_clean(&backup);
    assert_exact(&old, &later_events);
    drop(old);
    // No staging directory is left behind.
    assert!(!_dir.path().read_dir().unwrap().any(|e| {
        e.unwrap()
            .file_name()
            .to_string_lossy()
            .contains(".restore-")
    }));
    // An existing backup path is refused before anything is touched.
    assert!(matches!(
        snapshot::restore(
            &out,
            &root,
            RestoreMode::ReplaceExisting {
                backup_to: backup.clone()
            }
        ),
        Err(StorageError::Other(_))
    ));
    assert!(Database::exists(&root));
}

#[test]
fn restore_refuses_a_damaged_snapshot_before_touching_anything() {
    let (_dir, root) = temp_root();
    let mut db = Database::open(&root, writer_options()).unwrap();
    let device = db.device_id();
    db.ingest(make_events(device, 5, "a")).unwrap();
    db.flush().unwrap();
    let out = _dir.path().join("x.atdb");
    snapshot::export(&db, &out).unwrap();
    let acked = all_events(&db);
    drop(db);
    let mut bytes = std::fs::read(&out).unwrap();
    let mid = bytes.len() / 2;
    bytes[mid] ^= 0xff;
    std::fs::write(&out, bytes).unwrap();
    let backup = _dir.path().join("bak");
    assert!(matches!(
        snapshot::restore(
            &out,
            &root,
            RestoreMode::ReplaceExisting {
                backup_to: backup.clone()
            }
        ),
        Err(StorageError::Corrupt {
            what: "snapshot",
            ..
        })
    ));
    assert!(!backup.exists());
    let db = open_clean(&root);
    assert_exact(&db, &acked);
}

// ---------------------------------------------------------------------------
// (9) Locking
// ---------------------------------------------------------------------------

#[test]
fn apply_and_restore_return_locked_while_a_writer_is_open() {
    let (_dir, root) = temp_root();
    seeded(&root);
    let out = _dir.path().join("x.atdb");
    {
        let mut db = Database::open(&root, OpenOptions::default()).unwrap();
        db.flush().unwrap();
        snapshot::export(&db, &out).unwrap();
    }
    let writer = Database::open(
        &root,
        OpenOptions {
            flush_events: usize::MAX,
            flush_bytes: usize::MAX,
            ..Default::default()
        },
    )
    .unwrap();
    // Something for the plan to do, so `apply` would actually write (the
    // writer cleans temp files up on open, so this comes after it opened).
    std::fs::write(root.join("segments").join("stale.arrow.tmp"), b"x").unwrap();
    let plan = repair::plan(&root).unwrap();
    assert!(!plan.actions.is_empty());
    assert!(matches!(
        repair::apply(&root, &plan),
        Err(StorageError::Locked(_))
    ));
    assert!(matches!(
        snapshot::restore(
            &out,
            &root,
            RestoreMode::ReplaceExisting {
                backup_to: _dir.path().join("bak")
            }
        ),
        Err(StorageError::Locked(_))
    ));
    assert!(
        root.join("segments").join("stale.arrow.tmp").exists(),
        "nothing was touched"
    );
    drop(writer);
    apply_all(&root, &plan, "after the writer closed");
    assert_plan_empty(&root, "after the writer closed");
}

// ---------------------------------------------------------------------------
// Stale plans
// ---------------------------------------------------------------------------

#[test]
fn a_stale_plan_is_skipped_rather_than_executed() {
    let (_dir, root) = temp_root();
    let acked = seeded(&root);
    let tmp = root.join("segments").join("stale.arrow.tmp");
    std::fs::write(&tmp, b"x").unwrap();
    let plan = repair::plan(&root).unwrap();
    assert_eq!(plan.actions.len(), 1);
    // The writer cleans the temp file up on open; the plan is now stale.
    drop(Database::open(&root, OpenOptions::default()).unwrap());
    assert!(!tmp.exists());
    let report = repair::apply(&root, &plan).unwrap();
    assert!(report.applied.is_empty());
    assert_eq!(report.skipped.len(), 1);
    assert!(
        report.skipped[0].1.contains("no longer applicable"),
        "{}",
        report.skipped[0].1
    );
    // A plan that omits an action the new generation depends on writes nothing.
    let newest = manifest_files(&root).pop().unwrap();
    flip_byte(&newest, 60);
    let full = repair::plan(&root).unwrap();
    assert!(!adoptions(&full).is_empty());
    let partial = RepairPlan {
        actions: quarantines(&full)
            .iter()
            .map(|(p, r)| RepairAction::QuarantineFile {
                path: (*p).clone(),
                reason: (*r).clone(),
            })
            .collect(),
        problems: Vec::new(),
    };
    let report = repair::apply(&root, &partial).unwrap();
    assert_eq!(
        report.new_generation, None,
        "no generation written from a partial plan"
    );
    assert_eq!(
        report.applied.len(),
        1,
        "the corrupt generation is quarantined regardless: {report:#?}"
    );
    assert_eq!(manifest_files(&root).len(), 3);
    let db = Database::open(
        &root,
        OpenOptions {
            read_only: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(db.manifest().generation, 3, "still the previous generation");
    drop(db);
    // The full plan (re-made, since the quarantine already happened) adopts.
    let full = repair::plan(&root).unwrap();
    assert_eq!(adoptions(&full).len(), 1, "{full:#?}");
    apply_all(&root, &full, "full plan");
    assert_plan_empty(&root, "full plan");
    let db = open_clean(&root);
    assert_exact(&db, &acked);
}
