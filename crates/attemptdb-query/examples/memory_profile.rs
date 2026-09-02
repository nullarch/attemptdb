//! Where the read path's memory goes, stage by stage, over one database.
//!
//!   cargo run --release -p attemptdb-query --example memory_profile -- <db-dir>
//!
//! Prints the process's resident set after each stage a live reader goes
//! through: open, decode segments, project, snapshot, build the engine,
//! build the SQL layer. Deltas are what each stage costs on top of the
//! previous one; the numbers are for sizing, not benchmarking.

use attemptdb_query::EngineCache;
use attemptdb_storage::{Database, OpenOptions};
use std::path::PathBuf;

fn rss_mib() -> f64 {
    let pid = std::process::id();
    let out = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()
        .expect("ps");
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse::<f64>()
        .unwrap_or(0.0)
        / 1024.0
}

fn main() -> anyhow::Result<()> {
    let root = PathBuf::from(std::env::args().nth(1).expect("db dir"));
    let mut last = rss_mib();
    let mut stage = |name: &str, events: usize| {
        let now = rss_mib();
        let per = if events > 0 {
            (now - last) * 1024.0 * 1024.0 / events as f64
        } else {
            0.0
        };
        println!(
            "{name:<34} {now:>8.0} MiB   {:>+8.0} MiB   {per:>7.0} B/event",
            now - last
        );
        last = now;
    };
    stage("start", 0);
    let db = Database::open(
        &root,
        OpenOptions {
            read_only: true,
            ..Default::default()
        },
    )?;
    let n = db.stats().segment_rows as usize + db.stats().memtable_rows;
    stage("open (WAL replayed)", n);
    // The same decode the engine cache does, in isolation, to see what the
    // decoded segments cost before the projector touches them.
    {
        let mut scan = attemptdb_storage::ScanCache::new();
        let r = scan.refresh(&db)?;
        stage("  (scan cache alone: events+batches)", n);
        let mut p = attemptdb_project::IncrementalProjector::new();
        for ev in r.events() {
            p.push(&ev);
        }
        stage("  (+ projector push, Obs per event)", n);
        drop(p);
        drop(r);
        drop(scan);
        stage("  (dropped both)", n);
    }
    let mut cache = EngineCache::new();
    let refreshed = cache.refresh(&db, "profile")?;
    stage("refresh: decode + project push", n);
    let projection = cache.snapshot();
    stage("snapshot (projection)", n);
    let engine = cache.engine_with(&refreshed, projection)?;
    stage("engine (parts, no SQL yet)", n);
    let rt = tokio::runtime::Runtime::new()?;
    let r = rt.block_on(engine.sql("SELECT count(*) AS n FROM events"))?;
    stage("first SQL (tables built)", n);
    println!(
        "\n{} events, {} sessions, {} tool calls, {} attempts, {} edges; count(*) = {}",
        n,
        engine.projection().sessions.len(),
        engine.projection().tool_calls.len(),
        engine.projection().attempts.len(),
        engine.projection().edges.len(),
        r.to_json()[0]["n"]
    );
    drop(engine);
    stage("drop engine", n);
    drop(refreshed);
    stage("drop refreshed (WAL copy)", n);
    drop(cache);
    stage("drop cache (segments, projector)", n);
    Ok(())
}
