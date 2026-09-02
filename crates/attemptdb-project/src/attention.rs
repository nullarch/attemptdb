//! "Needs You": the high-precision queue of work that is waiting on a human.
//!
//! Everything here is inference, not fact: each item carries the events it
//! was derived from, a confidence, an honest statement of what the rule
//! cannot see, and [`ALGORITHM_VERSION`](crate::ALGORITHM_VERSION).
//!
//! Precision is the product requirement (`docs/agent-timeline-ui.md` §8.4):
//! a queue that fills with ordinary events is a queue nobody reads. Only
//! four situations qualify, in this order:
//!
//! 1. [`AttentionKind::PermissionGate`] — an uncleared permission request
//!    (or `permission_prompt` notification) in an open session.
//! 2. [`AttentionKind::InputRequest`] — an uncleared `idle_prompt` /
//!    `agent_needs_input` notification in an open session.
//! 3. [`AttentionKind::RepeatedFailure`] — an open work unit whose last two
//!    attempts failed with the same failure class and were not superseded by
//!    a successful attempt.
//! 4. [`AttentionKind::WorkConflict`] — two open work units editing the same
//!    paths at the same time.
//!
//! Deliberately *not* attention: a completed turn, an idle session, a single
//! failed tool call, a signal that a later event already cleared, and
//! anything in a session that has ended — nobody can act on a session that
//! is over.

use crate::model::{
    AlgorithmVersion, Attempt, AttemptOutcome, Conflict, CoverageGrade, Projection, Session,
    Signal, WorkUnit, WorkUnitStatus,
};
use crate::state::coverage_note;
use attemptdb_core::event::Provider;
use attemptdb_core::{EventId, EventKind, ProjectId, SessionId, Timestamp, WorkUnitId};
use serde::{Deserialize, Serialize};

/// Items below this confidence never reach the queue.
pub const DEFAULT_MIN_CONFIDENCE: f32 = 0.5;

const GATE_CONFIDENCE_FULL: f32 = 0.85;
const GATE_CONFIDENCE_DEGRADED: f32 = 0.65;
const REPEAT_CONFIDENCE_FULL: f32 = 0.7;
const REPEAT_CONFIDENCE_DEGRADED: f32 = 0.5;

/// Why an item is in the queue. The order of the variants is the ranking.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttentionKind {
    /// An agent asked for permission and nothing has happened since.
    PermissionGate,
    /// An agent asked a question, or went idle waiting for input.
    InputRequest,
    /// The same failure class twice with no successful attempt after it.
    RepeatedFailure,
    /// Two open work units are editing the same paths concurrently.
    WorkConflict,
}

impl AttentionKind {
    pub fn as_str(self) -> &'static str {
        match self {
            AttentionKind::PermissionGate => "permission_gate",
            AttentionKind::InputRequest => "input_request",
            AttentionKind::RepeatedFailure => "repeated_failure",
            AttentionKind::WorkConflict => "work_conflict",
        }
    }

    /// 1 (most urgent) to 4, as listed in the module docs.
    pub fn rank(self) -> u8 {
        match self {
            AttentionKind::PermissionGate => 1,
            AttentionKind::InputRequest => 2,
            AttentionKind::RepeatedFailure => 3,
            AttentionKind::WorkConflict => 4,
        }
    }

    pub const ALL: &'static [AttentionKind] = &[
        AttentionKind::PermissionGate,
        AttentionKind::InputRequest,
        AttentionKind::RepeatedFailure,
        AttentionKind::WorkConflict,
    ];

    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim().to_ascii_lowercase();
        Self::ALL.iter().copied().find(|k| k.as_str() == s)
    }
}

/// One thing a person has to do, with the evidence behind the claim.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AttentionItem {
    /// Stable within one projection: `atn_<kind>_<primary evidence>`. Used
    /// as a UI key and to remember dismissals; not a stored entity id.
    pub attention_id: String,
    pub kind: AttentionKind,
    pub rank: u8,
    /// What the human is being asked to do, in one sentence.
    pub action: String,
    pub project_id: ProjectId,
    pub project_name: String,
    pub session_id: Option<SessionId>,
    pub provider: Option<Provider>,
    pub work_unit_id: Option<WorkUnitId>,
    /// Notification type for an input request, when the provider named one.
    pub signal_type: Option<String>,
    /// Failure class for a repeated failure.
    pub failure_class: Option<String>,
    /// When the wait started: the signal, the last failed attempt's end, or
    /// the first shared edit of a conflict.
    pub since: Timestamp,
    /// How long it has been waiting at the evaluation time.
    pub waiting_ms: u64,
    /// Why the item was classified this way.
    pub claim: String,
    /// What the rule cannot see.
    pub uncertainty: String,
    pub evidence: Vec<EventId>,
    pub confidence: f32,
    #[serde(default)]
    pub algorithm_version: AlgorithmVersion,
}

impl AttentionItem {
    /// Ordering key: rank, then longest wait, then highest confidence.
    fn sort_key(&self) -> (u8, i64, i32, String) {
        (
            self.rank,
            self.since.as_micros(),
            -((self.confidence * 1000.0) as i32),
            self.attention_id.clone(),
        )
    }
}

fn id_for(kind: AttentionKind, primary: &str) -> String {
    format!("atn_{}_{}", kind.as_str(), primary)
}

fn provider_label(p: &Provider) -> String {
    p.display_name().to_string()
}

impl Projection {
    /// The Needs You queue as of `at`, highest priority first.
    ///
    /// `min_confidence` drops low-confidence items; pass
    /// [`DEFAULT_MIN_CONFIDENCE`] unless the caller has a reason.
    pub fn attention_at(&self, at: Timestamp, min_confidence: f32) -> Vec<AttentionItem> {
        let mut items: Vec<AttentionItem> = Vec::new();
        for s in &self.sessions {
            // A session that has ended cannot be unblocked by a human.
            if s.ended_at.is_some() {
                continue;
            }
            if let Some(item) = self.signal_item(s, at) {
                items.push(item);
            }
        }
        for u in &self.work_units {
            if let Some(item) = self.repeated_failure_item(u, at) {
                items.push(item);
            }
        }
        for c in &self.conflicts {
            if let Some(item) = self.conflict_item(c, at) {
                items.push(item);
            }
        }
        items.retain(|i| i.confidence >= min_confidence);
        items.sort_by(|a, b| a.sort_key().cmp(&b.sort_key()));
        items
    }

    /// [`Projection::attention_at`] at the latest observed event time — the
    /// answer for a stream that is not being watched live.
    pub fn attention(&self) -> Vec<AttentionItem> {
        let at = self
            .sessions
            .iter()
            .map(|s| s.last_event_at)
            .max()
            .unwrap_or_else(Timestamp::now);
        self.attention_at(at, DEFAULT_MIN_CONFIDENCE)
    }

    /// Rules 1 and 2: an uncleared pending-input signal in an open session.
    fn signal_item(&self, s: &Session, at: Timestamp) -> Option<AttentionItem> {
        let g: &Signal = self
            .signals_of(s.session_id)
            .filter(|g| g.at <= at && g.cleared_at.is_none_or(|c| c > at))
            .last()?;
        let full = s.coverage == CoverageGrade::Full;
        let signal_type = match g.kind {
            EventKind::PermissionRequested => Some("permission_request".to_string()),
            _ => g.signal_type.clone(),
        };
        let kind = match (g.kind, signal_type.as_deref()) {
            (EventKind::PermissionRequested, _) | (_, Some("permission_prompt")) => {
                AttentionKind::PermissionGate
            }
            (_, Some("idle_prompt")) | (_, Some("agent_needs_input")) => {
                AttentionKind::InputRequest
            }
            // The projector only raises signals for the kinds above; anything
            // else is not precise enough to interrupt a person for.
            _ => return None,
        };
        let what = match kind {
            AttentionKind::PermissionGate => "a permission request".to_string(),
            _ => format!(
                "a `{}` notification",
                signal_type.as_deref().unwrap_or("unknown")
            ),
        };
        let action = match kind {
            AttentionKind::PermissionGate => format!(
                "Approve or deny the permission request in the {} session on {}.",
                provider_label(&s.provider),
                s.project_name
            ),
            _ => format!(
                "Answer the {} session on {}: it is waiting for input.",
                provider_label(&s.provider),
                s.project_name
            ),
        };
        Some(AttentionItem {
            attention_id: id_for(kind, &g.event_id.short()),
            kind,
            rank: kind.rank(),
            action,
            project_id: s.project_id,
            project_name: s.project_name.clone(),
            session_id: Some(s.session_id),
            provider: Some(s.provider.clone()),
            work_unit_id: self.work_unit_of_session(s.session_id),
            signal_type,
            failure_class: None,
            since: g.at,
            waiting_ms: waited(g.at, at),
            claim: format!(
                "Session {} raised {} at {} and no later event was observed.",
                s.session_id.short(),
                what,
                g.at
            ),
            uncertainty: format!(
                "{} A response given outside the hook surface would not be captured, so the wait may already be over.",
                coverage_note(s)
            ),
            evidence: vec![g.event_id],
            confidence: if full {
                GATE_CONFIDENCE_FULL
            } else {
                GATE_CONFIDENCE_DEGRADED
            },
            algorithm_version: AlgorithmVersion::current(),
        })
    }

    /// Rule 3: an open work unit whose last two attempts failed the same way
    /// with nothing successful after them.
    fn repeated_failure_item(&self, u: &WorkUnit, at: Timestamp) -> Option<AttentionItem> {
        if u.status != WorkUnitStatus::Open {
            return None;
        }
        let mut members: Vec<&Attempt> = u
            .attempts
            .iter()
            .filter_map(|id| self.attempt(*id))
            .filter(|a| a.started_at <= at)
            .collect();
        members.sort_by_key(|a| (a.started_at, a.attempt_id));
        let [.., prev, last] = members.as_slice() else {
            return None;
        };
        let ended = |a: &Attempt| a.ended_at.is_some_and(|e| e <= at);
        if !(ended(prev)
            && ended(last)
            && prev.outcome.is_failure()
            && last.outcome.is_failure()
            && prev.failure_class.is_some()
            && prev.failure_class == last.failure_class)
        {
            return None;
        }
        // "with no successful superseding attempt": a retry that worked ends
        // the loop even when the two failures are still the last two by
        // start time.
        if self.superseded_by_success(last) || self.superseded_by_success(prev) {
            return None;
        }
        let class = last.failure_class.clone().unwrap_or_default();
        let mut evidence: Vec<EventId> = Vec::new();
        for e in prev.evidence.iter().chain(last.evidence.iter()) {
            if !evidence.contains(e) {
                evidence.push(*e);
            }
        }
        let full = u
            .sessions
            .iter()
            .all(|s| self.session(*s).is_some_and(|s| s.coverage == CoverageGrade::Full));
        let since = last.ended_at.unwrap_or(last.started_at);
        let session = u.sessions.last().and_then(|s| self.session(*s));
        Some(AttentionItem {
            attention_id: id_for(AttentionKind::RepeatedFailure, &last.attempt_id.short()),
            kind: AttentionKind::RepeatedFailure,
            rank: AttentionKind::RepeatedFailure.rank(),
            action: format!(
                "Decide how to break the loop on {}: two attempts in a row failed with `{}` and nothing has superseded them.",
                if u.paths.is_empty() {
                    format!("work unit {}", u.work_unit_id.short())
                } else {
                    u.paths.join(", ")
                },
                class
            ),
            project_id: u.project_id,
            project_name: u.project_name.clone(),
            session_id: session.map(|s| s.session_id),
            provider: session.map(|s| s.provider.clone()),
            work_unit_id: Some(u.work_unit_id),
            signal_type: None,
            failure_class: Some(class.clone()),
            since,
            waiting_ms: waited(since, at),
            claim: format!(
                "Work unit {} is repeating itself: attempts {} and {} both failed with `{}`.",
                u.work_unit_id.short(),
                prev.attempt_id.short(),
                last.attempt_id.short(),
                class
            ),
            uncertainty: format!(
                "Failure classes are coarse, so two failures with the same class are not necessarily the same problem, and work-unit membership is itself a heuristic ({}). {}",
                crate::ALGORITHM_VERSION,
                session.map(coverage_note).unwrap_or_default()
            ),
            evidence,
            confidence: if full {
                REPEAT_CONFIDENCE_FULL
            } else {
                REPEAT_CONFIDENCE_DEGRADED
            },
            algorithm_version: AlgorithmVersion::current(),
        })
    }

    /// Rule 4: two open work units editing the same paths at the same time.
    fn conflict_item(&self, c: &Conflict, at: Timestamp) -> Option<AttentionItem> {
        if c.started_at > at {
            return None;
        }
        let first = self.work_unit(c.first)?;
        let second = self.work_unit(c.second)?;
        if first.status != WorkUnitStatus::Open || second.status != WorkUnitStatus::Open {
            return None;
        }
        let uncommitted = c
            .paths
            .iter()
            .all(|p| !p.first_committed && !p.second_committed);
        let names: Vec<&str> = c.paths.iter().map(|p| p.path.as_str()).collect();
        Some(AttentionItem {
            attention_id: id_for(AttentionKind::WorkConflict, &c.conflict_id.short()),
            kind: AttentionKind::WorkConflict,
            rank: AttentionKind::WorkConflict.rank(),
            action: format!(
                "Reconcile two open work units editing {} at the same time.",
                names.join(", ")
            ),
            project_id: c.project_id,
            project_name: first.project_name.clone(),
            session_id: first.sessions.last().copied(),
            provider: first
                .sessions
                .last()
                .and_then(|s| self.session(*s))
                .map(|s| s.provider.clone()),
            work_unit_id: Some(c.first),
            signal_type: None,
            failure_class: None,
            since: c.started_at,
            waiting_ms: waited(c.started_at, at),
            claim: format!(
                "Work units {} and {} edited {} shared path(s) within the concurrency window{}.",
                c.first.short(),
                c.second.short(),
                c.paths.len(),
                if uncommitted {
                    ", and neither side has committed since"
                } else {
                    ""
                }
            ),
            uncertainty: format!(
                "Concurrency is inferred from edit times, not from the working tree: the two sides may already be the same change, or one may have been discarded ({}).",
                crate::ALGORITHM_VERSION
            ),
            evidence: c.evidence.clone(),
            confidence: c.confidence,
            algorithm_version: AlgorithmVersion::current(),
        })
    }

    fn attempt(&self, id: attemptdb_core::AttemptId) -> Option<&Attempt> {
        self.attempts.iter().find(|a| a.attempt_id == id)
    }

    /// Whether this attempt was superseded by one that did not fail.
    fn superseded_by_success(&self, a: &Attempt) -> bool {
        a.superseded_by
            .and_then(|id| self.attempt(id))
            .is_some_and(|next| {
                matches!(
                    next.outcome,
                    AttemptOutcome::Succeeded | AttemptOutcome::InProgress
                )
            })
    }

    fn work_unit_of_session(&self, id: SessionId) -> Option<WorkUnitId> {
        self.work_units
            .iter()
            .filter(|u| u.sessions.contains(&id))
            .max_by_key(|u| u.updated_at)
            .map(|u| u.work_unit_id)
    }
}

fn waited(since: Timestamp, at: Timestamp) -> u64 {
    let d = at.as_micros().saturating_sub(since.as_micros());
    (d.max(0) / 1_000) as u64
}
