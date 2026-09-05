//! `attempt maintenance`: what the daemon does in the background, as one
//! command — upload to every peer, then apply the release policy. The
//! Windows scheduled task runs it every minute (there is no daemon there);
//! elsewhere it is the way to do by hand what the daemon does on its own.

use crate::cli::Cli;
use crate::ctx::Ctx;
use crate::render::print_json;
use anyhow::Result;
use attemptdb_capture::sync::{SyncConfig, describe, upload_all};
use attemptdb_capture::update::{
    AutoContext, AutoOutcome, CHECK_INTERVAL, Decision, Outcome, UpdateOptions, auto_tick,
    health_check_for,
};
use std::process::ExitCode;

pub fn run(cli: &Cli) -> Result<ExitCode> {
    let ctx = Ctx::new(cli)?;

    // 1. Upload. Opening the database imports whatever the hooks spooled.
    let cfg = SyncConfig::load(&ctx.locator.paths.config_dir)?.unwrap_or_default();
    let mut peers = serde_json::Map::new();
    let mut failed = 0;
    if cfg.is_empty() {
        if !cli.json {
            println!("sync          not connected");
        }
    } else {
        let source = crate::inferences::source();
        for (name, r) in upload_all(&ctx.locator, &cfg, Some(&source)) {
            match r {
                Ok(report) => {
                    if !cli.json {
                        println!("sync {name:<8} {}", describe(&report));
                    }
                    peers.insert(name, serde_json::json!({ "ok": true, "report": report }));
                }
                Err(e) => {
                    failed += 1;
                    if !cli.json {
                        println!("sync {name:<8} error: {e:#}");
                    }
                    peers.insert(
                        name,
                        serde_json::json!({ "ok": false, "error": format!("{e:#}") }),
                    );
                }
            }
        }
    }

    // 2. The release policy. Quiet here: nothing in this process is
    //    mid-capture, and the swap is atomic for the hook binary as well.
    let auto = AutoContext {
        cache_dir: ctx.locator.paths.cache_dir.clone(),
        mode: ctx.config.auto_update,
        quiet: true,
        may_apply: true,
        check_interval: CHECK_INTERVAL,
        opts: UpdateOptions::default(),
    };
    let outcome = auto_tick(&auto, &health_check_for(&ctx.locator));
    if cli.json {
        print_json(&serde_json::json!({ "ok": failed == 0, "peers": peers, "update": outcome }));
    } else {
        print_outcome(&outcome);
    }
    Ok(if failed == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

fn print_outcome(o: &AutoOutcome) {
    let why = |held: &Option<String>| held.as_ref().map(|h| format!(" — {h}")).unwrap_or_default();
    match o {
        AutoOutcome::Disabled => println!("update        automatic updates are off"),
        AutoOutcome::Checked { decision, held, .. } => match decision {
            Decision::UpToDate => println!("update        up to date"),
            Decision::Optional(v) => println!("update        {v} available{}", why(held)),
            Decision::Required(v) => println!("update        {v} is required{}", why(held)),
        },
        AutoOutcome::Applied { report } => match &report.outcome {
            Outcome::Updated { .. } => println!("update        installed {}", report.resolved),
            Outcome::RolledBack { reason } => println!(
                "update        {} failed its health check and was rolled back: {reason}",
                report.resolved
            ),
            Outcome::Refused { reason } => println!("update        not installed: {reason}"),
            other => println!("update        {other:?}"),
        },
        AutoOutcome::Failed { error } => println!("update        check failed: {error}"),
    }
}
