//! The CLI reads through the daemon: `attempt daemon run` hosts the read
//! engine; `attempt query` and `attempt timeline` in another process ask it
//! and get the same answer the local engine gives, and `ATTEMPTDB_NO_DAEMON`
//! forces the local path.
//!
//! Unix only, like `attemptdb-capture/tests/daemon.rs`: the Windows daemon
//! (named pipe, per-user service) is not implemented yet, so `daemon run`
//! serves nothing there and the CLI takes the local path.
#![cfg(unix)]

use attemptdb_core::event::Provider;
use attemptdb_core::{CaptureMode, DeviceId, Event, EventKind, ProjectRef};
use attemptdb_storage::{Database, OpenOptions};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn events(dev: DeviceId, n: usize) -> Vec<Event> {
    let mut out = Vec::new();
    for s in 0..3 {
        let mut ev = Event::new(
            dev,
            Provider::ClaudeCode,
            "UserPromptSubmit",
            EventKind::PromptSubmitted,
            ProjectRef::derive("/home/dev/example/project", None, &dev),
            format!("session-{s}"),
            CaptureMode::LocalSemantic,
            "attempt-daemon-read-test/0.1",
        );
        ev.attrs
            .insert("prompt_chars".into(), serde_json::json!(12));
        out.push(ev);
        for _ in 0..n {
            out.push(Event::new(
                dev,
                Provider::ClaudeCode,
                "PostToolUse",
                EventKind::ToolCallFinished,
                ProjectRef::derive("/home/dev/example/project", None, &dev),
                format!("session-{s}"),
                CaptureMode::LocalSemantic,
                "attempt-daemon-read-test/0.1",
            ));
        }
    }
    out
}

fn attempt(data_dir: &Path, args: &[&str], no_daemon: bool) -> (bool, String) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_attempt"));
    cmd.arg("--data-dir")
        .arg(data_dir)
        .args(args)
        .env("ATTEMPTDB_KEYRING", "off")
        .env_remove("ATTEMPTDB_KEY_FILE")
        .env_remove("ATTEMPTDB_DIR")
        .current_dir(data_dir);
    if no_daemon {
        cmd.env("ATTEMPTDB_NO_DAEMON", "1");
    } else {
        cmd.env_remove("ATTEMPTDB_NO_DAEMON");
    }
    let out = cmd.output().expect("run attempt");
    (
        out.status.success(),
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
    )
}

fn wait_for(data_dir: &Path, running: bool) {
    let t = Instant::now();
    loop {
        let (_, text) = attempt(data_dir, &["daemon", "status"], true);
        let is_running = text.contains("running (pid");
        if is_running == running {
            return;
        }
        assert!(
            t.elapsed() < Duration::from_secs(20),
            "daemon state: {text}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

struct Daemon(Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn cli_reads_through_the_daemon_and_agrees_with_the_local_engine() {
    // Short prefix: the daemon's socket path must fit sun_path.
    let tmp = tempfile::Builder::new().prefix("atdb").tempdir().unwrap();
    let data_dir = tmp.path().join("data");
    let db = data_dir.join("db").join(".attemptdb");
    std::fs::create_dir_all(db.parent().unwrap()).unwrap();
    let dev = DeviceId::new();
    Database::create(&db, dev).unwrap();
    {
        let mut w = Database::open(
            &db,
            OpenOptions {
                device_id: Some(dev),
                ..Default::default()
            },
        )
        .unwrap();
        w.ingest(events(dev, 40)).unwrap();
        w.close().unwrap();
    }

    let child = Command::new(env!("CARGO_BIN_EXE_attempt"))
        .arg("--data-dir")
        .arg(&data_dir)
        .args(["daemon", "run", "--foreground"])
        .env("ATTEMPTDB_KEYRING", "off")
        .env_remove("ATTEMPTDB_KEY_FILE")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let _daemon = Daemon(child);
    wait_for(&data_dir, true);

    let sql = "SELECT count(*) AS n FROM events";
    let (ok, via_daemon) = attempt(&data_dir, &["--json", "query", sql], false);
    assert!(ok, "{via_daemon}");
    let (ok, local) = attempt(&data_dir, &["--json", "query", sql], true);
    assert!(ok, "{local}");
    assert_eq!(via_daemon, local, "daemon and local answers differ");
    assert!(
        via_daemon.contains("123"),
        "3 prompts + 120 tool calls: {via_daemon}"
    );

    let (ok, tl_daemon) = attempt(&data_dir, &["timeline", "--limit", "2"], false);
    assert!(ok, "{tl_daemon}");
    let (ok, tl_local) = attempt(&data_dir, &["timeline", "--limit", "2"], true);
    assert!(ok, "{tl_local}");
    assert_eq!(tl_daemon, tl_local, "timeline rendered differently");
    assert!(
        tl_daemon.starts_with("3 session(s), 3 turn(s)"),
        "totals come from the whole projection: {tl_daemon}"
    );

    // A statement the engine refuses is refused the same way through the
    // daemon (the CLI falls back to its own engine for the message).
    let (ok, text) = attempt(&data_dir, &["query", "SHOW NONSENSE"], false);
    assert!(!ok);
    assert!(text.contains("error"), "{text}");
}
