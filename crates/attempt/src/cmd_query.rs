//! Timeline and query commands (DataFusion + AttemptQL via attemptdb-query).

use crate::cli::{Cli, QueryArgs, ScopeArgs, TimelineArgs, TraceArgs, WhyArgs};
use crate::ctx::Ctx;
use crate::render::{duration, print_json, truncate, ts_local, ts_time};
use anyhow::Result;
use attemptdb_project::{AttemptOutcome, Projection, TurnStatus};
use attemptdb_query::{QueryEngine, QueryResult, ResultKind};
use std::process::ExitCode;

fn runtime() -> Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?)
}

fn engine(cli: &Cli, scope: &ScopeArgs) -> Result<(Ctx, QueryEngine)> {
    let ctx = Ctx::new(cli)?;
    let opened = ctx.open(cli)?;
    let mut loaded = opened.load()?;
    let filter = ctx.filter(scope, &loaded.facts)?;
    let engine = loaded.engine(&filter)?;
    Ok((ctx, engine))
}

fn emit(cli: &Cli, result: &QueryResult, csv: bool) {
    if cli.json {
        print_json(&result.to_json());
        return;
    }
    if csv {
        print!("{}", result.render_csv());
    } else if result.row_count() == 0 && matches!(result.kind, ResultKind::Empty) {
        println!("(no rows)");
    } else if matches!(result.kind, ResultKind::Explanation) {
        print_records(result);
    } else {
        println!("{}", result.render_table(term_width()));
    }
    for n in &result.notes {
        println!("note: {n}");
    }
}

/// Vertical key/value layout for explanations: wide prose columns read
/// badly in a grid.
fn print_records(result: &QueryResult) {
    let rows = result.to_json();
    let Some(rows) = rows.as_array() else {
        println!("{}", result.render_table(term_width()));
        return;
    };
    for (i, row) in rows.iter().enumerate() {
        if i > 0 {
            println!();
        }
        let Some(obj) = row.as_object() else { continue };
        for (k, v) in obj {
            let text = match v {
                serde_json::Value::String(s) => s.clone(),
                serde_json::Value::Array(a) => a
                    .iter()
                    .map(|x| {
                        x.as_str()
                            .map(str::to_string)
                            .unwrap_or_else(|| x.to_string())
                    })
                    .collect::<Vec<_>>()
                    .join(", "),
                serde_json::Value::Null => "—".into(),
                other => other.to_string(),
            };
            println!("{:<14} {}", k, crate::render::sanitize(&text));
        }
    }
    println!(
        "({} row{})",
        rows.len(),
        if rows.len() == 1 { "" } else { "s" }
    );
}

fn term_width() -> Option<usize> {
    if let Some(w) = std::env::var("COLUMNS").ok().and_then(|c| c.parse().ok()) {
        return Some(w);
    }
    terminal_size::terminal_size().map(|(w, _)| w.0 as usize)
}

pub fn query(cli: &Cli, args: &QueryArgs) -> Result<ExitCode> {
    let statement = args.statement.join(" ");
    if statement.trim().is_empty() {
        anyhow::bail!(
            "give a statement, e.g. `attempt query \"SHOW FAILED ATTEMPTS\"` or `attempt query \"SELECT kind, count(*) FROM events GROUP BY 1\"`"
        );
    }
    let (_ctx, engine) = engine(cli, &args.scope)?;
    let rt = runtime()?;
    let result = if args.explain {
        rt.block_on(engine.explain(&statement))
    } else {
        rt.block_on(engine.query(&statement))
    };
    match result {
        Ok(r) => {
            emit(cli, &r, args.csv);
            Ok(ExitCode::SUCCESS)
        }
        Err(attemptdb_query::QueryError::Parse { .. }) if !cli.json => {
            let e = result.unwrap_err();
            eprintln!("{}", attemptdb_query::format_parse_error(&statement, &e));
            Ok(ExitCode::from(1))
        }
        Err(e) => Err(e.into()),
    }
}

pub fn why(cli: &Cli, args: &WhyArgs) -> Result<ExitCode> {
    let (_ctx, engine) = engine(cli, &args.scope)?;
    let subject = args.subject.clone().unwrap_or_else(|| "project".into());
    let statement = if subject.starts_with("att_") {
        format!("WHY {subject} FAILED")
    } else if subject.starts_with("ses_") {
        format!("WHY session '{subject}' STATUS BLOCKED")
    } else {
        format!("WHY {subject} STATUS BLOCKED")
    };
    let r = runtime()?.block_on(engine.query(&statement))?;
    emit(cli, &r, false);
    Ok(ExitCode::SUCCESS)
}

pub fn trace(cli: &Cli, args: &TraceArgs) -> Result<ExitCode> {
    let (_ctx, engine) = engine(cli, &args.scope)?;
    let r = runtime()?.block_on(engine.query(&format!("TRACE {} CAUSES", args.id)))?;
    emit(cli, &r, false);
    Ok(ExitCode::SUCCESS)
}

pub fn failures(cli: &Cli, scope: &ScopeArgs) -> Result<ExitCode> {
    let (_ctx, engine) = engine(cli, scope)?;
    let limit = scope.limit.unwrap_or(50);
    // A compact column set for the terminal; `attempt query "SHOW FAILED ATTEMPTS"` has every column.
    let sql = format!(
        "SELECT attempt_id, provider, project_name, started_at, outcome, failure_class, approach, \
         superseded_by, confidence FROM attempts WHERE outcome IN ('failed', 'superseded') \
         ORDER BY started_at DESC LIMIT {limit}"
    );
    let mut r = runtime()?.block_on(engine.sql(&sql))?;
    if r.row_count() == 0 {
        r.kind = ResultKind::Empty;
        r.notes.push(format!(
            "no failed or superseded attempts among {} attempt(s) (tier1-v0)",
            engine.projection().attempts.len()
        ));
    } else {
        r.notes.push("attempts are Tier 1 inferences; run `attempt trace <att_id>` or `attempt why <att_id>` for the evidence".into());
    }
    emit(cli, &r, false);
    Ok(ExitCode::SUCCESS)
}

pub fn handoffs(cli: &Cli, scope: &ScopeArgs) -> Result<ExitCode> {
    let (_ctx, engine) = engine(cli, scope)?;
    let limit = scope.limit.unwrap_or(50);
    let sql = format!(
        "SELECT handoff_at, from_provider, to_provider, gap_ms, shared_paths, from_session, to_session, confidence \
         FROM handoffs ORDER BY handoff_at DESC LIMIT {limit}"
    );
    let mut r = runtime()?.block_on(engine.sql(&sql))?;
    if r.row_count() == 0 {
        r.kind = ResultKind::Empty;
        r.notes.push("no handoffs detected: a handoff needs two sessions from different agents in the same project within 30 minutes (tier1-v0)".into());
    }
    emit(cli, &r, false);
    Ok(ExitCode::SUCCESS)
}

pub fn tables(cli: &Cli) -> Result<ExitCode> {
    let (_ctx, engine) = engine(
        cli,
        &ScopeArgs {
            all_projects: true,
            ..Default::default()
        },
    )?;
    let tables = engine.tables()?;
    if cli.json {
        print_json(
            &tables
                .iter()
                .map(|t| serde_json::json!({"name": t.name, "rows": t.rows, "columns": t.columns}))
                .collect::<Vec<_>>(),
        );
        return Ok(ExitCode::SUCCESS);
    }
    for t in &tables {
        println!("{} ({} rows)", t.name, t.rows);
        for (c, ty) in &t.columns {
            println!("  {:<24} {}", c, ty);
        }
    }
    Ok(ExitCode::SUCCESS)
}

pub fn timeline(cli: &Cli, args: &TimelineArgs) -> Result<ExitCode> {
    let (_ctx, engine) = engine(cli, &args.scope)?;
    let p = engine.projection();
    if cli.json {
        print_json(p);
        return Ok(ExitCode::SUCCESS);
    }
    render_timeline(
        p,
        engine.event_count(),
        args.scope.limit.unwrap_or(10),
        args.tools,
        args.all,
    );
    Ok(ExitCode::SUCCESS)
}

fn render_timeline(
    p: &Projection,
    event_count: usize,
    session_limit: usize,
    show_tools: bool,
    show_all: bool,
) {
    let mut sessions: Vec<_> = p
        .sessions
        .iter()
        .filter(|s| show_all || s.prompt_count > 0 || s.tool_call_count > 0)
        .collect();
    if sessions.is_empty() {
        if p.sessions.is_empty() {
            println!(
                "no sessions yet ({event_count} events). Work with a coding agent whose hooks are installed, then come back."
            );
        } else {
            println!(
                "{} session(s) carry no prompts or tool calls (capture tests, stray events); use --all to list them.",
                p.sessions.len()
            );
        }
        println!("check wiring with `attempt doctor`.");
        return;
    }
    sessions.sort_by_key(|a| std::cmp::Reverse(a.started_at));
    let shown = sessions.iter().take(session_limit);
    println!(
        "{} session(s), {} turn(s), {} attempt(s), {} handoff(s) from {} events  [inference {}]",
        p.sessions.len(),
        p.turns.len(),
        p.attempts.len(),
        p.handoffs.len(),
        event_count,
        p.algorithm_version.0
    );
    for s in shown {
        println!();
        let end = s
            .ended_at
            .map(|t| format!("→ {}", ts_time(t)))
            .unwrap_or_else(|| "→ open".into());
        println!(
            "▌ {}  {}  {} {}  {:?} coverage  {} turns · {} tool calls · {} failures  {}",
            s.provider.display_name(),
            truncate(&s.project_name, 40),
            ts_local(s.started_at),
            end,
            s.coverage,
            s.turn_count,
            s.tool_call_count,
            s.failure_count,
            s.session_id.short()
        );
        let mut turns: Vec<_> = p
            .turns
            .iter()
            .filter(|t| t.session_id == s.session_id)
            .collect();
        turns.sort_by_key(|t| t.index);
        for t in turns {
            let status = match t.status {
                TurnStatus::Completed => "completed",
                TurnStatus::Failed => "failed",
                TurnStatus::InProgress => "in progress",
                TurnStatus::Unknown => "no stop seen",
            };
            let objective = t
                .objective
                .as_deref()
                .map(|o| truncate(o, 70))
                .or_else(|| {
                    t.prompt_chars
                        .map(|c| format!("(prompt, {c} chars, content not captured)"))
                })
                .unwrap_or_else(|| {
                    if t.index == 0 {
                        "(activity before the first prompt)".into()
                    } else {
                        "(prompt)".into()
                    }
                });
            println!(
                "  {} turn {:<3} {:<12} {}",
                ts_time(t.started_at),
                t.index,
                status,
                objective
            );
            let mut attempts: Vec<_> = p
                .attempts
                .iter()
                .filter(|a| a.turn_id == t.turn_id)
                .collect();
            attempts.sort_by_key(|a| a.index);
            for a in attempts {
                let outcome = match a.outcome {
                    AttemptOutcome::Succeeded => "✓ succeeded",
                    AttemptOutcome::Failed => "✗ failed",
                    AttemptOutcome::Superseded => "↻ superseded",
                    AttemptOutcome::Abandoned => "… abandoned",
                    AttemptOutcome::InProgress => "▶ in progress",
                    AttemptOutcome::Unknown => "? unknown",
                };
                let dur = a
                    .ended_at
                    .map(|e| duration((e.as_millis() - a.started_at.as_millis()).max(0) as u64))
                    .unwrap_or_default();
                let class = a
                    .failure_class
                    .as_deref()
                    .map(|c| format!(" [{c}]"))
                    .unwrap_or_default();
                let sup = a
                    .superseded_by
                    .map(|id| format!(" → {}", id.short()))
                    .unwrap_or_default();
                println!(
                    "    {} {:<14}{} {}  {}  {:>5}  conf {:.1}{}",
                    a.attempt_id.short(),
                    outcome,
                    class,
                    truncate(&a.approach, 60),
                    if a.paths.is_empty() {
                        String::new()
                    } else {
                        format!(
                            "({} path{})",
                            a.paths.len(),
                            if a.paths.len() == 1 { "" } else { "s" }
                        )
                    },
                    dur,
                    a.confidence,
                    sup
                );
                if !a.commit_shas.is_empty() {
                    let shas: Vec<String> = a
                        .commit_shas
                        .iter()
                        .map(|s| s.chars().take(7).collect())
                        .collect();
                    println!("             ⎇ committed {}", shas.join(", "));
                }
                if show_tools {
                    for id in &a.tool_call_ids {
                        if let Some(tc) = p.tool_calls.iter().find(|c| &c.tool_call_id == id) {
                            let o = tc
                                .outcome
                                .as_ref()
                                .map(|o| o.status.as_str())
                                .unwrap_or("in flight");
                            let path = tc
                                .paths
                                .first()
                                .map(|p| p.display().to_string())
                                .unwrap_or_default();
                            println!(
                                "        {:<16} {:<10} {:>7}  {}",
                                truncate(&tc.tool.name, 16),
                                o,
                                tc.duration_ms.map(duration).unwrap_or_default(),
                                truncate(&path, 50)
                            );
                        }
                    }
                }
            }
        }
    }
    if sessions.len() > session_limit {
        println!();
        println!(
            "({} more session(s); use --limit N or --session ID)",
            sessions.len() - session_limit
        );
    }
    if !p.handoffs.is_empty() {
        println!();
        for h in &p.handoffs {
            println!(
                "⇄ handoff {} → {} at {} after {} gap, {} shared path(s), conf {:.1}",
                h.from_provider.display_name(),
                h.to_provider.display_name(),
                ts_local(h.at),
                duration(h.gap_ms),
                h.shared_paths.len(),
                h.confidence
            );
        }
    }
}
