//! Daemon + IPC integration tests.
//!
//! Unix only: the tests drive the Unix-socket transport directly. The
//! named-pipe transport is exercised by the Windows CI job (planned).
#![cfg(unix)]

use attemptdb_capture::Locator;
use attemptdb_capture::daemon::{self, DaemonOptions};
use attemptdb_capture::hook::{Delivery, HookInput, HookOutcome, run_hook};
use attemptdb_capture::ipc::{
    self, Client, Frame, Hello, IpcError, MsgType, Nack, PROTOCOL_VERSION,
};
use attemptdb_capture::service;
use attemptdb_core::event::Provider;
use attemptdb_core::{CaptureMode, DeviceId, Event, EventKind, ProjectRef};
use attemptdb_storage::{Database, OpenOptions, ScanFilter};
use std::collections::HashSet;
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

struct Sandbox {
    _tmp: tempfile::TempDir,
    data_dir: PathBuf,
    project: PathBuf,
    locator: Locator,
}

fn sandbox() -> Sandbox {
    // Short prefix: the socket path must fit sun_path.
    let tmp = tempfile::Builder::new().prefix("atdb").tempdir().unwrap();
    let data_dir = tmp.path().join("data");
    let project = tmp.path().join("proj");
    std::fs::create_dir_all(&project).unwrap();
    let locator = Locator::resolve(&project, Some(&data_dir), None);
    Sandbox {
        _tmp: tmp,
        data_dir,
        project,
        locator,
    }
}

type DaemonHandle = JoinHandle<attemptdb_capture::Result<()>>;

fn start(locator: &Locator) -> DaemonHandle {
    let l = locator.clone();
    let handle = std::thread::spawn(move || {
        daemon::run(
            &l,
            DaemonOptions {
                spool_interval: Duration::from_millis(200),
                ..Default::default()
            },
        )
    });
    daemon::wait_until_running(locator, Duration::from_secs(15)).expect("daemon did not start");
    handle
}

fn stop(locator: &Locator, handle: DaemonHandle) {
    assert!(daemon::stop(locator).unwrap(), "daemon was not running");
    handle.join().unwrap().unwrap();
    assert!(
        !ipc::daemon_reachable(locator),
        "socket still present after shutdown"
    );
    assert!(
        !ipc::pid_path(locator).exists(),
        "pid file still present after shutdown"
    );
}

fn event(device: DeviceId, thread: usize, i: usize) -> Event {
    let mut e = Event::new(
        device,
        Provider::ClaudeCode,
        "PostToolUse",
        EventKind::ToolCallFinished,
        ProjectRef::derive("/p", None, &device),
        format!("s-{thread}"),
        CaptureMode::LocalSemantic,
        "test",
    );
    e.attrs.insert("i".into(), serde_json::json!(i));
    e
}

fn raw_connection(locator: &Locator) -> UnixStream {
    let path = ipc::endpoint(locator).socket_path().unwrap().to_path_buf();
    let s = UnixStream::connect(path).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    s
}

fn read_nack(s: &mut UnixStream) -> Nack {
    let f = Frame::read_from(s).unwrap();
    assert_eq!(
        f.kind(),
        Some(MsgType::Nack),
        "expected NACK, got type {}",
        f.msg_type
    );
    f.parse_json().unwrap()
}

fn open_read_only(locator: &Locator) -> Database {
    Database::open(
        &locator.db_dir,
        OpenOptions {
            read_only: true,
            ..Default::default()
        },
    )
    .unwrap()
}

#[test]
fn daemon_ingests_concurrent_batches_and_shuts_down() {
    let sb = sandbox();
    let handle = start(&sb.locator);

    let st = daemon::status(&sb.locator).expect("status");
    assert_eq!(st.pid, std::process::id());
    assert_eq!(st.db_dir, sb.locator.db_dir);
    assert_eq!(st.protocol_version, PROTOCOL_VERSION);
    assert!(ipc::pid_path(&sb.locator).exists());
    assert!(ipc::endpoint_record_path(&sb.locator).exists());

    // 4 threads x 25 events, one connection per event.
    let device = DeviceId::new();
    let threads: Vec<JoinHandle<Vec<Event>>> = (0..4)
        .map(|t| {
            let locator = sb.locator.clone();
            std::thread::spawn(move || {
                let mut sent = Vec::new();
                for i in 0..25 {
                    let ev = event(device, t, i);
                    let ack = Client::send_events(&locator, std::slice::from_ref(&ev))
                        .unwrap_or_else(|e| panic!("thread {t} event {i}: {e}"));
                    assert_eq!(ack.accepted, vec![ev.event_id]);
                    assert!(ack.duplicate.is_empty());
                    assert!(ack.rejected.is_empty());
                    assert!(ack.durable_source_seq >= 1);
                    sent.push(ev);
                }
                sent
            })
        })
        .collect();
    let mut all: Vec<Event> = Vec::new();
    for t in threads {
        all.extend(t.join().unwrap());
    }
    assert_eq!(all.len(), 100);

    // Re-sending is acknowledged as a duplicate, not stored twice.
    let ack = Client::send_events(&sb.locator, &all[..3]).unwrap();
    assert!(ack.accepted.is_empty());
    assert_eq!(ack.duplicate.len(), 3);
    assert_eq!(ack.durable_source_seq, 100);

    let st = daemon::status(&sb.locator).unwrap();
    assert_eq!(st.events_ingested, 100);
    assert_eq!(st.duplicates, 3);
    assert_eq!(st.batches, 101);
    assert!((1..=100).contains(&st.wal_commits), "{}", st.wal_commits);
    assert_eq!(st.last_source_seq, 100);
    assert!(st.connections >= 101);
    assert_eq!(st.rejected_connections, 0);

    // A second daemon for the same data dir refuses to start.
    let err = daemon::run(&sb.locator, DaemonOptions::default())
        .unwrap_err()
        .to_string();
    assert!(err.contains("already running"), "{err}");
    assert!(
        ipc::daemon_reachable(&sb.locator),
        "the refused daemon must not remove the live socket"
    );

    // Raw protocol conversation on one connection.
    let mut s = raw_connection(&sb.locator);
    s.write_all(&ipc::encode_prelude(PROTOCOL_VERSION, 0))
        .unwrap();
    // Unknown message type -> NACK, connection stays open.
    Frame {
        msg_type: 200,
        codec: ipc::CODEC_JSON,
        flags: 0,
        payload: vec![],
    }
    .write_to(&mut s)
    .unwrap();
    assert_eq!(read_nack(&mut s).code, "unknown_message_type");
    // INGEST before HELLO -> NACK.
    Frame::json(MsgType::Ingest, &vec![event(device, 9, 0)])
        .unwrap()
        .write_to(&mut s)
        .unwrap();
    assert_eq!(read_nack(&mut s).code, "hello_required");
    // HELLO for another database -> NACK.
    Frame::json(
        MsgType::Hello,
        &Hello::new("test", Path::new("/nonexistent/.attemptdb"), None),
    )
    .unwrap()
    .write_to(&mut s)
    .unwrap();
    assert_eq!(read_nack(&mut s).code, "wrong_database");
    // Ack/Pong are daemon->client only.
    Frame::empty(MsgType::Pong).write_to(&mut s).unwrap();
    assert_eq!(read_nack(&mut s).code, "unexpected_message_type");
    // Still alive: PING -> PONG.
    Frame::empty(MsgType::Ping).write_to(&mut s).unwrap();
    let pong = Frame::read_from(&mut s).unwrap();
    assert_eq!(pong.kind(), Some(MsgType::Pong));
    let status: ipc::DaemonStatus = pong.parse_json().unwrap();
    assert_eq!(status.events_ingested, 100);
    // Corrupt CRC -> NACK protocol_error and the connection is closed.
    let mut bytes = Frame::empty(MsgType::Ping).encode();
    bytes[4] ^= 0xff;
    s.write_all(&bytes).unwrap();
    assert_eq!(read_nack(&mut s).code, "protocol_error");
    assert!(matches!(
        Frame::read_from(&mut s),
        Err(IpcError::Closed) | Err(IpcError::Io(_))
    ));

    // Oversize length is rejected before any allocation.
    let mut s = raw_connection(&sb.locator);
    s.write_all(&ipc::encode_prelude(PROTOCOL_VERSION, 0))
        .unwrap();
    let mut header = Frame::empty(MsgType::Ping).encode();
    header[0..4].copy_from_slice(&(ipc::MAX_PAYLOAD + 1).to_le_bytes());
    s.write_all(&header).unwrap();
    assert_eq!(read_nack(&mut s).code, "protocol_error");

    // Unsupported protocol version.
    let mut s = raw_connection(&sb.locator);
    s.write_all(&ipc::encode_prelude(99, 0)).unwrap();
    let n = read_nack(&mut s);
    assert_eq!(n.code, "unsupported_protocol");
    assert!(!n.retryable);

    // Bad magic.
    let mut s = raw_connection(&sb.locator);
    s.write_all(b"NOPE\x01\x00\x00\x00").unwrap();
    assert_eq!(read_nack(&mut s).code, "protocol_error");

    // SHUTDOWN stops the daemon and removes socket + pid file.
    stop(&sb.locator, handle);
    assert!(!ipc::endpoint_record_path(&sb.locator).exists());

    // Exactly 100 unique events with contiguous source_seq, flushed to a
    // segment on shutdown.
    let db = open_read_only(&sb.locator);
    let events = db.scan(&ScanFilter::default()).unwrap();
    assert_eq!(events.len(), 100);
    let unique: HashSet<_> = events.iter().map(|e| e.event_id).collect();
    assert_eq!(unique.len(), 100);
    let mut seqs: Vec<u64> = events.iter().map(|e| e.source_seq).collect();
    seqs.sort_unstable();
    assert_eq!(seqs, (1..=100).collect::<Vec<u64>>());
    assert!(events.iter().all(Event::is_ingested));
    let sent: HashSet<_> = all.iter().map(|e| e.event_id).collect();
    assert_eq!(unique, sent);
    let stats = db.stats();
    assert!(stats.segments >= 1, "shutdown must flush the memtable");
    assert_eq!(stats.memtable_rows, 0);
    assert!(db.verify().unwrap().is_empty());
}

fn hook(sb: &Sandbox, session: &str) -> HookOutcome {
    let payload = serde_json::json!({
        "hook_event_name": "PostToolUse",
        "session_id": session,
        "cwd": sb.project.to_string_lossy(),
        "tool_name": "Bash",
        "tool_use_id": "tu_1",
        "tool_input": {"command": "cargo test"},
        "tool_response": {"stdout": "ok"}
    });
    run_hook(HookInput {
        provider_id: "claude-code",
        event_hint: None,
        payload_bytes: serde_json::to_vec(&payload).unwrap(),
        cwd_hint: Some(sb.project.clone()),
        data_dir_override: Some(sb.data_dir.clone()),
        db_override: None,
    })
}

#[test]
fn hook_uses_daemon_when_running_and_spools_otherwise() {
    let sb = sandbox();
    let inbox = sb.locator.db_dir.join("spool").join("inbox.spool");

    let handle = start(&sb.locator);
    let out = hook(&sb, "s-hook-1");
    assert!(out.error.is_none(), "{:?}", out.error);
    assert_eq!(out.event_kind, "tool_call_finished");
    assert_eq!(out.delivered, Delivery::Daemon);
    assert!(out.spool_path.is_none());
    assert!(
        !inbox.exists(),
        "no spool file when the daemon acknowledged"
    );
    assert!(
        out.elapsed_us < 200_000,
        "hook took {} us with the daemon",
        out.elapsed_us
    );
    let st = daemon::status(&sb.locator).unwrap();
    assert_eq!(st.events_ingested, 1);
    stop(&sb.locator, handle);

    // No daemon: the spool path, and fast.
    let started = Instant::now();
    let out = hook(&sb, "s-hook-2");
    let wall = started.elapsed();
    assert!(out.error.is_none(), "{:?}", out.error);
    assert_eq!(out.delivered, Delivery::Spool);
    assert!(out.spool_path.is_some());
    assert!(inbox.exists());
    assert!(
        out.elapsed_us < 200_000,
        "hook took {} us without the daemon",
        out.elapsed_us
    );
    assert!(wall < Duration::from_millis(200), "hook wall time {wall:?}");

    // The restarted daemon imports the spool before it starts listening.
    let handle = start(&sb.locator);
    let st = daemon::status(&sb.locator).unwrap();
    assert!(st.spool_files_imported >= 1, "{st:?}");
    assert_eq!(st.spool_events_imported, 1);
    assert!(!st.spool_pending);
    assert!(!inbox.exists());
    // ... and keeps sweeping while running.
    let out = hook(&sb, "s-hook-3");
    assert_eq!(out.delivered, Delivery::Daemon);
    stop(&sb.locator, handle);

    let db = open_read_only(&sb.locator);
    let events = db.scan(&ScanFilter::default()).unwrap();
    assert_eq!(events.len(), 3);
    let sessions: HashSet<&str> = events
        .iter()
        .map(|e| e.provider_session_id.as_str())
        .collect();
    assert_eq!(
        sessions,
        HashSet::from(["s-hook-1", "s-hook-2", "s-hook-3"])
    );
    assert!(events.iter().all(|e| e.kind == EventKind::ToolCallFinished));
    let mut seqs: Vec<u64> = events.iter().map(|e| e.source_seq).collect();
    seqs.sort_unstable();
    assert_eq!(seqs, vec![1, 2, 3]);
}

#[test]
fn status_is_none_quickly_when_nothing_listens() {
    let sb = sandbox();
    let started = Instant::now();
    assert!(daemon::status(&sb.locator).is_none());
    assert!(!ipc::daemon_reachable(&sb.locator));
    assert!(matches!(
        daemon::probe(&sb.locator),
        daemon::Probe::NotRunning
    ));
    assert!(matches!(
        Client::send_events(&sb.locator, &[]),
        Err(IpcError::NotRunning)
    ));
    assert!(!daemon::stop(&sb.locator).unwrap());
    assert!(started.elapsed() < Duration::from_millis(100));

    // A stale socket file (daemon crashed) is "present" but connect fails
    // fast, and the next daemon start reclaims it.
    let sock = ipc::endpoint(&sb.locator)
        .socket_path()
        .unwrap()
        .to_path_buf();
    std::fs::create_dir_all(sock.parent().unwrap()).unwrap();
    drop(std::os::unix::net::UnixListener::bind(&sock).unwrap());
    assert!(sock.exists());
    std::fs::write(ipc::pid_path(&sb.locator), "4000000000\n").unwrap();
    let started = Instant::now();
    assert!(daemon::status(&sb.locator).is_none());
    assert!(matches!(
        Client::send_events(&sb.locator, &[]),
        Err(IpcError::NotRunning)
    ));
    assert!(started.elapsed() < Duration::from_millis(100));
    let handle = start(&sb.locator);
    assert!(daemon::status(&sb.locator).is_some());
    stop(&sb.locator, handle);
}

#[test]
fn service_definitions_render_for_this_locator() {
    let sb = sandbox();
    let plist = service::render_launchd_plist(&sb.locator, Path::new("/opt/it's <here>/attempt"));
    assert!(plist.contains("<string>dev.attemptdb.daemon</string>"));
    assert!(plist.contains("<string>/opt/it's &lt;here&gt;/attempt</string>"));
    assert!(plist.contains("<key>ATTEMPTDB_DATA_DIR</key>"));
    assert!(plist.contains("<key>RunAtLoad</key>\n\t<true/>"));
    let unit = service::render_systemd_unit(&sb.locator, Path::new("/opt/100%/attempt"));
    assert!(unit.contains("ExecStart=\"/opt/100%%/attempt\" daemon run"));
    assert!(unit.contains("Environment=\"ATTEMPTDB_DATA_DIR="));
    assert!(
        service::service_env(&sb.locator)
            .iter()
            .any(|(k, _)| k == "ATTEMPTDB_DATA_DIR")
    );
}

#[test]
fn query_is_refused_without_a_read_service_and_needs_hello() {
    let sb = sandbox();
    Database::create(&sb.locator.db_dir, DeviceId::new()).unwrap();
    let handle = start(&sb.locator);

    let req = ipc::ReadRequest {
        kind: ipc::ReadKind::Query,
        statement: Some("SELECT 1".into()),
        scope: ipc::ReadScope::default(),
        session_limit: None,
        all_sessions: false,
    };
    // Without HELLO the daemon does not know which database the client means.
    let mut c = ipc::Client::connect(&sb.locator, ipc::Timeouts::interactive()).unwrap();
    match c.query(&req) {
        Err(ipc::IpcError::Nack(n)) => assert_eq!(n.code, "hello_required"),
        other => panic!("expected hello_required, got {other:?}"),
    }
    // With HELLO but no read service installed: refused, not crashed.
    match ipc::Client::read(&sb.locator, &req) {
        Err(ipc::IpcError::Nack(n)) => assert_eq!(n.code, "read_unavailable"),
        other => panic!("expected read_unavailable, got {other:?}"),
    }
    // The connection is still good for what the daemon does serve.
    assert!(ipc::Client::status(&sb.locator).is_ok());
    stop(&sb.locator, handle);
}
