//! The CLI's read surfaces and the commands that write corrections, end to
//! end through the real binary: a database seeded the way a hook seeds it
//! (adapter events appended to the spool, imported by the first command that
//! opens the database, so the real ingest allowlist applies), everything run
//! under a temporary HOME with no agent configuration variables and no OS key
//! store.

use attemptdb_adapters::{CaptureContext, adapter_for};
use attemptdb_core::event::Provider;
use attemptdb_core::{CaptureMode, DeviceId, Event, ProjectRef, Timestamp};
use attemptdb_storage::{Database, SpoolWriter};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Variables that point an agent (and so `attempt`) at its real, per-user
/// configuration. A temporary HOME does not protect against them.
const AGENT_ENV: [&str; 4] = [
    "CLAUDE_CONFIG_DIR",
    "CODEX_HOME",
    "CURSOR_CONFIG_DIR",
    "GEMINI_CONFIG_DIR",
];

fn bare_path() -> String {
    if cfg!(windows) {
        let root = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".into());
        format!("{root}\\System32")
    } else {
        "/usr/bin:/bin".into()
    }
}

const PROJECT_ROOT: &str = "/home/dev/example/project";
const PROJECT_REMOTE: &str = "git@github.com:example/project.git";

struct Out {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

impl Out {
    fn ok(&self) -> bool {
        self.code == Some(0)
    }

    fn all(&self) -> String {
        format!("{}{}", self.stdout, self.stderr)
    }

    fn json(&self) -> Value {
        serde_json::from_str(&self.stdout)
            .unwrap_or_else(|e| panic!("not JSON ({e}): {}", self.all()))
    }
}

struct Machine {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    data: PathBuf,
    /// A directory that is not a repository.
    work: PathBuf,
    device: DeviceId,
}

impl Machine {
    fn new() -> Self {
        // Short prefix: the daemon's socket path must fit sun_path.
        let tmp = tempfile::Builder::new().prefix("atdb").tempdir().unwrap();
        let home = tmp.path().join("home");
        let work = tmp.path().join("work");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&work).unwrap();
        Machine {
            data: tmp.path().join("data"),
            home,
            work,
            device: DeviceId::derive(&["read-surfaces-test"]),
            _tmp: tmp,
        }
    }

    fn db_dir(&self) -> PathBuf {
        self.data.join("db").join(".attemptdb")
    }

    /// Everything a child process of these tests inherits is decided here:
    /// a temporary HOME, a PATH with no agent launchers, no agent config
    /// variables, no OS key store, no daemon (unless a test asks for one).
    fn isolate(&self, cmd: &mut Command, daemon: bool) {
        cmd.env("PATH", bare_path())
            .env("HOME", &self.home)
            .env("USERPROFILE", &self.home)
            .env("ATTEMPTDB_KEYRING", "off");
        if daemon {
            cmd.env_remove("ATTEMPTDB_NO_DAEMON");
        } else {
            cmd.env("ATTEMPTDB_NO_DAEMON", "1");
        }
        for var in AGENT_ENV {
            cmd.env_remove(var);
        }
        for var in [
            "ATTEMPTDB_KEY_FILE",
            "ATTEMPTDB_PASSPHRASE",
            "ATTEMPTDB_DIR",
            "ATTEMPTDB_DATA_DIR",
        ] {
            cmd.env_remove(var);
        }
    }

    fn run(&self, cwd: &Path, args: &[&str], daemon: bool) -> Out {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_attempt"));
        cmd.arg("--data-dir")
            .arg(&self.data)
            .args(args)
            .current_dir(cwd)
            .stdin(Stdio::null());
        self.isolate(&mut cmd, daemon);
        let out = cmd.output().expect("run attempt");
        Out {
            code: out.status.code(),
            stdout: String::from_utf8_lossy(&out.stdout).to_string(),
            stderr: String::from_utf8_lossy(&out.stderr).to_string(),
        }
    }

    fn attempt(&self, args: &[&str]) -> Out {
        self.run(&self.work, args, false)
    }

    /// One hook event as the adapter normalises it.
    fn hook_event(&self, root: &str, at: &str, payload: Value) -> Event {
        let ctx = CaptureContext {
            device_id: self.device,
            capture_mode: CaptureMode::LocalSemantic,
            project: ProjectRef::derive(root, Some(PROJECT_REMOTE).filter(|_| root == PROJECT_ROOT), &self.device),
            captured_at: Timestamp::parse(at).unwrap(),
            provider_version: None,
            hook_version: Some(env!("CARGO_PKG_VERSION").into()),
        };
        adapter_for(&Provider::ClaudeCode)
            .unwrap()
            .normalise(&ctx, None, &payload)
            .unwrap()
    }

    /// Append events to the spool, the way hooks do; the next command that
    /// opens the database imports them through the real ingest.
    fn spool(&self, events: &[Event]) {
        let dir = self.db_dir();
        if !Database::exists(&dir) {
            std::fs::create_dir_all(dir.parent().unwrap()).unwrap();
            Database::create(&dir, self.device).unwrap();
        }
        SpoolWriter::new(&dir)
            .unwrap()
            .append_with(events, false)
            .unwrap();
    }

    /// One session in `root`: a prompt, an edit that fails, an edit that
    /// works, a stop. `start` is an RFC 3339 instant; every event follows it
    /// by a few seconds.
    fn session(&self, root: &str, id: &str, start: &str, end_it: bool) -> Vec<Event> {
        let t0 = Timestamp::parse(start).unwrap().as_micros();
        let at = |secs: i64| Timestamp::from_micros(t0 + secs * 1_000_000).to_rfc3339();
        let path = format!("{root}/src/lib.rs");
        let mut v = vec![
            self.hook_event(
                root,
                &at(0),
                json!({"hook_event_name": "SessionStart", "session_id": id, "cwd": root, "source": "startup"}),
            ),
            self.hook_event(
                root,
                &at(1),
                json!({"hook_event_name": "UserPromptSubmit", "session_id": id, "cwd": root, "prompt": "fix the parser"}),
            ),
            self.hook_event(
                root,
                &at(2),
                json!({"hook_event_name": "PreToolUse", "session_id": id, "tool_name": "Edit", "tool_use_id": format!("{id}-t1"), "tool_input": {"file_path": path}}),
            ),
            self.hook_event(
                root,
                &at(3),
                json!({"hook_event_name": "PostToolUseFailure", "session_id": id, "tool_name": "Edit", "tool_use_id": format!("{id}-t1"), "tool_input": {"file_path": path}, "error": "String to replace not found in file."}),
            ),
            self.hook_event(
                root,
                &at(4),
                json!({"hook_event_name": "PreToolUse", "session_id": id, "tool_name": "Edit", "tool_use_id": format!("{id}-t2"), "tool_input": {"file_path": path}}),
            ),
            self.hook_event(
                root,
                &at(5),
                json!({"hook_event_name": "PostToolUse", "session_id": id, "tool_name": "Edit", "tool_use_id": format!("{id}-t2"), "tool_input": {"file_path": path}, "tool_response": {"success": true}}),
            ),
            self.hook_event(
                root,
                &at(6),
                json!({"hook_event_name": "Stop", "session_id": id, "stop_hook_active": false}),
            ),
        ];
        if end_it {
            v.push(self.hook_event(
                root,
                &at(7),
                json!({"hook_event_name": "SessionEnd", "session_id": id, "cwd": root, "reason": "other"}),
            ));
        }
        v
    }

    /// `SELECT` through the CLI, as rows.
    fn rows(&self, sql: &str) -> Vec<Value> {
        let out = self.attempt(&["--json", "query", "--all-projects", sql]);
        assert!(out.ok(), "{sql}: {}", out.all());
        out.json().as_array().cloned().unwrap_or_default()
    }

    /// The `att_` id of the first failed attempt.
    fn failed_attempt(&self) -> String {
        let rows = self.rows(
            "SELECT attempt_id, outcome FROM attempts WHERE outcome IN ('failed', 'superseded') ORDER BY started_at LIMIT 1",
        );
        rows.first()
            .and_then(|r| r["attempt_id"].as_str())
            .unwrap_or_else(|| panic!("no failed attempt among {rows:?}"))
            .to_string()
    }
}

/// The harness is the only thing between these tests and the owner's real
/// `~/.claude*`: prove it hides what an inheriting shell would pass.
#[cfg(unix)]
#[test]
fn the_harness_hides_the_owners_agent_config_from_children() {
    let m = Machine::new();
    let mut cmd = Command::new("/usr/bin/env");
    for var in AGENT_ENV {
        cmd.env(var, "/owner/real/agent/config");
    }
    cmd.env("ATTEMPTDB_DIR", "/owner/real/db");
    m.isolate(&mut cmd, false);
    let out = cmd.output().unwrap();
    let env = String::from_utf8_lossy(&out.stdout);
    for var in AGENT_ENV.into_iter().chain(["ATTEMPTDB_DIR"]) {
        assert!(
            !env.lines().any(|l| l.starts_with(&format!("{var}="))),
            "{var} reached the child:\n{env}"
        );
    }
    assert!(env.contains(&format!("HOME={}", m.home.display())), "{env}");
    assert!(env.contains("ATTEMPTDB_KEYRING=off"), "{env}");
    assert!(env.contains("ATTEMPTDB_NO_DAEMON=1"), "{env}");
}

#[cfg(unix)]
struct Daemon(Child);

#[cfg(unix)]
impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[cfg(unix)]
fn start_daemon(m: &Machine) -> Daemon {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_attempt"));
    cmd.arg("--data-dir")
        .arg(&m.data)
        .args(["daemon", "run", "--foreground"])
        .current_dir(&m.work)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    m.isolate(&mut cmd, true);
    let child = Daemon(cmd.spawn().unwrap());
    let t = Instant::now();
    loop {
        let out = m.attempt(&["daemon", "status"]);
        if out.all().contains("running (pid") {
            return child;
        }
        assert!(
            t.elapsed() < Duration::from_secs(20),
            "the daemon did not start: {}",
            out.all()
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

// ---------------------------------------------------------------------------
// P1-1: one long statement must not kill the process
// ---------------------------------------------------------------------------

fn long_shapes(n: usize) -> Vec<(&'static str, String)> {
    let mut cte = String::from("WITH c0 AS (SELECT 1 AS x)");
    for i in 1..n {
        cte.push_str(&format!(", c{i} AS (SELECT x FROM c{})", i - 1));
    }
    cte.push_str(&format!(" SELECT * FROM c{}", n - 1));
    vec![
        ("plus", format!("SELECT 1{} AS x", " + 1".repeat(n))),
        (
            "or",
            format!(
                "SELECT count(*) FROM events WHERE kind = 'x0'{}",
                (1..n)
                    .map(|i| format!(" OR kind = 'x{i}'"))
                    .collect::<String>()
            ),
        ),
        (
            "and-like",
            format!(
                "SELECT count(*) FROM events WHERE kind LIKE 'a%'{}",
                (1..n)
                    .map(|i| format!(" AND kind LIKE '%b{i}%'"))
                    .collect::<String>()
            ),
        ),
        ("concat", format!("SELECT 'a'{} AS x", " || 'b'".repeat(n))),
        (
            "union-all",
            format!("SELECT 1 AS x{}", " UNION ALL SELECT 1".repeat(n)),
        ),
        ("cte-chain", cte),
    ]
}

#[test]
fn a_long_statement_is_an_error_on_the_cli_not_a_crash() {
    let m = Machine::new();
    m.spool(&m.session(PROJECT_ROOT, "long-1", "2026-08-20T09:00:00Z", true));
    for (name, sql) in long_shapes(1000) {
        let out = m.attempt(&["query", "--all-projects", &sql]);
        assert_eq!(out.code, Some(1), "{name}: a clean failure, not a signal: {}", out.all());
        assert!(
            out.stderr.contains("too complex"),
            "{name}: {}",
            &out.stderr[..out.stderr.len().min(400)]
        );
    }
    let out = m.attempt(&["query", "--all-projects", "SELECT count(*) AS n FROM events"]);
    assert!(out.ok(), "{}", out.all());
}

#[cfg(unix)]
#[test]
fn a_long_statement_does_not_take_the_daemon_down() {
    let m = Machine::new();
    m.spool(&m.session(PROJECT_ROOT, "long-2", "2026-08-20T09:00:00Z", true));
    // Let the CLI import the spool, then hand the database to a daemon.
    assert!(m.attempt(&["status"]).ok());
    let daemon = start_daemon(&m);
    for (name, sql) in long_shapes(1000) {
        let out = m.run(&m.work, &["query", "--all-projects", &sql], true);
        assert_eq!(out.code, Some(1), "{name}: {}", out.all());
        assert!(out.stderr.contains("too complex"), "{name}: {}", out.stderr);
    }
    // The daemon is still up and still answers.
    let out = m.run(
        &m.work,
        &["--json", "query", "--all-projects", "SELECT count(*) AS n FROM events"],
        true,
    );
    assert!(out.ok(), "{}", out.all());
    assert!(m.attempt(&["daemon", "status"]).all().contains("running (pid"));
    drop(daemon);
}

// ---------------------------------------------------------------------------
// P1-2 / P1-3: `attempt correct` and `attempt retract` really write, and the
// log holds what the preview promised
// ---------------------------------------------------------------------------

#[test]
fn correct_writes_the_outcome_and_the_failure_class_without_a_daemon() {
    let m = Machine::new();
    m.spool(&m.session(PROJECT_ROOT, "corr-1", "2026-08-20T09:00:00Z", true));
    let att = m.failed_attempt();
    let out = m.attempt(&[
        "correct",
        &att,
        "--outcome",
        "failed",
        "--failure-class",
        "wrong_fix",
    ]);
    assert!(out.ok(), "{}", out.all());
    assert!(out.stdout.contains("wrote correction"), "{}", out.stdout);
    assert!(
        out.stdout.contains("wrong_fix"),
        "the preview names the class it applies: {}",
        out.stdout
    );
    // The stored correction carries the class, and the projection shows it.
    let rows = m.rows(&format!(
        "SELECT failure_class, outcome FROM corrections WHERE target = '{att}'"
    ));
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["failure_class"], "wrong_fix", "{rows:?}");
    let rows = m.rows(&format!(
        "SELECT outcome, failure_class FROM attempts WHERE attempt_id = '{att}'"
    ));
    assert_eq!(rows[0]["outcome"], "failed", "{rows:?}");
    assert_eq!(rows[0]["failure_class"], "wrong_fix", "{rows:?}");
    // The log itself: the correction event kept the key, and dropped nothing.
    let rows = m.rows(
        "SELECT attrs_json FROM events WHERE kind = 'correction'",
    );
    let attrs = rows[0]["attrs_json"].as_str().unwrap();
    assert!(attrs.contains("\"failure_class\""), "{attrs}");
    assert!(!attrs.contains("\"redactions\""), "{attrs}");
}

#[test]
fn a_failure_class_that_is_prose_is_refused_before_anything_is_written() {
    let m = Machine::new();
    m.spool(&m.session(PROJECT_ROOT, "corr-2", "2026-08-20T09:00:00Z", true));
    let att = m.failed_attempt();
    let out = m.attempt(&[
        "correct",
        &att,
        "--outcome",
        "failed",
        "--failure-class",
        "it broke because the parser trimmed the string; see /Users/me/notes",
    ]);
    assert_eq!(out.code, Some(1), "{}", out.all());
    assert!(out.stderr.contains("not a class name"), "{}", out.stderr);
    assert_eq!(
        m.rows("SELECT event_id FROM events WHERE kind = 'correction'").len(),
        0
    );
}

#[test]
fn retract_writes_without_a_daemon_and_names_ids_once() {
    let m = Machine::new();
    m.spool(&m.session(PROJECT_ROOT, "retr-1", "2026-08-20T09:00:00Z", true));
    let att = m.failed_attempt();
    let session = m.rows("SELECT session_id FROM sessions")[0]["session_id"]
        .as_str()
        .unwrap()
        .to_string();

    let dry = m.attempt(&["retract", "--attempt", &att, "--reason", "test", "--dry-run"]);
    assert!(dry.ok(), "{}", dry.all());
    assert!(dry.stdout.contains("(dry run"), "{}", dry.stdout);
    assert!(
        !dry.stdout.contains("att_att_") && !dry.stdout.contains("ses_ses_"),
        "a doubled id prefix: {}",
        dry.stdout
    );
    let dry = m.attempt(&["retract", "--session", &session, "--reason", "test", "--dry-run"]);
    assert!(dry.ok(), "{}", dry.all());
    assert!(!dry.stdout.contains("ses_ses_"), "{}", dry.stdout);

    let out = m.attempt(&["retract", "--session", &session, "--reason", "privacy", "--yes"]);
    assert!(out.ok(), "{}", out.all());
    assert!(out.stdout.contains("wrote retraction"), "{}", out.stdout);
    let rows = m.rows("SELECT reason, matched FROM retractions");
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["reason"], "privacy", "{rows:?}");
    // The retraction took effect: the session's events are flagged.
    let rows = m.rows("SELECT count(*) AS n FROM events WHERE retracted = false AND kind = 'prompt_submitted'");
    assert_eq!(rows[0]["n"], 0, "{rows:?}");
}

#[cfg(unix)]
#[test]
fn correct_and_retract_write_through_a_running_daemon() {
    let m = Machine::new();
    m.spool(&m.session(PROJECT_ROOT, "daemon-1", "2026-08-20T09:00:00Z", true));
    m.spool(&m.session(PROJECT_ROOT, "daemon-2", "2026-08-20T10:00:00Z", true));
    assert!(m.attempt(&["status"]).ok());
    let att = m.failed_attempt();
    let daemon = start_daemon(&m);
    let out = m.run(
        &m.work,
        &["correct", &att, "--outcome", "failed", "--failure-class", "wrong_fix"],
        true,
    );
    assert!(out.ok(), "{}", out.all());
    let sessions = m.rows("SELECT session_id FROM sessions ORDER BY started_at");
    let second = sessions[1]["session_id"].as_str().unwrap().to_string();
    let out = m.run(
        &m.work,
        &["retract", "--session", &second, "--reason", "test", "--yes"],
        true,
    );
    assert!(out.ok(), "{}", out.all());
    let out = m.run(
        &m.work,
        &[
            "--json",
            "query",
            "--all-projects",
            "SELECT failure_class FROM corrections",
        ],
        true,
    );
    assert!(out.ok(), "{}", out.all());
    assert_eq!(out.json()[0]["failure_class"], "wrong_fix", "{}", out.all());
    drop(daemon);
}
