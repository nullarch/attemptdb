//! Timeline and query commands (DataFusion + AttemptQL via attemptdb-query).

use crate::cli::{Cli, QueryArgs, ScopeArgs, TimelineArgs, TraceArgs, WhyArgs};
use crate::ctx::Ctx;
use crate::read_service;
use crate::render::{duration, print_json, truncate, ts_local, ts_time};
use anyhow::Result;
use attemptdb_capture::ipc::{self, ReadScope};
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
    let engine = local_engine(cli, &ctx, scope)?;
    Ok((ctx, engine))
}

fn local_engine(cli: &Cli, ctx: &Ctx, scope: &ScopeArgs) -> Result<QueryEngine> {
    let opened = ctx.open(cli)?;
    let mut loaded = opened.load()?;
    let filter = ctx.filter(scope, &loaded.facts)?;
    loaded.engine(&filter)
}

/// Where a read command gets its answers: the daemon's resident engine
/// when one serves this database, else an engine built in this process.
/// The daemon is asked first because it already holds the projection;
/// anything it cannot answer (not running, no read service, a result over
/// the frame limit) falls back to the local engine, built once.
struct Reader<'a> {
    cli: &'a Cli,
    ctx: Ctx,
    scope: &'a ScopeArgs,
    daemon: Option<ReadScope>,
    local: std::cell::OnceCell<QueryEngine>,
}

impl<'a> Reader<'a> {
    fn open(cli: &'a Cli, scope: &'a ScopeArgs) -> Result<Self> {
        let ctx = Ctx::new(cli)?;
        let daemon = if read_service::daemon_allowed(cli) && daemon_serves(&ctx.locator) {
            Some(read_service::read_scope(scope, &ctx.cwd)?)
        } else {
            None
        };
        Ok(Self {
            cli,
            ctx,
            scope,
            daemon,
            local: std::cell::OnceCell::new(),
        })
    }

    fn local(&self) -> Result<&QueryEngine> {
        if let Some(e) = self.local.get() {
            return Ok(e);
        }
        let e = local_engine(self.cli, &self.ctx, self.scope)?;
        Ok(self.local.get_or_init(|| e))
    }

    /// Run SQL or AttemptQL.
    fn query(&self, statement: &str) -> Result<QueryResult> {
        if let Some(scope) = &self.daemon
            && let Some(r) =
                read_service::query_via_daemon(&self.ctx.locator, scope.clone(), statement)
        {
            return Ok(r);
        }
        Ok(runtime()?.block_on(self.local()?.query(statement))?)
    }

    /// One number from a `SELECT count(*) AS n …`.
    fn count(&self, sql: &str) -> Result<usize> {
        let r = self.query(sql)?;
        Ok(r.to_json()[0]["n"].as_u64().unwrap_or(0) as usize)
    }
}

/// Whether a daemon answers for this database (one ping).
fn daemon_serves(locator: &attemptdb_capture::Locator) -> bool {
    match ipc::Client::status(locator) {
        Ok(s) => {
            s.db_dir == locator.db_dir
                || attemptdb_capture::platform::canonical_display_path(&s.db_dir)
                    == attemptdb_capture::platform::canonical_display_path(&locator.db_dir)
        }
        Err(_) => false,
    }
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
    let reader = Reader::open(cli, &args.scope)?;
    let result = if args.explain {
        runtime()?
            .block_on(reader.local()?.explain(&statement))
            .map_err(anyhow::Error::from)
    } else {
        reader.query(&statement)
    };
    match result {
        Ok(r) => {
            emit(cli, &r, args.csv);
            Ok(ExitCode::SUCCESS)
        }
        Err(e) => match e.downcast_ref::<attemptdb_query::QueryError>() {
            Some(pe @ attemptdb_query::QueryError::Parse { .. }) if !cli.json => {
                eprintln!("{}", attemptdb_query::format_parse_error(&statement, pe));
                Ok(ExitCode::from(1))
            }
            _ => Err(e),
        },
    }
}

pub fn why(cli: &Cli, args: &WhyArgs) -> Result<ExitCode> {
    let reader = Reader::open(cli, &args.scope)?;
    let subject = args.subject.clone().unwrap_or_else(|| "project".into());
    let statement = if subject.starts_with("att_") {
        format!("WHY {subject} FAILED")
    } else if subject.starts_with("ses_") {
        format!("WHY session '{subject}' STATUS BLOCKED")
    } else {
        format!("WHY {subject} STATUS BLOCKED")
    };
    let r = reader.query(&statement)?;
    emit(cli, &r, false);
    Ok(ExitCode::SUCCESS)
}

pub fn trace(cli: &Cli, args: &TraceArgs) -> Result<ExitCode> {
    let reader = Reader::open(cli, &args.scope)?;
    let r = reader.query(&format!("TRACE {} CAUSES", args.id))?;
    emit(cli, &r, false);
    Ok(ExitCode::SUCCESS)
}

pub fn failures(cli: &Cli, scope: &ScopeArgs) -> Result<ExitCode> {
    let reader = Reader::open(cli, scope)?;
    let limit = scope.limit.unwrap_or(50);
    // A compact column set for the terminal; `attempt query "SHOW FAILED ATTEMPTS"` has every column.
    let sql = format!(
        "SELECT attempt_id, provider, project_name, started_at, outcome, failure_class, approach, \
         superseded_by, confidence FROM attempts WHERE outcome IN ('failed', 'superseded') \
         ORDER BY started_at DESC LIMIT {limit}"
    );
    let mut r = reader.query(&sql)?;
    if r.row_count() == 0 {
        r.kind = ResultKind::Empty;
        r.notes.push(format!(
            "no failed or superseded attempts among {} attempt(s) ({})",
            reader.count("SELECT count(*) AS n FROM attempts")?,
            attemptdb_project::ALGORITHM_VERSION
        ));
    } else {
        r.notes.push("attempts are Tier 1 inferences; run `attempt trace <att_id>` or `attempt why <att_id>` for the evidence".into());
    }
    emit(cli, &r, false);
    Ok(ExitCode::SUCCESS)
}

pub fn handoffs(cli: &Cli, scope: &ScopeArgs) -> Result<ExitCode> {
    let reader = Reader::open(cli, scope)?;
    let limit = scope.limit.unwrap_or(50);
    let sql = format!(
        "SELECT handoff_at, from_provider, to_provider, gap_ms, shared_paths, from_session, to_session, confidence \
         FROM handoffs ORDER BY handoff_at DESC LIMIT {limit}"
    );
    let mut r = reader.query(&sql)?;
    if r.row_count() == 0 {
        r.kind = ResultKind::Empty;
        r.notes.push(format!(
            "no handoffs detected: a handoff needs two sessions from different agents in the same project within 30 minutes ({})",
            attemptdb_project::ALGORITHM_VERSION
        ));
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
    let limit = args.scope.limit.unwrap_or(10);
    if !cli.json {
        // The daemon trims the projection to the sessions shown; the
        // header's totals come along separately.
        let reader = Reader::open(cli, &args.scope)?;
        if let Some(scope) = &reader.daemon
            && let Some((p, totals, event_count)) = read_service::timeline_via_daemon(
                &reader.ctx.locator,
                scope.clone(),
                Some(limit),
                args.all,
            )
        {
            render_timeline(
                &p,
                Totals {
                    sessions: totals.sessions,
                    turns: totals.turns,
                    attempts: totals.attempts,
                    handoffs: totals.handoffs,
                    listed: Some(totals.listed),
                    events: event_count,
                },
                limit,
                args.tools,
                args.all,
            );
            return Ok(ExitCode::SUCCESS);
        }
    }
    let (_ctx, engine) = engine(cli, &args.scope)?;
    let p = engine.projection();
    if cli.json {
        print_json(p);
        return Ok(ExitCode::SUCCESS);
    }
    render_timeline(
        p,
        Totals::of(p, engine.event_count()),
        limit,
        args.tools,
        args.all,
    );
    Ok(ExitCode::SUCCESS)
}

/// Counts of the whole projection for the timeline's header (a trimmed
/// projection no longer carries them).
struct Totals {
    sessions: usize,
    turns: usize,
    attempts: usize,
    handoffs: usize,
    /// Sessions eligible for listing before the limit, when the
    /// projection was trimmed (`None`: count them here).
    listed: Option<usize>,
    events: usize,
}

impl Totals {
    fn of(p: &Projection, events: usize) -> Self {
        Self {
            sessions: p.sessions.len(),
            turns: p.turns.len(),
            attempts: p.attempts.len(),
            handoffs: p.handoffs.len(),
            listed: None,
            events,
        }
    }
}

fn render_timeline(
    p: &Projection,
    totals: Totals,
    session_limit: usize,
    show_tools: bool,
    show_all: bool,
) {
    let event_count = totals.events;
    let mut sessions: Vec<_> = p
        .sessions
        .iter()
        .filter(|s| show_all || s.prompt_count > 0 || s.tool_call_count > 0)
        .collect();
    if sessions.is_empty() {
        if totals.sessions == 0 {
            println!(
                "no sessions yet ({event_count} events). Work with a coding agent whose hooks are installed, then come back."
            );
        } else {
            println!(
                "{} session(s) carry no prompts or tool calls (capture tests, stray events); use --all to list them.",
                totals.sessions
            );
        }
        println!("check wiring with `attempt doctor`.");
        return;
    }
    sessions.sort_by_key(|a| std::cmp::Reverse(a.started_at));
    let shown = sessions.iter().take(session_limit);
    println!(
        "{} session(s), {} turn(s), {} attempt(s), {} handoff(s) from {} events  [inference {}]",
        totals.sessions,
        totals.turns,
        totals.attempts,
        totals.handoffs,
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
    let listed = totals.listed.unwrap_or(sessions.len());
    if listed > session_limit {
        println!();
        println!(
            "({} more session(s); use --limit N or --session ID)",
            listed - session_limit
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
