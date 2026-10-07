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
    /// Events a receiver refused on their own and the worker moved past.
    pub set_aside: AtomicU64,
}

impl Stats {
    pub fn json(&self) -> Value {
        json!({
            "deliveries": self.deliveries.load(Ordering::Relaxed),
            "events": self.events.load(Ordering::Relaxed),
            "failures": self.failures.load(Ordering::Relaxed),
            "set_aside": self.set_aside.load(Ordering::Relaxed),
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

/// The last acknowledged `source_seq`. `Ok(0)` only when no cursor was ever
/// written for the tenant (nothing delivered yet). A cursor file that exists
/// but is empty, short or not a number is an **error**: reading it as 0 would
/// redeliver the tenant's whole history to the product, so delivery for that
/// tenant stops, loudly, until the file is fixed or removed.
pub fn read_cursor_checked(data_dir: &Path, tenant: &TenantId) -> Result<u64> {
    let path = cursor_path(data_dir, tenant);
    match std::fs::read_to_string(&path) {
        Ok(text) => text.trim().parse::<u64>().with_context(|| {
            format!(
                "webhook cursor {} is {} byte(s) and not a number; delivery for this tenant is \
                 paused (restore the file, or delete it to deliver from the start)",
                path.display(),
                text.len()
            )
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// The last acknowledged `source_seq` for display (0 when there is none or it
/// cannot be read). Delivery itself uses [`read_cursor_checked`].
pub fn read_cursor(data_dir: &Path, tenant: &TenantId) -> u64 {
    read_cursor_checked(data_dir, tenant).unwrap_or(0)
}

fn write_cursor(data_dir: &Path, tenant: &TenantId, seq: u64) -> Result<()> {
    crate::fsutil::write_atomic(
        &cursor_path(data_dir, tenant),
        format!("{seq}\n").as_bytes(),
        false,
    )
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
    let events = crate::read::scan_events_after(&db, after, limit, &|_| true)?;
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

/// The stored envelope as the product's receiver gets it: everything but the
/// content-bearing fields.
///
/// The receiver reads an event's kind, times, session and `attrs`; the
/// conversation is read straight from the store by whoever needs it. Sending
/// it here bought nothing and cost a tenant's feed: a request filter in front
/// of the receiver answered a page whose prompt looked like an attack with
/// HTTP 403, and since the cursor only moves on a 2xx, the same page was
/// retried for ever.
fn metadata_only(event: &Event) -> Value {
    let mut value = serde_json::to_value(event).unwrap_or(Value::Null);
    if let Value::Object(fields) = &mut value {
        fields.remove("content");
        fields.remove("raw");
    }
    value
}

fn body_for(
    state: &AppState,
    tenant: &TenantId,
    after: u64,
    page_events: &[Event],
) -> (Vec<u8>, u64) {
    let next = page_events.last().map_or(after, |e| e.source_seq);
    let events: Vec<Value> = page_events.iter().map(metadata_only).collect();
    let body = json!({
        "delivery_id": uuid::Uuid::now_v7().to_string(),
        "tenant": tenant.as_str(),
        "after": after,
        "next": next,
        "count": events.len(),
        "devices": devices_of(state, tenant, page_events),
        "events": events,
    });
    (serde_json::to_vec(&body).unwrap_or_default(), next)
}

/// A delivery with nothing in it, signed like any other: what a receiver
/// that is up and holds the right secret answers `2xx` to whatever the events
/// are. The proof, before an event is set aside, that the refusal was about
/// the event.
fn empty_body(tenant: &TenantId, after: u64) -> Vec<u8> {
    let body = json!({
        "delivery_id": uuid::Uuid::now_v7().to_string(),
        "tenant": tenant.as_str(),
        "after": after,
        "next": after,
        "count": 0,
        "devices": {},
        "events": [],
    });
    serde_json::to_vec(&body).unwrap_or_default()
}

/// Why a POST did not land.
#[derive(Debug)]
enum PostError {
    /// The receiver answered with a status other than 2xx.
    Refused { status: u16, text: String },
    /// No answer: a connection error, a timeout, a task that died.
    Failed(String),
}

impl PostError {
    /// The receiver read this body and said no in a way a retry of the same
    /// body will not change: 400, 403 (a request filter in front of the
    /// receiver), 413 and 422. A 401 is the secret, a 404 or 405 the URL, a
    /// 429 or 5xx the receiver's state, a timeout the network: none of those
    /// is about the body, so none of them may cost an event.
    fn is_refusal_of_the_body(&self) -> bool {
        matches!(
            self,
            PostError::Refused {
                status: 400 | 403 | 413 | 422,
                ..
            }
        )
    }
}

impl std::fmt::Display for PostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PostError::Refused { status, text } => write!(f, "{status}: {text}"),
            PostError::Failed(why) => f.write_str(why),
        }
    }
}

/// POST one signed body. `Ok(())` on 2xx; the error says what the endpoint
/// answered.
fn post(config: &WebhookConfig, tenant: &TenantId, body: &[u8]) -> Result<(), PostError> {
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
        Err(ureq::Error::Status(status, r)) => {
            let text = r.into_string().unwrap_or_default();
            Err(PostError::Refused {
                status,
                text: text.chars().take(200).collect(),
            })
        }
        Err(e) => Err(PostError::Failed(e.to_string())),
    }
}

async fn post_async(
    config: &WebhookConfig,
    tenant: &TenantId,
    body: Vec<u8>,
) -> Result<(), PostError> {
    let (c, t) = (config.clone(), tenant.clone());
    match tokio::task::spawn_blocking(move || post(&c, &t, &body)).await {
        Ok(r) => r,
        Err(e) => Err(PostError::Failed(format!("post task failed: {e}"))),
    }
}

/// Move the tenant's cursor. `false` when it cannot be written: the receiver
/// has what was sent, and without the cursor it will get it again. Loud, and
/// the caller stops for now rather than loop on a full disk.
fn advance(state: &AppState, data_dir: &Path, tenant: &TenantId, next: u64) -> bool {
    if let Err(e) = write_cursor(data_dir, tenant, next) {
        eprintln!("webhook: tenant {tenant}: cannot write cursor {next}: {e:#}");
        state.webhook_stats.failures.fetch_add(1, Ordering::Relaxed);
        return false;
    }
    true
}

fn count_delivered(state: &AppState, events: usize) {
    state
        .webhook_stats
        .deliveries
        .fetch_add(1, Ordering::Relaxed);
    state
        .webhook_stats
        .events
        .fetch_add(events as u64, Ordering::Relaxed);
}

/// The events one isolation may set aside before it gives up: a receiver that
/// refuses more than this in one page is refusing something other than
/// individual events.
const MAX_SET_ASIDE_PER_PAGE: usize = 16;
/// Requests one isolation may spend. A 500-event page with one bad event costs
/// about 20.
const MAX_ISOLATION_REQUESTS: usize = 96;

/// The line kept for an event the receiver would not take: enough to find it in
/// the store and to see why, no content. The store still has the event; to
/// deliver it after the cause is fixed, put the cursor back to `source_seq - 1`.
fn record_set_aside(data_dir: &Path, tenant: &TenantId, event: &Event, why: &PostError) {
    use std::io::Write;
    let line = json!({
        "at_unix": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs()),
        "tenant": tenant.as_str(),
        "event_id": event.event_id.to_string(),
        "source_seq": event.source_seq,
        "provider": event.provider,
        "kind": event.kind,
        "session_id": event.session_id.to_string(),
        "error": why.to_string().chars().take(200).collect::<String>(),
    });
    let path = cursor_dir(data_dir).join(format!("{}.set-aside.jsonl", tenant.as_str()));
    let written = std::fs::create_dir_all(cursor_dir(data_dir))
        .and_then(|()| {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
        })
        .and_then(|mut f| writeln!(f, "{line}"));
    if let Err(e) = written {
        eprintln!(
            "webhook: tenant {tenant}: cannot record the set-aside event in {}: {e}",
            path.display()
        );
    }
}

/// How an isolation ended.
enum Isolation {
    /// Every event of the page is past the cursor: delivered or set aside.
    Done,
    /// It stopped early (the receiver is down, the budget is spent, the cursor
    /// cannot be written). The cursor is wherever it got to; the sweep tries
    /// again.
    Stopped,
}

/// A page the receiver refused for what is in it. Deliver what can be
/// delivered, left to right, by halving: a half that lands moves the cursor, a
/// half that is refused is halved again, and a single event that is refused on
/// its own is set aside — if, and only if, the receiver is shown to accept
/// other bodies (a sibling landed, or an empty delivery does). Otherwise the
/// endpoint is refusing everything — a wrong secret, a blocked address, a
/// firewall rule — and nothing is dropped.
async fn isolate(
    state: &Arc<AppState>,
    config: &WebhookConfig,
    tenant: &TenantId,
    data_dir: &Path,
    after: u64,
    events: &[Event],
) -> Isolation {
    use std::collections::VecDeque;
    eprintln!(
        "webhook: tenant {tenant}: the receiver refused {} event(s) after {after}; delivering them in halves",
        events.len()
    );
    let mut todo: VecDeque<(usize, usize)> = VecDeque::from([(0, events.len())]);
    let mut cursor = after;
    let mut requests = 0usize;
    let mut set_aside = 0usize;
    let mut receiver_accepts = false;
    while let Some((start, end)) = todo.pop_front() {
        if requests >= MAX_ISOLATION_REQUESTS {
            eprintln!(
                "webhook: tenant {tenant}: isolation spent its {MAX_ISOLATION_REQUESTS} requests at {cursor}; the sweep continues"
            );
            return Isolation::Stopped;
        }
        requests += 1;
        let slice = &events[start..end];
        let (body, next) = body_for(state, tenant, cursor, slice);
        match post_async(config, tenant, body).await {
            Ok(()) => {
                receiver_accepts = true;
                if !advance(state, data_dir, tenant, next) {
                    return Isolation::Stopped;
                }
                count_delivered(state, slice.len());
                cursor = next;
            }
            Err(e) if e.is_refusal_of_the_body() && slice.len() > 1 => {
                state.webhook_stats.failures.fetch_add(1, Ordering::Relaxed);
                let mid = start + slice.len() / 2;
                todo.push_front((mid, end));
                todo.push_front((start, mid));
            }
            Err(e) if e.is_refusal_of_the_body() => {
                state.webhook_stats.failures.fetch_add(1, Ordering::Relaxed);
                if set_aside >= MAX_SET_ASIDE_PER_PAGE {
                    eprintln!(
                        "webhook: tenant {tenant}: {MAX_SET_ASIDE_PER_PAGE} events of one page were refused on their own; \
                         not setting aside more (last: {e})"
                    );
                    return Isolation::Stopped;
                }
                if !receiver_accepts {
                    requests += 1;
                    receiver_accepts = post_async(config, tenant, empty_body(tenant, cursor))
                        .await
                        .is_ok();
                }
                if !receiver_accepts {
                    eprintln!(
                        "webhook: tenant {tenant}: the receiver refuses an empty delivery too ({e}); \
                         that is the endpoint, not event {}: nothing is set aside",
                        slice[0].source_seq
                    );
                    return Isolation::Stopped;
                }
                let event = &slice[0];
                eprintln!(
                    "webhook: tenant {tenant}: setting aside event {} (source_seq {}, {}): the receiver refuses it on its own: {e}",
                    event.event_id,
                    event.source_seq,
                    serde_json::to_value(event.kind)
                        .ok()
                        .and_then(|k| k.as_str().map(str::to_string))
                        .unwrap_or_default()
                );
                record_set_aside(data_dir, tenant, event, &e);
                if !advance(state, data_dir, tenant, next) {
                    return Isolation::Stopped;
                }
                state
                    .webhook_stats
                    .set_aside
                    .fetch_add(1, Ordering::Relaxed);
                set_aside += 1;
                cursor = next;
            }
            Err(e) => {
                state.webhook_stats.failures.fetch_add(1, Ordering::Relaxed);
                eprintln!("webhook: tenant {tenant}: delivery in halves stopped at {cursor}: {e}");
                return Isolation::Stopped;
            }
        }
    }
    Isolation::Done
}

/// Deliver everything the tenant has past its cursor. Returns whether the
/// tenant is caught up (false: a delivery failed and the sweep retries).
async fn deliver(state: &Arc<AppState>, config: &WebhookConfig, tenant: &TenantId) -> bool {
    let data_dir = state.config.data_dir.clone();
    loop {
        let after = match read_cursor_checked(&data_dir, tenant) {
            Ok(n) => n,
            Err(e) => {
                eprintln!("webhook: tenant {tenant}: {e:#}");
                state.webhook_stats.failures.fetch_add(1, Ordering::Relaxed);
                return false;
            }
        };
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
        let (body, next) = body_for(state, tenant, after, &page.events);
        // A short in-line retry for the transient case; anything longer
        // is the sweep's job, so one dead endpoint does not park every
        // other tenant behind it. A receiver that has read the body and
        // refused it is not transient: retrying the same bytes only waits.
        let mut attempt = 0u32;
        let sent = loop {
            match post_async(config, tenant, body.clone()).await {
                Ok(()) => break Ok(()),
                Err(e) => {
                    attempt += 1;
                    state.webhook_stats.failures.fetch_add(1, Ordering::Relaxed);
                    eprintln!(
                        "webhook: tenant {tenant}: delivery of {count} event(s) after {after} failed (attempt {attempt}): {e}"
                    );
                    if e.is_refusal_of_the_body() || attempt >= 3 {
                        break Err(e);
                    }
                    tokio::time::sleep(Duration::from_secs(1 << attempt)).await;
                }
            }
        };
        match sent {
            Ok(()) => {
                if !advance(state, &data_dir, tenant, next) {
                    return false;
                }
                count_delivered(state, count);
            }
            Err(e) if e.is_refusal_of_the_body() => {
                // The same page, refused for what is in it, would be retried
                // for ever, and every event behind it with it.
                if matches!(
                    isolate(state, config, tenant, &data_dir, after, &page.events).await,
                    Isolation::Stopped
                ) {
                    return false;
                }
            }
            Err(_) => return false,
        }
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

/// `scheme://host[:port]` of a webhook URL for the log: the path and query
/// of a webhook endpoint often carry the credential that authorises it.
fn display_target(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return "(configured URL)".to_string();
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = authority.rsplit('@').next().unwrap_or("");
    format!("{scheme}://{host}")
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
        display_target(&config.url),
        config.page
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
        let left: Vec<_> = std::fs::read_dir(tmp.path().join("webhook"))
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(left, ["acme.cursor"], "no temp file is left behind");
    }

    #[test]
    fn an_empty_or_unreadable_cursor_is_an_error_not_zero() {
        let tmp = tempfile::tempdir().unwrap();
        let t = TenantId::parse("acme").unwrap();
        std::fs::create_dir_all(tmp.path().join("webhook")).unwrap();
        let path = tmp.path().join("webhook").join("acme.cursor");
        // No file: nothing was ever delivered.
        assert_eq!(read_cursor_checked(tmp.path(), &t).unwrap(), 0);
        for bad in ["", "\n", "  ", "12x", "-5", "forty"] {
            std::fs::write(&path, bad).unwrap();
            let err = read_cursor_checked(tmp.path(), &t).unwrap_err();
            let text = format!("{err:#}");
            assert!(
                text.contains("not a number") && text.contains("paused"),
                "{bad:?}: {text}"
            );
        }
        std::fs::write(&path, "17\n").unwrap();
        assert_eq!(read_cursor_checked(tmp.path(), &t).unwrap(), 17);
    }

    #[test]
    fn the_log_names_the_host_never_the_path_or_credentials() {
        assert_eq!(
            display_target("https://user:pw@hooks.example.com:8443/in/SECRETPATH?token=abc"),
            "https://hooks.example.com:8443"
        );
        assert_eq!(display_target("not a url"), "(configured URL)");
    }
}
