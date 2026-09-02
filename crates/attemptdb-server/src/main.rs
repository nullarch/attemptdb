//! `attemptdb-server` — run the sync server.
//!
//! Plain HTTP/1.1; put TLS termination in front. Keys are digests in a JSON
//! file (`attemptdb-server digest <key>` prints the line to add).

use anyhow::Result;
use attemptdb_core::CaptureMode;
use attemptdb_server::{Server, ServerConfig, auth};
use clap::{Parser, Subcommand};
use std::net::IpAddr;
use std::path::PathBuf;
// Only the SIGHUP reload below uses it, and that block is Unix-only.
#[cfg(unix)]
use std::sync::Arc;
use std::time::Duration;

#[derive(Parser)]
#[command(name = "attemptdb-server", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// Interface to bind.
    #[arg(long, default_value = "127.0.0.1")]
    bind: IpAddr,
    #[arg(long, default_value_t = 8787)]
    port: u16,
    /// Root directory; tenant databases live under `<data-dir>/tenants/`.
    #[arg(long, default_value = "data")]
    data_dir: PathBuf,
    /// JSON key file: `{"keys":[{"sha256":"…","tenant":"…","device_id":"…"}]}`.
    #[arg(long, default_value = "keys.json")]
    keys: PathBuf,
    /// Ceiling on what clients may persist here.
    #[arg(long, default_value = "metadata_only")]
    capture_mode: CaptureMode,
    /// Open tenant databases kept resident.
    #[arg(long, default_value_t = 256)]
    max_open: usize,
    /// Flush and close a tenant idle for this many seconds.
    #[arg(long, default_value_t = 300)]
    idle_flush_secs: u64,
    /// Largest request body, in bytes.
    #[arg(long, default_value_t = 4 * 1024 * 1024)]
    body_limit: usize,
    /// Enable `/v1/admin/*` (key issuance) behind this bearer token.
    #[arg(long, env = "ATTEMPTDB_ADMIN_TOKEN", hide_env_values = true)]
    admin_token: Option<String>,
    /// Never merge a tenant's small segments on close (default: compact when a tenant is flushed and closed).
    #[arg(long)]
    no_compaction: bool,
    /// Keep only the last N days of a tenant's events in its resident view (0 = whole history).
    /// Bounds memory per tenant; `/v1/events` backfill is unaffected.
    #[arg(long, default_value_t = 0)]
    view_window_days: u32,
    /// Sustained requests per second allowed per bearer key (burst is 10x).
    #[arg(long, default_value_t = 20.0)]
    rate_limit: f64,
    /// Pairing attempts per minute allowed per client address.
    #[arg(long, default_value_t = 12.0)]
    pair_rate_limit: f64,
}

#[derive(Subcommand)]
enum Command {
    /// Print the SHA-256 digest of a bearer key, for the key file.
    Digest { key: String },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    if let Some(Command::Digest { key }) = cli.command {
        println!("{}", auth::digest_hex(&key));
        return Ok(());
    }
    let config = ServerConfig {
        bind: cli.bind,
        port: cli.port,
        data_dir: cli.data_dir,
        keys_file: cli.keys,
        capture_mode: cli.capture_mode,
        max_open: cli.max_open,
        idle_flush: Duration::from_secs(cli.idle_flush_secs),
        body_limit: cli.body_limit,
        admin_token: cli.admin_token,
        compaction: if cli.no_compaction {
            None
        } else {
            Some(attemptdb_storage::CompactionPolicy::default())
        },
        view_window_days: (cli.view_window_days > 0).then_some(cli.view_window_days),
        key_rate: attemptdb_server::limiter::Rate::new(cli.rate_limit, cli.rate_limit * 10.0),
        pair_rate: attemptdb_server::limiter::Rate::new(
            cli.pair_rate_limit / 60.0,
            cli.pair_rate_limit,
        ),
    };
    let server = Server::bind(config.clone()).await?;
    eprintln!(
        "attemptdb-server listening on http://{} (tenants under {}, {} key(s), ceiling {})",
        server.addr(),
        config.data_dir.join("tenants").display(),
        server.state().keys.read().map(|k| k.len()).unwrap_or(0),
        config.capture_mode
    );
    if !config.bind.is_loopback() {
        eprintln!(
            "note: bound to a non-loopback address; this process speaks plain HTTP — terminate TLS in front of it"
        );
    }
    if config.admin_token.is_some() {
        eprintln!("admin surface enabled at /v1/admin/keys");
    }
    // SIGHUP re-reads the key file (an operator edited it by hand).
    #[cfg(unix)]
    {
        let state = Arc::clone(server.state());
        tokio::spawn(async move {
            let Ok(mut hup) =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
            else {
                return;
            };
            while hup.recv().await.is_some() {
                let st = Arc::clone(&state);
                match tokio::task::spawn_blocking(move || st.reload_keys()).await {
                    Ok(Ok(n)) => eprintln!("SIGHUP: key file reloaded ({n} key(s))"),
                    Ok(Err(e)) => eprintln!("SIGHUP: key file NOT reloaded: {e:#}"),
                    Err(e) => eprintln!("SIGHUP: reload task failed: {e}"),
                }
            }
        });
    }
    server
        .run(async {
            let _ = tokio::signal::ctrl_c().await;
            eprintln!("shutting down: flushing open tenants");
        })
        .await
}
