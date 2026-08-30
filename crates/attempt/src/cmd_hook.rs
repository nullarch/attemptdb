//! `attempt hook …` and `attempt doctor`.

use crate::cli::{Cli, HookArgs, LegacyArg, ScopeArg};
use crate::ctx::Ctx;
use crate::render::{print_json, ts_local};
use anyhow::{Context, Result};
use attemptdb_capture::agents::{AgentKind, detect_agents};
use attemptdb_capture::doctor::{ActivitySummary, HookState, diagnose};
use attemptdb_capture::hook::{HookInput, capture_test_payload, read_stdin, run_hook};
use attemptdb_capture::install::{InstallOptions, Outcome, Scope, install, uninstall};
use attemptdb_core::event::Provider;
use attemptdb_core::{EventKind, Timestamp};
use attemptdb_storage::{Database, ScanFilter};
use std::collections::HashMap;
use std::io::Write;
use std::process::ExitCode;

pub fn run(cli: &Cli, args: &HookArgs) -> Result<ExitCode> {
    match args.target.as_str() {
        "install" => install_cmd(cli, args, false),
        "uninstall" | "remove" => install_cmd(cli, args, true),
        "status" => doctor(cli),
        provider => hook_entry(cli, provider, args.event.as_deref()),
    }
}

/// The hot path. Always exits 0.
fn hook_entry(cli: &Cli, provider_id: &str, event_hint: Option<&str>) -> Result<ExitCode> {
    if AgentKind::from_provider_id(provider_id).is_none()
        && provider_id
            .parse::<Provider>()
            .map(|p| matches!(p, Provider::Other(_)))
            .unwrap_or(true)
    {
        // Unknown provider ids still get captured (as Provider::Other) so a
        // misconfigured hook is visible in the data rather than lost.
    }
    let payload = read_stdin();
    let outcome = run_hook(HookInput {
        provider_id,
        event_hint,
        payload_bytes: payload,
        cwd_hint: std::env::var_os("CLAUDE_PROJECT_DIR").map(Into::into),
        data_dir_override: cli.data_dir.clone(),
        db_override: cli.db.clone(),
    });
    if let Some(s) = &outcome.stdout {
        let mut out = std::io::stdout().lock();
        let _ = out.write_all(s.as_bytes());
        let _ = out.flush();
    }
    // Errors were logged to hook.log; never surface them to the agent.
    Ok(ExitCode::SUCCESS)
}

fn install_cmd(cli: &Cli, args: &HookArgs, remove: bool) -> Result<ExitCode> {
    let ctx = Ctx::new(cli)?;
    let scope = match args.scope {
        ScopeArg::User => Scope::User,
        ScopeArg::Project => Scope::Project(ctx.cwd.clone()),
        ScopeArg::Local => Scope::Local(ctx.cwd.clone()),
    };
    let providers = if args.providers.is_empty() {
        None
    } else {
        let mut kinds = Vec::new();
        for p in &args.providers {
            kinds.push(AgentKind::from_provider_id(p).with_context(|| {
                format!(
                    "unknown provider id {p:?} (expected claude-code, codex, cursor, gemini-cli)"
                )
            })?);
        }
        Some(kinds)
    };
    let opts = InstallOptions {
        scope,
        providers,
        binary_path: None,
        dry_run: args.dry_run,
        remove_legacy: matches!(args.remove_legacy, Some(LegacyArg::Vibemon)),
    };
    let report = if remove {
        uninstall(&opts)?
    } else {
        install(&opts)?
    };
    if cli.json {
        print_json(&report);
    } else {
        if report.actions.is_empty() {
            println!(
                "no coding agents detected (looked for Claude Code, Codex, Cursor, Gemini CLI)"
            );
        }
        for a in &report.actions {
            let label = match &a.outcome {
                Outcome::Installed => "installed".to_string(),
                Outcome::Updated => "updated".to_string(),
                Outcome::AlreadyCurrent => "already current".to_string(),
                Outcome::Removed => "removed".to_string(),
                Outcome::Skipped(r) => format!("skipped: {r}"),
                Outcome::Failed(e) => format!("FAILED: {e}"),
            };
            println!(
                "{:<12} {:<16} {}",
                a.agent.display_name(),
                label,
                a.config_path.display()
            );
            if let Some(b) = &a.backup_path {
                println!("{:<12} backup: {}", "", b.display());
            }
            for n in &a.notes {
                println!("{:<12} note: {n}", "");
            }
        }
        if args.dry_run {
            println!("(dry run — nothing was written)");
        }
    }
    if !remove && !args.dry_run && !args.no_verify {
        verify_capture(cli, &ctx, &report)?;
    }
    let failed = report
        .actions
        .iter()
        .any(|a| matches!(a.outcome, Outcome::Failed(_)));
    Ok(if failed {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    })
}

/// Push a synthetic capture-test event through the real hook pipeline for
/// every installed agent, then import it so `doctor` can show "verified".
fn verify_capture(
    cli: &Cli,
    ctx: &Ctx,
    report: &attemptdb_capture::install::InstallReport,
) -> Result<()> {
    if !Database::exists(&ctx.locator.db_dir) {
        println!(
            "\ndatabase not initialised yet at {} — run `attempt init` to start capturing",
            ctx.locator.db_dir.display()
        );
        return Ok(());
    }
    let mut tested = 0;
    for a in &report.actions {
        if !matches!(
            a.outcome,
            Outcome::Installed | Outcome::Updated | Outcome::AlreadyCurrent
        ) {
            continue;
        }
        let provider: Provider = a.agent.provider_id().parse().expect("infallible");
        let payload = capture_test_payload(&provider, &ctx.cwd);
        let out = run_hook(HookInput {
            provider_id: a.agent.provider_id(),
            event_hint: None,
            payload_bytes: serde_json::to_vec(&payload)?,
            cwd_hint: Some(ctx.cwd.clone()),
            data_dir_override: cli.data_dir.clone(),
            db_override: cli.db.clone(),
        });
        if let Some(e) = out.error {
            println!("{:<12} capture test FAILED: {e}", a.agent.display_name());
        } else {
            tested += 1;
        }
    }
    if tested > 0 {
        if let Ok((mut db, _, _)) = attemptdb_capture::ingest::open_fresh(&ctx.locator, false) {
            let _ = db.flush();
        }
        println!(
            "\ncapture test: {tested} event(s) went through the hook pipeline into {}",
            ctx.locator.db_dir.display()
        );
        println!("next: work normally with your coding agent, then run `attempt timeline`");
    }
    Ok(())
}

pub fn doctor(cli: &Cli) -> Result<ExitCode> {
    let ctx = Ctx::new(cli)?;
    // Activity per provider from the database, when it exists.
    let mut activity: HashMap<AgentKind, ActivitySummary> = HashMap::new();
    let db_line;
    if Database::exists(&ctx.locator.db_dir) {
        match ctx.open(cli) {
            Ok(opened) => {
                let stats = opened.db.stats();
                db_line = format!(
                    "database     {} ({} events in segments, {} in WAL{})",
                    opened.source,
                    stats.segment_rows,
                    stats.memtable_rows,
                    if opened.read_only {
                        ", read-only: another writer holds the lock"
                    } else {
                        ""
                    }
                );
                let events = opened.db.scan(&ScanFilter::default())?;
                for ev in &events {
                    let Some(kind) =
                        AgentKind::from_provider_id(&ev.provider.as_str().replace('_', "-"))
                    else {
                        continue;
                    };
                    let e = activity.entry(kind).or_default();
                    let at = ts_local(ev.observed_at);
                    if ev.kind != EventKind::CaptureTest {
                        e.event_count += 1;
                        e.last_event_at = Some(at);
                    } else {
                        e.capture_test_seen = true;
                        if e.last_event_at.is_none() {
                            e.last_event_at = Some(format!("{at} (capture test)"));
                        }
                    }
                }
            }
            Err(e) => {
                db_line = format!(
                    "database     {} — cannot open: {e}",
                    ctx.locator.db_dir.display()
                )
            }
        }
    } else {
        db_line = format!(
            "database     not initialised at {} — run `attempt init`",
            ctx.locator.db_dir.display()
        );
    }
    let diag = diagnose(&|kind| activity.get(&kind).cloned());
    if cli.json {
        print_json(
            &serde_json::json!({ "diagnosis": diag, "database": db_line, "capture_mode": ctx.config.capture_mode.as_str() }),
        );
        return Ok(ExitCode::SUCCESS);
    }
    println!("attempt {}", env!("CARGO_PKG_VERSION"));
    println!(
        "binary       {}{}",
        diag.binary.display(),
        if diag.binary_on_path {
            ""
        } else {
            "  (not on PATH)"
        }
    );
    println!("{db_line}");
    println!("capture mode {}", ctx.config.capture_mode);
    println!("data dir     {}", diag.paths.data_dir.display());
    match attemptdb_capture::daemon::probe(&ctx.locator) {
        attemptdb_capture::daemon::Probe::Running(s) => {
            println!("daemon       running (pid {}) at {}", s.pid, s.endpoint)
        }
        attemptdb_capture::daemon::Probe::NotRunning => println!(
            "daemon       not running (hooks spool to disk; read commands import the spool)"
        ),
        attemptdb_capture::daemon::Probe::Unresponsive(e) => {
            println!("daemon       not answering ({e})")
        }
    }
    println!();
    let mut problems = 0;
    for a in &diag.agents {
        let state = match a.state {
            HookState::NotInstalled => "not installed",
            HookState::Configured => "configured",
            HookState::Stale => "stale",
            HookState::Untrusted => "untrusted",
            HookState::Unverified => "unverified",
            HookState::Verified => "verified",
            HookState::Active => "active",
        };
        if !a.detected {
            println!("{:<12} not detected", a.agent.display_name());
            continue;
        }
        let act = activity.get(&a.agent);
        let act_s = match act {
            Some(s) if s.event_count > 0 => format!(
                "{} events, last {}",
                s.event_count,
                s.last_event_at.clone().unwrap_or_default()
            ),
            Some(s) if s.capture_test_seen => "capture test ok, no agent events yet".to_string(),
            _ => "no events yet".to_string(),
        };
        println!(
            "{:<12} {:<13} {}  [{}]",
            a.agent.display_name(),
            state,
            a.config_path.display(),
            act_s
        );
        for n in &a.notes {
            println!("{:<12} note: {n}", "");
        }
        if !a.events_missing.is_empty() && !matches!(a.state, HookState::NotInstalled) {
            println!("{:<12} missing events: {}", "", a.events_missing.join(", "));
        }
        if matches!(a.state, HookState::Stale | HookState::Untrusted) {
            problems += 1;
        }
    }
    if detect_agents().is_empty() {
        println!("no coding agents detected");
    }
    println!();
    println!("checked at {}", ts_local(Timestamp::now()));
    Ok(if problems > 0 {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    })
}
