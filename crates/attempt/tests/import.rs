//! `attempt import codex` and `attempt import claude-transcripts` end to end:
//! rollouts from a fixture tree into a temporary database, boundable and
//! idempotent, and — when another process holds the database's writer lock,
//! as the daemon does — queued in the spool instead of failing.
//!
//! The commands run under a temporary HOME with every agent directory
//! variable removed or pointed into it: an import reads `~/.codex` and
//! `~/.claude` by default, and a test must never see the developer's own.

use attemptdb_core::DeviceId;
use attemptdb_storage::{Database, OpenOptions, ScanFilter};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

struct Machine {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    data: PathBuf,
    db: PathBuf,
}

fn machine() -> Machine {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let data = tmp.path().join("data");
    let db = tmp.path().join("db");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&data).unwrap();
    Database::create(&db, DeviceId::derive(&["import-cli-tests"])).unwrap();
    Machine {
        _tmp: tmp,
        home,
        data,
        db,
    }
}

fn attempt(m: &Machine, args: &[&str]) -> (bool, String, String) {
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
        .env("CODEX_HOME", m.home.join(".codex"))
        .env("ATTEMPTDB_KEYRING", "off")
        .env("ATTEMPTDB_NO_DAEMON", "1")
        .env_remove("ATTEMPTDB_KEY_FILE")
        .env_remove("ATTEMPTDB_DIR")
        .env_remove("CLAUDE_CONFIG_DIR")
        .env_remove("CURSOR_CONFIG_DIR")
        .env_remove("GEMINI_CONFIG_DIR")
        .output()
        .expect("run attempt");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

fn json(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or_else(|e| panic!("not JSON ({e}):\n{text}"))
}

fn fixture(rel: &str) -> Vec<u8> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/transcripts")
        .join(rel);
    fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// A tree shaped like `~/.codex/sessions` under `m.home`: two rollouts, the
/// older one with an mtime 40 days back. Written, not copied, so the newer
/// one's mtime is "now" whatever the checkout did to the fixture.
fn codex_tree(m: &Machine) -> PathBuf {
    let day = m.home.join(".codex/sessions/2026/08/28");
    fs::create_dir_all(&day).unwrap();
    let new = day.join("rollout-2026-08-28T09-00-00-33333333-3333-4333-8333-333333333333.jsonl");
    let old = day.join("rollout-2026-08-28T08-00-00-22222222-2222-4222-8222-222222222222.jsonl");
    fs::write(&new, fixture("codex/classic_turn.jsonl")).unwrap();
    fs::write(&old, fixture("codex/modern_turn.jsonl")).unwrap();
    let forty_days = std::time::Duration::from_secs(40 * 86_400);
    fs::File::options()
        .write(true)
        .open(&old)
        .unwrap()
        .set_modified(std::time::SystemTime::now() - forty_days)
        .unwrap();
    m.home.join(".codex/sessions")
}

fn event_count(db: &Path) -> usize {
    let db = Database::open(
        db,
        OpenOptions {
            read_only: true,
            ..Default::default()
        },
    )
    .unwrap();
    db.scan(&ScanFilter::default()).unwrap().len()
}

#[test]
fn import_codex_stores_the_rollouts_and_a_second_run_stores_nothing() {
    let m = machine();
    let sessions = codex_tree(&m);
    let sessions = sessions.to_str().unwrap();

    let (ok, out, err) = attempt(&m, &["--json", "import", "codex", "--path", sessions]);
    assert!(ok, "{out}{err}");
    let v = json(&out);
    assert_eq!(v["plan"]["files"], 2, "{v:#}");
    assert_eq!(v["plan"]["sessions"], 2);
    let s = &v["summary"];
    assert_eq!(s["files"], 2);
    assert_eq!(s["files_failed"], 0);
    assert_eq!(s["sessions"], 2);
    assert_eq!(s["queued"], 0);
    assert_eq!(s["duplicates"], 0);
    assert_eq!(s["accepted"], s["events_seen"]);
    assert!(s["accepted"].as_u64().unwrap() > 50, "{v:#}");
    assert_eq!(v["queued_for_daemon"], false);
    let stored = event_count(&m.db);
    assert_eq!(stored as u64, s["accepted"].as_u64().unwrap());

    let (ok, out, err) = attempt(&m, &["--json", "import", "codex", "--path", sessions]);
    assert!(ok, "{out}{err}");
    let v = json(&out);
    assert_eq!(v["summary"]["accepted"], 0, "{v:#}");
    assert_eq!(v["summary"]["duplicates"], s["accepted"]);
    assert_eq!(event_count(&m.db), stored, "re-import is a no-op");

    // The text form says what happened in words.
    let (ok, out, err) = attempt(&m, &["import", "codex", "--path", sessions]);
    assert!(ok, "{out}{err}");
    assert!(
        out.contains("imported 0 new event(s) from 2 file(s)"),
        "{out}"
    );
    assert!(out.contains("reconstructed from Codex rollouts"), "{out}");
}

#[test]
fn import_codex_dry_run_writes_nothing_and_the_bounds_choose_files() {
    let m = machine();
    let sessions = codex_tree(&m);
    let sessions = sessions.to_str().unwrap();

    let (ok, out, err) = attempt(
        &m,
        &["--json", "import", "codex", "--path", sessions, "--dry-run"],
    );
    assert!(ok, "{out}{err}");
    let v = json(&out);
    assert_eq!(v["plan"]["files"], 2);
    assert!(v["summary"].is_null());
    assert_eq!(event_count(&m.db), 0, "a dry run writes nothing");

    // --days 30 leaves out the rollout last touched 40 days ago.
    let (ok, out, err) = attempt(
        &m,
        &[
            "--json",
            "import",
            "codex",
            "--path",
            sessions,
            "--days",
            "30",
            "--dry-run",
        ],
    );
    assert!(ok, "{out}{err}");
    let v = json(&out);
    assert_eq!(v["plan"]["files"], 1, "{v:#}");
    assert_eq!(v["plan"]["skipped_old"], 1);

    // --max-bytes keeps the newest file that fits.
    let (ok, out, err) = attempt(
        &m,
        &[
            "--json",
            "import",
            "codex",
            "--path",
            sessions,
            "--max-bytes",
            "10K",
            "--dry-run",
        ],
    );
    assert!(ok, "{out}{err}");
    let v = json(&out);
    assert_eq!(v["plan"]["files"], 1, "{v:#}");
    assert_eq!(v["plan"]["skipped_over_budget"], 1);
    assert_eq!(v["plan"]["max_bytes"], 10 * 1024);

    // The default source is CODEX_HOME's sessions directory.
    let (ok, out, err) = attempt(&m, &["--json", "import", "codex", "--dry-run"]);
    assert!(ok, "{out}{err}");
    let v = json(&out);
    assert_eq!(v["plan"]["files"], 2, "{v:#}");
    assert!(
        v["plan"]["searched"][0]
            .as_str()
            .unwrap()
            .ends_with(".codex/sessions"),
        "{v:#}"
    );

    // A day-bounded real run stores only what the plan named.
    let (ok, out, err) = attempt(
        &m,
        &[
            "--json", "import", "codex", "--path", sessions, "--days", "30",
        ],
    );
    assert!(ok, "{out}{err}");
    let one_file = event_count(&m.db);
    assert!(one_file > 20, "{one_file}");
}

#[test]
fn bad_import_arguments_are_refused_with_a_reason() {
    let m = machine();
    let (ok, _, err) = attempt(&m, &["import", "codex", "--max-bytes", "banana"]);
    assert!(!ok);
    assert!(err.contains("--max-bytes"), "{err}");
    let (ok, _, err) = attempt(&m, &["import", "codex", "--since=-2d", "--days", "3"]);
    assert!(!ok, "--since and --days are alternatives");
    assert!(err.contains("cannot be used with"), "{err}");
    let (ok, _, err) = attempt(&m, &["import", "codex", "--since", "not-a-time"]);
    assert!(!ok);
    assert!(err.contains("cannot parse --since"), "{err}");
    // No sessions directory at all is not an error.
    let (ok, out, err) = attempt(&m, &["import", "codex"]);
    assert!(ok, "{out}{err}");
    assert!(out.contains("none found"), "{out}");
}

/// Another process holds the writer lock — the daemon's situation. The
/// import must not fail; it queues in the spool, and the lock holder stores
/// the events when it imports the spool.
#[test]
fn a_locked_database_gets_codex_events_through_the_spool() {
    let m = machine();
    let sessions = codex_tree(&m);
    let sessions = sessions.to_str().unwrap();
    let mut daemon = Database::open(
        &m.db,
        OpenOptions {
            create: false,
            ..Default::default()
        },
    )
    .unwrap();

    let (ok, out, err) = attempt(&m, &["--json", "import", "codex", "--path", sessions]);
    assert!(ok, "a held lock must not fail the import:\n{out}{err}");
    let v = json(&out);
    assert_eq!(v["queued_for_daemon"], true, "{v:#}");
    let s = &v["summary"];
    assert_eq!(
        (s["accepted"].as_u64(), s["duplicates"].as_u64()),
        (Some(0), Some(0))
    );
    let queued = s["queued"].as_u64().unwrap();
    assert!(queued > 50, "{v:#}");
    assert!(daemon.stats().spool_pending);
    assert_eq!(event_count_via(&daemon), 0, "nothing stored yet");

    // The daemon's next spool tick.
    let report = daemon.import_spool().unwrap();
    assert_eq!(report.accepted as u64, queued);

    // The same import again: queued again, all duplicates when drained.
    let (ok, out, err) = attempt(&m, &["--json", "import", "codex", "--path", sessions]);
    assert!(ok, "{out}{err}");
    assert_eq!(json(&out)["summary"]["queued"], queued);
    let report = daemon.import_spool().unwrap();
    assert_eq!(
        (report.accepted as u64, report.duplicates as u64),
        (0, queued)
    );

    // The text form tells the person what happened.
    let (ok, out, err) = attempt(&m, &["import", "codex", "--path", sessions]);
    assert!(ok, "{out}{err}");
    assert!(out.contains("queued"), "{out}");
    assert!(out.contains("daemon"), "{out}");
    drop(daemon);
}

#[test]
fn a_locked_database_gets_claude_transcripts_through_the_spool() {
    let m = machine();
    let dir = m.home.join(".claude/projects/-home-dev-example-project");
    fs::create_dir_all(&dir).unwrap();
    let file = dir.join("11111111-1111-4111-8111-111111111111.jsonl");
    fs::write(&file, fixture("claude_code/basic_turn.jsonl")).unwrap();
    let mut daemon = Database::open(
        &m.db,
        OpenOptions {
            create: false,
            ..Default::default()
        },
    )
    .unwrap();

    let (ok, out, err) = attempt(
        &m,
        &[
            "--json",
            "import",
            "claude-transcripts",
            file.to_str().unwrap(),
        ],
    );
    assert!(ok, "a held lock must not fail the import:\n{out}{err}");
    let v = json(&out);
    assert_eq!(v["queued_for_daemon"], true, "{v:#}");
    assert_eq!(v["summary"]["queued"], 12);
    assert_eq!(v["summary"]["accepted"], 0);
    let report = daemon.import_spool().unwrap();
    assert_eq!(report.accepted, 12);
    drop(daemon);

    // With the lock free the same import writes directly: all duplicates.
    let (ok, out, err) = attempt(
        &m,
        &[
            "--json",
            "import",
            "claude-transcripts",
            file.to_str().unwrap(),
        ],
    );
    assert!(ok, "{out}{err}");
    let v = json(&out);
    assert_eq!(v["queued_for_daemon"], false);
    assert_eq!(
        (
            v["summary"]["accepted"].as_u64(),
            v["summary"]["duplicates"].as_u64()
        ),
        (Some(0), Some(12))
    );
}

fn event_count_via(db: &Database) -> usize {
    db.scan(&ScanFilter::default()).unwrap().len()
}
