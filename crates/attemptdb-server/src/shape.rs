//! JSON shapes of the read API, field for field the shapes the local UI's
//! `/api/` serves (readable ids such as `att_…`, RFC 3339 timestamps,
//! evidence ids inline), plus what only a server can add: which
//! computation an inference came from.
//!
//! Every inference object — attempt, handoff, work unit, decision,
//! explanation, session state — carries `computed_by`, `algorithm_version`,
//! `evidence` and `confidence`. A device-computed item keeps every field
//! the device uploaded; only the *encoding* of known id and timestamp
//! fields is normalised to the server's (prefixed ids, RFC 3339) so both
//! computations read the same way. Values are never taken from both.

use crate::engine::{SessionFacts, TenantView};
use crate::merge::DeviceItem;
use attemptdb_core::Timestamp;
use attemptdb_project::{
    ALGORITHM_VERSION, Attempt, Decision, Explanation, Handoff, Projection, Session, SessionState,
    ToolCall, Turn, WorkUnit,
};
use attemptdb_query::PrefixedId;
use serde_json::{Map, Value, json};
use uuid::Uuid;

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

pub fn elapsed_ms(start: Timestamp, end: Timestamp) -> u64 {
    (end.as_millis() - start.as_millis()).max(0) as u64
}

fn server_stamp(mut v: Value) -> Value {
    if let Some(obj) = v.as_object_mut() {
        obj.insert("computed_by".into(), json!("server"));
    }
    v
}

pub fn tool_call(tc: &ToolCall) -> Value {
    let duration = tc
        .duration_ms
        .or_else(|| match (tc.started_at, tc.finished_at) {
            (Some(s), Some(e)) => Some(elapsed_ms(s, e)),
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

/// The server's attempt, as the UI renders it.
pub fn attempt(a: &Attempt, p: &Projection, with_tools: bool) -> Value {
    let mut v = json!({
        "kind": "attempt",
        "attempt_id": id(&a.attempt_id),
        "session_id": id(&a.session_id),
        "turn_id": id(&a.turn_id),
        "turn_index": a.turn_index,
        "index": a.index,
        "objective": a.objective,
        "approach": a.approach,
        "started_at": ts(a.started_at),
        "ended_at": ts_opt(a.ended_at),
        "duration_ms": a.ended_at.map(|e| elapsed_ms(a.started_at, e)),
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
        "work_unit_id": id_opt(&a.work_unit_id),
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
    server_stamp(v)
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

pub fn session(
    s: &Session,
    facts: SessionFacts,
    attempt_count: usize,
    turns: Option<Vec<Value>>,
) -> Value {
    let mut v = json!({
        "session_id": id(&s.session_id),
        "provider": s.provider.as_str(),
        "provider_name": s.provider.display_name(),
        "provider_session_id": s.provider_session_id,
        "project_id": id(&s.project_id),
        "project_name": s.project_name,
        "device_id": id(&facts.device_id),
        "state": if s.ended_at.is_some() { "closed" } else { "open" },
        "started_at": ts(s.started_at),
        "ended_at": ts_opt(s.ended_at),
        "last_event_at": ts(s.last_event_at),
        "end_reason": s.end_reason,
        "start_source": s.start_source,
        "event_count": s.event_count,
        "turn_count": s.turn_count,
        "prompt_count": s.prompt_count,
        "tool_call_count": s.tool_call_count,
        "attempt_count": attempt_count,
        "failure_count": s.failure_count,
        "agents": ids(&s.agents),
        "coverage": s.coverage.as_str(),
        "captured_events": facts.captured,
        "reconstructed_events": facts.reconstructed,
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
    server_stamp(json!({
        "kind": "handoff",
        "handoff_id": format!("{}:{}", id(&h.from_session).as_str().unwrap_or_default(), id(&h.to_session).as_str().unwrap_or_default()),
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
        "algorithm_version": ALGORITHM_VERSION,
    }))
}

pub fn work_unit(w: &WorkUnit) -> Value {
    server_stamp(json!({
        "kind": "work_unit",
        "work_unit_id": id(&w.work_unit_id),
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
    }))
}

pub fn decision(d: &Decision) -> Value {
    server_stamp(json!({
        "kind": "decision",
        "decision_id": id(&d.decision_id),
        "work_unit_id": id_opt(&d.work_unit_id),
        "session_id": id(&d.session_id),
        "turn_id": id(&d.turn_id),
        "decision_kind": d.kind.as_str(),
        "selected": id(&d.selected),
        "alternatives": ids(&d.alternatives),
        "rationale": d.rationale,
        "rationale_source": d.rationale_source,
        "decided_at": ts(d.decided_at),
        "evidence": ids(&d.evidence),
        "confidence": conf(d.confidence),
        "algorithm_version": d.algorithm_version.as_str(),
    }))
}

pub fn explanation(e: &Explanation) -> Value {
    server_stamp(json!({
        "claim": e.claim,
        "evidence": ids(&e.evidence),
        "confidence": conf(e.confidence),
        "uncertainty": e.uncertainty,
        "algorithm_version": ALGORITHM_VERSION,
    }))
}

pub fn session_state(st: &SessionState) -> Value {
    server_stamp(json!({
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
        "block": st.block.as_ref().map(explanation),
        "evidence": ids(&st.evidence),
        "algorithm_version": ALGORITHM_VERSION,
    }))
}

// ---------------------------------------------------------------------------
// Device-computed items
// ---------------------------------------------------------------------------

/// How a known field of an uploaded projection row is encoded.
#[derive(Clone, Copy)]
enum Enc {
    Id(&'static str),
    IdList(&'static str),
    Time,
    TimeOpt,
}

/// Known id and timestamp fields of the four uploaded kinds
/// (`attemptdb_project::model`), by name. Unknown fields pass through.
fn encoding(kind: &str, key: &str) -> Option<Enc> {
    let common = match key {
        "session_id" | "from_session" | "to_session" => Some(Enc::Id("ses_")),
        "project_id" => Some(Enc::Id("prj_")),
        "turn_id" => Some(Enc::Id("trn_")),
        "attempt_id" | "superseded_by" | "supersedes" | "last_attempt" | "selected" => {
            Some(Enc::Id("att_"))
        }
        "work_unit_id" => Some(Enc::Id("wu_")),
        "decision_id" => Some(Enc::Id("dec_")),
        "event_id" | "objective_event_id" | "blocking_signal" => Some(Enc::Id("ev_")),
        "tool_call_ids" => Some(Enc::IdList("spn_")),
        "sessions" => Some(Enc::IdList("ses_")),
        "turns" => Some(Enc::IdList("trn_")),
        "attempts" | "alternatives" => Some(Enc::IdList("att_")),
        "started_at" | "updated_at" | "decided_at" | "at" => Some(Enc::Time),
        "ended_at" => Some(Enc::TimeOpt),
        _ => None,
    };
    match (kind, key) {
        // A work unit's `attempts` is the member list; an attempt has none.
        ("attempt", "attempts") => None,
        _ => common,
    }
}

fn readable_uuid(prefix: &str, v: &Value) -> Value {
    match v.as_str().and_then(|s| Uuid::parse_str(s.trim()).ok()) {
        Some(u) => Value::String(format!("{prefix}{}", u.hyphenated())),
        None => v.clone(),
    }
}

fn rfc3339(v: &Value) -> Value {
    match v.as_i64() {
        Some(us) => Value::String(Timestamp::from_micros(us).to_rfc3339()),
        None => v.clone(),
    }
}

/// Re-encode the known id and timestamp fields of an uploaded row the way
/// the server renders its own; every value stays the device's.
pub fn device_fields(kind: &str, fields: &Value) -> Value {
    let Some(obj) = fields.as_object() else {
        return fields.clone();
    };
    let mut out = Map::with_capacity(obj.len());
    for (k, v) in obj {
        let encoded = match encoding(kind, k) {
            Some(Enc::Id(prefix)) => readable_uuid(prefix, v),
            Some(Enc::IdList(prefix)) => match v.as_array() {
                Some(list) => Value::Array(list.iter().map(|x| readable_uuid(prefix, x)).collect()),
                None => v.clone(),
            },
            Some(Enc::Time) | Some(Enc::TimeOpt) => rfc3339(v),
            None if k == "corrected" && v.is_object() => device_fields(kind, v),
            None => v.clone(),
        };
        out.insert(k.clone(), encoded);
    }
    Value::Object(out)
}

/// A device-computed inference as the read API returns it: the device's
/// row (re-encoded, never re-computed) with its own provenance.
pub fn device_item(item: &DeviceItem) -> Value {
    let mut v = device_fields(&item.kind, &item.fields);
    let obj = match v.as_object_mut() {
        Some(o) => o,
        None => {
            v = json!({});
            v.as_object_mut().expect("object")
        }
    };
    obj.insert("kind".into(), Value::String(item.kind.clone()));
    obj.insert("computed_by".into(), json!("device"));
    obj.insert("device_id".into(), id(&item.device_id));
    obj.insert(
        "algorithm_version".into(),
        Value::String(item.algorithm_version.clone()),
    );
    obj.insert(
        "evidence".into(),
        Value::Array(
            item.evidence
                .iter()
                .map(|e| readable_uuid("ev_", &Value::String(e.clone())))
                .collect(),
        ),
    );
    obj.insert("confidence".into(), json!(item.confidence));
    obj.insert("computed_at".into(), rfc3339(&item.computed_at));
    obj.insert("received_at".into(), rfc3339(&item.received_at));
    v
}

// ---------------------------------------------------------------------------
// Sorting helpers shared by the handlers
// ---------------------------------------------------------------------------

/// Sessions newest first (by start), ties by id.
pub fn sessions_sorted(p: &Projection) -> Vec<&Session> {
    let mut sessions: Vec<&Session> = p.sessions.iter().collect();
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

/// Work units newest activity first, ties by id.
pub fn work_units_sorted(p: &Projection) -> Vec<&WorkUnit> {
    let mut list: Vec<&WorkUnit> = p.work_units.iter().collect();
    list.sort_by(|a, b| {
        b.updated_at
            .cmp(&a.updated_at)
            .then(a.work_unit_id.cmp(&b.work_unit_id))
    });
    list
}

pub fn session_facts(view: &TenantView, s: &Session) -> SessionFacts {
    view.sessions
        .get(&s.session_id)
        .copied()
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use attemptdb_core::DeviceId;

    #[test]
    fn device_rows_are_re_encoded_not_re_computed() {
        let dev = DeviceId::derive(&["shape", "d"]);
        let item = DeviceItem {
            device_id: dev,
            kind: "attempt".into(),
            id: "0192a7c4-2b3e-7f10-8d4a-0e1f2a3b4c5d".into(),
            algorithm_version: "tier1-v3".into(),
            evidence: vec![
                "0192a7c4-2b3f-7a11-9c2b-1f2e3d4c5b6a".into(),
                "not-an-id".into(),
            ],
            confidence: 0.75,
            session_id: None,
            project_id: None,
            fields: json!({
                "attempt_id": "0192a7c4-2b3e-7f10-8d4a-0e1f2a3b4c5d",
                "session_id": "b1a7e6d4-2c3f-5e1a-8b9c-0d1e2f3a4b5c",
                "started_at": 1_756_368_000_123_456i64,
                "ended_at": null,
                "outcome": "failed",
                "approach": "edit src/x.rs",
                "tool_call_ids": ["0192a7c4-2b3e-7f10-8d4a-0e1f2a3b4c00"],
                "corrected": { "event_id": "0192a7c4-2b3e-7f10-8d4a-0e1f2a3b4c01", "at": 1_756_368_000_000_000i64, "correction_type": "attempt_note" },
                "new_in_v3": { "anything": 1 },
            }),
            computed_at: json!(1_756_368_000_000_000i64),
            received_at: json!(1_756_368_001_000_000i64),
        };
        let v = device_item(&item);
        assert_eq!(v["computed_by"], "device");
        assert_eq!(v["kind"], "attempt");
        assert_eq!(v["algorithm_version"], "tier1-v3");
        assert_eq!(v["confidence"], 0.75);
        assert_eq!(v["device_id"], id(&dev));
        assert_eq!(v["attempt_id"], "att_0192a7c4-2b3e-7f10-8d4a-0e1f2a3b4c5d");
        assert_eq!(v["session_id"], "ses_b1a7e6d4-2c3f-5e1a-8b9c-0d1e2f3a4b5c");
        assert_eq!(v["started_at"], "2025-08-28T08:00:00.123456Z");
        assert!(v["ended_at"].is_null());
        assert_eq!(v["outcome"], "failed");
        assert_eq!(v["approach"], "edit src/x.rs");
        assert_eq!(
            v["tool_call_ids"][0],
            "spn_0192a7c4-2b3e-7f10-8d4a-0e1f2a3b4c00"
        );
        assert_eq!(
            v["corrected"]["event_id"],
            "ev_0192a7c4-2b3e-7f10-8d4a-0e1f2a3b4c01"
        );
        assert_eq!(v["corrected"]["at"], "2025-08-28T08:00:00.000000Z");
        assert_eq!(v["new_in_v3"]["anything"], 1, "unknown fields pass through");
        assert_eq!(v["evidence"][0], "ev_0192a7c4-2b3f-7a11-9c2b-1f2e3d4c5b6a");
        assert_eq!(v["evidence"][1], "not-an-id");
        assert_eq!(v["received_at"], "2025-08-28T08:00:01.000000Z");
        assert!(
            v.get("duration_ms").is_none(),
            "nothing derived on the server"
        );
    }

    #[test]
    fn a_work_units_member_list_is_ids_but_an_attempts_is_not() {
        let wu = device_fields(
            "work_unit",
            &json!({ "attempts": ["0192a7c4-2b3e-7f10-8d4a-0e1f2a3b4c5d"], "updated_at": 0 }),
        );
        assert_eq!(
            wu["attempts"][0],
            "att_0192a7c4-2b3e-7f10-8d4a-0e1f2a3b4c5d"
        );
        assert_eq!(wu["updated_at"], "1970-01-01T00:00:00.000000Z");
        let att = device_fields("attempt", &json!({ "attempts": ["x"] }));
        assert_eq!(att["attempts"][0], "x");
        assert_eq!(device_fields("attempt", &json!("scalar")), json!("scalar"));
    }

    #[test]
    fn confidence_renders_short() {
        assert_eq!(conf(0.9), json!(0.9));
        assert_eq!(conf(0.65), json!(0.65));
    }
}
