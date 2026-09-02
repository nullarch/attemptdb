//! Query-side benchmarks over an existing database: the recent timeline,
//! the full historical scan through the query engine, time travel, and
//! causal traversal.

use super::{StepCtx, fresh_dir, open_reader, open_writer, runtime};
use crate::stats::{Summary, disk_usage};
use crate::workload::{CHAIN_SESSION_ID, GenConfig, Workload};
use anyhow::{Context, Result};
use attemptdb_core::{Event, Timestamp};
use attemptdb_project::project;
use attemptdb_query::{QueryEngine, QueryResult};
use attemptdb_storage::{Database, ScanFilter};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::time::Instant;

const QUERY_RUNS: usize = 10;
const TIMELINE_BUILDS: usize = 5;
const TIMELINE_QUERY_RUNS: usize = 20;
const STATE_POINTS: usize = 10;

fn db_root(ctx: &StepCtx) -> Result<PathBuf> {
    ctx.db
        .clone()
        .context("this step needs --db <database dir>")
}

/// `observed_at` bounds from the manifest (the databases the benchmark
/// reads were closed with a final flush, so the memtable is empty).
fn time_bounds(db: &Database) -> Option<(Timestamp, Timestamp)> {
    let segs = &db.manifest().segments;
    let min = segs.iter().map(|s| s.min_observed_at).min()?;
    let max = segs.iter().map(|s| s.max_observed_at).max()?;
    Some((min, max))
}

/// The `observed_at` after which roughly `events` rows lie behind, walking
/// the manifest's time-ordered segments.
fn cutoff_for(db: &Database, events: u64) -> Option<Timestamp> {
    let mut acc = 0u64;
    let mut segs: Vec<_> = db.manifest().segments.iter().collect();
    segs.sort_by_key(|s| s.min_source_seq);
    for s in segs {
        acc += s.rows;
        if acc >= events {
            return Some(s.max_observed_at);
        }
    }
    None
}

fn timed_queries(
    rt: &tokio::runtime::Runtime,
    engine: &QueryEngine,
    statements: &[&str],
    runs: usize,
) -> Result<Value> {
    let mut out = serde_json::Map::new();
    for stmt in statements {
        let mut us = Vec::with_capacity(runs);
        let mut rows = 0;
        let mut notes: Vec<String> = Vec::new();
        for _ in 0..runs {
            let t = Instant::now();
            let r: QueryResult = rt.block_on(engine.query(stmt))?;
            us.push(t.elapsed().as_secs_f64() * 1e6);
            rows = r.row_count();
            notes = r.notes.clone();
        }
        out.insert(
            (*stmt).to_string(),
            json!({
                "runs": runs,
                "rows": rows,
                "notes": notes,
                "latency": Summary::of_micros(&mut us),
            }),
        );
    }
    Ok(Value::Object(out))
}

fn projection_counts(engine: &QueryEngine) -> Value {
    let p = engine.projection();
    json!({
        "sessions": p.sessions.len(),
        "turns": p.turns.len(),
        "tool_calls": p.tool_calls.len(),
        "attempts": p.attempts.len(),
        "handoffs": p.handoffs.len(),
        "edges": p.edges.len(),
        "signals": p.signals.len(),
    })
}

/// The readable id of the deepest attempt of the chained-failure session.
fn chain_attempt(engine: &QueryEngine) -> Option<(String, usize)> {
    let p = engine.projection();
    let session = p
        .sessions
        .iter()
        .find(|s| s.provider_session_id == CHAIN_SESSION_ID)?;
    let attempts: Vec<_> = p.attempts_of(session.session_id).collect();
    let last = attempts.last()?;
    Some((format!("att_{}", last.attempt_id), attempts.len()))
}

pub fn recent_timeline(ctx: &StepCtx) -> Result<Value> {
    let root = db_root(ctx)?;
    let db = open_reader(&root)?;
    let (min, max) = time_bounds(&db).context("database has no segments")?;
    let since = Timestamp::from_micros(max.as_micros() - 24 * 3_600 * 1_000_000);
    let filter = ScanFilter {
        since: Some(since),
        ..Default::default()
    };
    let rt = runtime()?;
    let mut build_secs = Vec::with_capacity(TIMELINE_BUILDS);
    let mut engine = None;
    for _ in 0..TIMELINE_BUILDS {
        let t = Instant::now();
        let e = rt.block_on(QueryEngine::from_database(&db, &filter))?;
        build_secs.push(t.elapsed().as_secs_f64());
        engine = Some(e);
    }
    let engine = engine.expect("at least one build");
    let mut build_us: Vec<f64> = build_secs.iter().map(|s| s * 1e6).collect();
    let queries = timed_queries(
        &rt,
        &engine,
        &[
            "SHOW FAILED ATTEMPTS LIMIT 50",
            "SHOW ATTEMPTS LIMIT 50",
            "SHOW SESSIONS LIMIT 50",
            "SELECT count(*) FROM events",
        ],
        TIMELINE_QUERY_RUNS,
    )?;
    Ok(json!({
        "db": root.display().to_string(),
        "window_hours": 24,
        "synthetic_min_observed_at": min.to_rfc3339(),
        "synthetic_max_observed_at": max.to_rfc3339(),
        "window_since": since.to_rfc3339(),
        "events_in_window": engine.event_count(),
        "segments_total": db.manifest().segments.len(),
        "engine_build": Summary::of_micros(&mut build_us),
        "projection": projection_counts(&engine),
        "queries": queries,
    }))
}

pub fn scan_project(ctx: &StepCtx) -> Result<Value> {
    let root = db_root(ctx)?;
    let t = Instant::now();
    let db = open_reader(&root)?;
    let open_secs = t.elapsed().as_secs_f64();
    let t = Instant::now();
    let events: Vec<Event> = db.scan(&ScanFilter::default())?;
    let scan_secs = t.elapsed().as_secs_f64();
    let t = Instant::now();
    let p = project(&events);
    let project_secs = t.elapsed().as_secs_f64();
    let n = events.len();
    Ok(json!({
        "db": root.display().to_string(),
        "events": n,
        "disk": disk_usage(&root),
        "open_secs": open_secs,
        "scan_secs": scan_secs,
        "scan_rows_per_sec": n as f64 / scan_secs.max(1e-9),
        "project_secs": project_secs,
        "project_rows_per_sec": n as f64 / project_secs.max(1e-9),
        "projection": {
            "sessions": p.sessions.len(),
            "turns": p.turns.len(),
            "tool_calls": p.tool_calls.len(),
            "attempts": p.attempts.len(),
            "handoffs": p.handoffs.len(),
            "edges": p.edges.len(),
            "out_of_order_events": p.stats.out_of_order_events,
            "unpaired_tool_starts": p.stats.unpaired_tool_starts,
            "unpaired_tool_finishes": p.stats.unpaired_tool_finishes,
        },
    }))
}

pub fn engine(ctx: &StepCtx) -> Result<Value> {
    let root = db_root(ctx)?;
    let db = open_reader(&root)?;
    let total: u64 = db.manifest().segments.iter().map(|s| s.rows).sum();
    let filter = if ctx.events > 0 && ctx.events < total {
        ScanFilter {
            until: cutoff_for(&db, ctx.events),
            ..Default::default()
        }
    } else {
        ScanFilter::default()
    };
    let rt = runtime()?;
    let t = Instant::now();
    let engine = rt.block_on(QueryEngine::from_database(&db, &filter))?;
    let build_secs = t.elapsed().as_secs_f64();
    let n = engine.event_count();

    let sql = timed_queries(
        &rt,
        &engine,
        &[
            "SELECT provider, kind, count(*) AS n FROM events GROUP BY 1, 2 ORDER BY 3 DESC",
            "SELECT count(*) FROM tool_calls",
            "SELECT count(*) FROM events",
            "SELECT count(*) FROM edges",
            "SHOW FAILED ATTEMPTS LIMIT 50",
        ],
        QUERY_RUNS,
    )?;

    // Time travel at ten points spread over the loaded history.
    let p = engine.projection();
    let start = p.sessions.iter().map(|s| s.started_at).min();
    let end = p.sessions.iter().map(|s| s.last_event_at).max();
    let mut state_us = Vec::with_capacity(STATE_POINTS);
    let mut state_rows = Vec::with_capacity(STATE_POINTS);
    if let (Some(a), Some(b)) = (start, end) {
        for i in 0..STATE_POINTS {
            let frac = (i as f64 + 0.5) / STATE_POINTS as f64;
            let at = Timestamp::from_micros(
                a.as_micros() + ((b.as_micros() - a.as_micros()) as f64 * frac) as i64,
            );
            let stmt = format!("STATE project AT '{}'", at.to_rfc3339());
            let t = Instant::now();
            let r = rt.block_on(engine.query(&stmt))?;
            state_us.push(t.elapsed().as_secs_f64() * 1e6);
            state_rows.push(r.row_count());
        }
    }

    let trace = match chain_attempt(&engine) {
        Some((id, attempts)) => {
            let stmt = format!("TRACE {id} CAUSES DEPTH 10");
            let mut us = Vec::with_capacity(QUERY_RUNS);
            let mut rows = 0;
            let mut notes = Vec::new();
            for _ in 0..QUERY_RUNS {
                let t = Instant::now();
                let r = rt.block_on(engine.query(&stmt))?;
                us.push(t.elapsed().as_secs_f64() * 1e6);
                rows = r.row_count();
                notes = r.notes.clone();
            }
            json!({
                "statement": stmt,
                "chain_attempts": attempts,
                "rows": rows,
                "notes": notes,
                "latency": Summary::of_micros(&mut us),
            })
        }
        None => {
            json!({"status": "not_run", "reason": "chained-failure session not in the loaded range"})
        }
    };

    Ok(json!({
        "db": root.display().to_string(),
        "events_requested": ctx.events,
        "events_loaded": n,
        "filtered": ctx.events > 0 && ctx.events < total,
        "engine_build_secs": build_secs,
        "engine_rows_per_sec": n as f64 / build_secs.max(1e-9),
        "projection": projection_counts(&engine),
        "tables": engine.tables()?.iter().map(|t| json!({"name": t.name, "rows": t.rows})).collect::<Vec<_>>(),
        "sql": sql,
        "state_at": {
            "points": STATE_POINTS,
            "rows_per_point": state_rows,
            "latency": Summary::of_micros(&mut state_us),
        },
        "trace_chain_depth_10": trace,
    }))
}

/// A small database whose first session is the chained-failure fixture,
/// then `TRACE` from its deepest attempt at several depths.
pub fn trace_chain(ctx: &StepCtx) -> Result<Value> {
    let root = ctx.out.join("db-chain");
    fresh_dir(&root)?;
    let events_total = ctx.events.clamp(2_000, 50_000);
    let mut cfg = GenConfig::new(ctx.seed, events_total);
    cfg.chain_at = 0.0;
    cfg.chain_attempts = 200;
    let mut stream = Workload::new(cfg);
    {
        let mut db = open_writer(&root, true, None)?;
        loop {
            let batch: Vec<Event> = stream.by_ref().take(1_000).collect();
            if batch.is_empty() {
                break;
            }
            db.ingest(batch)?;
        }
        db.close()?;
    }
    let db = open_reader(&root)?;
    let rt = runtime()?;
    let t = Instant::now();
    let engine = rt.block_on(QueryEngine::from_database(&db, &ScanFilter::default()))?;
    let build_secs = t.elapsed().as_secs_f64();
    let (id, attempts) = chain_attempt(&engine).context("chained session missing")?;
    let mut depths = serde_json::Map::new();
    for depth in [1usize, 10, 50, 200] {
        let stmt = format!("TRACE {id} CAUSES DEPTH {depth}");
        let mut us = Vec::with_capacity(TIMELINE_QUERY_RUNS);
        let mut rows = 0;
        let mut notes = Vec::new();
        for _ in 0..TIMELINE_QUERY_RUNS {
            let t = Instant::now();
            let r = rt.block_on(engine.query(&stmt))?;
            us.push(t.elapsed().as_secs_f64() * 1e6);
            rows = r.row_count();
            notes = r.notes.clone();
        }
        depths.insert(
            depth.to_string(),
            json!({
                "statement": stmt,
                "rows": rows,
                "notes": notes,
                "latency": Summary::of_micros(&mut us),
            }),
        );
    }
    let edges = rt
        .block_on(engine.query("SELECT count(*) AS n FROM edges"))?
        .to_json();
    let _ = std::fs::remove_dir_all(&root);
    Ok(json!({
        "events": engine.event_count(),
        "chain_attempts": attempts,
        "engine_build_secs": build_secs,
        "edges": edges,
        "projection": projection_counts(&engine),
        "depths": depths,
    }))
}
