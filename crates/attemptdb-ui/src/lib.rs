//! AgentTimeline: the local web UI for AttemptDB, plus the static HTML
//! export.
//!
//! `attempt ui` binds an HTTP server to a loopback port, prints a URL that
//! carries a one-run token, and serves server-rendered pages (project state
//! now, timeline, session waterfall, attempt detail with causal trace,
//! failures, handoffs, blocked explanations, time travel, a query console)
//! and a JSON API under `/api/`. All data goes through
//! [`attemptdb_query::QueryEngine`]; the database is re-opened per request
//! only when its files changed (see [`store`]), and the writer lock is never
//! held between requests.
//!
//! Everything displayed is untrusted content from the user's own sessions:
//! text is HTML-escaped, ids are validated before they become links, no
//! inline scripts run (strict CSP), and no external asset is ever loaded.

#![forbid(unsafe_code)]

mod api;
mod auth;
pub mod card;
pub mod demo;
pub mod export;
mod html;
mod json;
mod pages;
pub mod readonly;
mod scope;
pub mod store;
mod svg;

pub use auth::CSP;
pub use readonly::check_read_only;
pub use store::ScopeArgs;

use anyhow::{Context, Result, bail};
use axum::Router;
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use store::Store;

/// Projection algorithm version shown in the header.
pub const INFERENCE_VERSION: &str = attemptdb_project::ALGORITHM_VERSION;
/// The one sentence every page repeats.
pub const TAGLINE: &str =
    "attempts, blockers and handoffs are inferences with evidence; events are facts";
/// Session cookie name.
pub const COOKIE_NAME: &str = "attemptdb_ui";

/// A session counts as *live* while its last observed activity is inside
/// this window. Beyond it the session is merely open: a provider that never
/// sends an end event leaves sessions open forever, and calling those
/// "running" would be a claim the events do not support.
pub const LIVE_WINDOW_MS: u64 = 30 * 60 * 1_000;

pub const APP_CSS: &str = include_str!("../assets/app.css");
pub const APP_JS: &str = include_str!("../assets/app.js");

/// How the server finds the database and where it listens.
#[derive(Clone, Debug)]
pub struct UiConfig {
    /// Live database directory (`.attemptdb/`).
    pub db_dir: PathBuf,
    /// Portable data root (`--data-dir`), for config and the snapshot cache.
    pub data_dir: Option<PathBuf>,
    /// Serve a read-only `.atdb` snapshot instead of the live database.
    pub snapshot: Option<PathBuf>,
    /// Default project scope: the repository containing this directory.
    pub project_root: Option<PathBuf>,
    /// Interface to bind. Anything but loopback needs `allow_remote`.
    pub bind: IpAddr,
    /// Port; `0` picks a free one.
    pub port: u16,
    pub allow_remote: bool,
}

impl UiConfig {
    pub fn new(db_dir: impl Into<PathBuf>) -> Self {
        Self {
            db_dir: db_dir.into(),
            data_dir: None,
            snapshot: None,
            project_root: None,
            bind: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 0,
            allow_remote: false,
        }
    }
}

/// Shared server state.
pub struct AppState {
    pub(crate) store: Store,
    pub(crate) token: String,
    pub(crate) loopback_only: bool,
}

/// A bound, not yet running server.
pub struct Server {
    listener: tokio::net::TcpListener,
    addr: SocketAddr,
    state: Arc<AppState>,
}

impl Server {
    /// Bind the listener and generate the token. Refuses a non-loopback
    /// `bind` unless `allow_remote` is set.
    pub async fn bind(config: UiConfig) -> Result<Self> {
        let loopback = config.bind.is_loopback();
        if !loopback && !config.allow_remote {
            bail!(
                "refusing to bind {}: it is not a loopback address; pass --allow-remote to expose the UI (the token is the only protection)",
                config.bind
            );
        }
        let addr = SocketAddr::new(config.bind, config.port);
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .with_context(|| format!("binding {addr}"))?;
        let addr = listener.local_addr().context("reading the bound address")?;
        let state = Arc::new(AppState {
            store: Store::new(config),
            token: auth::new_token(),
            loopback_only: loopback,
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

    pub fn token(&self) -> &str {
        &self.state.token
    }

    /// The URL to open: carries the token once.
    pub fn url(&self) -> String {
        let host = match self.addr.ip() {
            IpAddr::V4(ip) if ip.is_unspecified() => "127.0.0.1".to_string(),
            IpAddr::V6(ip) if ip.is_unspecified() => "[::1]".to_string(),
            IpAddr::V6(ip) => format!("[{ip}]"),
            IpAddr::V4(ip) => ip.to_string(),
        };
        format!(
            "http://{host}:{}/?token={}",
            self.addr.port(),
            self.state.token
        )
    }

    /// Serve until `shutdown` resolves.
    pub async fn run(self, shutdown: impl Future<Output = ()> + Send + 'static) -> Result<()> {
        let app = router(Arc::clone(&self.state));
        axum::serve(self.listener, app)
            .with_graceful_shutdown(shutdown)
            .await
            .context("serving the UI")?;
        Ok(())
    }
}

fn asset(content_type: &'static str, body: &'static str) -> Response {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, HeaderValue::from_static(content_type)),
            (header::CACHE_CONTROL, HeaderValue::from_static("no-cache")),
        ],
        body,
    )
        .into_response()
}

fn router(state: Arc<AppState>) -> Router {
    let protected = Router::new()
        .route("/", get(pages::now))
        .route("/timeline", get(pages::timeline))
        .route("/work", get(pages::work))
        .route("/work/{id}", get(pages::work_detail))
        .route("/attention", get(pages::attention))
        .route("/session/{id}", get(pages::session))
        .route("/attempt/{id}", get(pages::attempt))
        .route("/evidence/{id}", get(pages::evidence))
        .route("/failures", get(pages::failures))
        .route("/handoffs", get(pages::handoffs))
        .route("/why", get(pages::why))
        .route("/state", get(pages::state))
        .route("/query", get(pages::query))
        .route("/api/status", get(api::status))
        .route("/api/live", get(api::live))
        .route("/card.svg", get(api::card))
        .route("/api/overview", get(api::overview))
        .route("/api/attention", get(api::attention))
        .route("/api/work", get(api::work))
        .route("/api/timeline", get(api::timeline))
        .route("/api/session/{id}", get(api::session))
        .route("/api/attempt/{id}", get(api::attempt))
        .route("/api/failures", get(api::failures))
        .route("/api/handoffs", get(api::handoffs))
        .route("/api/work_units", get(api::work_units))
        .route("/api/decisions", get(api::decisions))
        .route("/api/why", get(api::why))
        .route("/api/trace/{id}", get(api::trace))
        .route("/api/state", get(api::state))
        .route("/api/evidence/{id}", get(api::evidence))
        .route("/api/query", post(api::query))
        .route("/api/projects", get(api::projects))
        .route("/api/sessions", get(api::sessions))
        .route_layer(middleware::from_fn_with_state(
            Arc::clone(&state),
            auth::guard,
        ));
    Router::new()
        .route(
            "/assets/app.css",
            get(|| async { asset("text/css; charset=utf-8", APP_CSS) }),
        )
        .route(
            "/assets/app.js",
            get(|| async { asset("text/javascript; charset=utf-8", APP_JS) }),
        )
        .merge(protected)
        .fallback(|| async {
            (
                StatusCode::NOT_FOUND,
                [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
                html::bare(
                    "Not found",
                    "<section class=\"card\"><h1>404</h1><p>No such page.</p></section>",
                ),
            )
        })
        .layer(middleware::from_fn(auth::headers))
        .with_state(state)
}

/// Open the system browser on `url` (`open` / `xdg-open` / `cmd /c start`).
/// Failures are reported, never fatal.
pub fn open_browser(url: &str) -> std::io::Result<()> {
    let mut cmd = if cfg!(target_os = "macos") {
        let mut c = std::process::Command::new("open");
        c.arg(url);
        c
    } else if cfg!(target_os = "windows") {
        let mut c = std::process::Command::new("cmd");
        c.args(["/c", "start", "", url]);
        c
    } else {
        let mut c = std::process::Command::new("xdg-open");
        c.arg(url);
        c
    };
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|_| ())
}

/// Parse `--bind`: an IP address (a bare `localhost` is accepted too).
pub fn parse_bind(spec: Option<&str>) -> Result<IpAddr> {
    match spec.map(str::trim) {
        None | Some("") | Some("localhost") => Ok(IpAddr::V4(Ipv4Addr::LOCALHOST)),
        Some(s) => s
            .parse::<IpAddr>()
            .with_context(|| format!("--bind {s:?} is not an IP address")),
    }
}

/// The database path as printed in the header (helper for the CLI).
pub fn describe_source(config: &UiConfig) -> String {
    match &config.snapshot {
        Some(s) => format!("snapshot {}", s.display()),
        None => config.db_dir.display().to_string(),
    }
}
