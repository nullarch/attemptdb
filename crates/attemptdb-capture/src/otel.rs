//! Authenticated loopback OTLP receiver. Uses the daemon's existing durable
//! writer and sync path. It never sends telemetry to a third-party backend.

use crate::{config::Config, daemon::WriterCmd, ipc::IngestAck, locator::Locator};
use attemptdb_adapters::{
    CaptureContext,
    otel::{self, Signal},
};
use attemptdb_core::{DeviceId, ProjectRef, Timestamp, event::Provider};
use axum::{
    Json, Router,
    body::Bytes,
    extract::{DefaultBodyLimit, Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::{Semaphore, mpsc, oneshot};

pub const CONFIG_FILE: &str = "otel.json";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReceiverConfig {
    pub port: u16,
    pub token: String,
}
impl ReceiverConfig {
    pub fn path(locator: &Locator) -> PathBuf {
        locator.paths.config_dir.join(CONFIG_FILE)
    }
    pub fn load(locator: &Locator) -> anyhow::Result<Option<Self>> {
        let path = Self::path(locator);
        if !path.exists() {
            return Ok(None);
        }
        let value: Self = serde_json::from_slice(&std::fs::read(path)?)
            .map_err(|_| anyhow::anyhow!("invalid local telemetry receiver configuration"))?;
        anyhow::ensure!(
            value.port != 0
                && value.token.len() >= 32
                && value.token.bytes().all(|b| b.is_ascii_hexdigit()),
            "invalid local telemetry receiver configuration"
        );
        Ok(Some(value))
    }
    pub fn endpoint(&self, provider: &str, signal: &str) -> String {
        format!("http://127.0.0.1:{}/{provider}/v1/{signal}", self.port)
    }
    pub fn health(&self) -> String {
        format!("http://127.0.0.1:{}/health", self.port)
    }
}

#[derive(Clone)]
struct Receiver {
    locator: Locator,
    config: ReceiverConfig,
    device: DeviceId,
    writer: mpsc::Sender<WriterCmd>,
    capacity: Arc<Semaphore>,
    receipts: Arc<Mutex<BTreeMap<String, Value>>>,
}

fn authorised(headers: &HeaderMap, config: &ReceiverConfig) -> bool {
    // No CORS or browser-origin requests. The per-install secret is only
    // written to user-private agent settings and is never a sync credential.
    !headers.contains_key("origin")
        && headers.get("authorization").and_then(|h| h.to_str().ok())
            == Some(format!("Bearer {}", config.token).as_str())
}

async fn health(State(state): State<Receiver>, headers: HeaderMap) -> Response {
    if !authorised(&headers, &state.config) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    Json(json!({"service":"attemptdb-otel", "version":env!("CARGO_PKG_VERSION"), "configured":true, "running":true, "providers":*state.receipts.lock().unwrap_or_else(|e|e.into_inner())})).into_response()
}

async fn ingest(
    State(state): State<Receiver>,
    Path((provider, signal)): Path<(String, String)>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Response {
    if !authorised(&headers, &state.config) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let content_type = headers
        .get("content-type")
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    if content_type.split(';').next().map(str::trim) != Some("application/json")
        || headers.contains_key("content-encoding")
    {
        return (
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "OTLP/HTTP JSON without compression is required",
        )
            .into_response();
    }
    let provider = match provider.as_str() {
        "claude_code" => Provider::ClaudeCode,
        "codex" => Provider::Codex,
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    let signal = match signal.as_str() {
        "logs" => Signal::Logs,
        "metrics" => Signal::Metrics,
        "traces" => Signal::Traces,
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    let Ok(_permit) = state.capacity.clone().try_acquire_owned() else {
        return StatusCode::TOO_MANY_REQUESTS.into_response();
    };
    let Ok(payload) = serde_json::from_slice::<Value>(&bytes) else {
        return (StatusCode::BAD_REQUEST, "invalid OTLP JSON").into_response();
    };
    let config = Config::load_or_default(&state.locator.paths.config_dir);
    let ctx = CaptureContext {
        device_id: state.device,
        capture_mode: config.capture_mode,
        project: ProjectRef::derive("otel/unattributed", None, &state.device),
        captured_at: Timestamp::now(),
        provider_version: None,
        hook_version: None,
    };
    let batch = match otel::normalise(&ctx, provider.clone(), signal, &payload) {
        Ok(mut batch) => {
            if !config.keep_raw_payload {
                for e in &mut batch.events {
                    e.raw = None;
                }
            }
            batch
        }
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                "invalid or oversized OTLP record batch",
            )
                .into_response();
        }
    };
    let latest = batch.events.iter().map(|e| e.observed_at).max();
    let rejected = batch.rejected;
    let (reply, rx) = oneshot::channel();
    if state
        .writer
        .send(WriterCmd::Ingest {
            events: batch.events,
            reply,
        })
        .await
        .is_err()
    {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let ack: IngestAck = match tokio::time::timeout(Duration::from_secs(20), rx).await {
        Ok(Ok(Ok(ack))) => ack,
        _ => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };
    let rejected = rejected + ack.rejected.len();
    {
        let mut receipts = state.receipts.lock().unwrap_or_else(|e| e.into_inner());
        let key = format!("{}:{}", provider.as_str(), signal.as_str());
        let receipt = receipts
            .entry(key)
            .or_insert_with(|| json!({"requests":0,"accepted":0,"duplicates":0,"rejected":0}));
        for (field, count) in [
            ("requests", 1),
            ("accepted", ack.accepted.len()),
            ("duplicates", ack.duplicate.len()),
            ("rejected", rejected),
        ] {
            receipt[field] = json!(receipt[field].as_u64().unwrap_or(0) + count as u64);
        }
        receipt["last_received_at"] = json!(Timestamp::now().to_rfc3339());
        receipt["last_observed_at"] = json!(latest.map(|t| t.to_rfc3339()));
    }
    let response = if rejected == 0 {
        json!({})
    } else {
        let key = match signal {
            Signal::Logs => "rejectedLogRecords",
            Signal::Metrics => "rejectedDataPoints",
            Signal::Traces => "rejectedSpans",
        };
        json!({"partialSuccess":{key:rejected.to_string(),"errorMessage":"Records with invalid timestamps or metadata were rejected"}})
    };
    Json(response).into_response()
}

/// Poll configuration so installing hooks into a running daemon enables
/// collection without interrupting the coding agent or restarting capture.
pub(crate) async fn serve_configured(
    locator: Locator,
    device: DeviceId,
    writer: mpsc::Sender<WriterCmd>,
) {
    let log = crate::daemon::Logger::open(&crate::daemon::log_path(&locator), false);
    let mut last_issue = None;
    loop {
        let config = match ReceiverConfig::load(&locator) {
            Ok(Some(c)) => c,
            Ok(None) => {
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
            Err(_) => {
                let issue =
                    "OTel receiver configuration is invalid; run attempt doctor".to_string();
                if last_issue.as_ref() != Some(&issue) {
                    log.warn(&issue);
                    last_issue = Some(issue);
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
        };
        let listener =
            match tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, config.port)).await
            {
                Ok(l) => l,
                Err(error) => {
                    let issue = format!(
                        "OTel receiver cannot bind 127.0.0.1:{}: {error}",
                        config.port
                    );
                    if last_issue.as_ref() != Some(&issue) {
                        log.warn(&issue);
                        last_issue = Some(issue);
                    }
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    continue;
                }
            };
        log.info(format!(
            "OTel receiver listening on 127.0.0.1:{} (authenticated HTTP JSON)",
            config.port
        ));
        last_issue = None;
        let state = Receiver {
            locator: locator.clone(),
            config: config.clone(),
            device,
            writer: writer.clone(),
            capacity: Arc::new(Semaphore::new(16)),
            receipts: Arc::new(Mutex::new(BTreeMap::new())),
        };
        let app = Router::new()
            .route("/health", get(health))
            .route("/{provider}/v1/{signal}", post(ingest))
            .layer(DefaultBodyLimit::max(4 * 1024 * 1024))
            .with_state(state);
        let run = axum::serve(listener, app);
        let changed = async {
            loop {
                tokio::time::sleep(Duration::from_secs(2)).await;
                if ReceiverConfig::load(&locator).ok().flatten().as_ref() != Some(&config) {
                    break;
                }
            }
        };
        tokio::select! {_ = run => {}, _ = changed => {}}
    }
}

pub fn probe(locator: &Locator) -> anyhow::Result<Value> {
    let Some(config) = ReceiverConfig::load(locator)? else {
        return Ok(json!({"configured":false,"running":false}));
    };
    let response = ureq::get(&config.health())
        .set("Authorization", &format!("Bearer {}", config.token))
        .timeout(Duration::from_secs(2))
        .call();
    match response {
        Ok(r) => Ok(serde_json::from_reader(r.into_reader())?),
        Err(_) => Ok(json!({"configured":true,"running":false,"port":config.port})),
    }
}

/// Cache exact hook session identities, never a guess based on which agent
/// was most recently active. Misses retry after the next spool import.
#[derive(Default)]
pub(crate) struct SessionProjects {
    entries:
        BTreeMap<(attemptdb_core::SessionId, DeviceId), (Option<ProjectRef>, std::time::Instant)>,
}
impl SessionProjects {
    pub(crate) fn resolve(
        &mut self,
        db: &attemptdb_storage::Database,
        events: &mut [attemptdb_core::Event],
    ) {
        let mut lookups = 0;
        for event in events.iter_mut().filter(|e| e.is_telemetry()) {
            if event
                .attrs
                .get("x_otel_session_attributed")
                .and_then(Value::as_bool)
                != Some(true)
            {
                continue;
            }
            let key = (event.session_id, event.device_id);
            let cached = self.entries.get(&key);
            let retry = cached.is_none_or(|(project, at)| {
                project.is_none() && at.elapsed() >= Duration::from_secs(5)
            });
            if retry && lookups < 32 {
                lookups += 1;
                let project = session_project(db, event.session_id, event.device_id)
                    .ok()
                    .flatten();
                if self.entries.len() >= 4096 {
                    self.entries.clear();
                }
                self.entries
                    .insert(key, (project, std::time::Instant::now()));
            }
            if let Some((Some(project), _)) = self.entries.get(&key) {
                event.project = project.clone();
                event
                    .attrs
                    .insert("x_otel_project_attributed".into(), json!(true));
            } else {
                event
                    .attrs
                    .insert("x_otel_project_attributed".into(), json!(false));
            }
        }
    }
}

/// Project identity is metadata. A full event scan decrypts historical
/// prompts/tool output before filtering and can exhaust the SDK's export
/// timeout while blocking the daemon writer. Filter Arrow columns first and
/// decode only matching hooks, without ever resolving content blobs.
fn session_project(
    db: &attemptdb_storage::Database,
    session: attemptdb_core::SessionId,
    device: DeviceId,
) -> attemptdb_storage::Result<Option<ProjectRef>> {
    use attemptdb_core::EventKind;
    use attemptdb_storage::{ScanFilter, segment};
    let filter = ScanFilter {
        session_id: Some(session),
        kinds: vec![
            EventKind::SessionStarted,
            EventKind::PromptSubmitted,
            EventKind::ToolCallStarted,
            EventKind::ToolCallFinished,
            EventKind::ToolCallFailed,
            EventKind::SessionEnded,
        ],
        ..Default::default()
    };
    let matches = |e: &&attemptdb_core::Event| {
        e.device_id == device && e.session_id == session && filter.kinds.contains(&e.kind)
    };
    let mut latest = db
        .memtable_events()
        .iter()
        .filter(matches)
        .max_by_key(|e| (e.hlc, e.source_seq))
        .map(|e| ((e.hlc, e.source_seq), e.project.clone()));
    for seg in &db.manifest().segments {
        for batch in
            segment::read_segment_batches(&segment::segments_dir(db.root()).join(&seg.file))?
        {
            let Some(batch) = filter.filter_batch(&batch)? else {
                continue;
            };
            for event in segment::batch_to_events(&batch)? {
                let order = (event.hlc, event.source_seq);
                if event.device_id == device && latest.as_ref().is_none_or(|(at, _)| order > *at) {
                    latest = Some((order, event.project));
                }
            }
        }
    }
    Ok(latest.map(|(_, project)| project))
}

#[cfg(test)]
mod tests {
    use super::*;
    use attemptdb_core::{CaptureMode, Event, EventKind, event::EventContent};
    use attemptdb_storage::{
        Database, OpenOptions,
        blobs::{KeyId, KeyProvider, MasterKey, StaticKeyProvider},
    };
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountedKeys {
        keys: StaticKeyProvider,
        reads: AtomicUsize,
    }
    impl KeyProvider for CountedKeys {
        fn key(&self, id: KeyId) -> Option<MasterKey> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            self.keys.key(id)
        }
        fn current(&self) -> Option<(KeyId, MasterKey)> {
            self.keys.current()
        }
    }

    fn hook(device: DeviceId, path: &str) -> Event {
        let mut event = Event::new(
            device,
            Provider::ClaudeCode,
            "SessionStart",
            EventKind::SessionStarted,
            ProjectRef::derive(path, None, &device),
            "same-provider-session",
            CaptureMode::LocalSemantic,
            "fixture",
        );
        event.content = Some(EventContent {
            command: Some("PRIVATE_CONTENT_MUST_NOT_BE_READ".repeat(128)),
            ..Default::default()
        });
        event
    }

    #[test]
    fn project_lookup_never_decrypts_history_and_keeps_devices_separate() {
        let tmp = tempfile::tempdir().unwrap();
        let keys = Arc::new(CountedKeys {
            keys: StaticKeyProvider::with_current([37; 32]),
            reads: AtomicUsize::new(0),
        });
        let mut db = Database::open(
            &tmp.path().join("db"),
            OpenOptions {
                create: true,
                keys: Some(keys.clone()),
                ..Default::default()
            },
        )
        .unwrap();
        let a = hook(DeviceId::new(), "/home/dev/first");
        let mut b = hook(DeviceId::new(), "/home/dev/other-device");
        b.session_id = a.session_id;
        db.ingest(vec![a.clone(), b.clone()]).unwrap();
        db.flush().unwrap();
        keys.reads.store(0, Ordering::SeqCst);

        let mut observations = [a.clone(), b.clone()];
        for event in &mut observations {
            event.kind = EventKind::Unknown;
            event.attrs.insert("source".into(), json!("otel"));
            event
                .attrs
                .insert("x_otel_session_attributed".into(), json!(true));
            event.project = ProjectRef::derive("otel/unattributed", None, &event.device_id);
        }
        SessionProjects::default().resolve(&db, &mut observations);
        assert_eq!(observations[0].project, a.project);
        assert_eq!(observations[1].project, b.project);
        assert_eq!(keys.reads.load(Ordering::SeqCst), 0);

        // Fresh hooks in the memtable take precedence over older segments.
        let mut latest = hook(a.device_id, "/home/dev/moved-project");
        latest.session_id = a.session_id;
        db.ingest(vec![latest.clone()]).unwrap();
        assert_eq!(
            session_project(&db, a.session_id, a.device_id).unwrap(),
            Some(latest.project)
        );
        assert_eq!(
            session_project(&db, a.session_id, DeviceId::new()).unwrap(),
            None
        );
        assert_eq!(keys.reads.load(Ordering::SeqCst), 0);

        // Prove this fixture contains readable encrypted blobs, so the zero
        // key reads above detect accidental use of a content-resolving scan.
        db.scan(&attemptdb_storage::ScanFilter::default()).unwrap();
        assert!(keys.reads.load(Ordering::SeqCst) > 0);
    }
}
