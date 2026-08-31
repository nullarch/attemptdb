//! `attempt update`: download → verify → stage → health-check → swap →
//! verify again, with the previous binary kept for `--rollback`. The
//! mechanics live in `attemptdb_capture::update`; this file supplies the
//! health check (the new binary must print its version and, when a database
//! exists, open it) and restarts a running daemon afterwards.

use crate::cli::Cli;
use crate::ctx::Ctx;
use crate::render::print_json;
use anyhow::{Context, Result, bail};
use attemptdb_capture::daemon;
use attemptdb_capture::service;
use attemptdb_capture::update::{self, Outcome, UpdateOptions, UpdateReport};
use attemptdb_storage::Database;
use clap::Args;
use serde::Serialize;
use std::path::Path;
use std::process::{Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

#[derive(Args, Debug)]
pub struct UpdateArgs {
    /// Install this version instead of the latest release (e.g. `--to 0.2.0`).
    #[arg(long, value_name = "VERSION")]
    pub to: Option<String>,
    /// Only check for a newer release; download nothing.
    #[arg(long)]
    pub check: bool,
    /// Reinstall even when already at the resolved version.
    #[arg(long)]
    pub force: bool,
    /// Restore the binary kept by the last update (`attempt.prev`).
    #[arg(long)]
    pub rollback: bool,
    /// Leave a running daemon on the old binary instead of restarting it.
    #[arg(long)]
    pub no_restart: bool,
    /// Skip opening the database with the new binary (`--version` is still checked).
    #[arg(long)]
    pub no_health_check: bool,
}

#[derive(Serialize)]
struct DaemonNote {
    was_running: bool,
    restarted: bool,
    via: Option<String>,
    version: Option<String>,
    pid: Option<u32>,
}

const HEALTH_TIMEOUT: Duration = Duration::from_secs(30);

/// Run `cmd`, killing it after `timeout`. Returns stdout on exit 0.
fn run_with_timeout(cmd: &mut Command, timeout: Duration) -> Result<String> {
    // `spawn_executable`, not `spawn`: this runs a binary written moments ago,
    // and Linux refuses to execute a file another thread still has open for
    // writing.
    let mut child = update::spawn_executable(
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped()),
    )
    .with_context(|| format!("spawning {:?}", cmd.get_program()))?;
    let started = Instant::now();
    loop {
        if child.try_wait()?.is_some() {
            break;
        }
        if started.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            bail!("timed out after {}s", timeout.as_secs());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let out = child.wait_with_output()?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        bail!(
            "exit {}: {}",
            out.status.code().unwrap_or(-1),
            err.lines().next().unwrap_or("").trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// The health check: `--version` must print something, and when a database
/// exists here, `status --json` must succeed against it (that is the failure
/// an update must catch — a binary that runs but cannot read our files).
fn health_check(cli: &Cli, ctx: &Ctx, open_database: bool) -> impl Fn(&Path) -> Result<()> {
    let data_dir = cli.data_dir.clone();
    let db = cli.db.clone();
    let db_exists = Database::exists(&ctx.locator.db_dir);
    move |bin: &Path| {
        let out = run_with_timeout(Command::new(bin).arg("--version"), HEALTH_TIMEOUT)
            .with_context(|| format!("{} --version", bin.display()))?;
        if out.trim().is_empty() {
            bail!("{} --version printed nothing", bin.display());
        }
        if open_database && db_exists {
            let mut cmd = Command::new(bin);
            if let Some(d) = &data_dir {
                cmd.arg("--data-dir").arg(d);
            }
            if let Some(d) = &db {
                cmd.arg("--db").arg(d);
            }
            cmd.args(["status", "--json"]);
            run_with_timeout(&mut cmd, HEALTH_TIMEOUT)
                .with_context(|| format!("{} status --json (open the database)", bin.display()))?;
        }
        Ok(())
    }
}

/// Restart a daemon that was running the old binary: through the service
/// manager when a service is installed, else stop + respawn `daemon run`.
fn restart_daemon(ctx: &Ctx, binary: &Path) -> DaemonNote {
    let mut note = DaemonNote {
        was_running: true,
        restarted: false,
        via: None,
        version: None,
        pid: None,
    };
    match service::restart_service(&ctx.locator) {
        Ok(true) => {
            note.via = Some("service manager".into());
        }
        Ok(false) | Err(_) => {
            let _ = daemon::stop(&ctx.locator);
            daemon::wait_until_stopped(&ctx.locator, Duration::from_secs(15));
            let mut cmd = Command::new(binary);
            cmd.args(["daemon", "run"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            #[cfg(unix)]
            {
                use std::os::unix::process::CommandExt;
                cmd.process_group(0);
            }
            if cmd.spawn().is_ok() {
                note.via = Some("respawned `attempt daemon run`".into());
            }
        }
    }
    if let Some(st) = daemon::wait_until_running(&ctx.locator, Duration::from_secs(15)) {
        note.restarted = true;
        note.version = Some(st.version);
        note.pid = Some(st.pid);
    }
    note
}

fn print_report(report: &UpdateReport, daemon_note: Option<&DaemonNote>) {
    println!(
        "attempt {} ({}) at {}",
        report.current,
        report.target,
        report.binary.display()
    );
    match &report.outcome {
        Outcome::UpToDate => println!("up to date (latest release: {})", report.resolved),
        Outcome::Available => println!(
            "{} is available — run `attempt update` to install it",
            report.resolved
        ),
        Outcome::Updated { previous } => {
            println!("updated to {}", report.resolved);
            println!("previous binary kept at {}", previous.display());
        }
        Outcome::RolledBack { reason } => {
            println!("{} failed its health check: {reason}", report.resolved);
            println!("rolled back to {}", report.current);
        }
        Outcome::Refused { reason } => println!("not updated: {reason}"),
    }
    for n in &report.notes {
        println!("  note: {n}");
    }
    if let Some(d) = daemon_note {
        match (d.restarted, &d.via) {
            (true, Some(via)) => println!(
                "daemon restarted via {via}: pid {}, version {}",
                d.pid.unwrap_or(0),
                d.version.as_deref().unwrap_or("?")
            ),
            _ => println!(
                "daemon was running the old binary and did not come back; start it with `attempt daemon run`"
            ),
        }
    }
}

pub fn run(cli: &Cli, args: &UpdateArgs) -> Result<ExitCode> {
    let ctx = Ctx::new(cli)?;
    let was_running = daemon::status(&ctx.locator).is_some();

    if args.rollback {
        let binary = attemptdb_capture::platform::current_exe_path();
        let failed = update::rollback(&binary)?;
        let daemon_note = (was_running && !args.no_restart).then(|| restart_daemon(&ctx, &binary));
        if cli.json {
            print_json(&serde_json::json!({
                "binary": binary,
                "rolled_back": true,
                "replaced_kept_at": failed,
                "daemon": daemon_note,
            }));
        } else {
            println!("rolled back {}", binary.display());
            println!("the replaced binary is kept at {}", failed.display());
            if let Some(d) = &daemon_note
                && d.restarted
            {
                println!(
                    "daemon restarted: pid {}, version {}",
                    d.pid.unwrap_or(0),
                    d.version.as_deref().unwrap_or("?")
                );
            }
        }
        return Ok(ExitCode::SUCCESS);
    }

    let opts = UpdateOptions {
        version: args.to.clone(),
        force: args.force,
        check_only: args.check,
        binary: None,
        ..UpdateOptions::default()
    };
    let check = health_check(cli, &ctx, !args.no_health_check);
    let report = update::run(&opts, &check)?;
    let daemon_note = match &report.outcome {
        Outcome::Updated { .. } if was_running && !args.no_restart => {
            Some(restart_daemon(&ctx, &report.binary))
        }
        _ => None,
    };
    if cli.json {
        print_json(&serde_json::json!({
            "binary": report.binary,
            "target": report.target,
            "current": report.current,
            "resolved": report.resolved,
            "outcome": report.outcome,
            "notes": report.notes,
            "daemon": daemon_note,
        }));
    } else {
        print_report(&report, daemon_note.as_ref());
    }
    let code = match report.outcome {
        Outcome::RolledBack { .. } | Outcome::Refused { .. } => ExitCode::from(1),
        Outcome::Available if args.check => ExitCode::from(3),
        _ => ExitCode::SUCCESS,
    };
    Ok(code)
}
