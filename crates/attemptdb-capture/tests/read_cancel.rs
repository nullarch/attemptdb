//! A `QUERY` whose client has gone away is cancelled.
//!
//! The CLI gives up on the daemon after 30 s and reads the database itself.
//! The daemon used to carry on: a statement that opened blobs for a quarter
//! of an hour at a fifth of a core, with nobody to answer. The daemon now
//! watches the connection while a read runs, and raises a cancel token the
//! read service honours when the client closes it. This file drives the
//! daemon with a stand-in read service that does nothing but wait for it.
//!
//! The engine's own use of the token (it drops the DataFusion plan) is
//! tested in `attempt`'s `read_service` unit tests.
#![cfg(unix)]

use attemptdb_capture::Locator;
use attemptdb_capture::daemon::{self, DaemonOptions, ReadCancel, ReadError, ReadService};
use attemptdb_capture::ipc::{
    self, Client, Frame, Hello, MsgType, PROTOCOL_VERSION, ReadKind, ReadRequest, ReadResponse,
    Timeouts,
};
use attemptdb_core::DeviceId;
use attemptdb_storage::Database;
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// Waits until it is cancelled (or `give_up`), and records what happened.
#[derive(Debug, Default)]
struct Waiter {
    started: AtomicUsize,
    cancelled: AtomicBool,
    completed: AtomicBool,
    /// How long a read waits before it answers by itself.
    give_up: Duration,
}

impl ReadService for Waiter {
    fn refresh(&self, _db: &Database) -> Result<(), String> {
        Ok(())
    }

    fn handle(
        &self,
        _req: ReadRequest,
        _rt: &tokio::runtime::Handle,
        cancel: &ReadCancel,
    ) -> Result<ReadResponse, ReadError> {
        self.started.fetch_add(1, Ordering::SeqCst);
        let began = Instant::now();
        loop {
            if cancel.is_cancelled() {
                self.cancelled.store(true, Ordering::SeqCst);
                return Err(ReadError::cancelled());
            }
            if began.elapsed() >= self.give_up {
                self.completed.store(true, Ordering::SeqCst);
                return Ok(ReadResponse {
                    event_count: 7,
                    ..Default::default()
                });
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

struct Sandbox {
    _tmp: tempfile::TempDir,
    locator: Locator,
}

fn sandbox() -> Sandbox {
    let tmp = tempfile::Builder::new().prefix("atcan").tempdir().unwrap();
    let project = tmp.path().join("proj");
    std::fs::create_dir_all(&project).unwrap();
    let locator = Locator::resolve(&project, Some(&tmp.path().join("data")), None);
    Database::create(&locator.db_dir, DeviceId::new()).unwrap();
    Sandbox { _tmp: tmp, locator }
}

fn start(
    locator: &Locator,
    service: Arc<Waiter>,
) -> std::thread::JoinHandle<attemptdb_capture::Result<()>> {
    let l = locator.clone();
    let handle = std::thread::spawn(move || {
        daemon::run(
            &l,
            DaemonOptions {
                read_service: Some(service),
                spool_interval: Duration::from_millis(100),
                ..Default::default()
            },
        )
    });
    daemon::wait_until_running(locator, Duration::from_secs(15)).expect("daemon did not start");
    handle
}

fn request() -> ReadRequest {
    ReadRequest {
        kind: ReadKind::Query,
        statement: Some("SELECT 1".into()),
        scope: Default::default(),
        session_limit: None,
        all_sessions: false,
    }
}

fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_client_that_gives_up_cancels_the_read() {
    let sb = sandbox();
    let service = Arc::new(Waiter {
        give_up: Duration::from_secs(60),
        ..Default::default()
    });
    let handle = start(&sb.locator, service.clone());

    // The client's budget runs out while the read is still going.
    let started = Instant::now();
    let outcome = {
        let mut client = Client::connect(
            &sb.locator,
            Timeouts {
                connect: Duration::from_millis(250),
                roundtrip: Duration::from_millis(400),
            },
        )
        .unwrap();
        client
            .hello(&Hello::new("cli", &sb.locator.db_dir, None))
            .unwrap();
        client.query(&request())
    };
    assert!(outcome.is_err(), "the read cannot have finished in 400 ms");
    assert!(started.elapsed() < Duration::from_secs(5));

    // The client is gone; the daemon notices and the read is told to stop,
    // long before its own 60 s.
    wait_for("the read to be cancelled", || {
        service.cancelled.load(Ordering::SeqCst)
    });
    assert!(!service.completed.load(Ordering::SeqCst));
    assert_eq!(service.started.load(Ordering::SeqCst), 1);

    // The daemon is fine and answers the next client.
    assert!(daemon::status(&sb.locator).is_some());
    assert!(daemon::stop(&sb.locator).unwrap());
    handle.join().unwrap().unwrap();
}

#[test]
fn a_read_that_is_waited_for_is_answered_and_a_pipelined_request_is_not_lost() {
    let sb = sandbox();
    let service = Arc::new(Waiter {
        give_up: Duration::from_millis(300),
        ..Default::default()
    });
    let handle = start(&sb.locator, service.clone());

    // HELLO, QUERY and PING in one write: the daemon reads one byte of the
    // PING while it waits for the read, and must still answer all three in
    // order.
    let path = ipc::endpoint(&sb.locator)
        .socket_path()
        .unwrap()
        .to_path_buf();
    let mut stream = UnixStream::connect(path).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let hello = Frame::json(MsgType::Hello, &Hello::new("cli", &sb.locator.db_dir, None)).unwrap();
    let query = Frame::json(MsgType::Query, &request()).unwrap();
    let ping = Frame::empty(MsgType::Ping);
    let mut buf = Vec::new();
    buf.extend_from_slice(&ipc::encode_prelude(PROTOCOL_VERSION, 0));
    hello.encode_into(&mut buf);
    query.encode_into(&mut buf);
    ping.encode_into(&mut buf);
    stream.write_all(&buf).unwrap();
    stream.flush().unwrap();

    let first = Frame::read_from(&mut stream).unwrap();
    assert_eq!(first.kind(), Some(MsgType::HelloAck));
    let second = Frame::read_from(&mut stream).unwrap();
    assert_eq!(second.kind(), Some(MsgType::Result), "{}", second.msg_type);
    let result: ReadResponse = second.parse_json().unwrap();
    assert_eq!(result.event_count, 7);
    let third = Frame::read_from(&mut stream).unwrap();
    assert_eq!(third.kind(), Some(MsgType::Pong), "{}", third.msg_type);
    assert!(service.completed.load(Ordering::SeqCst));
    assert!(!service.cancelled.load(Ordering::SeqCst));
    drop(stream);

    assert!(daemon::stop(&sb.locator).unwrap());
    handle.join().unwrap().unwrap();
}
