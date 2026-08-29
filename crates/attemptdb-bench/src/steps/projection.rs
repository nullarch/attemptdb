//! Projection cost versus event count, two ways:
//!
//! - `materialized`: the whole stream is held as `Vec<Event>` (what the
//!   query engine does today) and `project()` runs over it. The RSS is
//!   dominated by the events themselves.
//! - `streaming`: events are pushed one at a time into a `Projector` and
//!   dropped, so the RSS is the projector's own state.

use super::{StepCtx, workload};
use anyhow::Result;
use attemptdb_core::Event;
use attemptdb_project::{Projector, project};
use serde_json::{Value, json};
use std::time::Instant;

pub fn run(ctx: &StepCtx) -> Result<Value> {
    match ctx.mode.as_str() {
        "streaming" => streaming(ctx),
        _ => materialized(ctx),
    }
}

fn counts(p: &attemptdb_project::Projection) -> Value {
    json!({
        "sessions": p.sessions.len(),
        "turns": p.turns.len(),
        "tool_calls": p.tool_calls.len(),
        "attempts": p.attempts.len(),
        "handoffs": p.handoffs.len(),
        "edges": p.edges.len(),
    })
}

fn materialized(ctx: &StepCtx) -> Result<Value> {
    let t = Instant::now();
    let events: Vec<Event> = workload(ctx.seed, ctx.events).collect();
    let generate_secs = t.elapsed().as_secs_f64();
    let json_bytes: u64 = events
        .iter()
        .step_by(97)
        .map(|e| {
            attemptdb_core::codec::encode_event(e)
                .map(|b| b.len() as u64)
                .unwrap_or(0)
        })
        .sum::<u64>()
        * 97;
    let t = Instant::now();
    let p = project(&events);
    let project_secs = t.elapsed().as_secs_f64();
    Ok(json!({
        "mode": "materialized",
        "events": events.len(),
        "approx_json_bytes": json_bytes,
        "generate_secs": generate_secs,
        "project_secs": project_secs,
        "project_rows_per_sec": events.len() as f64 / project_secs.max(1e-9),
        "projection": counts(&p),
    }))
}

fn streaming(ctx: &StepCtx) -> Result<Value> {
    let mut projector = Projector::new();
    let mut push_secs = 0.0;
    let t = Instant::now();
    let mut n = 0usize;
    for ev in workload(ctx.seed, ctx.events) {
        let t = Instant::now();
        projector.push(&ev);
        push_secs += t.elapsed().as_secs_f64();
        n += 1;
    }
    let generate_and_push_secs = t.elapsed().as_secs_f64();
    let t = Instant::now();
    let p = projector.finish();
    let finish_secs = t.elapsed().as_secs_f64();
    Ok(json!({
        "mode": "streaming",
        "events": n,
        "generate_and_push_secs": generate_and_push_secs,
        "push_secs": push_secs,
        "finish_secs": finish_secs,
        "project_secs": push_secs + finish_secs,
        "project_rows_per_sec": n as f64 / (push_secs + finish_secs).max(1e-9),
        "projection": counts(&p),
    }))
}
