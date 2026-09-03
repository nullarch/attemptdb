//! The outbound webhook: accepted events reach the product's endpoint,
//! signed, in `source_seq` order, exactly past a durable cursor — through
//! a failing endpoint and across a restart.

mod common;

use attemptdb_server::ServerConfig;
use attemptdb_server::webhook::{WebhookConfig, verify};
use common::{KEY_ALPHA, StartOptions, batch, device, device_keys, events, post, start_with};
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
                let status = if n < fail_first {
                    "500 Internal Server Error"
                } else {
                    "200 OK"
                };
                if n >= fail_first {
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
        first.body["events"][0]["content"].is_null(),
        "metadata only"
    );

    // The cursor is on disk; a second batch is delivered from there.
    let cursor = r.data_dir.join("webhook").join("alpha.cursor");
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
