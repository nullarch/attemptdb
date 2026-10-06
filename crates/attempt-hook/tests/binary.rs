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
            .env_remove("CLAUDE_PROJECT_DIR")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
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
    let (out, _) = wait_within(child, Duration::from_secs(10));
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
    let (out, elapsed) = wait_within(child, Duration::from_secs(8));
    drop(stdin);
    assert_silent_success(&out);
    assert!(
        elapsed < Duration::from_secs(6),
        "waited {elapsed:?}: the idle deadline is about 2 s"
    );
    let events = sb.events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].provider_session_id, "stuck-1");
    assert_eq!(
        events[0].attrs.get("capture_gap"),
        Some(&serde_json::json!("stdin_timeout"))
    );
}

#[test]
fn a_complete_payload_on_a_stdin_left_open_is_recorded_without_the_idle_wait() {
    let sb = sandbox();
    let mut child = sb.command(&["claude-code"]).spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin
        .write_all(payload(sb.tmp.path(), "open-1").as_bytes())
        .unwrap();
    let (out, elapsed) = wait_within(child, Duration::from_secs(8));
    drop(stdin);
    assert_silent_success(&out);
    assert!(elapsed < Duration::from_millis(1500), "waited {elapsed:?}");
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
    let (out, _) = wait_within(child, Duration::from_secs(30));
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
    let (out, _) = wait_within(child, Duration::from_secs(10));
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
    let (out, _) = wait_within(child, Duration::from_secs(10));
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
        let (out, _) = wait_within(child, Duration::from_secs(30));
        assert_silent_success(&out);
    }
    let events = sb.events();
    assert_eq!(events.len(), 24);
    let devices: std::collections::HashSet<_> = events.iter().map(|e| e.device_id).collect();
    assert_eq!(devices.len(), 1, "one machine, one device id: {devices:?}");
}

#[test]
fn an_ordinary_event_takes_low_milliseconds_of_wall_time() {
    let sb = sandbox();
    let run = |session: &str| {
        let mut child = sb.command(&["claude-code"]).spawn().unwrap();
        let mut stdin = child.stdin.take().unwrap();
        stdin
            .write_all(payload(sb.tmp.path(), session).as_bytes())
            .unwrap();
        drop(stdin);
        let (out, elapsed) = wait_within(child, Duration::from_secs(10));
        assert_silent_success(&out);
        elapsed
    };
    run("warm-up");
    let mut times: Vec<Duration> = (0..15).map(|i| run(&format!("timed-{i}"))).collect();
    times.sort();
    let median = times[times.len() / 2];
    // A debug build on a shared machine: generous. A stdin wait is 2000 ms,
    // a database open is far more than this.
    assert!(
        median < Duration::from_millis(300),
        "median {median:?}, all {times:?}"
    );
}
