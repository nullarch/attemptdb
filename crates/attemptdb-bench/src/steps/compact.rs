//! What compaction buys a reader, and what it costs the writer.
//!
//! Ingest the workload with a 500-event flush threshold so the database
//! ends up with ~200 small segments (the `segments` step measured what
//! those cost), take the reader numbers, run `Database::compact` until the
//! plan is empty, publish one more generation so the inputs are collected,
//! and take the reader numbers again on the same events.
//!
//! Output: `before` / `after` (segment count, bytes on disk, open / scan /
//! batches p50 over `READS` opens), `compaction` (wall seconds, runs,
//! inputs, input and output bytes, events), and the ratios.

use super::{StepCtx, ingest, open_reader, open_writer, workload};
use crate::stats::{Summary, disk_usage};
use anyhow::Result;
use attemptdb_storage::{CompactionPolicy, ScanFilter};
use serde_json::{Value, json};
use std::path::Path;
use std::time::Instant;

const READS: usize = 3;
pub const FLUSH_EVENTS: usize = 500;

pub fn run(ctx: &StepCtx) -> Result<Value> {
    let events = ctx.events.clamp(10_000, 100_000);
    let root = ctx.out.join("db-compact");
    let sub = StepCtx {
        events,
        relaxed: true,
        reader: false,
        keep: true,
        flush_events: Some(FLUSH_EVENTS),
        db: Some(root.clone()),
        ..ctx.clone()
    };
    let ingested = ingest::ingest_into(&sub, &root)?;
    {
        // The ingest step closes without a final flush; publish the tail so
        // every event is in a segment before and after.
        let mut db = open_writer(&root, true, Some(FLUSH_EVENTS))?;
        db.flush()?;
    }
    let before = measure(&root)?;

    // Everything under 8 MiB is small here (~1 MiB per 500-event flush);
    // `max_segments: 1` asks for as few segments as the policy allows.
    let policy = CompactionPolicy {
        max_segments: 1,
        ..Default::default()
    };
    let t = Instant::now();
    let mut db = open_writer(&root, true, None)?;
    let plan = db.compaction_plan(&policy)?;
    let mut runs = 0u64;
    let mut inputs = 0u64;
    let mut input_bytes = 0u64;
    let mut output_bytes = 0u64;
    let mut merged_events = 0u64;
    while let Some(r) = db.compact(&policy)? {
        runs += 1;
        inputs += r.inputs.len() as u64;
        input_bytes += r.input_bytes;
        output_bytes += r.output_bytes;
        merged_events += r.events;
    }
    let compact_secs = t.elapsed().as_secs_f64();
    let generation = db.manifest().generation;
    // The inputs are deleted once a later generation is durable: one more
    // event, one more flush, and the collection runs.
    let tail: Vec<_> = workload(ctx.seed.wrapping_add(7), 1).collect();
    db.ingest(tail)?;
    db.flush()?;
    let pending_after_flush = db.stats().tombstones;
    drop(db);
    let after = measure(&root)?;

    if !ctx.keep && root.exists() {
        std::fs::remove_dir_all(&root)?;
    }
    let ratio = |a: &Value, b: &Value, key: &str| -> f64 {
        let x = a[key]["p50_us"].as_f64().unwrap_or(0.0);
        let y = b[key]["p50_us"].as_f64().unwrap_or(0.0);
        if y > 0.0 { x / y } else { 0.0 }
    };
    Ok(json!({
        "events": events,
        "flush_events": FLUSH_EVENTS,
        "durability": "relaxed",
        "reads_per_measurement": READS,
        "ingest_wall_secs": ingested["wall_secs"],
        "before": before,
        "after": after,
        "compaction": {
            "secs": compact_secs,
            "planned_runs": plan.runs.len(),
            "runs": runs,
            "inputs": inputs,
            "input_bytes": input_bytes,
            "output_bytes": output_bytes,
            "events": merged_events,
            "generation": generation,
            "events_per_sec": if compact_secs > 0.0 { merged_events as f64 / compact_secs } else { 0.0 },
            "pending_deletions_after_next_flush": pending_after_flush,
        },
        "speedup_open_p50": ratio(&before, &after, "open"),
        "speedup_scan_all_p50": ratio(&before, &after, "scan_all"),
        "speedup_batches_all_p50": ratio(&before, &after, "batches_all"),
    }))
}

fn measure(root: &Path) -> Result<Value> {
    let mut open_us = Vec::new();
    let mut scan_us = Vec::new();
    let mut batches_us = Vec::new();
    let mut rows = 0usize;
    let mut segments = 0usize;
    for _ in 0..READS {
        let t = Instant::now();
        let db = open_reader(root)?;
        open_us.push(t.elapsed().as_secs_f64() * 1e6);
        segments = db.manifest().segments.len();
        let t = Instant::now();
        let evs = db.scan(&ScanFilter::default())?;
        scan_us.push(t.elapsed().as_secs_f64() * 1e6);
        rows = evs.len();
        drop(evs);
        let t = Instant::now();
        let b = db.batches(&ScanFilter::default())?;
        batches_us.push(t.elapsed().as_secs_f64() * 1e6);
        drop(b);
    }
    let usage = disk_usage(root);
    Ok(json!({
        "events": rows,
        "segments": segments,
        "segment_files": usage.segment_files,
        "segment_bytes": usage.segments_bytes,
        "manifest_bytes": usage.manifest_bytes,
        "open": Summary::of_micros(&mut open_us),
        "scan_all": Summary::of_micros(&mut scan_us),
        "batches_all": Summary::of_micros(&mut batches_us),
    }))
}
