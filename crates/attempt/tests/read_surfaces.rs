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
            project: ProjectRef::derive(
                root,
                Some(PROJECT_REMOTE).filter(|_| root == PROJECT_ROOT),
                &self.device,
            ),
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
        assert_eq!(
            out.code,
            Some(1),
            "{name}: a clean failure, not a signal: {}",
            out.all()
        );
        assert!(
            out.stderr.contains("too complex"),
            "{name}: {}",
            &out.stderr[..out.stderr.len().min(400)]
        );
    }
    let out = m.attempt(&[
        "query",
        "--all-projects",
        "SELECT count(*) AS n FROM events",
    ]);
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
        &[
            "--json",
            "query",
            "--all-projects",
            "SELECT count(*) AS n FROM events",
        ],
        true,
    );
    assert!(out.ok(), "{}", out.all());
    assert!(
        m.attempt(&["daemon", "status"])
            .all()
            .contains("running (pid")
    );
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
    let rows = m.rows("SELECT attrs_json FROM events WHERE kind = 'correction'");
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
        m.rows("SELECT event_id FROM events WHERE kind = 'correction'")
            .len(),
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

    let dry = m.attempt(&[
        "retract",
        "--attempt",
        &att,
        "--reason",
        "test",
        "--dry-run",
    ]);
    assert!(dry.ok(), "{}", dry.all());
    assert!(dry.stdout.contains("(dry run"), "{}", dry.stdout);
    assert!(
        !dry.stdout.contains("att_att_") && !dry.stdout.contains("ses_ses_"),
        "a doubled id prefix: {}",
        dry.stdout
    );
    let dry = m.attempt(&[
        "retract",
        "--session",
        &session,
        "--reason",
        "test",
        "--dry-run",
    ]);
    assert!(dry.ok(), "{}", dry.all());
    assert!(!dry.stdout.contains("ses_ses_"), "{}", dry.stdout);

    let out = m.attempt(&[
        "retract",
        "--session",
        &session,
        "--reason",
        "privacy",
        "--yes",
    ]);
    assert!(out.ok(), "{}", out.all());
    assert!(out.stdout.contains("wrote retraction"), "{}", out.stdout);
    let rows = m.rows("SELECT reason, matched FROM retractions");
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["reason"], "privacy", "{rows:?}");
    // The retraction took effect: the session's events are flagged.
    let rows = m.rows(
        "SELECT count(*) AS n FROM events WHERE retracted = false AND kind = 'prompt_submitted'",
    );
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
        &[
            "correct",
            &att,
            "--outcome",
            "failed",
            "--failure-class",
            "wrong_fix",
        ],
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

// ---------------------------------------------------------------------------
// P1-4: an unknown repository must not silently become "every project"
// ---------------------------------------------------------------------------

#[cfg(unix)]
const OTHER_ROOT: &str = "/home/dev/other/secret-repo";

#[cfg(unix)]
impl Machine {
    /// A directory that git would call a repository (`git_info` only reads
    /// `.git`), canonicalised so that it matches the path a process started
    /// there reports.
    fn repo(&self, name: &str) -> PathBuf {
        let dir = self.work.join(name);
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::write(dir.join(".git").join("HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::canonicalize(&dir).unwrap()
    }

    /// Two projects' worth of history: this one's, and another repository's.
    fn two_projects(&self, known: &Path) {
        self.spool(&self.session(
            &known.display().to_string(),
            "known-1",
            "2026-08-20T09:00:00Z",
            true,
        ));
        self.spool(&self.session(OTHER_ROOT, "other-1", "2026-08-20T10:00:00Z", true));
    }
}

// A repository's path is compared as text; Windows spells a temp directory
// several ways (8.3 names, `\\?\` prefixes), so these run where it is one.
#[cfg(unix)]
#[test]
fn a_shareable_export_from_an_unknown_repository_is_refused() {
    let m = Machine::new();
    let known = m.repo("known");
    m.two_projects(&known);
    let unknown = m.repo("repo3");
    let out_dir = m.work.join("out");
    std::fs::create_dir_all(&out_dir).unwrap();

    for (file, extra) in [
        ("x.html", vec!["--sanitized"]),
        ("y.html", vec![]),
        ("x.svg", vec!["--sanitized"]),
        ("y.svg", vec![]),
    ] {
        let target = out_dir.join(file);
        let mut args = vec!["ui", "export", target.to_str().unwrap()];
        args.extend(extra.iter().copied());
        let out = m.run(&unknown, &args, false);
        assert_eq!(out.code, Some(1), "{file}: {}", out.all());
        assert!(
            out.stderr.contains("--all-projects") && out.stderr.contains("--project"),
            "{file}: the message says how to proceed: {}",
            out.stderr
        );
        assert!(
            out.stderr
                .contains("no events are recorded for the repository"),
            "{file}: {}",
            out.stderr
        );
        assert!(!target.exists(), "{file}: nothing may be written");
        assert!(!out.stdout.contains("scope all projects"), "{}", out.stdout);
    }

    // The sanitized snapshot is the same kind of export.
    let snap = out_dir.join("s.atdb");
    let out = m.run(
        &unknown,
        &["snapshot", "export", snap.to_str().unwrap(), "--sanitized"],
        false,
    );
    assert_eq!(out.code, Some(1), "{}", out.all());
    assert!(out.stderr.contains("--all-projects"), "{}", out.stderr);
    assert!(!snap.exists());
    let out = m.run(
        &unknown,
        &[
            "snapshot",
            "export",
            snap.to_str().unwrap(),
            "--sanitized",
            "--project",
            "known",
        ],
        false,
    );
    assert!(out.ok(), "{}", out.all());

    // Not inside a repository at all: refused too, with its own reason.
    let target = out_dir.join("z.html");
    let out = m.run(
        &m.work,
        &["ui", "export", target.to_str().unwrap(), "--sanitized"],
        false,
    );
    assert_eq!(out.code, Some(1), "{}", out.all());
    assert!(
        out.stderr.contains("not inside a git repository"),
        "{}",
        out.stderr
    );
    assert!(!target.exists());

    // Naming the scope is how to proceed.
    let target = out_dir.join("all.html");
    let out = m.run(
        &unknown,
        &[
            "ui",
            "export",
            target.to_str().unwrap(),
            "--sanitized",
            "--all-projects",
        ],
        false,
    );
    assert!(out.ok(), "{}", out.all());
    assert!(
        std::fs::read_to_string(&target)
            .unwrap()
            .contains("secret-repo")
    );
    let target = out_dir.join("one.html");
    let out = m.run(
        &unknown,
        &[
            "ui",
            "export",
            target.to_str().unwrap(),
            "--sanitized",
            "--project",
            "known",
        ],
        false,
    );
    assert!(out.ok(), "{}", out.all());
    let html = std::fs::read_to_string(&target).unwrap();
    assert!(
        !html.contains("secret-repo"),
        "another repository leaked into a one-project export"
    );

    // A repository the database knows needs no flag, and carries only itself.
    let target = out_dir.join("known.html");
    let out = m.run(
        &known,
        &["ui", "export", target.to_str().unwrap(), "--sanitized"],
        false,
    );
    assert!(out.ok(), "{}", out.all());
    let html = std::fs::read_to_string(&target).unwrap();
    assert!(
        !html.contains("secret-repo"),
        "the default scope is this repository"
    );
}

// A repository's path is compared as text; Windows spells a temp directory
// several ways (8.3 names, `\\?\` prefixes), so these run where it is one.
#[cfg(unix)]
#[test]
fn read_commands_in_an_unknown_repository_say_they_show_every_project() {
    let m = Machine::new();
    let known = m.repo("known");
    m.two_projects(&known);
    let unknown = m.repo("repo3");
    let warning = "no events recorded for this repository; showing all projects, pass --project or --all-projects";

    for args in [
        vec!["timeline"],
        vec!["failures"],
        vec!["handoffs"],
        vec!["query", "SELECT count(*) AS n FROM sessions"],
        vec!["why"],
        vec!["events"],
    ] {
        let out = m.run(&unknown, &args, false);
        assert!(out.ok(), "{args:?}: {}", out.all());
        assert!(
            out.stderr.contains(warning),
            "{args:?}: a warning on stderr: {}",
            out.stderr
        );
        assert!(!out.stdout.contains(warning), "{args:?}: not in the result");
    }
    // The result really is every project's.
    let out = m.run(
        &unknown,
        &["--json", "query", "SELECT count(*) AS n FROM sessions"],
        false,
    );
    assert_eq!(out.json()[0]["n"], 2, "{}", out.all());

    // No warning when the repository is known, when a scope is named, or
    // outside any repository (where "every project" is what the help says).
    for (cwd, args) in [
        (&known, vec!["timeline"]),
        (&unknown, vec!["timeline", "--all-projects"]),
        (&unknown, vec!["timeline", "--project", "known"]),
        (&m.work, vec!["timeline"]),
    ] {
        let out = m.run(cwd, &args, false);
        assert!(out.ok(), "{args:?}: {}", out.all());
        assert!(
            !out.stderr.contains("no events recorded"),
            "{args:?}: {}",
            out.stderr
        );
    }
    // And a known repository sees only itself.
    let out = m.run(
        &known,
        &["--json", "query", "SELECT count(*) AS n FROM sessions"],
        false,
    );
    assert_eq!(out.json()[0]["n"], 1, "{}", out.all());
}

#[cfg(unix)]
#[test]
fn the_warning_comes_through_a_running_daemon_too() {
    let m = Machine::new();
    let known = m.repo("known");
    m.two_projects(&known);
    let unknown = m.repo("repo3");
    assert!(m.attempt(&["status"]).ok());
    let daemon = start_daemon(&m);
    for args in [
        vec!["timeline"],
        vec!["query", "SELECT count(*) AS n FROM sessions"],
    ] {
        let out = m.run(&unknown, &args, true);
        assert!(out.ok(), "{args:?}: {}", out.all());
        assert!(
            out.stderr
                .contains("no events recorded for this repository"),
            "{args:?}: {}",
            out.stderr
        );
        assert!(
            !out.stdout.contains("no events recorded"),
            "{args:?}: {}",
            out.stdout
        );
    }
    let out = m.run(&known, &["timeline"], true);
    assert!(!out.stderr.contains("no events recorded"), "{}", out.stderr);
    drop(daemon);
}

// ---------------------------------------------------------------------------
// P1-5: a session with no end that went quiet is stale, not open
// ---------------------------------------------------------------------------

#[test]
fn the_timeline_and_the_retract_preview_call_a_quiet_session_stale() {
    let m = Machine::new();
    // Weeks old, never ended.
    m.spool(&m.session(PROJECT_ROOT, "quiet-1", "2026-08-20T09:00:00Z", false));
    // Weeks old, ended.
    m.spool(&m.session(PROJECT_ROOT, "done-1", "2026-08-21T09:00:00Z", true));
    let out = m.attempt(&["timeline", "--all-projects"]);
    assert!(out.ok(), "{}", out.all());
    assert!(out.stdout.contains("→ stale"), "{}", out.stdout);
    assert!(!out.stdout.contains("→ open"), "{}", out.stdout);
    let rows = m.rows("SELECT provider_session_id, state FROM sessions ORDER BY started_at");
    assert_eq!(rows[0]["state"], "stale", "{rows:?}");
    assert_eq!(rows[1]["state"], "closed", "{rows:?}");

    let ses = m.rows("SELECT session_id FROM sessions ORDER BY started_at")[0]["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    let out = m.attempt(&[
        "retract",
        "--session",
        &ses,
        "--reason",
        "test",
        "--dry-run",
    ]);
    assert!(out.ok(), "{}", out.all());
    assert!(out.stdout.contains("→ stale"), "{}", out.stdout);
    assert!(!out.stdout.contains("→ open"), "{}", out.stdout);
}

// ---------------------------------------------------------------------------
// P1-6: `--since -2h`, the documented spelling, works
// ---------------------------------------------------------------------------

#[test]
fn a_relative_time_is_accepted_after_a_space() {
    let m = Machine::new();
    m.spool(&m.session(PROJECT_ROOT, "time-1", "2026-08-20T09:00:00Z", true));
    assert!(m.attempt(&["status"]).ok());
    // Both spellings mean the same thing, for every command with a window.
    for flag in ["--since", "--until"] {
        for value in ["-2h", "-30m", "-1d", "-1w"] {
            for cmd in [
                vec!["timeline", "--all-projects"],
                vec![
                    "query",
                    "--all-projects",
                    "SELECT count(*) AS n FROM events",
                ],
                vec!["events", "--all-projects"],
            ] {
                let mut spaced = cmd.clone();
                spaced.extend([flag, value]);
                let joined = format!("{flag}={value}");
                let mut equals = cmd.clone();
                equals.push(&joined);
                let a = m.attempt(&spaced);
                let b = m.attempt(&equals);
                assert!(a.ok(), "{spaced:?}: {}", a.all());
                assert!(b.ok(), "{equals:?}: {}", b.all());
                assert!(
                    !a.stderr.contains("unexpected argument"),
                    "{spaced:?}: {}",
                    a.stderr
                );
            }
        }
    }
    // The window applies: events from August are older than two hours, so a
    // window that starts two hours ago holds none of them.
    let out = m.attempt(&[
        "--json",
        "query",
        "--all-projects",
        "--since",
        "-2h",
        "SELECT count(*) AS n FROM events",
    ]);
    assert!(out.ok(), "{}", out.all());
    assert_eq!(out.json()[0]["n"], 0, "{}", out.all());
    let out = m.attempt(&[
        "--json",
        "query",
        "--all-projects",
        "--until",
        "-2h",
        "SELECT count(*) AS n FROM events",
    ]);
    assert!(out.ok(), "{}", out.all());
    assert!(out.json()[0]["n"].as_u64().unwrap() > 0, "{}", out.all());
}

// ---------------------------------------------------------------------------
// P2(a): `-n` caps the rows of a SQL statement
// ---------------------------------------------------------------------------

#[test]
fn n_caps_the_rows_of_a_sql_statement() {
    let m = Machine::new();
    m.spool(&m.session(PROJECT_ROOT, "limit-1", "2026-08-20T09:00:00Z", true));
    let all = m.rows("SELECT event_id FROM events");
    assert!(all.len() > 3, "{all:?}");
    let out = m.attempt(&[
        "--json",
        "query",
        "--all-projects",
        "-n",
        "1",
        "SELECT event_id FROM events",
    ]);
    assert!(out.ok(), "{}", out.all());
    assert_eq!(out.json().as_array().unwrap().len(), 1, "{}", out.all());
    let out = m.attempt(&[
        "query",
        "--all-projects",
        "-n",
        "2",
        "SELECT event_id FROM events",
    ]);
    assert!(out.ok(), "{}", out.all());
    assert!(
        out.stdout.contains("showing the first 2 of"),
        "{}",
        out.stdout
    );
    // AttemptQL takes the same cap, and a cap above the result changes nothing.
    let out = m.attempt(&[
        "--json",
        "query",
        "--all-projects",
        "-n",
        "1",
        "SHOW SESSIONS",
    ]);
    assert!(out.ok(), "{}", out.all());
    let out = m.attempt(&[
        "--json",
        "query",
        "--all-projects",
        "-n",
        "1000",
        "SELECT event_id FROM events",
    ]);
    assert_eq!(out.json().as_array().unwrap().len(), all.len());
}

// ---------------------------------------------------------------------------
// P2(d)/(e): first-run hints, database that cannot be opened, repeated tails
// ---------------------------------------------------------------------------

#[test]
fn a_missing_database_points_at_setup_and_import_does_not_create_one() {
    let m = Machine::new();
    for args in [
        vec!["timeline"],
        vec!["status"],
        vec!["query", "SELECT 1"],
        vec!["ui", "--no-open"],
        vec!["correct", "att_00000000", "--outcome", "failed"],
        vec!["compact"],
        vec!["import"],
    ] {
        let out = m.attempt(&args);
        assert_eq!(out.code, Some(1), "{args:?}: {}", out.all());
        assert!(
            out.stderr.contains("attempt setup") && out.stderr.contains("attempt init"),
            "{args:?}: setup first, init as the database-only alternative: {}",
            out.stderr
        );
    }
    assert!(
        !m.db_dir().exists() && !m.data.join("db").exists(),
        "no command above may create a database"
    );
    // `init` itself says what comes next, in the same words.
    let out = m.attempt(&["init", "--no-encryption"]);
    assert!(out.ok(), "{}", out.all());
    assert!(out.stdout.contains("`attempt setup`"), "{}", out.stdout);
    assert!(m.db_dir().exists());
}

#[test]
fn db_pointing_at_the_directory_that_holds_the_database_says_so() {
    let m = Machine::new();
    let project = m.work.join("proj");
    let inside = project.join(".attemptdb");
    std::fs::create_dir_all(&project).unwrap();
    Database::create(&inside, m.device).unwrap();
    let out = m.attempt(&[
        "--db",
        project.to_str().unwrap(),
        "timeline",
        "--all-projects",
    ]);
    assert_eq!(out.code, Some(1), "{}", out.all());
    assert!(
        out.stderr.contains(&format!("--db {}", inside.display())),
        "{}",
        out.stderr
    );
    // Pointing at the database itself works.
    let out = m.attempt(&[
        "--db",
        inside.to_str().unwrap(),
        "timeline",
        "--all-projects",
    ]);
    assert!(out.ok(), "{}", out.all());
}

#[test]
fn a_database_from_a_newer_attempt_says_to_update_and_the_ui_will_not_serve_it() {
    let m = Machine::new();
    m.spool(&m.session(PROJECT_ROOT, "newer-1", "2026-08-20T09:00:00Z", true));
    assert!(m.attempt(&["status"]).ok());
    let identity = m.db_dir().join("ATTEMPTDB");
    let identity = if identity.exists() {
        identity
    } else {
        std::fs::read_dir(m.db_dir())
            .unwrap()
            .map(|e| e.unwrap().path())
            .find(|p| {
                p.file_name().is_some_and(|n| {
                    n.to_string_lossy()
                        .to_ascii_lowercase()
                        .contains("identity")
                })
            })
            .expect("an identity file")
    };
    let mut doc: Value = serde_json::from_slice(&std::fs::read(&identity).unwrap()).unwrap();
    doc["format_version"] = json!(7);
    std::fs::write(&identity, serde_json::to_vec_pretty(&doc).unwrap()).unwrap();

    let out = m.attempt(&["status"]);
    assert_eq!(out.code, Some(1), "{}", out.all());
    assert!(
        out.stderr.contains("unsupported format version 7"),
        "{}",
        out.stderr
    );
    assert!(
        out.stderr.contains("newer attempt") && out.stderr.contains("update attempt"),
        "{}",
        out.stderr
    );
    // The UI refuses to start (a regression would serve until killed).
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_attempt"));
    cmd.arg("--data-dir")
        .arg(&m.data)
        .args(["ui", "--no-open"])
        .current_dir(&m.work)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    m.isolate(&mut cmd, false);
    let mut child = cmd.spawn().unwrap();
    let started = Instant::now();
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s;
        }
        if started.elapsed() > Duration::from_secs(20) {
            let _ = child.kill();
            panic!("`attempt ui` started on a database it cannot read");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let mut stdout = String::new();
    let mut stderr = String::new();
    use std::io::Read;
    child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut stdout)
        .unwrap();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    assert_eq!(status.code(), Some(1), "{stdout}{stderr}");
    assert!(!stdout.contains("url"), "{stdout}");
    assert!(stderr.contains("update attempt"), "{stderr}");
}

#[cfg(unix)]
#[test]
fn a_read_only_database_directory_is_named_and_the_error_is_said_once() {
    use std::os::unix::fs::PermissionsExt;
    let m = Machine::new();
    m.spool(&m.session(PROJECT_ROOT, "ro-1", "2026-08-20T09:00:00Z", true));
    assert!(m.attempt(&["status"]).ok());
    let dir = m.db_dir();
    // A database on a read-only mount: every directory and file of it.
    fn set_mode(path: &Path, dir_mode: u32, file_mode: u32) {
        for entry in std::fs::read_dir(path).unwrap() {
            let p = entry.unwrap().path();
            if p.is_dir() {
                set_mode(&p, dir_mode, file_mode);
            } else {
                std::fs::set_permissions(&p, std::fs::Permissions::from_mode(file_mode)).unwrap();
            }
        }
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(dir_mode)).unwrap();
    }
    set_mode(&dir, 0o555, 0o444);
    // As root, permissions do not apply: nothing to test.
    let writable = std::fs::File::create(dir.join("probe")).is_ok();
    let out = m.attempt(&["status"]);
    set_mode(&dir, 0o755, 0o644);
    if writable {
        eprintln!("skipped: this user can write through a read-only directory");
        return;
    }
    assert_eq!(out.code, Some(1), "{}", out.all());
    assert!(out.stderr.contains("is not writable"), "{}", out.stderr);
    assert!(out.stderr.contains("--snapshot"), "{}", out.stderr);
    assert_eq!(
        out.stderr.matches("Permission denied").count(),
        1,
        "the cause is not repeated: {}",
        out.stderr
    );
}

// ---------------------------------------------------------------------------
// P2(h): a typo gets a suggestion on the CLI too
// ---------------------------------------------------------------------------

#[test]
fn a_mistyped_keyword_gets_a_suggestion_on_the_cli() {
    let m = Machine::new();
    m.spool(&m.session(PROJECT_ROOT, "typo-1", "2026-08-20T09:00:00Z", true));
    for (statement, want) in [
        ("SELEC 1", "did you mean SELECT?"),
        ("SHOWW SESSIONS", "did you mean SHOW?"),
    ] {
        let out = m.attempt(&["query", statement]);
        assert_eq!(out.code, Some(1), "{}", out.all());
        assert!(out.stderr.contains(want), "{statement}: {}", out.stderr);
    }
}
