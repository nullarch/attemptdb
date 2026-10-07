//! The outbound webhook: accepted events reach the product's endpoint,
//! signed, in `source_seq` order, exactly past a durable cursor — through
//! a failing endpoint and across a restart.

mod common;

use attemptdb_core::event::EventContent;
use attemptdb_core::{CaptureMode, EventKind};
use attemptdb_server::ServerConfig;
use attemptdb_server::webhook::{WebhookConfig, verify};
use common::{
    KEY_ALPHA, READER_ALPHA, StartOptions, batch, device, device_keys, events, get, post,
    reader_keys, start_with,
};
use serde_json::Value;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Mutex;

const SECRET: &str = "whsec_test";

/// One delivery as the receiver saw it.
#[derive(Clone, Debug)]
struct Delivery {
    tenant_header: String,
    signature_ok: bool,
    body: Value,
}

/// A minimal HTTP/1.1 receiver: records every POST, answers 500 to the
/// first `fail_first` requests, 200 afterwards.
struct Receiver {
    url: String,
    deliveries: Arc<Mutex<Vec<Delivery>>>,
    requests: Arc<AtomicUsize>,
}

async fn receiver(fail_first: usize) -> Receiver {
    receiver_refusing(fail_first, None).await
}

/// Like [`receiver`], and a request whose body contains the marker is
/// answered with the status line instead (and is not recorded as delivered):
/// what a request filter in front of a real receiver does. The empty marker
/// matches every body.
async fn receiver_refusing(
    fail_first: usize,
    refuse: Option<(&'static str, &'static str)>,
) -> Receiver {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/attemptdb", listener.local_addr().unwrap());
    let deliveries = Arc::new(Mutex::new(Vec::new()));
    let requests = Arc::new(AtomicUsize::new(0));
    let (d, r) = (Arc::clone(&deliveries), Arc::clone(&requests));
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let d = Arc::clone(&d);
            let r = Arc::clone(&r);
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut tmp = [0u8; 8192];
                let (head_end, content_length);
                loop {
                    let n = sock.read(&mut tmp).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                    if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        head_end = i + 4;
                        let head = String::from_utf8_lossy(&buf[..i]).to_string();
                        content_length = head
                            .lines()
                            .find_map(|l| {
                                let (k, v) = l.split_once(':')?;
                                k.eq_ignore_ascii_case("content-length")
                                    .then(|| v.trim().parse::<usize>().ok())
                                    .flatten()
                            })
                            .unwrap_or(0);
                        break;
                    }
                }
                while buf.len() < head_end + content_length {
                    let n = sock.read(&mut tmp).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                }
                let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
                let body = &buf[head_end..head_end + content_length];
                let header = |name: &str| {
                    head.lines()
                        .find_map(|l| {
                            let (k, v) = l.split_once(':')?;
                            k.eq_ignore_ascii_case(name).then(|| v.trim().to_string())
                        })
                        .unwrap_or_default()
                };
                let n = r.fetch_add(1, Ordering::SeqCst);
                let refused = refuse
                    .filter(|(marker, _)| String::from_utf8_lossy(body).contains(marker))
                    .map(|(_, status)| status);
                let status = if let Some(status) = refused {
                    status
                } else if n < fail_first {
                    "500 Internal Server Error"
                } else {
                    "200 OK"
                };
                if refused.is_none() && n >= fail_first {
                    d.lock().await.push(Delivery {
                        tenant_header: header("x-attemptdb-tenant"),
                        signature_ok: verify(SECRET, body, &header("x-attemptdb-signature")),
                        body: serde_json::from_slice(body).unwrap_or(Value::Null),
                    });
                }
                let resp = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok"
                );
                let _ = sock.write_all(resp.as_bytes()).await;
            });
        }
    });
    Receiver {
        url,
        deliveries,
        requests,
    }
}

async fn wait_for<F: Fn() -> bool>(what: &str, f: F) {
    let t = Instant::now();
    while !f() {
        assert!(
            t.elapsed() < Duration::from_secs(20),
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn accepted_events_are_delivered_signed_in_order_past_a_durable_cursor() {
    let rx = receiver(1).await; // the first request fails: the page is re-sent
    let mut r = start_with(StartOptions {
        webhook: Some(WebhookConfig::new(&rx.url, SECRET)),
        ..Default::default()
    })
    .await;
    let addr = r.addr;
    let d1 = device("d1");

    let (status, ack) = post(addr, Some(KEY_ALPHA), batch(d1, "b1", &events(d1, 5, "s"))).await;
    assert_eq!(status, 200, "{ack}");
    assert_eq!(ack["accepted"], 5);

    let deliveries = Arc::clone(&rx.deliveries);
    let dv = Arc::clone(&deliveries);
    wait_for("the first delivery", move || {
        dv.try_lock().map(|d| !d.is_empty()).unwrap_or(false)
    })
    .await;
    assert!(
        rx.requests.load(Ordering::SeqCst) >= 2,
        "the failed attempt was retried"
    );
    let first = deliveries.lock().await[0].clone();
    assert!(first.signature_ok, "HMAC over the exact body");
    assert_eq!(first.tenant_header, "alpha");
    assert_eq!(first.body["tenant"], "alpha");
    assert_eq!(first.body["after"], 0);
    assert_eq!(first.body["count"], 5);
    let seqs: Vec<u64> = first.body["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["source_seq"].as_u64().unwrap())
        .collect();
    assert_eq!(seqs, vec![1, 2, 3, 4, 5]);
    assert_eq!(first.body["next"], 5);
    let dev = first.body["devices"][d1.to_string()].clone();
    assert_eq!(
        dev["label"], "alpha d1",
        "the key table's view of the device: {dev}"
    );
    assert!(
        dev["paired_at"].is_null(),
        "a hand-written key has no issue time: {dev}"
    );
    assert!(
        first.body["events"][0]["content"].is_null(),
        "metadata only"
    );

    // The cursor is on disk; a second batch is delivered from there.
    let cursor = r.data_dir.join("webhook").join("alpha.cursor");
    // The fixture records receipt before the sender receives HTTP 200 and
    // persists its cursor. Wait for that durable boundary, not receipt alone.
    wait_for("the durable first cursor", || {
        std::fs::read_to_string(&cursor).is_ok_and(|value| value.trim() == "5")
    })
    .await;
    assert_eq!(std::fs::read_to_string(&cursor).unwrap().trim(), "5");
    let (status, _) = post(addr, Some(KEY_ALPHA), batch(d1, "b2", &events(d1, 3, "t"))).await;
    assert_eq!(status, 200);
    let dv = Arc::clone(&deliveries);
    wait_for("the second delivery", move || {
        dv.try_lock().map(|d| d.len() >= 2).unwrap_or(false)
    })
    .await;
    let second = deliveries.lock().await[1].clone();
    assert_eq!(second.body["after"], 5);
    assert_eq!(second.body["next"], 8);
    assert_eq!(second.body["count"], 3);
    let health: Value = {
        let (_, h) = common::get(addr, "/v1/health", KEY_ALPHA).await;
        h
    };
    assert_eq!(health["webhook"]["deliveries"], 2, "{health}");
    assert_eq!(health["webhook"]["events"], 8);

    // A restart delivers nothing again (the cursor is at the end) — and
    // what was ingested while the endpoint was unreachable is delivered
    // after the endpoint comes back, from the cursor, once.
    let data_dir = r.data_dir.clone();
    let keys_file = r.keys_file.clone();
    let tmp = r._tmp.take();
    r.stop().await;
    drop(r); // the state's registry holds the tenant's writer lock
    let dead = receiver(usize::MAX).await; // every request fails
    let r2 = common::restart_config(ServerConfig {
        data_dir: data_dir.clone(),
        keys_file: keys_file.clone(),
        webhook: Some(WebhookConfig::new(&dead.url, SECRET)),
        ..Default::default()
    })
    .await;
    let (status, _) = post(
        r2.addr,
        Some(KEY_ALPHA),
        batch(d1, "b3", &events(d1, 2, "u")),
    )
    .await;
    assert_eq!(status, 200);
    let req = Arc::clone(&dead.requests);
    wait_for("the dead endpoint to be tried", move || {
        req.load(Ordering::SeqCst) >= 1
    })
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        std::fs::read_to_string(&cursor).unwrap().trim(),
        "8",
        "cursor unmoved"
    );
    let mut r2 = r2;
    r2.stop().await;
    drop(r2);

    let alive = receiver(0).await;
    let mut r3 = common::restart_config(ServerConfig {
        data_dir,
        keys_file,
        webhook: Some(WebhookConfig::new(&alive.url, SECRET)),
        ..Default::default()
    })
    .await;
    let dv = Arc::clone(&alive.deliveries);
    wait_for("the catch-up delivery at start", move || {
        dv.try_lock().map(|d| !d.is_empty()).unwrap_or(false)
    })
    .await;
    let catch_up = alive.deliveries.lock().await.clone();
    assert_eq!(catch_up.len(), 1, "one page, once: {catch_up:?}");
    assert_eq!(catch_up[0].body["after"], 8);
    assert_eq!(catch_up[0].body["next"], 10);
    assert_eq!(std::fs::read_to_string(&cursor).unwrap().trim(), "10");
    r3.stop().await;
    drop(tmp);
    let _ = device_keys();
}

/// A cursor file that exists but holds nothing (a torn write, an editor)
/// used to read as 0 and redeliver a tenant's whole history to the product.
/// Now delivery for the tenant stops and says why; only a cursor that says 0
/// replays.
#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn an_empty_cursor_file_pauses_delivery_instead_of_replaying_history() {
    let mut r = start_with(StartOptions::default()).await;
    let addr = r.addr;
    let d1 = device("d1");
    let (status, _) = post(addr, Some(KEY_ALPHA), batch(d1, "b1", &events(d1, 4, "s"))).await;
    assert_eq!(status, 200);
    let data_dir = r.data_dir.clone();
    let keys_file = r.keys_file.clone();
    let tmp = r._tmp.take();
    r.stop().await;
    drop(r);

    let cursor_dir = data_dir.join("webhook");
    std::fs::create_dir_all(&cursor_dir).unwrap();
    let cursor = cursor_dir.join("alpha.cursor");
    std::fs::write(&cursor, "").unwrap();

    let rx = receiver(0).await;
    let mut r2 = common::restart_config(ServerConfig {
        data_dir: data_dir.clone(),
        keys_file: keys_file.clone(),
        webhook: Some(WebhookConfig::new(&rx.url, SECRET)),
        ..Default::default()
    })
    .await;
    wait_for("the worker to try the tenant", || {
        r2.state.webhook_stats.failures.load(Ordering::Relaxed) >= 1
    })
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        rx.requests.load(Ordering::SeqCst),
        0,
        "an empty cursor file redelivered history"
    );
    assert!(
        std::fs::read_to_string(&cursor).unwrap().is_empty(),
        "left for the operator"
    );
    r2.stop().await;
    drop(r2);

    // A cursor that says 0 is a decision: the history is delivered.
    std::fs::write(&cursor, "0\n").unwrap();
    let rx = receiver(0).await;
    let mut r3 = common::restart_config(ServerConfig {
        data_dir,
        keys_file,
        webhook: Some(WebhookConfig::new(&rx.url, SECRET)),
        ..Default::default()
    })
    .await;
    let dv = Arc::clone(&rx.deliveries);
    wait_for("the replay a cursor of 0 asks for", move || {
        dv.try_lock().map(|d| !d.is_empty()).unwrap_or(false)
    })
    .await;
    assert_eq!(rx.deliveries.lock().await[0].body["count"], 4);
    r3.stop().await;
    drop(tmp);
}

/// The product's receiver reads an event's kind, times, session and `attrs`;
/// it never reads the conversation. Text in a delivery only gave a request
/// filter in front of the receiver something to refuse: a prompt that looks
/// like an attack answered one page with HTTP 403 and froze that tenant's feed
/// on the page for good (the cursor only moves on a 2xx). So a delivery is the
/// stored envelope without `content` and `raw`, even when the server stores
/// the conversation.
#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn a_delivery_never_carries_the_conversation_even_when_the_server_stores_it() {
    const CANARY: &str = "SELECT * FROM users; <script>alert(1)</script> ../../etc/passwd";
    let rx = receiver(0).await;
    let tmp = tempfile::tempdir().unwrap();
    let mut keys = device_keys();
    keys.extend(reader_keys());
    let keys_file = common::write_keys(tmp.path(), &keys);
    let mut r = common::restart_config(ServerConfig {
        data_dir: tmp.path().join("data"),
        keys_file,
        capture_mode: CaptureMode::LocalSemantic,
        webhook: Some(WebhookConfig::new(&rx.url, SECRET)),
        ..Default::default()
    })
    .await;
    let d1 = device("d1");
    let mut evs = events(d1, 3, "w");
    evs[0].kind = EventKind::PromptSubmitted;
    evs[0].content = Some(EventContent {
        prompt: Some(CANARY.into()),
        ..Default::default()
    });
    let (status, ack) = post(r.addr, Some(KEY_ALPHA), batch(d1, "bw", &evs)).await;
    assert_eq!(status, 200, "{ack}");

    // The server did keep the text: the conversation is stored and readable,
    // so what follows is the delivery's doing, not the ceiling's.
    let (status, stored) = get(r.addr, "/v1/events?after=0&limit=10", READER_ALPHA).await;
    assert_eq!(status, 200, "{stored}");
    assert!(
        stored.to_string().contains("etc/passwd"),
        "the server was meant to store the conversation: {stored}"
    );

    let dv = Arc::clone(&rx.deliveries);
    wait_for("the delivery", move || {
        dv.try_lock().map(|d| !d.is_empty()).unwrap_or(false)
    })
    .await;
    let delivery = rx.deliveries.lock().await[0].clone();
    assert!(delivery.signature_ok, "HMAC over the exact body");
    let body = delivery.body.to_string();
    for needle in ["etc/passwd", "<script>", "secret output", "secret raw"] {
        assert!(
            !body.contains(needle),
            "{needle:?} reached the receiver: {body}"
        );
    }
    let events = delivery.body["events"].as_array().unwrap();
    assert_eq!(events.len(), 3);
    for (i, e) in events.iter().enumerate() {
        assert!(e.get("content").is_none(), "event {i} carries content: {e}");
        assert!(e.get("raw").is_none(), "event {i} carries raw: {e}");
        assert_eq!(e["attrs"]["x_test_index"], i, "metadata is untouched: {e}");
    }
    assert_eq!(events[0]["kind"], "prompt_submitted");
    r.stop().await;
    drop(tmp);
}

/// Start a server with a webhook onto `url`, post `n` events as one batch (the
/// event at `poison`, if any, carries the marker in its provider session id) and return
/// the running server, the cursor file and the events' `source_seq`s.
async fn server_with_events(
    url: &str,
    n: usize,
    poison: Option<usize>,
) -> (common::Running, std::path::PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let keys_file = common::write_keys(tmp.path(), &device_keys());
    let mut evs = events(device("d1"), n, "iso");
    if let Some(i) = poison {
        evs[i].provider_session_id = "BLOCKME-session".into();
    }
    let mut r = common::restart_config(ServerConfig {
        data_dir: tmp.path().join("data"),
        keys_file,
        webhook: Some(WebhookConfig::new(url, SECRET)),
        ..Default::default()
    })
    .await;
    let (status, ack) = post(r.addr, Some(KEY_ALPHA), batch(device("d1"), "b-iso", &evs)).await;
    assert_eq!(status, 200, "{ack}");
    let cursor = r.data_dir.join("webhook").join("alpha.cursor");
    r._tmp = Some(tmp);
    (r, cursor)
}

fn delivered_seqs(deliveries: &[Delivery]) -> Vec<u64> {
    let mut seqs: Vec<u64> = deliveries
        .iter()
        .flat_map(|d| d.body["events"].as_array().cloned().unwrap_or_default())
        .map(|e| e["source_seq"].as_u64().unwrap())
        .collect();
    seqs.sort_unstable();
    seqs
}

/// A page the receiver refuses for what is in it used to be retried for ever,
/// and every event behind it with it. Now it is delivered in halves: what the
/// receiver takes lands, the one event it refuses on its own is set aside (in
/// a line of its own, without content) and the cursor goes on.
#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn one_refused_event_is_set_aside_and_the_rest_of_its_page_is_delivered() {
    let rx = receiver_refusing(0, Some(("BLOCKME", "403 Forbidden"))).await;
    let (mut r, cursor) = server_with_events(&rx.url, 6, Some(3)).await;

    wait_for("the cursor to pass the whole page", || {
        std::fs::read_to_string(&cursor).is_ok_and(|v| v.trim() == "6")
    })
    .await;
    let got = delivered_seqs(&rx.deliveries.lock().await);
    assert_eq!(got, vec![1, 2, 3, 5, 6], "everything but source_seq 4");

    let aside = std::fs::read_to_string(r.data_dir.join("webhook").join("alpha.set-aside.jsonl"))
        .expect("the set-aside record");
    let lines: Vec<Value> = aside
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 1, "{aside}");
    assert_eq!(lines[0]["source_seq"], 4);
    assert_eq!(lines[0]["kind"], "tool_call_finished");
    assert!(
        lines[0]["error"].as_str().unwrap().starts_with("403"),
        "{aside}"
    );
    for needle in ["secret output", "secret raw", "BLOCKME"] {
        assert!(!aside.contains(needle), "{needle:?} in the record: {aside}");
    }
    let (_, health) = get(r.addr, "/v1/health", KEY_ALPHA).await;
    assert_eq!(health["webhook"]["set_aside"], 1, "{health}");

    // The feed goes on: the next batch is an ordinary delivery.
    let (status, _) = post(
        r.addr,
        Some(KEY_ALPHA),
        batch(device("d1"), "b-after", &events(device("d1"), 2, "next")),
    )
    .await;
    assert_eq!(status, 200);
    wait_for("the next batch", || {
        std::fs::read_to_string(&cursor).is_ok_and(|v| v.trim() == "8")
    })
    .await;
    r.stop().await;
}

/// A receiver that refuses every body — a wrong URL, a blocked address, a
/// rule that matches everything — says nothing about any one event, and an
/// empty delivery is refused too. Nothing is set aside: the feed waits.
#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn an_endpoint_that_refuses_everything_costs_no_event() {
    let rx = receiver_refusing(0, Some(("", "403 Forbidden"))).await;
    let (mut r, cursor) = server_with_events(&rx.url, 4, None).await;

    let requests = Arc::clone(&rx.requests);
    wait_for("the receiver to be asked", move || {
        requests.load(Ordering::SeqCst) >= 4
    })
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !std::fs::read_to_string(&cursor).is_ok_and(|v| v.trim() != "0"),
        "the cursor moved past events nobody accepted"
    );
    assert!(
        !r.data_dir
            .join("webhook")
            .join("alpha.set-aside.jsonl")
            .exists(),
        "an event was set aside because the whole endpoint refused"
    );
    let (_, health) = get(r.addr, "/v1/health", KEY_ALPHA).await;
    assert_eq!(health["webhook"]["set_aside"], 0, "{health}");
    r.stop().await;
}

/// A 401 is the secret, not the body: it is retried like any outage and never
/// split, never set aside.
#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn a_rejected_signature_is_not_a_refusal_of_the_events() {
    let rx = receiver_refusing(0, Some(("", "401 Unauthorized"))).await;
    let (mut r, cursor) = server_with_events(&rx.url, 4, None).await;

    let requests = Arc::clone(&rx.requests);
    wait_for("three attempts at the whole page", move || {
        requests.load(Ordering::SeqCst) >= 3
    })
    .await;
    // Only whole-page deliveries: a split would have sent smaller bodies, and
    // the log would say so. Nothing is set aside and the cursor stays.
    assert!(
        !std::fs::read_to_string(&cursor).is_ok_and(|v| v.trim() != "0"),
        "the cursor moved"
    );
    assert!(
        !r.data_dir
            .join("webhook")
            .join("alpha.set-aside.jsonl")
            .exists()
    );
    r.stop().await;
}
