//! A synthetic database shaped like a long-lived real one: a small share of
//! hook events (sessions, prompts, tool calls, retractions) across several
//! projects, and a large share of OpenTelemetry `unknown` rows the projection
//! ignores. Used to measure the read path (`attempt status`, a scoped
//! `attempt query`) without touching anyone's real data.
//!
//!   cargo run --release -p attemptdb-query --example mixed_db -- <dir> [events] [unknown-percent]
//!
//! `<dir>` becomes the database directory (it must not exist yet). Segments
//! are written inline (format 1, no encryption key), 20,000 events each.

use attemptdb_core::event::{EventContent, Provider};
use attemptdb_core::{
    CaptureMode, DeviceId, Event, EventKind, Outcome, PortablePath, ProjectRef, Timestamp,
    ToolCategory, ToolRef,
};
use attemptdb_storage::{Database, DurabilityPolicy, OpenOptions};
use serde_json::json;
use std::path::PathBuf;

const PROJECTS: &[(&str, &str)] = &[
    ("/work/alpha", "git@github.com:acme/alpha.git"),
    ("/work/beta", "git@github.com:acme/beta.git"),
    ("/work/gamma", "git@github.com:acme/gamma.git"),
    ("/work/delta", "git@github.com:acme/delta.git"),
    ("/work/epsilon", "git@github.com:acme/epsilon.git"),
    ("/work/zeta", "git@github.com:acme/zeta.git"),
];

/// A deterministic xorshift, enough to shuffle a synthetic stream.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let dir = PathBuf::from(
        args.next()
            .expect("usage: mixed_db <dir> [events] [unknown%]"),
    );
    let total: u64 = args
        .next()
        .map(|s| s.parse())
        .transpose()?
        .unwrap_or(1_000_000);
    let unknown_pct: u64 = args.next().map(|s| s.parse()).transpose()?.unwrap_or(90);
    anyhow::ensure!(!dir.exists(), "{} already exists", dir.display());

    let device = DeviceId::derive(&["mixed-db"]);
    let mut db = Database::open(
        &dir,
        OpenOptions {
            create: true,
            device_id: Some(device),
            durability: DurabilityPolicy::Relaxed,
            ..Default::default()
        },
    )?;
    let projects: Vec<ProjectRef> = PROJECTS
        .iter()
        .map(|(root, remote)| ProjectRef::derive(root, Some(remote), &device))
        .collect();
    let otel_project = ProjectRef::derive("otel/unattributed", None, &device);
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let start = 1_787_904_000_000_000i64; // 2026-08-28
    let span = 60 * 24 * 3600 * 1_000_000i64;
    let step = span / total as i64;

    let mut batch: Vec<Event> = Vec::with_capacity(5000);
    let mut session_no = 0u64;
    let mut hook_left = 0u64; // events left in the current hook session
    let mut sess = (String::new(), 0usize, Provider::ClaudeCode);
    let mut tool_no = 0u64;
    for i in 0..total {
        let at = Timestamp::from_micros(start + i as i64 * step);
        let telemetry = rng.below(100) < unknown_pct;
        let mut ev;
        if telemetry {
            let provider = if rng.below(2) == 0 {
                Provider::Codex
            } else {
                Provider::ClaudeCode
            };
            ev = Event::new(
                device,
                provider,
                "codex.sqlite.logs.write",
                EventKind::Unknown,
                otel_project.clone(),
                format!("otel-session-{}", rng.below(300)),
                CaptureMode::LocalSemantic,
                "otel/1",
            );
            let signal = ["logs", "metrics", "traces"][rng.below(3) as usize];
            ev.attrs.insert("source".into(), json!("otel"));
            ev.attrs.insert("x_otel_signal".into(), json!(signal));
            ev.attrs.insert("x_otel_record_type".into(), json!("log"));
            ev.attrs
                .insert("x_otel_session_attributed".into(), json!(false));
            ev.attrs
                .insert("x_otel_event_kind".into(), json!("sqlite_write"));
            ev.attrs
                .insert("x_otel_request_id".into(), json!(format!("req-{i:016x}")));
            ev.attrs
                .insert("x_otel_duration_ms".into(), json!(rng.below(900)));
            ev.attrs.insert(
                "x_otel_trace_id".into(),
                json!(format!("{:032x}", rng.next() as u128 * 7919)),
            );
            ev.attrs.insert(
                "x_otel_span_id".into(),
                json!(format!("{:016x}", rng.next())),
            );
            ev.raw = Some(json!({
                "timeUnixNano": at.as_micros() * 1000,
                "severityText": "INFO",
                "body": {"stringValue": format!("write {} rows to logs table", rng.below(500))},
                "attributes": (0..12).map(|k| json!({
                    "key": format!("attr.{k}"),
                    "value": {"stringValue": format!("value-{}-{}", k, rng.next())}
                })).collect::<Vec<_>>(),
            }));
        } else {
            if hook_left == 0 {
                session_no += 1;
                hook_left = 20 + rng.below(80);
                sess = (
                    format!("hook-session-{session_no}"),
                    rng.below(PROJECTS.len() as u64) as usize,
                    if rng.below(3) == 0 {
                        Provider::Codex
                    } else {
                        Provider::ClaudeCode
                    },
                );
            }
            let (id, p, provider) = (&sess.0, sess.1, sess.2.clone());
            let kind_roll = hook_left % 10;
            hook_left -= 1;
            let (kind, name) = if kind_roll == 9 {
                (EventKind::PromptSubmitted, "UserPromptSubmit")
            } else if kind_roll.is_multiple_of(2) {
                (EventKind::ToolCallStarted, "PreToolUse")
            } else if kind_roll == 7 && rng.below(10) == 0 {
                (EventKind::ToolCallFailed, "PostToolUseFailure")
            } else {
                (EventKind::ToolCallFinished, "PostToolUse")
            };
            ev = Event::new(
                device,
                provider,
                name,
                kind,
                projects[p].clone(),
                id.clone(),
                CaptureMode::LocalSemantic,
                "mixed-db/1",
            );
            match kind {
                EventKind::PromptSubmitted => {
                    ev.content = Some(EventContent {
                        prompt: Some(format!("please fix the failing test number {i}")),
                        ..Default::default()
                    });
                }
                _ => {
                    tool_no += 1;
                    ev.tool = Some(ToolRef {
                        name: "Edit".into(),
                        category: ToolCategory::FileEdit,
                        call_id: Some(format!("call-{}", tool_no / 2)),
                    });
                    ev.paths = vec![PortablePath::from_raw(
                        &format!("{}/src/file{}.rs", PROJECTS[p].0, rng.below(40)),
                        Some(PROJECTS[p].0),
                    )];
                    if kind == EventKind::ToolCallFinished {
                        ev.outcome = Some(Outcome::success());
                    } else if kind == EventKind::ToolCallFailed {
                        ev.outcome = Some(Outcome::failure(Some("string_mismatch".into())));
                    }
                }
            }
        }
        ev.observed_at = at;
        ev.captured_at = at;
        batch.push(ev);
        if batch.len() == 5000 {
            db.ingest(std::mem::take(&mut batch))?;
        }
    }
    if !batch.is_empty() {
        db.ingest(batch)?;
    }
    db.flush()?;
    let stats = db.stats();
    println!(
        "{}: {} events in {} segments, {} bytes on disk",
        dir.display(),
        stats.segment_rows,
        stats.segments,
        stats.segment_bytes
    );
    db.close()?;
    Ok(())
}
