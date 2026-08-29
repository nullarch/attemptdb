//! `attempt repair` and `attempt snapshot restore`.
//!
//! Repair is a dry run by default: it prints what it would do and what it
//! cannot fix. `--apply` executes the plan; actions that move, cut, or delete
//! files ask for confirmation unless `--yes` is given, and refuse outright
//! when there is no terminal to ask.

use crate::cli::Cli;
use crate::ctx::Ctx;
use crate::render::print_json;
use anyhow::{Context, Result};
use attemptdb_storage::repair::{self, RepairAction, RepairPlan, RepairReport, quarantine_target};
use attemptdb_storage::snapshot::{self, RestoreMode};
use attemptdb_storage::{Database, OpenOptions, StorageError};
use clap::Args;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

#[derive(Args, Debug)]
pub struct RepairArgs {
    /// Execute the plan (default: print it and change nothing).
    #[arg(long)]
    pub apply: bool,
    /// Do not ask before quarantining, truncating, or deleting files.
    #[arg(long, short = 'y')]
    pub yes: bool,
}

#[derive(Args, Debug)]
pub struct RestoreArgs {
    /// Snapshot file (`.atdb`) to restore.
    pub file: PathBuf,
    /// Replace an existing database; it is moved to `<db dir>.bak-<unix time>` first.
    #[arg(long)]
    pub replace: bool,
    /// Do not ask for confirmation before replacing.
    #[arg(long, short = 'y')]
    pub yes: bool,
}

pub fn repair(cli: &Cli, args: &RepairArgs) -> Result<ExitCode> {
    if cli.snapshot.is_some() {
        anyhow::bail!(
            "repair works on a live database directory, not on a snapshot; use `attempt snapshot inspect` to check a snapshot"
        );
    }
    let ctx = Ctx::new(cli)?;
    let db_dir = ctx.locator.db_dir.clone();
    let plan = match repair::plan(&db_dir) {
        Ok(p) => p,
        Err(StorageError::NotADatabase(p)) => anyhow::bail!(
            "no database at {}\n  nothing there looks like an AttemptDB directory (no ATTEMPTDB, manifest/, or segments/)",
            p.display()
        ),
        Err(e) => return Err(e).with_context(|| format!("analysing {}", db_dir.display())),
    };

    if !args.apply {
        if cli.json {
            print_json(&serde_json::json!({"database": db_dir, "applied": false, "plan": plan}));
        } else {
            print_plan(&db_dir, &plan);
            if !plan.actions.is_empty() {
                println!();
                println!(
                    "dry run: nothing was changed. Run `attempt repair --apply` to execute the plan."
                );
            }
        }
        return Ok(if plan.is_empty() {
            ExitCode::SUCCESS
        } else {
            ExitCode::from(1)
        });
    }

    if plan.actions.is_empty() {
        if cli.json {
            print_json(
                &serde_json::json!({"database": db_dir, "applied": true, "plan": plan, "report": RepairReport::default()}),
            );
        } else {
            print_plan(&db_dir, &plan);
        }
        return Ok(if plan.problems.is_empty() {
            ExitCode::SUCCESS
        } else {
            ExitCode::from(1)
        });
    }
    if !cli.json {
        print_plan(&db_dir, &plan);
        println!();
    }
    if plan.needs_confirmation() && !args.yes {
        let n = plan.actions.iter().filter(|a| a.is_destructive()).count();
        if !confirm(&format!(
            "{n} action(s) move, cut, or delete files (nothing is deleted without a quarantined copy except temp and tombstoned files); continue"
        ))? {
            println!("aborted; nothing was changed");
            return Ok(ExitCode::from(1));
        }
    }
    let report = match repair::apply(&db_dir, &plan) {
        Ok(r) => r,
        Err(StorageError::Locked(p)) => anyhow::bail!(
            "database at {} is locked by another writer; stop the daemon (`attempt daemon stop`) or the other `attempt` process first",
            p.display()
        ),
        Err(e) => return Err(e).with_context(|| format!("repairing {}", db_dir.display())),
    };
    // Prove the result: the repaired database must open.
    let check = Database::open(
        &db_dir,
        OpenOptions {
            read_only: true,
            ..Default::default()
        },
    );
    let (opens, open_warnings, verify_problems) = match &check {
        Ok(db) => (
            true,
            db.warnings.clone(),
            db.verify().unwrap_or_else(|e| vec![e.to_string()]),
        ),
        Err(e) => (false, vec![e.to_string()], Vec::new()),
    };
    if cli.json {
        print_json(&serde_json::json!({
            "database": db_dir,
            "applied": true,
            "plan": plan,
            "report": report,
            "opens": opens,
            "open_warnings": open_warnings,
            "verify_problems": verify_problems,
        }));
    } else {
        for a in &report.applied {
            println!("applied  {}", describe(&db_dir, a));
        }
        for (a, why) in &report.skipped {
            println!("skipped  {}", describe(&db_dir, a));
            println!("         {why}");
        }
        if let Some(g) = report.new_generation {
            println!("wrote manifest generation {g}");
        }
        match &check {
            Ok(db) => {
                let stats = db.stats();
                println!(
                    "database opens: generation {}, {} segment(s), {} event(s) in segments, {} in WAL",
                    stats.generation, stats.segments, stats.segment_rows, stats.memtable_rows
                );
                for w in &open_warnings {
                    println!("warning: {w}");
                }
                for p in &verify_problems {
                    println!("verify:  {p}");
                }
            }
            Err(e) => println!("database still does not open: {e}"),
        }
        for p in &plan.problems {
            println!("unfixable: {p}");
        }
    }
    let ok = opens && report.skipped.is_empty() && verify_problems.is_empty();
    Ok(if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}

pub fn restore(cli: &Cli, args: &RestoreArgs) -> Result<ExitCode> {
    let ctx = Ctx::new(cli)?;
    let db_dir = ctx.locator.db_dir.clone();
    let occupied = db_dir.exists()
        && std::fs::read_dir(&db_dir)
            .map(|mut d| d.next().is_some())
            .unwrap_or(true);
    let mode = if occupied {
        if !args.replace {
            anyhow::bail!(
                "{} already holds a database; pass --replace to back it up and replace it, or --db DIR to restore elsewhere",
                db_dir.display()
            );
        }
        let backup_to = backup_path(&db_dir)?;
        if !args.yes {
            let ok = confirm(&format!(
                "replace the database at {} with {} (the current one moves to {})",
                db_dir.display(),
                args.file.display(),
                backup_to.display()
            ))?;
            if !ok {
                println!("aborted; nothing was changed");
                return Ok(ExitCode::from(1));
            }
        }
        RestoreMode::ReplaceExisting { backup_to }
    } else {
        RestoreMode::IntoEmptyDir
    };
    let report = match snapshot::restore(&args.file, &db_dir, mode) {
        Ok(r) => r,
        Err(StorageError::Locked(p)) => anyhow::bail!(
            "database at {} is locked by another writer; stop the daemon (`attempt daemon stop`) first",
            p.display()
        ),
        Err(e) => {
            return Err(e).with_context(|| {
                format!(
                    "restoring {} into {}",
                    args.file.display(),
                    db_dir.display()
                )
            });
        }
    };
    if cli.json {
        print_json(
            &serde_json::json!({"database": db_dir, "snapshot": args.file, "events": report.events, "segments": report.segments, "backup": report.backup}),
        );
    } else {
        println!(
            "restored {} event(s) in {} segment(s) from {} into {}",
            report.events,
            report.segments,
            args.file.display(),
            db_dir.display()
        );
        if let Some(b) = &report.backup {
            println!("previous database moved to {}", b.display());
        }
        println!("next: `attempt status`");
    }
    Ok(ExitCode::SUCCESS)
}

fn backup_path(db_dir: &Path) -> Result<PathBuf> {
    let name = db_dir
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .with_context(|| format!("cannot derive a backup name for {}", db_dir.display()))?;
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    Ok(db_dir.with_file_name(format!("{name}.bak-{ts}")))
}

fn print_plan(root: &Path, plan: &RepairPlan) {
    println!("database  {}", root.display());
    if plan.is_empty() {
        println!("nothing to repair");
        return;
    }
    if plan.actions.is_empty() {
        println!("no repairable damage");
    } else {
        println!("{} action(s):", plan.actions.len());
        for a in &plan.actions {
            println!("  {}", describe(root, a));
        }
    }
    if !plan.problems.is_empty() {
        println!();
        println!("{} problem(s) repair cannot fix:", plan.problems.len());
        for p in &plan.problems {
            println!("  - {p}");
        }
    }
}

fn describe(root: &Path, a: &RepairAction) -> String {
    let rel = |p: &Path| p.strip_prefix(root).unwrap_or(p).display().to_string();
    match a {
        RepairAction::AdoptSegment {
            file,
            rows,
            min_seq,
            max_seq,
            sha256,
        } => {
            format!(
                "adopt      segments/{file}  ({rows} rows, source_seq {min_seq}..{max_seq}, sha256 {}…)",
                &sha256[..12.min(sha256.len())]
            )
        }
        RepairAction::QuarantineFile { path, reason } => {
            format!(
                "quarantine {} -> {}  ({reason})",
                rel(path),
                rel(&quarantine_target(root, path))
            )
        }
        RepairAction::RemoveStaleTmp { path } => {
            format!("remove     {}  (stale temp file)", rel(path))
        }
        RepairAction::RemoveUnreferencedTombstoned { file } => {
            format!("delete     segments/{file}  (tombstoned; its data lives in newer segments)")
        }
        RepairAction::RebuildManifest {
            from_generation,
            segments,
        } => format!(
            "rebuild    manifest from {} with {} segment(s): {}",
            if *from_generation == 0 {
                "scratch".to_string()
            } else {
                format!("generation {from_generation}")
            },
            segments.len(),
            segments.join(", ")
        ),
        RepairAction::TruncateTornTail { path, at } => {
            format!("truncate   {} at byte {at}  (torn tail)", rel(path))
        }
        RepairAction::RecreateIdentity { db_id, device_id } => {
            format!(
                "recreate   ATTEMPTDB  (db_id {db_id}, device_id {device_id}; created_at is unknown and set to now)"
            )
        }
    }
}

fn confirm(question: &str) -> Result<bool> {
    use std::io::{IsTerminal, Write};
    if !std::io::stdin().is_terminal() {
        anyhow::bail!(
            "refusing to {question} without confirmation; pass --yes to confirm non-interactively"
        );
    }
    print!("{question}? [y/N] ");
    std::io::stdout().flush().ok();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).ok();
    Ok(matches!(
        line.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}
