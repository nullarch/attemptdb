use clap::{Args, Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(
    name = "attempt",
    version,
    about = "AttemptDB — the database for what agents tried",
    long_about = "Git records what changed. AttemptDB records what AI coding agents attempted.\n\n\
                  First use:\n  attempt init\n  attempt hook install\n  (work normally with your coding agent)\n  attempt timeline",
    propagate_version = true
)]
pub struct Cli {
    /// Portable mode: keep data, config, cache, and logs under this directory.
    #[arg(long, global = true, env = "ATTEMPTDB_DATA_DIR", value_name = "DIR")]
    pub data_dir: Option<PathBuf>,

    /// Use this live database directory instead of auto-detection.
    #[arg(long, global = true, env = "ATTEMPTDB_DIR", value_name = "DIR")]
    pub db: Option<PathBuf>,

    /// Run read-only against a portable `.atdb` snapshot instead of a live database.
    #[arg(long, global = true, value_name = "FILE")]
    pub snapshot: Option<PathBuf>,

    /// Key file for encrypted content (also ATTEMPTDB_KEY_FILE); overrides the OS key store.
    #[arg(long, global = true, env = "ATTEMPTDB_KEY_FILE", value_name = "FILE")]
    pub key_file: Option<PathBuf>,

    /// Emit machine-readable JSON instead of tables.
    #[arg(long, global = true)]
    pub json: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Create a database (per-user by default, or project-local with --local).
    Init(InitArgs),
    /// Hook entrypoint and installer: `hook install|uninstall|status` or `hook <provider>`.
    Hook(HookArgs),
    /// Check the installation: binary, database, and every agent's hook wiring.
    Doctor,
    /// Database location, size, capture mode, and recent activity.
    Status,
    /// Verify manifests, segments, and WAL checksums.
    Verify,
    /// Diagnose and repair a damaged database directory (dry run unless --apply).
    Repair(crate::cmd_repair::RepairArgs),
    /// Merge runs of small segments into one (--dry-run shows the plan).
    Compact(crate::cmd_compact::CompactArgs),
    /// Manage the master key for encrypted content blobs.
    Keys(crate::cmd_keys::KeysArgs),
    /// Correct an attempt's outcome/note or a turn's objective (writes a Correction event).
    Correct(crate::cmd_correct::CorrectArgs),
    /// Retract a session, attempt, or event from every projection (writes a Retraction event).
    Retract(crate::cmd_correct::RetractArgs),
    /// Check a stream of canonical events against AttemptDB Event v1 (spec/).
    Conformance(crate::cmd_conformance::ConformanceArgs),
    /// Upload this database to one or more sync servers (connect / add / now / status / disconnect).
    Sync(crate::cmd_sync::SyncArgs),
    /// Import pending spool files written by hooks (default), or reconstruct history from agent transcripts.
    Import(ImportArgs),
    /// List raw events (newest last).
    Events(EventsArgs),
    /// Export or inspect portable `.atdb` snapshots.
    Snapshot(SnapshotArgs),
    /// Sessions, turns, and attempts — the human-facing timeline.
    Timeline(TimelineArgs),
    /// Run an AttemptQL statement or plain SQL.
    Query(QueryArgs),
    /// Why is a session (or the project) blocked? Evidence-backed answer.
    Why(WhyArgs),
    /// Walk causal edges backwards from an attempt, turn, session, or event.
    Trace(TraceArgs),
    /// Failed and superseded attempts.
    Failures(ScopeArgs),
    /// Work handed off between different coding agents.
    Handoffs(ScopeArgs),
    /// List queryable tables and their columns.
    Tables,
    /// Run, inspect, stop, or install the background capture daemon.
    Daemon(crate::cmd_daemon::DaemonArgs),
    /// Open the local AgentTimeline UI, or `ui export <out.html>` for a shareable static page.
    Ui(crate::cmd_ui::UiArgs),
    /// Serve AttemptDB over MCP (stdio) to coding agents; --print-config / --install register it.
    Mcp(crate::cmd_mcp::McpArgs),
    /// Update the binary from the latest GitHub release (SHA-256 verified, health-checked, rollback-safe).
    Update(crate::cmd_update::UpdateArgs),
    /// Remove hooks from every agent and, with --purge-data, delete the database and config.
    Uninstall(UninstallArgs),
}

#[derive(Args, Debug)]
pub struct InitArgs {
    /// Create `./.attemptdb/` in the current project instead of the per-user database.
    #[arg(long)]
    pub local: bool,
    /// Capture mode: metadata_only, local_semantic (default), or full_sync.
    #[arg(long, value_name = "MODE")]
    pub capture_mode: Option<String>,
    /// Where this install came from (attribution only, never uploaded by the local product).
    #[arg(long, value_name = "SOURCE")]
    pub source: Option<String>,
    /// Do not encrypt content blobs (sets config.encryption = off). Metadata is never encrypted.
    #[arg(long)]
    pub no_encryption: bool,
}

#[derive(Args, Debug)]
pub struct HookArgs {
    /// `install`, `uninstall`, `status`, or a provider id: claude-code, codex, cursor, gemini-cli.
    pub target: String,
    /// Explicit provider event name for providers whose payload lacks one.
    #[arg(long, value_name = "NAME")]
    pub event: Option<String>,
    /// Installer scope.
    #[arg(long, value_enum, default_value_t = ScopeArg::User)]
    pub scope: ScopeArg,
    /// Restrict install/uninstall to these providers (default: all detected).
    #[arg(long = "provider", value_name = "ID")]
    pub providers: Vec<String>,
    /// Show what would change without writing.
    #[arg(long)]
    pub dry_run: bool,
    /// Skip the capture test event after installing.
    #[arg(long)]
    pub no_verify: bool,
    /// Also remove a legacy collector's hook entries (currently: `vibemon`, the ~/.vibemon/notify.sh thin client).
    #[arg(long, value_enum, value_name = "TOOL")]
    pub remove_legacy: Option<LegacyArg>,
}

#[derive(Clone, Copy, Debug, ValueEnum, PartialEq, Eq)]
pub enum LegacyArg {
    Vibemon,
}

#[derive(Clone, Copy, Debug, ValueEnum, PartialEq, Eq)]
pub enum ScopeArg {
    User,
    Project,
    Local,
}

#[derive(Args, Debug, Default)]
pub struct ScopeArgs {
    /// Restrict to a project (name, `prj_` id, or path). Defaults to the current repository when inside one.
    #[arg(long, value_name = "PROJECT")]
    pub project: Option<String>,
    /// Include every project instead of the current one.
    #[arg(long)]
    pub all_projects: bool,
    /// Restrict to one session (`ses_` id or provider session id).
    #[arg(long, value_name = "SESSION")]
    pub session: Option<String>,
    /// Only events observed at or after this time (RFC 3339, `YYYY-MM-DD`, `-2h`, `-30m`, `-1d`, `today`).
    #[arg(long, value_name = "TIME")]
    pub since: Option<String>,
    /// Only events observed at or before this time.
    #[arg(long, value_name = "TIME")]
    pub until: Option<String>,
    /// Maximum rows.
    #[arg(long, short = 'n', value_name = "N")]
    pub limit: Option<usize>,
    /// Ignore events reconstructed from transcripts; use only hook-captured facts.
    #[arg(long)]
    pub captured_only: bool,
}

#[derive(Args, Debug)]
pub struct EventsArgs {
    #[command(flatten)]
    pub scope: ScopeArgs,
    /// Only these canonical kinds (comma separated), e.g. tool_call_failed,prompt_submitted.
    #[arg(long, value_name = "KINDS")]
    pub kind: Option<String>,
}

#[derive(Args, Debug)]
pub struct SnapshotArgs {
    #[command(subcommand)]
    pub cmd: SnapshotCmd,
}

#[derive(Subcommand, Debug)]
pub enum SnapshotCmd {
    /// Flush and export the live database to a single portable file.
    Export {
        /// Output path (`.atdb`).
        out: PathBuf,
        /// Strip prompts, commands, tool output, raw payloads, unknown fields, and absolute paths
        /// so the file can be published. Combine with --project to export one project only.
        #[arg(long)]
        sanitized: bool,
        /// Also drop the git remote URL from a sanitized export.
        #[arg(long, requires = "sanitized")]
        drop_remote: bool,
        /// Replace provider session ids with stable anonymous hashes.
        #[arg(long, requires = "sanitized")]
        anonymize_sessions: bool,
        /// Include encrypted content blobs as-is (readable only where this database's key is).
        #[arg(long, conflicts_with_all = ["key_out", "sanitized"])]
        include_blobs: bool,
        /// Re-wrap content blobs under a fresh key written to FILE so the snapshot opens anywhere with --key-file FILE.
        #[arg(long, value_name = "FILE", conflicts_with = "sanitized")]
        key_out: Option<PathBuf>,
        #[command(flatten)]
        scope: ScopeArgs,
    },
    /// Verify a snapshot and print its contents.
    Inspect { file: PathBuf },
    /// Verify a snapshot and print its status (use `--snapshot FILE` with any query command to query it).
    Open { file: PathBuf },
    /// Privacy review of a snapshot before publishing: content, raw payloads, absolute paths, secrets, emails.
    Audit { file: PathBuf },
    /// Restore a snapshot into the database directory (empty, or --replace with a backup).
    Restore(crate::cmd_repair::RestoreArgs),
}

#[derive(Args, Debug)]
pub struct TimelineArgs {
    #[command(flatten)]
    pub scope: ScopeArgs,
    /// Show tool calls under each attempt.
    #[arg(long)]
    pub tools: bool,
    /// Include sessions with no prompts and no tool calls (capture tests, stray events).
    #[arg(long)]
    pub all: bool,
}

#[derive(Args, Debug)]
pub struct QueryArgs {
    /// AttemptQL (`SHOW FAILED ATTEMPTS`, `WHY ses_… STATUS BLOCKED`, …) or SQL (`SELECT …`).
    pub statement: Vec<String>,
    #[command(flatten)]
    pub scope: ScopeArgs,
    /// Output CSV instead of a table.
    #[arg(long)]
    pub csv: bool,
    /// Show the query plan instead of results (SQL only).
    #[arg(long)]
    pub explain: bool,
}

#[derive(Args, Debug)]
pub struct WhyArgs {
    /// `project` (default), a `ses_` id, or an `att_` id.
    pub subject: Option<String>,
    #[command(flatten)]
    pub scope: ScopeArgs,
}

#[derive(Args, Debug)]
pub struct TraceArgs {
    /// An `att_`, `trn_`, `ses_`, or `ev_` identifier.
    pub id: String,
    #[command(flatten)]
    pub scope: ScopeArgs,
}

#[derive(Args, Debug)]
pub struct UninstallArgs {
    /// Also delete the per-user database, config, cache, and logs. Irreversible.
    #[arg(long)]
    pub purge_data: bool,
    /// Do not ask for confirmation before purging data.
    #[arg(long, short = 'y')]
    pub yes: bool,
    /// Show what would be removed without changing anything.
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Args, Debug)]
pub struct ImportArgs {
    #[command(subcommand)]
    pub source: Option<ImportSource>,
}

#[derive(Subcommand, Debug)]
pub enum ImportSource {
    /// Reconstruct sessions from Claude Code transcripts (~/.claude/projects/**.jsonl). Events are marked reconstructed.
    ClaudeTranscripts(crate::cmd_import::ImportTranscriptArgs),
    /// Backfill history from an export of VibeMon's legacy `hook_events` table (NDJSON or JSON array). Idempotent.
    VibemonExport(crate::cmd_import::ImportVibemonArgs),
}
