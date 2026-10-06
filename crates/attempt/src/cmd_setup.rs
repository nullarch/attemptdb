//! `attempt setup`: everything a fresh machine needs, in one idempotent
//! command — the database, the agent hooks, the background daemon, and a
//! check of the result. The one-line installers (and the VibeMon installers
//! that wrap them) call this and carry no configuration logic of their own,
//! so there is exactly one place that knows how to wire a machine.
//!
//! Every step reports rather than aborts. A machine without a GUI session
//! cannot register a launchd agent, but its hooks still spool to disk and the
//! report says exactly that. The exit code is 1 only for a failure (a hook
//! file that could not be written, a daemon that would not start); a step the
//! user has to finish themselves — trusting Codex's new hook entries — is
//! listed under `needs you` and does not fail the command.
//!
//! `--dry-run` computes the same report without writing anything: what a
//! person — or the coding agent installing AttemptDB for them — reads before
//! letting it change the machine.
//!
//! The last step is the history backfill: the recent Claude Code transcripts
//! and Codex rollouts already on the machine are imported, so the first
//! `attempt ui` or `attempt timeline` shows the person's own work instead of an
//! empty database. It is bounded (30 days, 512 MiB per agent by default,
//! newest first), reads files from their metadata only to plan, goes through
//! the spool when the daemon holds the writer lock, is idempotent (event ids
//! derive from the transcript entries) and honours the database's capture
//! mode. It is optional: `--no-backfill` skips it, and a failure is reported
//! under `history.error` without making setup fail.

use crate::cli::Cli;
use crate::cmd_db::ensure_database;
use crate::cmd_hook::run_capture_tests;
use crate::ctx::Ctx;
use crate::render::{human_bytes, print_json};
use anyhow::{Context, Result};
use attemptdb_capture::agents::{AgentKind, DetectOptions, detect_agents_with};
use attemptdb_capture::daemon::{self, Probe};
use attemptdb_capture::doctor::{HookState, diagnose_scope};
use attemptdb_capture::import::{
    TranscriptSource, claude_projects_dirs, discover_claude_transcripts,
    import_claude_transcripts_to,
};
use attemptdb_capture::import_codex::{
    RolloutSource, codex_session_dirs, count_rollouts, discover_rollouts, import_codex_rollouts,
};
use attemptdb_capture::import_common::{
    BudgetOptions, EventSink, ImportTarget, import_device, open_import_target, pick_within_budget,
};
use attemptdb_capture::install::{
    InstallAction, InstallOptions, Outcome, Scope, install, preferred_hook_binary,
};
use attemptdb_capture::platform::current_exe_path;
use attemptdb_capture::{otel, otel_install, service};
use attemptdb_storage::Database;
use clap::Args;
use serde::Serialize;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
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
    /// Do not import the recent Claude Code and Codex history already on this machine.
    #[arg(long)]
    pub no_backfill: bool,
    /// How many days of history to import (0 = all of it).
    #[arg(long, value_name = "N", default_value_t = DEFAULT_BACKFILL_DAYS)]
    pub backfill_days: u64,
    /// At most this many MiB of transcripts per agent, newest first (0 = no limit).
    #[arg(long, value_name = "N", default_value_t = DEFAULT_BACKFILL_MAX_MIB)]
    pub backfill_max_mib: u64,
}

/// Days of history the backfill imports unless told otherwise.
pub const DEFAULT_BACKFILL_DAYS: u64 = 30;

/// MiB of transcripts per agent the backfill reads unless told otherwise.
pub const DEFAULT_BACKFILL_MAX_MIB: u64 = 512;

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
    /// The recent Claude Code and Codex history imported (or, in a dry run,
    /// what would be).
    pub history: HistoryStep,
    /// The local OpenTelemetry receiver (inside the daemon) that Claude Code
    /// and Codex export to: `attempt otel probe`'s answer, once the daemon
    /// step has run. `None` when no agent was wired for it or the daemon
    /// step was skipped.
    pub telemetry: Option<serde_json::Value>,
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

/// The history backfill. `providers[]` says, per agent, what is on disk
/// inside the window (counts from file metadata; nothing is parsed to
/// produce them) and, once the import ran, what it did.
#[derive(Serialize)]
pub struct HistoryStep {
    /// The step is on (`--no-backfill` turns it off).
    pub enabled: bool,
    /// Why nothing was imported.
    pub skipped: Option<String>,
    /// Window in days; `None` is all history.
    pub days: Option<u64>,
    /// Transcript budget per agent in MiB; `None` is unlimited.
    pub max_mib: Option<u64>,
    /// The capture mode the history is recorded under (`metadata_only` stores no text).
    pub capture_mode: String,
    pub providers: Vec<HistoryProvider>,
    /// Events stored by this run (all agents).
    pub accepted: usize,
    /// Events handed to the daemon's spool instead (it holds the database lock).
    pub queued: usize,
    /// Why the import failed, if it did. Does not make `ok` false: the
    /// history is optional.
    pub error: Option<String>,
}

#[derive(Serialize)]
pub struct HistoryProvider {
    pub agent: AgentKind,
    pub name: &'static str,
    pub directories: Vec<PathBuf>,
    /// Transcript files inside the window and budget.
    pub files: usize,
    /// Sessions those files hold (subagent transcripts do not count).
    pub sessions: usize,
    pub bytes: u64,
    /// Files older than the window.
    pub skipped_old: usize,
    /// Files in the window that did not fit the budget.
    pub skipped_over_budget: usize,
    /// What the import did; absent in a dry run.
    pub imported: Option<HistoryImport>,
}

#[derive(Serialize, Default)]
pub struct HistoryImport {
    pub events: usize,
    pub accepted: usize,
    pub duplicates: usize,
    pub queued: usize,
    pub lines_skipped: usize,
    pub files_failed: usize,
    pub error: Option<String>,
    /// The first few warnings (`attempt import` prints them all).
    pub warnings: Vec<String>,
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
    let scope = providers.clone();
    let binary = current_exe_path();
    let hook_binary = preferred_hook_binary(binary.clone());
    let mut problems = Vec::new();
    let mut needs_you = Vec::new();

    let database = database_step(&mut ctx, args, &mut problems);
    let hooks = hooks_step(cli, &ctx, args, providers, &binary, &mut problems)?;
    let daemon = daemon_step(&ctx, args, &binary, &mut problems);
    let telemetry = telemetry_step(&ctx, args, &hooks, &daemon, &mut problems);
    let history = history_step(&ctx, args, scope.as_deref(), &database);
    let (agents, binary_on_path) = check_step(&hook_binary, &mut needs_you);

    let report = SetupReport {
        version: env!("CARGO_PKG_VERSION"),
        dry_run: args.dry_run,
        binary,
        hook_binary,
        binary_on_path,
        database,
        hooks,
        daemon,
        history,
        telemetry,
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
    binary: &Path,
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
        binary_path: Some(binary.to_path_buf()),
        dry_run: args.dry_run,
        remove_legacy: false,
    };
    let mut report = match install(&opts) {
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
    // Claude Code and Codex also export OpenTelemetry; point them at the
    // local receiver the daemon runs, exactly as `attempt hook install` does.
    if let Err(e) =
        otel_install::apply(&ctx.locator, &Scope::User, &mut report, false, args.dry_run)
    {
        problems.push(format!("telemetry: {e:#}"));
    }
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
fn daemon_step(
    ctx: &Ctx,
    args: &SetupArgs,
    binary: &Path,
    problems: &mut Vec<String>,
) -> DaemonStep {
    let mut step = DaemonStep {
        supported: service::is_supported(),
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
    match service::install_service(&ctx.locator, binary) {
        Ok(path) => {
            step.service = Some(path);
            step.registered = true;
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

/// Step 3b: the OTel receiver. Claude Code and Codex were just told to
/// export to it; it lives inside the daemon, so it is only checked once the
/// daemon step ran, and a receiver that never answers is a problem — the
/// agents would retry against a closed port on every turn.
fn telemetry_step(
    ctx: &Ctx,
    args: &SetupArgs,
    hooks: &HooksStep,
    daemon: &DaemonStep,
    problems: &mut Vec<String>,
) -> Option<serde_json::Value> {
    let eligible = hooks.actions.iter().any(|a| {
        matches!(a.agent, AgentKind::ClaudeCode | AgentKind::Codex)
            && !matches!(a.outcome, Outcome::Failed(_) | Outcome::Skipped(_))
    });
    if !eligible || args.dry_run || daemon.skipped.is_some() || daemon.error.is_some() {
        return None;
    }
    let mut last = None;
    for _ in 0..20 {
        match otel::probe(&ctx.locator) {
            Ok(v) if v["running"] == true => return Some(v),
            Ok(v) => last = Some(v),
            Err(e) => {
                last = Some(
                    serde_json::json!({ "configured": true, "running": false, "error": e.to_string() }),
                )
            }
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    problems.push(format!(
        "telemetry: the local OTel receiver did not answer within 5 s; check {} (a port conflict, or an older daemon)",
        daemon.log.display()
    ));
    last
}

/// Step 5: the history backfill. Plan from file metadata, then import the
/// recent transcripts of each agent in scope. Never aborts and never makes
/// setup fail: whatever goes wrong is reported in the step.
fn history_step(
    ctx: &Ctx,
    args: &SetupArgs,
    scope: Option<&[AgentKind]>,
    database: &DatabaseStep,
) -> HistoryStep {
    let days = (args.backfill_days > 0).then_some(args.backfill_days);
    let max_mib = (args.backfill_max_mib > 0).then_some(args.backfill_max_mib);
    let mut step = HistoryStep {
        enabled: !args.no_backfill,
        skipped: None,
        days,
        max_mib,
        capture_mode: database.capture_mode.clone(),
        providers: Vec::new(),
        accepted: 0,
        queued: 0,
        error: None,
    };
    if args.no_backfill {
        step.skipped = Some("--no-backfill".into());
        return step;
    }
    let in_scope = |k: AgentKind| scope.is_none_or(|s| s.contains(&k));
    let budget = BudgetOptions {
        since: days.map(|d| {
            let micros = i64::try_from(d.saturating_mul(86_400_000_000)).unwrap_or(i64::MAX);
            attemptdb_core::Timestamp::from_micros(
                attemptdb_core::Timestamp::now().as_micros() - micros,
            )
        }),
        max_bytes: max_mib.map(|m| m.saturating_mul(1024 * 1024)),
    };

    // Plan: what is on disk inside the window. Metadata only.
    let mut claude: Vec<TranscriptSource> = Vec::new();
    let mut codex: Vec<RolloutSource> = Vec::new();
    if in_scope(AgentKind::ClaudeCode) {
        let picked = pick_within_budget(
            discover_claude_transcripts(None),
            |s| (s.modified_at, s.bytes),
            &budget,
        );
        let sessions: BTreeSet<String> = picked
            .files
            .iter()
            .filter(|s| !s.is_subagent())
            .filter_map(TranscriptSource::stem)
            .collect();
        step.providers.push(HistoryProvider {
            agent: AgentKind::ClaudeCode,
            name: AgentKind::ClaudeCode.display_name(),
            directories: claude_projects_dirs(),
            files: picked.files.len(),
            sessions: sessions.len(),
            bytes: picked.files.iter().map(|s| s.bytes).sum(),
            skipped_old: picked.skipped_old,
            skipped_over_budget: picked.skipped_over_budget,
            imported: None,
        });
        claude = picked.files;
    }
    if in_scope(AgentKind::Codex) {
        let dirs = codex_session_dirs();
        let picked = pick_within_budget(
            discover_rollouts(&dirs),
            |s| (s.modified_at, s.bytes),
            &budget,
        );
        let counts = count_rollouts(&picked.files);
        step.providers.push(HistoryProvider {
            agent: AgentKind::Codex,
            name: AgentKind::Codex.display_name(),
            directories: dirs,
            files: counts.files,
            sessions: counts.sessions,
            bytes: counts.bytes,
            skipped_old: picked.skipped_old,
            skipped_over_budget: picked.skipped_over_budget,
            imported: None,
        });
        codex = picked.files;
    }

    if args.dry_run {
        return step;
    }
    if database.error.is_some() || !Database::exists(&ctx.locator.db_dir) {
        step.skipped = Some("there is no database to import into".into());
        return step;
    }
    if claude.is_empty() && codex.is_empty() {
        step.skipped = Some("no history in the window".into());
        return step;
    }

    // Import: the writer when it is free, the daemon's spool when it is not.
    let mut target: ImportTarget = match open_import_target(&ctx.locator) {
        Ok(t) => t,
        Err(e) => {
            step.error = Some(format!("opening the database: {e}"));
            return step;
        }
    };
    let device = match import_device(&ctx.locator, &target) {
        Ok(d) => d,
        Err(e) => {
            step.error = Some(format!("reading the device id: {e}"));
            return step;
        }
    };
    let mut errors: Vec<String> = Vec::new();
    for provider in &mut step.providers {
        let result = match provider.agent {
            AgentKind::ClaudeCode if !claude.is_empty() => Some(import_claude_transcripts_to(
                &mut target,
                &claude,
                &ctx.config,
                device,
            )),
            AgentKind::Codex if !codex.is_empty() => Some(import_codex_rollouts(
                &mut target,
                &codex,
                &ctx.config,
                device,
            )),
            _ => None,
        };
        let Some(result) = result else { continue };
        provider.imported = Some(match result {
            Ok(s) => HistoryImport {
                events: s.events_seen,
                accepted: s.accepted,
                duplicates: s.duplicates,
                queued: s.queued,
                lines_skipped: s.lines_skipped,
                files_failed: s.files_failed,
                error: None,
                warnings: s.warnings.into_iter().take(5).collect(),
            },
            Err(e) => {
                errors.push(format!("{}: {e}", provider.name));
                HistoryImport {
                    error: Some(e.to_string()),
                    ..HistoryImport::default()
                }
            }
        });
    }
    if let Err(e) = target.finish() {
        errors.push(format!("flushing: {e}"));
    }
    step.accepted = step
        .providers
        .iter()
        .filter_map(|p| p.imported.as_ref())
        .map(|i| i.accepted)
        .sum();
    step.queued = step
        .providers
        .iter()
        .filter_map(|p| p.imported.as_ref())
        .map(|i| i.queued)
        .sum();
    if !errors.is_empty() {
        step.error = Some(errors.join("; "));
    }
    step
}

/// Step 4: what `attempt doctor` would say about the hook wiring, judged
/// against the binary setup installs, without the activity scan (nothing
/// has been captured yet, and the scan reads the whole database). A stale
/// entry is not listed under `needs you`: setup rewrites it, and a dry run
/// already says "would update". Returns the per-agent lines and whether an
/// `attempt` binary is on `PATH`.
fn check_step(hook_binary: &Path, needs_you: &mut Vec<String>) -> (Vec<AgentCheck>, bool) {
    let diag = diagnose_scope(&Scope::User, Some(hook_binary), &|_| None);
    let mut lines = Vec::new();
    for a in diag.agents {
        let state = state_label(a.state);
        if a.detected && matches!(a.state, HookState::Untrusted | HookState::Disabled) {
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

fn print_history(h: &HistoryStep, dry_run: bool) {
    if let Some(e) = &h.error {
        println!("history      FAILED: {e}  (optional; setup continues)");
    }
    if !h.enabled {
        println!("history      skipped (--no-backfill)");
        return;
    }
    let window = match (h.days, h.max_mib) {
        (Some(d), Some(m)) => format!("last {d} days, up to {m} MiB per agent, newest first"),
        (Some(d), None) => format!("last {d} days"),
        (None, Some(m)) => format!("all history, up to {m} MiB per agent, newest first"),
        (None, None) => "all history".to_string(),
    };
    for (i, p) in h.providers.iter().enumerate() {
        let lead = if i == 0 { "history" } else { "" };
        let what = match &p.imported {
            _ if p.files == 0 => "nothing found".to_string(),
            Some(done) if done.error.is_some() => {
                format!("FAILED: {}", done.error.as_deref().unwrap_or_default())
            }
            Some(done) if done.queued > 0 => format!(
                "queued {} event(s) from {} file(s) for the daemon ({})",
                done.queued,
                p.files,
                human_bytes(p.bytes)
            ),
            Some(done) => format!(
                "imported {} new event(s) from {} file(s), {} session(s) ({}), {} already there",
                done.accepted,
                p.files,
                p.sessions,
                human_bytes(p.bytes),
                done.duplicates
            ),
            None if dry_run => format!(
                "would import {} file(s), {} session(s), {}",
                p.files,
                p.sessions,
                human_bytes(p.bytes)
            ),
            None => format!(
                "{} file(s), {} session(s), {}: not imported",
                p.files,
                p.sessions,
                human_bytes(p.bytes)
            ),
        };
        println!("{lead:<12} {:<13} {what}", p.name);
        if p.skipped_old + p.skipped_over_budget > 0 && p.files > 0 {
            println!(
                "{:<12} {:<13} left out: {} older than the window, {} over the size budget",
                "", "", p.skipped_old, p.skipped_over_budget
            );
        }
    }
    if let Some(why) = &h.skipped {
        println!("{:<12} {:<13} not imported: {why}", "", "");
    } else if !h.providers.is_empty() {
        println!(
            "{:<12} {:<13} {window}; recorded as {}; `attempt import` runs it again for more",
            "", "", h.capture_mode
        );
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

    if let Some(t) = &r.telemetry {
        println!(
            "telemetry    {}",
            if t["running"] == true {
                format!(
                    "local OTel receiver running{}",
                    t["port"]
                        .as_u64()
                        .map(|p| format!(" on port {p}"))
                        .unwrap_or_default()
                )
            } else {
                format!(
                    "local OTel receiver not answering{}",
                    t["error"]
                        .as_str()
                        .map(|e| format!(" ({e})"))
                        .unwrap_or_default()
                )
            }
        );
    }

    print_history(&r.history, r.dry_run);

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
