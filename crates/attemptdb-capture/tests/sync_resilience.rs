//! The uploader against servers that say no: one event too large, one the
//! server cannot read, a server that is down, one that refuses everything —
//! and what the person agreed to (history before consent), what leaves the
//! device (paths, text), and how a device ends its relationship with a server
//! (forget, revoke). Stub servers for the refusals, the real server for the
//! rest.

#![cfg(unix)]

use attemptdb_capture::ingest;
use attemptdb_capture::locator::Locator;
use attemptdb_capture::sync::{
    Consent, ForgetReport, InferenceItem, InferenceSet, InferenceSource, MAX_QUARANTINE_STREAK,
    PeerConfig, RevokeOutcome, SyncProfile, SyncState, UploadOptions, UploadReport, forget_remote,
    handshake, local_newest_seq, prepare_for_upload, retry_set_aside, revoke_key, scrub_paths,
    upload_once, upload_once_opts,
};
use attemptdb_core::event::{EventContent, Provider};
use attemptdb_core::{
    CaptureMode, DeviceId, Event, EventKind, PortablePath, ProjectRef, Timestamp,
};
use attemptdb_server::auth::digest_hex;
use attemptdb_server::{Server, ServerConfig};
use attemptdb_storage::{Database, OpenOptions, ScanFilter};
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

const KEY: &str = "device-key-resilience";

fn events(device: DeviceId, n: usize, tag: &str) -> Vec<Event> {
    (0..n)
        .map(|i| {
            let mut ev = Event::new(
                device,
                Provider::ClaudeCode,
                "PostToolUse",
                EventKind::ToolCallFinished,
                ProjectRef::derive("/Users/alice/work/repo", None, &device),
                format!("session-{tag}"),
                CaptureMode::LocalSemantic,
                "resilience/0.1",
            );
            ev.attrs.insert("x_test_index".into(), json!(i));
            ev.paths = vec![PortablePath::from_raw(
                &format!("/Users/alice/work/repo/src/file{i}.rs"),
                Some("/Users/alice/work/repo"),
            )];
            ev.content = Some(EventContent {
                command: Some(format!("echo {tag} {i}")),
                ..Default::default()
            });
            ev
        })
        .collect()
}

fn local_db(root: &Path) -> (Locator, DeviceId) {
    let locator = Locator::resolve(root, Some(root), None);
    let db = ingest::open_writer(&locator, true).unwrap();
    let device = db.device_id();
    drop(db);
    (locator, device)
}

fn write_events(locator: &Locator, evs: Vec<Event>) {
    let mut db = ingest::open_writer(locator, false).unwrap();
    db.ingest(evs).unwrap();
}

fn cfg(url: &str, batch: usize) -> PeerConfig {
    PeerConfig {
        batch_events: batch,
        interval_secs: 5,
        inference_interval_secs: 0,
        ..PeerConfig::new(url, KEY)
    }
}

fn state(locator: &Locator) -> SyncState {
    SyncState::load(&SyncState::path(
        &locator.paths.data_dir,
        &locator.db_dir,
        "default",
    ))
    .unwrap()
}

fn run(locator: &Locator, c: &PeerConfig) -> anyhow::Result<UploadReport> {
    upload_once(locator, "default", c)
}

// ---------------------------------------------------------------------------
// A stub server
// ---------------------------------------------------------------------------

/// One request as the stub saw it.
#[derive(Clone, Debug)]
struct Seen {
    path: String,
    body: Value,
    len: usize,
}

struct Stub {
    url: String,
    requests: Arc<AtomicUsize>,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl Stub {
    fn count(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }

    /// Event ids the stub answered 200 to, in the order it did.
    fn accepted_ids(&self) -> Vec<String> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .filter(|s| s.path == "/v1/sync")
            .filter(|s| s.body["events"].as_array().is_some_and(|e| !e.is_empty()))
            .flat_map(|s| {
                s.body["events"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|e| e["event_id"].as_str().unwrap().to_string())
                    .collect::<Vec<_>>()
            })
            .collect()
    }
}

/// An HTTP/1.1 server that answers each request with `rule(path, body,
/// byte length, request number)` → `(status, JSON body)`. `rule` decides; a
/// 200 is whatever it returns, so the rule builds the acknowledgement.
fn stub(rule: impl Fn(&str, &Value, usize, usize) -> (u16, Value) + Send + Sync + 'static) -> Stub {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let requests = Arc::new(AtomicUsize::new(0));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let rule = Arc::new(rule);
    let (r2, s2) = (Arc::clone(&requests), Arc::clone(&seen));
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut sock) = conn else { return };
            let (rule, requests, seen) = (Arc::clone(&rule), Arc::clone(&r2), Arc::clone(&s2));
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                let mut tmp = [0u8; 16384];
                let (head_end, content_length) = loop {
                    let n = sock.read(&mut tmp).unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                    if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&buf[..i]).to_string();
                        let cl = head
                            .lines()
                            .find_map(|l| {
                                let (k, v) = l.split_once(':')?;
                                k.eq_ignore_ascii_case("content-length")
                                    .then(|| v.trim().parse::<usize>().ok())
                                    .flatten()
                            })
                            .unwrap_or(0);
                        break (i + 4, cl);
                    }
                };
                while buf.len() < head_end + content_length {
                    let n = sock.read(&mut tmp).unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                }
                let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
                let path = head
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or_default()
                    .to_string();
                let body: Value = serde_json::from_slice(&buf[head_end..head_end + content_length])
                    .unwrap_or(Value::Null);
                let n = requests.fetch_add(1, Ordering::SeqCst);
                let (status, reply) = rule(&path, &body, content_length, n);
                if status == 200 {
                    seen.lock().unwrap().push(Seen {
                        path,
                        body,
                        len: content_length,
                    });
                }
                let text = reply.to_string();
                let resp = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
                    text.len()
                );
                let _ = sock.write_all(resp.as_bytes());
            });
        }
    });
    Stub {
        url,
        requests,
        seen,
    }
}

fn ack(body: &Value) -> Value {
    json!({
        "sync_version": 1,
        "batch_id": body["batch_id"],
        "accepted": body["events"].as_array().map_or(0, Vec::len),
        "duplicates": 0, "rejected": [], "redactions": 0, "stripped_content": 0
    })
}

fn has_attr(body: &Value, attr: &str) -> bool {
    body["events"]
        .as_array()
        .is_some_and(|evs| evs.iter().any(|e| e["attrs"].get(attr).is_some()))
}

fn mark(evs: &mut [Event], index: usize, attr: &str) {
    evs[index].attrs.insert(attr.into(), json!(true));
}

// ---------------------------------------------------------------------------
// One bad event must not wedge the cursor
// ---------------------------------------------------------------------------

#[test]
fn an_event_too_large_for_the_server_is_skipped_with_a_record_and_the_cursor_moves_on() {
    let tmp = tempfile::tempdir().unwrap();
    let (locator, device) = local_db(tmp.path());
    let mut evs = events(device, 6, "big");
    mark(&mut evs, 3, "x_test_huge");
    let huge_id = evs[3].event_id.to_string();
    write_events(&locator, evs);
    // 413 whenever the huge event is in the body, however alone.
    let server = stub(|_, body, _, _| {
        if has_attr(body, "x_test_huge") {
            (413, json!({"error": "body too large"}))
        } else {
            (200, ack(body))
        }
    });
    let c = cfg(&server.url, 6);

    let r = run(&locator, &c).unwrap();
    assert_eq!(r.accepted, 5, "{r:?}");
    assert_eq!((r.quarantined, r.content_withheld), (1, 0), "{r:?}");
    assert_eq!(r.cursor, 6, "the cursor is past the event that cannot go");
    let s = state(&locator);
    assert_eq!(s.last_acked_source_seq, 6);
    assert_eq!((s.quarantined, s.quarantine.len()), (1, 1));
    let rec = &s.quarantine[0];
    assert_eq!(rec.event_id.to_string(), huge_id);
    assert_eq!((rec.status, rec.action.as_str()), (413, "skipped"));
    assert!(
        rec.reason.contains("larger than the server accepts"),
        "{}",
        rec.reason
    );
    assert!(
        !serde_json::to_string(&s).unwrap().contains("echo big"),
        "the record names the event, never its content"
    );
    // The five others arrived, in order, once each.
    let ids = server.accepted_ids();
    assert_eq!(ids.len(), 5);
    assert!(!ids.contains(&huge_id));
    // Found by halving: nowhere near one request per event.
    assert!(server.count() <= 8, "{} requests", server.count());
    // The next run has nothing to do: the stream is not wedged.
    let before = server.count();
    let r = run(&locator, &c).unwrap();
    assert_eq!((r.pending_before, r.quarantined), (0, 0));
    assert_eq!(server.count(), before);
}

#[test]
fn bisecting_finds_the_one_event_the_server_cannot_read_and_keeps_order() {
    let tmp = tempfile::tempdir().unwrap();
    let (locator, device) = local_db(tmp.path());
    let mut evs = events(device, 16, "poison");
    mark(&mut evs, 10, "x_test_poison");
    let all: Vec<String> = evs.iter().map(|e| e.event_id.to_string()).collect();
    write_events(&locator, evs);
    let server = stub(|_, body, _, _| {
        if has_attr(body, "x_test_poison") {
            (422, json!({"error": "unknown variant `some_new_kind`"}))
        } else {
            (200, ack(body))
        }
    });
    let r = run(&locator, &cfg(&server.url, 16)).unwrap();
    assert_eq!((r.accepted, r.quarantined, r.cursor), (15, 1, 16), "{r:?}");
    let mut expected = all.clone();
    let poisoned = expected.remove(10);
    assert_eq!(server.accepted_ids(), expected, "everything else, in order");
    let s = state(&locator);
    assert_eq!(s.quarantine[0].event_id.to_string(), poisoned);
    assert_eq!(s.quarantine[0].status, 422);
    assert!(s.quarantine[0].reason.contains("unknown variant"));
    // 1 + 2*log2(16) requests at most, plus slack for the halves that fit.
    assert!(server.count() <= 11, "{} requests", server.count());
}

#[test]
fn text_the_server_refuses_is_withheld_and_the_metadata_still_goes() {
    let tmp = tempfile::tempdir().unwrap();
    let (locator, device) = local_db(tmp.path());
    let mut evs = events(device, 3, "talk");
    evs[1].kind = EventKind::PromptSubmitted;
    evs[1].content = Some(EventContent {
        prompt: Some("x".repeat(200_000)),
        ..Default::default()
    });
    let big_id = evs[1].event_id.to_string();
    write_events(&locator, evs);
    // A server whose limit is 100 kB.
    let server = stub(|_, body, len, _| {
        if len > 100_000 {
            (413, json!({"error": "body too large"}))
        } else {
            (200, ack(body))
        }
    });
    let mut c = cfg(&server.url, 3);
    c.set_profile(SyncProfile::Messages);
    let r = run(&locator, &c).unwrap();
    assert_eq!(r.accepted, 3, "all three arrived: {r:?}");
    assert_eq!((r.quarantined, r.content_withheld), (1, 1), "{r:?}");
    let s = state(&locator);
    assert_eq!(s.quarantine[0].action, "content_withheld");
    assert_eq!(s.quarantine[0].event_id.to_string(), big_id);
    assert_eq!(s.quarantine_streak, 0, "the server accepted its metadata");
    // The prompt itself never reached the stub.
    let seen = server.seen.lock().unwrap();
    for s in seen.iter() {
        assert!(s.len < 100_000);
    }
    let sent_big = seen
        .iter()
        .flat_map(|s| s.body["events"].as_array().unwrap().clone())
        .find(|e| e["event_id"] == json!(big_id))
        .unwrap();
    assert!(sent_big.get("content").is_none() || sent_big["content"].is_null());
}

#[test]
fn server_trouble_and_client_trouble_are_never_blamed_on_the_events() {
    let tmp = tempfile::tempdir().unwrap();
    let (locator, device) = local_db(tmp.path());
    write_events(&locator, events(device, 4, "t"));
    let mode = Arc::new(AtomicUsize::new(0));
    let m = Arc::clone(&mode);
    let server = stub(move |_, body, _, _| match m.load(Ordering::SeqCst) {
        0 => (500, json!({"error": "ingest failed"})),
        1 => (503, json!({"error": "storage trouble"})),
        2 => (429, json!({"error": "rate limit exceeded"})),
        3 => (401, json!({"error": "missing or unknown bearer key"})),
        4 => (
            403,
            json!({"error": "batch device_id does not match the key's device"}),
        ),
        5 => (
            400,
            json!({"error": "sync_version 2 not supported (server speaks 1)"}),
        ),
        _ => (200, ack(body)),
    });
    let c = cfg(&server.url, 4);
    for step in 0..6 {
        mode.store(step, Ordering::SeqCst);
        let err = run(&locator, &c).unwrap_err().to_string();
        assert!(err.contains("cursor kept at 0"), "step {step}: {err}");
        let s = state(&locator);
        assert_eq!(s.last_acked_source_seq, 0, "step {step}");
        assert_eq!((s.quarantined, s.quarantine.len()), (0, 0), "step {step}");
        assert_eq!(
            s.failures,
            step as u32 + 1,
            "step {step}: failures count up"
        );
        assert!(s.last_error.is_some());
    }
    // Healthy again: everything goes, and the failure count resets.
    mode.store(9, Ordering::SeqCst);
    let r = run(&locator, &c).unwrap();
    assert_eq!((r.accepted, r.cursor), (4, 4));
    let s = state(&locator);
    assert_eq!((s.failures, s.last_error), (0, None));
}

#[test]
fn a_server_that_refuses_everything_stops_the_skipping() {
    let tmp = tempfile::tempdir().unwrap();
    let (locator, device) = local_db(tmp.path());
    write_events(&locator, events(device, 40, "all"));
    let server = stub(|_, _, _, _| (422, json!({"error": "unprocessable"})));
    let c = cfg(&server.url, 40);
    let err = run(&locator, &c).unwrap_err().to_string();
    assert!(err.contains("refused 25 events in a row"), "{err}");
    let s = state(&locator);
    assert_eq!(s.quarantined as u32, MAX_QUARANTINE_STREAK);
    assert_eq!(s.last_acked_source_seq, 25, "25 skipped, the 26th held");
    // The streak is persisted: the next run does not skip another one.
    let err = run(&locator, &c).unwrap_err().to_string();
    assert!(err.contains("refused 25 events in a row"), "{err}");
    let s = state(&locator);
    assert_eq!((s.quarantined, s.last_acked_source_seq), (25, 25));
}

// ---------------------------------------------------------------------------
// Policy and transport are checked before anything is sent
// ---------------------------------------------------------------------------

#[test]
fn an_exclude_entry_that_names_nothing_sends_nothing_and_says_so() {
    let tmp = tempfile::tempdir().unwrap();
    let (locator, device) = local_db(tmp.path());
    write_events(&locator, events(device, 2, "p"));
    let server = stub(|_, body, _, _| (200, ack(body)));
    let mut c = cfg(&server.url, 10);
    c.exclude = vec!["acme/private".into()];
    let err = run(&locator, &c).unwrap_err().to_string();
    assert!(
        err.contains("`acme/private`") && err.contains("nothing is uploaded"),
        "{err}"
    );
    assert_eq!(server.count(), 0, "not a byte left");
    let s = state(&locator);
    assert!(s.last_error.as_deref().unwrap().contains("acme/private"));
    assert_eq!(s.last_acked_source_seq, 0);
}

#[test]
fn a_plain_http_host_in_the_config_is_not_spoken_to() {
    let tmp = tempfile::tempdir().unwrap();
    let (locator, device) = local_db(tmp.path());
    write_events(&locator, events(device, 1, "h"));
    let c = cfg("http://203.0.113.9:8787", 10);
    let err = run(&locator, &c).unwrap_err().to_string();
    assert!(err.contains("--allow-insecure-http"), "{err}");
    let err = handshake(&locator, &c).unwrap_err().to_string();
    assert!(err.contains("--allow-insecure-http"), "{err}");
    assert!(
        matches!(revoke_key(&c), RevokeOutcome::Unreachable(m) if m.contains("--allow-insecure-http"))
    );
    assert!(forget_remote(&c).is_err());
}

// ---------------------------------------------------------------------------
// Consent: history before it is not uploaded
// ---------------------------------------------------------------------------

#[test]
fn history_from_before_consent_stays_on_the_device_unless_asked_for() {
    let now = Timestamp::now().as_micros();
    let hour = 3_600_000_000;
    let consent_at = Timestamp::from_micros(now - hour / 2);
    let build = |tag: &str| {
        let tmp = tempfile::tempdir().unwrap();
        let (locator, device) = local_db(tmp.path());
        let mut evs = events(device, 5, tag);
        for (i, e) in evs.iter_mut().enumerate() {
            // Three events from two hours ago, two from the last ten minutes.
            let at = if i < 3 {
                now - 2 * hour
            } else {
                now - hour / 6
            };
            e.observed_at = Timestamp::from_micros(at);
        }
        write_events(&locator, evs);
        (tmp, locator)
    };
    let server = stub(|_, body, _, _| (200, ack(body)));
    let consent = |history_before| Consent {
        at: consent_at,
        profile: SyncProfile::Semantic,
        include: vec![],
        exclude: vec![],
        history_before,
        history_before_seq: None,
    };

    // Default: only what happened after the person connected.
    let (_keep, locator) = build("a");
    let mut c = cfg(&server.url, 10);
    c.consent = Some(consent(Some(consent_at)));
    let r = run(&locator, &c).unwrap();
    assert_eq!((r.accepted, r.before_consent, r.cursor), (2, 3, 5), "{r:?}");
    assert_eq!(state(&locator).before_consent, 3);
    // Not re-examined, not uploaded later.
    let r = run(&locator, &c).unwrap();
    assert_eq!((r.pending_before, r.before_consent), (0, 0));

    // `--include-history`: no watermark, everything goes.
    let (_keep2, locator2) = build("b");
    let mut c = cfg(&server.url, 10);
    c.consent = Some(consent(None));
    let r = run(&locator2, &c).unwrap();
    assert_eq!((r.accepted, r.before_consent), (5, 0), "{r:?}");

    // A peer configured before consent was recorded behaves as it always did.
    let (_keep3, locator3) = build("c");
    let r = run(&locator3, &cfg(&server.url, 10)).unwrap();
    assert_eq!(r.accepted, 5);

    // Only old events pending: nothing is sent, and the cursor still moves.
    let tmp = tempfile::tempdir().unwrap();
    let (locator4, device) = local_db(tmp.path());
    let mut evs = events(device, 2, "d");
    for e in &mut evs {
        e.observed_at = Timestamp::from_micros(now - 3 * hour);
    }
    write_events(&locator4, evs);
    let before = server.count();
    let mut c = cfg(&server.url, 10);
    c.consent = Some(consent(Some(consent_at)));
    let r = run(&locator4, &c).unwrap();
    assert_eq!((r.accepted, r.before_consent, r.cursor), (0, 2, 2), "{r:?}");
    assert_eq!(
        server.count(),
        before,
        "no request for events that may not go"
    );
}

// ---------------------------------------------------------------------------
// Against the real server: paths, forgetting, revoking
// ---------------------------------------------------------------------------

struct Real {
    url: String,
    data_dir: PathBuf,
    keys_file: PathBuf,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<anyhow::Result<()>>,
}

async fn real_server(root: &Path, device: DeviceId, ceiling: CaptureMode) -> Real {
    let keys_file = root.join("keys.json");
    std::fs::write(
        &keys_file,
        json!({"keys": [{"sha256": digest_hex(KEY), "tenant": "t1", "device_id": device}]})
            .to_string(),
    )
    .unwrap();
    let data_dir = root.join("server-data");
    let server = Server::bind(ServerConfig {
        port: 0,
        data_dir: data_dir.clone(),
        keys_file: keys_file.clone(),
        capture_mode: ceiling,
        ..Default::default()
    })
    .await
    .unwrap();
    let url = format!("http://{}", server.addr());
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let task = tokio::spawn(server.run(async move {
        let _ = rx.await;
    }));
    Real {
        url,
        data_dir,
        keys_file,
        stop: Some(tx),
        task,
    }
}

impl Real {
    async fn stop(mut self) {
        let _ = self.stop.take().unwrap().send(());
        let _ = self.task.await;
    }

    fn stored(&self) -> Vec<Event> {
        let db = Database::open(
            &self.data_dir.join("tenants").join("t1"),
            OpenOptions {
                read_only: true,
                ..Default::default()
            },
        )
        .unwrap();
        db.scan(&ScanFilter::default()).unwrap()
    }
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    tokio::task::spawn_blocking(f).await.unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_profile_short_of_full_sends_a_home_directory_to_the_server() {
    for profile in [
        SyncProfile::MetadataOnly,
        SyncProfile::Semantic,
        SyncProfile::Messages,
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let (locator, device) = local_db(tmp.path());
        let mut evs = events(device, 3, "paths");
        evs[0].kind = EventKind::PromptSubmitted;
        evs[0].content = Some(EventContent {
            prompt: Some("fix the build".into()),
            ..Default::default()
        });
        write_events(&locator, evs);
        let server = real_server(tmp.path(), device, CaptureMode::LocalSemantic).await;
        let mut c = cfg(&server.url, 10);
        c.set_profile(profile);
        let (l, cc) = (locator.clone(), c.clone());
        let r = blocking(move || run(&l, &cc)).await.unwrap();
        assert_eq!(r.accepted, 3, "{profile}");
        let stored = server.stored();
        let text = serde_json::to_string(&stored).unwrap();
        assert!(!text.contains("alice"), "{profile}: {text}");
        assert!(!text.contains("/Users/"), "{profile}: {text}");
        for e in &stored {
            assert_eq!(e.project.root, "~/work/repo", "{profile}");
            assert_eq!(
                e.paths[0].logical,
                format!("src/file{}.rs", e.attrs["x_test_index"])
            );
        }
        server.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_device_deletes_what_it_uploaded_and_then_revokes_its_key() {
    let tmp = tempfile::tempdir().unwrap();
    let (locator, device) = local_db(tmp.path());
    write_events(&locator, events(device, 6, "bye"));
    let server = real_server(tmp.path(), device, CaptureMode::LocalSemantic).await;
    let mut c = cfg(&server.url, 10);
    c.set_profile(SyncProfile::Semantic);
    let (l, cc) = (locator.clone(), c.clone());
    assert_eq!(blocking(move || run(&l, &cc)).await.unwrap().accepted, 6);
    assert_eq!(server.stored().len(), 6);

    let cc = c.clone();
    let report: ForgetReport = blocking(move || forget_remote(&cc)).await.unwrap();
    assert_eq!(report.events_deleted, 6, "{report:?}");
    assert!(
        report.not_reached.iter().any(|s| s.contains("webhook")),
        "the server says what a deletion does not reach: {report:?}"
    );
    let left = server.stored();
    assert_eq!(left.len(), 1, "only the deletion record remains");
    assert_eq!(left[0].kind, EventKind::ConfigChanged);
    // The local database is untouched, and the cursor did not move: what was
    // forgotten is not uploaded again.
    assert_eq!(state(&locator).last_acked_source_seq, 6);
    let r = blocking({
        let (l, cc) = (locator.clone(), c.clone());
        move || run(&l, &cc)
    })
    .await
    .unwrap();
    assert_eq!(r.pending_before, 0);

    // Revoke: the key stops working, and the file no longer lists it.
    let cc = c.clone();
    assert_eq!(
        blocking(move || revoke_key(&cc)).await,
        RevokeOutcome::Revoked
    );
    let (l, cc) = (locator.clone(), c.clone());
    let err = blocking(move || handshake(&l, &cc))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("401"), "{err}");
    let on_disk = std::fs::read_to_string(&server.keys_file).unwrap();
    assert!(!on_disk.contains(&digest_hex(KEY)));
    let cc = c.clone();
    assert_eq!(
        blocking(move || revoke_key(&cc)).await,
        RevokeOutcome::AlreadyGone,
        "a second revoke finds nothing"
    );
    let cc = c.clone();
    assert!(
        blocking(move || forget_remote(&cc))
            .await
            .unwrap_err()
            .to_string()
            .contains("401")
    );
    server.stop().await;
}

#[test]
fn an_older_server_and_an_absent_one_are_reported_as_what_they_are() {
    let older = stub(|_, _, _, _| (404, json!({"error": "not found"})));
    let c = cfg(&older.url, 10);
    assert_eq!(revoke_key(&c), RevokeOutcome::Unsupported);
    let err = forget_remote(&c).unwrap_err().to_string();
    assert!(err.contains("older server"), "{err}");
    let refusing = stub(|_, _, _, _| (403, json!({"error": "a reader key cannot do this"})));
    let c = cfg(&refusing.url, 10);
    assert!(matches!(revoke_key(&c), RevokeOutcome::Refused(403, m) if m.contains("reader")));
    // Nothing listening.
    let c = cfg("http://127.0.0.1:1", 10);
    assert!(matches!(revoke_key(&c), RevokeOutcome::Unreachable(_)));
    assert!(
        forget_remote(&c)
            .unwrap_err()
            .to_string()
            .contains("cannot reach")
    );
}

// ---------------------------------------------------------------------------
// The consent watermark is a sequence, not a clock
// ---------------------------------------------------------------------------

/// A database that held three events when the person connected (the machine's
/// clock was an hour ahead then), then got two live events after the clock was
/// set back two hours, a transcript import and a VibeMon export row.
fn watermark_db(root: &Path) -> (Locator, Vec<Event>) {
    let now = Timestamp::now().as_micros();
    let hour = 3_600_000_000i64;
    let (locator, device) = local_db(root);
    let mut before = events(device, 3, "before");
    for e in &mut before {
        e.observed_at = Timestamp::from_micros(now + hour);
    }
    write_events(&locator, before);
    let mut after = events(device, 2, "after");
    for e in &mut after {
        e.observed_at = Timestamp::from_micros(now - 2 * hour);
    }
    let mut imported = events(device, 2, "imported");
    imported[0]
        .attrs
        .insert("reconstructed".into(), json!(true));
    imported[1]
        .attrs
        .insert("x_vibemon_import".into(), json!("hook_events"));
    for e in &mut imported {
        e.observed_at = Timestamp::from_micros(now - 30 * 24 * hour);
    }
    let live = after.clone();
    let mut later = after;
    later.extend(imported);
    write_events(&locator, later);
    (locator, live)
}

#[test]
fn the_watermark_is_a_sequence_so_a_clock_set_back_withholds_nothing_new() {
    let tmp = tempfile::tempdir().unwrap();
    let (locator, live) = watermark_db(tmp.path());
    let server = stub(|_, body, _, _| (200, ack(body)));
    let consent_at = Timestamp::now();
    let consent = |seq: Option<u64>| Consent {
        at: consent_at,
        profile: SyncProfile::Semantic,
        include: vec![],
        exclude: vec![],
        history_before: Some(consent_at),
        history_before_seq: seq,
    };

    // With the sequence: what the database held (seq 1..=3) stays local
    // whatever its timestamps say, the two live events go although their
    // timestamps are in the past, and both imports (a transcript, an export)
    // are history read in after connecting: withheld, for the explicit command.
    let mut c = cfg(&server.url, 10);
    c.consent = Some(consent(Some(3)));
    let r = run(&locator, &c).unwrap();
    assert_eq!((r.accepted, r.before_consent, r.cursor), (2, 5, 7), "{r:?}");
    let mut sent = server.accepted_ids();
    sent.sort();
    let mut want: Vec<String> = live.iter().map(|e| e.event_id.to_string()).collect();
    want.sort();
    assert_eq!(sent, want, "exactly the live events");
    assert_eq!(state(&locator).before_consent, 5);

    // The time alone (a `sync.json` from before the sequence existed) gets
    // all of it wrong with this clock: the new events are "from before" the
    // connection and the old ones, stamped in the future, are not.
    let tmp2 = tempfile::tempdir().unwrap();
    let (locator2, _) = watermark_db(tmp2.path());
    let before = server.count();
    let mut c = cfg(&server.url, 10);
    c.consent = Some(consent(None));
    let r = run(&locator2, &c).unwrap();
    assert_eq!(r.accepted, 3, "the legacy rule: {r:?}");
    assert!(server.count() > before);
}

#[test]
fn a_watermark_can_move_forward_and_be_cleared_and_never_moves_back() {
    let at = Timestamp::now();
    let mut c = Consent {
        at,
        profile: SyncProfile::Semantic,
        include: vec![],
        exclude: vec![],
        history_before: Some(at),
        history_before_seq: Some(10),
    };
    let later = Timestamp::from_micros(at.as_micros() + 1_000_000);
    c.advance_watermark(later, 25);
    assert_eq!(
        (c.history_before, c.history_before_seq),
        (Some(later), Some(25))
    );
    c.advance_watermark(at, 5);
    assert_eq!(
        (c.history_before, c.history_before_seq),
        (Some(later), Some(25))
    );
    assert!(c.has_watermark());
    c.clear_watermark();
    assert!(!c.has_watermark());
}

#[test]
fn the_withheld_count_is_taken_once_the_cursor_has_passed_them() {
    let tmp = tempfile::tempdir().unwrap();
    let (locator, device) = local_db(tmp.path());
    write_events(&locator, events(device, 3, "old"));
    let seq = local_newest_seq(&locator).unwrap();
    write_events(&locator, events(device, 2, "new"));
    // The first request fails, the rest are answered.
    let server = stub(|_, body, _, n| {
        if n == 0 {
            (503, json!({"error": "ingest failed"}))
        } else {
            (200, ack(body))
        }
    });
    let at = Timestamp::now();
    let mut c = cfg(&server.url, 10);
    c.consent = Some(Consent {
        at,
        profile: SyncProfile::Semantic,
        include: vec![],
        exclude: vec![],
        history_before: Some(at),
        history_before_seq: Some(seq),
    });
    run(&locator, &c).unwrap_err();
    assert_eq!(
        state(&locator).before_consent,
        0,
        "a run that did not get past them has counted nothing"
    );
    let r = run(&locator, &c).unwrap();
    assert_eq!((r.accepted, r.before_consent), (2, 3), "{r:?}");
    assert_eq!(state(&locator).before_consent, 3, "counted once, not twice");
    let r = run(&locator, &c).unwrap();
    assert_eq!(r.before_consent, 0);
    assert_eq!(state(&locator).before_consent, 3);
}

#[test]
fn failing_in_the_middle_counts_the_withheld_events_the_cursor_already_passed() {
    let tmp = tempfile::tempdir().unwrap();
    let (locator, device) = local_db(tmp.path());
    write_events(&locator, events(device, 2, "old"));
    let seq = local_newest_seq(&locator).unwrap();
    write_events(&locator, events(device, 4, "new"));
    // Batches of two: the first goes, the second fails.
    let server = stub(|_, body, _, n| {
        if n == 1 {
            (503, json!({"error": "ingest failed"}))
        } else {
            (200, ack(body))
        }
    });
    let at = Timestamp::now();
    let mut c = cfg(&server.url, 2);
    c.consent = Some(Consent {
        at,
        profile: SyncProfile::Semantic,
        include: vec![],
        exclude: vec![],
        history_before: Some(at),
        history_before_seq: Some(seq),
    });
    run(&locator, &c).unwrap_err();
    let s = state(&locator);
    assert_eq!(
        s.last_acked_source_seq, 4,
        "the first batch of new events went"
    );
    assert_eq!(
        s.before_consent, 2,
        "the two old events are behind the cursor now"
    );
    let r = run(&locator, &c).unwrap();
    assert_eq!((r.accepted, r.before_consent), (2, 0), "{r:?}");
    assert_eq!(state(&locator).before_consent, 2);
}

// ---------------------------------------------------------------------------
// The idle tick, and the inference recompute
// ---------------------------------------------------------------------------

fn inference_server() -> Stub {
    stub(|path, body, _, _| {
        if path == "/v1/sync/inferences" {
            (
                200,
                json!({"stored": body["items"].as_array().map_or(0, Vec::len), "rejected": []}),
            )
        } else {
            (200, ack(body))
        }
    })
}

/// A source that counts how often it is asked, and lists the project names
/// and file names of what it was fed.
fn counting_source(calls: Arc<AtomicUsize>, fed: Arc<Mutex<Vec<Event>>>) -> InferenceSource {
    InferenceSource::streaming(move |feed| {
        calls.fetch_add(1, Ordering::SeqCst);
        let mut items: Vec<InferenceItem> = Vec::new();
        let mut seen = fed.lock().unwrap();
        seen.clear();
        feed(&mut |e: &Event| {
            seen.push(e.clone());
            items.push(InferenceItem {
                kind: "attempt".into(),
                id: format!("att_{}", e.event_id),
                session_id: None,
                project_id: None,
                evidence: vec![e.event_id],
                confidence: 0.9,
                algorithm_version: "test-v0".into(),
                fields: json!({
                    "files": e.paths.iter().map(|p| p.logical.clone()).collect::<Vec<_>>(),
                    "project": e.project.name,
                    "remote": e.project.repo_remote,
                }),
            });
        })?;
        Ok(InferenceSet {
            algorithm_version: "test-v0".into(),
            computed_at: Timestamp::now(),
            items,
        })
    })
}

fn set_state(locator: &Locator, f: impl FnOnce(&mut SyncState)) {
    let path = SyncState::path(&locator.paths.data_dir, &locator.db_dir, "default");
    let mut s = SyncState::load(&path).unwrap();
    f(&mut s);
    s.save(&path).unwrap();
}

#[test]
fn the_inference_set_is_recomputed_at_most_once_per_interval_unless_asked_for() {
    let tmp = tempfile::tempdir().unwrap();
    let (locator, device) = local_db(tmp.path());
    write_events(&locator, events(device, 2, "a"));
    let server = inference_server();
    let calls = Arc::new(AtomicUsize::new(0));
    let source = counting_source(Arc::clone(&calls), Arc::default());
    let mut c = cfg(&server.url, 10);
    c.send_inferences = true;
    c.inference_interval_secs = 600;
    let go = |opts: UploadOptions| upload_once_opts(&locator, "default", &c, Some(&source), opts);

    // The first run computes: nothing was ever uploaded.
    let r = go(UploadOptions::default()).unwrap();
    assert_eq!((r.accepted, calls.load(Ordering::SeqCst)), (2, 1), "{r:?}");
    assert_eq!(r.inferences.as_ref().unwrap().uploaded, 2);
    assert!(!state(&locator).inference_dirty);

    // New events arrive: they upload, the set waits for its interval.
    write_events(&locator, events(device, 2, "b"));
    let r = go(UploadOptions::default()).unwrap();
    assert_eq!((r.accepted, calls.load(Ordering::SeqCst)), (2, 1), "{r:?}");
    assert!(r.inferences.as_ref().unwrap().unchanged);
    assert!(state(&locator).inference_dirty, "owed, not forgotten");
    // Idle ticks in between cost no recompute.
    for _ in 0..3 {
        go(UploadOptions::default()).unwrap();
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    // `sync now --inferences`: no waiting.
    let r = go(UploadOptions {
        force_inferences: true,
    })
    .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(r.inferences.as_ref().unwrap().items, 4);
    assert!(!state(&locator).inference_dirty);

    // Dirty again, then the interval passes (the cursor file says the last
    // computation was eleven minutes ago): recomputed with no new events.
    write_events(&locator, events(device, 1, "c"));
    go(UploadOptions::default()).unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    set_state(&locator, |s| {
        s.inference_computed_at = Some(Timestamp::from_micros(
            Timestamp::now().as_micros() - 11 * 60_000_000,
        ));
    });
    let r = go(UploadOptions::default()).unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 3, "{r:?}");
    assert_eq!(r.inferences.as_ref().unwrap().items, 5);
    assert!(!state(&locator).inference_dirty);
}

#[test]
fn a_history_too_large_to_project_is_skipped_once_and_said_not_retried_every_tick() {
    let tmp = tempfile::tempdir().unwrap();
    let (locator, device) = local_db(tmp.path());
    write_events(&locator, events(device, 5, "a"));
    let server = inference_server();
    let calls = Arc::new(AtomicUsize::new(0));
    let source = counting_source(Arc::clone(&calls), Arc::default());
    let mut c = cfg(&server.url, 10);
    c.send_inferences = true;
    c.inference_max_events = 3;
    let go = |opts: UploadOptions| upload_once_opts(&locator, "default", &c, Some(&source), opts);

    let r = go(UploadOptions::default()).unwrap();
    assert_eq!(r.accepted, 5, "the events themselves still go: {r:?}");
    let inf = r.inferences.unwrap();
    assert_eq!(
        (inf.skipped_over_events, inf.uploaded),
        (Some(3), 0),
        "{inf:?}"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(
        attemptdb_capture::sync::describe(&UploadReport {
            inferences: Some(inf),
            ..Default::default()
        })
        .contains("more than 3 events to project")
    );
    let s = state(&locator);
    assert_eq!(s.inference_skipped_over, Some(3));
    assert!(!s.inference_dirty);
    // New events and more ticks do not try again at once: a try decodes up to
    // the limit before giving up.
    write_events(&locator, events(device, 2, "b"));
    for _ in 0..3 {
        go(UploadOptions::default()).unwrap();
    }
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "not retried within the wait"
    );
    // Asking for it is asking.
    go(UploadOptions {
        force_inferences: true,
    })
    .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    // With room for the history it is computed and the note goes away.
    c.inference_max_events = 0;
    let r = upload_once_opts(
        &locator,
        "default",
        &c,
        Some(&source),
        UploadOptions {
            force_inferences: true,
        },
    )
    .unwrap();
    assert_eq!(r.inferences.unwrap().uploaded, 7);
    assert_eq!(state(&locator).inference_skipped_over, None);
}

#[test]
fn an_idle_tick_does_not_open_the_database() {
    let tmp = tempfile::tempdir().unwrap();
    let (locator, device) = local_db(tmp.path());
    write_events(&locator, events(device, 2, "a"));
    let server = stub(|_, body, _, _| (200, ack(body)));
    let c = cfg(&server.url, 10);
    assert_eq!(run(&locator, &c).unwrap().accepted, 2);
    // A run with nothing to do marks the tick idle.
    let r = run(&locator, &c).unwrap();
    assert_eq!((r.pending_before, r.cursor), (0, 2));

    // From here a tick that finds the database's files unchanged answers
    // without opening it: with the identity file unreadable, an open fails.
    let identity = locator.db_dir.join("ATTEMPTDB");
    let good = std::fs::read(&identity).unwrap();
    std::fs::write(&identity, b"not a database").unwrap();
    let requests = server.count();
    for _ in 0..5 {
        let r = run(&locator, &c).expect("an idle tick does not open the database");
        assert_eq!((r.pending_before, r.cursor), (0, 2));
    }
    assert_eq!(server.count(), requests, "and sends nothing");
    // A changed configuration is not idle: the run goes to the database (and
    // finds it unreadable).
    let mut changed = c.clone();
    changed.batch_events = 7;
    assert!(run(&locator, &changed).is_err());
    std::fs::write(&identity, &good).unwrap();

    // A new event changes the WAL: the next tick finds it.
    write_events(&locator, events(device, 1, "b"));
    let r = run(&locator, &c).unwrap();
    assert_eq!((r.accepted, r.cursor), (1, 3), "{r:?}");
    // The cursor file changing under it (`attempt sync history include`
    // resets it) also ends the idleness.
    run(&locator, &c).unwrap();
    set_state(&locator, |s| {
        s.last_acked_source_seq = 0;
        s.last_acked_hlc = 0;
    });
    let r = run(&locator, &c).unwrap();
    assert_eq!(
        r.pending_before, 3,
        "everything again, the server dedupes: {r:?}"
    );
}

// ---------------------------------------------------------------------------
// An event over the server's limit: a reset is a refusal
// ---------------------------------------------------------------------------

/// A server that, like the one before this fix, drops the connection without
/// reading a body over 4 MiB.
fn resetting_server() -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let resets = Arc::new(AtomicUsize::new(0));
    let r2 = Arc::clone(&resets);
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut sock) = conn else { return };
            let resets = Arc::clone(&r2);
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                let mut tmp = [0u8; 16384];
                let head_end = loop {
                    let n = sock.read(&mut tmp).unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                    if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break i + 4;
                    }
                };
                let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
                let first = head.lines().next().unwrap_or_default().to_string();
                let cl: usize = head
                    .lines()
                    .find_map(|l| {
                        let (k, v) = l.split_once(':')?;
                        k.eq_ignore_ascii_case("content-length")
                            .then(|| v.trim().parse().ok())
                            .flatten()
                    })
                    .unwrap_or(0);
                if first.starts_with("GET /v1/health") {
                    let body = json!({"status": "ok", "server_version": "9.9.9"}).to_string();
                    let _ = sock.write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        )
                        .as_bytes(),
                    );
                    return;
                }
                if cl > 4 * 1024 * 1024 {
                    // Answer nothing and close with the body unread: the peer
                    // sees a reset while it is still writing.
                    resets.fetch_add(1, Ordering::SeqCst);
                    return;
                }
                while buf.len() < head_end + cl {
                    let n = sock.read(&mut tmp).unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                }
                let body: Value =
                    serde_json::from_slice(&buf[head_end..head_end + cl]).unwrap_or(Value::Null);
                let text = ack(&body).to_string();
                let _ = sock.write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
                        text.len()
                    )
                    .as_bytes(),
                );
            });
        }
    });
    (url, resets)
}

#[test]
fn a_connection_reset_on_a_body_past_the_server_limit_is_a_refusal_not_an_outage() {
    let tmp = tempfile::tempdir().unwrap();
    let (locator, device) = local_db(tmp.path());
    let mut evs = events(device, 3, "huge");
    evs[1].kind = EventKind::PromptSubmitted;
    evs[1].content = Some(EventContent {
        prompt: Some("y".repeat(5 * 1024 * 1024)),
        ..Default::default()
    });
    let big_id = evs[1].event_id.to_string();
    write_events(&locator, evs);
    let (url, resets) = resetting_server();
    let mut c = cfg(&url, 3);
    c.set_profile(SyncProfile::Messages);
    let r = run(&locator, &c).expect("the event is set aside, not retried for ever");
    assert!(resets.load(Ordering::SeqCst) >= 1, "the server did reset");
    assert_eq!(r.accepted, 3, "all three arrived, the big one bare: {r:?}");
    assert_eq!((r.quarantined, r.content_withheld), (1, 1), "{r:?}");
    let s = state(&locator);
    assert_eq!(s.last_acked_source_seq, 3);
    let rec = &s.quarantine[0];
    assert_eq!(rec.event_id.to_string(), big_id);
    assert_eq!((rec.status, rec.action.as_str()), (413, "content_withheld"));
    assert_eq!(rec.server_version.as_deref(), Some("9.9.9"));

    // A server that is simply not there is still an outage, not a refusal.
    let tmp2 = tempfile::tempdir().unwrap();
    let (locator2, device2) = local_db(tmp2.path());
    let mut evs = events(device2, 1, "down");
    evs[0].kind = EventKind::PromptSubmitted;
    evs[0].content = Some(EventContent {
        prompt: Some("z".repeat(5 * 1024 * 1024)),
        ..Default::default()
    });
    write_events(&locator2, evs);
    let mut c = cfg("http://127.0.0.1:1", 3);
    c.set_profile(SyncProfile::Messages);
    let err = run(&locator2, &c).unwrap_err().to_string();
    assert!(err.contains("cannot reach"), "{err}");
    assert_eq!(state(&locator2).quarantine.len(), 0);
}

// ---------------------------------------------------------------------------
// Set-aside events can be delivered again
// ---------------------------------------------------------------------------

#[test]
fn set_aside_events_are_delivered_again_on_request() {
    let tmp = tempfile::tempdir().unwrap();
    let (locator, device) = local_db(tmp.path());
    let mut evs = events(device, 5, "later");
    mark(&mut evs, 2, "x_test_poison");
    let poisoned = evs[2].event_id.to_string();
    write_events(&locator, evs);
    let upgraded = Arc::new(AtomicUsize::new(0));
    let u = Arc::clone(&upgraded);
    let server = stub(move |_, body, _, _| {
        if has_attr(body, "x_test_poison") && u.load(Ordering::SeqCst) == 0 {
            (422, json!({"error": "unknown variant `some_new_kind`"}))
        } else {
            (200, ack(body))
        }
    });
    let c = cfg(&server.url, 5);
    let r = run(&locator, &c).unwrap();
    assert_eq!((r.accepted, r.quarantined), (4, 1), "{r:?}");
    assert_eq!(state(&locator).quarantine.len(), 1);

    // Still the same server: refused again, and the record says so.
    let report = retry_set_aside(&locator, "default", &c).unwrap();
    assert_eq!(
        (
            report.tried,
            report.delivered,
            report.refused_again,
            report.gone
        ),
        (1, 0, 1, 0)
    );
    assert_eq!(state(&locator).quarantine.len(), 1);
    assert!(!server.accepted_ids().contains(&poisoned));

    // The server was upgraded: delivered, and off the list.
    upgraded.store(1, Ordering::SeqCst);
    let report = retry_set_aside(&locator, "default", &c).unwrap();
    assert_eq!(
        (
            report.tried,
            report.delivered,
            report.refused_again,
            report.gone
        ),
        (1, 1, 0, 0)
    );
    let s = state(&locator);
    assert!(s.quarantine.is_empty());
    assert_eq!(s.set_aside_retried, 1);
    assert!(server.accepted_ids().contains(&poisoned));
    // Nothing left to retry.
    assert_eq!(retry_set_aside(&locator, "default", &c).unwrap().tried, 0);
}

// ---------------------------------------------------------------------------
// What leaves with an event besides its text
// ---------------------------------------------------------------------------

#[test]
fn the_branch_the_project_name_and_the_remote_are_scanned_for_secrets_under_every_profile() {
    let token = "ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZabcdef0123";
    let d = DeviceId::derive(&["secret-test"]);
    for profile in SyncProfile::ALL {
        let mut e = Event::new(
            d,
            Provider::ClaudeCode,
            "PostToolUse",
            EventKind::ToolCallFinished,
            ProjectRef::derive("/home/dev/repo", Some("github.com/acme/repo"), &d),
            "s",
            CaptureMode::LocalSemantic,
            "t",
        );
        e.project.branch = Some(format!("feat/{token}"));
        e.project.name = format!("acme/{token}");
        e.project.repo_remote = Some(format!("github.com/acme/{token}"));
        let mut c = PeerConfig::new("https://x", "k");
        c.set_profile(profile);
        let (out, stats) = prepare_for_upload(&c, e);
        let text = serde_json::to_string(&out).unwrap();
        assert!(!text.contains("ghp_"), "{profile}: {text}");
        assert!(stats.spans >= 3, "{profile}: {stats:?}");
        assert!(
            out.project
                .branch
                .as_deref()
                .unwrap()
                .starts_with("feat/[REDACTED:"),
            "{profile}: the rest of the branch name is kept"
        );
        assert_eq!(
            out.attrs["x_attemptdb_secrets_ruleset"],
            json!(attemptdb_core::secrets::RULESET)
        );
    }
}

#[test]
fn the_upload_scrub_removes_every_home_directory_shape_the_review_found() {
    let d = DeviceId::derive(&["home-test"]);
    for home in [
        "/mnt/c/Users/alice",
        "/var/home/alice",
        "/Volumes/Data/Users/alice",
        "/System/Volumes/Data/Users/alice",
        "/usr/home/alice",
    ] {
        let root = format!("{home}/code/app");
        let mut e = Event::new(
            d,
            Provider::ClaudeCode,
            "PostToolUse",
            EventKind::ToolCallFinished,
            ProjectRef::derive(&root, None, &d),
            "s",
            CaptureMode::LocalSemantic,
            "t",
        );
        e.paths = vec![PortablePath::from_raw(
            &format!("{home}/notes/todo.md"),
            Some(&root),
        )];
        scrub_paths(&mut e);
        let text = serde_json::to_string(&e).unwrap();
        assert!(!text.contains("alice"), "{home}: {text}");
        assert_eq!(e.project.root, "~/code/app");
        assert_eq!(e.paths[0].logical, "~/notes/todo.md");
    }
}

// ---------------------------------------------------------------------------
// `sync forget` is not undone by the next upload
// ---------------------------------------------------------------------------

fn all_bytes(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for entry in rd.flatten() {
            let p = entry.path();
            if p.is_dir() {
                stack.push(p);
            } else if let Ok(bytes) = std::fs::read(&p) {
                out.push((p, bytes));
            }
        }
    }
    out
}

/// What the server holds of a tenant, as one searchable text: every file
/// under the tenant directory (inference documents are plain JSON) and every
/// stored event decoded.
fn tenant_text(server: &Real) -> String {
    let dir = server.data_dir.join("tenants").join("t1");
    let mut text = String::new();
    for (path, bytes) in all_bytes(&dir) {
        text.push_str(&format!("\n-- {}\n", path.display()));
        text.push_str(&String::from_utf8_lossy(&bytes));
    }
    text.push_str(&serde_json::to_string(&server.stored()).unwrap());
    text
}

fn canary_event(device: DeviceId, project: &str, file: &str, prompt: &str) -> Event {
    let root = format!("/Users/alice/work/{project}");
    let mut e = Event::new(
        device,
        Provider::ClaudeCode,
        "UserPromptSubmit",
        EventKind::PromptSubmitted,
        ProjectRef::derive(
            &root,
            Some(&format!("git@github.com:acme/{project}.git")),
            &device,
        ),
        format!("session-{project}"),
        CaptureMode::LocalSemantic,
        "resilience/0.1",
    );
    e.paths = vec![PortablePath::from_raw(
        &format!("{root}/src/{file}"),
        Some(&root),
    )];
    e.content = Some(EventContent {
        prompt: Some(prompt.into()),
        ..Default::default()
    });
    e
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_forgotten_range_is_not_rebuilt_from_the_device_by_the_next_upload() {
    const REPO: &str = "forgotten-canary-repo";
    const FILE: &str = "forgotten_canary_file.rs";
    const PROMPT: &str = "FORGOTTEN_PROMPT_CANARY please refactor";
    for with_watermark in [false, true] {
        let tmp = tempfile::tempdir().unwrap();
        let (locator, device) = local_db(tmp.path());
        write_events(&locator, vec![canary_event(device, REPO, FILE, PROMPT)]);
        let server = real_server(tmp.path(), device, CaptureMode::FullSync).await;
        let mut c = cfg(&server.url, 10);
        c.set_profile(SyncProfile::Full);
        let source = counting_source(Arc::default(), Arc::default());
        let go = {
            let (l, c) = (locator.clone(), c.clone());
            let source = source.clone();
            move || {
                let (l, c, source) = (l.clone(), c.clone(), source.clone());
                blocking(move || {
                    attemptdb_capture::sync::upload_once_with(&l, "default", &c, Some(&source))
                })
            }
        };
        let r = go().await.unwrap();
        assert_eq!(r.accepted, 1);
        let before = tenant_text(&server);
        assert!(
            before.contains(REPO) && before.contains(FILE) && before.contains("FORGOTTEN_PROMPT")
        );

        // Forget: the server deletes the events and the inference documents.
        let cc = c.clone();
        let report = blocking(move || forget_remote(&cc)).await.unwrap();
        assert_eq!(report.events_deleted, 1);
        let after_forget = tenant_text(&server);
        assert!(!after_forget.contains(REPO) && !after_forget.contains(FILE));
        assert!(!after_forget.contains("FORGOTTEN_PROMPT"));

        if with_watermark {
            // What `attempt sync forget` does: the range is closed.
            let at = Timestamp::now();
            let seq = local_newest_seq(&locator).unwrap();
            let mut consent = c.consent.clone().unwrap_or(Consent {
                at,
                profile: c.profile(),
                include: vec![],
                exclude: vec![],
                history_before: None,
                history_before_seq: None,
            });
            consent.advance_watermark(at, seq);
            c.consent = Some(consent);
        }
        // The person goes on working, in another repository.
        write_events(
            &locator,
            vec![canary_event(
                device,
                "fresh-repo",
                "fresh_file.rs",
                "a new prompt",
            )],
        );
        let go = {
            let (l, c, source) = (locator.clone(), c.clone(), source.clone());
            move || {
                let (l, c, source) = (l.clone(), c.clone(), source.clone());
                blocking(move || {
                    attemptdb_capture::sync::upload_once_with(&l, "default", &c, Some(&source))
                })
            }
        };
        let r = go().await.unwrap();
        assert_eq!(r.accepted, 1, "with_watermark={with_watermark}: {r:?}");
        let text = tenant_text(&server);
        assert!(text.contains("fresh_file.rs") && text.contains("fresh-repo"));
        if with_watermark {
            for canary in [REPO, FILE, "FORGOTTEN_PROMPT"] {
                assert!(
                    !text.contains(canary),
                    "`{canary}` is back on the server after forget + a new event"
                );
            }
        } else {
            // Without the watermark the inference document is rebuilt from
            // the whole local history: why the watermark exists.
            assert!(
                text.contains(FILE),
                "the control: forget alone is undone by the next upload"
            );
        }
        server.stop().await;
    }
}
