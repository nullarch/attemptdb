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
//! TLS is terminated in front of this process; it speaks plain HTTP/1.1.

pub mod auth;
pub mod sync;
pub mod tenants;

use anyhow::{Context, Result};
use attemptdb_core::CaptureMode;
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
        }
    }
}

/// Shared by every request.
pub struct AppState {
    pub config: ServerConfig,
    pub keys: auth::KeyTable,
    pub tenants: tenants::Registry,
}

pub struct Server {
    listener: tokio::net::TcpListener,
    addr: SocketAddr,
    state: Arc<AppState>,
}

impl Server {
    /// Load the key table, bind the listener, and prepare the tenant root.
    pub async fn bind(config: ServerConfig) -> Result<Self> {
        let keys = auth::KeyTable::load(&config.keys_file)
            .with_context(|| format!("loading keys from {}", config.keys_file.display()))?;
        let tenants = tenants::Registry::new(&config.data_dir, config.max_open)?;
        let addr = SocketAddr::new(config.bind, config.port);
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .with_context(|| format!("binding {addr}"))?;
        let addr = listener.local_addr().context("reading the bound address")?;
        let state = Arc::new(AppState {
            config,
            keys,
            tenants,
        });
        Ok(Self {
            listener,
            addr,
            state,
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
        let result = axum::serve(self.listener, app)
            .with_graceful_shutdown(shutdown)
            .await
            .context("serving");
        sweeper.abort();
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
        .layer(DefaultBodyLimit::max(limit))
        .with_state(state)
}

async fn health(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    Json(json!({
        "status": "ok",
        "sync_version": sync::SYNC_VERSION,
        "capture_mode": state.config.capture_mode.as_str(),
        "open_tenants": state.tenants.open_count(),
    }))
}
