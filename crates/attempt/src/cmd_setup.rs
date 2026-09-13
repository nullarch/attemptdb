//! `attempt setup`: everything a fresh machine needs, in one idempotent
//! command — the database, the agent hooks, the background daemon, and a
//! check of the result. The one-line installer, the desktop app and the
//! editor plugins all call this; none of them carry configuration logic of
//! their own, so there is exactly one place that knows how to wire a machine.
//!
//! Every step reports rather than aborts. A machine without a GUI session
//! cannot register a launchd agent, but its hooks still spool to disk and the
//! report says exactly that. The exit code is 1 only for a failure (a hook
//! file that could not be written, a daemon that would not start); a step the
//! user has to finish themselves — trusting Codex's new hook entries — is
//! listed under `needs you` and does not fail the command.
//!
//! `--dry-run` computes the same report without writing anything, which is
//! how the desktop app learns the state of a machine before offering to set
//! it up.

use crate::cli::Cli;
use crate::cmd_db::ensure_database;
use crate::cmd_hook::run_capture_tests;
use crate::ctx::Ctx;
use crate::render::print_json;
use anyhow::{Context, Result};
use attemptdb_capture::agents::{AgentKind, DetectOptions, detect_agents_with};
use attemptdb_capture::daemon::{self, Probe};
use attemptdb_capture::doctor::{HookState, diagnose};
use attemptdb_capture::install::{
    InstallAction, InstallOptions, Outcome, Scope, install, preferred_hook_binary,
};
use attemptdb_capture::platform::current_exe_path;
use attemptdb_capture::service;
use attemptdb_storage::Database;
use clap::Args;
use serde::Serialize;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

#[derive(Args, Debug)]
pub struct SetupArgs {
    /// Capture mode for a NEW database: metadata_only, local_semantic (default), or full_sync.
    /// An existing database keeps its setting.
    #[arg(long, value_name = "MODE")]
    pub capture_mode: Option<String>,
    /// Where this install came from (attribution only; never uploaded by the local product).
    #[arg(long, value_name = "SOURCE")]
    pub source: Option<String>,
    /// Restrict hook installation to these providers (default: every detected agent).
    #[arg(long = "provider", value_name = "ID")]
    pub providers: Vec<String>,
    /// Do not register the background daemon (hooks spool to disk; read commands import the spool).
    #[arg(long)]
    pub no_daemon: bool,
    /// Skip the capture test event after installing hooks.
    #[arg(long)]
    pub no_verify: bool,
    /// Report the machine's state and what would change; write nothing.
    #[arg(long)]
    pub dry_run: bool,
}

/// The whole report, printed as JSON with `--json`.
#[derive(Serialize)]
pub struct SetupReport {
    pub version: &'static str,
    pub dry_run: bool,
    /// The binary that ran setup; the daemon and `attempt update` use it.
    pub binary: PathBuf,
    /// The executable hooks call: `attempt-hook` beside `binary` when present.
    pub hook_binary: PathBuf,
    pub binary_on_path: bool,
    pub database: DatabaseStep,
    pub hooks: HooksStep,
    pub daemon: DaemonStep,
    pub agents: Vec<AgentCheck>,
    /// Failures: something setup could not do. Non-empty means exit code 1.
    pub problems: Vec<String>,
    /// Steps only the user can finish (trusting Codex's hook entries, …).
    pub needs_you: Vec<String>,
    pub ok: bool,
}

#[derive(Serialize)]
pub struct DatabaseStep {
    pub path: PathBuf,
    pub existed: bool,
    pub created: bool,
    pub capture_mode: String,
    pub encryption: Option<String>,
    pub device_id: Option<String>,
    pub error: Option<String>,
}

#[derive(Serialize)]
pub struct HooksStep {
    /// Provider ids of the agents found on this machine.
    pub detected: Vec<&'static str>,
    pub actions: Vec<InstallAction>,
    pub capture_tests: Vec<CaptureTestLine>,
    pub error: Option<String>,
}

#[derive(Serialize)]
pub struct CaptureTestLine {
    pub agent: AgentKind,
    pub ok: bool,
    pub error: Option<String>,
}

#[derive(Serialize)]
pub struct DaemonStep {
    /// A per-user service manager exists on this platform.
    pub supported: bool,
    /// Windows registers a periodic upload task, not a supervised daemon.
    pub periodic_upload: bool,
    /// The service file or task that is (or would be) registered.
    pub service: Option<PathBuf>,
    pub registered: bool,
    pub running: bool,
    pub pid: Option<u32>,
    pub log: PathBuf,
    /// Why registration was not attempted.
    pub skipped: Option<String>,
    pub error: Option<String>,
}

#[derive(Serialize)]
pub struct AgentCheck {
    pub agent: AgentKind,
    pub name: &'static str,
    pub detected: bool,
    pub version: Option<String>,
    pub state: &'static str,
    pub config_path: PathBuf,
    pub notes: Vec<String>,
}

pub fn run(cli: &Cli, args: &SetupArgs) -> Result<ExitCode> {
    let mut ctx = Ctx::new(cli)?;
    let providers = parse_providers(&args.providers)?;
    let binary = current_exe_path();
    let hook_binary = preferred_hook_binary(binary.clone());
    let mut problems = Vec::new();
    let mut needs_you = Vec::new();

    let database = database_step(&mut ctx, args, &mut problems);
    let hooks = hooks_step(cli, &ctx, args, providers, &mut problems)?;
    let daemon = daemon_step(&ctx, args, &mut problems);
    let (agents, binary_on_path) = check_step(&mut needs_you);

    let report = SetupReport {
        version: env!("CARGO_PKG_VERSION"),
        dry_run: args.dry_run,
        binary,
        hook_binary,
        binary_on_path,
        database,
        hooks,
        daemon,
        agents,
        ok: problems.is_empty(),
        problems,
        needs_you,
    };
    if cli.json {
        print_json(&report);
    } else {
        print_text(&report);
    }
    Ok(if report.ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}

fn parse_providers(ids: &[String]) -> Result<Option<Vec<AgentKind>>> {
    if ids.is_empty() {
        return Ok(None);
    }
    let mut kinds = Vec::new();
    for p in ids {
        kinds.push(AgentKind::from_provider_id(p).with_context(|| {
            format!("unknown provider id {p:?} (expected claude-code, codex, cursor, gemini-cli)")
        })?);
    }
    Ok(Some(kinds))
}

/// Step 1: the per-user database. An existing one is left exactly as it is,
/// including its capture mode; `--capture-mode` only shapes a new one.
fn database_step(ctx: &mut Ctx, args: &SetupArgs, problems: &mut Vec<String>) -> DatabaseStep {
    let path = ctx.locator.db_dir.clone();
    let existed = Database::exists(&path);
    let requested_mode = if existed {
        None
    } else {
        args.capture_mode.as_deref()
    };
    if args.dry_run {
        return DatabaseStep {
            path,
            existed,
            created: false,
            capture_mode: requested_mode
                .map(str::to_string)
                .unwrap_or_else(|| ctx.config.capture_mode.to_string()),
            encryption: None,
            device_id: None,
            error: None,
        };
    }
    match ensure_database(ctx, false, requested_mode, args.source.as_deref(), false) {
        Ok(s) => DatabaseStep {
            path: s.db_dir,
            existed,
            created: s.created,
            capture_mode: s.capture_mode,
            encryption: Some(s.encryption),
            device_id: Some(s.device_id),
            error: None,
        },
        Err(e) => {
            let e = format!("{e:#}");
            problems.push(format!("database: {e}"));
            DatabaseStep {
                path,
                existed,
                created: false,
                capture_mode: ctx.config.capture_mode.to_string(),
                encryption: None,
                device_id: None,
                error: Some(e),
            }
        }
    }
}

/// Step 2: hook entries in every detected agent's user-scope config, next
/// to whatever is already there, then one synthetic event through the real
/// pipeline per agent so the wiring is proven before the user's first turn.
fn hooks_step(
    cli: &Cli,
    ctx: &Ctx,
    args: &SetupArgs,
    providers: Option<Vec<AgentKind>>,
    problems: &mut Vec<String>,
) -> Result<HooksStep> {
    let detected: Vec<&'static str> = detect_agents_with(&DetectOptions {
        probe_versions: false,
        ..Default::default()
    })
    .into_iter()
    .map(|a| a.kind.provider_id())
    .collect();
    let opts = InstallOptions {
        scope: Scope::User,
        providers,
        binary_path: None,
        dry_run: args.dry_run,
        remove_legacy: false,
    };
    let report = match install(&opts) {
        Ok(r) => r,
        Err(e) => {
            let e = format!("{e:#}");
            problems.push(format!("hooks: {e}"));
            return Ok(HooksStep {
                detected,
                actions: Vec::new(),
                capture_tests: Vec::new(),
                error: Some(e),
            });
        }
    };
    for a in &report.actions {
        if let Outcome::Failed(e) = &a.outcome {
            problems.push(format!("hooks: {}: {e}", a.agent.display_name()));
        }
    }
    let capture_tests = if args.dry_run || args.no_verify {
        Vec::new()
    } else {
        run_capture_tests(cli, ctx, &report)?
            .into_iter()
            .map(|t| CaptureTestLine {
                agent: t.agent,
                ok: t.error.is_none(),
                error: t.error,
            })
            .collect()
    };
    for t in &capture_tests {
        if let Some(e) = &t.error {
            problems.push(format!("capture test: {}: {e}", t.agent.display_name()));
        }
    }
    Ok(HooksStep {
        detected,
        actions: report.actions,
        capture_tests,
        error: None,
    })
}

/// Step 3: the background daemon as a per-user service. Not having one is
/// never fatal for capture — hooks spool to disk and every read command
/// imports the spool — so the step only records what it could and could not
/// do, and a registration that fails is a problem, not a crash.
fn daemon_step(ctx: &Ctx, args: &SetupArgs, problems: &mut Vec<String>) -> DaemonStep {
    let mut step = DaemonStep {
        supported: service::is_supported(),
        periodic_upload: service::is_periodic_uploader(),
        service: service::service_path(),
        registered: false,
        running: false,
        pid: None,
        log: daemon::log_path(&ctx.locator),
        skipped: None,
        error: None,
    };
    step.registered = step.service.as_ref().is_some_and(|p| p.exists());
    let probe = |step: &mut DaemonStep| {
        if let Probe::Running(s) = daemon::probe(&ctx.locator) {
            step.running = true;
            step.pid = Some(s.pid);
        }
    };
    if args.no_daemon {
        step.skipped = Some("--no-daemon".into());
        probe(&mut step);
        return step;
    }
    if std::env::var_os("ATTEMPTDB_NO_DAEMON").is_some() {
        step.skipped = Some("ATTEMPTDB_NO_DAEMON is set".into());
        probe(&mut step);
        return step;
    }
    if !step.supported {
        step.skipped = Some(
            "no per-user service manager on this platform; hooks spool to disk and read commands import the spool"
                .into(),
        );
        return step;
    }
    if args.dry_run {
        probe(&mut step);
        return step;
    }
    match service::install_service(&ctx.locator, &current_exe_path()) {
        Ok(path) => {
            step.service = Some(path);
            step.registered = true;
            if step.periodic_upload {
                return step;
            }
            match daemon::wait_until_running(&ctx.locator, Duration::from_secs(10)) {
                Some(s) => {
                    step.running = true;
                    step.pid = Some(s.pid);
                }
                None => {
                    let e = format!(
                        "registered, but the daemon did not answer within 10 s; see {}",
                        step.log.display()
                    );
                    problems.push(format!("daemon: {e}"));
                    step.error = Some(e);
                }
            }
        }
        Err(e) => {
            let e = format!("{e:#}");
            problems.push(format!("daemon: {e}"));
            step.error = Some(e);
        }
    }
    step
}

/// Step 4: what `attempt doctor` would say about the hook wiring, without
/// the activity scan (nothing has been captured yet, and the scan reads the
/// whole database). Returns the per-agent lines and whether an `attempt`
/// binary is on `PATH`.
fn check_step(needs_you: &mut Vec<String>) -> (Vec<AgentCheck>, bool) {
    let diag = diagnose(&|_| None);
    let mut lines = Vec::new();
    for a in diag.agents {
        let state = state_label(a.state);
        if a.detected
            && matches!(
                a.state,
                HookState::Stale | HookState::Untrusted | HookState::Disabled
            )
        {
            let why = a.notes.first().cloned().unwrap_or_default();
            needs_you.push(format!(
                "{}: hooks {state}{}",
                a.agent.display_name(),
                if why.is_empty() {
                    String::new()
                } else {
                    format!(" — {why}")
                }
            ));
        }
        lines.push(AgentCheck {
            agent: a.agent,
            name: a.agent.display_name(),
            detected: a.detected,
            version: a.version,
            state,
            config_path: a.config_path,
            notes: a.notes,
        });
    }
    (lines, diag.binary_on_path)
}

fn state_label(s: HookState) -> &'static str {
    match s {
        HookState::NotInstalled => "not installed",
        HookState::Configured => "configured",
        HookState::Stale => "stale",
        HookState::Untrusted => "untrusted",
        HookState::Disabled => "disabled",
        HookState::Unverified => "unverified",
        HookState::Verified => "verified",
        HookState::Active => "active",
    }
}

fn outcome_label(o: &Outcome) -> String {
    match o {
        Outcome::Installed => "installed".into(),
        Outcome::Updated => "updated".into(),
        Outcome::AlreadyCurrent => "already current".into(),
        Outcome::Removed => "removed".into(),
        Outcome::Skipped(r) => format!("skipped: {r}"),
        Outcome::Failed(e) => format!("FAILED: {e}"),
    }
}

fn print_text(r: &SetupReport) {
    println!(
        "attempt setup {}{}",
        r.version,
        if r.dry_run {
            "  (dry run — nothing was written)"
        } else {
            ""
        }
    );
    println!(
        "binary       {}{}",
        r.binary.display(),
        if r.binary_on_path {
            ""
        } else {
            "  (not on PATH; hooks use the absolute path)"
        }
    );
    if r.hook_binary != r.binary {
        println!("hook binary  {}", r.hook_binary.display());
    }

    let d = &r.database;
    let what = match (d.error.as_deref(), d.created, d.existed, r.dry_run) {
        (Some(e), ..) => format!("FAILED: {e}"),
        (None, true, _, _) => "created".into(),
        (None, false, true, _) => "exists".into(),
        (None, false, false, true) => "would create".into(),
        (None, false, false, false) => "missing".into(),
    };
    println!(
        "database     {:<13} {}  ({}{})",
        what,
        d.path.display(),
        d.capture_mode,
        d.encryption
            .as_deref()
            .map(|e| format!(", encryption {e}"))
            .unwrap_or_default()
    );

    let h = &r.hooks;
    if let Some(e) = &h.error {
        println!("hooks        FAILED: {e}");
    } else if h.actions.is_empty() {
        println!(
            "hooks        no coding agents detected (looked for Claude Code, Codex, Cursor, Gemini CLI)"
        );
    } else {
        for (i, a) in h.actions.iter().enumerate() {
            println!(
                "{:<12} {:<13} {:<16} {}",
                if i == 0 { "hooks" } else { "" },
                a.agent.display_name(),
                if r.dry_run {
                    match &a.outcome {
                        Outcome::Installed => "would install".to_string(),
                        Outcome::Updated => "would update".to_string(),
                        o => outcome_label(o),
                    }
                } else {
                    outcome_label(&a.outcome)
                },
                a.config_path.display()
            );
            for n in &a.notes {
                println!("{:<12} {:<13} note: {n}", "", "");
            }
        }
    }
    if !h.capture_tests.is_empty() {
        let ok = h.capture_tests.iter().filter(|t| t.ok).count();
        let failed: Vec<String> = h
            .capture_tests
            .iter()
            .filter_map(|t| {
                t.error
                    .as_ref()
                    .map(|e| format!("{}: {e}", t.agent.display_name()))
            })
            .collect();
        println!(
            "capture test {ok} event(s) went through the hook pipeline{}",
            if failed.is_empty() {
                String::new()
            } else {
                format!("; FAILED {}", failed.join("; "))
            }
        );
    }

    let dm = &r.daemon;
    let state = if let Some(e) = &dm.error {
        format!("FAILED: {e}")
    } else if let Some(s) = &dm.skipped {
        format!(
            "{}skipped ({s})",
            if dm.running {
                format!(
                    "running (pid {}), registration ",
                    dm.pid.unwrap_or_default()
                )
            } else {
                String::new()
            }
        )
    } else if dm.periodic_upload && dm.registered {
        "scheduled task registered (uploads every minute)".into()
    } else if dm.running {
        format!(
            "running (pid {}){}",
            dm.pid.unwrap_or_default(),
            if dm.registered {
                ", registered"
            } else {
                ", not registered as a service"
            }
        )
    } else if r.dry_run {
        if dm.registered {
            "registered, not running".into()
        } else {
            "would register".into()
        }
    } else {
        "not running".into()
    };
    println!(
        "daemon       {state}{}",
        dm.service
            .as_ref()
            .filter(|_| dm.registered || r.dry_run)
            .map(|p| format!("  {}", p.display()))
            .unwrap_or_default()
    );

    let mut first = true;
    for a in r.agents.iter().filter(|a| a.detected) {
        println!(
            "{:<12} {:<13} {}",
            if first { "check" } else { "" },
            a.name,
            a.state
        );
        first = false;
    }

    if !r.needs_you.is_empty() {
        println!();
        println!("needs you:");
        for n in &r.needs_you {
            println!("  - {n}");
        }
    }
    if !r.problems.is_empty() {
        println!();
        println!("problems:");
        for p in &r.problems {
            println!("  - {p}");
        }
    }
    println!();
    if r.dry_run {
        println!("run `attempt setup` to apply.");
    } else if r.ok {
        println!(
            "done. Work normally with your coding agent; `attempt timeline` shows what it tried, and `attempt ui` opens the timeline."
        );
    } else {
        println!(
            "finished with problems; fix the ones above and run `attempt setup` again (it is safe to repeat)."
        );
    }
}
