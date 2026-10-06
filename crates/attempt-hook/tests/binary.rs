//! The `attempt-hook` executable as an agent runs it: stdin piped in,
//! whatever the agent hands it, and the invariants that must hold every
//! time: exit status 0, nothing on stdout (but Gemini's allow), nothing on
//! stderr, and a bounded wait.

use attemptdb_core::{CaptureMode, DeviceId, Event};
use attemptdb_storage::{Database, OpenOptions, ScanFilter};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_attempt-hook");

/// How long a hook may take to exit before a test calls it hung. Starting a
/// process can take tens of seconds on a loaded macOS machine (the kernel
/// vets the executable before `main` runs), so this does not measure speed:
/// the in-process tests in `attemptdb-capture` do (`read_bounded`, "low
/// milliseconds"). It tells a hook that exits from one that waits forever.
const EXIT_LIMIT: Duration = Duration::from_secs(180);

/// Variables that point an agent (and so AttemptDB) at its real, per-user
/// configuration, plus the ones that choose a database. None may reach the
/// child: these tests name their directories on the command line.
const INHERITED_ENV: [&str; 9] = [
    "CLAUDE_CONFIG_DIR",
    "CODEX_HOME",
    "CURSOR_CONFIG_DIR",
    "GEMINI_CONFIG_DIR",
    "CLAUDE_PROJECT_DIR",
    "ATTEMPTDB_DIR",
    "ATTEMPTDB_DATA_DIR",
    "ATTEMPTDB_KEY_FILE",
    "ATTEMPTDB_PASSPHRASE",
];

fn isolate(c: &mut Command, home: &Path) {
    for var in INHERITED_ENV {
        c.env_remove(var);
    }
    c.env("HOME", home)
        .env("USERPROFILE", home)
        .env("ATTEMPTDB_KEYRING", "off");
}

struct Sandbox {
    tmp: tempfile::TempDir,
}

fn sandbox() -> Sandbox {
    Sandbox {
        tmp: tempfile::tempdir().unwrap(),
    }
}

impl Sandbox {
    fn data(&self) -> PathBuf {
        self.tmp.path().join("data")
    }

    fn db(&self) -> PathBuf {
        self.tmp.path().join("db")
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut c = Command::new(BIN);
        c.args(args)
            .arg("--data-dir")
            .arg(self.data())
            .arg("--db")
            .arg(self.db())
            .current_dir(self.tmp.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        isolate(&mut c, &self.tmp.path().join("home"));
        c
    }

    fn events(&self) -> Vec<Event> {
        let mut db = Database::open(
            &self.db(),
            OpenOptions {
                create: true,
                device_id: Some(DeviceId::new()),
                ..Default::default()
            },
        )
        .unwrap();
        db.import_spool().unwrap();
        db.scan(&ScanFilter::default()).unwrap()
    }
}

fn payload(cwd: &Path, session: &str) -> String {
    serde_json::json!({
        "hook_event_name": "UserPromptSubmit",
        "session_id": session,
        "cwd": cwd.to_string_lossy(),
        "prompt": "hello there",
    })
    .to_string()
}

/// Wait for `child`, killing it (and failing) after `limit`.
fn wait_within(mut child: Child, limit: Duration) -> (std::process::Output, Duration) {
    let started = Instant::now();
    loop {
        if child.try_wait().unwrap().is_some() {
            let elapsed = started.elapsed();
            return (child.wait_with_output().unwrap(), elapsed);
        }
        if started.elapsed() > limit {
            let _ = child.kill();
            panic!("the hook did not exit within {limit:?}");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn assert_silent_success(out: &std::process::Output) {
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.stdout.is_empty(),
        "stdout: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(
        out.stderr.is_empty(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[cfg(unix)]
#[test]
fn the_harness_hides_the_owners_agent_config_from_children() {
    let sb = sandbox();
    let mut cmd = Command::new("/usr/bin/env");
    // What a shell with the owner's configuration hands down.
    for var in INHERITED_ENV {
        cmd.env(var, "/owner/real/config");
    }
    isolate(&mut cmd, &sb.tmp.path().join("home"));
    let out = cmd.output().unwrap();
    let env = String::from_utf8_lossy(&out.stdout);
    for var in INHERITED_ENV {
        assert!(
            !env.lines().any(|l| l.starts_with(&format!("{var}="))),
            "{var} reached the child:\n{env}"
        );
    }
    assert!(env.contains("HOME="), "{env}");
}

#[test]
fn a_missing_provider_exits_zero_silently_and_is_logged() {
    let sb = sandbox();
    let mut child = sb.command(&[]).spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"{\"hook_event_name\":\"Stop\"}")
        .unwrap();
    let (out, _) = wait_within(child, EXIT_LIMIT);
    assert_silent_success(&out);
    let log = std::fs::read_to_string(sb.data().join("logs").join("hook.log")).unwrap();
    assert!(log.contains("without a provider id"), "{log}");
    assert!(sb.events().is_empty());
}

#[test]
fn a_stdin_that_never_closes_ends_within_the_deadline_and_still_records_an_event() {
    let sb = sandbox();
    let mut child = sb.command(&["claude-code"]).spawn().unwrap();
    // Half a payload, then silence with the pipe held open.
    let mut stdin = child.stdin.take().unwrap();
    stdin
        .write_all(br#"{"session_id":"stuck-1","hook_event_name":"PostToolUse","tool_input":{"command":"ls"#)
        .unwrap();
    // Returns although the agent never closes the pipe: an unbounded read
    // would sit here until the kill.
    let (out, _) = wait_within(child, EXIT_LIMIT);
    drop(stdin);
    assert_silent_success(&out);
    let events = sb.events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].provider_session_id, "stuck-1");
    assert_eq!(
        events[0].attrs.get("capture_gap"),
        Some(&serde_json::json!("stdin_timeout"))
    );
}

#[test]
fn a_complete_payload_on_a_stdin_left_open_is_recorded_normally() {
    // Whether the hook waits for the idle deadline is measured in-process
    // (`a_complete_payload_does_not_wait_for_a_stdin_that_stays_open`); here
    // the outcome is checked through the real binary.
    let sb = sandbox();
    let mut child = sb.command(&["claude-code"]).spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin
        .write_all(payload(sb.tmp.path(), "open-1").as_bytes())
        .unwrap();
    let (out, _) = wait_within(child, EXIT_LIMIT);
    drop(stdin);
    assert_silent_success(&out);
    let events = sb.events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].kind.as_str(), "prompt_submitted");
    assert!(events[0].attrs.get("capture_gap").is_none());
}

#[test]
fn an_oversize_payload_is_drained_so_the_agent_write_succeeds() {
    let sb = sandbox();
    let mut child = sb.command(&["claude-code"]).spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let head = br#"{"session_id":"huge-1","hook_event_name":"PostToolUse","tool_response":""#;
    let total = 17 * 1024 * 1024;
    let writer = std::thread::spawn(move || {
        stdin.write_all(head)?;
        let chunk = vec![b'x'; 1024 * 1024];
        let mut sent = head.len();
        while sent < total {
            stdin.write_all(&chunk)?;
            sent += chunk.len();
        }
        Ok::<usize, std::io::Error>(sent)
    });
    let (out, _) = wait_within(child, EXIT_LIMIT);
    assert_silent_success(&out);
    let sent = writer
        .join()
        .unwrap()
        .expect("the agent's write failed (EPIPE)");
    assert!(sent >= total);
    let events = sb.events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].provider_session_id, "huge-1");
    assert_eq!(
        events[0].attrs.get("capture_gap"),
        Some(&serde_json::json!("payload_truncated"))
    );
}

#[test]
fn gemini_still_gets_its_allow_decision() {
    let sb = sandbox();
    let mut child = sb.command(&["gemini-cli"]).spawn().unwrap();
    child.stdin.take().unwrap().write_all(b"{}").unwrap();
    let (out, _) = wait_within(child, EXIT_LIMIT);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "{\"decision\":\"allow\"}"
    );
    assert!(out.stderr.is_empty());
}

#[test]
fn an_unusable_config_records_metadata_only_through_the_real_binary() {
    let sb = sandbox();
    std::fs::create_dir_all(sb.data().join("config")).unwrap();
    std::fs::write(
        sb.data().join("config").join("config.json"),
        br#"{"capture_mode":"metadata-only"}"#,
    )
    .unwrap();
    let mut child = sb.command(&["claude-code"]).spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload(sb.tmp.path(), "cfg-1").as_bytes())
        .unwrap();
    let (out, _) = wait_within(child, EXIT_LIMIT);
    assert_silent_success(&out);
    let events = sb.events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].capture_mode, CaptureMode::MetadataOnly);
    assert!(events[0].content.is_none() && events[0].raw.is_none());
    assert!(!format!("{:?}", events[0]).contains("hello there"));
}

#[test]
fn many_first_use_hooks_agree_on_one_device_id() {
    let sb = sandbox();
    let mut children = Vec::new();
    for i in 0..24 {
        let mut child = sb.command(&["claude-code"]).spawn().unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(payload(sb.tmp.path(), &format!("race-{i}")).as_bytes())
            .unwrap();
        children.push(child);
    }
    for child in children {
        let (out, _) = wait_within(child, EXIT_LIMIT);
        assert_silent_success(&out);
    }
    let events = sb.events();
    assert_eq!(events.len(), 24);
    let devices: std::collections::HashSet<_> = events.iter().map(|e| e.device_id).collect();
    assert_eq!(devices.len(), 1, "one machine, one device id: {devices:?}");
}
