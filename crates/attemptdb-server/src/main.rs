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
    };
    let server = Server::bind(config.clone()).await?;
    eprintln!(
        "attemptdb-server listening on http://{} (tenants under {}, {} key(s), ceiling {})",
        server.addr(),
        config.data_dir.join("tenants").display(),
        server.state().keys.len(),
        config.capture_mode
    );
    if !config.bind.is_loopback() {
        eprintln!(
            "note: bound to a non-loopback address; this process speaks plain HTTP — terminate TLS in front of it"
        );
    }
    server
        .run(async {
            let _ = tokio::signal::ctrl_c().await;
            eprintln!("shutting down: flushing open tenants");
        })
        .await
}
