//! `attempt compact` end to end: a dry run reports the plan and changes
//! nothing; the real run merges the small segments, the database still
//! holds every event, and the inputs are deleted by the next generation.

use attemptdb_core::event::Provider;
use attemptdb_core::{CaptureMode, DeviceId, Event, EventKind, ProjectRef};
use attemptdb_storage::{Database, OpenOptions, ScanFilter};
use std::path::Path;
use std::process::Command;

fn events(dev: DeviceId, n: usize, tag: &str) -> Vec<Event> {
    (0..n)
        .map(|i| {
            let mut ev = Event::new(
                dev,
                Provider::ClaudeCode,
                "PostToolUse",
                EventKind::ToolCallFinished,
                ProjectRef::derive("/home/dev/example/project", None, &dev),
                format!("session-{tag}"),
                CaptureMode::LocalSemantic,
                "attempt-compact-test/0.1",
            );
            ev.attrs.insert("x_test_index".into(), serde_json::json!(i));
            ev
        })
        .collect()
}

fn attempt(data_dir: &Path, db: &Path, args: &[&str]) -> (bool, serde_json::Value, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_attempt"))
        .arg("--data-dir")
        .arg(data_dir)
        .arg("--db")
        .arg(db)
        .arg("--json")
        .args(args)
        // Never touch the OS key store from a test.
        .env("ATTEMPTDB_KEYRING", "off")
        .env_remove("ATTEMPTDB_KEY_FILE")
        .env_remove("ATTEMPTDB_DIR")
        .output()
        .expect("run attempt");
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    let json = serde_json::from_str(&stdout).unwrap_or(serde_json::Value::Null);
    (out.status.success(), json, format!("{stdout}\n{stderr}"))
}

#[test]
fn dry_run_plans_and_the_real_run_merges() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = tmp.path().join("data");
    let db_dir = tmp.path().join("db.attemptdb");
    let mut db = Database::open(
        &db_dir,
        OpenOptions {
            create: true,
            flush_events: usize::MAX,
            flush_bytes: usize::MAX,
            ..Default::default()
        },
    )
    .unwrap();
    let dev = db.device_id();
    for b in 0..5 {
        db.ingest(events(dev, 12, &format!("b{b}"))).unwrap();
        db.flush().unwrap();
    }
    let before = db.scan(&ScanFilter::default()).unwrap();
    drop(db);

    // Dry run: one run of five, nothing written.
    let (ok, v, text) = attempt(
        &data_dir,
        &db_dir,
        &[
            "compact",
            "--dry-run",
            "--max-segments",
            "2",
            "--min-inputs",
            "2",
        ],
    );
    assert!(ok, "{text}");
    assert_eq!(v["dry_run"], true, "{text}");
    assert_eq!(
        v["plan"]["runs"].as_array().map(Vec::len),
        Some(1),
        "{text}"
    );
    assert_eq!(
        v["plan"]["runs"][0]["inputs"].as_array().map(Vec::len),
        Some(5)
    );
    assert_eq!(v["plan"]["segments_after"], 1);
    let ro = Database::open(
        &db_dir,
        OpenOptions {
            read_only: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(ro.manifest().segments.len(), 5, "dry run changed nothing");
    drop(ro);

    // Default policy on a small database: nothing to do, and it says so.
    let (ok, v, text) = attempt(&data_dir, &db_dir, &["compact"]);
    assert!(ok, "{text}");
    assert_eq!(v["reports"].as_array().map(Vec::len), Some(0), "{text}");
    assert_eq!(v["segments"], 5);

    // Real run.
    let (ok, v, text) = attempt(
        &data_dir,
        &db_dir,
        &["compact", "--max-segments", "2", "--min-inputs", "2"],
    );
    assert!(ok, "{text}");
    assert_eq!(v["reports"].as_array().map(Vec::len), Some(1), "{text}");
    assert_eq!(v["reports"][0]["events"], 60);
    assert_eq!(v["segments"], 1);
    assert_eq!(
        v["pending_deletions"], 5,
        "inputs wait for the next generation"
    );
    let mut db = Database::open(&db_dir, OpenOptions::default()).unwrap();
    assert_eq!(db.manifest().segments.len(), 1);
    assert_eq!(db.scan(&ScanFilter::default()).unwrap(), before);
    assert!(db.verify().unwrap().is_empty());
    db.ingest(events(dev, 1, "tail")).unwrap();
    db.flush().unwrap();
    assert_eq!(
        db.stats().tombstones,
        0,
        "the next generation deleted the inputs"
    );
    let files = std::fs::read_dir(db_dir.join("segments"))
        .unwrap()
        .flatten()
        .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("arrow"))
        .count();
    assert_eq!(files, 2);

    // A writer holding the lock is reported, not waited for.
    let (ok, _, text) = attempt(&data_dir, &db_dir, &["compact", "--max-segments", "1"]);
    assert!(!ok, "{text}");
    assert!(text.contains("locked by another writer"), "{text}");
    drop(db);
}
