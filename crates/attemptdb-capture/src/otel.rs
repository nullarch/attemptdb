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
    collections::{BTreeMap, HashMap},
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
    let dropped = batch.dropped;
    // A batch whose every record was discarded (or rejected) has nothing to
    // store: acknowledge it without waking the single writer.
    let ack: IngestAck = if batch.events.is_empty() {
        IngestAck::default()
    } else {
        let (reply, rx) = oneshot::channel();
        if state
            .writer
            .send(WriterCmd::Ingest {
                events: batch.events,
                reply,
                spool_on_hold: true,
            })
            .await
            .is_err()
        {
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
        match tokio::time::timeout(Duration::from_secs(20), rx).await {
            Ok(Ok(Ok(ack))) => ack,
            _ => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
        }
    };
    let rejected = rejected + ack.rejected.len();
    {
        let mut receipts = state.receipts.lock().unwrap_or_else(|e| e.into_inner());
        let key = format!("{}:{}", provider.as_str(), signal.as_str());
        let receipt = receipts.entry(key).or_insert_with(
            || json!({"requests":0,"accepted":0,"duplicates":0,"rejected":0,"dropped":0}),
        );
        for (field, count) in [
            ("requests", 1),
            ("accepted", ack.accepted.len()),
            ("duplicates", ack.duplicate.len()),
            ("rejected", rejected),
            // Read correctly, not kept: see `otel::retained`.
            ("dropped", dropped),
        ] {
            receipt[field] = json!(receipt[field].as_u64().unwrap_or(0) + count as u64);
        }
        receipt["last_received_at"] = json!(Timestamp::now().to_rfc3339());
        // A batch that was entirely discarded observed nothing to store: keep
        // the previous observation time rather than overwriting it with null.
        if latest.is_some() || receipt.get("last_observed_at").is_none() {
            receipt["last_observed_at"] = json!(latest.map(|t| t.to_rfc3339()));
        }
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

/// Hook events that say which project a session works in: the lifecycle
/// and tool events a hook writes. Telemetry rows never do.
fn names_project(kind: attemptdb_core::EventKind) -> bool {
    use attemptdb_core::EventKind::*;
    matches!(
        kind,
        SessionStarted
            | PromptSubmitted
            | ToolCallStarted
            | ToolCallFinished
            | ToolCallFailed
            | SessionEnded
    )
}

/// The most sessions whose project is remembered. A session older than that
/// many newer ones stops being resolved (its telemetry is stored with
/// `x_otel_project_attributed = false`, as for a session no hook ever
/// named); a real database holds hundreds to a few thousand.
const MAX_TRACKED_SESSIONS: usize = 200_000;

/// Which project each hook session works in, kept by the writer thread, so
/// attributing an OTel record is a map lookup and never a read of the
/// database.
///
/// The map is seeded once, from the project columns of every segment (and
/// the memtable) and nothing else: no content, raw or attrs column is
/// decoded and no encryption key is asked for, so seeding a database of
/// millions of events costs about a tenth of a second per million. After
/// that the writer feeds it every hook event it stores ([`observe`]).
/// Because the map is complete, a session it does not know has no hook
/// event in the database: that answer is final until one arrives, so an
/// unattributed session is not looked up again (it used to be re-read from
/// every segment every five seconds, stalling the writer 1.2 s each time
/// and every hook acknowledgement behind it). Exact hook session
/// identities only, never a guess based on which agent was most recently
/// active.
///
/// [`observe`]: SessionProjects::observe
#[derive(Default)]
pub(crate) struct SessionProjects {
    /// False until the first lookup; then the map is complete and kept
    /// current.
    seeded: bool,
    /// When a seeding attempt last failed outright (the next lookup tries
    /// again after [`RESEED_AFTER_FAILURE`], never on every record).
    failed_at: Option<std::time::Instant>,
    entries: HashMap<(attemptdb_core::SessionId, DeviceId), Tracked>,
    /// Segment files read by seeding (a diagnostic: it stops growing once
    /// the map is seeded).
    pub(crate) segment_reads: usize,
}

struct Tracked {
    observed_at: Timestamp,
    project: ProjectRef,
}

const RESEED_AFTER_FAILURE: Duration = Duration::from_secs(60);

/// Whether a segment read error is damage to the file rather than a
/// failure that may pass.
fn segment_is_gone_for_good(e: &attemptdb_storage::StorageError) -> bool {
    use attemptdb_storage::StorageError as E;
    match e {
        E::Corrupt { .. } | E::UnsupportedFormat { .. } | E::Arrow(_) | E::Json(_) | E::Core(_) => {
            true
        }
        // The writer holds the database exclusively: a listed segment that
        // is not there will not come back.
        E::Io { source, .. } => source.kind() == std::io::ErrorKind::NotFound,
        _ => false,
    }
}

impl SessionProjects {
    /// Feed the map the events the writer is about to store. Free until the
    /// map is seeded (seeding reads the database as it is then, memtable
    /// included, so nothing stored earlier is missed).
    pub(crate) fn observe(&mut self, events: &[attemptdb_core::Event]) {
        if !self.seeded {
            return;
        }
        for event in events.iter().filter(|e| names_project(e.kind)) {
            self.note(
                (event.session_id, event.device_id),
                event.observed_at,
                &event.project,
            );
        }
    }

    /// Remember `project` for `key` unless the map holds a newer hook
    /// event's. Equal times take the later arrival, as a stream does.
    fn note(
        &mut self,
        key: (attemptdb_core::SessionId, DeviceId),
        observed_at: Timestamp,
        project: &ProjectRef,
    ) {
        match self.entries.get_mut(&key) {
            Some(held) if held.observed_at > observed_at => {}
            Some(held) => {
                held.observed_at = observed_at;
                if held.project != *project {
                    held.project = project.clone();
                }
            }
            None => {
                if self.entries.len() >= MAX_TRACKED_SESSIONS {
                    self.forget_oldest();
                }
                self.entries.insert(
                    key,
                    Tracked {
                        observed_at,
                        project: project.clone(),
                    },
                );
            }
        }
    }

    /// Drop the quarter of the sessions whose latest hook event is oldest.
    fn forget_oldest(&mut self) {
        let mut times: Vec<Timestamp> = self.entries.values().map(|t| t.observed_at).collect();
        times.sort_unstable();
        let cutoff = times[times.len() / 4];
        self.entries.retain(|_, t| t.observed_at > cutoff);
    }

    /// Read the project columns of the memtable and of every segment.
    fn seed(&mut self, db: &attemptdb_storage::Database) -> attemptdb_storage::Result<()> {
        use attemptdb_storage::segment::{self, Cols, col};
        for event in db
            .memtable_events()
            .iter()
            .filter(|e| names_project(e.kind))
        {
            self.note(
                (event.session_id, event.device_id),
                event.observed_at,
                &event.project,
            );
        }
        let dir = segment::segments_dir(db.root());
        let columns = [
            col::SESSION_ID,
            col::DEVICE_ID,
            col::KIND,
            col::OBSERVED_AT,
            col::PROJECT_ID,
            col::PROJECT_ROOT,
            col::PROJECT_NAME,
            col::REPO_REMOTE,
            col::BRANCH,
            col::HEAD,
        ];
        let mut first_error = None;
        for seg in &db.manifest().segments {
            self.segment_reads += 1;
            let read =
                segment::for_each_segment_columns(&dir.join(&seg.file), &columns, &mut |b| {
                    let cols = Cols::new(b)?;
                    for row in 0..cols.num_rows() {
                        let named = cols
                            .str_ref(col::KIND, row)
                            .and_then(attemptdb_core::EventKind::parse)
                            .is_some_and(names_project);
                        let (Some(session), Some(device)) = (
                            cols.fsb(col::SESSION_ID, row),
                            cols.fsb(col::DEVICE_ID, row),
                        ) else {
                            continue;
                        };
                        if !named {
                            continue;
                        }
                        let key = (
                            attemptdb_core::SessionId::from_bytes(session),
                            DeviceId::from_bytes(device),
                        );
                        let at = cols.ts(col::OBSERVED_AT, row).unwrap_or_default();
                        if self.entries.get(&key).is_some_and(|t| t.observed_at > at) {
                            continue;
                        }
                        let project = ProjectRef {
                            project_id: attemptdb_core::ProjectId::from_bytes(
                                cols.fsb(col::PROJECT_ID, row).unwrap_or([0; 16]),
                            ),
                            root: cols.s(col::PROJECT_ROOT, row).unwrap_or_default(),
                            name: cols.s(col::PROJECT_NAME, row).unwrap_or_default(),
                            repo_remote: cols.s(col::REPO_REMOTE, row),
                            branch: cols.s(col::BRANCH, row),
                            head: cols.s(col::HEAD, row),
                        };
                        self.note(key, at, &project);
                    }
                    Ok(true)
                });
            // A segment that is damaged for good is skipped: its sessions
            // are resolved from whatever else names them, like a session no
            // hook named. A failure that may pass (an I/O error) fails the
            // seeding, which is tried again later; nothing is half-trusted.
            if let Err(e) = read
                && !segment_is_gone_for_good(&e)
            {
                first_error.get_or_insert(e);
            }
        }
        match first_error {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    fn ensure_seeded(&mut self, db: &attemptdb_storage::Database) {
        if self.seeded
            || self
                .failed_at
                .is_some_and(|at| at.elapsed() < RESEED_AFTER_FAILURE)
        {
            return;
        }
        match self.seed(db) {
            Ok(()) => {
                self.seeded = true;
                self.failed_at = None;
            }
            Err(_) => self.failed_at = Some(std::time::Instant::now()),
        }
    }

    /// Give every attributed telemetry record the project of its session's
    /// latest hook event, or mark it unattributed.
    pub(crate) fn resolve(
        &mut self,
        db: &attemptdb_storage::Database,
        events: &mut [attemptdb_core::Event],
    ) {
        self.ensure_seeded(db);
        for event in events.iter_mut().filter(|e| e.is_telemetry()) {
            if event
                .attrs
                .get("x_otel_session_attributed")
                .and_then(Value::as_bool)
                != Some(true)
            {
                continue;
            }
            match self.entries.get(&(event.session_id, event.device_id)) {
                Some(held) => {
                    event.project = held.project.clone();
                    event
                        .attrs
                        .insert("x_otel_project_attributed".into(), json!(true));
                }
                None => {
                    event
                        .attrs
                        .insert("x_otel_project_attributed".into(), json!(false));
                }
            }
        }
    }
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

    fn receiver(tmp: &std::path::Path, writer: mpsc::Sender<WriterCmd>) -> Receiver {
        Receiver {
            locator: Locator::resolve(tmp, Some(&tmp.join("data")), None),
            config: ReceiverConfig {
                port: 4318,
                token: "a".repeat(32),
            },
            device: DeviceId::new(),
            writer,
            capacity: Arc::new(Semaphore::new(16)),
            receipts: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    fn records(names: &[&str]) -> Vec<u8> {
        let rows: Vec<Value> = names
            .iter()
            .map(|n| {
                json!({"timeUnixNano":"1787904000000000000","body":{"stringValue":n},"attributes":[
                    {"key":"conversation.id","value":{"stringValue":"fixture-session"}}
                ]})
            })
            .collect();
        serde_json::to_vec(&json!({"resourceLogs":[{"scopeLogs":[{"logRecords":rows}]}]})).unwrap()
    }

    async fn post(state: &Receiver, provider: &str, body: Vec<u8>) -> (StatusCode, Value) {
        let mut headers = HeaderMap::new();
        headers.insert(
            "authorization",
            format!("Bearer {}", state.config.token).parse().unwrap(),
        );
        headers.insert("content-type", "application/json".parse().unwrap());
        let response = ingest(
            State(state.clone()),
            Path((provider.to_string(), "logs".to_string())),
            headers,
            Bytes::from(body),
        )
        .await;
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    fn receipt(state: &Receiver, key: &str) -> Value {
        state.receipts.lock().unwrap()[key].clone()
    }

    /// A batch of nothing but discarded families is acknowledged as received
    /// (HTTP 200, no `partialSuccess`), counted as `dropped`, and never wakes
    /// the single writer; a batch with something to keep still reaches it.
    #[test]
    fn a_batch_with_nothing_left_is_acknowledged_without_waking_the_writer() {
        let tmp = tempfile::tempdir().unwrap();
        let (tx, mut rx) = mpsc::channel::<WriterCmd>(4);
        let state = receiver(tmp.path(), tx);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        runtime.block_on(async {
            let discarded = [
                "codex.sse_event",
                "codex.sqlite.logs.write.count",
                "codex.sqlite.logs.write.bytes",
                "hook_execution_start",
                "hook_execution_complete",
            ];
            let (status, body) = post(&state, "codex", records(&discarded)).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(body, json!({}), "no partial success: nothing was rejected");
            assert!(
                matches!(rx.try_recv(), Err(mpsc::error::TryRecvError::Empty)),
                "the writer is not woken for a batch with nothing to store"
            );
            let r = receipt(&state, "codex:logs");
            assert_eq!(r["dropped"], discarded.len());
            assert_eq!(
                (r["accepted"].as_u64(), r["rejected"].as_u64()),
                (Some(0), Some(0))
            );

            // One record to keep among discarded ones: the writer is asked
            // for exactly that one.
            let writer = tokio::spawn(async move {
                let Some(WriterCmd::Ingest { events, reply, .. }) = rx.recv().await else {
                    panic!("an ingest was expected");
                };
                let names: Vec<String> = events
                    .iter()
                    .map(|e| e.provider_event_name.clone())
                    .collect();
                let ack = IngestAck {
                    accepted: events.iter().map(|e| e.event_id).collect(),
                    ..Default::default()
                };
                let _ = reply.send(Ok(ack));
                names
            });
            let (status, body) = post(
                &state,
                "codex",
                records(&[
                    "codex.api_request",
                    "codex.sse_event",
                    "hook_execution_start",
                ]),
            )
            .await;
            assert_eq!((status, body), (StatusCode::OK, json!({})));
            assert_eq!(writer.await.unwrap(), ["codex.api_request"]);
            let r = receipt(&state, "codex:logs");
            assert_eq!(
                (r["accepted"].as_u64(), r["dropped"].as_u64()),
                (Some(1), Some(7))
            );
        });
    }

    fn telemetry_for(hook: &Event) -> Event {
        let mut event = hook.clone();
        event.kind = EventKind::Unknown;
        event.attrs.insert("source".into(), json!("otel"));
        event
            .attrs
            .insert("x_otel_session_attributed".into(), json!(true));
        event.project = ProjectRef::derive("otel/unattributed", None, &event.device_id);
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

        let mut observations = [telemetry_for(&a), telemetry_for(&b)];
        let mut projects = SessionProjects::default();
        projects.resolve(&db, &mut observations);
        assert_eq!(observations[0].project, a.project);
        assert_eq!(observations[1].project, b.project);
        assert_eq!(keys.reads.load(Ordering::SeqCst), 0);
        assert_eq!(projects.segment_reads, 1, "seeded from the one segment");

        // A hook stored later takes precedence over the older segment, with
        // no read of the database: the writer feeds the map as it stores.
        let mut latest = hook(a.device_id, "/home/dev/moved-project");
        latest.session_id = a.session_id;
        latest.observed_at = Timestamp::from_micros(a.observed_at.as_micros() + 1_000);
        projects.observe(std::slice::from_ref(&latest));
        let mut again = [telemetry_for(&a)];
        projects.resolve(&db, &mut again);
        assert_eq!(again[0].project, latest.project);
        // ... and an older event (a spooled duplicate imported late) does not
        // move it back.
        projects.observe(std::slice::from_ref(&a));
        let mut once_more = [telemetry_for(&a)];
        projects.resolve(&db, &mut once_more);
        assert_eq!(once_more[0].project, latest.project);
        assert_eq!(projects.segment_reads, 1, "nothing was read again");
        assert_eq!(keys.reads.load(Ordering::SeqCst), 0);

        // Prove this fixture contains readable encrypted blobs, so the zero
        // key reads above detect accidental use of a content-resolving scan.
        db.scan(&attemptdb_storage::ScanFilter::default()).unwrap();
        assert!(keys.reads.load(Ordering::SeqCst) > 0);
    }

    #[test]
    fn a_session_without_hooks_is_unattributed_until_a_hook_arrives() {
        let tmp = tempfile::tempdir().unwrap();
        let mut db = Database::open(
            &tmp.path().join("db"),
            OpenOptions {
                create: true,
                ..Default::default()
            },
        )
        .unwrap();
        let known = hook(db.device_id(), "/home/dev/known");
        db.ingest(vec![known.clone()]).unwrap();
        db.flush().unwrap();
        let mut unknown = hook(db.device_id(), "/home/dev/never-hooked");
        unknown.session_id = attemptdb_core::SessionId::derive(&["no hook ever named me"]);

        let mut projects = SessionProjects::default();
        let mut batch = [telemetry_for(&known), telemetry_for(&unknown)];
        projects.resolve(&db, &mut batch);
        assert_eq!(batch[0].attrs["x_otel_project_attributed"], true);
        assert_eq!(batch[1].attrs["x_otel_project_attributed"], false);
        assert_eq!(
            batch[1].project,
            ProjectRef::derive("otel/unattributed", None, &unknown.device_id)
        );

        // Asking again is a map lookup, however often: no segment read.
        let reads = projects.segment_reads;
        for _ in 0..1_000 {
            let mut batch = [telemetry_for(&unknown)];
            projects.resolve(&db, &mut batch);
            assert_eq!(batch[0].attrs["x_otel_project_attributed"], false);
        }
        assert_eq!(projects.segment_reads, reads);

        // The session's first hook event names it from then on.
        projects.observe(std::slice::from_ref(&unknown));
        let mut batch = [telemetry_for(&unknown)];
        projects.resolve(&db, &mut batch);
        assert_eq!(batch[0].attrs["x_otel_project_attributed"], true);
        assert_eq!(batch[0].project, unknown.project);
    }

    #[test]
    fn a_memtable_hook_names_its_session_at_seeding_and_telemetry_rows_never_do() {
        let tmp = tempfile::tempdir().unwrap();
        let mut db = Database::open(
            &tmp.path().join("db"),
            OpenOptions {
                create: true,
                ..Default::default()
            },
        )
        .unwrap();
        let in_wal = hook(db.device_id(), "/home/dev/in-the-wal");
        let mut otel_only = telemetry_for(&hook(db.device_id(), "/home/dev/x"));
        otel_only.session_id = attemptdb_core::SessionId::derive(&["telemetry only"]);
        db.ingest(vec![in_wal.clone(), otel_only.clone()]).unwrap();
        let mut projects = SessionProjects::default();
        let mut batch = [telemetry_for(&in_wal), otel_only.clone()];
        projects.resolve(&db, &mut batch);
        assert_eq!(batch[0].project, in_wal.project);
        assert_eq!(
            batch[1].attrs["x_otel_project_attributed"], false,
            "an OTel row does not name a project for its session"
        );
        assert_eq!(projects.segment_reads, 0, "no segments, only the memtable");
    }
}
