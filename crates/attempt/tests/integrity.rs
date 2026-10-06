//! A database whose newest manifest generation cannot be used still opens and
//! answers queries, from an older generation: `verify` and `doctor` have to
//! say so, and exit non-zero, instead of reporting `ok`.
//!
//! The commands run under a temporary HOME with every agent directory
//! variable removed, like the other CLI tests.

use attemptdb_core::event::Provider;
use attemptdb_core::{CaptureMode, DeviceId, Event, EventKind, ProjectRef};
use attemptdb_storage::{Database, OpenOptions};
use serde_json::Value;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

struct Machine {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    data: PathBuf,
    db: PathBuf,
}

/// A database with three flushed segments (generations 2, 3 and 4), twenty
/// events each.
fn machine() -> Machine {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let data = tmp.path().join("data");
    let db_dir = tmp.path().join("db");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&data).unwrap();
    let device = DeviceId::derive(&["integrity-cli-tests"]);
    Database::create(&db_dir, device).unwrap();
    let mut db = Database::open(
        &db_dir,
        OpenOptions {
            create: false,
            flush_events: usize::MAX,
            flush_bytes: usize::MAX,
            ..Default::default()
        },
    )
    .unwrap();
    for batch in 0..3 {
        let events: Vec<Event> = (0..20)
            .map(|i| {
                let mut ev = Event::new(
                    device,
                    Provider::ClaudeCode,
                    "PostToolUse",
                    EventKind::ToolCallFinished,
                    ProjectRef::derive("/home/dev/example/project", None, &device),
                    format!("session-{batch}"),
                    CaptureMode::LocalSemantic,
                    "integrity-test/0.1",
                );
                ev.attrs
                    .insert("turn_index_hint".into(), serde_json::json!(i));
                ev
            })
            .collect();
        db.ingest(events).unwrap();
        db.flush().unwrap();
    }
    assert_eq!(db.manifest().generation, 4);
    drop(db);
    Machine {
        _tmp: tmp,
        home,
        data,
        db: db_dir,
    }
}

fn attempt(m: &Machine, args: &[&str]) -> (Option<i32>, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_attempt"))
        .arg("--data-dir")
        .arg(&m.data)
        .arg("--db")
        .arg(&m.db)
        .args(args)
        .current_dir(&m.home)
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", &m.home)
        .env("USERPROFILE", &m.home)
        .env("ATTEMPTDB_KEYRING", "off")
        .env("ATTEMPTDB_NO_DAEMON", "1")
        .env_remove("ATTEMPTDB_KEY_FILE")
        .env_remove("ATTEMPTDB_DIR")
        .env_remove("CLAUDE_CONFIG_DIR")
        .env_remove("CODEX_HOME")
        .env_remove("CURSOR_CONFIG_DIR")
        .env_remove("GEMINI_CONFIG_DIR")
        .output()
        .expect("run attempt");
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

/// The newest manifest document, readable but with a checksum that fails: the
/// newest generation is skipped on open.
fn break_the_newest_manifest(m: &Machine) {
    let mut files: Vec<PathBuf> = fs::read_dir(m.db.join("manifest"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("json"))
        .collect();
    files.sort();
    let newest = files.pop().unwrap();
    let mut doc: Value = serde_json::from_slice(&fs::read(&newest).unwrap()).unwrap();
    doc["checksum"] = serde_json::json!(7_654_321);
    fs::write(&newest, serde_json::to_vec_pretty(&doc).unwrap()).unwrap();
}

#[test]
fn verify_of_a_healthy_database_says_ok() {
    let m = machine();
    let (code, out, err) = attempt(&m, &["verify"]);
    assert_eq!(code, Some(0), "{out}{err}");
    assert!(out.starts_with("ok:"), "{out}");
}

#[test]
fn verify_exits_nonzero_and_names_a_newest_generation_that_cannot_be_used() {
    let m = machine();
    break_the_newest_manifest(&m);

    // Queries still answer, from the older generation: nothing else says so.
    let (code, out, err) = attempt(&m, &["--json", "verify"]);
    assert_eq!(code, Some(1), "{out}{err}");
    let v: Value = serde_json::from_str(&out).unwrap_or_else(|e| panic!("{e}: {out}"));
    assert_eq!(v["ok"], false, "{v:#}");
    let problems = v["problems"].as_array().unwrap();
    assert_eq!(problems.len(), 1, "{v:#}");
    let text = problems[0].as_str().unwrap();
    assert!(
        text.contains("newest manifest generation (4) cannot be used")
            && text.contains("serving generation 3")
            && text.contains("up to 20 event(s) are not visible"),
        "{text}"
    );

    let (code, out, err) = attempt(&m, &["verify"]);
    assert_eq!(code, Some(1), "{out}{err}");
    assert!(
        out.contains("problem: the newest manifest generation (4)"),
        "{out}"
    );
    assert!(!out.starts_with("ok:"), "{out}");
}

#[test]
fn doctor_reports_it_too_and_a_repair_clears_it() {
    let m = machine();
    let (_, out, err) = attempt(&m, &["doctor"]);
    assert!(!out.contains("PROBLEM"), "{out}{err}");

    break_the_newest_manifest(&m);
    let (code, out, err) = attempt(&m, &["doctor"]);
    assert_eq!(code, Some(1), "{out}{err}");
    assert!(
        out.contains("database     PROBLEM: the newest manifest generation (4)"),
        "{out}"
    );
    let (code, out, _) = attempt(&m, &["--json", "doctor"]);
    assert_eq!(code, Some(1), "{out}");
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["database_problems"].as_array().unwrap().len(), 1, "{v:#}");

    // Repair adopts the hidden segment; verify and doctor are quiet again.
    let (code, out, err) = attempt(&m, &["repair", "--apply", "--yes"]);
    assert_eq!(code, Some(0), "{out}{err}");
    let (code, out, err) = attempt(&m, &["verify"]);
    assert_eq!(code, Some(0), "{out}{err}");
    let (_, out, _) = attempt(&m, &["doctor"]);
    assert!(!out.contains("PROBLEM"), "{out}");
}
