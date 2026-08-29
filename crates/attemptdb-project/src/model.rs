//! Projected entities.
//!
//! Every struct here is serialisable so the query layer can expose it as a
//! table. Fields that name events are the *evidence* for the entity; consumers
//! must never present a projected value without being able to point back to
//! those events.

use crate::ALGORITHM_VERSION;
use attemptdb_core::event::Provider;
use attemptdb_core::{
    AgentId, AttemptId, EventId, EventKind, Outcome, PortablePath, ProjectId, SessionId, SpanId,
    Timestamp, ToolRef, TurnId,
};
use serde::{Deserialize, Deserializer, Serialize};
use std::fmt;
use std::ops::Deref;

/// Identifier of a projected tool call. Tool calls are spans, so the span id
/// type is reused rather than introducing a parallel identifier.
pub type ToolCallId = SpanId;

/// The algorithm version stamp carried by projected entities.
///
/// Wraps a `&'static str` (always [`ALGORITHM_VERSION`] for values produced
/// by this build). Serialises as a plain string; deserialising accepts only
/// this build's own version, because projections are derived data that must
/// be rebuilt, not migrated, when the algorithm changes. A newtype is used
/// because serde's derive treats a bare `&str` field as borrowed input.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct AlgorithmVersion(pub &'static str);

impl AlgorithmVersion {
    /// The version of this build.
    pub const fn current() -> Self {
        Self(ALGORITHM_VERSION)
    }

    pub const fn as_str(self) -> &'static str {
        self.0
    }
}

impl Default for AlgorithmVersion {
    fn default() -> Self {
        Self::current()
    }
}

impl Deref for AlgorithmVersion {
    type Target = str;
    fn deref(&self) -> &str {
        self.0
    }
}

impl PartialEq<str> for AlgorithmVersion {
    fn eq(&self, other: &str) -> bool {
        self.0 == other
    }
}

impl PartialEq<&str> for AlgorithmVersion {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}

impl fmt::Display for AlgorithmVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

impl fmt::Debug for AlgorithmVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.0)
    }
}

impl Serialize for AlgorithmVersion {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.0)
    }
}

impl<'de> Deserialize<'de> for AlgorithmVersion {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        if s == ALGORITHM_VERSION {
            Ok(Self::current())
        } else {
            Err(serde::de::Error::custom(format!(
                "unsupported projection algorithm version `{s}` (this build produces `{ALGORITHM_VERSION}`); re-project the event stream"
            )))
        }
    }
}

/// How much of a session's lifecycle the event stream actually shows.
///
/// - `Full`: session start, session end, at least one prompt and at least one
///   tool event were all observed.
/// - `Partial`: some lifecycle context (start or end) plus some activity
///   (prompts or tool events) was observed, but something is missing.
/// - `Minimal`: only activity was observed (only tool events, only prompts, or
///   both) with no session start or end at all.
/// - `Unknown`: no prompts and no tool events; there is nothing to project a
///   turn from (for example a session that only produced notifications).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CoverageGrade {
    Full,
    Partial,
    Minimal,
    Unknown,
}

impl CoverageGrade {
    pub fn as_str(self) -> &'static str {
        match self {
            CoverageGrade::Full => "full",
            CoverageGrade::Partial => "partial",
            CoverageGrade::Minimal => "minimal",
            CoverageGrade::Unknown => "unknown",
        }
    }
}

/// One agent session, grouped by `session_id`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    pub session_id: SessionId,
    pub provider: Provider,
    /// Provider-native session identifier, kept for joins against raw events.
    pub provider_session_id: String,
    pub project_id: ProjectId,
    pub project_name: String,
    /// `SessionStarted.observed_at` when observed, else the first event.
    pub started_at: Timestamp,
    /// `SessionEnded.observed_at`; `None` while the session is open (or when
    /// the end was never observed).
    pub ended_at: Option<Timestamp>,
    /// Content-free end reason reported by the provider, when present.
    pub end_reason: Option<String>,
    /// Content-free start source (`startup`, `resume`, ...), when present.
    pub start_source: Option<String>,
    pub event_count: u64,
    /// Turns including the implicit pre-prompt turn, when present.
    pub turn_count: u32,
    /// `PromptSubmitted` events observed.
    pub prompt_count: u32,
    pub tool_call_count: u32,
    /// Tool calls that ended in `failure`/`denied` plus `TurnFailed` events.
    pub failure_count: u32,
    /// Distinct agent ids in order of first appearance (nil ids skipped).
    pub agents: Vec<AgentId>,
    pub coverage: CoverageGrade,
    pub first_event_id: EventId,
    pub last_event_id: EventId,
    /// `observed_at` of the last event in the session.
    pub last_event_at: Timestamp,
    /// The `SessionStarted` event, when observed.
    pub start_event_id: Option<EventId>,
    /// The `SessionEnded` event, when observed.
    pub end_event_id: Option<EventId>,
}

/// Outcome of a turn as far as the stream shows.
///
/// - `Completed`: a `TurnStopped` was observed.
/// - `Failed`: a `TurnFailed` was observed.
/// - `InProgress`: the turn is still open at the end of the stream.
/// - `Unknown`: the turn was cut by the next prompt or the session end
///   without any stop event.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnStatus {
    Completed,
    Failed,
    InProgress,
    Unknown,
}

impl TurnStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            TurnStatus::Completed => "completed",
            TurnStatus::Failed => "failed",
            TurnStatus::InProgress => "in_progress",
            TurnStatus::Unknown => "unknown",
        }
    }
}

/// One human prompt and everything the agent did in response.
///
/// Index `0` is reserved for the *implicit* turn that holds tool events seen
/// before any prompt in the session; prompt-initiated turns are numbered from
/// `1`. `turn_id` is `TurnId::derive(&[session_id, index])`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Turn {
    pub turn_id: TurnId,
    pub session_id: SessionId,
    pub index: u32,
    pub started_at: Timestamp,
    pub ended_at: Option<Timestamp>,
    /// `None` for the implicit turn.
    pub prompt_event_id: Option<EventId>,
    /// The `TurnStopped` / `TurnFailed` event that ended the turn. When several
    /// were observed (a stop hook that continued the turn), the last one.
    pub stop_event_id: Option<EventId>,
    pub status: TurnStatus,
    /// Tool calls attributed to the turn, in order of first observation.
    pub tool_call_ids: Vec<ToolCallId>,
    /// Prompt text when content was captured; `None` in `metadata_only` mode
    /// and for the implicit turn.
    pub objective: Option<String>,
    /// Prompt length in characters, from metadata when available, else from
    /// the captured text.
    pub prompt_chars: Option<u64>,
    /// First and last event attributed to the turn.
    pub first_event_id: EventId,
    pub last_event_id: EventId,
}

/// A tool invocation, paired from its start and end events.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    pub tool_call_id: ToolCallId,
    pub session_id: SessionId,
    /// The turn current when the call was first observed.
    pub turn_id: Option<TurnId>,
    pub agent_id: AgentId,
    pub tool: ToolRef,
    /// `None` when only the end of the call was observed.
    pub started_at: Option<Timestamp>,
    /// `None` while the call is in flight (start observed, no end).
    pub finished_at: Option<Timestamp>,
    /// Provider-reported duration when present, else derived from timestamps.
    pub duration_ms: Option<u64>,
    /// Outcome from the end event. A `ToolCallFinished` without an explicit
    /// outcome is treated as success; a `ToolCallFailed` without one as an
    /// unclassified failure.
    pub outcome: Option<Outcome>,
    /// Union of the paths reported on the start and end events.
    pub paths: Vec<PortablePath>,
    pub start_event_id: Option<EventId>,
    pub end_event_id: Option<EventId>,
}

/// Outcome of an attempt.
///
/// - `Succeeded`: the turn stopped normally and the attempt was not ended by
///   a failure.
/// - `Failed`: a file-mutating or shell call failed (or the turn failed).
/// - `Abandoned`: the turn was cut by the next prompt or the session end
///   without a stop.
/// - `Superseded`: a failed attempt that a later attempt retried on at least
///   one of the same paths.
/// - `InProgress`: the turn is still open.
/// - `Unknown`: the turn stopped but the attempt contains no tool call.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptOutcome {
    Succeeded,
    Failed,
    Abandoned,
    Superseded,
    InProgress,
    Unknown,
}

impl AttemptOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            AttemptOutcome::Succeeded => "succeeded",
            AttemptOutcome::Failed => "failed",
            AttemptOutcome::Abandoned => "abandoned",
            AttemptOutcome::Superseded => "superseded",
            AttemptOutcome::InProgress => "in_progress",
            AttemptOutcome::Unknown => "unknown",
        }
    }

    /// Whether the attempt ended in a failure, including failures that were
    /// later superseded by a retry.
    pub fn is_failure(self) -> bool {
        matches!(self, AttemptOutcome::Failed | AttemptOutcome::Superseded)
    }
}

/// One approach toward a turn's objective.
///
/// `attempt_id` is `AttemptId::derive(&[session_id, turn_index, index])`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Attempt {
    pub attempt_id: AttemptId,
    pub session_id: SessionId,
    pub turn_id: TurnId,
    /// Index of the owning turn (see [`Turn::index`]).
    pub turn_index: u32,
    /// Position within the turn, from `0`.
    pub index: u32,
    /// The turn's prompt text when content was captured.
    pub objective: Option<String>,
    /// Content-free summary built from tool categories and repository
    /// relative paths, e.g. `edit src/lib.rs · shell ×3 · read ×2`.
    pub approach: String,
    pub started_at: Timestamp,
    pub ended_at: Option<Timestamp>,
    pub outcome: AttemptOutcome,
    /// Content-free failure classification when the attempt failed (kept
    /// when the attempt was later superseded).
    pub failure_class: Option<String>,
    pub tool_call_ids: Vec<ToolCallId>,
    /// Repository-relative (else logical) paths touched, in first-touch order.
    pub paths: Vec<String>,
    pub superseded_by: Option<AttemptId>,
    pub supersedes: Option<AttemptId>,
    /// Events this attempt was derived from: the prompt, every tool call
    /// start/end, and the stop event for the turn's last attempt.
    pub evidence: Vec<EventId>,
    /// `0.9` with call-id pairing and an explicit stop, `0.6` with FIFO or
    /// unpaired calls or a missing stop, `0.4` when coverage is minimal or
    /// unknown.
    pub confidence: f32,
    #[serde(default)]
    pub algorithm_version: AlgorithmVersion,
}

/// A session of one provider taking over from a session of another provider
/// in the same project.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Handoff {
    pub from_session: SessionId,
    pub to_session: SessionId,
    pub from_provider: Provider,
    pub to_provider: Provider,
    pub project_id: ProjectId,
    /// Start of the receiving session.
    pub at: Timestamp,
    /// Time between the giving session's last activity and `at`.
    pub gap_ms: u64,
    /// Paths touched by both sessions, sorted.
    pub shared_paths: Vec<String>,
    pub evidence: Vec<EventId>,
    /// `0.8` when the sessions share a path within 30 minutes, `0.5` when the
    /// receiving session merely starts within 5 minutes.
    pub confidence: f32,
}

/// Kind of a causal relation between two projected entities or events.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeKind {
    ParentOf,
    Caused,
    Triggered,
    Blocked,
    Resolved,
    Superseded,
    Produced,
    Verified,
    Contradicted,
    HandedOff,
    EvidenceFor,
}

impl EdgeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            EdgeKind::ParentOf => "parent_of",
            EdgeKind::Caused => "caused",
            EdgeKind::Triggered => "triggered",
            EdgeKind::Blocked => "blocked",
            EdgeKind::Resolved => "resolved",
            EdgeKind::Superseded => "superseded",
            EdgeKind::Produced => "produced",
            EdgeKind::Verified => "verified",
            EdgeKind::Contradicted => "contradicted",
            EdgeKind::HandedOff => "handed_off",
            EdgeKind::EvidenceFor => "evidence_for",
        }
    }
}

/// One end of a causal edge.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum EdgeEndpoint {
    Event(EventId),
    Span(SpanId),
    Turn(TurnId),
    Attempt(AttemptId),
    Session(SessionId),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CausalEdge {
    pub from: EdgeEndpoint,
    pub to: EdgeEndpoint,
    pub kind: EdgeKind,
    pub evidence: Vec<EventId>,
}

/// An event that leaves the session waiting on a human until a later event
/// arrives: `PermissionRequested`, or a `Notification` whose type is
/// `permission_prompt`, `idle_prompt` or `agent_needs_input`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Signal {
    pub session_id: SessionId,
    pub event_id: EventId,
    pub at: Timestamp,
    pub kind: EventKind,
    /// Notification type for `Notification` signals.
    pub signal_type: Option<String>,
    /// `observed_at` of the next event in the session, which ends the wait;
    /// `None` when the signal is the session's latest event.
    pub cleared_at: Option<Timestamp>,
    pub cleared_by: Option<EventId>,
}

/// Counters describing how the stream was consumed.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectionStats {
    pub events_seen: u64,
    /// Events whose ordering key was smaller than the previous pushed event's
    /// (the projector sorted them; this only reports that it had to).
    pub out_of_order_events: u64,
    /// `ToolCallStarted` events that never received an end event.
    pub unpaired_tool_starts: u64,
    /// `ToolCallFinished`/`ToolCallFailed` events with no matching start.
    pub unpaired_tool_finishes: u64,
    /// Pairings that fell back to FIFO by `(agent, tool name)`.
    pub fifo_pairings: u64,
    /// Events of kind `Unknown`.
    pub unknown_events: u64,
    /// `PromptSubmitted` events that were client-injected notifications, not
    /// human prompts (skipped when opening turns).
    #[serde(default)]
    pub injected_prompts: u64,
}

/// The complete Tier 1 projection of an event stream.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Projection {
    #[serde(default)]
    pub algorithm_version: AlgorithmVersion,
    /// Sorted by `(started_at, session_id)`.
    pub sessions: Vec<Session>,
    /// Grouped by session (in session order), then by index.
    pub turns: Vec<Turn>,
    /// Grouped by session, then in order of first observation.
    pub tool_calls: Vec<ToolCall>,
    /// Grouped by session, then by `(turn index, attempt index)`.
    pub attempts: Vec<Attempt>,
    /// Sorted by `(at, to_session)`.
    pub handoffs: Vec<Handoff>,
    pub edges: Vec<CausalEdge>,
    /// Pending-input signals, grouped by session in event order.
    pub signals: Vec<Signal>,
    pub stats: ProjectionStats,
}

/// A claim about a session with the evidence it rests on.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Explanation {
    pub claim: String,
    pub evidence: Vec<EventId>,
    pub confidence: f32,
    /// Honest statement of what the inference cannot see.
    pub uncertainty: String,
}

/// State of one session as of a point in time.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionState {
    pub session_id: SessionId,
    pub provider: Provider,
    pub project_id: ProjectId,
    /// `true` unless a `SessionEnded` at or before the snapshot time was
    /// observed.
    pub open: bool,
    pub coverage: CoverageGrade,
    /// The latest turn that had started by the snapshot time.
    pub current_turn: Option<TurnId>,
    pub turn_index: Option<u32>,
    /// The turn's status as of the snapshot time (`InProgress` until its end
    /// was observed).
    pub turn_status: Option<TurnStatus>,
    /// Tool calls started but not finished as of the snapshot time.
    pub in_flight_tool_calls: Vec<ToolCallId>,
    /// The latest attempt that had started by the snapshot time.
    pub last_attempt: Option<AttemptId>,
    /// Its outcome as of the snapshot time: `InProgress` until the attempt
    /// ended, and `Failed` rather than `Superseded` while the superseding
    /// attempt had not yet started.
    pub last_attempt_outcome: Option<AttemptOutcome>,
    pub last_failure_class: Option<String>,
    /// Latest projected timestamp at or before the snapshot time
    /// (approximate: derived from projected entities, not the raw event
    /// index).
    pub last_activity_at: Timestamp,
    pub blocked: bool,
    pub block: Option<Explanation>,
    pub evidence: Vec<EventId>,
}

/// Snapshot of every session active at a point in time; the basis of
/// `STATE ... AT <timestamp>`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProjectStateSnapshot {
    pub at: Timestamp,
    pub sessions: Vec<SessionState>,
    #[serde(default)]
    pub algorithm_version: AlgorithmVersion,
}
