//! Benchmark steps. Each step runs in its own process (spawned by `run`) so
//! peak RSS and page-cache state are attributable to that step alone, and
//! returns one JSON object.

pub mod hook;
pub mod ingest;
pub mod projection;
pub mod queries;
pub mod segments;
pub mod size;
pub mod wal;

use crate::stats::peak_rss_bytes;
use crate::workload::{GenConfig, Workload};
use anyhow::{Context, Result};
use attemptdb_core::Event;
use attemptdb_storage::{Database, DurabilityPolicy, OpenOptions};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Everything a step may need; unused fields are ignored by most steps.
#[derive(Clone, Debug)]
pub struct StepCtx {
    /// Working directory for datasets and scratch files.
    pub out: PathBuf,
    /// Event count the step should target.
    pub events: u64,
    pub seed: u64,
    /// Soft time cap: loops stop early once exceeded and report `capped`.
    pub time_cap: Duration,
    /// Path of the `attempt` binary (hook and daemon benchmarks).
    pub attempt_bin: Option<PathBuf>,
    /// Database directory to read (query benchmarks).
    pub db: Option<PathBuf>,
    /// Durability for ingest steps.
    pub relaxed: bool,
    /// Concurrent reader thread for the ingest step.
    pub reader: bool,
    /// Keep the database after an ingest step.
    pub keep: bool,
    /// Memtable flush threshold override.
    pub flush_events: Option<usize>,
    /// Projection mode: `materialized` or `streaming`.
    pub mode: String,
}

/// Names accepted by `attemptdb-bench step <name>`.
pub const STEP_NAMES: &[&str] = &[
    "size_by_kind",
    "wal_latency",
    "hook",
    "ingest",
    "segments",
    "projection",
    "recent_timeline",
    "scan_project",
    "engine",
    "trace_chain",
];

pub fn run_step(name: &str, ctx: &StepCtx) -> Result<Value> {
    let mut v = match name {
        "size_by_kind" => size::run(ctx)?,
        "wal_latency" => wal::run(ctx)?,
        "hook" => hook::run(ctx)?,
        "ingest" => ingest::run(ctx)?,
        "segments" => segments::run(ctx)?,
        "projection" => projection::run(ctx)?,
        "recent_timeline" => queries::recent_timeline(ctx)?,
        "scan_project" => queries::scan_project(ctx)?,
        "engine" => queries::engine(ctx)?,
        "trace_chain" => queries::trace_chain(ctx)?,
        other => anyhow::bail!("unknown step {other:?}; expected one of {STEP_NAMES:?}"),
    };
    if let Value::Object(m) = &mut v {
        m.insert("peak_rss_bytes".into(), json!(peak_rss_bytes()));
    }
    Ok(v)
}

/// Remove and recreate a directory.
pub fn fresh_dir(path: &Path) -> Result<()> {
    if path.exists() {
        std::fs::remove_dir_all(path).with_context(|| format!("removing {}", path.display()))?;
    }
    std::fs::create_dir_all(path).with_context(|| format!("creating {}", path.display()))?;
    Ok(())
}

pub fn open_writer(root: &Path, relaxed: bool, flush_events: Option<usize>) -> Result<Database> {
    let mut opts = OpenOptions {
        create: true,
        durability: if relaxed {
            DurabilityPolicy::Relaxed
        } else {
            DurabilityPolicy::Strict
        },
        device_id: Some(GenConfig::new(0, 0).device_id),
        ..Default::default()
    };
    if let Some(n) = flush_events {
        // An explicit event threshold means "count-governed": disable the
        // byte threshold, which at ~11 KB per event would otherwise flush
        // every ~750 events regardless of `flush_events`.
        opts.flush_events = n;
        opts.flush_bytes = usize::MAX;
    }
    Ok(Database::open(root, opts)?)
}

pub fn open_reader(root: &Path) -> Result<Database> {
    Ok(Database::open(
        root,
        OpenOptions {
            read_only: true,
            ..Default::default()
        },
    )?)
}

/// A tokio runtime for the async query engine.
pub fn runtime() -> Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?)
}

/// The seeded workload stream for `events` events.
pub fn workload(seed: u64, events: u64) -> Workload {
    Workload::new(GenConfig::new(seed, events))
}

/// Count events per `(kind, tool category)` — the weights the size
/// benchmark needs to turn per-profile bytes into a whole-workload share.
pub fn kind_category_counts(events: impl Iterator<Item = Event>) -> Vec<(String, u64)> {
    let mut counts: std::collections::BTreeMap<String, u64> = Default::default();
    for ev in events {
        let key = match &ev.tool {
            Some(t)
                if matches!(
                    ev.kind,
                    attemptdb_core::EventKind::ToolCallStarted
                        | attemptdb_core::EventKind::ToolCallFinished
                        | attemptdb_core::EventKind::ToolCallFailed
                ) =>
            {
                format!("{}/{}", ev.kind.as_str(), t.category.as_str())
            }
            _ => ev.kind.as_str().to_string(),
        };
        *counts.entry(key).or_default() += 1;
    }
    counts.into_iter().collect()
}
