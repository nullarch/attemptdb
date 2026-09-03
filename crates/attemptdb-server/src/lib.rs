//! AttemptDB sync server — the server half of RFC 0006 §10.
//!
//! A second deployment of the same engine, not a different product. A device
//! runs `attempt hook` exactly as it does locally; the events it spools are
//! uploaded in batches to `POST /v1/sync`, and the server ingests them into
//! **one database per tenant**: a plain `.attemptdb` directory under
//! `data_dir/tenants/<tenant>/`. The writer's exclusive lock is the tenancy
//! model — nothing in `attemptdb-core` or `attemptdb-storage` knows that
//! tenants exist, and this crate never touches their internals.
//!
//! What the boundary enforces, in order:
//! 1. a bearer key identifies `(tenant, device)`; a batch must name that device;
//! 2. the server's capture mode is a ceiling — content a client uploaded under
//!    a more permissive mode is stripped before it reaches the WAL;
//! 3. the engine's own `attrs` contract check runs at ingestion and counts
//!    every dropped key, so a misbehaving client is visible in the ack.
//!
//! The read side (`/v1/sessions`, `/v1/timeline`, `/v1/work`,
//! `/v1/attention`, `/v1/state`, `/v1/events`, `/v1/query`, `/v1/status`)
//! serves a tenant's work graph to reader and admin keys from a per-tenant
//! engine cache (see [`engine`]); `docs/server-api.md` is the contract.
//!
//! TLS is terminated in front of this process; it speaks plain HTTP/1.1.

pub mod admin;
pub mod auth;
pub mod corrections;
pub mod devices;
pub mod engine;
pub mod inferences;
pub mod legacy;
pub mod limiter;
pub mod live;
pub mod merge;
pub mod pairing;
pub mod read;
pub mod shape;
pub mod sync;
pub mod tenants;
pub mod webhook;

use anyhow::{Context, Result};
use attemptdb_core::{CaptureMode, DeviceId, Timestamp};
use axum::extract::{DefaultBodyLimit, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::json;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// Server configuration; every field has a conservative default.
#[derive(Clone, Debug)]
pub struct ServerConfig {
    pub bind: IpAddr,
    pub port: u16,
    /// Root under which `tenants/<tenant>/` databases live.
    pub data_dir: PathBuf,
    /// JSON file of key digests → principal (see [`auth::KeyTable`]).
    pub keys_file: PathBuf,
    /// Ceiling on what a client may persist here. `metadata_only` for a
    /// hosted service: content never reaches the server's disk.
    pub capture_mode: CaptureMode,
    /// Open databases kept resident before the least recently used one is
    /// flushed and closed.
    pub max_open: usize,
    /// A tenant idle for this long is flushed and closed by the sweeper.
    pub idle_flush: Duration,
    /// Largest request body accepted, in bytes.
    pub body_limit: usize,
    /// Bearer token for `/v1/admin/*` (key issuance, revocation, reload).
    /// `None` disables the admin surface entirely (404).
    pub admin_token: Option<String>,
    /// Merge a tenant's small segments when it is flushed and closed (idle
    /// sweep, LRU eviction). `None` never compacts.
    pub compaction: Option<attemptdb_storage::CompactionPolicy>,
    /// Keep only the last this many days of a tenant's events in its
    /// resident view. A console reads "now, today, this piece of work"; an
    /// organisation's year of history need not sit in memory for that.
    /// Segments the manifest places before the window are never decoded;
    /// `/v1/events` (backfill by sequence) is not affected. `None` keeps
    /// the whole history resident.
    pub view_window_days: Option<u32>,
    /// Requests per bearer key (sustained per second, burst).
    pub key_rate: limiter::Rate,
    /// Requests per client address on the unauthenticated `/v1/pair*`.
    pub pair_rate: limiter::Rate,
    /// Deliver accepted events to the product's endpoint (see [`webhook`]).
    pub webhook: Option<webhook::WebhookConfig>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 8787,
            data_dir: PathBuf::from("data"),
            keys_file: PathBuf::from("keys.json"),
            capture_mode: CaptureMode::MetadataOnly,
            max_open: 256,
            idle_flush: Duration::from_secs(300),
            body_limit: 4 * 1024 * 1024,
            admin_token: None,
            compaction: Some(attemptdb_storage::CompactionPolicy::default()),
            view_window_days: None,
            key_rate: limiter::Rate::new(20.0, 200.0),
            pair_rate: limiter::Rate::new(0.2, 10.0),
            webhook: None,
        }
    }
}

/// Shared by every request.
pub struct AppState {
    pub config: ServerConfig,
    pub keys: std::sync::RwLock<auth::KeyTable>,
    pub tenants: tenants::Registry,
    /// Per-tenant "newest event" facts for `/v1/live`; never evicted.
    pub live: live::LiveMap,
    /// One-time pairing tokens (digests), see [`pairing`].
    pub pairings: pairing::PairingTable,
    pub limiter: limiter::Limiter,
    /// When each device last authenticated an upload — including the empty
    /// batch a fresh pairing sends as its handshake, which stores nothing.
    /// `/v1/devices` reports it as `last_seen_at`; process-local.
    pub seen: std::sync::Mutex<std::collections::HashMap<(tenants::TenantId, DeviceId), Timestamp>>,
    /// The webhook worker's handle (`None` when no webhook is configured).
    pub outbox: Option<webhook::Outbox>,
    pub webhook_stats: webhook::Stats,
}

impl AppState {
    /// After an ingest stored something for `tenant`: let the webhook
    /// worker know. Nothing happens when no webhook is configured.
    pub fn ingested(&self, tenant: &tenants::TenantId) {
        if let Some(o) = &self.outbox {
            o.notify(tenant);
        }
    }
}

impl AppState {
    /// Resolve a bearer header against the current key table.
    pub fn authenticate(&self, authorization: Option<&str>) -> Option<auth::Principal> {
        self.keys
            .read()
            .ok()
            .and_then(|k| k.authenticate(authorization))
    }

    /// Re-read the key file. Returns the number of keys.
    pub fn reload_keys(&self) -> Result<usize> {
        let table = auth::KeyTable::load(&self.config.keys_file)?;
        let n = table.len();
        *self
            .keys
            .write()
            .map_err(|_| anyhow::anyhow!("key table poisoned"))? = table;
        Ok(n)
    }

    /// Append an entry to the key file and reload.
    pub fn add_key(&self, entry: auth::KeyEntry) -> Result<()> {
        let mut entries = self
            .keys
            .read()
            .map_err(|_| anyhow::anyhow!("key table poisoned"))?
            .entries();
        entries.push(entry);
        auth::KeyTable::from_entries(entries.clone())?; // validate before writing
        auth::KeyTable::save(&entries, &self.config.keys_file)?;
        self.reload_keys().map(|_| ())
    }

    /// Remove every entry the predicate selects and reload. Returns how
    /// many were removed; the file is rewritten only when that is non-zero.
    pub fn remove_keys_where(&self, keep: impl Fn(&auth::KeyEntry) -> bool) -> Result<usize> {
        let mut entries = self
            .keys
            .read()
            .map_err(|_| anyhow::anyhow!("key table poisoned"))?
            .entries();
        let before = entries.len();
        entries.retain(|e| keep(e));
        let removed = before - entries.len();
        if removed == 0 {
            return Ok(0);
        }
        auth::KeyTable::save(&entries, &self.config.keys_file)?;
        self.reload_keys().map(|_| removed)
    }

    /// Remove the entry with this digest and reload. `Ok(false)` when absent.
    pub fn remove_key(&self, sha256: &str) -> Result<bool> {
        let mut entries = self
            .keys
            .read()
            .map_err(|_| anyhow::anyhow!("key table poisoned"))?
            .entries();
        let before = entries.len();
        entries.retain(|e| !e.sha256.eq_ignore_ascii_case(sha256));
        if entries.len() == before {
            return Ok(false);
        }
        auth::KeyTable::save(&entries, &self.config.keys_file)?;
        self.reload_keys().map(|_| true)
    }
}

pub struct Server {
    listener: tokio::net::TcpListener,
    addr: SocketAddr,
    state: Arc<AppState>,
    webhook_rx: Option<tokio::sync::mpsc::UnboundedReceiver<tenants::TenantId>>,
}

impl Server {
    /// Load the key table, bind the listener, and prepare the tenant root.
    pub async fn bind(config: ServerConfig) -> Result<Self> {
        let keys = auth::KeyTable::load(&config.keys_file)
            .with_context(|| format!("loading keys from {}", config.keys_file.display()))?;
        let tenants = tenants::Registry::new(&config.data_dir, config.max_open)?
            .with_compaction(config.compaction.clone());
        let data_dir_for_pairings = config.data_dir.clone();
        let addr = SocketAddr::new(config.bind, config.port);
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .with_context(|| format!("binding {addr}"))?;
        let addr = listener.local_addr().context("reading the bound address")?;
        let (outbox, webhook_rx) = match &config.webhook {
            Some(_) => {
                let (o, rx) = webhook::Outbox::channel();
                (Some(o), Some(rx))
            }
            None => (None, None),
        };
        let state = Arc::new(AppState {
            config,
            keys: std::sync::RwLock::new(keys),
            tenants,
            live: live::LiveMap::default(),
            pairings: pairing::PairingTable::load(&data_dir_for_pairings)
                .context("loading the pairing file")?,
            limiter: limiter::Limiter::default(),
            seen: std::sync::Mutex::new(std::collections::HashMap::new()),
            outbox,
            webhook_stats: webhook::Stats::default(),
        });
        Ok(Self {
            listener,
            addr,
            state,
            webhook_rx,
        })
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn state(&self) -> &Arc<AppState> {
        &self.state
    }

    /// Serve until `shutdown` resolves, then flush every open tenant.
    pub async fn run(self, shutdown: impl Future<Output = ()> + Send + 'static) -> Result<()> {
        let app = router(Arc::clone(&self.state));
        let sweeper = {
            let state = Arc::clone(&self.state);
            let idle = state.config.idle_flush;
            tokio::spawn(async move {
                let mut tick = tokio::time::interval(idle.max(Duration::from_secs(1)) / 2);
                tick.tick().await;
                loop {
                    tick.tick().await;
                    let state = Arc::clone(&state);
                    let _ =
                        tokio::task::spawn_blocking(move || state.tenants.flush_idle(idle)).await;
                }
            })
        };
        let worker = match (self.webhook_rx, self.state.config.webhook.clone()) {
            (Some(rx), Some(config)) => {
                let state = Arc::clone(&self.state);
                Some(tokio::spawn(webhook::run(state, config, rx)))
            }
            _ => None,
        };
        let result = axum::serve(self.listener, app)
            .with_graceful_shutdown(shutdown)
            .await
            .context("serving");
        sweeper.abort();
        if let Some(w) = worker {
            w.abort();
        }
        let state = Arc::clone(&self.state);
        let _ = tokio::task::spawn_blocking(move || state.tenants.flush_all()).await;
        result
    }
}

fn router(state: Arc<AppState>) -> Router {
    let limit = state.config.body_limit;
    Router::new()
        .route("/v1/health", get(health))
        .route("/v1/sync", post(sync::handle))
        .route("/v1/sync/inferences", post(inferences::handle))
        .route("/v1/inferences", get(inferences::get))
        .route("/v1/vibemon/hook", post(legacy::handle))
        .route("/v1/status", get(read::status))
        .route("/v1/live", get(live::live))
        .route("/v1/devices", get(read::devices))
        .route("/v1/sessions", get(read::sessions))
        .route("/v1/timeline", get(read::timeline))
        .route("/v1/work", get(read::work))
        .route("/v1/attention", get(read::attention))
        .route("/v1/state", get(read::state_at))
        .route("/v1/events", get(read::events))
        .route("/v1/events/{id}", get(corrections::event_by_id))
        .route("/v1/corrections", post(corrections::post_correction))
        .route("/v1/query", post(read::query))
        .route("/v1/pair", post(pairing::exchange))
        .route("/v1/pair/{token}", get(pairing::check))
        .route(
            "/v1/admin/pairings",
            get(pairing::list).post(pairing::issue),
        )
        .route("/v1/admin/keys", get(admin::list).post(admin::issue))
        .route("/v1/admin/keys/reload", post(admin::reload))
        .route(
            "/v1/admin/keys/{sha256}",
            axum::routing::delete(admin::revoke),
        )
        .route(
            "/v1/admin/devices/{device_id}",
            axum::routing::delete(devices::delete),
        )
        .layer(DefaultBodyLimit::max(limit))
        .layer(axum::middleware::from_fn_with_state(
            Arc::clone(&state),
            limiter::middleware,
        ))
        .with_state(state)
}

async fn health(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    Json(json!({
        "status": "ok",
        "sync_version": sync::SYNC_VERSION,
        "capture_mode": state.config.capture_mode.as_str(),
        "open_tenants": state.tenants.open_count(),
        "webhook": state.config.webhook.as_ref().map(|_| state.webhook_stats.json()),
    }))
}
