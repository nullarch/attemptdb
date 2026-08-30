//! JSON shapes of the projected entities for the `/api/` endpoints: readable
//! ids (`att_…`), RFC 3339 timestamps, evidence ids inline.

use crate::store::{CaptureCounts, View};
use attemptdb_core::{DecisionId, Timestamp, WorkUnitId};
use attemptdb_project::{
    Attempt, Decision, Handoff, Projection, Session, SessionState, Signal, ToolCall, Turn, WorkUnit,
};
use attemptdb_query::PrefixedId;
use serde_json::{Value, json};

pub fn id<T: PrefixedId>(x: &T) -> Value {
    Value::String(x.readable())
}

pub fn id_opt<T: PrefixedId>(x: &Option<T>) -> Value {
    x.as_ref().map(id).unwrap_or(Value::Null)
}

pub fn ids<T: PrefixedId>(list: &[T]) -> Value {
    Value::Array(list.iter().map(id).collect())
}

pub fn ts(t: Timestamp) -> Value {
    Value::String(t.to_rfc3339())
}

pub fn ts_opt(t: Option<Timestamp>) -> Value {
    t.map(ts).unwrap_or(Value::Null)
}

/// A `f32` confidence as the short JSON number people expect (`0.9`).
pub fn conf(c: f32) -> Value {
    c.to_string()
        .parse::<f64>()
        .ok()
        .and_then(serde_json::Number::from_f64)
        .map(Value::Number)
        .unwrap_or(Value::Null)
}

pub fn tool_call(tc: &ToolCall) -> Value {
    let duration = tc
        .duration_ms
        .or_else(|| match (tc.started_at, tc.finished_at) {
            (Some(s), Some(e)) => Some(crate::html::elapsed_ms(s, e)),
            _ => None,
        });
    json!({
        "tool_call_id": id(&tc.tool_call_id),
        "session_id": id(&tc.session_id),
        "turn_id": id_opt(&tc.turn_id),
        "agent_id": id(&tc.agent_id),
        "tool_name": tc.tool.name,
        "tool_category": tc.tool.category.as_str(),
        "provider_call_id": tc.tool.call_id,
        "started_at": ts_opt(tc.started_at),
        "finished_at": ts_opt(tc.finished_at),
        "in_flight": tc.started_at.is_some() && tc.finished_at.is_none(),
        "duration_ms": duration,
        "outcome_status": tc.outcome.as_ref().map(|o| o.status.as_str()),
        "outcome_class": tc.outcome.as_ref().and_then(|o| o.class.clone()),
        "exit_code": tc.outcome.as_ref().and_then(|o| o.exit_code),
        "paths": tc.paths.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
        "start_event_id": id_opt(&tc.start_event_id),
        "end_event_id": id_opt(&tc.end_event_id),
    })
}

pub fn attempt(a: &Attempt, p: &Projection, with_tools: bool) -> Value {
    let mut v = json!({
        "attempt_id": id(&a.attempt_id),
        "session_id": id(&a.session_id),
        "turn_id": id(&a.turn_id),
        "turn_index": a.turn_index,
        "index": a.index,
        "objective": a.objective,
        "approach": a.approach,
        "started_at": ts(a.started_at),
        "ended_at": ts_opt(a.ended_at),
        "duration_ms": a.ended_at.map(|e| crate::html::elapsed_ms(a.started_at, e)),
        "outcome": a.outcome.as_str(),
        "failure_class": a.failure_class,
        "paths": a.paths,
        "commit_shas": a.commit_shas,
        "tool_call_ids": ids(&a.tool_call_ids),
        "superseded_by": id_opt(&a.superseded_by),
        "supersedes": id_opt(&a.supersedes),
        "evidence": ids(&a.evidence),
        "confidence": conf(a.confidence),
        "algorithm_version": a.algorithm_version.as_str(),
        "work_unit_id": a.work_unit_id.as_ref().map(wu_id),
        "corrected": a.corrected.as_ref().map(|c| json!({
            "event_id": id(&c.event_id),
            "at": ts(c.at),
            "correction_type": c.correction_type.as_str(),
        })),
        "inferred_outcome": a.inferred_outcome.map(|o| o.as_str()),
        "inferred_failure_class": a.inferred_failure_class,
        "note": a.note,
    });
    if with_tools {
        let calls: Vec<Value> = a
            .tool_call_ids
            .iter()
            .filter_map(|id| p.tool_calls.iter().find(|c| &c.tool_call_id == id))
            .map(tool_call)
            .collect();
        v["tool_calls"] = Value::Array(calls);
    }
    v
}

pub fn turn(t: &Turn, attempts: Vec<Value>) -> Value {
    json!({
        "turn_id": id(&t.turn_id),
        "session_id": id(&t.session_id),
        "index": t.index,
        "status": t.status.as_str(),
        "started_at": ts(t.started_at),
        "ended_at": ts_opt(t.ended_at),
        "objective": t.objective,
        "prompt_chars": t.prompt_chars,
        "prompt_event_id": id_opt(&t.prompt_event_id),
        "stop_event_id": id_opt(&t.stop_event_id),
        "tool_call_ids": ids(&t.tool_call_ids),
        "attempts": attempts,
    })
}

pub fn session(s: &Session, capture: CaptureCounts, turns: Option<Vec<Value>>) -> Value {
    let mut v = json!({
        "session_id": id(&s.session_id),
        "provider": s.provider.as_str(),
        "provider_name": s.provider.display_name(),
        "provider_session_id": s.provider_session_id,
        "project_id": id(&s.project_id),
        "project_name": s.project_name,
        "started_at": ts(s.started_at),
        "ended_at": ts_opt(s.ended_at),
        "last_event_at": ts(s.last_event_at),
        "end_reason": s.end_reason,
        "start_source": s.start_source,
        "event_count": s.event_count,
        "turn_count": s.turn_count,
        "prompt_count": s.prompt_count,
        "tool_call_count": s.tool_call_count,
        "failure_count": s.failure_count,
        "agents": ids(&s.agents),
        "coverage": s.coverage.as_str(),
        "captured_events": capture.captured,
        "reconstructed_events": capture.reconstructed,
        "first_event_id": id(&s.first_event_id),
        "last_event_id": id(&s.last_event_id),
        "start_event_id": id_opt(&s.start_event_id),
        "end_event_id": id_opt(&s.end_event_id),
    });
    if let Some(t) = turns {
        v["turns"] = Value::Array(t);
    }
    v
}

pub fn handoff(h: &Handoff) -> Value {
    json!({
        "at": ts(h.at),
        "from_session": id(&h.from_session),
        "to_session": id(&h.to_session),
        "from_provider": h.from_provider.as_str(),
        "to_provider": h.to_provider.as_str(),
        "project_id": id(&h.project_id),
        "gap_ms": h.gap_ms,
        "shared_paths": h.shared_paths,
        "evidence": ids(&h.evidence),
        "confidence": conf(h.confidence),
    })
}

pub fn signal(g: &Signal) -> Value {
    json!({
        "session_id": id(&g.session_id),
        "event_id": id(&g.event_id),
        "at": ts(g.at),
        "kind": g.kind.as_str(),
        "signal_type": g.signal_type,
        "cleared_at": ts_opt(g.cleared_at),
        "cleared_by": id_opt(&g.cleared_by),
    })
}

pub fn session_state(st: &SessionState) -> Value {
    json!({
        "session_id": id(&st.session_id),
        "provider": st.provider.as_str(),
        "project_id": id(&st.project_id),
        "open": st.open,
        "coverage": st.coverage.as_str(),
        "current_turn": id_opt(&st.current_turn),
        "turn_index": st.turn_index,
        "turn_status": st.turn_status.map(|s| s.as_str()),
        "in_flight_tool_calls": ids(&st.in_flight_tool_calls),
        "last_attempt": id_opt(&st.last_attempt),
        "last_attempt_outcome": st.last_attempt_outcome.map(|o| o.as_str()),
        "last_failure_class": st.last_failure_class,
        "last_activity_at": ts(st.last_activity_at),
        "blocked": st.blocked,
        "block": st.block.as_ref().map(|b| json!({
            "claim": b.claim,
            "evidence": ids(&b.evidence),
            "confidence": conf(b.confidence),
            "uncertainty": b.uncertainty,
        })),
        "evidence": ids(&st.evidence),
    })
}

/// Sessions sorted newest first (by start), optionally without the empty
/// ones (capture tests, stray events).
pub fn sessions_sorted(p: &Projection, include_empty: bool) -> Vec<&Session> {
    let mut sessions: Vec<&Session> = p
        .sessions
        .iter()
        .filter(|s| include_empty || s.prompt_count > 0 || s.tool_call_count > 0)
        .collect();
    sessions.sort_by(|a, b| {
        b.started_at
            .cmp(&a.started_at)
            .then(a.session_id.cmp(&b.session_id))
    });
    sessions
}

pub fn turns_of<'a>(p: &'a Projection, s: &Session) -> Vec<&'a Turn> {
    let mut turns: Vec<&Turn> = p.turns_of(s.session_id).collect();
    turns.sort_by_key(|t| t.index);
    turns
}

pub fn attempts_of_turn<'a>(p: &'a Projection, t: &Turn) -> Vec<&'a Attempt> {
    let mut attempts: Vec<&Attempt> = p
        .attempts
        .iter()
        .filter(|a| a.turn_id == t.turn_id)
        .collect();
    attempts.sort_by_key(|a| a.index);
    attempts
}

pub fn status(view: &View) -> Value {
    let st = &view.status;
    let p = view.engine.projection();
    let scoped = view.scoped_capture();
    json!({
        "database": st.source,
        "read_only": st.read_only,
        "snapshot": st.snapshot,
        "capture_mode": st.capture_mode.as_str(),
        "daemon": match &st.daemon {
            crate::store::DaemonState::Running { pid, endpoint, events_ingested } => json!({
                "state": "running", "pid": pid, "endpoint": endpoint, "events_ingested": events_ingested
            }),
            other => json!({ "state": other.state(), "detail": other.label() }),
        },
        "generation": st.generation,
        "segments": st.segments,
        "segment_rows": st.segment_rows,
        "memtable_rows": st.memtable_rows,
        "wal_bytes": st.wal_bytes,
        "spool_pending": st.spool_pending,
        "events": st.events,
        "sessions": st.sessions,
        "captured_events": st.captured_events,
        "reconstructed_events": st.reconstructed_events,
        "last_event_at": ts_opt(st.last_event_at),
        "providers": st.providers.iter().map(|p| json!({
            "provider": p.provider, "events": p.events, "last_event_at": ts_opt(p.last_event_at)
        })).collect::<Vec<_>>(),
        "projects": st.projects.iter().map(|p| json!({
            "project_id": p.project_id.as_ref().map(id), "name": p.name, "events": p.events, "sessions": p.sessions
        })).collect::<Vec<_>>(),
        "import": st.import.as_ref().map(|r| json!({
            "accepted": r.accepted, "duplicates": r.duplicates, "spool_files": r.spool_files
        })),
        "warnings": st.warnings,
        "loaded_at": ts(st.loaded_at),
        "scope": {
            "label": view.scope.label,
            "default_reason": view.scope.default_reason,
            "project_id": view.scope.project_id.as_ref().map(id),
            "project_name": view.scope.project_name,
            "session_id": view.scope.session_id.as_ref().map(id),
            "since": ts_opt(view.scope.since),
            "until": ts_opt(view.scope.until),
            "captured_only": view.scope.captured_only,
            "events": view.engine.event_count(),
            "captured_events": scoped.captured,
            "reconstructed_events": scoped.reconstructed,
            "sessions": p.sessions.len(),
            "turns": p.turns.len(),
            "tool_calls": p.tool_calls.len(),
            "attempts": p.attempts.len(),
            "handoffs": p.handoffs.len(),
        },
        "projection_stats": {
            "events_seen": p.stats.events_seen,
            "out_of_order_events": p.stats.out_of_order_events,
            "unpaired_tool_starts": p.stats.unpaired_tool_starts,
            "unpaired_tool_finishes": p.stats.unpaired_tool_finishes,
            "fifo_pairings": p.stats.fifo_pairings,
            "unknown_events": p.stats.unknown_events,
            "injected_prompts": p.stats.injected_prompts,
        },
        "inference_version": crate::INFERENCE_VERSION,
        "note": crate::TAGLINE,
    })
}

/// `wu_…` — work units have no query-layer prefix helper yet.
pub fn wu_id(id: &WorkUnitId) -> Value {
    Value::String(format!("wu_{id}"))
}

pub fn dec_id(id: &DecisionId) -> Value {
    Value::String(format!("dec_{id}"))
}

pub fn work_unit(w: &WorkUnit) -> Value {
    json!({
        "work_unit_id": wu_id(&w.work_unit_id),
        "project_id": id(&w.project_id),
        "project_name": w.project_name,
        "objective": w.objective,
        "objective_event_id": id_opt(&w.objective_event_id),
        "phase": w.phase.as_str(),
        "phase_reason": w.phase_reason,
        "status": w.status.as_str(),
        "status_reason": w.status_reason,
        "started_at": ts(w.started_at),
        "updated_at": ts(w.updated_at),
        "ended_at": ts_opt(w.ended_at),
        "sessions": ids(&w.sessions),
        "turns": ids(&w.turns),
        "attempts": ids(&w.attempts),
        "paths": w.paths,
        "commit_shas": w.commit_shas,
        "actors": w.actors.iter().map(|p| p.as_str().to_string()).collect::<Vec<_>>(),
        "failure_count": w.failure_count,
        "last_attempt": id_opt(&w.last_attempt),
        "blocking_signal": id_opt(&w.blocking_signal),
        "evidence": ids(&w.evidence),
        "confidence": conf(w.confidence),
        "algorithm_version": w.algorithm_version.as_str(),
        "version": w.version,
    })
}

pub fn decision(d: &Decision) -> Value {
    json!({
        "decision_id": dec_id(&d.decision_id),
        "work_unit_id": d.work_unit_id.as_ref().map(wu_id),
        "session_id": id(&d.session_id),
        "turn_id": id(&d.turn_id),
        "kind": d.kind.as_str(),
        "selected": id(&d.selected),
        "alternatives": ids(&d.alternatives),
        "rationale": d.rationale,
        "rationale_source": d.rationale_source,
        "decided_at": ts(d.decided_at),
        "evidence": ids(&d.evidence),
        "confidence": conf(d.confidence),
        "algorithm_version": d.algorithm_version.as_str(),
    })
}

/// Work units newest first.
pub fn work_units_sorted(p: &Projection) -> Vec<&WorkUnit> {
    let mut list: Vec<&WorkUnit> = p.work_units.iter().collect();
    list.sort_by_key(|a| std::cmp::Reverse(a.updated_at));
    list
}
