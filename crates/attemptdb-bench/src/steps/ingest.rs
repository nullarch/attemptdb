//! Sustained ingest: `Database::ingest` in batches of 100 from the seeded
//! workload, Strict or Relaxed durability, optionally with a concurrent
//! reader thread that opens the database read-only, scans everything, and
//! projects it once a second.

use super::{StepCtx, fresh_dir, open_reader, open_writer, workload};
use crate::stats::{Stopwatch, Summary, disk_usage};
use anyhow::Result;
use attemptdb_core::Event;
use attemptdb_project::project;
use attemptdb_storage::ScanFilter;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

pub const BATCH: usize = 100;

/// Where an ingest step keeps its database.
pub fn db_path(ctx: &StepCtx) -> PathBuf {
    ctx.db.clone().unwrap_or_else(|| {
        ctx.out.join(format!(
            "db-{}-{}{}",
            if ctx.relaxed { "relaxed" } else { "strict" },
            ctx.events,
            if ctx.reader { "-reader" } else { "" }
        ))
    })
}

struct ReaderStats {
    iterations: u64,
    errors: u64,
    open_us: Vec<f64>,
    scan_us: Vec<f64>,
    project_us: Vec<f64>,
    max_events_seen: u64,
}

fn reader_loop(
    root: PathBuf,
    stop: Arc<AtomicBool>,
    events_visible: Arc<AtomicU64>,
) -> ReaderStats {
    let mut st = ReaderStats {
        iterations: 0,
        errors: 0,
        open_us: Vec::new(),
        scan_us: Vec::new(),
        project_us: Vec::new(),
        max_events_seen: 0,
    };
    while !stop.load(Ordering::Relaxed) {
        let started = Instant::now();
        let t0 = Instant::now();
        match open_reader(&root) {
            Ok(db) => {
                st.open_us.push(t0.elapsed().as_secs_f64() * 1e6);
                let t1 = Instant::now();
                match db.scan(&ScanFilter::default()) {
                    Ok(events) => {
                        st.scan_us.push(t1.elapsed().as_secs_f64() * 1e6);
                        let t2 = Instant::now();
                        let p = project(&events);
                        st.project_us.push(t2.elapsed().as_secs_f64() * 1e6);
                        st.max_events_seen = st.max_events_seen.max(p.stats.events_seen);
                        events_visible.store(p.stats.events_seen, Ordering::Relaxed);
                    }
                    Err(_) => st.errors += 1,
                }
            }
            Err(_) => st.errors += 1,
        }
        st.iterations += 1;
        let elapsed = started.elapsed();
        if elapsed < Duration::from_secs(1) {
            std::thread::sleep(Duration::from_secs(1) - elapsed);
        }
    }
    st
}

pub fn run(ctx: &StepCtx) -> Result<Value> {
    let root = db_path(ctx);
    if root.exists() {
        std::fs::remove_dir_all(&root)?;
    }
    if let Some(parent) = root.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let v = ingest_into(ctx, &root)?;
    if !ctx.keep && root.exists() {
        std::fs::remove_dir_all(&root)?;
    }
    Ok(v)
}

pub fn ingest_into(ctx: &StepCtx, root: &Path) -> Result<Value> {
    fresh_dir(root)?;
    let mut db = open_writer(root, ctx.relaxed, ctx.flush_events)?;
    let mut stream = workload(ctx.seed, ctx.events);

    let stop = Arc::new(AtomicBool::new(false));
    let visible = Arc::new(AtomicU64::new(0));
    let reader = ctx.reader.then(|| {
        let root = root.to_path_buf();
        let stop = Arc::clone(&stop);
        let visible = Arc::clone(&visible);
        std::thread::spawn(move || reader_loop(root, stop, visible))
    });

    let wall = Stopwatch::start();
    let mut gen_secs = 0.0;
    let mut ingest_secs = 0.0;
    let mut batch_us: Vec<f64> = Vec::with_capacity((ctx.events as usize / BATCH) + 1);
    let mut accepted = 0u64;
    let mut duplicates = 0u64;
    let mut wal_bytes = 0u64;
    let mut flushes = 0u64;
    let mut capped = false;
    let mut batch: Vec<Event> = Vec::with_capacity(BATCH);
    loop {
        let g = Instant::now();
        batch.clear();
        while batch.len() < BATCH {
            match stream.next() {
                Some(ev) => batch.push(ev),
                None => break,
            }
        }
        gen_secs += g.elapsed().as_secs_f64();
        if batch.is_empty() {
            break;
        }
        let events = std::mem::replace(&mut batch, Vec::with_capacity(BATCH));
        let t = Instant::now();
        let report = db.ingest(events)?;
        let d = t.elapsed();
        ingest_secs += d.as_secs_f64();
        batch_us.push(d.as_secs_f64() * 1e6);
        accepted += report.accepted as u64;
        duplicates += report.duplicates as u64;
        wal_bytes += report.bytes as u64;
        flushes += report.flushed_segments as u64;
        if wall.elapsed() > ctx.time_cap {
            capped = true;
            break;
        }
    }
    let t = Instant::now();
    db.close()?;
    let close_secs = t.elapsed().as_secs_f64();
    let wall_secs = wall.secs();

    stop.store(true, Ordering::Relaxed);
    let reader_stats = reader.map(|h| h.join().expect("reader thread"));

    let usage = disk_usage(root);
    let batch_summary = Summary::of_micros(&mut batch_us);
    let mut v = json!({
        "durability": if ctx.relaxed { "relaxed" } else { "strict" },
        "batch_size": BATCH,
        "flush_events": ctx.flush_events.unwrap_or(attemptdb_storage::OpenOptions::default().flush_events),
        "events_requested": ctx.events,
        "events_ingested": accepted,
        "duplicates": duplicates,
        "sessions_started": stream.sessions_started(),
        "capped": capped,
        "wall_secs": wall_secs,
        "generate_secs": gen_secs,
        "ingest_secs": ingest_secs,
        "close_secs": close_secs,
        "events_per_sec_ingest": accepted as f64 / ingest_secs.max(1e-9),
        "events_per_sec_wall": accepted as f64 / wall_secs.max(1e-9),
        "wal_bytes_written": wal_bytes,
        "wal_bytes_per_event": wal_bytes as f64 / accepted.max(1) as f64,
        "flushes": flushes,
        "batch_latency": batch_summary,
        "disk": usage,
        "segment_bytes_per_event": usage.segments_bytes as f64 / accepted.max(1) as f64,
        "compression_ratio_wal_to_segments": wal_bytes as f64 / usage.segments_bytes.max(1) as f64,
        "kind_counts": stream.kind_counts().into_iter().map(|(k, n)| (k.to_string(), json!(n))).collect::<serde_json::Map<String, Value>>(),
        "db_path": root.display().to_string(),
    });
    if let Some(r) = reader_stats {
        let mut open = r.open_us;
        let mut scan = r.scan_us;
        let mut proj = r.project_us;
        let mut total: Vec<f64> = scan
            .iter()
            .zip(&proj)
            .zip(&open)
            .map(|((s, p), o)| s + p + o)
            .collect();
        v["reader"] = json!({
            "iterations": r.iterations,
            "errors": r.errors,
            "max_events_seen": r.max_events_seen,
            "open": Summary::of_micros(&mut open),
            "scan": Summary::of_micros(&mut scan),
            "project": Summary::of_micros(&mut proj),
            "open_scan_project": Summary::of_micros(&mut total),
        });
    }
    Ok(v)
}
