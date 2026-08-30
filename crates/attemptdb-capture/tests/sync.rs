//! End-to-end sync: a local database, a real `attemptdb-server` in-process,
//! `upload_once` between them. What the daemon does on its interval is
//! exactly this call.

#![cfg(unix)]

use attemptdb_capture::ingest;
use attemptdb_capture::locator::Locator;
use attemptdb_capture::sync::{SyncConfig, SyncState, upload_once};
use attemptdb_core::event::{EventContent, Provider};
use attemptdb_core::{CaptureMode, DeviceId, Event, EventKind, ProjectRef};
use attemptdb_server::auth::digest_hex;
use attemptdb_server::{Server, ServerConfig};
use attemptdb_storage::{Database, OpenOptions, ScanFilter};
use serde_json::json;
use std::path::{Path, PathBuf};

const KEY: &str = "device-key-1";

fn events(device: DeviceId, n: usize, tag: &str) -> Vec<Event> {
    (0..n)
        .map(|i| {
            let mut ev = Event::new(
                device,
                Provider::ClaudeCode,
                "PostToolUse",
                EventKind::ToolCallFinished,
                ProjectRef::derive("/home/dev/example/project", None, &device),
                format!("session-{tag}"),
                CaptureMode::LocalSemantic,
                "sync-test/0.1",
            );
            ev.attrs.insert("x_test_index".into(), json!(i));
            ev.content = Some(EventContent {
                command: Some(format!("echo {tag} {i}")),
                ..Default::default()
            });
            ev
        })
        .collect()
}

/// A portable-mode locator under `root`, with a fresh database.
fn local_db(root: &Path) -> (Locator, DeviceId) {
    let locator = Locator::resolve(root, Some(root), None);
    let db = ingest::open_writer(&locator, true).unwrap();
    let device = db.device_id();
    drop(db);
    (locator, device)
}

fn write_events(locator: &Locator, evs: Vec<Event>) {
    let mut db = ingest::open_writer(locator, false).unwrap();
    let r = db.ingest(evs).unwrap();
    assert_eq!(r.duplicates, 0);
    drop(db); // stays in the WAL; a read-only open replays it
}

struct ServerHandle {
    url: String,
    data_dir: PathBuf,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<anyhow::Result<()>>,
}

async fn start_server(root: &Path, device: DeviceId, max_open: usize) -> ServerHandle {
    let keys = root.join("keys.json");
    std::fs::write(
        &keys,
        json!({"keys": [{"sha256": digest_hex(KEY), "tenant": "t1", "device_id": device}]})
            .to_string(),
    )
    .unwrap();
    let data_dir = root.join("server-data");
    let server = Server::bind(ServerConfig {
        port: 0,
        data_dir: data_dir.clone(),
        keys_file: keys,
        max_open,
        body_limit: 256 * 1024,
        ..Default::default()
    })
    .await
    .unwrap();
    let url = format!("http://{}", server.addr());
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let task = tokio::spawn(server.run(async move {
        let _ = rx.await;
    }));
    ServerHandle {
        url,
        data_dir,
        stop: Some(tx),
        task,
    }
}

impl ServerHandle {
    async fn stop(mut self) {
        let _ = self.stop.take().unwrap().send(());
        let _ = self.task.await;
    }
    fn tenant_events(&self) -> Vec<Event> {
        let dir = self.data_dir.join("tenants").join("t1");
        let db = Database::open(
            &dir,
            OpenOptions {
                read_only: true,
                ..Default::default()
            },
        )
        .unwrap();
        db.scan(&ScanFilter::default()).unwrap()
    }
}

fn cfg(url: &str, key: &str, batch: usize) -> SyncConfig {
    SyncConfig {
        url: url.to_string(),
        key: key.to_string(),
        send_content: false,
        send_inferences: false,
        batch_events: batch,
        interval_secs: 5,
        include: vec![],
        exclude: vec![],
    }
}

async fn upload(
    locator: &Locator,
    cfg: &SyncConfig,
) -> anyhow::Result<attemptdb_capture::sync::UploadReport> {
    let (l, c) = (locator.clone(), cfg.clone());
    tokio::task::spawn_blocking(move || upload_once(&l, &c))
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn uploads_in_order_advances_the_cursor_and_strips_content() {
    let tmp = tempfile::tempdir().unwrap();
    let (locator, device) = local_db(tmp.path());
    write_events(&locator, events(device, 7, "a"));
    let server = start_server(tmp.path(), device, 4).await;
    let c = cfg(&server.url, KEY, 3);

    // 7 events, batches of 3 → 3 batches, all accepted, cursor at seq 7.
    let r = upload(&locator, &c).await.unwrap();
    assert_eq!(
        (r.pending_before, r.batches, r.accepted, r.duplicates),
        (7, 3, 7, 0)
    );
    // The client clamped content off before serialising, so the server had
    // nothing left to strip: the default keeps content on the device, not
    // merely out of the server's disk.
    assert_eq!(r.stripped_content, 0, "content never left the device");
    assert_eq!(r.cursor, 7);
    let state =
        SyncState::load(&SyncState::path(&locator.paths.data_dir, &locator.db_dir)).unwrap();
    assert_eq!(state.last_acked_source_seq, 7);
    assert_eq!(state.batches, 3);
    assert!(state.last_ok_at.is_some());
    assert!(state.last_error.is_none());

    // Nothing pending: no request is made.
    let r = upload(&locator, &c).await.unwrap();
    assert_eq!((r.pending_before, r.batches), (0, 0));

    // More events: only those after the cursor go, in order.
    write_events(&locator, events(device, 2, "b"));
    let r = upload(&locator, &c).await.unwrap();
    assert_eq!((r.pending_before, r.accepted, r.cursor), (2, 2, 9));

    let stored = server.tenant_events();
    assert_eq!(stored.len(), 9);
    for ev in &stored {
        assert!(ev.content.is_none(), "content reached the server");
        assert!(ev.raw.is_none());
        assert_eq!(ev.capture_mode, CaptureMode::MetadataOnly);
        assert!(ev.attrs.contains_key("device_seq"), "client seq preserved");
    }
    let mut seqs: Vec<u64> = stored
        .iter()
        .map(|e| e.attrs["device_seq"].as_u64().unwrap())
        .collect();
    seqs.sort_unstable();
    assert_eq!(seqs, (1..=9).collect::<Vec<_>>());

    // Local database untouched: content still there.
    let local = ingest::open_reader(&locator)
        .unwrap()
        .scan(&ScanFilter::default())
        .unwrap();
    assert!(local.iter().all(|e| e.content.is_some()));
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failures_keep_the_cursor_and_are_reported() {
    let tmp = tempfile::tempdir().unwrap();
    let (locator, device) = local_db(tmp.path());
    write_events(&locator, events(device, 3, "a"));
    let server = start_server(tmp.path(), device, 4).await;

    // Wrong key: rejected, cursor unchanged, error recorded.
    let err = upload(&locator, &cfg(&server.url, "wrong", 10))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("401"), "{err}");
    let state_path = SyncState::path(&locator.paths.data_dir, &locator.db_dir);
    let state = SyncState::load(&state_path).unwrap();
    assert_eq!(state.last_acked_source_seq, 0);
    assert!(state.last_error.as_deref().unwrap_or("").contains("401"));

    // Server gone: transport error, cursor unchanged.
    server.stop().await;
    let err = upload(&locator, &cfg("http://127.0.0.1:1", KEY, 10))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("cannot reach"), "{err}");
    assert_eq!(
        SyncState::load(&state_path).unwrap().last_acked_source_seq,
        0
    );

    // Body too large for the server: the batch splits and still succeeds.
    let (l2, d2) = local_db(&tmp.path().join("second"));
    let big: Vec<Event> = events(d2, 40, "big")
        .into_iter()
        .map(|mut e| {
            e.attrs.insert("x_test_pad".into(), json!("p".repeat(200)));
            e
        })
        .collect();
    write_events(&l2, big);
    let server = start_server(&tmp.path().join("second"), d2, 4).await;
    let r = upload(&l2, &cfg(&server.url, KEY, 40)).await.unwrap();
    assert_eq!(r.accepted, 40);
    assert!(r.batches >= 1);
    assert_eq!(server.tenant_events().len(), 40);
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn send_content_is_an_explicit_opt_in_and_the_server_still_has_the_last_word() {
    let tmp = tempfile::tempdir().unwrap();
    let (locator, device) = local_db(tmp.path());
    write_events(&locator, events(device, 2, "a"));
    let server = start_server(tmp.path(), device, 4).await;
    let mut c = cfg(&server.url, KEY, 10);
    c.send_content = true;
    let r = upload(&locator, &c).await.unwrap();
    assert_eq!(r.accepted, 2);
    // The client sent content; the server's metadata_only ceiling removed it.
    assert_eq!(r.stripped_content, 2);
    assert!(server.tenant_events().iter().all(|e| e.content.is_none()));
    server.stop().await;
}

fn events_for(device: DeviceId, remote: &str, n: usize, tag: &str) -> Vec<Event> {
    (0..n)
        .map(|i| {
            let mut ev = Event::new(
                device,
                Provider::ClaudeCode,
                "PostToolUse",
                EventKind::ToolCallFinished,
                ProjectRef::derive("/home/dev/work/repo", Some(remote), &device),
                format!("session-{tag}"),
                CaptureMode::LocalSemantic,
                "sync-test/0.1",
            );
            ev.attrs.insert("x_test_index".into(), json!(i));
            ev.content = Some(EventContent {
                command: Some(format!(
                    "curl -H 'Authorization: Bearer ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZabcdef0123' https://x/{i}"
                )),
                ..Default::default()
            });
            ev
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn excluded_repositories_never_leave_the_device_and_secrets_never_do() {
    let tmp = tempfile::tempdir().unwrap();
    let (locator, device) = local_db(tmp.path());
    let mut all = events_for(device, "github.com/acme/public", 3, "pub");
    all.extend(events_for(device, "github.com/acme/private", 2, "priv"));
    write_events(&locator, all);
    let server = start_server(tmp.path(), device, 4).await;

    let mut c = cfg(&server.url, KEY, 10);
    c.exclude = vec!["github.com/acme/private".into()];
    c.send_content = true;
    let r = upload(&locator, &c).await.unwrap();
    assert_eq!(
        (r.pending_before, r.accepted),
        (3, 3),
        "only the public repo uploads"
    );
    assert_eq!(
        r.secrets_redacted, 3,
        "one token per event, redacted on the device"
    );
    // The cursor covers the excluded events too: they are not re-examined.
    assert_eq!(r.cursor, 5);

    let stored = server.tenant_events();
    assert_eq!(stored.len(), 3);
    assert!(
        stored
            .iter()
            .all(|e| e.project.repo_remote.as_deref() == Some("github.com/acme/public"))
    );
    // The local copy is untouched: redaction happened on the wire copy.
    let local = ingest::open_reader(&locator)
        .unwrap()
        .scan(&ScanFilter::default())
        .unwrap();
    assert!(local.iter().all(|e| {
        e.content
            .as_ref()
            .unwrap()
            .command
            .as_ref()
            .unwrap()
            .contains("ghp_")
    }));

    let r = upload(&locator, &c).await.unwrap();
    assert_eq!(r.pending_before, 0);
    server.stop().await;
}

// ---------------------------------------------------------------------------
// Inference sync (RFC 0006 §10.7)
// ---------------------------------------------------------------------------

use attemptdb_capture::sync::{
    InferenceItem, InferenceSet, InferenceSource, inference_batch_body, prepare_inferences,
    upload_once_with,
};
use attemptdb_core::Timestamp;
use serde_json::Value;
use std::sync::Arc;

/// A source shaped like the projector's output: one attempt with provenance
/// and a prompt in `objective`, one without evidence, one of a kind that is
/// not synced.
fn test_source() -> InferenceSource {
    InferenceSource(Arc::new(|events: &[Event]| {
        let evidence: Vec<_> = events.iter().map(|e| e.event_id).collect();
        let mk = |kind: &str, id: &str, ev: Vec<attemptdb_core::EventId>| InferenceItem {
            kind: kind.into(),
            id: id.into(),
            session_id: None,
            project_id: None,
            evidence: ev,
            confidence: 0.9,
            algorithm_version: "test-v0".into(),
            fields: json!({ "objective": "the user's prompt", "approach": "edit src/lib.rs" }),
        };
        Ok(InferenceSet {
            algorithm_version: "test-v0".into(),
            computed_at: Timestamp::now(),
            items: vec![
                mk("attempt", "att_1", evidence.clone()),
                mk("attempt", "att_2", vec![]),
                mk("causal_edge", "edge_1", evidence),
            ],
        })
    }))
}

async fn get_json(url: String, key: &str) -> (u16, Value) {
    let key = key.to_string();
    tokio::task::spawn_blocking(move || {
        let parse = |r: ureq::Response| -> (u16, Value) {
            let status = r.status();
            let text = r.into_string().unwrap_or_default();
            (status, serde_json::from_str(&text).unwrap_or(Value::Null))
        };
        match ureq::get(&url)
            .set("Authorization", &format!("Bearer {key}"))
            .call()
        {
            Ok(r) => parse(r),
            Err(ureq::Error::Status(_, r)) => parse(r),
            Err(e) => panic!("{e}"),
        }
    })
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inferences_leave_only_with_provenance_and_without_content() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let (locator, device) = local_db(root);
    write_events(&locator, events(device, 3, "inf"));
    let server = start_server(root, device, 2).await;
    let mut c = cfg(&server.url, KEY, 100);
    c.send_inferences = true;
    let source = test_source();

    let (l, cc, src) = (locator.clone(), c.clone(), source.clone());
    let report = tokio::task::spawn_blocking(move || upload_once_with(&l, &cc, Some(&src)))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(report.accepted, 3, "facts still upload first");
    let inf = report.inferences.expect("inference half ran");
    assert_eq!(
        inf.items, 1,
        "no evidence → dropped; causal_edge → not synced"
    );
    assert_eq!(inf.kinds, 1);
    assert_eq!(inf.uploaded, 1);
    assert_eq!(inf.rejected, 0);
    assert_eq!(
        inf.content_removed, 1,
        "objective stripped: send_content is off"
    );
    assert!(!inf.unchanged);

    // The stored document carries the provenance and not the prompt, and it
    // lives beside the tenant's event database, never inside it.
    let (status, doc) = get_json(format!("{}/v1/inferences?kind=attempt", server.url), KEY).await;
    assert_eq!(status, 200, "{doc}");
    assert_eq!(doc["schema"], json!("attemptdb.inference/v1"));
    assert_eq!(doc["algorithm_version"], json!("test-v0"));
    let items = doc["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["id"], json!("att_1"));
    assert_eq!(items[0]["evidence"].as_array().unwrap().len(), 3);
    assert_eq!(items[0]["confidence"], json!(0.9));
    assert!(items[0]["fields"]["objective"].is_null());
    assert_eq!(items[0]["fields"]["approach"], json!("edit src/lib.rs"));
    assert!(
        server
            .data_dir
            .join("tenants/t1/inferences")
            .join(device.to_string())
            .join("attempt.json")
            .is_file()
    );
    assert_eq!(server.tenant_events().len(), 3);
    let (status, summary) = get_json(format!("{}/v1/inferences", server.url), KEY).await;
    assert_eq!(status, 200);
    assert_eq!(summary["kinds"][0]["kind"], json!("attempt"));
    assert_eq!(summary["kinds"][0]["items"], json!(1));
    let (status, _) = get_json(format!("{}/v1/inferences?kind=handoff", server.url), KEY).await;
    assert_eq!(status, 404);

    // Same set again: nothing is re-sent.
    let (l, cc, src) = (locator.clone(), c.clone(), source.clone());
    let again = tokio::task::spawn_blocking(move || upload_once_with(&l, &cc, Some(&src)))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(again.pending_before, 0);
    assert!(again.inferences.unwrap().unchanged);

    // Off by default: the flag is the only way inferences leave.
    let mut off = c.clone();
    off.send_inferences = false;
    let (l, src) = (locator.clone(), source.clone());
    let plain = tokio::task::spawn_blocking(move || upload_once_with(&l, &off, Some(&src)))
        .await
        .unwrap()
        .unwrap();
    assert!(plain.inferences.is_none());
    server.stop().await;
}

#[test]
fn inference_uploads_match_the_published_schema() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let text = std::fs::read_to_string(root.join("spec/inference-v1.schema.json")).unwrap();
    let schema: Value = serde_json::from_str(&text).unwrap();
    let validator = jsonschema::validator_for(&schema).expect("valid schema");
    let device = DeviceId::derive(&["spec", "inference"]);
    let policy = cfg("https://x", "k", 1);
    let item = |id: &str, evidence: usize, confidence: f32| InferenceItem {
        kind: "attempt".into(),
        id: id.into(),
        session_id: Some("ses_1".into()),
        project_id: None,
        evidence: (0..evidence)
            .map(|_| attemptdb_core::EventId::new())
            .collect(),
        confidence,
        algorithm_version: "tier1-v0".into(),
        fields: json!({ "objective": "prompt", "approach": "edit" }),
    };
    let (good, _) = prepare_inferences(&policy, vec![item("att_1", 2, 0.9), item("att_2", 1, 0.4)]);
    let refs: Vec<&InferenceItem> = good.iter().collect();
    let body = inference_batch_body(device, "attempt", "tier1-v0", Timestamp::now(), &refs);
    let errors: Vec<String> = validator
        .iter_errors(&body)
        .map(|e| e.to_string())
        .collect();
    assert!(errors.is_empty(), "{errors:?}\n{body}");

    // What the schema refuses is what the server refuses.
    let bad = item("att_3", 0, 1.5);
    let mut wire = serde_json::to_value(&bad).unwrap();
    let mut body = inference_batch_body(device, "attempt", "tier1-v0", Timestamp::now(), &[]);
    body["items"] = json!([wire.clone()]);
    assert!(
        validator.validate(&body).is_err(),
        "empty evidence and confidence > 1 must fail"
    );
    wire["evidence"] = json!(["evt_1"]);
    wire["confidence"] = json!(0.5);
    body["items"] = json!([wire]);
    assert!(validator.validate(&body).is_ok());
    body["kind"] = json!("causal_edge");
    assert!(
        validator.validate(&body).is_err(),
        "unsynced kinds are not in the schema"
    );
}
