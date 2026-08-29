//! Database-level commands: init, status, verify, import, events, snapshot.

use crate::cli::{Cli, EventsArgs, InitArgs, SnapshotArgs, SnapshotCmd};
use crate::ctx::Ctx;
use crate::render::{human_bytes, print_json, truncate, ts_local};
use anyhow::{Context, Result};
use attemptdb_capture::{Config, DeviceRecord, ingest, locator::LOCAL_DB_DIR_NAME};
use attemptdb_core::{CaptureMode, EventKind};
use attemptdb_storage::{Database, ScanFilter, snapshot};
use std::process::ExitCode;

pub fn init(cli: &Cli, args: &InitArgs) -> Result<ExitCode> {
    let mut ctx = Ctx::new(cli)?;
    if let Some(mode) = &args.capture_mode {
        ctx.config.capture_mode = mode.parse::<CaptureMode>().map_err(|e| anyhow::anyhow!("{e}"))?;
    }
    if let Some(src) = &args.source {
        ctx.config.install_source = Some(src.clone());
    }
    ctx.config.save(&ctx.locator.paths.config_dir)?;
    let device = DeviceRecord::load_or_create(&ctx.locator.paths.data_dir)?;

    let db_dir = if args.local {
        let dir = ctx.cwd.join(LOCAL_DB_DIR_NAME);
        ensure_gitignore(&ctx.cwd);
        dir
    } else {
        ctx.locator.db_dir.clone()
    };
    if Database::exists(&db_dir) {
        println!("database already exists at {}", db_dir.display());
    } else {
        if let Some(parent) = db_dir.parent() {
            std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        Database::create(&db_dir, device.device_id)?;
        println!("created database at {}", db_dir.display());
    }
    println!("capture mode  {}", ctx.config.capture_mode);
    println!("device id     {}", device.device_id.short());
    println!("config        {}", Config::path(&ctx.locator.paths.config_dir).display());
    println!();
    println!("next: `attempt hook install` to wire your coding agents, then work normally and run `attempt timeline`");
    Ok(ExitCode::SUCCESS)
}

fn ensure_gitignore(project: &std::path::Path) {
    let gi = project.join(".gitignore");
    let existing = std::fs::read_to_string(&gi).unwrap_or_default();
    if existing.lines().any(|l| l.trim() == ".attemptdb/" || l.trim() == ".attemptdb") {
        return;
    }
    let mut s = existing;
    if !s.is_empty() && !s.ends_with('\n') {
        s.push('\n');
    }
    s.push_str("\n# AttemptDB live database (local evidence store, never commit)\n.attemptdb/\n");
    if std::fs::write(&gi, s).is_ok() {
        println!("added .attemptdb/ to {}", gi.display());
    }
}

pub fn status(cli: &Cli) -> Result<ExitCode> {
    let ctx = Ctx::new(cli)?;
    let opened = ctx.open(cli)?;
    let stats = opened.db.stats();
    let events = opened.db.scan(&ScanFilter::default())?;
    let mut by_provider: std::collections::BTreeMap<String, (u64, Option<attemptdb_core::Timestamp>)> = Default::default();
    let mut projects: std::collections::BTreeMap<String, u64> = Default::default();
    let mut sessions = std::collections::HashSet::new();
    for ev in &events {
        let e = by_provider.entry(ev.provider.as_str().to_string()).or_default();
        e.0 += 1;
        if ev.kind != EventKind::CaptureTest {
            e.1 = Some(e.1.map_or(ev.observed_at, |t| t.max(ev.observed_at)));
        }
        *projects.entry(ev.project.name.clone()).or_default() += 1;
        sessions.insert(ev.session_id);
    }
    if cli.json {
        print_json(&serde_json::json!({
            "database": opened.source,
            "read_only": opened.read_only,
            "capture_mode": ctx.config.capture_mode.as_str(),
            "generation": stats.generation,
            "segments": stats.segments,
            "segment_rows": stats.segment_rows,
            "segment_bytes": stats.segment_bytes,
            "memtable_rows": stats.memtable_rows,
            "wal_bytes": stats.wal_bytes,
            "spool_pending": stats.spool_pending,
            "events": events.len(),
            "sessions": sessions.len(),
            "providers": by_provider.iter().map(|(k, v)| serde_json::json!({"provider": k, "events": v.0, "last_event_at": v.1.map(|t| t.to_rfc3339())})).collect::<Vec<_>>(),
            "projects": projects,
            "import": opened.import,
            "warnings": opened.db.warnings,
        }));
        return Ok(ExitCode::SUCCESS);
    }
    println!("database      {}{}", opened.source, if opened.read_only { "  (read-only)" } else { "" });
    println!("capture mode  {}", ctx.config.capture_mode);
    println!("events        {} ({} in {} segment(s), {} in WAL) · {} session(s)", events.len(), stats.segment_rows, stats.segments, stats.memtable_rows, sessions.len());
    println!("on disk       {} segments · {} WAL · generation {}", human_bytes(stats.segment_bytes), human_bytes(stats.wal_bytes), stats.generation);
    if let Some(r) = opened.import.as_ref().filter(|r| r.spool_files > 0 || r.accepted > 0) {
        println!("imported      {} new event(s) from {} spool file(s){}", r.accepted, r.spool_files, if r.duplicates > 0 { format!(", {} duplicate(s) skipped", r.duplicates) } else { String::new() });
    }
    if stats.spool_pending {
        println!("spool         pending files could not be imported (read-only)");
    }
    if !by_provider.is_empty() {
        println!();
        for (p, (n, last)) in &by_provider {
            println!("{:<13} {:>7} events   last {}", p, n, last.map(ts_local).unwrap_or_else(|| "capture test only".into()));
        }
    }
    if !projects.is_empty() {
        println!();
        for (p, n) in projects.iter().take(20) {
            println!("{:<40} {:>7} events", truncate(p, 40), n);
        }
    }
    for w in &opened.db.warnings {
        println!("warning: {w}");
    }
    Ok(ExitCode::SUCCESS)
}

pub fn verify(cli: &Cli) -> Result<ExitCode> {
    let ctx = Ctx::new(cli)?;
    let opened = ctx.open(cli)?;
    let problems = opened.db.verify()?;
    if cli.json {
        print_json(&serde_json::json!({"ok": problems.is_empty(), "problems": problems}));
    } else if problems.is_empty() {
        println!("ok: {} segment(s) verified, WAL clean", opened.db.stats().segments);
    } else {
        for p in &problems {
            println!("problem: {p}");
        }
    }
    Ok(if problems.is_empty() { ExitCode::SUCCESS } else { ExitCode::from(1) })
}

pub fn import(cli: &Cli) -> Result<ExitCode> {
    let ctx = Ctx::new(cli)?;
    let mut db = ingest::open_writer(&ctx.locator, true)?;
    let r = db.import_spool()?;
    let seg = db.flush()?;
    if cli.json {
        print_json(&serde_json::json!({"accepted": r.accepted, "duplicates": r.duplicates, "spool_files": r.spool_files, "undecodable": r.undecodable, "flushed": seg.map(|s| s.rows)}));
    } else {
        println!("imported {} event(s) from {} spool file(s); {} duplicate(s); {} undecodable", r.accepted, r.spool_files, r.duplicates, r.undecodable);
    }
    Ok(ExitCode::SUCCESS)
}

pub fn events(cli: &Cli, args: &EventsArgs) -> Result<ExitCode> {
    let ctx = Ctx::new(cli)?;
    let opened = ctx.open(cli)?;
    let mut filter = ctx.filter(&args.scope, &opened.db)?;
    if let Some(k) = &args.kind {
        for name in k.split(',') {
            let kind = EventKind::parse(name.trim()).with_context(|| format!("unknown event kind {name:?}"))?;
            filter.kinds.push(kind);
        }
    }
    filter.limit = Some(args.scope.limit.unwrap_or(50));
    let events = opened.db.scan(&filter)?;
    if cli.json {
        print_json(&events);
        return Ok(ExitCode::SUCCESS);
    }
    if events.is_empty() {
        println!("no events match");
        return Ok(ExitCode::SUCCESS);
    }
    for ev in &events {
        let tool = ev.tool.as_ref().map(|t| t.name.as_str()).unwrap_or("");
        let path = ev.paths.first().map(|p| p.display().to_string()).unwrap_or_default();
        let outcome = ev.outcome.as_ref().map(|o| format!("{}{}", o.status.as_str(), o.class.as_ref().map(|c| format!(":{c}")).unwrap_or_default())).unwrap_or_default();
        println!(
            "{} {:<11} {:<20} {:<14} {:<28} {:<18} {}",
            ts_local(ev.observed_at),
            truncate(ev.provider.as_str(), 11),
            truncate(ev.kind.as_str(), 20),
            truncate(tool, 14),
            truncate(&path, 28),
            truncate(&outcome, 18),
            ev.event_id.short()
        );
    }
    Ok(ExitCode::SUCCESS)
}

pub fn snapshot(cli: &Cli, args: &SnapshotArgs) -> Result<ExitCode> {
    match &args.cmd {
        SnapshotCmd::Export { out } => {
            let ctx = Ctx::new(cli)?;
            let mut db = ingest::open_writer(&ctx.locator, false)?;
            db.import_spool()?;
            db.flush()?;
            let (info, unflushed) = snapshot::export(&db, out)?;
            let bytes = std::fs::metadata(out).map(|m| m.len()).unwrap_or(0);
            if cli.json {
                print_json(&serde_json::json!({"file": out, "snapshot_id": info.snapshot_id, "entries": info.entries.len(), "bytes": bytes, "unflushed": unflushed}));
            } else {
                println!("exported {} ({}, {} entries, snapshot {})", out.display(), human_bytes(bytes), info.entries.len(), info.snapshot_id);
                println!("open it anywhere with: attempt --snapshot {} timeline", out.display());
            }
            Ok(ExitCode::SUCCESS)
        }
        SnapshotCmd::Inspect { file } | SnapshotCmd::Open { file } => {
            let info = snapshot::inspect(file)?;
            if cli.json {
                print_json(&serde_json::json!({"snapshot_id": info.snapshot_id, "schema_version": info.schema_version, "created_at": info.created_at.to_rfc3339(), "entries": info.entries.iter().map(|e| serde_json::json!({"name": e.name, "bytes": e.len})).collect::<Vec<_>>()}));
            } else {
                println!("snapshot {}  schema v{}  created {}", info.snapshot_id, info.schema_version, ts_local(info.created_at));
                for e in &info.entries {
                    println!("  {:<48} {}", e.name, human_bytes(e.len));
                }
                println!("all checksums verified");
                if matches!(args.cmd, SnapshotCmd::Open { .. }) {
                    println!("query it with: attempt --snapshot {} timeline", file.display());
                }
            }
            Ok(ExitCode::SUCCESS)
        }
    }
}

pub fn not_yet(cli: &Cli) -> Result<ExitCode> {
    let name = match cli.command {
        crate::cli::Command::Daemon => "daemon",
        crate::cli::Command::Ui => "ui",
        crate::cli::Command::Mcp => "mcp",
        crate::cli::Command::Update => "update",
        crate::cli::Command::Uninstall => "uninstall",
        _ => "command",
    };
    eprintln!("`attempt {name}` is not available in this build yet.");
    match name {
        "daemon" => eprintln!("hooks spool events durably without a daemon; every read command imports the spool first."),
        "uninstall" => eprintln!("remove hooks with `attempt hook uninstall`; the database directory can be deleted by hand (see `attempt status`)."),
        "ui" => eprintln!("use `attempt timeline`, `attempt why`, and `attempt query` from the terminal for now."),
        _ => {}
    }
    Ok(ExitCode::from(2))
}
