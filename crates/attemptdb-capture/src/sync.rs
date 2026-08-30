//! Client side of RFC 0006 §10: upload this database's events to a sync
//! server in batches, one batch in flight, in `source_seq` order.
//!
//! The local database stays authoritative. The uploader reads it (read-only,
//! so it coexists with the daemon's writer), sends everything after the last
//! acknowledged `source_seq`, and advances the cursor only on an
//! acknowledgement. A failed batch leaves the cursor where it was; the next
//! run re-sends it, and the server's dedupe makes that a no-op.
//!
//! By default nothing content-bearing leaves the device: every event is
//! clamped to `metadata_only` before it is serialised, which removes
//! `content` and `raw`. `send_content` is the explicit opt-in.

use crate::locator::Locator;
use anyhow::{Context, Result, anyhow, bail};
use attemptdb_core::{CaptureMode, Event, EventId, Timestamp, secrets};
use attemptdb_storage::{Database, OpenOptions, ScanFilter};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

pub const CONFIG_FILE: &str = "sync.json";
pub const DEFAULT_BATCH_EVENTS: usize = 1_000;
pub const DEFAULT_INTERVAL_SECS: u64 = 30;
/// Largest body the server accepts by default (4 MiB); stay well under.
const MAX_BODY_BYTES: usize = 3 * 1024 * 1024;

/// Wire schema of an inference upload (RFC 0006 §10.7, `spec/inference-v1.schema.json`).
pub const INFERENCE_SCHEMA: &str = "attemptdb.inference/v1";
/// Inference kinds that leave the device. Sessions, turns, and tool calls are
/// one-to-one with facts and derivable server-side; causal edges are the
/// largest table and equally derivable. These four are what a reader asks
/// about and what may differ when the device saw content the server did not.
pub const INFERENCE_KINDS: &[&str] = &["attempt", "handoff", "work_unit", "decision"];
/// Most items of one kind per upload; the newest are kept and the count of
/// dropped items is reported, never hidden.
pub const MAX_INFERENCE_ITEMS: usize = 20_000;

fn default_batch() -> usize {
    DEFAULT_BATCH_EVENTS
}
fn default_interval() -> u64 {
    DEFAULT_INTERVAL_SECS
}

/// Where and how to upload. Stored at `<config_dir>/sync.json`, mode 0600.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SyncConfig {
    /// Base URL of the server, e.g. `https://sync.vibemon.dev`.
    pub url: String,
    /// Bearer key issued for this device.
    pub key: String,
    /// Upload `content`/`raw` too. Off by default: metadata only.
    #[serde(default)]
    pub send_content: bool,
    /// Also upload this device's Tier-1 inferences (attempts, handoffs, work
    /// units, decisions), each with its evidence ids, confidence, and
    /// algorithm version. Off by default; inferences never leave without
    /// provenance, and under `send_content == false` their content-bearing
    /// fields (`objective`, `rationale`) are removed first.
    #[serde(default)]
    pub send_inferences: bool,
    #[serde(default = "default_batch")]
    pub batch_events: usize,
    #[serde(default = "default_interval")]
    pub interval_secs: u64,
    /// Repository policy (RFC 0006 §10.5), evaluated on the device. Entries
    /// are normalised remotes (`github.com/owner/repo`) or project ids
    /// (`prj_…`). When `include` is non-empty only those projects upload;
    /// `exclude` always wins. Excluded projects never leave the device —
    /// not even their metadata.
    #[serde(default)]
    pub include: Vec<String>,
    #[serde(default)]
    pub exclude: Vec<String>,
}

impl SyncConfig {
    pub fn path(config_dir: &Path) -> PathBuf {
        config_dir.join(CONFIG_FILE)
    }

    /// `None` when no sync has been configured.
    pub fn load(config_dir: &Path) -> Result<Option<Self>> {
        let path = Self::path(config_dir);
        match std::fs::read_to_string(&path) {
            Ok(text) => Ok(Some(
                serde_json::from_str(&text)
                    .with_context(|| format!("parsing {}", path.display()))?,
            )),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn save(&self, config_dir: &Path) -> Result<()> {
        std::fs::create_dir_all(config_dir)
            .with_context(|| format!("creating {}", config_dir.display()))?;
        let path = Self::path(config_dir);
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)
            .with_context(|| format!("writing {}", tmp.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
        }
        std::fs::rename(&tmp, &path).with_context(|| format!("replacing {}", path.display()))?;
        Ok(())
    }

    /// Returns whether a configuration existed.
    pub fn remove(config_dir: &Path) -> Result<bool> {
        match std::fs::remove_file(Self::path(config_dir)) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    pub fn interval(&self) -> Duration {
        Duration::from_secs(self.interval_secs.max(5))
    }

    fn endpoint_inferences(&self) -> String {
        format!("{}/v1/sync/inferences", self.url.trim_end_matches('/'))
    }

    fn endpoint(&self) -> String {
        format!("{}/v1/sync", self.url.trim_end_matches('/'))
    }

    /// Whether an event's project may be uploaded under this policy.
    pub fn allows(&self, ev: &Event) -> bool {
        let matches = |entry: &String| {
            let e = entry.trim().trim_start_matches("prj_");
            if let Some(remote) = &ev.project.repo_remote
                && remote.eq_ignore_ascii_case(entry.trim())
            {
                return true;
            }
            ev.project.project_id.to_string() == e
        };
        if self.exclude.iter().any(matches) {
            return false;
        }
        self.include.is_empty() || self.include.iter().any(matches)
    }

    /// The key, masked for display.
    pub fn masked_key(&self) -> String {
        let k = &self.key;
        if k.len() <= 8 {
            "••••".to_string()
        } else {
            format!("{}…{}", &k[..4], &k[k.len() - 4..])
        }
    }
}

/// Per-database upload cursor (RFC 0006 §10.1 `sync_state`). Lives under
/// `<data_dir>/sync/<hash of db dir>.json` so several databases on one
/// machine keep separate cursors.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SyncState {
    pub last_acked_source_seq: u64,
    pub last_acked_hlc: u64,
    pub batches: u64,
    pub events: u64,
    pub duplicates: u64,
    pub rejected: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_ok_at: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error_at: Option<Timestamp>,
    /// Inference uploads that reached the server.
    #[serde(default)]
    pub inference_uploads: u64,
    /// Items stored by the last inference upload.
    #[serde(default)]
    pub inference_items: u64,
    /// Digest of the last uploaded inference set; an identical set is not
    /// re-sent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_inference_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_inference_at: Option<Timestamp>,
}

impl SyncState {
    pub fn path(data_dir: &Path, db_dir: &Path) -> PathBuf {
        let digest = Sha256::digest(db_dir.to_string_lossy().as_bytes());
        data_dir
            .join("sync")
            .join(format!("{}.json", hex::encode(&digest[..8])))
    }

    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }
}

/// What one run did.
#[derive(Clone, Debug, Default, Serialize)]
pub struct UploadReport {
    /// Events after the cursor when the run started.
    pub pending_before: usize,
    pub batches: usize,
    pub accepted: usize,
    pub duplicates: usize,
    pub rejected: usize,
    pub redactions: usize,
    pub stripped_content: usize,
    /// Secret spans redacted from content before upload (`--send-content`).
    pub secrets_redacted: usize,
    /// Cursor after the run.
    pub cursor: u64,
    /// Present when `send_inferences` is on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inferences: Option<InferenceReport>,
}

/// One device-computed inference on the wire: what it is, what it was
/// derived from, how sure the algorithm was, and which algorithm.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct InferenceItem {
    /// One of [`INFERENCE_KINDS`].
    pub kind: String,
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    /// Never empty: an inference without evidence is not uploaded.
    pub evidence: Vec<EventId>,
    /// Serialised rounded to four decimals so an `f32` such as `0.9` is
    /// `0.9` on the wire, not `0.8999999761581421`.
    #[serde(serialize_with = "round_confidence")]
    pub confidence: f32,
    pub algorithm_version: String,
    /// The projection row minus the provenance fields above.
    pub fields: Value,
}

fn round_confidence<S: serde::Serializer>(c: &f32, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_f64((f64::from(*c) * 10_000.0).round() / 10_000.0)
}

/// Everything a device computed at one point in time.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InferenceSet {
    pub algorithm_version: String,
    pub computed_at: Timestamp,
    pub items: Vec<InferenceItem>,
}

/// Computes the inference set from the policy-allowed events. Supplied by
/// the binary (the projector lives above this crate), so the uploader stays
/// free of inference code.
pub type InferenceFn = dyn Fn(&[Event]) -> Result<InferenceSet> + Send + Sync;

#[derive(Clone)]
pub struct InferenceSource(pub Arc<InferenceFn>);

impl fmt::Debug for InferenceSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("InferenceSource")
    }
}

/// What the inference half of a run did.
#[derive(Clone, Debug, Default, Serialize, PartialEq, Eq)]
pub struct InferenceReport {
    /// Items computed (after the provenance and kind filters).
    pub items: usize,
    pub kinds: usize,
    /// Stored by the server.
    pub uploaded: usize,
    pub rejected: usize,
    /// Identical to the last upload: nothing was sent.
    pub unchanged: bool,
    /// Items beyond [`MAX_INFERENCE_ITEMS`] per kind, dropped (oldest first).
    pub truncated: usize,
    /// Content-bearing fields removed because `send_content` is off.
    pub content_removed: usize,
}

#[derive(Deserialize)]
struct Ack {
    #[serde(default)]
    accepted: usize,
    #[serde(default)]
    duplicates: usize,
    #[serde(default)]
    rejected: Vec<Value>,
    #[serde(default)]
    redactions: usize,
    #[serde(default)]
    stripped_content: usize,
}

/// Open the database read-only: coexists with a running daemon.
fn open_read_only(locator: &Locator) -> Result<Database> {
    Database::open(
        &locator.db_dir,
        OpenOptions {
            read_only: true,
            ..Default::default()
        },
    )
    .with_context(|| format!("opening {} read-only", locator.db_dir.display()))
}

/// Upload everything after the cursor, one batch at a time, in order.
pub fn upload_once(locator: &Locator, cfg: &SyncConfig) -> Result<UploadReport> {
    upload_once_with(locator, cfg, None)
}

/// [`upload_once`], then — when `send_inferences` is on and a source is
/// supplied — the device's inference set computed from the same
/// policy-allowed events.
pub fn upload_once_with(
    locator: &Locator,
    cfg: &SyncConfig,
    source: Option<&InferenceSource>,
) -> Result<UploadReport> {
    let db = open_read_only(locator)?;
    let device_id = db.device_id();
    let state_path = SyncState::path(&locator.paths.data_dir, &locator.db_dir);
    let mut state = SyncState::load(&state_path)?;

    let all = db.scan(&ScanFilter::default()).context("scanning events")?;
    let newest_seq = all.iter().map(|e| e.source_seq).max().unwrap_or(0);
    let mut allowed: Vec<Event> = all.into_iter().filter(|e| cfg.allows(e)).collect();
    allowed.sort_by_key(|e| e.source_seq);
    drop(db);
    let pending: Vec<Event> = allowed
        .iter()
        .filter(|e| e.source_seq > state.last_acked_source_seq)
        .cloned()
        .collect();

    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(60))
        .build();
    let mut report = upload_events(
        &agent,
        cfg,
        device_id,
        pending,
        newest_seq,
        &mut state,
        &state_path,
    )?;
    if cfg.send_inferences {
        report.inferences = match source {
            Some(source) => Some(upload_inferences(
                &agent,
                cfg,
                device_id,
                &allowed,
                source,
                &mut state,
                &state_path,
            )?),
            // A caller without a projector (a bare uploader): report that
            // nothing was computed rather than pretend.
            None => Some(InferenceReport::default()),
        };
    }
    Ok(report)
}

fn upload_events(
    agent: &ureq::Agent,
    cfg: &SyncConfig,
    device_id: attemptdb_core::DeviceId,
    pending: Vec<Event>,
    newest_seq: u64,
    state: &mut SyncState,
    state_path: &Path,
) -> Result<UploadReport> {
    let mut report = UploadReport {
        pending_before: pending.len(),
        cursor: state.last_acked_source_seq,
        ..Default::default()
    };
    if pending.is_empty() {
        // Everything after the cursor was excluded by policy (or nothing is
        // new): advance the cursor so those events are not re-examined.
        if newest_seq > state.last_acked_source_seq {
            state.last_acked_source_seq = newest_seq;
            state.save(state_path)?;
            report.cursor = newest_seq;
        }
        return Ok(report);
    }
    let capture_mode = if cfg.send_content {
        CaptureMode::LocalSemantic
    } else {
        CaptureMode::MetadataOnly
    };

    let mut batch_size = cfg.batch_events.clamp(1, 5_000);
    let mut start = 0;
    let mut redacted = 0usize;
    while start < pending.len() {
        let end = (start + batch_size).min(pending.len());
        let chunk = &pending[start..end];
        let events: Vec<Event> = chunk
            .iter()
            .cloned()
            .map(|mut e| {
                if cfg.send_content {
                    // Content leaves only on explicit opt-in, and never with
                    // a credential in it (RFC 0006 §5).
                    redacted += e.redact_secrets();
                } else {
                    e.capture_mode = CaptureMode::MetadataOnly;
                    e.apply_capture_mode();
                }
                e
            })
            .collect();
        let body = serde_json::to_vec(&json!({
            "sync_version": 1,
            "device_id": device_id,
            "batch_id": EventId::new().to_string(),
            "capture_mode": capture_mode.as_str(),
            "events": events,
        }))?;
        if body.len() > MAX_BODY_BYTES && chunk.len() > 1 {
            batch_size = (chunk.len() / 2).max(1);
            continue;
        }

        match post(agent, cfg, &body) {
            Ok(ack) => {
                let last = chunk.last().expect("non-empty chunk");
                state.last_acked_source_seq = last.source_seq;
                state.last_acked_hlc = last.hlc.as_u64();
                state.batches += 1;
                state.events += ack.accepted as u64;
                state.duplicates += ack.duplicates as u64;
                state.rejected += ack.rejected.len() as u64;
                state.last_ok_at = Some(Timestamp::now());
                state.last_error = None;
                state.last_error_at = None;
                state.save(state_path)?;
                report.batches += 1;
                report.accepted += ack.accepted;
                report.duplicates += ack.duplicates;
                report.rejected += ack.rejected.len();
                report.redactions += ack.redactions;
                report.stripped_content += ack.stripped_content;
                report.cursor = state.last_acked_source_seq;
                start = end;
            }
            Err(PostError::TooLarge) if chunk.len() > 1 => {
                batch_size = (chunk.len() / 2).max(1);
            }
            Err(e) => {
                state.last_error = Some(e.to_string());
                state.last_error_at = Some(Timestamp::now());
                state.save(state_path)?;
                return Err(anyhow!(
                    "{e} (cursor kept at {})",
                    state.last_acked_source_seq
                ));
            }
        }
    }
    // Every event of the scan was either uploaded or excluded by policy:
    // the cursor covers the whole scan, so excluded events are not
    // re-examined on the next run.
    if newest_seq > state.last_acked_source_seq {
        state.last_acked_source_seq = newest_seq;
        state.save(state_path)?;
        report.cursor = newest_seq;
    }
    report.secrets_redacted = redacted;
    Ok(report)
}

/// Fields of an inference row that carry captured text. Removed unless the
/// device opted into `send_content`.
const CONTENT_FIELDS: &[&str] = &["objective", "rationale"];

/// Null out content-bearing fields; returns how many held a value.
pub fn strip_inference_content(fields: &mut Value) -> usize {
    let Some(obj) = fields.as_object_mut() else {
        return 0;
    };
    let mut n = 0;
    for key in CONTENT_FIELDS {
        if let Some(v) = obj.get_mut(*key)
            && !v.is_null()
        {
            *v = Value::Null;
            n += 1;
        }
    }
    n
}

/// Stable digest of an inference set: sorted by (kind, id), prefixed with
/// the algorithm version, so an unchanged projection is not re-sent.
pub fn inference_digest(algorithm_version: &str, items: &[InferenceItem]) -> Result<String> {
    let mut hasher = Sha256::new();
    hasher.update(algorithm_version.as_bytes());
    hasher.update(b"\n");
    hasher.update(serde_json::to_vec(items)?);
    Ok(hex::encode(hasher.finalize()))
}

/// Apply the device policy to a computed set: drop unknown kinds and items
/// without evidence, strip or redact content, sort for a stable digest.
pub fn prepare_inferences(
    cfg: &SyncConfig,
    mut items: Vec<InferenceItem>,
) -> (Vec<InferenceItem>, usize) {
    items.retain(|it| INFERENCE_KINDS.contains(&it.kind.as_str()) && !it.evidence.is_empty());
    let mut content_removed = 0;
    for it in &mut items {
        if cfg.send_content {
            secrets::redact_value(&mut it.fields);
        } else {
            content_removed += strip_inference_content(&mut it.fields);
        }
    }
    items.sort_by(|a, b| (a.kind.as_str(), a.id.as_str()).cmp(&(b.kind.as_str(), b.id.as_str())));
    (items, content_removed)
}

/// The wire body of one inference upload (`spec/inference-v1.schema.json`).
pub fn inference_batch_body(
    device_id: attemptdb_core::DeviceId,
    kind: &str,
    algorithm_version: &str,
    computed_at: Timestamp,
    items: &[&InferenceItem],
) -> Value {
    json!({
        "sync_version": 1,
        "schema": INFERENCE_SCHEMA,
        "device_id": device_id,
        "batch_id": EventId::new().to_string(),
        "kind": kind,
        "algorithm_version": algorithm_version,
        "computed_at": computed_at,
        "items": items,
    })
}

#[derive(Deserialize)]
struct InferenceAck {
    #[serde(default)]
    stored: usize,
    #[serde(default)]
    rejected: Vec<Value>,
}

fn upload_inferences(
    agent: &ureq::Agent,
    cfg: &SyncConfig,
    device_id: attemptdb_core::DeviceId,
    events: &[Event],
    source: &InferenceSource,
    state: &mut SyncState,
    state_path: &Path,
) -> Result<InferenceReport> {
    let set = (source.0)(events).context("computing inferences")?;
    let (items, content_removed) = prepare_inferences(cfg, set.items);
    let digest = inference_digest(&set.algorithm_version, &items)?;
    let mut report = InferenceReport {
        items: items.len(),
        content_removed,
        ..Default::default()
    };
    if state.last_inference_digest.as_deref() == Some(digest.as_str()) {
        report.unchanged = true;
        return Ok(report);
    }
    let mut by_kind: BTreeMap<&str, Vec<&InferenceItem>> = BTreeMap::new();
    for it in &items {
        by_kind.entry(it.kind.as_str()).or_default().push(it);
    }
    for (kind, list) in by_kind {
        let keep = list.len().min(MAX_INFERENCE_ITEMS);
        report.truncated += list.len() - keep;
        let list = &list[list.len() - keep..];
        let body = serde_json::to_vec(&inference_batch_body(
            device_id,
            kind,
            &set.algorithm_version,
            set.computed_at,
            list,
        ))?;
        match post_inferences(agent, cfg, &body) {
            Ok(ack) => {
                report.kinds += 1;
                report.uploaded += ack.stored;
                report.rejected += ack.rejected.len();
            }
            Err(e) => {
                state.last_error = Some(format!("inferences: {e}"));
                state.last_error_at = Some(Timestamp::now());
                state.save(state_path)?;
                return Err(anyhow!("inferences ({kind}): {e}"));
            }
        }
    }
    state.inference_uploads += 1;
    state.inference_items = report.uploaded as u64;
    state.last_inference_digest = Some(digest);
    state.last_inference_at = Some(Timestamp::now());
    state.save(state_path)?;
    Ok(report)
}

fn post_inferences(
    agent: &ureq::Agent,
    cfg: &SyncConfig,
    body: &[u8],
) -> Result<InferenceAck, PostError> {
    let url = cfg.endpoint_inferences();
    let response = agent
        .post(&url)
        .set("Authorization", &format!("Bearer {}", cfg.key))
        .set("Content-Type", "application/json")
        .send_bytes(body);
    match response {
        Ok(r) => {
            let text = r
                .into_string()
                .map_err(|e| PostError::BadAck(e.to_string()))?;
            serde_json::from_str(&text).map_err(|e| PostError::BadAck(e.to_string()))
        }
        Err(ureq::Error::Status(413, _)) => Err(PostError::TooLarge),
        Err(ureq::Error::Status(status, r)) => {
            let text = r.into_string().unwrap_or_default();
            let message = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| v.get("error").and_then(Value::as_str).map(String::from))
                .unwrap_or(text);
            if status == 429 || status >= 500 {
                Err(PostError::Retryable { status, message })
            } else {
                Err(PostError::Rejected { status, message })
            }
        }
        Err(ureq::Error::Transport(t)) => Err(PostError::Transport {
            url,
            message: t.to_string(),
        }),
    }
}

#[derive(Debug, thiserror::Error)]
enum PostError {
    #[error("server refused the body as too large")]
    TooLarge,
    /// The server or the network is the problem: keep the batch, retry later.
    #[error("upload failed ({status}): {message}; will retry")]
    Retryable { status: u16, message: String },
    /// Something about this client is wrong: stop and say so.
    #[error("server rejected the request ({status}): {message}")]
    Rejected { status: u16, message: String },
    #[error("cannot reach {url}: {message}")]
    Transport { url: String, message: String },
    #[error("unreadable acknowledgement: {0}")]
    BadAck(String),
}

fn post(agent: &ureq::Agent, cfg: &SyncConfig, body: &[u8]) -> Result<Ack, PostError> {
    let url = cfg.endpoint();
    let response = agent
        .post(&url)
        .set("Authorization", &format!("Bearer {}", cfg.key))
        .set("Content-Type", "application/json")
        .send_bytes(body);
    match response {
        Ok(r) => {
            let text = r
                .into_string()
                .map_err(|e| PostError::BadAck(e.to_string()))?;
            serde_json::from_str(&text).map_err(|e| PostError::BadAck(format!("{e}: {text}")))
        }
        Err(ureq::Error::Status(status, r)) => {
            let text = r.into_string().unwrap_or_default();
            let message = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| v.get("error").and_then(Value::as_str).map(str::to_string))
                .unwrap_or(text);
            match status {
                413 => Err(PostError::TooLarge),
                s if s >= 500 || s == 408 || s == 429 => {
                    Err(PostError::Retryable { status: s, message })
                }
                s => Err(PostError::Rejected { status: s, message }),
            }
        }
        Err(ureq::Error::Transport(t)) => Err(PostError::Transport {
            url,
            message: t.to_string(),
        }),
    }
}

/// Human-readable summary line.
pub fn describe(report: &UploadReport) -> String {
    if report.pending_before == 0 {
        let mut s = format!("nothing to upload (cursor {})", report.cursor);
        if let Some(i) = &report.inferences {
            s.push_str(&describe_inferences(i));
        }
        return s;
    }
    let mut s = format!(
        "uploaded {} event(s) in {} batch(es): {} new, {} duplicate(s)",
        report.pending_before, report.batches, report.accepted, report.duplicates
    );
    if report.rejected > 0 {
        s.push_str(&format!(", {} rejected", report.rejected));
    }
    if report.redactions > 0 {
        s.push_str(&format!(
            ", {} attr(s) redacted by the server",
            report.redactions
        ));
    }
    if report.secrets_redacted > 0 {
        s.push_str(&format!(
            ", {} secret(s) redacted before upload",
            report.secrets_redacted
        ));
    }
    s.push_str(&format!("; cursor {}", report.cursor));
    if let Some(i) = &report.inferences {
        s.push_str(&describe_inferences(i));
    }
    s
}

fn describe_inferences(i: &InferenceReport) -> String {
    if i.unchanged {
        return format!("; inferences unchanged ({} item(s))", i.items);
    }
    let mut s = format!(
        "; inferences: {} item(s) in {} kind(s), {} stored",
        i.items, i.kinds, i.uploaded
    );
    if i.rejected > 0 {
        s.push_str(&format!(", {} rejected", i.rejected));
    }
    if i.truncated > 0 {
        s.push_str(&format!(", {} dropped (per-kind limit)", i.truncated));
    }
    if i.content_removed > 0 {
        s.push_str(&format!(
            ", {} content field(s) removed before upload",
            i.content_removed
        ));
    }
    s
}

/// Validate a URL the user typed for `attempt sync connect`.
pub fn validate_url(url: &str) -> Result<String> {
    let trimmed = url.trim().trim_end_matches('/');
    if !(trimmed.starts_with("https://") || trimmed.starts_with("http://")) {
        bail!("the sync URL must start with https:// (or http:// for a local server)");
    }
    if trimmed.len() <= "https://".len() {
        bail!("the sync URL has no host");
    }
    Ok(trimmed.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inference(kind: &str, id: &str, evidence: usize, objective: Option<&str>) -> InferenceItem {
        InferenceItem {
            kind: kind.into(),
            id: id.into(),
            session_id: None,
            project_id: None,
            evidence: (0..evidence).map(|_| EventId::new()).collect(),
            confidence: 0.9,
            algorithm_version: "test-v0".into(),
            fields: json!({ "objective": objective, "approach": "edit src/lib.rs" }),
        }
    }

    fn policy(send_content: bool) -> SyncConfig {
        SyncConfig {
            url: "https://x".into(),
            key: "k".into(),
            send_content,
            send_inferences: true,
            batch_events: 1,
            interval_secs: 5,
            include: vec![],
            exclude: vec![],
        }
    }

    #[test]
    fn inferences_without_evidence_or_of_unknown_kinds_never_leave() {
        let items = vec![
            inference("attempt", "att_b", 2, Some("fix the build")),
            inference("attempt", "att_a", 0, Some("no evidence")),
            inference("causal_edge", "edge_1", 3, None),
            inference("decision", "dec_1", 1, None),
        ];
        let (kept, removed) = prepare_inferences(&policy(false), items);
        let ids: Vec<&str> = kept.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(
            ids,
            ["att_b", "dec_1"],
            "sorted by (kind, id); dropped without evidence or of an unsynced kind"
        );
        assert_eq!(removed, 1, "one objective held text");
        assert!(kept[0].fields["objective"].is_null());
        assert_eq!(kept[0].fields["approach"], json!("edit src/lib.rs"));
    }

    #[test]
    fn with_content_opt_in_objectives_travel_but_secrets_do_not() {
        let items = vec![inference(
            "attempt",
            "att_1",
            1,
            Some("use token ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZabcdef0123 please"),
        )];
        let (kept, removed) = prepare_inferences(&policy(true), items);
        assert_eq!(removed, 0);
        let objective = kept[0].fields["objective"].as_str().unwrap();
        assert!(objective.contains("[REDACTED:github_token]"), "{objective}");
        assert!(!objective.contains("ghp_"));
    }

    #[test]
    fn inference_digest_is_stable_across_order_and_changes_with_content() {
        let a = inference("attempt", "att_1", 1, None);
        let b = inference("attempt", "att_2", 1, None);
        let (one, _) = prepare_inferences(&policy(false), vec![a.clone(), b.clone()]);
        let (two, _) = prepare_inferences(&policy(false), vec![b.clone(), a.clone()]);
        assert_eq!(
            inference_digest("v", &one).unwrap(),
            inference_digest("v", &two).unwrap()
        );
        assert_ne!(
            inference_digest("v", &one).unwrap(),
            inference_digest("v2", &one).unwrap()
        );
        let mut changed = a.clone();
        changed.confidence = 0.5;
        let (three, _) = prepare_inferences(&policy(false), vec![changed, b]);
        assert_ne!(
            inference_digest("v", &one).unwrap(),
            inference_digest("v", &three).unwrap()
        );
    }

    #[test]
    fn describe_mentions_inferences() {
        let mut r = UploadReport::default();
        assert!(!describe(&r).contains("inferences"));
        r.inferences = Some(InferenceReport {
            items: 3,
            kinds: 2,
            uploaded: 2,
            rejected: 1,
            truncated: 0,
            unchanged: false,
            content_removed: 3,
        });
        let s = describe(&r);
        assert!(
            s.contains("3 item(s) in 2 kind(s), 2 stored, 1 rejected, 3 content field(s) removed"),
            "{s}"
        );
        r.inferences = Some(InferenceReport {
            unchanged: true,
            items: 3,
            ..Default::default()
        });
        assert!(describe(&r).contains("inferences unchanged (3 item(s))"));
    }

    #[test]
    fn config_round_trips_and_is_private() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(SyncConfig::load(tmp.path()).unwrap().is_none());
        let cfg = SyncConfig {
            url: "https://sync.example.test".into(),
            key: "k-0123456789".into(),
            send_content: false,
            send_inferences: false,
            batch_events: 10,
            interval_secs: 5,
            include: vec![],
            exclude: vec![],
        };
        cfg.save(tmp.path()).unwrap();
        assert_eq!(SyncConfig::load(tmp.path()).unwrap(), Some(cfg.clone()));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(SyncConfig::path(tmp.path()))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        assert_eq!(cfg.masked_key(), "k-01…6789");
        assert_eq!(cfg.endpoint(), "https://sync.example.test/v1/sync");
        assert!(SyncConfig::remove(tmp.path()).unwrap());
        assert!(!SyncConfig::remove(tmp.path()).unwrap());
    }

    #[test]
    fn repository_policy() {
        use attemptdb_core::event::Provider;
        use attemptdb_core::{DeviceId, EventKind, ProjectRef};
        let d = DeviceId::derive(&["t", "d"]);
        let mk = |remote: Option<&str>| {
            Event::new(
                d,
                Provider::ClaudeCode,
                "x",
                EventKind::Unknown,
                ProjectRef::derive("/home/dev/p", remote, &d),
                "s",
                CaptureMode::MetadataOnly,
                "t/0",
            )
        };
        let public = mk(Some("github.com/acme/public"));
        let private = mk(Some("github.com/acme/private"));
        let local = mk(None);
        let mut cfg = SyncConfig {
            url: "https://x".into(),
            key: "k".into(),
            send_content: false,
            send_inferences: false,
            batch_events: 1,
            interval_secs: 5,
            include: vec![],
            exclude: vec![],
        };
        assert!(cfg.allows(&public) && cfg.allows(&private) && cfg.allows(&local));
        cfg.exclude = vec!["GitHub.com/acme/private".into()];
        assert!(cfg.allows(&public) && !cfg.allows(&private) && cfg.allows(&local));
        cfg.include = vec!["github.com/acme/public".into()];
        assert!(cfg.allows(&public) && !cfg.allows(&private) && !cfg.allows(&local));
        cfg.include = vec![format!("prj_{}", local.project.project_id)];
        assert!(!cfg.allows(&public) && cfg.allows(&local));
    }

    #[test]
    fn url_validation() {
        assert_eq!(validate_url(" https://a.b/ ").unwrap(), "https://a.b");
        assert!(validate_url("a.b").is_err());
        assert!(validate_url("https://").is_err());
    }

    #[test]
    fn state_is_per_database() {
        let a = SyncState::path(Path::new("/d"), Path::new("/x/.attemptdb"));
        let b = SyncState::path(Path::new("/d"), Path::new("/y/.attemptdb"));
        assert_ne!(a, b);
        assert!(a.starts_with("/d/sync"));
    }
}
