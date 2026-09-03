//! Outbound webhook — how the product learns what arrived.
//!
//! A product keeps its application state (points, presence, notifications)
//! next to its users, not in the event store. When a device's batch has
//! been accepted, the server delivers the new events to the product's
//! endpoint, and the product applies its own rules. Nothing here knows
//! those rules.
//!
//! Delivery is a per-tenant **cursor**, not a queue: the worker reads the
//! tenant's events strictly after the last acknowledged `source_seq`
//! (`<data-dir>/webhook/<tenant>.cursor`), POSTs them, and advances the
//! cursor only on a 2xx. The store is the queue, so a restart, a crash, or
//! an endpoint that was down for an hour costs nothing but a later catch-
//! up; an event is delivered at least once and, because the cursor is
//! written after the acknowledgement, a redelivery is always a whole page
//! the receiver has already seen (it keys on `event_id`). Ingest never
//! waits: the sync handler only nudges the worker.
//!
//! Every request carries `X-AttemptDB-Signature: sha256=<hex>`, an
//! HMAC-SHA256 of the exact body under the shared secret, plus the tenant
//! and a delivery id. The body is the tenant, the cursor range, the
//! devices concerned (with the product's own user id from their keys), and
//! the events as stored — metadata only on a `metadata_only` server.

use crate::AppState;
use crate::tenants::TenantId;
use anyhow::{Context, Result};
use attemptdb_core::{DeviceId, Event};
use hmac::{Hmac, Mac};
use serde_json::{Value, json};
use sha2::Sha256;
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::mpsc;

/// Where deliveries go and how they are signed.
#[derive(Clone, Debug)]
pub struct WebhookConfig {
    pub url: String,
    pub secret: String,
    /// One request's timeout.
    pub timeout: Duration,
    /// Events per delivery.
    pub page: usize,
}

impl WebhookConfig {
    pub fn new(url: impl Into<String>, secret: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            secret: secret.into(),
            timeout: Duration::from_secs(10),
            page: 500,
        }
    }
}

/// Counters for `/v1/health`.
#[derive(Debug, Default)]
pub struct Stats {
    pub deliveries: AtomicU64,
    pub events: AtomicU64,
    pub failures: AtomicU64,
}

impl Stats {
    pub fn json(&self) -> Value {
        json!({
            "deliveries": self.deliveries.load(Ordering::Relaxed),
            "events": self.events.load(Ordering::Relaxed),
            "failures": self.failures.load(Ordering::Relaxed),
        })
    }
}

/// The ingest side's handle: nudge the worker about a tenant.
#[derive(Clone, Debug)]
pub struct Outbox {
    tx: mpsc::UnboundedSender<TenantId>,
}

impl Outbox {
    pub fn channel() -> (Self, mpsc::UnboundedReceiver<TenantId>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (Self { tx }, rx)
    }

    /// Something new for this tenant. Never blocks, never fails loudly: a
    /// worker that is gone means the sweep picks the tenant up later.
    pub fn notify(&self, tenant: &TenantId) {
        let _ = self.tx.send(tenant.clone());
    }
}

/// `sha256=<hex>` over `body` under `secret`.
pub fn signature(secret: &str, body: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("any key length");
    mac.update(body);
    format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
}

/// Verify a `sha256=<hex>` header against `body` (constant time).
pub fn verify(secret: &str, body: &[u8], header: &str) -> bool {
    let Some(hex_digest) = header.strip_prefix("sha256=") else {
        return false;
    };
    let Ok(given) = hex::decode(hex_digest) else {
        return false;
    };
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("any key length");
    mac.update(body);
    mac.verify_slice(&given).is_ok()
}

fn cursor_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("webhook")
}

fn cursor_path(data_dir: &Path, tenant: &TenantId) -> PathBuf {
    cursor_dir(data_dir).join(format!("{}.cursor", tenant.as_str()))
}

/// The last acknowledged `source_seq` (0 when nothing was delivered yet).
pub fn read_cursor(data_dir: &Path, tenant: &TenantId) -> u64 {
    std::fs::read_to_string(cursor_path(data_dir, tenant))
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

fn write_cursor(data_dir: &Path, tenant: &TenantId, seq: u64) -> Result<()> {
    let dir = cursor_dir(data_dir);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = cursor_path(data_dir, tenant);
    let tmp = path.with_extension("cursor.tmp");
    std::fs::write(&tmp, format!("{seq}\n"))
        .with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("renaming to {}", path.display()))?;
    Ok(())
}

/// One page to deliver: the events after the cursor, and where the store
/// ends (so the worker knows whether to go around again).
struct Page {
    events: Vec<Event>,
    last_source_seq: u64,
}

fn read_page(state: &AppState, tenant: &TenantId, after: u64, limit: usize) -> Result<Page> {
    let db = state.tenants.open(tenant)?;
    let db = db
        .lock()
        .map_err(|_| anyhow::anyhow!("tenant {tenant}: database poisoned"))?;
    let mut events = crate::read::scan_events_after(&db, after)?;
    events.sort_by_key(|e| e.source_seq);
    events.truncate(limit);
    Ok(Page {
        events,
        last_source_seq: db.manifest().last_source_seq.max(
            db.memtable_events()
                .iter()
                .map(|e| e.source_seq)
                .max()
                .unwrap_or(0),
        ),
    })
}

/// The devices a page mentions, with what the key table knows about them.
fn devices_of(state: &AppState, tenant: &TenantId, events: &[Event]) -> Value {
    let ids: HashSet<DeviceId> = events.iter().map(|e| e.device_id).collect();
    let entries = state.keys.read().map(|k| k.entries()).unwrap_or_default();
    let mut out = BTreeMap::new();
    for id in ids {
        let key = entries
            .iter()
            .filter(|e| e.tenant == tenant.as_str() && e.device_id == id)
            .find(|e| e.scope == crate::auth::Scope::Device);
        out.insert(
            id.to_string(),
            json!({
                "user_id": key.and_then(|k| k.user_id.clone()),
                "label": key.map(|k| k.label.clone()),
                // Server time the device key was issued: the product's
                // notion of when this device joined.
                "paired_at": key.and_then(|k| k.issued_at).map(|t| t.to_rfc3339()),
            }),
        );
    }
    Value::Object(out.into_iter().collect())
}

fn body_for(state: &AppState, tenant: &TenantId, after: u64, page: &Page) -> (Vec<u8>, u64) {
    let next = page.events.last().map_or(after, |e| e.source_seq);
    let events: Vec<Value> = page
        .events
        .iter()
        .map(|e| serde_json::to_value(e).unwrap_or(Value::Null))
        .collect();
    let body = json!({
        "delivery_id": uuid::Uuid::now_v7().to_string(),
        "tenant": tenant.as_str(),
        "after": after,
        "next": next,
        "count": events.len(),
        "devices": devices_of(state, tenant, &page.events),
        "events": events,
    });
    (serde_json::to_vec(&body).unwrap_or_default(), next)
}

/// POST one signed body. `Ok(())` on 2xx; the error says what the endpoint
/// answered.
fn post(config: &WebhookConfig, tenant: &TenantId, body: &[u8]) -> Result<()> {
    let signature = signature(&config.secret, body);
    let agent = ureq::AgentBuilder::new().timeout(config.timeout).build();
    let resp = agent
        .post(&config.url)
        .set("Content-Type", "application/json")
        .set(
            "User-Agent",
            concat!("attemptdb-server/", env!("CARGO_PKG_VERSION")),
        )
        .set("X-AttemptDB-Tenant", tenant.as_str())
        .set("X-AttemptDB-Signature", &signature)
        .send_bytes(body);
    match resp {
        Ok(_) => Ok(()),
        Err(ureq::Error::Status(code, r)) => {
            let text = r.into_string().unwrap_or_default();
            anyhow::bail!("{code}: {}", text.chars().take(200).collect::<String>())
        }
        Err(e) => anyhow::bail!("{e}"),
    }
}

/// Deliver everything the tenant has past its cursor. Returns whether the
/// tenant is caught up (false: a delivery failed and the sweep retries).
async fn deliver(state: &Arc<AppState>, config: &WebhookConfig, tenant: &TenantId) -> bool {
    let data_dir = state.config.data_dir.clone();
    loop {
        let after = read_cursor(&data_dir, tenant);
        let page = {
            let st = Arc::clone(state);
            let t = tenant.clone();
            let limit = config.page;
            tokio::task::spawn_blocking(move || read_page(&st, &t, after, limit)).await
        };
        let page = match page {
            Ok(Ok(p)) => p,
            Ok(Err(e)) => {
                eprintln!("webhook: tenant {tenant}: cannot read events after {after}: {e:#}");
                state.webhook_stats.failures.fetch_add(1, Ordering::Relaxed);
                return false;
            }
            Err(e) => {
                eprintln!("webhook: tenant {tenant}: read task failed: {e}");
                state.webhook_stats.failures.fetch_add(1, Ordering::Relaxed);
                return false;
            }
        };
        if page.events.is_empty() {
            return true;
        }
        let count = page.events.len();
        let (body, next) = body_for(state, tenant, after, &page);
        // A short in-line retry for the transient case; anything longer
        // is the sweep's job, so one dead endpoint does not park every
        // other tenant behind it.
        let mut attempt = 0u32;
        let sent = loop {
            let c = config.clone();
            let t = tenant.clone();
            let b = body.clone();
            let r = tokio::task::spawn_blocking(move || post(&c, &t, &b)).await;
            match r {
                Ok(Ok(())) => break true,
                Ok(Err(e)) => {
                    attempt += 1;
                    state.webhook_stats.failures.fetch_add(1, Ordering::Relaxed);
                    eprintln!(
                        "webhook: tenant {tenant}: delivery of {count} event(s) after {after} failed (attempt {attempt}): {e}"
                    );
                    if attempt >= 3 {
                        break false;
                    }
                    tokio::time::sleep(Duration::from_secs(1 << attempt)).await;
                }
                Err(e) => {
                    eprintln!("webhook: tenant {tenant}: post task failed: {e}");
                    break false;
                }
            }
        };
        if !sent {
            return false;
        }
        if let Err(e) = write_cursor(&data_dir, tenant, next) {
            // The receiver has the page; without the cursor it will get it
            // again. Loud, and stop for now rather than loop on a full disk.
            eprintln!("webhook: tenant {tenant}: cannot write cursor {next}: {e:#}");
            state.webhook_stats.failures.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        state
            .webhook_stats
            .deliveries
            .fetch_add(1, Ordering::Relaxed);
        state
            .webhook_stats
            .events
            .fetch_add(count as u64, Ordering::Relaxed);
        if next >= page.last_source_seq {
            return true;
        }
    }
}

/// Every tenant directory under the data dir: the catch-up set at start.
fn all_tenants(data_dir: &Path) -> Vec<TenantId> {
    let root = data_dir.join("tenants");
    let Ok(rd) = std::fs::read_dir(&root) else {
        return Vec::new();
    };
    rd.filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .filter_map(|e| TenantId::parse(&e.file_name().to_string_lossy()).ok())
        .collect()
}

/// The worker: one at a time, in arrival order, with a sweep of the
/// tenants whose last round failed. Runs until the server stops.
pub async fn run(
    state: Arc<AppState>,
    config: WebhookConfig,
    mut rx: mpsc::UnboundedReceiver<TenantId>,
) {
    eprintln!(
        "webhook: delivering to {} (page {})",
        config.url, config.page
    );
    let mut retry: HashSet<TenantId> = HashSet::new();
    // Whatever was ingested before this process started and never
    // delivered (a restart mid-backlog) is delivered first.
    for t in all_tenants(&state.config.data_dir) {
        if !deliver(&state, &config, &t).await {
            retry.insert(t);
        }
    }
    let mut sweep = tokio::time::interval(Duration::from_secs(60));
    sweep.tick().await;
    loop {
        tokio::select! {
            got = rx.recv() => {
                let Some(t) = got else { return };
                if deliver(&state, &config, &t).await {
                    retry.remove(&t);
                } else {
                    retry.insert(t);
                }
            }
            _ = sweep.tick() => {
                let due: Vec<TenantId> = retry.iter().cloned().collect();
                for t in due {
                    if deliver(&state, &config, &t).await {
                        retry.remove(&t);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_round_trips_and_rejects_tampering() {
        let body = br#"{"tenant":"acme","events":[]}"#;
        let sig = signature("s3cret", body);
        assert!(sig.starts_with("sha256="));
        assert!(verify("s3cret", body, &sig));
        assert!(!verify("other", body, &sig));
        assert!(!verify("s3cret", b"{}", &sig));
        assert!(!verify("s3cret", body, "md5=00"));
    }

    #[test]
    fn signature_matches_rfc4231_style_vector() {
        // HMAC-SHA256("key", "The quick brown fox jumps over the lazy dog")
        let sig = signature("key", b"The quick brown fox jumps over the lazy dog");
        assert_eq!(
            sig,
            "sha256=f7bc83f430538424b13298e6aa6fb143ef4d59a14946175997479dbc2d1a3cd8"
        );
    }

    #[test]
    fn cursor_is_zero_until_written_and_survives_rewrites() {
        let tmp = tempfile::tempdir().unwrap();
        let t = TenantId::parse("acme").unwrap();
        assert_eq!(read_cursor(tmp.path(), &t), 0);
        write_cursor(tmp.path(), &t, 17).unwrap();
        assert_eq!(read_cursor(tmp.path(), &t), 17);
        write_cursor(tmp.path(), &t, 40).unwrap();
        assert_eq!(read_cursor(tmp.path(), &t), 40);
        assert!(!tmp.path().join("webhook").join("acme.cursor.tmp").exists());
    }
}
