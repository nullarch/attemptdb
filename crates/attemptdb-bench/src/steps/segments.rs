//! Many small segments versus few large ones: there is no compaction yet,
//! so the segment count is set by the flush thresholds. Ingest the same
//! stream under three event thresholds (with the byte threshold disabled so
//! the count is what varies) and measure what readers pay.

use super::{StepCtx, ingest, open_reader};
use crate::stats::{Summary, disk_usage};
use anyhow::Result;
use attemptdb_storage::ScanFilter;
use serde_json::{Value, json};
use std::time::Instant;

const READS: usize = 3;
pub const THRESHOLDS: &[usize] = &[500, 5_000, 50_000];

pub fn run(ctx: &StepCtx) -> Result<Value> {
    let events = ctx.events.clamp(10_000, 100_000);
    let mut variants = Vec::new();
    for &threshold in THRESHOLDS {
        let root = ctx.out.join(format!("db-segments-{threshold}"));
        let sub = StepCtx {
            events,
            relaxed: true,
            reader: false,
            keep: true,
            flush_events: Some(threshold),
            db: Some(root.clone()),
            ..ctx.clone()
        };
        let ingested = ingest::ingest_into(&sub, &root)?;
        let mut open_us = Vec::new();
        let mut scan_us = Vec::new();
        let mut batches_us = Vec::new();
        let mut rows = 0usize;
        for _ in 0..READS {
            let t = Instant::now();
            let db = open_reader(&root)?;
            open_us.push(t.elapsed().as_secs_f64() * 1e6);
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
        let usage = disk_usage(&root);
        let _ = std::fs::remove_dir_all(&root);
        variants.push(json!({
            "flush_events": threshold,
            "events": rows,
            "segments": usage.segment_files,
            "segment_bytes": usage.segments_bytes,
            "manifest_bytes": usage.manifest_bytes,
            "ingest_events_per_sec": ingested["events_per_sec_ingest"],
            "ingest_wall_secs": ingested["wall_secs"],
            "flushes": ingested["flushes"],
            "open": Summary::of_micros(&mut open_us),
            "scan_all": Summary::of_micros(&mut scan_us),
            "batches_all": Summary::of_micros(&mut batches_us),
        }));
    }
    Ok(json!({
        "events": events,
        "durability": "relaxed",
        "reads_per_variant": READS,
        "variants": variants,
    }))
}
