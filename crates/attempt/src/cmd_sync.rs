//! `attempt sync` — connect this database to a sync server, upload now,
//! show status, disconnect. The daemon uploads on its own once connected.

use crate::cli::Cli;
use crate::ctx::Ctx;
use crate::render::print_json;
use anyhow::{Context, Result, bail};
use attemptdb_capture::sync::{SyncConfig, SyncState, describe, upload_once, validate_url};
use clap::{Args, Subcommand};
use serde_json::json;
use std::process::ExitCode;

#[derive(Args, Debug)]
pub struct SyncArgs {
    #[command(subcommand)]
    pub cmd: SyncCmd,
}

#[derive(Subcommand, Debug)]
pub enum SyncCmd {
    /// Store the server URL and device key; the daemon starts uploading.
    Connect(ConnectArgs),
    /// Upload everything after the cursor now.
    Now {
        #[arg(long)]
        json: bool,
    },
    /// Cursor, counts, last success and last error.
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Forget the server and key. The local database is untouched.
    Disconnect,
}

#[derive(Args, Debug)]
pub struct ConnectArgs {
    /// Server base URL, e.g. https://sync.vibemon.dev
    pub url: String,
    /// Bearer key issued for this device.
    #[arg(long)]
    pub key: String,
    /// Also upload content (prompts, commands, tool output). Off by default.
    #[arg(long)]
    pub send_content: bool,
    /// Seconds between daemon uploads.
    #[arg(long, default_value_t = attemptdb_capture::sync::DEFAULT_INTERVAL_SECS)]
    pub interval: u64,
    /// Skip the connectivity check.
    #[arg(long)]
    pub no_verify: bool,
}

pub fn run(cli: &Cli, args: &SyncArgs) -> Result<ExitCode> {
    let ctx = Ctx::new(cli)?;
    let config_dir = ctx.locator.paths.config_dir.clone();
    match &args.cmd {
        SyncCmd::Connect(a) => {
            let url = validate_url(&a.url)?;
            if a.key.trim().is_empty() {
                bail!("--key is empty");
            }
            let cfg = SyncConfig {
                url,
                key: a.key.trim().to_string(),
                send_content: a.send_content,
                batch_events: attemptdb_capture::sync::DEFAULT_BATCH_EVENTS,
                interval_secs: a.interval,
            };
            if !a.no_verify {
                let health = format!("{}/v1/health", cfg.url);
                ureq::get(&health)
                    .timeout(std::time::Duration::from_secs(10))
                    .call()
                    .with_context(|| format!("checking {health} (use --no-verify to skip)"))?;
            }
            cfg.save(&config_dir)?;
            println!("connected: {}", cfg.url);
            println!(
                "  key       {}\n  content   {}\n  interval  {}s\n  config    {}",
                cfg.masked_key(),
                if cfg.send_content {
                    "uploaded (opt-in)"
                } else {
                    "stays local (metadata only)"
                },
                cfg.interval_secs,
                SyncConfig::path(&config_dir).display()
            );
            println!("the daemon uploads on that interval; `attempt sync now` uploads immediately");
            Ok(ExitCode::SUCCESS)
        }
        SyncCmd::Now { json } => {
            let Some(cfg) = SyncConfig::load(&config_dir)? else {
                bail!("not connected: run `attempt sync connect <url> --key <key>` first");
            };
            let report = upload_once(&ctx.locator, &cfg)?;
            if *json {
                print_json(&report);
            } else {
                println!("{}", describe(&report));
            }
            Ok(ExitCode::SUCCESS)
        }
        SyncCmd::Status { json } => {
            let cfg = SyncConfig::load(&config_dir)?;
            let state_path = SyncState::path(&ctx.locator.paths.data_dir, &ctx.locator.db_dir);
            let state = SyncState::load(&state_path)?;
            if *json {
                print_json(&json!({
                    "connected": cfg.is_some(),
                    "url": cfg.as_ref().map(|c| c.url.clone()),
                    "send_content": cfg.as_ref().map(|c| c.send_content),
                    "interval_secs": cfg.as_ref().map(|c| c.interval_secs),
                    "state": state,
                }));
                return Ok(ExitCode::SUCCESS);
            }
            match cfg {
                None => println!("not connected"),
                Some(c) => {
                    println!("connected: {}  (key {})", c.url, c.masked_key());
                    println!(
                        "  content   {}",
                        if c.send_content {
                            "uploaded"
                        } else {
                            "stays local"
                        }
                    );
                    println!("  interval  {}s", c.interval_secs);
                }
            }
            println!(
                "  cursor    source_seq {}  ({} batch(es), {} event(s), {} duplicate(s), {} rejected)",
                state.last_acked_source_seq,
                state.batches,
                state.events,
                state.duplicates,
                state.rejected
            );
            if let Some(t) = state.last_ok_at {
                println!("  last ok   {}", t.to_rfc3339());
            }
            if let Some(e) = &state.last_error {
                let when = state
                    .last_error_at
                    .map(|t| t.to_rfc3339())
                    .unwrap_or_default();
                println!("  last err  {when} {e}");
            }
            Ok(ExitCode::SUCCESS)
        }
        SyncCmd::Disconnect => {
            if SyncConfig::remove(&config_dir)? {
                println!("disconnected; the local database is untouched");
            } else {
                println!("not connected");
            }
            Ok(ExitCode::SUCCESS)
        }
    }
}
