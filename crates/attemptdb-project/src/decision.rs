//! Derived decisions (`tier1-v0`, RFC 0003 §5.7).
//!
//! Nothing here is stated by a human; every rationale is assembled from
//! failure classes, tool names/categories and repository-relative paths, and
//! `rationale_source` is always `"derived"`.
//!
//! - **`approach_change`**: for every superseded → superseding attempt pair
//!   (the failed attempt and the retry that touched one of its paths). The
//!   retry is `selected`, the failed attempt the only alternative; evidence
//!   is the failing event and the retry's first action.
//! - **`human_intervention`**: a permission denial (a `PermissionDenied`
//!   event or a tool call ending `denied`) followed, in the same session, by
//!   a call to a *different* tool. The attempt holding the retry is
//!   `selected`; the attempt holding the denied call (when it is a different
//!   one) is the alternative.
//!
//! Confidence is the minimum of the involved attempts' confidence capped at
//! [`crate::workunit::CONFIDENCE_CAP`].

use crate::model::{Attempt, Decision, DecisionKind, Projection, ToolCall, Turn, TurnStatus};
use crate::workunit::CONFIDENCE_CAP;
use attemptdb_core::{
    AttemptId, DecisionId, EventId, OutcomeStatus, SessionId, SpanId, Timestamp, TurnId, WorkUnitId,
};
use std::collections::{HashMap, HashSet};

/// A permission denial observed in a session.
#[derive(Clone, Debug)]
pub(crate) struct Denial {
    pub session_id: SessionId,
    pub event_id: EventId,
    pub at: Timestamp,
    pub tool_name: Option<String>,
    /// The denied tool call, when the denial was its end event.
    pub tool_call_id: Option<SpanId>,
}

const DERIVED: &str = "derived";
const MAX_PATHS: usize = 3;

/// The event that ended a failed attempt: the last failing/denied tool call
/// end in the attempt, else the `TurnFailed` stop event of its turn.
pub(crate) fn failing_event(
    a: &Attempt,
    calls: &HashMap<SpanId, &ToolCall>,
    turn: Option<&Turn>,
) -> Option<EventId> {
    for id in a.tool_call_ids.iter().rev() {
        if let Some(c) = calls.get(id)
            && c.outcome
                .as_ref()
                .is_some_and(|o| matches!(o.status, OutcomeStatus::Failure | OutcomeStatus::Denied))
            && let Some(end) = c.end_event_id
        {
            return Some(end);
        }
    }
    match turn {
        Some(t) if t.status == TurnStatus::Failed => t.stop_event_id,
        _ => None,
    }
}

fn path_list(paths: &[&String]) -> String {
    if paths.is_empty() {
        return "no shared path".to_string();
    }
    let mut s = paths
        .iter()
        .take(MAX_PATHS)
        .map(|p| p.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    if paths.len() > MAX_PATHS {
        s.push_str(&format!(" +{} more", paths.len() - MAX_PATHS));
    }
    s
}

pub(crate) fn derive(
    p: &Projection,
    denials: &[Denial],
    unit_of: &HashMap<AttemptId, WorkUnitId>,
) -> Vec<Decision> {
    let by_id: HashMap<AttemptId, &Attempt> =
        p.attempts.iter().map(|a| (a.attempt_id, a)).collect();
    let calls: HashMap<SpanId, &ToolCall> =
        p.tool_calls.iter().map(|c| (c.tool_call_id, c)).collect();
    let turns: HashMap<TurnId, &Turn> = p.turns.iter().map(|t| (t.turn_id, t)).collect();
    let mut attempt_of_call: HashMap<SpanId, &Attempt> = HashMap::new();
    for a in &p.attempts {
        for id in &a.tool_call_ids {
            attempt_of_call.insert(*id, a);
        }
    }
    let mut out: Vec<Decision> = Vec::new();

    for a in &p.attempts {
        let Some(b) = a.superseded_by.and_then(|id| by_id.get(&id).copied()) else {
            continue;
        };
        let failing = failing_event(a, &calls, turns.get(&a.turn_id).copied())
            .or_else(|| a.evidence.last().copied());
        let first = b
            .tool_call_ids
            .first()
            .and_then(|id| calls.get(id))
            .and_then(|c| c.start_event_id.or(c.end_event_id))
            .or_else(|| b.evidence.first().copied());
        let mut evidence: Vec<EventId> = Vec::new();
        for e in failing.into_iter().chain(first) {
            if !evidence.contains(&e) {
                evidence.push(e);
            }
        }
        if evidence.is_empty() {
            continue;
        }
        let shared: Vec<&String> = a.paths.iter().filter(|x| b.paths.contains(x)).collect();
        let class = a
            .failure_class
            .clone()
            .unwrap_or_else(|| "a failure".to_string());
        let retry = if a.approach == b.approach {
            format!("the same kind of change ({})", b.approach)
        } else {
            format!("a different edit ({})", b.approach)
        };
        out.push(Decision {
            decision_id: DecisionId::derive(&[
                "approach_change",
                &a.attempt_id.to_string(),
                &b.attempt_id.to_string(),
            ]),
            work_unit_id: unit_of.get(&b.attempt_id).copied(),
            session_id: b.session_id,
            turn_id: b.turn_id,
            kind: DecisionKind::ApproachChange,
            selected: b.attempt_id,
            alternatives: vec![a.attempt_id],
            rationale: format!(
                "abandoned approach after {class} on {}; retried with {retry}",
                path_list(&shared)
            ),
            rationale_source: DERIVED.to_string(),
            decided_at: b.started_at,
            evidence,
            confidence: a.confidence.min(b.confidence).min(CONFIDENCE_CAP),
            algorithm_version: Default::default(),
        });
    }

    let mut seen_retry: HashSet<EventId> = HashSet::new();
    for d in denials {
        let retry = p
            .tool_calls
            .iter()
            .filter(|c| c.session_id == d.session_id && Some(c.tool_call_id) != d.tool_call_id)
            .filter_map(|c| c.started_at.or(c.finished_at).map(|t| (t, c)))
            .filter(|(t, _)| *t >= d.at)
            .min_by_key(|(t, c)| (*t, c.tool_call_id));
        let Some((retry_at, retry)) = retry else {
            continue;
        };
        if d.tool_name.as_deref() == Some(retry.tool.name.as_str()) {
            continue;
        }
        let Some(retry_event) = retry.start_event_id.or(retry.end_event_id) else {
            continue;
        };
        if !seen_retry.insert(retry_event) {
            continue;
        }
        let Some(selected) = attempt_of_call.get(&retry.tool_call_id).copied() else {
            continue;
        };
        let alternative = d
            .tool_call_id
            .and_then(|id| attempt_of_call.get(&id).copied())
            .filter(|a| a.attempt_id != selected.attempt_id);
        let confidence = alternative
            .map(|a| a.confidence.min(selected.confidence))
            .unwrap_or(selected.confidence)
            .min(CONFIDENCE_CAP);
        out.push(Decision {
            decision_id: DecisionId::derive(&[
                "human_intervention",
                &d.event_id.to_string(),
                &retry_event.to_string(),
            ]),
            work_unit_id: unit_of.get(&selected.attempt_id).copied(),
            session_id: selected.session_id,
            turn_id: selected.turn_id,
            kind: DecisionKind::HumanIntervention,
            selected: selected.attempt_id,
            alternatives: alternative.map(|a| vec![a.attempt_id]).unwrap_or_default(),
            rationale: format!(
                "permission denied for {}; continued with {} ({})",
                d.tool_name.as_deref().unwrap_or("a tool"),
                retry.tool.name,
                retry.tool.category.as_str()
            ),
            rationale_source: DERIVED.to_string(),
            decided_at: retry_at,
            evidence: vec![d.event_id, retry_event],
            confidence,
            algorithm_version: Default::default(),
        });
    }

    out.sort_by(|a, b| (a.decided_at, a.decision_id).cmp(&(b.decided_at, b.decision_id)));
    out
}
