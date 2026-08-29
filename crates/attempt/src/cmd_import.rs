//! `attempt import claude-transcripts`: reconstruct pre-hook history from
//! the transcript files Claude Code keeps under `~/.claude/projects`.
//!
//! Everything imported here is marked *reconstructed* (`attrs.reconstructed`)
//! and is never presented as captured fact. Re-running the import is safe:
//! event ids are derived from the transcript entries, so only entries that
//! appeared since the last run are added.

use crate::cli::Cli;
use crate::ctx::Ctx;
use crate::render::{human_bytes, print_json, truncate, ts_local};
use anyhow::{Context, Result};
use attemptdb_capture::import::{
    TranscriptSource, claude_projects_dirs, collect_transcripts, discover_claude_transcripts,
    import_claude_transcripts, sort_sources,
};
use attemptdb_capture::ingest;
use attemptdb_storage::Database;
use clap::Args;
use std::collections::BTreeSet;
use std::path::PathBuf;
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
        anyhow::bail!(
            "no database at {}\n  run `attempt init` first (or `attempt init --local` for a project-local database)",
            ctx.locator.db_dir.display()
        );
    }
    let mut db =
        ingest::open_writer(&ctx.locator, false).context("opening the database for writing")?;
    let device = db.device_id();
    let summary = import_claude_transcripts(&mut db, &sources, &ctx.config, device)?;

    if cli.json {
        print_json(&serde_json::json!({"plan": plan, "summary": summary}));
        return Ok(ExitCode::SUCCESS);
    }
    println!();
    println!(
        "imported {} new event(s) from {} file(s); {} duplicate(s) skipped; {} session(s) touched",
        summary.accepted, summary.files, summary.duplicates, summary.sessions
    );
    if summary.files_failed > 0 {
        println!("{} file(s) could not be read", summary.files_failed);
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
    println!();
    println!(
        "these events are reconstructed from transcripts (attrs.reconstructed = true), not captured by hooks;"
    );
    println!(
        "timelines built from them are approximations. Re-running this command only adds new entries."
    );
    Ok(ExitCode::SUCCESS)
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
