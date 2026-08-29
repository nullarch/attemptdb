//! Database size and compression by event kind: per-profile synthetic
//! subsets encoded as JSON, JSONL+zstd, uncompressed Arrow IPC, and a real
//! segment (Arrow IPC with per-buffer zstd), then weighted by the kind mix
//! of a generated stream.

use super::{StepCtx, fresh_dir, kind_category_counts, workload};
use crate::model;
use crate::workload::{Profile, profile_events};
use anyhow::Result;
use arrow::ipc::writer::FileWriter;
use attemptdb_core::ToolCategory;
use attemptdb_storage::segment::{events_schema, events_to_batch, write_segment};
use serde_json::{Value, json};
use std::io::Cursor;

pub const EVENTS_PER_PROFILE: usize = 3_000;
const MIX_SAMPLE_EVENTS: u64 = 100_000;

fn profiles() -> Vec<Profile> {
    let cats = [
        ToolCategory::Shell,
        ToolCategory::FileEdit,
        ToolCategory::FileRead,
        ToolCategory::FileWrite,
        ToolCategory::Subagent,
        ToolCategory::Web,
    ];
    let mut v = Vec::new();
    for c in cats {
        v.push(Profile::ToolStart(c));
    }
    for c in cats {
        v.push(Profile::ToolFinish(c));
    }
    v.push(Profile::ToolFailed(ToolCategory::Shell));
    v.extend([
        Profile::Prompt,
        Profile::AgentMessage,
        Profile::SubagentStopped,
        Profile::TurnStopped,
        Profile::Notification,
        Profile::SessionStarted,
    ]);
    v
}

pub fn run(ctx: &StepCtx) -> Result<Value> {
    let tmp = ctx.out.join("size-by-kind");
    fresh_dir(&tmp)?;
    let mut rows = Vec::new();
    for profile in profiles() {
        let events = profile_events(ctx.seed, profile, EVENTS_PER_PROFILE);
        let n = events.len();
        let mut jsonl = Vec::new();
        for ev in &events {
            jsonl.extend(attemptdb_core::codec::encode_event(ev)?);
            jsonl.push(b'\n');
        }
        let json_bytes = jsonl.len() as u64;
        let jsonl_zstd = zstd::bulk::compress(&jsonl, 3)?.len() as u64;
        let batch = events_to_batch(&events)?;
        let mut buf = Cursor::new(Vec::new());
        {
            let mut w = FileWriter::try_new(&mut buf, &events_schema())?;
            w.write(&batch)?;
            w.finish()?;
        }
        let arrow_plain = buf.into_inner().len() as u64;
        let meta = write_segment(&tmp, &events)?;
        let segment = meta.bytes;
        let _ = std::fs::remove_file(tmp.join("segments").join(&meta.file));
        rows.push(json!({
            "profile": profile.label(),
            "events": n,
            "json_bytes_per_event": json_bytes as f64 / n as f64,
            "jsonl_zstd3_bytes_per_event": jsonl_zstd as f64 / n as f64,
            "arrow_ipc_plain_bytes_per_event": arrow_plain as f64 / n as f64,
            "segment_bytes_per_event": segment as f64 / n as f64,
            "segment_ratio_vs_json": json_bytes as f64 / segment.max(1) as f64,
            "jsonl_zstd3_ratio_vs_json": json_bytes as f64 / jsonl_zstd.max(1) as f64,
        }));
    }
    let _ = std::fs::remove_dir_all(&tmp);

    // Weight the profiles by the kind/category mix of a generated stream.
    let counts = kind_category_counts(workload(ctx.seed, MIX_SAMPLE_EVENTS));
    let total: u64 = counts.iter().map(|(_, n)| n).sum();
    let mut weighted_segment = 0.0;
    let mut weighted_json = 0.0;
    let mut covered = 0u64;
    let mut shares = Vec::new();
    for (key, n) in &counts {
        if let Some(row) = rows.iter().find(|r| r["profile"] == *key) {
            let seg = row["segment_bytes_per_event"].as_f64().unwrap_or(0.0);
            let js = row["json_bytes_per_event"].as_f64().unwrap_or(0.0);
            weighted_segment += seg * *n as f64;
            weighted_json += js * *n as f64;
            covered += n;
            shares.push((key.clone(), *n, seg * *n as f64));
        }
    }
    let share_rows: Vec<Value> = shares
        .iter()
        .map(|(k, n, bytes)| {
            json!({
                "profile": k,
                "event_share": *n as f64 / total as f64,
                "segment_byte_share": bytes / weighted_segment.max(1.0),
            })
        })
        .collect();
    Ok(json!({
        "events_per_profile": EVENTS_PER_PROFILE,
        "profiles": rows,
        "mix_sample_events": MIX_SAMPLE_EVENTS,
        "mix_coverage": covered as f64 / total as f64,
        "weighted_segment_bytes_per_event": weighted_segment / covered.max(1) as f64,
        "weighted_json_bytes_per_event": weighted_json / covered.max(1) as f64,
        "weighted_ratio": weighted_json / weighted_segment.max(1.0),
        "segment_byte_share": share_rows,
        "sampled_real_database": {
            "json_bytes_per_event": model::SAMPLED_JSON_BYTES_PER_EVENT,
            "segment_bytes_per_event": model::SAMPLED_SEGMENT_BYTES_PER_EVENT,
            "ratio": model::SAMPLED_JSON_BYTES_PER_EVENT / model::SAMPLED_SEGMENT_BYTES_PER_EVENT,
        },
    }))
}
