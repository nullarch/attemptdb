//! Database-level commands: init, status, verify, import, events, snapshot.

use crate::cli::{Cli, EventsArgs, InitArgs, SnapshotArgs, SnapshotCmd, UninstallArgs};
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
        ctx.config.capture_mode = mode
            .parse::<CaptureMode>()
            .map_err(|e| anyhow::anyhow!("{e}"))?;
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
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        Database::create(&db_dir, device.device_id)?;
        println!("created database at {}", db_dir.display());
    }
    if args.no_encryption {
        ctx.config.encryption = attemptdb_capture::config::EncryptionMode::Off;
        ctx.config.save(&ctx.locator.paths.config_dir)?;
        println!("encryption    off (content stays inline in segments)");
    } else if ctx.config.encryption != attemptdb_capture::config::EncryptionMode::Off {
        let db_id = attemptdb_storage::Identity::load(&db_dir)?.db_id;
        match attemptdb_capture::keys::init(
            &ctx.locator,
            db_id,
            &attemptdb_capture::keys::InitOptions::default(),
        ) {
            Ok(r) => println!(
                "encryption    on, {} key {} via {} — {}",
                if r.created { "new" } else { "existing" },
                r.key_id,
                r.source,
                r.reason
            ),
            Err(e) => println!(
                "encryption    not enabled: {e}\n              content will stay inline; run `attempt keys init --key-file` to enable"
            ),
        }
    }
    println!("capture mode  {}", ctx.config.capture_mode);
    println!("device id     {}", device.device_id.short());
    println!(
        "config        {}",
        Config::path(&ctx.locator.paths.config_dir).display()
    );
    println!();
    println!(
        "next: `attempt hook install` to wire your coding agents, then work normally and run `attempt timeline`"
    );
    Ok(ExitCode::SUCCESS)
}

fn ensure_gitignore(project: &std::path::Path) {
    let gi = project.join(".gitignore");
    let existing = std::fs::read_to_string(&gi).unwrap_or_default();
    if existing
        .lines()
        .any(|l| l.trim() == ".attemptdb/" || l.trim() == ".attemptdb")
    {
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
    // Counts come from the segments' columns; no event is decoded.
    let facts = opened.load()?.facts;
    let by_provider: std::collections::BTreeMap<String, (u64, Option<attemptdb_core::Timestamp>)> =
        facts
            .providers
            .values()
            .map(|p| (p.provider.clone(), (p.events, p.last_event_at)))
            .collect();
    let mut projects: std::collections::BTreeMap<String, u64> = Default::default();
    for p in facts.projects.values() {
        *projects.entry(p.name.clone()).or_default() += p.events;
    }
    let event_count = facts.events as usize;
    let session_count = facts.session_count();
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
            "events": event_count,
            "sessions": session_count,
            "providers": by_provider.iter().map(|(k, v)| serde_json::json!({"provider": k, "events": v.0, "last_event_at": v.1.map(|t| t.to_rfc3339())})).collect::<Vec<_>>(),
            "projects": projects,
            "import": opened.import,
            "warnings": opened.db.warnings,
        }));
        return Ok(ExitCode::SUCCESS);
    }
    println!(
        "database      {}{}",
        opened.source,
        if opened.read_only {
            "  (read-only)"
        } else {
            ""
        }
    );
    println!("capture mode  {}", ctx.config.capture_mode);
    println!(
        "events        {} ({} in {} segment(s), {} in WAL) · {} session(s)",
        event_count, stats.segment_rows, stats.segments, stats.memtable_rows, session_count
    );
    println!(
        "on disk       {} segments · {} WAL · generation {}",
        human_bytes(stats.segment_bytes),
        human_bytes(stats.wal_bytes),
        stats.generation
    );
    if let Some(r) = opened
        .import
        .as_ref()
        .filter(|r| r.spool_files > 0 || r.accepted > 0)
    {
        println!(
            "imported      {} new event(s) from {} spool file(s){}",
            r.accepted,
            r.spool_files,
            if r.duplicates > 0 {
                format!(", {} duplicate(s) skipped", r.duplicates)
            } else {
                String::new()
            }
        );
    }
    if stats.spool_pending {
        println!("spool         pending files could not be imported (read-only)");
    }
    if !by_provider.is_empty() {
        println!();
        for (p, (n, last)) in &by_provider {
            println!(
                "{:<13} {:>7} events   last {}",
                p,
                n,
                last.map(ts_local)
                    .unwrap_or_else(|| "capture test only".into())
            );
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
        println!(
            "ok: {} segment(s) verified, WAL clean",
            opened.db.stats().segments
        );
    } else {
        for p in &problems {
            println!("problem: {p}");
        }
    }
    Ok(if problems.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}

pub fn import(cli: &Cli) -> Result<ExitCode> {
    let ctx = Ctx::new(cli)?;
    let mut db = ingest::open_writer(&ctx.locator, true)?;
    let r = db.import_spool()?;
    let seg = db.flush()?;
    if cli.json {
        print_json(
            &serde_json::json!({"accepted": r.accepted, "duplicates": r.duplicates, "spool_files": r.spool_files, "undecodable": r.undecodable, "flushed": seg.map(|s| s.rows)}),
        );
    } else {
        println!(
            "imported {} event(s) from {} spool file(s); {} duplicate(s); {} undecodable",
            r.accepted, r.spool_files, r.duplicates, r.undecodable
        );
    }
    Ok(ExitCode::SUCCESS)
}

pub fn events(cli: &Cli, args: &EventsArgs) -> Result<ExitCode> {
    let ctx = Ctx::new(cli)?;
    let opened = ctx.open(cli)?;
    let loaded = opened.load()?;
    let mut filter = ctx.filter(&args.scope, &loaded.facts)?;
    if let Some(k) = &args.kind {
        for name in k.split(',') {
            let kind = EventKind::parse(name.trim())
                .with_context(|| format!("unknown event kind {name:?}"))?;
            filter.kinds.push(kind);
        }
    }
    filter.limit = Some(args.scope.limit.unwrap_or(50));
    let events = loaded.refreshed.scan(&filter);
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
        let path = ev
            .paths
            .first()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        let outcome = ev
            .outcome
            .as_ref()
            .map(|o| {
                format!(
                    "{}{}",
                    o.status.as_str(),
                    o.class
                        .as_ref()
                        .map(|c| format!(":{c}"))
                        .unwrap_or_default()
                )
            })
            .unwrap_or_default();
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
        SnapshotCmd::Export {
            out,
            sanitized,
            drop_remote,
            anonymize_sessions,
            include_blobs,
            key_out,
            scope,
        } => {
            let ctx = Ctx::new(cli)?;
            let mut db = ingest::open_writer(&ctx.locator, false)?;
            db.import_spool()?;
            db.flush()?;
            let scoped = scope.project.is_some()
                || scope.session.is_some()
                || scope.since.is_some()
                || scope.until.is_some()
                || !scope.all_projects;
            let export_key = if let Some(kf) = key_out {
                snapshot::ExportKey::Portable(kf.clone())
            } else if *include_blobs {
                snapshot::ExportKey::Same
            } else {
                snapshot::ExportKey::None
            };
            let (info, exported, unflushed) = if *sanitized || scoped {
                // Retractions are inference-layer instructions; an export
                // meant for other eyes honours them (facts stay on disk).
                let all = db.scan(&ScanFilter::default())?;
                let mut filter = ctx.filter(
                    scope,
                    &attemptdb_query::StreamFacts::from_events(all.iter()),
                )?;
                let retracted = attemptdb_project::retracted_ids(&all);
                filter.exclude_sessions = retracted.sessions.to_vec();
                filter.exclude_events = retracted.events.to_vec();
                let policy = sanitized.then(|| snapshot::SanitizePolicy {
                    drop_remote: *drop_remote,
                    hash_session_ids: *anonymize_sessions,
                    ..Default::default()
                });
                let (info, n) = snapshot::export_filtered_with(
                    &db,
                    out,
                    &filter,
                    policy.as_ref(),
                    &export_key,
                )?;
                (info, Some(n), 0)
            } else {
                let (info, unflushed) = snapshot::export_with(&db, out, &export_key)?;
                (info, None, unflushed)
            };
            if db.encryption_active()
                && matches!(export_key, snapshot::ExportKey::None)
                && !*sanitized
            {
                println!(
                    "note: content blobs are encrypted and were not included; add --include-blobs (same device) or --key-file FILE (portable)"
                );
            }
            let bytes = std::fs::metadata(out).map(|m| m.len()).unwrap_or(0);
            if cli.json {
                print_json(
                    &serde_json::json!({"file": out, "snapshot_id": info.snapshot_id, "entries": info.entries.len(), "bytes": bytes, "events": exported, "sanitized": sanitized, "unflushed": unflushed}),
                );
            } else {
                match exported {
                    Some(n) => println!(
                        "exported {} event(s) to {} ({}, {}snapshot {})",
                        n,
                        out.display(),
                        human_bytes(bytes),
                        if *sanitized { "sanitized, " } else { "" },
                        info.snapshot_id
                    ),
                    None => println!(
                        "exported {} ({}, {} entries, snapshot {})",
                        out.display(),
                        human_bytes(bytes),
                        info.entries.len(),
                        info.snapshot_id
                    ),
                }
                if *sanitized {
                    println!(
                        "review before publishing: attempt --snapshot {} events --all-projects -n 20",
                        out.display()
                    );
                }
                println!(
                    "open it anywhere with: attempt --snapshot {} timeline --all-projects",
                    out.display()
                );
            }
            Ok(ExitCode::SUCCESS)
        }
        SnapshotCmd::Audit { file } => audit_snapshot(cli, file),
        SnapshotCmd::Restore(args) => crate::cmd_repair::restore(cli, args),
        SnapshotCmd::Inspect { file } | SnapshotCmd::Open { file } => {
            let info = snapshot::inspect(file)?;
            if cli.json {
                print_json(
                    &serde_json::json!({"snapshot_id": info.snapshot_id, "schema_version": info.schema_version, "created_at": info.created_at.to_rfc3339(), "entries": info.entries.iter().map(|e| serde_json::json!({"name": e.name, "bytes": e.len})).collect::<Vec<_>>()}),
                );
            } else {
                println!(
                    "snapshot {}  schema v{}  created {}",
                    info.snapshot_id,
                    info.schema_version,
                    ts_local(info.created_at)
                );
                for e in &info.entries {
                    println!("  {:<48} {}", e.name, human_bytes(e.len));
                }
                println!("all checksums verified");
                if matches!(args.cmd, SnapshotCmd::Open { .. }) {
                    println!(
                        "query it with: attempt --snapshot {} timeline",
                        file.display()
                    );
                }
            }
            Ok(ExitCode::SUCCESS)
        }
    }
}

/// Privacy review of a snapshot: decode every event and look for things
/// that must not be published. Exit code 1 when anything is found.
fn audit_snapshot(cli: &Cli, file: &std::path::Path) -> Result<ExitCode> {
    let ctx = Ctx::new(cli)?;
    let (db, _) = snapshot::open_read_only(file, &ctx.locator.snapshot_cache_dir())?;
    let events = db.scan(&ScanFilter::default())?;
    let mut findings: std::collections::BTreeMap<&'static str, usize> = Default::default();
    let mut examples: std::collections::BTreeMap<&'static str, String> = Default::default();
    let mut note = |key: &'static str, example: String| {
        *findings.entry(key).or_default() += 1;
        examples.entry(key).or_insert(example);
    };
    let secret_markers = [
        "sk-",
        "ghp_",
        "gho_",
        "AKIA",
        "-----BEGIN",
        "xoxb-",
        "xoxp-",
        "Bearer ",
        "api_key",
        "apikey",
        "password",
        "secret",
    ];
    for ev in &events {
        if ev.content.as_ref().is_some_and(|c| !c.is_empty()) {
            note("content present", ev.event_id.short());
        }
        if ev.raw.is_some() {
            note("raw payload present", ev.event_id.short());
        }
        if !ev.unknown.is_empty() {
            note("unknown fields present", ev.event_id.short());
        }
        for p in &ev.paths {
            let l = &p.logical;
            if l.starts_with("/Users/")
                || l.starts_with("/home/")
                || l.starts_with("C:/Users/")
                || l.contains("/Users/")
            {
                note("home-directory path", l.clone());
            }
        }
        if ev.project.root.starts_with("/Users/")
            || ev.project.root.starts_with("/home/")
            || ev.project.root.starts_with("C:/Users/")
        {
            note("home-directory project root", ev.project.root.clone());
        }
        let attrs = serde_json::to_string(&ev.attrs).unwrap_or_default();
        for key in ["cwd", "previous_cwd", "worktree_path"] {
            if let Some(v) = ev.attrs.get(key).and_then(|v| v.as_str())
                && (v.starts_with("/Users/")
                    || v.starts_with("/home/")
                    || v.starts_with("C:/Users/"))
            {
                note("home-directory attr", format!("{key}={v}"));
            }
        }
        if attrs.contains('@')
            && attrs.split('@').nth(1).is_some_and(|rest| {
                rest.chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_alphanumeric())
            })
        {
            note(
                "possible email in attrs",
                crate::render::truncate(&attrs, 80),
            );
        }
        let hay = format!(
            "{} {} {}",
            attrs,
            ev.provider_session_id,
            ev.project.repo_remote.clone().unwrap_or_default()
        );
        for m in secret_markers {
            if hay.contains(m) {
                note(
                    "possible secret marker",
                    format!("{m} in {}", ev.event_id.short()),
                );
            }
        }
        if ev
            .project
            .repo_remote
            .as_deref()
            .is_some_and(|r| r.contains('@') || r.contains("://"))
        {
            note(
                "remote with credentials or scheme",
                ev.project.repo_remote.clone().unwrap_or_default(),
            );
        }
    }
    if cli.json {
        print_json(
            &serde_json::json!({"events": events.len(), "findings": findings, "examples": examples}),
        );
    } else {
        println!("audited {} event(s) in {}", events.len(), file.display());
        if findings.is_empty() {
            println!(
                "no findings: no content, no raw payloads, no unknown fields, no home-directory paths, no secret markers"
            );
        } else {
            for (k, n) in &findings {
                println!(
                    "  {:<32} {:>6}   e.g. {}",
                    k,
                    n,
                    crate::render::truncate(&examples[k], 70)
                );
            }
            println!(
                "re-export with `attempt snapshot export --sanitized` (and --drop-remote / --anonymize-sessions as needed)"
            );
        }
    }
    Ok(if findings.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}

/// `attempt uninstall`: remove our hook entries from every detected agent
/// (user scope) and optionally purge local data. The binary itself is left
/// in place (package managers own it).
pub fn uninstall(cli: &Cli, args: &UninstallArgs) -> Result<ExitCode> {
    use attemptdb_capture::install::{
        InstallOptions, Outcome, Scope, uninstall as uninstall_hooks,
    };
    let ctx = Ctx::new(cli)?;
    let report = uninstall_hooks(&InstallOptions {
        scope: Scope::User,
        providers: None,
        binary_path: None,
        dry_run: args.dry_run,
        remove_legacy: false,
    })?;
    for a in &report.actions {
        let label = match &a.outcome {
            Outcome::Removed if args.dry_run => "would remove",
            Outcome::Removed => "hooks removed",
            Outcome::AlreadyCurrent => "no hooks present",
            Outcome::Skipped(_) => "skipped",
            Outcome::Failed(_) => "FAILED",
            _ => "changed",
        };
        println!(
            "{:<12} {:<16} {}",
            a.agent.display_name(),
            label,
            a.config_path.display()
        );
        if let Outcome::Failed(e) | Outcome::Skipped(e) = &a.outcome {
            println!("{:<12} {e}", "");
        }
    }
    if !args.purge_data {
        println!();
        println!(
            "local data kept at {} (add --purge-data to delete it)",
            ctx.locator.paths.data_dir.display()
        );
        return Ok(ExitCode::SUCCESS);
    }
    let mut targets: Vec<std::path::PathBuf> = vec![ctx.locator.db_dir.clone()];
    for p in [
        &ctx.locator.paths.data_dir,
        &ctx.locator.paths.config_dir,
        &ctx.locator.paths.cache_dir,
        &ctx.locator.paths.log_dir,
        &ctx.locator.paths.runtime_dir,
    ] {
        if !targets.iter().any(|t| p.starts_with(t)) {
            targets.push(p.clone());
        }
    }
    targets.retain(|t| t.exists());
    if targets.is_empty() {
        println!("nothing to purge");
        return Ok(ExitCode::SUCCESS);
    }
    println!();
    println!("purge would delete:");
    for t in &targets {
        println!("  {}", t.display());
    }
    if args.dry_run {
        println!("(dry run — nothing was deleted)");
        return Ok(ExitCode::SUCCESS);
    }
    if !args.yes {
        use std::io::{IsTerminal, Write};
        if !std::io::stdin().is_terminal() {
            anyhow::bail!(
                "refusing to purge without confirmation; pass --yes to confirm non-interactively"
            );
        }
        print!("type 'delete' to confirm: ");
        std::io::stdout().flush().ok();
        let mut line = String::new();
        std::io::stdin().read_line(&mut line).ok();
        if line.trim() != "delete" {
            println!("aborted; nothing was deleted");
            return Ok(ExitCode::from(1));
        }
    }
    for t in &targets {
        std::fs::remove_dir_all(t).with_context(|| format!("deleting {}", t.display()))?;
        println!("deleted {}", t.display());
    }
    Ok(ExitCode::SUCCESS)
}
