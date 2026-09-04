//! `attempt daemon …`: run the capture daemon, inspect it, stop it, or
//! register it as a per-user background service.
//!
//! Wiring (in `cli.rs` / `main.rs`):
//!
//! ```text
//! Command::Daemon(crate::cmd_daemon::DaemonArgs)   // cli.rs, replaces the unit variant
//! Command::Daemon(args) => cmd_daemon::run(&cli, args),   // main.rs
//! ```

use crate::cli::Cli;
use crate::render::{print_json, ts_local};
use anyhow::{Context, Result};
use attemptdb_capture::daemon::{self, DaemonOptions, Probe};
use attemptdb_capture::platform::current_exe_path;
use attemptdb_capture::{Locator, ipc, service};
use attemptdb_storage::DurabilityPolicy;
use clap::{Args, Subcommand};
use std::process::ExitCode;
use std::time::Duration;

#[derive(Args, Debug)]
pub struct DaemonArgs {
    #[command(subcommand)]
    pub cmd: Option<DaemonCmd>,
    /// Log to stderr as well as to the log file (no subcommand = `run`).
    #[arg(long)]
    pub foreground: bool,
}

#[derive(Subcommand, Debug)]
pub enum DaemonCmd {
    /// Run the daemon in this process until it is stopped (the default).
    Run {
        /// Log to stderr as well as to the log file.
        #[arg(long)]
        foreground: bool,
        /// Acknowledge events before the WAL is fsynced (safe across crashes, not power loss).
        #[arg(long)]
        relaxed: bool,
    },
    /// Ping the daemon and show what it is doing.
    Status,
    /// Ask the running daemon to flush and exit.
    Stop,
    /// Register the daemon as a per-user service (launchd / systemd --user) and start it.
    Install,
    /// Stop the daemon and remove the per-user service registration.
    Uninstall,
}

fn locator(cli: &Cli) -> Result<Locator> {
    let cwd = std::env::current_dir().context("reading current directory")?;
    Ok(Locator::resolve(
        &cwd,
        cli.data_dir.as_deref(),
        cli.db.as_deref(),
    ))
}

pub fn run(cli: &Cli, args: &DaemonArgs) -> Result<ExitCode> {
    let locator = locator(cli)?;
    match &args.cmd {
        None => run_daemon(&locator, args.foreground, false),
        Some(DaemonCmd::Run {
            foreground,
            relaxed,
        }) => run_daemon(&locator, args.foreground || *foreground, *relaxed),
        Some(DaemonCmd::Status) => status(cli, &locator),
        Some(DaemonCmd::Stop) => stop(cli, &locator),
        Some(DaemonCmd::Install) => install(cli, &locator),
        Some(DaemonCmd::Uninstall) => uninstall(cli, &locator),
    }
}

fn run_daemon(locator: &Locator, foreground: bool, relaxed: bool) -> Result<ExitCode> {
    let opts = DaemonOptions {
        foreground,
        durability: if relaxed {
            DurabilityPolicy::Relaxed
        } else {
            DurabilityPolicy::Strict
        },
        inference_source: Some(crate::inferences::source()),
        read_service: Some(std::sync::Arc::new(
            crate::read_service::EngineService::new(),
        )),
        ..Default::default()
    };
    if !foreground {
        eprintln!(
            "attemptdb daemon starting for {} (log: {}); use `attempt daemon status` / `attempt daemon stop`",
            locator.db_dir.display(),
            daemon::log_path(locator).display()
        );
    }
    daemon::run(locator, opts)?;
    Ok(ExitCode::SUCCESS)
}

fn human_uptime(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else if secs < 86_400 {
        format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
    } else {
        format!("{}d{:02}h", secs / 86_400, (secs % 86_400) / 3600)
    }
}

fn status(cli: &Cli, locator: &Locator) -> Result<ExitCode> {
    let endpoint = ipc::endpoint(locator);
    match daemon::probe(locator) {
        Probe::Running(s) => {
            if cli.json {
                print_json(&serde_json::json!({ "running": true, "status": s }));
                return Ok(ExitCode::SUCCESS);
            }
            println!(
                "daemon        running (pid {}, v{}) for {}",
                s.pid,
                s.version,
                human_uptime(s.uptime_secs)
            );
            println!("endpoint      {}", s.endpoint);
            println!("database      {}", s.db_dir.display());
            println!(
                "device        {}   capture mode {}, {} durability",
                s.device_id.short(),
                s.capture_mode,
                s.durability
            );
            println!(
                "ingested      {} events in {} batches / {} WAL commits ({} duplicates, {} rejected) over {} connections{}",
                s.events_ingested,
                s.batches,
                s.wal_commits,
                s.duplicates,
                s.rejected_events,
                s.connections,
                if s.rejected_connections > 0 {
                    format!(", {} rejected", s.rejected_connections)
                } else {
                    String::new()
                }
            );
            println!(
                "spool         {} file(s) imported ({} events){}; pending: {}",
                s.spool_files_imported,
                s.spool_events_imported,
                s.last_spool_import_at
                    .map(|t| format!(", last {}", ts_local(t)))
                    .unwrap_or_default(),
                if s.spool_pending { "yes" } else { "no" }
            );
            println!(
                "storage       generation {}, {} segments, {} events in memtable, last seq {}, {} flushes{}",
                s.generation,
                s.segments,
                s.memtable_rows,
                s.last_source_seq,
                s.flushes,
                s.last_flush_at
                    .map(|t| format!(" (last {})", ts_local(t)))
                    .unwrap_or_default()
            );
            println!("log           {}", s.log_path.display());
            Ok(ExitCode::SUCCESS)
        }
        Probe::Unresponsive(e) => {
            if cli.json {
                print_json(
                    &serde_json::json!({ "running": false, "endpoint": endpoint.to_string(), "error": e.to_string() }),
                );
            } else {
                println!("daemon        not answering at {endpoint} ({e})");
                println!(
                    "              a crashed daemon leaves its socket behind; `attempt daemon` reclaims it"
                );
            }
            Ok(ExitCode::from(1))
        }
        Probe::NotRunning => {
            if cli.json {
                print_json(
                    &serde_json::json!({ "running": false, "endpoint": endpoint.to_string() }),
                );
            } else {
                println!("daemon        not running (nothing listens at {endpoint})");
                println!(
                    "              hooks spool to {}; start the daemon with `attempt daemon` or register it with `attempt daemon install`",
                    locator.db_dir.join("spool").display()
                );
            }
            Ok(ExitCode::from(1))
        }
    }
}

fn stop(cli: &Cli, locator: &Locator) -> Result<ExitCode> {
    let pid = match daemon::probe(locator) {
        Probe::Running(s) => Some(s.pid),
        Probe::Unresponsive(e) => anyhow::bail!(
            "daemon is not answering at {} ({e})",
            ipc::endpoint(locator)
        ),
        Probe::NotRunning => {
            if cli.json {
                print_json(&serde_json::json!({ "stopped": false, "running": false }));
            } else {
                println!("daemon is not running");
            }
            return Ok(ExitCode::SUCCESS);
        }
    };
    daemon::stop(locator)?;
    if !daemon::wait_until_stopped(locator, Duration::from_secs(30)) {
        anyhow::bail!(
            "daemon acknowledged the stop request but is still running after 30 s; check {}",
            daemon::log_path(locator).display()
        );
    }
    if cli.json {
        print_json(&serde_json::json!({ "stopped": true, "pid": pid }));
    } else {
        println!("daemon stopped (pid {})", pid.unwrap_or_default());
        if service::service_path().is_some_and(|p| p.exists()) {
            println!(
                "the per-user service stays registered and restarts the daemon at next login; `attempt daemon install` restarts it now"
            );
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn install(cli: &Cli, locator: &Locator) -> Result<ExitCode> {
    let binary = current_exe_path();
    let path = service::install_service(locator, &binary)?;
    // Windows registers a periodic upload, not a supervised daemon: there is
    // no process to wait for.
    if service::is_periodic_uploader() {
        if cli.json {
            print_json(&serde_json::json!({
                "service": path, "binary": binary, "periodic_upload": true,
                "every_minutes": service::WINDOWS_TASK_MINUTES,
            }));
            return Ok(ExitCode::SUCCESS);
        }
        println!("service       {}", path.display());
        println!("binary        {}", binary.display());
        println!(
            "uploads       every {} minute(s); capture is immediate, the server is at most that far behind",
            service::WINDOWS_TASK_MINUTES
        );
        return Ok(ExitCode::SUCCESS);
    }
    let status = daemon::wait_until_running(locator, Duration::from_secs(10));
    if cli.json {
        print_json(
            &serde_json::json!({ "service": path, "binary": binary, "running": status.is_some(), "status": status }),
        );
        return Ok(ExitCode::SUCCESS);
    }
    println!("service       {}", path.display());
    println!("binary        {}", binary.display());
    match status {
        Some(s) => println!(
            "daemon        running (pid {}), log {}",
            s.pid,
            s.log_path.display()
        ),
        None => println!(
            "daemon        registered but not answering yet; check {}",
            daemon::log_path(locator).display()
        ),
    }
    Ok(ExitCode::SUCCESS)
}

fn uninstall(cli: &Cli, locator: &Locator) -> Result<ExitCode> {
    let removed = service::uninstall_service(locator)?;
    // A daemon started by hand is not the service's; stop it too so
    // `uninstall` leaves nothing running.
    let stopped =
        daemon::stop(locator)? && daemon::wait_until_stopped(locator, Duration::from_secs(30));
    if cli.json {
        print_json(&serde_json::json!({ "removed": removed, "daemon_stopped": stopped }));
        return Ok(ExitCode::SUCCESS);
    }
    match removed {
        Some(p) => println!("removed       {}", p.display()),
        None => println!(
            "no service registration found ({})",
            service::service_label()
        ),
    }
    if stopped {
        println!("daemon        stopped");
    }
    Ok(ExitCode::SUCCESS)
}
