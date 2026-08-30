//! `attempt compact`: merge runs of small segments into one.
//!
//! A dry run prints the plan (`Database::compaction_plan`) and needs no
//! lock. The real run takes the writer lock and merges one run per manifest
//! generation until nothing is left to merge; the inputs are tombstoned and
//! deleted after the next flush or compaction (see
//! `attemptdb_storage::compaction`).

use crate::cli::Cli;
use crate::ctx::Ctx;
use crate::render::{human_bytes, print_json};
use anyhow::{Context, Result};
use attemptdb_capture::{CaptureError, ingest, ipc};
use attemptdb_storage::{
    CompactionPlan, CompactionPolicy, CompactionReport, Database, StorageError,
};
use clap::Args;
use std::path::Path;
use std::process::ExitCode;

#[derive(Args, Debug)]
pub struct CompactArgs {
    /// Show what would be merged without writing anything.
    #[arg(long)]
    pub dry_run: bool,
    /// Compact while the database holds more segments than this.
    #[arg(long, value_name = "N", default_value_t = CompactionPolicy::default().max_segments)]
    pub max_segments: usize,
    /// Only segments smaller than this many bytes are merged; larger ones are kept as they are.
    #[arg(long, value_name = "BYTES", default_value_t = CompactionPolicy::default().small_segment_bytes)]
    pub small_segment_bytes: u64,
    /// A run of consecutive small segments must be at least this long to be merged.
    #[arg(long, value_name = "N", default_value_t = CompactionPolicy::default().min_inputs)]
    pub min_inputs: usize,
}

pub fn run(cli: &Cli, args: &CompactArgs) -> Result<ExitCode> {
    if cli.snapshot.is_some() {
        anyhow::bail!(
            "compaction works on a live database directory, not on a snapshot (a snapshot already holds exactly the segments it was exported with)"
        );
    }
    let ctx = Ctx::new(cli)?;
    let db_dir = ctx.locator.db_dir.clone();
    if !Database::exists(&db_dir) {
        anyhow::bail!(
            "no database at {}\n  run `attempt init` first (or `attempt init --local` for a project-local database)",
            db_dir.display()
        );
    }
    let policy = CompactionPolicy {
        max_segments: args.max_segments,
        small_segment_bytes: args.small_segment_bytes,
        min_inputs: args.min_inputs,
    };

    if args.dry_run {
        // Planning reads the manifest (and segment footers without a key);
        // a read-only handle suffices and never waits for the daemon.
        let db = ingest::open_reader(&ctx.locator)
            .with_context(|| format!("opening {}", db_dir.display()))?;
        let plan = db.compaction_plan(&policy)?;
        if cli.json {
            print_json(&serde_json::json!({
                "database": db_dir,
                "dry_run": true,
                "policy": policy,
                "plan": plan,
                "generation": db.manifest().generation,
            }));
        } else {
            print_plan(&db_dir, &plan);
            if !plan.is_empty() {
                println!();
                println!(
                    "dry run: nothing was changed. Run `attempt compact` to execute the plan."
                );
            }
        }
        return Ok(ExitCode::SUCCESS);
    }

    let mut db = match ingest::open_writer(&ctx.locator, false) {
        Ok(db) => db,
        Err(CaptureError::Storage(StorageError::Locked(p))) => {
            if ipc::daemon_reachable(&ctx.locator) {
                anyhow::bail!(
                    "the daemon holds the writer lock on {}; it compacts on its own schedule — stop it with `attempt daemon stop` to compact now, or use `attempt compact --dry-run` to see the plan",
                    p.display()
                );
            }
            anyhow::bail!(
                "database at {} is locked by another writer (another `attempt` process); wait for it to finish, or use `attempt compact --dry-run` to see the plan",
                p.display()
            );
        }
        Err(e) => return Err(e).with_context(|| format!("opening {}", db_dir.display())),
    };
    let plan = db.compaction_plan(&policy)?;
    if !cli.json {
        print_plan(&db_dir, &plan);
        if !plan.is_empty() {
            println!();
        }
    }
    let mut reports: Vec<CompactionReport> = Vec::new();
    while let Some(report) = db.compact(&policy)? {
        if !cli.json {
            println!(
                "merged   {} segment(s) ({}, {} events) into {} ({}) — generation {}",
                report.inputs.len(),
                human_bytes(report.input_bytes),
                report.events,
                report.output_segment.file,
                human_bytes(report.output_bytes),
                report.generation
            );
        }
        reports.push(report);
    }
    let stats = db.stats();
    let pending_bytes: u64 = db
        .manifest()
        .tombstones
        .iter()
        .filter_map(|t| {
            std::fs::metadata(attemptdb_storage::segment::segments_dir(&db_dir).join(&t.file))
                .ok()
                .map(|m| m.len())
        })
        .sum();
    if cli.json {
        print_json(&serde_json::json!({
            "database": db_dir,
            "dry_run": false,
            "policy": policy,
            "plan": plan,
            "reports": reports,
            "segments": stats.segments,
            "generation": stats.generation,
            "pending_deletions": stats.tombstones,
            "pending_bytes": pending_bytes,
            "warnings": db.warnings,
        }));
        return Ok(ExitCode::SUCCESS);
    }
    if reports.is_empty() {
        println!("nothing to compact");
    } else {
        let merged: usize = reports.iter().map(|r| r.inputs.len()).sum();
        println!(
            "done: {} run(s) merged {} segment(s) into {}; {} segment(s) remain, generation {}",
            reports.len(),
            merged,
            reports.len(),
            stats.segments,
            stats.generation
        );
    }
    if stats.tombstones > 0 {
        println!(
            "pending: {} input file(s) ({}) are deleted after the next flush or compaction, once the generation that dropped them is no longer the newest",
            stats.tombstones,
            human_bytes(pending_bytes)
        );
    }
    for w in &db.warnings {
        println!("warning: {w}");
    }
    Ok(ExitCode::SUCCESS)
}

fn print_plan(db_dir: &Path, plan: &CompactionPlan) {
    println!("database  {}", db_dir.display());
    println!(
        "segments  {} (limit {}; small below {}; runs of at least {})",
        plan.segments_before,
        plan.policy.max_segments,
        human_bytes(plan.policy.small_segment_bytes),
        plan.policy.min_inputs.max(2)
    );
    if plan.is_empty() {
        println!("plan      nothing to compact");
    } else {
        println!(
            "plan      {} run(s) → {} segment(s)",
            plan.runs.len(),
            plan.segments_after
        );
        for (i, run) in plan.runs.iter().enumerate() {
            println!(
                "  run {:<3} {} segment(s) at position {} ({}, {} events, source_seq {}..{}) → one format {} segment",
                i + 1,
                run.inputs.len(),
                run.first_index,
                human_bytes(run.bytes),
                run.rows,
                run.min_source_seq(),
                run.max_source_seq(),
                run.format_version
            );
        }
    }
    for n in &plan.notes {
        println!("note      {n}");
    }
}
