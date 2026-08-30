//! End-to-end: a real listener, raw HTTP/1.1 over TCP, real tenant
//! databases on disk. What a client sees is what these tests see.

use attemptdb_core::event::{EventContent, Provider};
use attemptdb_core::{CaptureMode, DeviceId, Event, EventKind, ProjectRef};
use attemptdb_server::auth::digest_hex;
use attemptdb_server::{Server, ServerConfig};
use attemptdb_storage::{Database, OpenOptions, ScanFilter};
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

const KEY_ALPHA: &str = "k-alpha-d1";
const KEY_BETA: &str = "k-beta-d2";

fn device(tag: &str) -> DeviceId {
    DeviceId::derive(&["server-test", tag])
}

fn write_keys(dir: &Path) -> PathBuf {
    let path = dir.join("keys.json");
    let keys = json!({ "keys": [
        { "sha256": digest_hex(KEY_ALPHA), "tenant": "alpha", "device_id": device("d1"), "label": "alpha d1" },
        { "sha256": digest_hex(KEY_BETA),  "tenant": "beta",  "device_id": device("d2"), "label": "beta d2" },
    ]});
    std::fs::write(&path, serde_json::to_vec_pretty(&keys).unwrap()).unwrap();
    path
}

struct Running {
    addr: SocketAddr,
    data_dir: PathBuf,
    _tmp: tempfile::TempDir,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<anyhow::Result<()>>,
}

async fn start(max_open: usize) -> Running {
    let tmp = tempfile::tempdir().unwrap();
    let keys_file = write_keys(tmp.path());
    let data_dir = tmp.path().join("data");
    let config = ServerConfig {
        port: 0,
        data_dir: data_dir.clone(),
        keys_file,
        max_open,
        body_limit: 64 * 1024,
        ..Default::default()
    };
    let server = Server::bind(config).await.unwrap();
    let addr = server.addr();
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let task = tokio::spawn(server.run(async move {
        let _ = rx.await;
    }));
    Running {
        addr,
        data_dir,
        _tmp: tmp,
        stop: Some(tx),
        task,
    }
}

impl Running {
    /// Graceful shutdown; the temp dir stays alive so tests can inspect
    /// what the server left on disk.
    async fn stop(&mut self) {
        let _ = self.stop.take().expect("stop once").send(());
        tokio::time::timeout(Duration::from_secs(10), &mut self.task)
            .await
            .expect("server stopped")
            .expect("server task")
            .expect("server exit");
    }

    fn tenant_dir(&self, tenant: &str) -> PathBuf {
        self.data_dir.join("tenants").join(tenant)
    }
}

/// One HTTP/1.1 request; `Connection: close`, read to EOF.
fn http(
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> (u16, Value) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n");
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    if !body.is_empty() {
        req.push_str("Content-Type: application/json\r\n");
    }
    req.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));
    req.push_str(body);
    s.write_all(req.as_bytes()).unwrap();
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).unwrap();
    let text = String::from_utf8_lossy(&raw).to_string();
    let (head, body) = text.split_once("\r\n\r\n").expect("header terminator");
    let status: u16 = head.split_whitespace().nth(1).unwrap().parse().unwrap();
    let body = if head
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked")
    {
        dechunk(body)
    } else {
        body.to_string()
    };
    let json = if body.trim().is_empty() {
        Value::Null
    } else {
        serde_json::from_str(&body).unwrap_or(Value::String(body))
    };
    (status, json)
}

fn dechunk(body: &str) -> String {
    let mut out = String::new();
    let mut rest = body;
    while let Some((size_line, after)) = rest.split_once("\r\n") {
        let size = usize::from_str_radix(size_line.trim(), 16).unwrap_or(0);
        if size == 0 {
            break;
        }
        out.push_str(&after[..size]);
        rest = &after[size + 2..];
    }
    out
}

async fn post(addr: SocketAddr, key: Option<&str>, body: Value) -> (u16, Value) {
    let body = body.to_string();
    let key = key.map(str::to_string);
    tokio::task::spawn_blocking(move || {
        let auth = key.map(|k| format!("Bearer {k}"));
        let headers: Vec<(&str, &str)> = auth
            .as_deref()
            .map(|a| vec![("Authorization", a)])
            .unwrap_or_default();
        http(addr, "POST", "/v1/sync", &headers, &body)
    })
    .await
    .unwrap()
}

/// Events as a client would upload them: full content, some attrs that break
/// the contract, one with a local `source_seq`.
fn events(dev: DeviceId, n: usize, tag: &str) -> Vec<Event> {
    (0..n)
        .map(|i| {
            let mut ev = Event::new(
                dev,
                Provider::ClaudeCode,
                "PostToolUse",
                EventKind::ToolCallFinished,
                ProjectRef::derive("/home/dev/example/project", None, &dev),
                format!("session-{tag}"),
                CaptureMode::LocalSemantic,
                "server-test/0.1",
            );
            ev.attrs.insert("x_test_index".into(), json!(i));
            ev.attrs
                .insert("prompt".into(), json!("rewrite the auth module please"));
            ev.content = Some(EventContent {
                tool_output: Some(Value::String("secret output".repeat(4))),
                ..Default::default()
            });
            ev.raw = Some(json!({"tool_response": "secret raw"}));
            if i == 0 {
                ev.source_seq = 7;
            }
            ev
        })
        .collect()
}

fn batch(dev: DeviceId, id: &str, events: &[Event]) -> Value {
    json!({
        "sync_version": 1,
        "device_id": dev,
        "batch_id": id,
        "capture_mode": "local_semantic",
        "events": events,
    })
}

fn scan(dir: &Path) -> Vec<Event> {
    let db = Database::open(
        dir,
        OpenOptions {
            read_only: true,
            ..Default::default()
        },
    )
    .unwrap();
    db.scan(&ScanFilter::default()).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn health_and_auth() {
    let mut r = start(8).await;
    let addr = r.addr;
    let (status, body) =
        tokio::task::spawn_blocking(move || http(addr, "GET", "/v1/health", &[], ""))
            .await
            .unwrap();
    assert_eq!(status, 200);
    assert_eq!(body["status"], "ok");
    assert_eq!(body["capture_mode"], "metadata_only");

    let dev = device("d1");
    let (status, body) = post(addr, None, batch(dev, "b0", &events(dev, 1, "a"))).await;
    assert_eq!(status, 401, "{body}");
    let (status, _) = post(addr, Some("nope"), batch(dev, "b0", &events(dev, 1, "a"))).await;
    assert_eq!(status, 401);
    // Right key, wrong device in the batch.
    let (status, body) = post(
        addr,
        Some(KEY_ALPHA),
        batch(device("d2"), "b0", &events(device("d2"), 1, "a")),
    )
    .await;
    assert_eq!(status, 403, "{body}");
    // Wrong sync version.
    let mut v = batch(dev, "b0", &events(dev, 1, "a"));
    v["sync_version"] = json!(2);
    let (status, _) = post(addr, Some(KEY_ALPHA), v).await;
    assert_eq!(status, 400);
    // Malformed body.
    let (status, _) = tokio::task::spawn_blocking(move || {
        http(
            addr,
            "POST",
            "/v1/sync",
            &[("Authorization", "Bearer k-alpha-d1")],
            "{not json",
        )
    })
    .await
    .unwrap();
    assert!(status == 400 || status == 422, "{status}");
    r.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ingest_is_idempotent_and_the_ceiling_strips_content() {
    let mut r = start(8).await;
    let addr = r.addr;
    let dev = device("d1");
    let evs = events(dev, 3, "a");

    let (status, ack) = post(addr, Some(KEY_ALPHA), batch(dev, "b1", &evs)).await;
    assert_eq!(status, 200, "{ack}");
    assert_eq!(ack["batch_id"], "b1");
    assert_eq!(ack["accepted"], 3);
    assert_eq!(ack["duplicates"], 0);
    assert_eq!(ack["rejected"], json!([]));
    assert_eq!(ack["stripped_content"], 3, "content removed by the ceiling");
    assert_eq!(ack["redactions"], 3, "one forbidden attr per event");

    // Re-sending the same batch stores nothing new.
    let (status, ack) = post(addr, Some(KEY_ALPHA), batch(dev, "b1-again", &evs)).await;
    assert_eq!(status, 200);
    assert_eq!(ack["accepted"], 0);
    assert_eq!(ack["duplicates"], 3);

    // On disk: metadata only, contract enforced, client sequence kept.
    let stored = scan(&r.tenant_dir("alpha"));
    assert_eq!(stored.len(), 3);
    for ev in &stored {
        assert!(ev.content.is_none(), "content reached the server's disk");
        assert!(ev.raw.is_none(), "raw payload reached the server's disk");
        assert_eq!(ev.capture_mode, CaptureMode::MetadataOnly);
        assert!(!ev.attrs.contains_key("prompt"), "{:?}", ev.attrs);
        assert_eq!(ev.attrs["redactions"], 1);
        assert!(ev.attrs.contains_key("x_test_index"));
        assert!(ev.is_ingested());
    }
    let with_seq: Vec<&Event> = stored
        .iter()
        .filter(|e| e.attrs.contains_key("device_seq"))
        .collect();
    assert_eq!(with_seq.len(), 1);
    assert_eq!(with_seq[0].attrs["device_seq"], 7);

    // A mixed batch: events for another device are rejected, not stored.
    let mut mixed = events(dev, 1, "b");
    mixed.extend(events(device("d2"), 1, "b"));
    let (status, ack) = post(addr, Some(KEY_ALPHA), batch(dev, "b2", &mixed)).await;
    assert_eq!(status, 200, "{ack}");
    assert_eq!(ack["accepted"], 1);
    assert_eq!(ack["rejected"].as_array().unwrap().len(), 1);
    assert_eq!(
        ack["rejected"][0]["reason"],
        "event device_id does not match the batch"
    );
    assert_eq!(scan(&r.tenant_dir("alpha")).len(), 4);
    r.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tenants_are_isolated_and_lru_reopens_cleanly() {
    // max_open = 1: every switch between tenants evicts the other one.
    let mut r = start(1).await;
    let addr = r.addr;
    let d1 = device("d1");
    let d2 = device("d2");
    for round in 0..3 {
        let (status, ack) = post(
            addr,
            Some(KEY_ALPHA),
            batch(
                d1,
                &format!("a{round}"),
                &events(d1, 2, &format!("a{round}")),
            ),
        )
        .await;
        assert_eq!(status, 200, "alpha round {round}: {ack}");
        assert_eq!(ack["accepted"], 2);
        let (status, ack) = post(
            addr,
            Some(KEY_BETA),
            batch(
                d2,
                &format!("b{round}"),
                &events(d2, 1, &format!("b{round}")),
            ),
        )
        .await;
        assert_eq!(status, 200, "beta round {round}: {ack}");
        assert_eq!(ack["accepted"], 1);
    }
    let (_, health) = tokio::task::spawn_blocking(move || http(addr, "GET", "/v1/health", &[], ""))
        .await
        .unwrap();
    assert_eq!(health["open_tenants"], 1, "LRU keeps one tenant resident");

    let alpha = scan(&r.tenant_dir("alpha"));
    let beta = scan(&r.tenant_dir("beta"));
    assert_eq!(alpha.len(), 6);
    assert_eq!(beta.len(), 3);
    assert!(alpha.iter().all(|e| e.device_id == d1));
    assert!(beta.iter().all(|e| e.device_id == d2));
    r.stop().await;

    // Shutdown flushed every open tenant: the WAL is drained into segments,
    // so the next open (a cold start, or a reader) pays no replay.
    for tenant in ["alpha", "beta"] {
        let db = Database::open(
            &r.tenant_dir(tenant),
            OpenOptions {
                read_only: true,
                ..Default::default()
            },
        )
        .unwrap();
        let st = db.stats();
        assert!(st.segments > 0, "{tenant}: no segments after shutdown");
        assert_eq!(
            st.memtable_rows, 0,
            "{tenant}: WAL not drained by shutdown flush"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversized_bodies_are_refused() {
    let mut r = start(8).await;
    let addr = r.addr;
    let dev = device("d1");
    // body_limit is 64 KiB in these tests; 200 events with content is well past it.
    let big = events(dev, 200, "big");
    let (status, _) = post(addr, Some(KEY_ALPHA), batch(dev, "big", &big)).await;
    assert_eq!(status, 413);
    assert!(!r.tenant_dir("alpha").exists() || scan(&r.tenant_dir("alpha")).is_empty());
    r.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn legacy_vibemon_envelope_lands_in_the_same_tenant() {
    let mut r = start(8).await;
    let addr = r.addr;
    let envelope = json!({
        "v": 2, "agent": "claude_code", "event": "bash", "session_id": "sess-legacy-1",
        "cwd": "/home/dev/proj", "project_root": "example/project",
        "timestamp": "2026-08-30T09:00:00Z",
        "payload": {"tool_name": "Bash", "session_id": "sess-legacy-1"},
        "signals": {"bash.category": "git.commit", "bash.byte_len": 40, "commit.message": "feat: x"}
    });
    let post_env = |key: Option<&'static str>, body: Value| {
        let body = body.to_string();
        tokio::task::spawn_blocking(move || {
            let auth = key.map(|k| format!("Bearer {k}"));
            let headers: Vec<(&str, &str)> = auth
                .as_deref()
                .map(|a| vec![("Authorization", a)])
                .unwrap_or_default();
            http(addr, "POST", "/v1/vibemon/hook", &headers, &body)
        })
    };
    let (status, _) = post_env(None, envelope.clone()).await.unwrap();
    assert_eq!(status, 401);
    let (status, body) = post_env(
        Some(KEY_ALPHA),
        json!({"v": 1, "event": "bash", "agent": "claude_code"}),
    )
    .await
    .unwrap();
    assert_eq!(status, 400, "{body}");
    let (status, body) = post_env(Some(KEY_ALPHA), envelope.clone()).await.unwrap();
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["accepted"], 1);

    let stored = scan(&r.tenant_dir("alpha"));
    assert_eq!(stored.len(), 1);
    let ev = &stored[0];
    assert_eq!(ev.kind.as_str(), "tool_call_finished");
    assert_eq!(
        ev.device_id,
        device("d1"),
        "device comes from the key, not the envelope"
    );
    assert_eq!(ev.attrs["command_subcategory"], "git.commit");
    assert_eq!(ev.attrs["cwd"], "~/proj");
    assert!(
        ev.content.is_none(),
        "commit title is content: gone under the ceiling"
    );
    assert!(ev.raw.is_none());
    assert_eq!(ev.hook_version.as_deref(), Some("vibemon-envelope-v2"));
    r.stop().await;
}

// ---------------------------------------------------------------------------
// Key issuance: the admin surface mints keys, stores digests, reloads.
// ---------------------------------------------------------------------------

const ADMIN: &str = "admin-secret-token";

async fn start_admin() -> Running {
    let tmp = tempfile::tempdir().unwrap();
    let keys_file = write_keys(tmp.path());
    let data_dir = tmp.path().join("data");
    let config = ServerConfig {
        port: 0,
        data_dir: data_dir.clone(),
        keys_file,
        max_open: 8,
        body_limit: 64 * 1024,
        admin_token: Some(ADMIN.into()),
        ..Default::default()
    };
    let server = Server::bind(config).await.unwrap();
    let addr = server.addr();
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let task = tokio::spawn(server.run(async move {
        let _ = rx.await;
    }));
    Running {
        addr,
        data_dir,
        _tmp: tmp,
        stop: Some(tx),
        task,
    }
}

async fn admin(
    addr: SocketAddr,
    method: &'static str,
    path: String,
    token: Option<&'static str>,
    body: Value,
) -> (u16, Value) {
    let body = if body.is_null() {
        String::new()
    } else {
        body.to_string()
    };
    tokio::task::spawn_blocking(move || {
        let auth = token.map(|t| format!("Bearer {t}"));
        let headers: Vec<(&str, &str)> = auth
            .as_deref()
            .map(|a| vec![("Authorization", a)])
            .unwrap_or_default();
        http(addr, method, &path, &headers, &body)
    })
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admin_surface_is_absent_without_a_token() {
    let mut r = start(8).await;
    let (status, _) = admin(
        r.addr,
        "GET",
        "/v1/admin/keys".into(),
        Some(ADMIN),
        Value::Null,
    )
    .await;
    assert_eq!(
        status, 404,
        "no admin token configured: the routes do not exist"
    );
    r.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn issued_keys_work_until_revoked_and_the_file_holds_only_digests() {
    let mut r = start_admin().await;
    let addr = r.addr;

    let (status, _) = admin(addr, "GET", "/v1/admin/keys".into(), None, Value::Null).await;
    assert_eq!(status, 401);
    let (status, _) = admin(
        addr,
        "GET",
        "/v1/admin/keys".into(),
        Some("wrong"),
        Value::Null,
    )
    .await;
    assert_eq!(status, 401);

    // Issue a key for a new tenant; the server mints the device id.
    let (status, issued) = admin(
        addr,
        "POST",
        "/v1/admin/keys".into(),
        Some(ADMIN),
        json!({"tenant": "gamma", "label": "gamma laptop"}),
    )
    .await;
    assert_eq!(status, 201, "{issued}");
    let key = issued["key"].as_str().unwrap().to_string();
    assert!(key.starts_with("atk_") && key.len() > 60);
    let digest = issued["sha256"].as_str().unwrap().to_string();
    let device: DeviceId = serde_json::from_value(issued["device_id"].clone()).unwrap();
    assert_eq!(digest, digest_hex(&key));

    // The key file holds the digest and never the key.
    let file = std::fs::read_to_string(r._tmp.path().join("keys.json")).unwrap();
    assert!(file.contains(&digest));
    assert!(!file.contains(&key));
    let (status, listed) = admin(
        addr,
        "GET",
        "/v1/admin/keys".into(),
        Some(ADMIN),
        Value::Null,
    )
    .await;
    assert_eq!(status, 200);
    let listed = listed["keys"].as_array().unwrap();
    assert_eq!(listed.len(), 3, "two fixtures plus the new one");
    assert!(
        listed
            .iter()
            .any(|k| k["sha256"] == digest && k["label"] == "gamma laptop")
    );
    assert!(!listed.iter().any(|k| k.get("key").is_some()));

    // The new key uploads into its own tenant, immediately (no restart).
    let (status, ack) = post(
        addr,
        Some(&key),
        batch(device, "g1", &events(device, 2, "g")),
    )
    .await;
    assert_eq!(status, 200, "{ack}");
    assert_eq!(ack["accepted"], 2);
    assert_eq!(scan(&r.tenant_dir("gamma")).len(), 2);

    // Revoke: the next upload is refused; the fixtures still work.
    let (status, _) = admin(
        addr,
        "DELETE",
        format!("/v1/admin/keys/{digest}"),
        Some(ADMIN),
        Value::Null,
    )
    .await;
    assert_eq!(status, 200);
    let (status, _) = post(
        addr,
        Some(&key),
        batch(device, "g2", &events(device, 1, "g")),
    )
    .await;
    assert_eq!(status, 401, "revoked key");
    let d1 = device_fixture("d1");
    let (status, _) = post(addr, Some(KEY_ALPHA), batch(d1, "a1", &events(d1, 1, "a"))).await;
    assert_eq!(status, 200);
    let (status, _) = admin(
        addr,
        "DELETE",
        format!("/v1/admin/keys/{digest}"),
        Some(ADMIN),
        Value::Null,
    )
    .await;
    assert_eq!(status, 404, "already gone");

    // Reload after an external edit of the file.
    let path = r._tmp.path().join("keys.json");
    let mut doc: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    doc["keys"].as_array_mut().unwrap().push(json!({
        "sha256": digest_hex("hand-added-key"), "tenant": "delta", "device_id": device_fixture("d9"), "label": "hand"
    }));
    std::fs::write(&path, doc.to_string()).unwrap();
    let d9 = device_fixture("d9");
    let (status, _) = post(
        addr,
        Some("hand-added-key"),
        batch(d9, "h1", &events(d9, 1, "h")),
    )
    .await;
    assert_eq!(status, 401, "not loaded yet");
    let (status, body) = admin(
        addr,
        "POST",
        "/v1/admin/keys/reload".into(),
        Some(ADMIN),
        Value::Null,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["keys"], 3);
    let (status, _) = post(
        addr,
        Some("hand-added-key"),
        batch(d9, "h1", &events(d9, 1, "h")),
    )
    .await;
    assert_eq!(status, 200, "loaded after reload");
    r.stop().await;
}

fn device_fixture(tag: &str) -> DeviceId {
    device(tag)
}

// ---------------------------------------------------------------------------
// Inference uploads (RFC 0006 §10.7)
// ---------------------------------------------------------------------------

async fn call(addr: SocketAddr, method: &str, path: &str, key: &str, body: Value) -> (u16, Value) {
    let (method, path, key) = (method.to_string(), path.to_string(), key.to_string());
    let body = if body.is_null() {
        String::new()
    } else {
        body.to_string()
    };
    tokio::task::spawn_blocking(move || {
        let auth = format!("Bearer {key}");
        http(addr, &method, &path, &[("Authorization", &auth)], &body)
    })
    .await
    .unwrap()
}

fn inference_batch(dev: DeviceId, kind: &str, items: Value) -> Value {
    json!({
        "sync_version": 1,
        "schema": "attemptdb.inference/v1",
        "device_id": dev,
        "batch_id": "inf-1",
        "kind": kind,
        "algorithm_version": "tier1-v0",
        "computed_at": attemptdb_core::Timestamp::now(),
        "items": items,
    })
}

#[tokio::test]
async fn inference_uploads_require_provenance_and_stay_out_of_the_event_database() {
    let mut running = start(2).await;
    let addr = running.addr;
    let d1 = device("d1");
    let items = json!([
        { "kind": "attempt", "id": "att_ok", "evidence": ["evt_1", "evt_2"], "confidence": 0.9,
          "algorithm_version": "tier1-v0", "fields": { "objective": "the prompt", "approach": "edit src/lib.rs" } },
        { "kind": "attempt", "id": "att_no_evidence", "evidence": [], "confidence": 0.9,
          "algorithm_version": "tier1-v0", "fields": {} },
        { "kind": "attempt", "id": "att_bad_confidence", "evidence": ["evt_1"], "confidence": 2.0,
          "algorithm_version": "tier1-v0", "fields": {} },
    ]);
    let (status, ack) = call(
        addr,
        "POST",
        "/v1/sync/inferences",
        KEY_ALPHA,
        inference_batch(d1, "attempt", items),
    )
    .await;
    assert_eq!(status, 200, "{ack}");
    assert_eq!(ack["stored"], json!(1));
    let rejected = ack["rejected"].as_array().unwrap();
    assert_eq!(rejected.len(), 2);
    assert_eq!(rejected[0]["id"], json!("att_no_evidence"));
    assert!(rejected[0]["reason"].as_str().unwrap().contains("evidence"));
    assert_eq!(rejected[1]["id"], json!("att_bad_confidence"));
    assert_eq!(
        ack["stripped"],
        json!(1),
        "metadata_only ceiling removed the objective"
    );

    // Read back: provenance kept, prompt gone.
    let (status, doc) = call(
        addr,
        "GET",
        "/v1/inferences?kind=attempt",
        KEY_ALPHA,
        Value::Null,
    )
    .await;
    assert_eq!(status, 200, "{doc}");
    assert_eq!(doc["items"].as_array().unwrap().len(), 1);
    assert_eq!(doc["items"][0]["id"], json!("att_ok"));
    assert!(doc["items"][0]["fields"]["objective"].is_null());
    assert_eq!(
        doc["items"][0]["fields"]["approach"],
        json!("edit src/lib.rs")
    );
    assert_eq!(doc["items"][0]["evidence"].as_array().unwrap().len(), 2);

    // A second upload of the same kind replaces the document wholesale.
    let (status, ack) = call(
        addr,
        "POST",
        "/v1/sync/inferences",
        KEY_ALPHA,
        inference_batch(d1, "attempt", json!([])),
    )
    .await;
    assert_eq!(status, 200, "{ack}");
    assert_eq!(ack["stored"], json!(0));
    let (_, doc) = call(
        addr,
        "GET",
        "/v1/inferences?kind=attempt",
        KEY_ALPHA,
        Value::Null,
    )
    .await;
    assert_eq!(doc["items"].as_array().unwrap().len(), 0);

    // Wrong device, unknown kind, unknown schema, other tenant's key.
    let (status, _) = call(
        addr,
        "POST",
        "/v1/sync/inferences",
        KEY_ALPHA,
        inference_batch(device("d2"), "attempt", json!([])),
    )
    .await;
    assert_eq!(status, 403);
    let (status, _) = call(
        addr,
        "POST",
        "/v1/sync/inferences",
        KEY_ALPHA,
        inference_batch(d1, "causal_edge", json!([])),
    )
    .await;
    assert_eq!(status, 400);
    let mut wrong_schema = inference_batch(d1, "attempt", json!([]));
    wrong_schema["schema"] = json!("attemptdb.inference/v2");
    let (status, _) = call(addr, "POST", "/v1/sync/inferences", KEY_ALPHA, wrong_schema).await;
    assert_eq!(status, 400);
    let (status, doc) = call(addr, "GET", "/v1/inferences", KEY_BETA, Value::Null).await;
    assert_eq!(status, 200);
    assert_eq!(
        doc["kinds"].as_array().unwrap().len(),
        0,
        "beta sees nothing of alpha"
    );

    // Stored beside the tenant, not as events: no database was created.
    let tenant_dir = running.data_dir.join("tenants").join("alpha");
    assert!(
        tenant_dir
            .join("inferences")
            .join(d1.to_string())
            .join("attempt.json")
            .is_file()
    );
    assert!(
        !Database::exists(&tenant_dir),
        "inferences are not facts; nothing was ingested"
    );

    let _ = running.stop.take().unwrap().send(());
    let _ = (&mut running.task).await;
}
