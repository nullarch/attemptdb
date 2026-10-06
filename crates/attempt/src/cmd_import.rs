//! `attempt import claude-transcripts`: reconstruct pre-hook history from
//! the transcript files Claude Code keeps under `~/.claude/projects`.
//!
//! Everything imported here is marked *reconstructed* (`attrs.reconstructed`)
//! and is never presented as captured fact. Re-running the import is safe:
//! event ids are derived from the transcript entries, so only entries that
//! appeared since the last run are added.
//!
//! `attempt import codex`: the same for Codex's rollouts under
//! `~/.codex/sessions` (`CODEX_HOME` honoured), streamed so a 600 MB rollout
//! is read in constant memory, and bounded with `--days`, `--since` and
//! `--max-bytes` for a first look at recent work.
//!
//! Both importers write through [`open_import_target`]: the database when the
//! writer lock is free, and otherwise the spool the running daemon drains —
//! the events are *queued* and the daemon stores them within seconds.
//!
//! `attempt import vibemon-export`: backfill history captured live by
//! VibeMon's legacy client from an export of the hosted `hook_events`
//! table. Those events are facts (not reconstructed); their ids derive from
//! the row's primary key, so re-running is a no-op.

use crate::cli::Cli;
use crate::ctx::{Ctx, parse_time};
use crate::render::{human_bytes, print_json, truncate, ts_local};
use anyhow::{Context, Result};
use attemptdb_capture::import::{
    ImportSummary, TranscriptSource, claude_projects_dirs, collect_transcripts,
    discover_claude_transcripts, import_claude_transcripts_to, sort_sources,
};
use attemptdb_capture::import_codex::{
    RolloutCounts, RolloutSource, codex_session_dirs, collect_rollouts, count_rollouts,
    discover_rollouts, import_codex_rollouts,
};
use attemptdb_capture::import_common::{
    BudgetOptions, Picked, import_device, open_import_target, pick_within_budget,
};
use attemptdb_capture::import_vibemon::{
    DevicePolicy, VibemonImportSummary, import_vibemon_export, parse_export_file,
    plan as plan_vibemon,
};
use attemptdb_capture::{CaptureError, ingest};
use attemptdb_core::DeviceId;
use attemptdb_storage::{Database, StorageError};
use clap::Args;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

#[derive(Args, Debug)]
pub struct ImportTranscriptArgs {
    /// Transcript files or directories to import. Default: discover Claude Code's
    /// transcripts for the current repository.
    #[arg(value_name = "PATH")]
    pub paths: Vec<PathBuf>,
    /// Discover transcripts of every project instead of the current repository.
    #[arg(long)]
    pub all_projects: bool,
    /// Show what would be imported without writing anything.
    #[arg(long)]
    pub dry_run: bool,
}

pub fn claude_transcripts(cli: &Cli, args: &ImportTranscriptArgs) -> Result<ExitCode> {
    let ctx = Ctx::new(cli)?;
    let (sources, searched) = if args.paths.is_empty() {
        let root = if args.all_projects {
            None
        } else {
            Some(
                attemptdb_capture::git::git_info(&ctx.cwd)
                    .map(|g| g.root)
                    .unwrap_or_else(|| ctx.cwd.clone()),
            )
        };
        (
            discover_claude_transcripts(root.as_deref()),
            claude_projects_dirs(),
        )
    } else {
        let mut found = Vec::new();
        for path in &args.paths {
            let here = collect_transcripts(path);
            if here.is_empty() && !cli.json {
                eprintln!("warning: no transcripts under {}", path.display());
            }
            found.extend(here);
        }
        sort_sources(&mut found);
        (found, args.paths.clone())
    };

    let plan = Plan::new(&sources, &searched, args);
    if !cli.json {
        plan.print();
    }
    if sources.is_empty() {
        if cli.json {
            print_json(&serde_json::json!({"plan": plan, "summary": null}));
        } else if args.paths.is_empty() && !args.all_projects {
            println!(
                "hint: `attempt import claude-transcripts --all-projects` imports every project, or pass a transcript path"
            );
        }
        return Ok(ExitCode::SUCCESS);
    }
    if args.dry_run {
        if cli.json {
            print_json(&serde_json::json!({"plan": plan, "summary": null}));
        } else {
            println!("dry run: nothing written");
        }
        return Ok(ExitCode::SUCCESS);
    }

    if !Database::exists(&ctx.locator.db_dir) {
        return Err(crate::ctx::no_database(&ctx.locator.db_dir));
    }
    let mut target =
        open_import_target(&ctx.locator).context("opening the database for writing")?;
    let device = import_device(&ctx.locator, &target).context("reading the device id")?;
    let spool = target.is_spool();
    let summary = import_claude_transcripts_to(&mut target, &sources, &ctx.config, device)?;

    if cli.json {
        print_json(
            &serde_json::json!({"plan": plan, "summary": summary, "queued_for_daemon": spool}),
        );
        return Ok(ExitCode::SUCCESS);
    }
    println!();
    print_totals(&summary, spool);
    println!();
    println!(
        "these events are reconstructed from transcripts (attrs.reconstructed = true), not captured by hooks;"
    );
    println!(
        "timelines built from them are approximations. Re-running this command only adds new entries."
    );
    Ok(ExitCode::SUCCESS)
}

/// What an import run did, in words: stored events, or events queued for the
/// daemon that holds the writer lock.
fn print_totals(summary: &ImportSummary, spool: bool) {
    if spool {
        println!(
            "queued {} event(s) from {} file(s) in the spool; the running daemon holds the database lock and imports them within seconds, skipping duplicates; {} session(s) touched",
            summary.queued, summary.files, summary.sessions
        );
    } else {
        println!(
            "imported {} new event(s) from {} file(s); {} duplicate(s) skipped; {} session(s) touched",
            summary.accepted, summary.files, summary.duplicates, summary.sessions
        );
    }
    if summary.files_failed > 0 {
        println!("{} file(s) could not be read", summary.files_failed);
    }
    if summary.lines_skipped > 0 {
        println!(
            "{} line(s) skipped (malformed, over the size limit, or a partial last line)",
            summary.lines_skipped
        );
    }
    for w in summary.warnings.iter().take(20) {
        println!("warning: {}", truncate(w, 200));
    }
    if summary.warnings.len() > 20 {
        println!(
            "... {} more warning(s) (use --json to see all)",
            summary.warnings.len() - 20
        );
    }
}

#[derive(serde::Serialize)]
struct Plan<'a> {
    searched: &'a [PathBuf],
    all_projects: bool,
    dry_run: bool,
    files: usize,
    subagent_files: usize,
    sessions: usize,
    bytes: u64,
    sources: &'a [TranscriptSource],
}

impl<'a> Plan<'a> {
    fn new(
        sources: &'a [TranscriptSource],
        searched: &'a [PathBuf],
        args: &ImportTranscriptArgs,
    ) -> Self {
        let sessions: BTreeSet<String> = sources
            .iter()
            .filter(|s| !s.is_subagent())
            .filter_map(TranscriptSource::stem)
            .collect();
        Self {
            searched,
            all_projects: args.all_projects,
            dry_run: args.dry_run,
            files: sources.len(),
            subagent_files: sources.iter().filter(|s| s.is_subagent()).count(),
            sessions: sessions.len(),
            bytes: sources.iter().map(|s| s.bytes).sum(),
            sources,
        }
    }

    fn print(&self) {
        if self.searched.is_empty() {
            println!(
                "searched      (no Claude Code projects directory found; set CLAUDE_CONFIG_DIR or pass a path)"
            );
        }
        for dir in self.searched {
            println!("searched      {}", dir.display());
        }
        if self.files == 0 {
            println!("transcripts   none found");
            return;
        }
        println!(
            "transcripts   {} file(s) ({}), {} session(s), {} subagent file(s)",
            self.files,
            human_bytes(self.bytes),
            self.sessions,
            self.subagent_files
        );
        for s in self.sources.iter().take(20) {
            println!(
                "  {:<19} {:>9}  {}",
                s.modified_at.map(ts_local).unwrap_or_default(),
                human_bytes(s.bytes),
                truncate(&s.path.display().to_string(), 90)
            );
        }
        if self.files > 20 {
            println!("  ... and {} more", self.files - 20);
        }
    }
}

// ---------------------------------------------------------------------------
// codex
// ---------------------------------------------------------------------------

#[derive(Args, Debug)]
#[command(
    long_about = "Reconstruct history from Codex's rollout files (~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl; \
CODEX_HOME is honoured) into this database.\n\n\
Hosted tools bypass Codex's hooks, so rollouts hold what the agent actually did: prompts, \
messages, shell commands, file changes, MCP and web calls, interruptions, token counts. Events \
are marked reconstructed (attrs.reconstructed = true) and obey the capture mode: under \
metadata_only no text is stored.\n\n\
Rollouts are large; they are streamed line by line, never read whole. Bound a first run with \
--days or --max-bytes: files are taken newest first. Re-running is safe: every event id derives \
from the rollout, so only what is new is added. When the daemon holds the database lock the \
events are queued in the spool and the daemon stores them."
)]
pub struct ImportCodexArgs {
    /// Rollout files or directories to import. Default: ~/.codex/sessions and ~/.codex/archived_sessions ($CODEX_HOME).
    #[arg(long, value_name = "DIR")]
    pub path: Vec<PathBuf>,
    /// Only rollouts modified at or after this time (RFC 3339, `YYYY-MM-DD`, `-2d`, `today`).
    #[arg(
        long,
        value_name = "TIME",
        conflicts_with = "days",
        allow_hyphen_values = true
    )]
    pub since: Option<String>,
    /// Only rollouts modified in the last N days (0 = no limit).
    #[arg(long, value_name = "N")]
    pub days: Option<u64>,
    /// Stop after this much rollout data, newest first (bytes; K, M, G suffixes accepted).
    #[arg(long, value_name = "N")]
    pub max_bytes: Option<String>,
    /// Show what would be imported without writing anything.
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(serde::Serialize)]
struct CodexPlan<'a> {
    searched: &'a [PathBuf],
    since: Option<String>,
    max_bytes: Option<u64>,
    dry_run: bool,
    #[serde(flatten)]
    counts: RolloutCounts,
    skipped_old: usize,
    skipped_over_budget: usize,
    sources: &'a [RolloutSource],
}

pub fn codex(cli: &Cli, args: &ImportCodexArgs) -> Result<ExitCode> {
    let ctx = Ctx::new(cli)?;
    let since = match (&args.since, args.days) {
        (Some(t), _) => Some(parse_time(t).with_context(|| format!("cannot parse --since {t:?}"))?),
        (None, Some(d)) if d > 0 => Some(days_ago(d)),
        _ => None,
    };
    let max_bytes = args
        .max_bytes
        .as_deref()
        .map(parse_size)
        .transpose()
        .context("--max-bytes")?;

    let (found, searched) = if args.path.is_empty() {
        let dirs = codex_session_dirs();
        (discover_rollouts(&dirs), dirs)
    } else {
        let mut found = Vec::new();
        for path in &args.path {
            let here = collect_rollouts(path);
            if here.is_empty() && !cli.json {
                eprintln!("warning: no rollouts under {}", path.display());
            }
            found.extend(here);
        }
        attemptdb_capture::import_codex::sort_newest_first(&mut found);
        found.dedup_by(|a, b| a.path == b.path);
        (found, args.path.clone())
    };
    let picked: Picked<RolloutSource> = pick_within_budget(
        found,
        |s| (s.modified_at, s.bytes),
        &BudgetOptions { since, max_bytes },
    );
    let counts = count_rollouts(&picked.files);
    let plan = CodexPlan {
        searched: &searched,
        since: since.map(|t| t.to_rfc3339()),
        max_bytes,
        dry_run: args.dry_run,
        counts,
        skipped_old: picked.skipped_old,
        skipped_over_budget: picked.skipped_over_budget,
        sources: &picked.files,
    };
    if !cli.json {
        print_codex_plan(&plan);
    }
    if picked.files.is_empty() || args.dry_run {
        if cli.json {
            print_json(&serde_json::json!({"plan": plan, "summary": null}));
        } else if args.dry_run && !picked.files.is_empty() {
            println!("dry run: nothing written");
        }
        return Ok(ExitCode::SUCCESS);
    }

    require_database(&ctx)?;
    let mut target =
        open_import_target(&ctx.locator).context("opening the database for writing")?;
    let device = import_device(&ctx.locator, &target).context("reading the device id")?;
    let spool = target.is_spool();
    let summary = import_codex_rollouts(&mut target, &picked.files, &ctx.config, device)?;

    if cli.json {
        print_json(
            &serde_json::json!({"plan": plan, "summary": summary, "queued_for_daemon": spool}),
        );
        return Ok(ExitCode::SUCCESS);
    }
    println!();
    print_totals(&summary, spool);
    println!();
    println!(
        "these events are reconstructed from Codex rollouts (attrs.reconstructed = true), not captured by hooks;"
    );
    println!(
        "timelines built from them are approximations. Re-running this command only adds new entries."
    );
    Ok(ExitCode::SUCCESS)
}

fn print_codex_plan(plan: &CodexPlan<'_>) {
    if plan.searched.is_empty() {
        println!(
            "searched      (no Codex sessions directory found; set CODEX_HOME or pass --path)"
        );
    }
    for dir in plan.searched {
        println!("searched      {}", dir.display());
    }
    if plan.counts.files == 0 {
        println!("rollouts      none found");
    } else {
        println!(
            "rollouts      {} file(s) ({}), {} session(s)",
            plan.counts.files,
            human_bytes(plan.counts.bytes),
            plan.counts.sessions
        );
    }
    if plan.skipped_old > 0 || plan.skipped_over_budget > 0 {
        println!(
            "left out      {} older than the window, {} over the size budget",
            plan.skipped_old, plan.skipped_over_budget
        );
    }
    for s in plan.sources.iter().take(20) {
        println!(
            "  {:<19} {:>9}  {}",
            s.modified_at.map(ts_local).unwrap_or_default(),
            human_bytes(s.bytes),
            truncate(&s.path.display().to_string(), 90)
        );
    }
    if plan.sources.len() > 20 {
        println!("  ... and {} more", plan.sources.len() - 20);
    }
    if plan.max_bytes.is_none() && plan.since.is_none() && plan.counts.bytes > (2u64 << 30) {
        println!(
            "hint: that is a lot of history; --days 30 or --max-bytes 512M imports the recent part first"
        );
    }
}

/// `N days ago`, as a timestamp.
fn days_ago(days: u64) -> attemptdb_core::Timestamp {
    let micros = i64::try_from(days.saturating_mul(86_400_000_000)).unwrap_or(i64::MAX);
    attemptdb_core::Timestamp::from_micros(attemptdb_core::Timestamp::now().as_micros() - micros)
}

/// A byte count: digits, optionally followed by `k`, `m` or `g` (powers of
/// 1024; `kb`, `kib`, ... accepted).
fn parse_size(text: &str) -> Result<u64> {
    let t = text.trim().to_ascii_lowercase();
    let digits = t.trim_end_matches(|c: char| c.is_ascii_alphabetic());
    let unit = &t[digits.len()..];
    let n: u64 = digits
        .trim()
        .parse()
        .with_context(|| format!("{text:?} is not a number of bytes (try 512M)"))?;
    let factor: u64 = match unit {
        "" | "b" => 1,
        "k" | "kb" | "kib" => 1 << 10,
        "m" | "mb" | "mib" => 1 << 20,
        "g" | "gb" | "gib" => 1 << 30,
        other => anyhow::bail!("unknown size unit {other:?} in {text:?} (use K, M or G)"),
    };
    n.checked_mul(factor)
        .with_context(|| format!("{text:?} is too large"))
}

// ---------------------------------------------------------------------------
// vibemon-export
// ---------------------------------------------------------------------------

#[derive(Args, Debug)]
#[command(
    long_about = "Backfill history captured by VibeMon's legacy client (notify.sh) into this database.\n\n\
FILE is an export of the hosted service's `hook_events` table, either NDJSON (one row per \
line, e.g. psql: \\copy (select row_to_json(h) from hook_events h where user_id = '<uuid>' \
order by created_at) to 'hook_events.ndjson') or a JSON array of rows (the Supabase \
dashboard / PostgREST shape). Unknown columns are ignored. A row missing `id`, `created_at`, \
or a known `event_type` is rejected, counted, and reported with its line number; never fatal.\n\n\
Idempotent: every event id derives from the row's primary key, so re-running (or importing an \
overlapping later export) stores nothing twice and reports those rows as duplicates. Rows are \
ingested in `created_at` order. Events are stored metadata_only (the legacy client never \
captured content) and carry attrs.x_vibemon_import = \"hook_events\"; they are captured \
facts, not reconstructed ones.\n\n\
Device rule: --device <uuid> attributes every event to that device; --device local uses this \
database's own device id, so legacy and live events of the same directory share a project. \
Without --device each row goes to a device derived from its device/machine column \
(device_id, machine_id, install_id) when the export has one, else from its user_id: \
DeviceId::derive([\"vibemon-export\", <value>]). Rows with neither are rejected. The device \
does not enter the event id, so rows already imported keep the device they were first given.\n\n\
Hosted tenants: the same command backfills a server tenant. Run it on the server host with \
--db data/tenants/<tenant>/ while that tenant is not open by the server; the writer lock \
refuses otherwise and the error says so."
)]
pub struct ImportVibemonArgs {
    /// Export of the `hook_events` table: NDJSON (one row per line) or a JSON array of rows.
    #[arg(value_name = "FILE")]
    pub file: PathBuf,
    /// Attribute every event to this device: a UUID, or `local` for this database's own device. Default: derived per row from the device column, else user_id.
    #[arg(long, value_name = "ID|local")]
    pub device: Option<String>,
    /// Parse, plan, and report without writing anything.
    #[arg(long)]
    pub dry_run: bool,
}

pub fn vibemon_export(cli: &Cli, args: &ImportVibemonArgs) -> Result<ExitCode> {
    let ctx = Ctx::new(cli)?;
    let policy = device_policy(args.device.as_deref(), || {
        require_database(&ctx)?;
        let db = ingest::open_reader(&ctx.locator)
            .context("reading this database's device id for --device local")?;
        Ok(db.device_id())
    })?;

    let parsed = parse_export_file(&args.file)
        .with_context(|| format!("reading {}", args.file.display()))?;
    let plan = plan_vibemon(parsed, policy);

    if args.dry_run || plan.summary.events == 0 {
        if cli.json {
            print_json(&serde_json::json!({"file": args.file, "summary": plan.summary}));
        } else {
            print_vibemon_summary(&args.file, &plan.summary);
            println!();
            if plan.summary.events == 0 {
                println!("nothing to import");
            } else {
                println!("dry run: nothing written");
            }
        }
        return Ok(ExitCode::SUCCESS);
    }

    require_database(&ctx)?;
    let mut db = match ingest::open_writer(&ctx.locator, false) {
        Ok(db) => db,
        Err(CaptureError::Storage(StorageError::Locked(path))) => anyhow::bail!(
            "database is locked by another writer: {}\n  \
             a running `attempt daemon` holds the writer lock (stop it with `attempt daemon stop`), \
             or on a server host the tenant is open by the server: close it, then re-run",
            path.display()
        ),
        Err(e) => return Err(e).context("opening the database for writing"),
    };
    let summary = import_vibemon_export(&mut db, plan)?;

    if cli.json {
        print_json(&serde_json::json!({"file": args.file, "summary": summary}));
        return Ok(ExitCode::SUCCESS);
    }
    print_vibemon_summary(&args.file, &summary);
    println!();
    println!(
        "imported {} new event(s); {} duplicate(s) skipped; {} batch(es)",
        summary.accepted, summary.duplicates, summary.batches
    );
    if summary.redactions > 0 {
        println!(
            "warning: {} attr(s) were dropped by the metadata contract at ingest (unexpected for this importer)",
            summary.redactions
        );
    }
    println!();
    println!(
        "these events were captured live by VibeMon's legacy client (attrs.x_vibemon_import = \"hook_events\") and are stored metadata_only;"
    );
    println!("re-running this command, or importing an overlapping export, adds nothing twice.");
    Ok(ExitCode::SUCCESS)
}

/// `--device`: absent → derived per row; `local` → this database's device
/// (looked up lazily, so a dry run without a database still works when the
/// flag is not `local`); anything else must parse as a device id.
fn device_policy(
    spec: Option<&str>,
    local: impl FnOnce() -> Result<DeviceId>,
) -> Result<DevicePolicy> {
    match spec.map(str::trim) {
        None | Some("") => Ok(DevicePolicy::Derived),
        Some("local") => Ok(DevicePolicy::Fixed(local()?)),
        Some(spec) => Ok(DevicePolicy::Fixed(spec.parse::<DeviceId>().with_context(
            || format!("--device {spec:?} is neither a device id (UUID) nor `local`"),
        )?)),
    }
}

fn require_database(ctx: &Ctx) -> Result<()> {
    if !Database::exists(&ctx.locator.db_dir) {
        return Err(crate::ctx::no_database(&ctx.locator.db_dir));
    }
    Ok(())
}

fn print_vibemon_summary(file: &Path, s: &VibemonImportSummary) {
    println!(
        "file          {} ({})",
        file.display(),
        s.format.map(|f| f.as_str()).unwrap_or("empty")
    );
    println!(
        "rows          {} read, {} parsed, {} rejected",
        s.rows_read, s.rows_parsed, s.rows_rejected
    );
    if s.rows_rejected > 0 {
        let by_reason: Vec<String> = s
            .rejected_by_reason
            .iter()
            .map(|(reason, n)| format!("{reason} {n}"))
            .collect();
        println!("  rejected    {}", by_reason.join(", "));
        const SHOWN: usize = 10;
        for r in s.rejections.iter().take(SHOWN) {
            let place = if r.line > 0 {
                format!("line {}", r.line)
            } else {
                "row".to_string()
            };
            let detail = r
                .detail
                .as_deref()
                .map(|d| format!(" ({})", truncate(d, 120)))
                .unwrap_or_default();
            println!("    {place}: {}{detail}", r.reason);
        }
        if s.rows_rejected > SHOWN {
            println!(
                "    ... {} more (use --json for the first {})",
                s.rows_rejected - SHOWN,
                attemptdb_capture::import_vibemon::MAX_REJECTION_DETAILS
            );
        }
    }
    let span = match (&s.first_event, &s.last_event) {
        (Some(a), Some(b)) => format!(", {a} to {b}"),
        _ => String::new(),
    };
    println!(
        "events        {} across {} session(s){span}",
        s.events, s.sessions
    );
    if s.rows_without_session > 0 {
        println!(
            "              {} row(s) carry no session id (grouped under the provider's `unknown` session)",
            s.rows_without_session
        );
    }
    if s.rows_without_cwd > 0 {
        println!(
            "              {} row(s) had no recoverable working directory (project root `/` or the repo identifier)",
            s.rows_without_cwd
        );
    }
    let rule = match s.device_rule {
        "fixed" => "from --device",
        _ => "derived per row from the device column, else user_id; --device overrides",
    };
    match s.devices.len() {
        0 => println!("devices       none ({rule})"),
        n if n <= 5 => {
            for (i, (id, rows)) in s.devices.iter().enumerate() {
                let label = if i == 0 { "devices" } else { "" };
                println!("{label:<13} {id} ({rows} row(s))");
            }
            println!("              {rule}");
        }
        n => println!("devices       {n} distinct ({rule}; see --json)"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_flag_rules() {
        let local = DeviceId::derive(&["cli-tests", "local"]);
        let lookup = || Ok(local);
        assert_eq!(device_policy(None, lookup).unwrap(), DevicePolicy::Derived);
        assert_eq!(
            device_policy(Some("  "), lookup).unwrap(),
            DevicePolicy::Derived
        );
        assert_eq!(
            device_policy(Some("local"), lookup).unwrap(),
            DevicePolicy::Fixed(local)
        );
        let explicit = DeviceId::derive(&["cli-tests", "explicit"]);
        assert_eq!(
            device_policy(Some(&explicit.to_string()), lookup).unwrap(),
            DevicePolicy::Fixed(explicit)
        );
        assert_eq!(
            device_policy(Some(&format!("dev_{explicit}")), lookup).unwrap(),
            DevicePolicy::Fixed(explicit),
            "the display prefix is accepted"
        );
        let err = device_policy(Some("this-machine"), lookup).unwrap_err();
        assert!(err.to_string().contains("neither a device id"), "{err:#}");
        // `local` is the only value that touches the database.
        let failing = || anyhow::bail!("no database");
        assert!(device_policy(Some("local"), failing).is_err());
        assert!(device_policy(None, failing).is_ok());
    }
}
