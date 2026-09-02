//! Time travel and blocked-state explanation.
//!
//! Blocked heuristic (v0), evaluated at a point in time `t`:
//!
//! 1. The session's latest event at or before `t` is a pending-input signal
//!    (`PermissionRequested`, or a `Notification` of type `permission_prompt`,
//!    `idle_prompt` or `agent_needs_input`) with no later event at or before
//!    `t`.
//! 2. The last two attempts that had started by `t` both ended by `t` in a
//!    failure with the same failure class.
//!
//! Confidence is lower when session coverage is not `Full`, and every
//! explanation states what the inference cannot see.

use crate::model::{
    AlgorithmVersion, Attempt, AttemptOutcome, CorrectionStatus, CorrectionTarget, CorrectionType,
    CoverageGrade, Explanation, Phase, ProjectStateSnapshot, Projection, Session, SessionState,
    ToolCall, Turn, TurnStatus, WorkUnit,
};
use crate::workunit;
use attemptdb_core::{EventId, EventKind, SessionId, Timestamp, WorkUnitId};

const SIGNAL_CONFIDENCE_FULL: f32 = 0.85;
const SIGNAL_CONFIDENCE_DEGRADED: f32 = 0.65;
const REPEAT_CONFIDENCE_FULL: f32 = 0.7;
const REPEAT_CONFIDENCE_DEGRADED: f32 = 0.5;
const ALGORITHM_VERSION_NOTE: &str = "tier1-v1, confidence capped at 0.7";

/// Honest description of what the projection could and could not observe for
/// a session.
pub(crate) fn coverage_note(s: &Session) -> String {
    if s.coverage == CoverageGrade::Full {
        return "Coverage is full (session start, session end, prompts and tool calls were all observed), but the inference only sees hook events: anything done outside the hook surface is invisible.".to_string();
    }
    let mut missing: Vec<&str> = Vec::new();
    if s.start_event_id.is_none() {
        missing.push("no session start");
    }
    if s.end_event_id.is_none() {
        missing.push("no session end");
    }
    if s.prompt_count == 0 {
        missing.push("no prompts");
    }
    if s.tool_call_count == 0 {
        missing.push("no tool events");
    }
    format!(
        "Coverage is {} ({}); events may be missing, so the session may have moved on unobserved.",
        s.coverage.as_str(),
        missing.join(", ")
    )
}

fn latest(acc: &mut Timestamp, candidate: Option<Timestamp>, at: Timestamp) {
    if let Some(t) = candidate
        && t <= at
        && t > *acc
    {
        *acc = t;
    }
}

/// Rows of `rows` at the positions the index lists for a session, in
/// table order.
fn rows_at<'a, T>(rows: &'a [T], positions: Option<&'a Vec<u32>>) -> impl Iterator<Item = &'a T> {
    positions
        .map(Vec::as_slice)
        .unwrap_or(&[])
        .iter()
        .map(move |&i| &rows[i as usize])
}

impl Projection {
    pub fn session(&self, session_id: SessionId) -> Option<&Session> {
        self.index()
            .sessions
            .get(&session_id)
            .map(|&i| &self.sessions[i])
    }

    pub fn turns_of(&self, session_id: SessionId) -> impl Iterator<Item = &Turn> {
        rows_at(&self.turns, self.index().turns.get(&session_id))
    }

    pub fn tool_calls_of(&self, session_id: SessionId) -> impl Iterator<Item = &ToolCall> {
        rows_at(&self.tool_calls, self.index().tool_calls.get(&session_id))
    }

    pub fn attempts_of(&self, session_id: SessionId) -> impl Iterator<Item = &Attempt> {
        rows_at(&self.attempts, self.index().attempts.get(&session_id))
    }

    pub fn signals_of(&self, session_id: SessionId) -> impl Iterator<Item = &crate::model::Signal> {
        rows_at(&self.signals, self.index().signals.get(&session_id))
    }

    pub fn work_unit(&self, id: WorkUnitId) -> Option<&WorkUnit> {
        self.index()
            .work_units
            .get(&id)
            .map(|&i| &self.work_units[i])
    }

    /// The work unit an attempt belongs to.
    pub fn work_unit_of_attempt(&self, id: attemptdb_core::AttemptId) -> Option<&WorkUnit> {
        self.index()
            .work_unit_of_attempt
            .get(&id)
            .map(|&i| &self.work_units[i])
    }

    /// Work units as they stood at `at`: only turns, tool calls, attempts,
    /// handoffs and signals observed at or before `at` take part, outcomes
    /// are masked to what was known then (corrections after `at` are
    /// ignored), and idleness is judged against `at`. Retracted entities are
    /// excluded regardless of when the retraction was written.
    pub fn work_units_at(&self, at: Timestamp) -> Vec<WorkUnit> {
        workunit::build(self, Some(at), at)
    }

    /// An attempt's outcome and failure class as of `at` (`None` = as of
    /// the end of the stream): `InProgress` before it ended, `Failed` rather
    /// than `Superseded` before the superseding attempt started, and only
    /// the corrections written at or before `at` applied.
    pub fn attempt_outcome_at(
        &self,
        a: &Attempt,
        at: Option<Timestamp>,
    ) -> (AttemptOutcome, Option<String>) {
        if let Some(t) = at
            && a.ended_at.is_none_or(|e| e > t)
        {
            return (AttemptOutcome::InProgress, None);
        }
        let (mut outcome, mut class) = match a.inferred_outcome {
            Some(o) => (o, a.inferred_failure_class.clone()),
            None => (a.outcome, a.failure_class.clone()),
        };
        if outcome == AttemptOutcome::Superseded
            && let Some(t) = at
        {
            let superseder_started = a
                .superseded_by
                .and_then(|id| self.attempts.iter().find(|x| x.attempt_id == id))
                .map(|x| x.started_at);
            if superseder_started.is_none_or(|st| st > t) {
                outcome = AttemptOutcome::Failed;
            }
        }
        for c in &self.corrections {
            if c.status == CorrectionStatus::Applied
                && c.correction_type == Some(CorrectionType::AttemptOutcome)
                && c.target == Some(CorrectionTarget::Attempt(a.attempt_id))
                && at.is_none_or(|t| c.at <= t)
                && let Some(o) = c.outcome
            {
                class = c
                    .failure_class
                    .clone()
                    .or_else(|| if o.is_failure() { class.clone() } else { None });
                outcome = o;
            }
        }
        (outcome, class)
    }

    /// Why the work unit looks blocked as of the end of the stream, or
    /// `None` when it does not: an uncleared pending-input signal in a member
    /// session (the unit's phase is `Blocked`), else its last two attempts
    /// failing with the same failure class.
    pub fn why_blocked_unit(&self, id: WorkUnitId) -> Option<Explanation> {
        let u = self.work_unit(id)?;
        if u.phase == Phase::Blocked
            && let Some(g) = u
                .blocking_signal
                .and_then(|e| self.signals.iter().find(|g| g.event_id == e))
        {
            let session = self.session(g.session_id);
            let full = session.is_some_and(|s| s.coverage == CoverageGrade::Full);
            let what = match g.kind {
                EventKind::PermissionRequested => "a permission request".to_string(),
                EventKind::Notification => format!(
                    "a `{}` notification",
                    g.signal_type.as_deref().unwrap_or("unknown")
                ),
                other => format!("a `{}` event", other.as_str()),
            };
            return Some(Explanation {
                claim: format!(
                    "Work unit {} is waiting on {} raised at {} in session {} with no later event observed.",
                    u.work_unit_id.short(),
                    what,
                    g.at,
                    g.session_id.short()
                ),
                evidence: vec![g.event_id],
                confidence: if full {
                    SIGNAL_CONFIDENCE_FULL
                } else {
                    SIGNAL_CONFIDENCE_DEGRADED
                },
                uncertainty: format!(
                    "{} A response given outside the hook surface would not be captured, so the wait may already be over. Work-unit membership is itself a heuristic ({ALGORITHM_VERSION_NOTE}).",
                    session.map(coverage_note).unwrap_or_default()
                ),
            });
        }
        let mut members: Vec<&Attempt> = u
            .attempts
            .iter()
            .filter_map(|id| self.attempts.iter().find(|a| a.attempt_id == *id))
            .collect();
        members.sort_by_key(|a| a.started_at);
        if let [.., prev, last] = members.as_slice()
            && prev.ended_at.is_some()
            && last.ended_at.is_some()
            && prev.outcome.is_failure()
            && last.outcome.is_failure()
            && prev.failure_class.is_some()
            && prev.failure_class == last.failure_class
        {
            let class = last.failure_class.clone().unwrap_or_default();
            let mut evidence: Vec<EventId> = Vec::new();
            for e in prev.evidence.iter().chain(last.evidence.iter()) {
                if !evidence.contains(e) {
                    evidence.push(*e);
                }
            }
            let full = u.sessions.iter().all(|s| {
                self.session(*s)
                    .is_some_and(|s| s.coverage == CoverageGrade::Full)
            });
            return Some(Explanation {
                claim: format!(
                    "Work unit {} is repeating itself: its last two attempts ({} and {}) both failed with `{}`.",
                    u.work_unit_id.short(),
                    prev.attempt_id.short(),
                    last.attempt_id.short(),
                    class
                ),
                evidence,
                confidence: if full {
                    REPEAT_CONFIDENCE_FULL
                } else {
                    REPEAT_CONFIDENCE_DEGRADED
                },
                uncertainty: format!(
                    "Failure classes are coarse; two failures with the same class are not necessarily the same problem. Work-unit membership is itself a heuristic ({ALGORITHM_VERSION_NOTE})."
                ),
            });
        }
        None
    }

    /// State of every session active at `at`: started at or before `at` and
    /// not ended before it.
    pub fn state_at(&self, at: Timestamp) -> ProjectStateSnapshot {
        let sessions = self
            .sessions
            .iter()
            .filter(|s| s.started_at <= at && s.ended_at.is_none_or(|e| e >= at))
            .map(|s| self.session_state_at(s, at))
            .collect();
        ProjectStateSnapshot {
            at,
            sessions,
            algorithm_version: AlgorithmVersion::current(),
        }
    }

    /// Why the session looks blocked as of its latest event, or `None` when
    /// it does not.
    pub fn why_blocked(&self, session_id: SessionId) -> Option<Explanation> {
        let s = self.session(session_id)?;
        self.block_at(s, s.last_event_at)
    }

    fn session_state_at(&self, s: &Session, at: Timestamp) -> SessionState {
        let sid = s.session_id;
        let turn = self.turns_of(sid).filter(|t| t.started_at <= at).last();
        let turn_status = turn.map(|t| {
            if t.ended_at.is_some_and(|e| e <= at) {
                t.status
            } else {
                TurnStatus::InProgress
            }
        });

        let in_flight: Vec<&ToolCall> = self
            .tool_calls_of(sid)
            .filter(|c| {
                c.started_at.is_some_and(|st| st <= at) && c.finished_at.is_none_or(|f| f > at)
            })
            .collect();

        let attempt = self.attempts_of(sid).filter(|a| a.started_at <= at).last();
        let masked = attempt.map(|a| self.attempt_outcome_at(a, Some(at)));
        let last_attempt_outcome = masked.as_ref().map(|(o, _)| *o);
        let last_failure_class = match (&masked, attempt) {
            (Some((AttemptOutcome::InProgress, _)), Some(a)) => a.failure_class.clone(),
            (Some((_, class)), _) => class.clone(),
            (None, _) => None,
        };

        let mut last_activity_at = s.started_at;
        latest(&mut last_activity_at, s.ended_at, at);
        for t in self.turns_of(sid) {
            latest(&mut last_activity_at, Some(t.started_at), at);
            latest(&mut last_activity_at, t.ended_at, at);
        }
        for c in self.tool_calls_of(sid) {
            latest(&mut last_activity_at, c.started_at, at);
            latest(&mut last_activity_at, c.finished_at, at);
        }
        for g in self.signals_of(sid) {
            latest(&mut last_activity_at, Some(g.at), at);
        }

        let block = self.block_at(s, at);

        let mut evidence: Vec<EventId> = Vec::new();
        let mut push = |id: Option<EventId>| {
            if let Some(id) = id
                && !evidence.contains(&id)
            {
                evidence.push(id);
            }
        };
        push(turn.and_then(|t| t.prompt_event_id));
        for c in &in_flight {
            push(c.start_event_id);
        }
        if let Some(a) = attempt {
            for e in &a.evidence {
                push(Some(*e));
            }
        }
        if let Some(b) = &block {
            for e in &b.evidence {
                push(Some(*e));
            }
        }

        SessionState {
            session_id: sid,
            provider: s.provider.clone(),
            project_id: s.project_id,
            open: s.ended_at.is_none_or(|e| e > at),
            coverage: s.coverage,
            current_turn: turn.map(|t| t.turn_id),
            turn_index: turn.map(|t| t.index),
            turn_status,
            in_flight_tool_calls: in_flight.iter().map(|c| c.tool_call_id).collect(),
            last_attempt: attempt.map(|a| a.attempt_id),
            last_attempt_outcome,
            last_failure_class,
            last_activity_at,
            blocked: block.is_some(),
            block,
            evidence,
        }
    }

    fn block_at(&self, s: &Session, at: Timestamp) -> Option<Explanation> {
        let full = s.coverage == CoverageGrade::Full;
        let sid = s.session_id;

        // Rule 1: a pending-input signal with no later event.
        let pending = self
            .signals
            .iter()
            .rfind(|g| g.session_id == sid && g.at <= at && g.cleared_at.is_none_or(|c| c > at));
        if let Some(g) = pending {
            let what = match g.kind {
                EventKind::PermissionRequested => "a permission request".to_string(),
                EventKind::Notification => format!(
                    "a `{}` notification",
                    g.signal_type.as_deref().unwrap_or("unknown")
                ),
                other => format!("a `{}` event", other.as_str()),
            };
            return Some(Explanation {
                claim: format!(
                    "Session {} is waiting on {} raised at {} with no later event observed.",
                    sid.short(),
                    what,
                    g.at
                ),
                evidence: vec![g.event_id],
                confidence: if full {
                    SIGNAL_CONFIDENCE_FULL
                } else {
                    SIGNAL_CONFIDENCE_DEGRADED
                },
                uncertainty: format!(
                    "{} A response given outside the hook surface would not be captured, so the wait may already be over.",
                    coverage_note(s)
                ),
            });
        }

        // Rule 2: the last two attempts failed the same way.
        let started: Vec<&Attempt> = self
            .attempts_of(sid)
            .filter(|a| a.started_at <= at)
            .collect();
        if let [.., prev, last] = started.as_slice() {
            let ended = |a: &Attempt| a.ended_at.is_some_and(|e| e <= at);
            if ended(prev)
                && ended(last)
                && prev.outcome.is_failure()
                && last.outcome.is_failure()
                && prev.failure_class.is_some()
                && prev.failure_class == last.failure_class
            {
                let class = last.failure_class.clone().unwrap_or_default();
                let mut evidence: Vec<EventId> = Vec::new();
                for e in prev.evidence.iter().chain(last.evidence.iter()) {
                    if !evidence.contains(e) {
                        evidence.push(*e);
                    }
                }
                return Some(Explanation {
                    claim: format!(
                        "Session {} is repeating itself: its last two attempts (turn {} #{} and turn {} #{}) both failed with `{}`.",
                        sid.short(),
                        prev.turn_index,
                        prev.index,
                        last.turn_index,
                        last.index,
                        class
                    ),
                    evidence,
                    confidence: if full {
                        REPEAT_CONFIDENCE_FULL
                    } else {
                        REPEAT_CONFIDENCE_DEGRADED
                    },
                    uncertainty: format!(
                        "{} Failure classes are coarse; two failures with the same class are not necessarily the same problem.",
                        coverage_note(s)
                    ),
                });
            }
        }
        None
    }
}
