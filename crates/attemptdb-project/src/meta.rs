//! Corrections and retractions (RFC 0003 §8).
//!
//! Both are canonical events written by AttemptDB itself
//! (`provider = "attemptdb"`). They describe the log rather than the work, so
//! the projector splits them off before grouping sessions and applies them
//! afterwards, in stream order:
//!
//! - A **correction** overrides one projected value. `attempt_outcome`
//!   replaces an attempt's outcome (and failure class), `attempt_note`
//!   attaches a note, `turn_objective` replaces a turn's objective and that
//!   of its attempts. The latest applied correction wins; the projection's
//!   own value is kept alongside (`inferred_outcome`, `inferred_objective`).
//! - A **retraction** removes a session, an event, or an attempt from every
//!   projection. Sessions and events are removed *before* projecting (the
//!   remaining events are projected as if the retracted ones never
//!   happened, so a retracted prompt merges its turn into the previous one,
//!   and retracting the only evidence of an attempt removes that attempt).
//!   Attempts are removed *after* projecting, together with their tool
//!   calls, because attempt ids are positional and re-splitting the turn
//!   would let the retracted id reappear on a different set of calls.
//!   Sibling attempts keep their ids; a `superseded_by` pointer to the
//!   retracted attempt is cleared and the pointing attempt reverts to
//!   `Failed`.

use crate::model::{
    Attempt, AttemptOutcome, Correction, CorrectionRef, CorrectionStatus, CorrectionTarget,
    CorrectionType, EdgeEndpoint, Projection, ProjectionStats, RetractedSet, Retraction,
    RetractionReason, RetractionTarget, RetractionTargetType, Turn,
};
use crate::projector::{MetaObs, Obs};
use attemptdb_core::{AttemptId, EventId, OutcomeStatus, SessionId, SpanId, TurnId};
use std::collections::HashSet;

fn parse_outcome(s: &str) -> Option<AttemptOutcome> {
    match s.trim().to_ascii_lowercase().as_str() {
        "succeeded" | "success" => Some(AttemptOutcome::Succeeded),
        "failed" | "failure" => Some(AttemptOutcome::Failed),
        "abandoned" => Some(AttemptOutcome::Abandoned),
        "superseded" => Some(AttemptOutcome::Superseded),
        _ => None,
    }
}

/// Outcomes a correction may set.
pub const CORRECTABLE_OUTCOMES: &[&str] = &["succeeded", "failed", "abandoned", "superseded"];

fn parse_correction_target(text: &str, ty: Option<CorrectionType>) -> Option<CorrectionTarget> {
    let t = text.trim();
    if let Some(rest) = t.strip_prefix("att_") {
        return rest
            .parse::<AttemptId>()
            .ok()
            .map(CorrectionTarget::Attempt);
    }
    if let Some(rest) = t.strip_prefix("trn_") {
        return rest.parse::<TurnId>().ok().map(CorrectionTarget::Turn);
    }
    if let Some(rest) = t.strip_prefix("ses_") {
        return rest
            .parse::<SessionId>()
            .ok()
            .map(CorrectionTarget::Session);
    }
    match ty? {
        CorrectionType::AttemptOutcome | CorrectionType::AttemptNote => {
            t.parse::<AttemptId>().ok().map(CorrectionTarget::Attempt)
        }
        CorrectionType::TurnObjective => t.parse::<TurnId>().ok().map(CorrectionTarget::Turn),
    }
}

pub(crate) fn parse_correction(o: &Obs) -> Correction {
    let m: MetaObs = o.meta.clone().unwrap_or_default();
    let correction_type = m.correction_type.as_deref().and_then(CorrectionType::parse);
    let target_text = m.target.clone().unwrap_or_default();
    let target = parse_correction_target(&target_text, correction_type);
    let outcome = m.outcome.as_deref().and_then(parse_outcome);
    let status = if correction_type.is_none() || target.is_none() {
        CorrectionStatus::Invalid
    } else {
        // Provisional; `apply_corrections` decides.
        CorrectionStatus::TargetNotFound
    };
    Correction {
        event_id: o.event_id,
        at: o.at,
        session_id: o.session_id,
        project_id: o.project_id,
        correction_type,
        target,
        target_text,
        outcome,
        failure_class: m.failure_class,
        note: m.note,
        note_chars: m.note_chars,
        status,
    }
}

fn parse_retraction_target(
    text: &str,
    ty: Option<RetractionTargetType>,
) -> Option<RetractionTarget> {
    let t = text.trim();
    match ty {
        Some(RetractionTargetType::Session) => {
            t.parse::<SessionId>().ok().map(RetractionTarget::Session)
        }
        Some(RetractionTargetType::Event) => t.parse::<EventId>().ok().map(RetractionTarget::Event),
        Some(RetractionTargetType::Attempt) => {
            t.parse::<AttemptId>().ok().map(RetractionTarget::Attempt)
        }
        None => {
            if let Some(rest) = t.strip_prefix("ses_") {
                rest.parse::<SessionId>()
                    .ok()
                    .map(RetractionTarget::Session)
            } else if let Some(rest) = t.strip_prefix("ev_") {
                rest.parse::<EventId>().ok().map(RetractionTarget::Event)
            } else if let Some(rest) = t.strip_prefix("att_") {
                rest.parse::<AttemptId>()
                    .ok()
                    .map(RetractionTarget::Attempt)
            } else {
                None
            }
        }
    }
}

pub(crate) fn parse_retraction(o: &Obs) -> Retraction {
    let m: MetaObs = o.meta.clone().unwrap_or_default();
    let declared = m
        .target_type
        .as_deref()
        .and_then(RetractionTargetType::parse);
    let target_text = m.target.clone().unwrap_or_default();
    let target = parse_retraction_target(&target_text, declared);
    Retraction {
        event_id: o.event_id,
        at: o.at,
        project_id: o.project_id,
        target_type: declared.or_else(|| target.map(RetractionTarget::target_type)),
        target,
        target_text,
        reason: m
            .reason
            .as_deref()
            .map(RetractionReason::parse)
            .unwrap_or(RetractionReason::Other),
        note: m.note,
        note_chars: m.note_chars,
        matched: false,
        retracted_events: 0,
    }
}

/// The ids every well-formed retraction names, whether or not anything
/// loaded matches them.
pub(crate) fn retracted_set(retractions: &[Retraction]) -> RetractedSet {
    let mut set = RetractedSet::default();
    for r in retractions {
        match r.target {
            Some(RetractionTarget::Session(id)) => set.insert_session(id),
            Some(RetractionTarget::Event(id)) => set.insert_event(id),
            Some(RetractionTarget::Attempt(id)) => set.insert_attempt(id),
            None => {}
        }
    }
    set
}

pub(crate) fn note_session_match(retractions: &mut [Retraction], sid: SessionId) {
    for r in retractions {
        if r.target == Some(RetractionTarget::Session(sid)) {
            r.matched = true;
            r.retracted_events += 1;
        }
    }
}

pub(crate) fn note_event_match(retractions: &mut [Retraction], eid: EventId) {
    for r in retractions {
        if r.target == Some(RetractionTarget::Event(eid)) {
            r.matched = true;
            r.retracted_events += 1;
        }
    }
}

/// Remove retracted attempts from the projection (see the module docs).
pub(crate) fn retract_attempts(
    p: &mut Projection,
    retractions: &mut [Retraction],
    ids: &mut RetractedSet,
    stats: &mut ProjectionStats,
) {
    let targets: Vec<AttemptId> = ids.attempts.clone();
    for id in targets {
        let Some(pos) = p.attempts.iter().position(|a| a.attempt_id == id) else {
            continue;
        };
        let attempt = p.attempts.remove(pos);
        let call_ids: HashSet<SpanId> = attempt.tool_call_ids.iter().copied().collect();
        let (removed, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut p.tool_calls)
            .into_iter()
            .partition(|c| call_ids.contains(&c.tool_call_id));
        p.tool_calls = kept;

        let mut removed_events: Vec<EventId> = Vec::new();
        let mut removed_failures = 0u32;
        for c in &removed {
            for e in c.start_event_id.into_iter().chain(c.end_event_id) {
                ids.insert_event(e);
                removed_events.push(e);
            }
            if c.outcome
                .as_ref()
                .is_some_and(|o| matches!(o.status, OutcomeStatus::Failure | OutcomeStatus::Denied))
            {
                removed_failures += 1;
            }
        }
        stats.retracted_events += removed_events.len() as u64;
        for r in retractions.iter_mut() {
            if r.target == Some(RetractionTarget::Attempt(id)) {
                r.matched = true;
                r.retracted_events += removed_events.len() as u64;
            }
        }
        if let Some(s) = p
            .sessions
            .iter_mut()
            .find(|s| s.session_id == attempt.session_id)
        {
            s.tool_call_count = s.tool_call_count.saturating_sub(removed.len() as u32);
            s.failure_count = s.failure_count.saturating_sub(removed_failures);
        }
        for t in p.turns.iter_mut().filter(|t| t.turn_id == attempt.turn_id) {
            t.tool_call_ids.retain(|c| !call_ids.contains(c));
        }
        for b in p.attempts.iter_mut() {
            if b.superseded_by == Some(id) {
                b.superseded_by = None;
                if b.outcome == AttemptOutcome::Superseded {
                    b.outcome = AttemptOutcome::Failed;
                }
            }
            if b.supersedes == Some(id) {
                b.supersedes = None;
            }
        }
        let touches = |e: &EdgeEndpoint| match e {
            EdgeEndpoint::Attempt(a) => *a == id,
            EdgeEndpoint::Span(s) => call_ids.contains(s),
            EdgeEndpoint::Event(ev) => removed_events.contains(ev),
            _ => false,
        };
        p.edges.retain(|e| !touches(&e.from) && !touches(&e.to));
        p.retracted.attempts.push(attempt);
        p.retracted.tool_calls.extend(removed);
    }
}

/// Apply corrections in stream order to the projected attempts and turns.
pub(crate) fn apply_corrections(
    corrections: &mut [Correction],
    attempts: &mut [Attempt],
    turns: &mut [Turn],
    retracted: &RetractedSet,
    stats: &mut ProjectionStats,
) {
    for c in corrections.iter_mut() {
        if c.status == CorrectionStatus::Invalid {
            continue;
        }
        let (Some(ty), Some(target)) = (c.correction_type, c.target) else {
            c.status = CorrectionStatus::Invalid;
            continue;
        };
        let reference = CorrectionRef {
            event_id: c.event_id,
            at: c.at,
            correction_type: ty,
        };
        c.status = match (ty, target) {
            (CorrectionType::AttemptOutcome, CorrectionTarget::Attempt(id)) => {
                let Some(outcome) = c.outcome else {
                    c.status = CorrectionStatus::Invalid;
                    continue;
                };
                match attempts.iter_mut().find(|a| a.attempt_id == id) {
                    Some(a) => {
                        if a.inferred_outcome.is_none() {
                            a.inferred_outcome = Some(a.outcome);
                            a.inferred_failure_class = a.failure_class.clone();
                        }
                        a.failure_class = c.failure_class.clone().or_else(|| {
                            if outcome.is_failure() {
                                a.failure_class.clone()
                            } else {
                                None
                            }
                        });
                        a.outcome = outcome;
                        if c.note.is_some() {
                            a.note = c.note.clone();
                        }
                        a.corrected = Some(reference);
                        CorrectionStatus::Applied
                    }
                    None if retracted.contains_attempt(&id) => CorrectionStatus::TargetRetracted,
                    None => CorrectionStatus::TargetNotFound,
                }
            }
            (CorrectionType::AttemptNote, CorrectionTarget::Attempt(id)) => {
                match attempts.iter_mut().find(|a| a.attempt_id == id) {
                    Some(a) => {
                        a.note = c.note.clone();
                        a.corrected = Some(reference);
                        CorrectionStatus::Applied
                    }
                    None if retracted.contains_attempt(&id) => CorrectionStatus::TargetRetracted,
                    None => CorrectionStatus::TargetNotFound,
                }
            }
            (CorrectionType::TurnObjective, CorrectionTarget::Turn(id)) => {
                match turns.iter_mut().find(|t| t.turn_id == id) {
                    Some(t) => {
                        if t.corrected.is_none() {
                            t.inferred_objective = t.objective.clone();
                        }
                        if c.note.is_some() {
                            t.objective = c.note.clone();
                            for a in attempts.iter_mut().filter(|a| a.turn_id == id) {
                                a.objective = c.note.clone();
                            }
                        }
                        t.corrected = Some(reference);
                        CorrectionStatus::Applied
                    }
                    None => CorrectionStatus::TargetNotFound,
                }
            }
            _ => CorrectionStatus::Invalid,
        };
        if c.status == CorrectionStatus::Applied {
            stats.corrections_applied += 1;
        }
    }
}
