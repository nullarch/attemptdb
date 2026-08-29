//! WAL acknowledgement latency: single-event `ingest` calls under Strict and
//! Relaxed durability, single-event spool appends with sync off and on, and
//! the raw fsync floor of the file system the database sits on.

use super::{StepCtx, fresh_dir};
use crate::stats::Summary;
use crate::workload::{Profile, profile_events};
use anyhow::Result;
use attemptdb_core::ToolCategory;
use attemptdb_storage::{Database, DurabilityPolicy, OpenOptions, SpoolWriter};
use serde_json::{Value, json};
use std::io::Write;
use std::time::Instant;

pub const SAMPLES: usize = 2_000;
pub const FSYNC_SAMPLES: usize = 500;

fn single_ingest(root: &std::path::Path, durability: DurabilityPolicy, seed: u64) -> Result<Value> {
    fresh_dir(root)?;
    let mut db = Database::open(
        root,
        OpenOptions {
            create: true,
            durability,
            // No flush during the sample: the WAL acknowledgement is what is
            // measured, not the periodic segment write.
            flush_events: usize::MAX,
            flush_bytes: usize::MAX,
            ..Default::default()
        },
    )?;
    let events = profile_events(seed, Profile::ToolFinish(ToolCategory::Shell), SAMPLES);
    let bytes: usize = events
        .iter()
        .map(|e| {
            attemptdb_core::codec::encode_event(e)
                .map(|b| b.len())
                .unwrap_or(0)
        })
        .sum();
    let mut us = Vec::with_capacity(SAMPLES);
    for ev in events {
        let t = Instant::now();
        db.ingest(vec![ev])?;
        us.push(t.elapsed().as_secs_f64() * 1e6);
    }
    drop(db);
    Ok(json!({
        "samples": SAMPLES,
        "event_bytes_mean": bytes as f64 / SAMPLES as f64,
        "latency": Summary::of_micros(&mut us),
    }))
}

fn spool_append(root: &std::path::Path, sync: bool, seed: u64) -> Result<Value> {
    fresh_dir(root)?;
    let writer = SpoolWriter::new(root)?;
    let events = profile_events(seed, Profile::ToolFinish(ToolCategory::Shell), SAMPLES);
    let mut us = Vec::with_capacity(SAMPLES);
    for ev in &events {
        let t = Instant::now();
        writer.append_with(std::slice::from_ref(ev), sync)?;
        us.push(t.elapsed().as_secs_f64() * 1e6);
    }
    Ok(json!({
        "samples": SAMPLES,
        "sync": sync,
        "latency": Summary::of_micros(&mut us),
    }))
}

/// Append 4 KiB and sync, three ways: Rust's `sync_data` (which is
/// `F_FULLFSYNC` on macOS), plain `fsync(2)`, and `F_FULLFSYNC` explicitly.
fn fsync_floor(root: &std::path::Path) -> Result<Value> {
    #[cfg(unix)]
    use std::os::fd::AsRawFd;
    fresh_dir(root)?;
    let block = vec![0x5au8; 4096];
    let mut out = serde_json::Map::new();

    let mut sample =
        |label: &str, sync: &dyn Fn(&std::fs::File) -> std::io::Result<()>| -> Result<()> {
            let path = root.join(format!("{label}.bin"));
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)?;
            let mut us = Vec::with_capacity(FSYNC_SAMPLES);
            for _ in 0..FSYNC_SAMPLES {
                f.write_all(&block)?;
                let t = Instant::now();
                sync(&f)?;
                us.push(t.elapsed().as_secs_f64() * 1e6);
            }
            out.insert(label.to_string(), json!(Summary::of_micros(&mut us)));
            Ok(())
        };

    sample("sync_data", &|f| f.sync_data())?;
    // Raw fsync(2) has no Windows equivalent to compare against; there
    // `sync_data` is the only sync primitive and is already sampled above.
    #[cfg(unix)]
    sample("fsync", &|f| {
        // SAFETY: the descriptor belongs to an open `File` for the call's
        // duration.
        let rc = unsafe { libc::fsync(f.as_raw_fd()) };
        if rc == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    })?;
    #[cfg(target_os = "macos")]
    sample("f_fullfsync", &|f| {
        // SAFETY: as above; F_FULLFSYNC takes no argument.
        let rc = unsafe { libc::fcntl(f.as_raw_fd(), libc::F_FULLFSYNC) };
        if rc == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    })?;
    out.insert("samples".into(), json!(FSYNC_SAMPLES));
    out.insert("write_bytes".into(), json!(block.len()));
    Ok(Value::Object(out))
}

pub fn run(ctx: &StepCtx) -> Result<Value> {
    let base = ctx.out.join("wal-latency");
    fresh_dir(&base)?;
    let strict = single_ingest(&base.join("strict"), DurabilityPolicy::Strict, ctx.seed)?;
    let relaxed = single_ingest(&base.join("relaxed"), DurabilityPolicy::Relaxed, ctx.seed)?;
    let spool_nosync = spool_append(&base.join("spool-nosync"), false, ctx.seed)?;
    let spool_sync = spool_append(&base.join("spool-sync"), true, ctx.seed)?;
    let floor = fsync_floor(&base.join("fsync"))?;
    let _ = std::fs::remove_dir_all(&base);
    Ok(json!({
        "ingest_strict": strict,
        "ingest_relaxed": relaxed,
        "spool_append_nosync": spool_nosync,
        "spool_append_sync": spool_sync,
        "fsync_floor": floor,
    }))
}
