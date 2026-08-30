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
use attemptdb_core::{CaptureMode, Event, EventId, Timestamp};
use attemptdb_storage::{Database, OpenOptions, ScanFilter};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const CONFIG_FILE: &str = "sync.json";
pub const DEFAULT_BATCH_EVENTS: usize = 1_000;
pub const DEFAULT_INTERVAL_SECS: u64 = 30;
/// Largest body the server accepts by default (4 MiB); stay well under.
const MAX_BODY_BYTES: usize = 3 * 1024 * 1024;

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
    #[serde(default = "default_batch")]
    pub batch_events: usize,
    #[serde(default = "default_interval")]
    pub interval_secs: u64,
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

    fn endpoint(&self) -> String {
        format!("{}/v1/sync", self.url.trim_end_matches('/'))
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
    /// Cursor after the run.
    pub cursor: u64,
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
    let db = open_read_only(locator)?;
    let device_id = db.device_id();
    let state_path = SyncState::path(&locator.paths.data_dir, &locator.db_dir);
    let mut state = SyncState::load(&state_path)?;

    let mut pending: Vec<Event> = db
        .scan(&ScanFilter::default())
        .context("scanning events")?
        .into_iter()
        .filter(|e| e.source_seq > state.last_acked_source_seq)
        .collect();
    pending.sort_by_key(|e| e.source_seq);
    drop(db);

    let mut report = UploadReport {
        pending_before: pending.len(),
        cursor: state.last_acked_source_seq,
        ..Default::default()
    };
    if pending.is_empty() {
        return Ok(report);
    }

    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(60))
        .build();
    let capture_mode = if cfg.send_content {
        CaptureMode::LocalSemantic
    } else {
        CaptureMode::MetadataOnly
    };

    let mut batch_size = cfg.batch_events.clamp(1, 5_000);
    let mut start = 0;
    while start < pending.len() {
        let end = (start + batch_size).min(pending.len());
        let chunk = &pending[start..end];
        let events: Vec<Event> = chunk
            .iter()
            .cloned()
            .map(|mut e| {
                if !cfg.send_content {
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

        match post(&agent, cfg, &body) {
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
                state.save(&state_path)?;
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
                state.save(&state_path)?;
                return Err(anyhow!(
                    "{e} (cursor kept at {})",
                    state.last_acked_source_seq
                ));
            }
        }
    }
    Ok(report)
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
        return format!("nothing to upload (cursor {})", report.cursor);
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
    s.push_str(&format!("; cursor {}", report.cursor));
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

    #[test]
    fn config_round_trips_and_is_private() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(SyncConfig::load(tmp.path()).unwrap().is_none());
        let cfg = SyncConfig {
            url: "https://sync.example.test".into(),
            key: "k-0123456789".into(),
            send_content: false,
            batch_events: 10,
            interval_secs: 5,
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
