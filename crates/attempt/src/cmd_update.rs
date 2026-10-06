//! `attempt update`: download → verify → stage → health-check → swap →
//! verify again, with the previous binary kept for `--rollback`. The
//! mechanics live in `attemptdb_capture::update`; this file restarts a
//! running daemon afterwards and hosts `attempt health`, the cheap check the
//! update runs on the new binary (it must print its version and, when a
//! database exists, read its manifest — never a full `status`).

use crate::cli::Cli;
use crate::ctx::Ctx;
use crate::render::print_json;
use anyhow::Result;
use attemptdb_capture::daemon;
use attemptdb_capture::service;
use attemptdb_capture::update::{self, Outcome, UpdateOptions, UpdateReport};
use attemptdb_storage::Database;
use clap::Args;
use serde::Serialize;
use std::path::Path;
use std::process::{Command, ExitCode, Stdio};
use std::time::Duration;

#[derive(Args, Debug)]
pub struct UpdateArgs {
    /// Install this version instead of the latest release (e.g. `--to 1.2.3`).
    /// A version older than the running one is a downgrade and needs `--force`.
    #[arg(long, value_name = "VERSION")]
    pub to: Option<String>,
    /// Only check for a newer release; download nothing.
    #[arg(long)]
    pub check: bool,
    /// Reinstall even when already at the resolved version, and allow `--to` an older version.
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
    /// Why the respawn did not happen, when it did not. Without this the user
    /// is told only that the daemon "did not come back".
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    /// Set when the restart and the status query cannot refer to the same
    /// daemon, so no pid is reported rather than the wrong one.
    #[serde(skip_serializing_if = "Option::is_none")]
    scope_note: Option<String>,
}

/// `attempt health`: the cheap self-check `attempt update` (and the daemon's
/// automatic update) runs on a freshly downloaded binary, and the one that
/// goes with an install the user wants to probe by hand.
///
/// It proves what an update must prove — this binary starts, and can read the
/// database files it is about to be handed — without doing what `status` does
/// (open every segment, which on a large history outlasts any timeout): the
/// identity file and the newest valid manifest generation are loaded
/// read-only, nothing is imported, nothing is written, no lock is taken.
/// Exit 0 when the database is readable or there is none; 1 when it is not.
pub fn health(cli: &Cli) -> Result<ExitCode> {
    let ctx = Ctx::new(cli)?;
    let db = &ctx.locator.db_dir;
    let (state, detail, ok): (&str, serde_json::Value, bool) = if !Database::exists(db) {
        ("absent", serde_json::Value::Null, true)
    } else {
        let read = attemptdb_storage::Identity::load(db)
            .map(|_| ())
            .map_err(|e| format!("identity: {e}"))
            .and_then(|()| {
                attemptdb_storage::manifest::Manifest::load_latest(db)
                    .map_err(|e| format!("manifest: {e}"))
            });
        match read {
            Ok(Some((m, _))) => (
                "ok",
                serde_json::json!({ "generation": m.generation, "segments": m.segments.len() }),
                true,
            ),
            Ok(None) => (
                "ok",
                serde_json::json!({ "generation": 0, "segments": 0 }),
                true,
            ),
            Err(e) => ("unreadable", serde_json::json!(e), false),
        }
    };
    let version = env!("CARGO_PKG_VERSION");
    if cli.json {
        print_json(&serde_json::json!({
            "version": version,
            "database": { "path": db, "state": state, "detail": detail },
            "ok": ok,
        }));
    } else {
        println!("attempt {version}");
        match &detail {
            serde_json::Value::Null => println!("database {state}"),
            d => println!("database {state} ({d})"),
        }
        if !ok {
            eprintln!(
                "the database at {} cannot be read by this binary",
                db.display()
            );
        }
    }
    Ok(if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}

/// Restart a daemon that was running the old binary: through the service
/// manager when a service is installed, else stop + respawn `daemon run`.
///
/// `scoped` says the caller passed an explicit `--data-dir`/`--db`. It matters
/// because the two branches restart different daemons. The service manager's
/// unit is registered per user (`dev.attemptdb.daemon`, `attemptdb.service`)
/// and `restart_service` ignores the locator entirely, so it always bounces
/// the user's daemon — while the status query below is locator-scoped. With a
/// scoped locator those are two different processes, and reporting one's pid
/// under the other's restart is a lie the output used to tell.
fn restart_daemon(ctx: &Ctx, binary: &Path, scoped: bool) -> DaemonNote {
    let mut note = DaemonNote {
        was_running: true,
        restarted: false,
        via: None,
        version: None,
        pid: None,
        error: None,
        scope_note: None,
    };
    match service::restart_service(&ctx.locator) {
        Ok(true) => {
            note.via = Some("service manager".into());
            if scoped {
                // We restarted the user's service; the daemon we can see is
                // the one at the given data directory. Say so instead of
                // pinning its pid to a restart it did not receive.
                note.restarted = true;
                note.scope_note = Some(
                    "the service is registered per user, so `--data-dir`/`--db` did not \
                     scope the restart; the restarted daemon's pid is not reported here"
                        .into(),
                );
                return note;
            }
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
            // `spawn_executable`, not `spawn`: `binary` is the binary the swap
            // just wrote, and on Linux it can still be `ETXTBSY` for a few
            // milliseconds. Failing here is worse than failing the health
            // check — the update reports success while the user's capture
            // daemon stays stopped, which is silent data loss.
            match update::spawn_executable(&mut cmd) {
                Ok(_) => note.via = Some("respawned `attempt daemon run`".into()),
                Err(e) => note.error = Some(format!("respawning the daemon failed: {e}")),
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
        Outcome::UpToDate if report.pinned => {
            println!("already at {}; nothing to do", report.resolved)
        }
        Outcome::UpToDate => println!("up to date (latest release: {})", report.resolved),
        Outcome::Available => println!(
            "{} is available{} — run `attempt update` to install it",
            report.resolved,
            if report.required {
                " and REQUIRED by the release policy (this binary is below its floor)"
            } else {
                ""
            }
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
            (true, Some(via)) if d.pid.is_none() => {
                println!("daemon restarted via {via}");
                if let Some(n) = &d.scope_note {
                    println!("  {n}");
                }
            }
            (true, Some(via)) => println!(
                "daemon restarted via {via}: pid {}, version {}",
                d.pid.unwrap_or(0),
                d.version.as_deref().unwrap_or("?")
            ),
            _ => {
                if let Some(e) = &d.error {
                    println!("daemon was running the old binary and did not come back: {e}");
                } else {
                    println!(
                        "daemon was running the old binary and did not come back; start it with `attempt daemon run`"
                    );
                }
            }
        }
    }
}

pub fn run(cli: &Cli, args: &UpdateArgs) -> Result<ExitCode> {
    let ctx = Ctx::new(cli)?;
    // The service manager's unit is per user, so an explicit data directory
    // does not scope a restart through it. `restart_daemon` needs to know.
    let scoped = cli.data_dir.is_some() || cli.db.is_some();
    let was_running = daemon::status(&ctx.locator).is_some();

    if args.rollback {
        let binary = attemptdb_capture::platform::current_exe_path();
        let failed = update::rollback(&binary)?;
        let daemon_note =
            (was_running && !args.no_restart).then(|| restart_daemon(&ctx, &binary, scoped));
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
    let check = update::health_check_with(&ctx.locator, !args.no_health_check);
    let report = update::run(&opts, &check)?;
    let daemon_note = match &report.outcome {
        Outcome::Updated { .. } if was_running && !args.no_restart => {
            Some(restart_daemon(&ctx, &report.binary, scoped))
        }
        _ => None,
    };
    if cli.json {
        print_json(&serde_json::json!({
            "binary": report.binary,
            "target": report.target,
            "current": report.current,
            "resolved": report.resolved,
            "pinned": report.pinned,
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
