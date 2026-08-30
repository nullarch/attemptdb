//! What a refreshing reader pays after new events arrive.
//!
//! Before: every reload re-decoded every segment and re-projected the whole
//! history (`docs/benchmarks.md` item 7). Now a reader keeps a `ScanCache`
//! and an `IncrementalProjector`; a reload decodes only new segments and
//! re-finalises only the sessions the new events touched. This step measures
//! both paths on the same database so the claim is a number, not a design.
//!
//! Output (seconds unless stated): `cold_from_database` (the old path, from
//! scratch), `cache_cold` (first cached load: decode + project + engine),
//! `warm_after_wal_append` (reload after `appended` events landed in the
//! WAL), `warm_after_flush` (reload after those events became a segment),
//! with the decode counts, plus `cold_from_database_after_append` for the
//! like-for-like comparison.

use super::{StepCtx, ingest, open_reader, open_writer, runtime, workload};
use anyhow::Result;
use attemptdb_project::IncrementalProjector;
use attemptdb_query::QueryEngine;
use attemptdb_storage::{ScanCache, ScanFilter};
use serde_json::{Value, json};
use std::time::Instant;

pub fn run(ctx: &StepCtx) -> Result<Value> {
    let root = ctx.out.join("refresh-db");
    let ingest_report = ingest::ingest_into(ctx, &root)?;
    {
        // The benchmark writer closes without a final flush; publish the
        // tail so the cold path starts from segments only.
        let mut db = open_writer(&root, ctx.relaxed, ctx.flush_events)?;
        db.flush()?;
    }
    let rt = runtime()?;

    // Old path, cold.
    let db = open_reader(&root)?;
    let t = Instant::now();
    let engine = rt.block_on(QueryEngine::from_database(&db, &ScanFilter::default()))?;
    let cold_from_database = t.elapsed().as_secs_f64();
    let base_events = engine.event_count();
    drop(engine);
    drop(db);

    // Cached path, cold: the same work, but the cache and projector persist.
    let mut cache = ScanCache::new();
    let mut projector = IncrementalProjector::new();
    let db = open_reader(&root)?;
    let t = Instant::now();
    let refreshed = cache.refresh(&db)?;
    for ev in refreshed.fresh_events() {
        projector.push(ev);
    }
    let decode_secs = t.elapsed().as_secs_f64();
    let t2 = Instant::now();
    let projection = projector.snapshot();
    let project_secs = t2.elapsed().as_secs_f64();
    let t3 = Instant::now();
    let engine = rt.block_on(QueryEngine::from_parts(
        refreshed.batches()?,
        projection,
        refreshed.events(),
    ))?;
    let engine_secs = t3.elapsed().as_secs_f64();
    let cache_cold = t.elapsed().as_secs_f64();
    assert_eq!(engine.event_count(), base_events);
    let cold_decodes = cache.decodes;
    drop(engine);
    drop(refreshed);
    drop(db);

    // New events arrive in the WAL.
    let appended = (ctx.events / 200).clamp(50, 5_000);
    {
        let mut db = open_writer(&root, ctx.relaxed, None)?;
        let batch: Vec<_> = workload(ctx.seed.wrapping_add(1), appended).collect();
        db.ingest(batch)?;
    }
    let warm_wal = timed_refresh(&rt, &root, &mut cache, &mut projector)?;
    assert_eq!(warm_wal.events, base_events + appended as usize);

    // The WAL becomes a segment.
    {
        let mut db = open_writer(&root, ctx.relaxed, None)?;
        db.flush()?;
    }
    let warm_flush = timed_refresh(&rt, &root, &mut cache, &mut projector)?;
    assert_eq!(warm_flush.events, base_events + appended as usize);

    // Old path again on the grown database, for the like-for-like number.
    let db = open_reader(&root)?;
    let t = Instant::now();
    let engine = rt.block_on(QueryEngine::from_database(&db, &ScanFilter::default()))?;
    let cold_after = t.elapsed().as_secs_f64();
    assert_eq!(engine.event_count(), base_events + appended as usize);
    drop(engine);
    drop(db);

    if !ctx.keep && root.exists() {
        std::fs::remove_dir_all(&root)?;
    }
    Ok(json!({
        "events": base_events,
        "appended": appended,
        "ingest": ingest_report,
        "cold_from_database": cold_from_database,
        "cache_cold": cache_cold,
        "cache_cold_breakdown": {
            "decode_and_push": decode_secs,
            "project": project_secs,
            "engine": engine_secs,
            "segments_decoded": cold_decodes,
        },
        "warm_after_wal_append": warm_wal.as_json(),
        "warm_after_flush": warm_flush.as_json(),
        "cold_from_database_after_append": cold_after,
        "speedup_warm_vs_cold": if warm_flush.total > 0.0 { cold_after / warm_flush.total } else { 0.0 },
    }))
}

struct Timed {
    total: f64,
    refresh: f64,
    project: f64,
    engine: f64,
    decodes_this_refresh: u64,
    sessions_rebuilt: usize,
    events: usize,
}

impl Timed {
    fn as_json(&self) -> Value {
        json!({
            "total": self.total,
            "refresh": self.refresh,
            "project": self.project,
            "engine": self.engine,
            "segments_decoded": self.decodes_this_refresh,
            "sessions_rebuilt": self.sessions_rebuilt,
            "events": self.events,
        })
    }
}

fn timed_refresh(
    rt: &tokio::runtime::Runtime,
    root: &std::path::Path,
    cache: &mut ScanCache,
    projector: &mut IncrementalProjector,
) -> Result<Timed> {
    let before = cache.decodes;
    let db = open_reader(root)?;
    let t = Instant::now();
    let refreshed = cache.refresh(&db)?;
    for ev in refreshed.fresh_events() {
        projector.push(ev);
    }
    let refresh = t.elapsed().as_secs_f64();
    let sessions_rebuilt = projector.pending_sessions();
    let t2 = Instant::now();
    let projection = projector.snapshot();
    let project = t2.elapsed().as_secs_f64();
    let t3 = Instant::now();
    let engine = rt.block_on(QueryEngine::from_parts(
        refreshed.batches()?,
        projection,
        refreshed.events(),
    ))?;
    let engine_secs = t3.elapsed().as_secs_f64();
    Ok(Timed {
        total: t.elapsed().as_secs_f64(),
        refresh,
        project,
        engine: engine_secs,
        decodes_this_refresh: cache.decodes - before,
        sessions_rebuilt,
        events: engine.event_count(),
    })
}
