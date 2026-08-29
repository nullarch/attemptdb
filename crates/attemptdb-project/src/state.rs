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
    AlgorithmVersion, Attempt, AttemptOutcome, CoverageGrade, Explanation, ProjectStateSnapshot,
    Projection, Session, SessionState, ToolCall, Turn, TurnStatus,
};
use attemptdb_core::{EventId, EventKind, SessionId, Timestamp};

const SIGNAL_CONFIDENCE_FULL: f32 = 0.85;
const SIGNAL_CONFIDENCE_DEGRADED: f32 = 0.65;
const REPEAT_CONFIDENCE_FULL: f32 = 0.7;
const REPEAT_CONFIDENCE_DEGRADED: f32 = 0.5;

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

impl Projection {
    pub fn session(&self, session_id: SessionId) -> Option<&Session> {
        self.sessions.iter().find(|s| s.session_id == session_id)
    }

    pub fn turns_of(&self, session_id: SessionId) -> impl Iterator<Item = &Turn> {
        self.turns
            .iter()
            .filter(move |t| t.session_id == session_id)
    }

    pub fn tool_calls_of(&self, session_id: SessionId) -> impl Iterator<Item = &ToolCall> {
        self.tool_calls
            .iter()
            .filter(move |c| c.session_id == session_id)
    }

    pub fn attempts_of(&self, session_id: SessionId) -> impl Iterator<Item = &Attempt> {
        self.attempts
            .iter()
            .filter(move |a| a.session_id == session_id)
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
        let last_attempt_outcome = attempt.map(|a| {
            if a.ended_at.is_none_or(|e| e > at) {
                return AttemptOutcome::InProgress;
            }
            if a.outcome == AttemptOutcome::Superseded {
                let superseder_started = a
                    .superseded_by
                    .and_then(|id| self.attempts.iter().find(|x| x.attempt_id == id))
                    .map(|x| x.started_at);
                if superseder_started.is_none_or(|st| st > at) {
                    return AttemptOutcome::Failed;
                }
            }
            a.outcome
        });

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
        for g in self.signals.iter().filter(|g| g.session_id == sid) {
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
            last_failure_class: attempt.and_then(|a| a.failure_class.clone()),
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
