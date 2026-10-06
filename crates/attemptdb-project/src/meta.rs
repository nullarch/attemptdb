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
//!   calls: re-splitting the turn would merge the siblings around the hole.
//!   Sibling attempts keep their ids (an id comes from the attempt's own
//!   evidence, so it would survive a re-split anyway); a `superseded_by`
//!   pointer to the retracted attempt is cleared and the pointing attempt
//!   reverts to `Failed`. Whatever was built from the attempt's events and
//!   cites them — edges, handoffs — goes with it, and a retraction with
//!   reason `privacy` also blanks the text of the attempt's turn.
//!
//! An id written against the positional scheme of `tier1-v4` and earlier
//! (`session, turn index, position`) is still honoured: when no attempt
//! carries it, it is resolved by position under today's rules and the
//! correction or retraction is marked `legacy_position`.

use crate::model::{
    Attempt, AttemptOutcome, Correction, CorrectionRef, CorrectionStatus, CorrectionTarget,
    CorrectionType, EdgeEndpoint, Projection, ProjectionStats, RetractedSet, Retraction,
    RetractionReason, RetractionTarget, RetractionTargetType, ToolCall, Turn,
};
use crate::projector::{MetaObs, Obs};
use attemptdb_core::{AttemptId, EventId, OutcomeStatus, SessionId, SpanId, TurnId};
use std::collections::{HashMap, HashSet};

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
        legacy_position: false,
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
        legacy_position: false,
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

/// Where `id` names an attempt: by its own id, else by the positional id the
/// attempt would have had before `tier1-v5`. The flag is whether the
/// positional fallback was needed.
fn locate(attempts: &[Attempt], id: AttemptId) -> Option<(usize, bool)> {
    attempts
        .iter()
        .position(|a| a.attempt_id == id)
        .map(|i| (i, false))
        .or_else(|| {
            attempts
                .iter()
                .position(|a| a.legacy_position_id() == id)
                .map(|i| (i, true))
        })
}

/// Sessions retracted for the reason `privacy`: the text they carried is
/// hidden from the retracted rows too.
pub(crate) fn privacy_sessions(retractions: &[Retraction]) -> HashSet<SessionId> {
    retractions
        .iter()
        .filter(|r| r.reason == RetractionReason::Privacy)
        .filter_map(|r| match r.target {
            Some(RetractionTarget::Session(id)) => Some(id),
            _ => None,
        })
        .collect()
}

/// Remove retracted attempts from the projection (see the module docs).
/// Returns the ids (current, after positional resolution) of the attempts
/// removed for the reason `privacy`.
pub(crate) fn retract_attempts(
    p: &mut Projection,
    retractions: &mut [Retraction],
    ids: &mut RetractedSet,
    stats: &mut ProjectionStats,
) -> HashSet<AttemptId> {
    let targets: Vec<AttemptId> = ids.attempts.clone();
    let mut all_removed: HashSet<EventId> = HashSet::new();
    let mut privacy_removed: HashSet<AttemptId> = HashSet::new();
    for named in targets {
        let Some((pos, _)) = locate(&p.attempts, named) else {
            continue;
        };
        let mut attempt = p.attempts.remove(pos);
        let id = attempt.attempt_id;
        // The two ids this attempt answers to: its own, and the positional
        // one an older retraction may have used.
        let legacy_id = attempt.legacy_position_id();
        let names_it = |r: &Retraction| matches!(r.target, Some(RetractionTarget::Attempt(t)) if t == id || t == legacy_id);
        ids.insert_attempt(id);
        let privacy = retractions
            .iter()
            .any(|r| r.reason == RetractionReason::Privacy && names_it(r));
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
                all_removed.insert(e);
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
            if names_it(r) {
                r.matched = true;
                r.legacy_position |=
                    r.target == Some(RetractionTarget::Attempt(legacy_id)) && legacy_id != id;
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
        let calls_left: HashMap<SpanId, &ToolCall> =
            p.tool_calls.iter().map(|c| (c.tool_call_id, c)).collect();
        for t in p.turns.iter_mut().filter(|t| t.turn_id == attempt.turn_id) {
            t.tool_call_ids.retain(|c| !call_ids.contains(c));
            // A turn must not keep naming a removed event as its first or
            // last: fall back to what is left of it.
            let survivor = |t: &Turn| {
                t.stop_event_id
                    .or_else(|| {
                        t.tool_call_ids
                            .iter()
                            .rev()
                            .find_map(|c| calls_left.get(c))
                            .and_then(|c| c.end_event_id.or(c.start_event_id))
                    })
                    .or(t.prompt_event_id)
            };
            if removed_events.contains(&t.last_event_id)
                && let Some(e) = survivor(t)
            {
                t.last_event_id = e;
            }
            if removed_events.contains(&t.first_event_id)
                && let Some(e) = t.prompt_event_id.or_else(|| survivor(t))
            {
                t.first_event_id = e;
            }
            if privacy {
                t.objective = None;
                t.inferred_objective = None;
            }
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
            if privacy && b.turn_id == attempt.turn_id {
                b.objective = None;
            }
        }
        let touches = |e: &EdgeEndpoint| match e {
            EdgeEndpoint::Attempt(a) => *a == id,
            EdgeEndpoint::Span(s) => call_ids.contains(s),
            EdgeEndpoint::Event(ev) => removed_events.contains(ev),
            _ => false,
        };
        p.edges.retain(|e| !touches(&e.from) && !touches(&e.to));
        if privacy {
            attempt.objective = None;
            attempt.note = None;
            privacy_removed.insert(id);
        }
        p.retracted.attempts.push(attempt);
        p.retracted.tool_calls.extend(removed);
    }
    if !all_removed.is_empty() {
        // What was derived from the removed events and cites them goes too:
        // a handoff whose evidence is a retracted tool call, and the edges
        // built on it.
        let cites = |ev: &[EventId]| ev.iter().any(|e| all_removed.contains(e));
        p.handoffs.retain(|h| !cites(&h.evidence));
        p.edges.retain(|e| !cites(&e.evidence));
    }
    privacy_removed
}

/// Apply corrections in stream order to the projected attempts and turns.
/// `privacy` lists attempts retracted for the reason `privacy`: a correction
/// aimed at one reports `target_retracted` and its text is dropped.
pub(crate) fn apply_corrections(
    corrections: &mut [Correction],
    attempts: &mut [Attempt],
    turns: &mut [Turn],
    retracted: &RetractedSet,
    privacy: &HashSet<AttemptId>,
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
                match locate(attempts, id) {
                    Some((i, legacy)) => {
                        let a = &mut attempts[i];
                        c.legacy_position = legacy;
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
                    None => missing_attempt(c, id, retracted, privacy),
                }
            }
            (CorrectionType::AttemptNote, CorrectionTarget::Attempt(id)) => {
                match locate(attempts, id) {
                    // A note that was not stored (`metadata_only`) has
                    // nothing to attach.
                    Some(_) if c.note.is_none() => CorrectionStatus::ContentUnavailable,
                    Some((i, legacy)) => {
                        let a = &mut attempts[i];
                        c.legacy_position = legacy;
                        a.note = c.note.clone();
                        a.corrected = Some(reference);
                        CorrectionStatus::Applied
                    }
                    None => missing_attempt(c, id, retracted, privacy),
                }
            }
            (CorrectionType::TurnObjective, CorrectionTarget::Turn(id)) => {
                match turns.iter_mut().find(|t| t.turn_id == id) {
                    // The new objective is the note; without it (not stored
                    // under `metadata_only`) there is nothing to apply and
                    // the turn is left exactly as it was.
                    Some(_) if c.note.is_none() => CorrectionStatus::ContentUnavailable,
                    Some(t) => {
                        if t.corrected.is_none() {
                            t.inferred_objective = t.objective.clone();
                        }
                        t.objective = c.note.clone();
                        for a in attempts.iter_mut().filter(|a| a.turn_id == id) {
                            a.objective = c.note.clone();
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

/// The status of a correction whose attempt is not in the projection, and
/// the hygiene of the text it carried.
fn missing_attempt(
    c: &mut Correction,
    id: AttemptId,
    retracted: &RetractedSet,
    privacy: &HashSet<AttemptId>,
) -> CorrectionStatus {
    if retracted.contains_attempt(&id) {
        if privacy.contains(&id) {
            c.note = None;
        }
        CorrectionStatus::TargetRetracted
    } else {
        CorrectionStatus::TargetNotFound
    }
}
