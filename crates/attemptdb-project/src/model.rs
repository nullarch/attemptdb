//! Projected entities.
//!
//! Every struct here is serialisable so the query layer can expose it as a
//! table. Fields that name events are the *evidence* for the entity; consumers
//! must never present a projected value without being able to point back to
//! those events.

use crate::ALGORITHM_VERSION;
use attemptdb_core::event::Provider;
use attemptdb_core::{
    AgentId, AttemptId, DecisionId, Event, EventId, EventKind, Outcome, PortablePath, ProjectId,
    SessionId, SpanId, Timestamp, ToolRef, TurnId, WorkUnitId,
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
    /// The latest `turn_objective` correction applied to this turn, when any
    /// (RFC 0003 §8). `objective` then holds the corrected text and
    /// `inferred_objective` the prompt text the projection derived.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub corrected: Option<CorrectionRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inferred_objective: Option<String>,
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
    /// Content-free shell command classification from the adapter
    /// (`attrs.command_category`: `test`, `git`, `build`, ...) and, for git
    /// commands, the subcommand (`attrs.git_subcommand`: `commit`, `push`,
    /// ...). Read from the start event, else the end event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command_category: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_subcommand: Option<String>,
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
    /// Shas of the commits made by this attempt's `git commit` calls, in
    /// call order (see [`Commit`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub commit_shas: Vec<String>,
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
    /// The work unit this attempt was grouped into (`tier1-v0`, §5.6).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work_unit_id: Option<WorkUnitId>,
    /// The latest correction applied to this attempt, when any. With an
    /// `attempt_outcome` correction, `outcome` / `failure_class` hold the
    /// corrected values and `inferred_outcome` / `inferred_failure_class`
    /// what the projection derived before the (first) correction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub corrected: Option<CorrectionRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inferred_outcome: Option<AttemptOutcome>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inferred_failure_class: Option<String>,
    /// Human note from the latest `attempt_note` (or `attempt_outcome`)
    /// correction that carried one; content, so `None` in `metadata_only`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
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
    WorkUnit(WorkUnitId),
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
    /// Fact events excluded because they, their session, or the attempt
    /// they evidenced were retracted. Still counted in `events_seen`.
    #[serde(default)]
    pub retracted_events: u64,
    /// `Correction` events observed / actually applied to a projected entity.
    #[serde(default)]
    pub corrections_seen: u64,
    #[serde(default)]
    pub corrections_applied: u64,
    /// `Retraction` events observed.
    #[serde(default)]
    pub retractions_seen: u64,
}

// ---------------------------------------------------------------------------
// Corrections and retractions (RFC 0003 §8)
// ---------------------------------------------------------------------------

/// What a `Correction` event corrects (`attrs.correction_type`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CorrectionType {
    /// Override an attempt's `outcome` (and optionally `failure_class`).
    AttemptOutcome,
    /// Attach a human note to an attempt.
    AttemptNote,
    /// Override a turn's objective text (and that of its attempts).
    TurnObjective,
}

impl CorrectionType {
    pub fn as_str(self) -> &'static str {
        match self {
            CorrectionType::AttemptOutcome => "attempt_outcome",
            CorrectionType::AttemptNote => "attempt_note",
            CorrectionType::TurnObjective => "turn_objective",
        }
    }

    pub const ALL: &'static [CorrectionType] = &[
        CorrectionType::AttemptOutcome,
        CorrectionType::AttemptNote,
        CorrectionType::TurnObjective,
    ];

    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim().to_ascii_lowercase().replace('-', "_");
        Self::ALL.iter().copied().find(|k| k.as_str() == s)
    }
}

/// Pointer from a corrected entity to the correction event that last
/// changed it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CorrectionRef {
    pub event_id: EventId,
    pub at: Timestamp,
    pub correction_type: CorrectionType,
}

/// The projected entity a correction names (`attrs.target`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum CorrectionTarget {
    Attempt(AttemptId),
    Turn(TurnId),
    Session(SessionId),
}

/// Whether and how a correction took effect.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CorrectionStatus {
    /// The target was found and the correction changed it.
    Applied,
    /// The target id is well-formed but no loaded entity carries it (the
    /// scan may be filtered, or the entity was renumbered by a retraction).
    TargetNotFound,
    /// The target belongs to a retracted session or attempt.
    TargetRetracted,
    /// Missing or malformed `correction_type`, `target`, or `outcome`.
    Invalid,
}

impl CorrectionStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            CorrectionStatus::Applied => "applied",
            CorrectionStatus::TargetNotFound => "target_not_found",
            CorrectionStatus::TargetRetracted => "target_retracted",
            CorrectionStatus::Invalid => "invalid",
        }
    }
}

/// One `Correction` event as the projection read it. Every field except
/// `note` is metadata; `note` is content and absent in `metadata_only`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Correction {
    pub event_id: EventId,
    pub at: Timestamp,
    /// The session the correction event was written into (the target's
    /// session for attempt/turn corrections).
    pub session_id: SessionId,
    pub project_id: ProjectId,
    pub correction_type: Option<CorrectionType>,
    pub target: Option<CorrectionTarget>,
    /// `attrs.target` as written, for diagnostics when it did not parse.
    pub target_text: String,
    pub outcome: Option<AttemptOutcome>,
    pub failure_class: Option<String>,
    pub note: Option<String>,
    pub note_chars: Option<u64>,
    pub status: CorrectionStatus,
}

/// What a `Retraction` event retracts (`attrs.target_type`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetractionTargetType {
    Session,
    Event,
    Attempt,
}

impl RetractionTargetType {
    pub fn as_str(self) -> &'static str {
        match self {
            RetractionTargetType::Session => "session",
            RetractionTargetType::Event => "event",
            RetractionTargetType::Attempt => "attempt",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "session" => Some(RetractionTargetType::Session),
            "event" => Some(RetractionTargetType::Event),
            "attempt" => Some(RetractionTargetType::Attempt),
            _ => None,
        }
    }
}

/// Why something was retracted (`attrs.reason`). A fixed, content-free
/// vocabulary; free text goes to `content.note`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetractionReason {
    Benchmark,
    Test,
    Duplicate,
    MistakenImport,
    Privacy,
    /// The device's credential was revoked (it left the organisation, or
    /// its key was withdrawn): its facts stay on disk, its sessions leave
    /// every projection.
    Revoked,
    Other,
}

impl RetractionReason {
    pub fn as_str(self) -> &'static str {
        match self {
            RetractionReason::Benchmark => "benchmark",
            RetractionReason::Test => "test",
            RetractionReason::Duplicate => "duplicate",
            RetractionReason::MistakenImport => "mistaken_import",
            RetractionReason::Privacy => "privacy",
            RetractionReason::Revoked => "revoked",
            RetractionReason::Other => "other",
        }
    }

    pub const ALL: &'static [RetractionReason] = &[
        RetractionReason::Benchmark,
        RetractionReason::Test,
        RetractionReason::Duplicate,
        RetractionReason::MistakenImport,
        RetractionReason::Privacy,
        RetractionReason::Revoked,
        RetractionReason::Other,
    ];

    /// Unknown text maps to `Other` so a retraction is never dropped for a
    /// typo in its reason.
    pub fn parse(s: &str) -> Self {
        let s = s.trim().to_ascii_lowercase().replace('-', "_");
        Self::ALL
            .iter()
            .copied()
            .find(|r| r.as_str() == s)
            .unwrap_or(RetractionReason::Other)
    }
}

/// The typed target of a retraction.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum RetractionTarget {
    Session(SessionId),
    Event(EventId),
    Attempt(AttemptId),
}

impl RetractionTarget {
    pub fn target_type(self) -> RetractionTargetType {
        match self {
            RetractionTarget::Session(_) => RetractionTargetType::Session,
            RetractionTarget::Event(_) => RetractionTargetType::Event,
            RetractionTarget::Attempt(_) => RetractionTargetType::Attempt,
        }
    }
}

/// One `Retraction` event as the projection read it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Retraction {
    pub event_id: EventId,
    pub at: Timestamp,
    pub project_id: ProjectId,
    pub target_type: Option<RetractionTargetType>,
    pub target: Option<RetractionTarget>,
    /// `attrs.target` as written.
    pub target_text: String,
    pub reason: RetractionReason,
    /// Content; absent in `metadata_only`.
    pub note: Option<String>,
    pub note_chars: Option<u64>,
    /// Whether a loaded session / event / attempt matched the target. A
    /// retraction with a well-formed id is honoured (kept in
    /// [`RetractedSet`]) even when nothing loaded matches it.
    pub matched: bool,
    /// Fact events this retraction removed from the projection.
    pub retracted_events: u64,
}

/// Ids removed by retractions, sorted for binary search. `events` holds
/// explicitly retracted event ids plus the tool-call events of retracted
/// attempts; events of retracted sessions are covered by `sessions`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetractedSet {
    pub sessions: Vec<SessionId>,
    pub attempts: Vec<AttemptId>,
    pub events: Vec<EventId>,
}

impl RetractedSet {
    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty() && self.attempts.is_empty() && self.events.is_empty()
    }

    pub fn contains_session(&self, id: &SessionId) -> bool {
        self.sessions.binary_search(id).is_ok()
    }

    pub fn contains_attempt(&self, id: &AttemptId) -> bool {
        self.attempts.binary_search(id).is_ok()
    }

    pub fn contains_event(&self, id: &EventId) -> bool {
        self.events.binary_search(id).is_ok()
    }

    /// Whether an event is retracted: its id or its session is. Correction
    /// and retraction events themselves are never retracted, so the audit
    /// trail survives filtering.
    pub fn is_retracted(&self, ev: &Event) -> bool {
        !is_meta_kind(ev.kind)
            && (self.contains_event(&ev.event_id) || self.contains_session(&ev.session_id))
    }

    pub(crate) fn insert_session(&mut self, id: SessionId) {
        if let Err(i) = self.sessions.binary_search(&id) {
            self.sessions.insert(i, id);
        }
    }

    pub(crate) fn insert_attempt(&mut self, id: AttemptId) {
        if let Err(i) = self.attempts.binary_search(&id) {
            self.attempts.insert(i, id);
        }
    }

    pub(crate) fn insert_event(&mut self, id: EventId) {
        if let Err(i) = self.events.binary_search(&id) {
            self.events.insert(i, id);
        }
    }
}

/// Event kinds that describe the log rather than the work: never grouped
/// into sessions, never retracted.
pub fn is_meta_kind(kind: EventKind) -> bool {
    matches!(kind, EventKind::Correction | EventKind::Retraction)
}

/// Entities that a retraction removed from the main projection, kept so the
/// query layer can show them on request (`INCLUDING RETRACTED`). Sessions
/// and their turns/calls/attempts are projected in isolation; retracted
/// attempts are the rows removed from their (still projected) session.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RetractedEntities {
    pub sessions: Vec<Session>,
    pub turns: Vec<Turn>,
    pub tool_calls: Vec<ToolCall>,
    pub attempts: Vec<Attempt>,
}

// ---------------------------------------------------------------------------
// Work units and decisions (RFC 0003 §5.6, §5.7)
// ---------------------------------------------------------------------------

/// What the unit is doing, judged from its last five tool calls.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Explore,
    Plan,
    Implement,
    Debug,
    Verify,
    Review,
    Deliver,
    Blocked,
}

impl Phase {
    pub fn as_str(self) -> &'static str {
        match self {
            Phase::Explore => "explore",
            Phase::Plan => "plan",
            Phase::Implement => "implement",
            Phase::Debug => "debug",
            Phase::Verify => "verify",
            Phase::Review => "review",
            Phase::Deliver => "deliver",
            Phase::Blocked => "blocked",
        }
    }

    pub const ALL: &'static [Phase] = &[
        Phase::Explore,
        Phase::Plan,
        Phase::Implement,
        Phase::Debug,
        Phase::Verify,
        Phase::Review,
        Phase::Deliver,
        Phase::Blocked,
    ];

    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim().to_ascii_lowercase();
        Self::ALL.iter().copied().find(|p| p.as_str() == s)
    }
}

/// Whether the unit is still being worked on. Independent of `phase`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkUnitStatus {
    Open,
    Completed,
    Abandoned,
    Unknown,
}

impl WorkUnitStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            WorkUnitStatus::Open => "open",
            WorkUnitStatus::Completed => "completed",
            WorkUnitStatus::Abandoned => "abandoned",
            WorkUnitStatus::Unknown => "unknown",
        }
    }

    pub const ALL: &'static [WorkUnitStatus] = &[
        WorkUnitStatus::Open,
        WorkUnitStatus::Completed,
        WorkUnitStatus::Abandoned,
        WorkUnitStatus::Unknown,
    ];

    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim().to_ascii_lowercase();
        Self::ALL.iter().copied().find(|p| p.as_str() == s)
    }
}

/// A connected component of turns within one project (`tier1-v0`): turns
/// are linked when they touch a common repository path through a mutating
/// or shell tool call, when they are consecutive turns of one session less
/// than ten minutes apart, or when a handoff links their sessions.
///
/// `work_unit_id` is `WorkUnitId::derive(&[project_id, first evidence
/// event id])`; the struct is the versioned inference record (`version`
/// is `1` until units are stored and superseded individually).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WorkUnit {
    pub work_unit_id: WorkUnitId,
    pub project_id: ProjectId,
    pub project_name: String,
    /// Prompt text of the earliest prompted turn, when content was captured.
    pub objective: Option<String>,
    /// The prompt event that objective comes from (also present without
    /// content).
    pub objective_event_id: Option<EventId>,
    pub phase: Phase,
    /// Content-free statement of the rule that produced `phase`.
    pub phase_reason: String,
    pub status: WorkUnitStatus,
    pub status_reason: String,
    pub started_at: Timestamp,
    /// Latest activity observed in any member turn.
    pub updated_at: Timestamp,
    /// `updated_at` once the status is `Completed` or `Abandoned`; `None`
    /// while `Open` or `Unknown`.
    pub ended_at: Option<Timestamp>,
    /// Distinct sessions in order of first member turn.
    pub sessions: Vec<SessionId>,
    /// Member turns in projection order (session order, then index).
    pub turns: Vec<TurnId>,
    pub attempts: Vec<AttemptId>,
    /// Repository-relative paths touched by mutating or shell calls, in
    /// first-touch order.
    pub paths: Vec<String>,
    /// Shas committed by member attempts, in attempt order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub commit_shas: Vec<String>,
    /// Distinct providers of the member sessions.
    pub actors: Vec<Provider>,
    /// Member attempts whose (possibly corrected) outcome is a failure.
    pub failure_count: u32,
    /// The latest member attempt by start time.
    pub last_attempt: Option<AttemptId>,
    /// The uncleared pending-input signal that makes the phase `Blocked`.
    pub blocking_signal: Option<EventId>,
    pub evidence: Vec<EventId>,
    /// Minimum over member attempts, capped at `0.7`: grouping is a
    /// heuristic.
    pub confidence: f32,
    #[serde(default)]
    pub algorithm_version: AlgorithmVersion,
    pub version: u32,
}

/// How a decision was derived.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionKind {
    /// A failed attempt was superseded by a retry on the same paths.
    ApproachChange,
    /// A permission denial was followed by a retry with a different tool.
    HumanIntervention,
}

impl DecisionKind {
    pub fn as_str(self) -> &'static str {
        match self {
            DecisionKind::ApproachChange => "approach_change",
            DecisionKind::HumanIntervention => "human_intervention",
        }
    }
}

/// A decision derived from the attempt structure. Nothing here is stated by
/// a human: `rationale` is assembled from tool categories, failure classes
/// and repository-relative paths, and `rationale_source` is always
/// `"derived"`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Decision {
    pub decision_id: DecisionId,
    pub work_unit_id: Option<WorkUnitId>,
    pub session_id: SessionId,
    pub turn_id: TurnId,
    pub kind: DecisionKind,
    /// The attempt that was continued with.
    pub selected: AttemptId,
    /// The attempts given up on (empty when the retry stayed within the
    /// same attempt).
    pub alternatives: Vec<AttemptId>,
    pub rationale: String,
    pub rationale_source: String,
    pub decided_at: Timestamp,
    pub evidence: Vec<EventId>,
    /// Minimum of the involved attempts' confidence, capped at `0.7`.
    pub confidence: f32,
    #[serde(default)]
    pub algorithm_version: AlgorithmVersion,
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
    /// Sorted by `(started_at, work_unit_id)`.
    #[serde(default)]
    pub work_units: Vec<WorkUnit>,
    /// Sorted by `(decided_at, decision_id)`.
    #[serde(default)]
    pub decisions: Vec<Decision>,
    /// Grouped by session, in call order.
    #[serde(default)]
    pub commits: Vec<Commit>,
    /// Every correction event, in stream order.
    #[serde(default)]
    pub corrections: Vec<Correction>,
    /// Every retraction event, in stream order.
    #[serde(default)]
    pub retractions: Vec<Retraction>,
    /// Ids the retractions removed.
    #[serde(default)]
    pub retracted_ids: RetractedSet,
    /// The removed entities themselves.
    #[serde(default)]
    pub retracted: RetractedEntities,
    /// The reference time `status` was judged against: the latest observed
    /// timestamp in the stream unless the caller supplied one.
    #[serde(default)]
    pub reference_time: Timestamp,
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

/// A commit made by a `git commit` tool call, tied to the sha the
/// repository moved to. Content-free: the hook records the repository
/// `HEAD` on every event, so a successful commit call whose own end event
/// (or the next head-bearing event) shows a new `HEAD` names the commit
/// without reading any command output. This is the artifact side of the
/// timeline — what an attempt shipped, joinable with a forge's commit
/// records on `sha`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Commit {
    pub commit_id: attemptdb_core::CommitId,
    pub session_id: SessionId,
    pub project_id: ProjectId,
    pub turn_id: Option<TurnId>,
    /// The attempt whose tool call made the commit.
    pub attempt_id: Option<AttemptId>,
    pub tool_call_id: ToolCallId,
    /// The new `HEAD`. `None` when the call succeeded but no event carried
    /// a changed head afterwards (git context not captured, or the session
    /// ended before the next hook fired).
    pub sha: Option<String>,
    /// `HEAD` before the call, when known.
    pub previous_sha: Option<String>,
    pub branch: Option<String>,
    /// When the commit call finished.
    pub at: Timestamp,
    /// How `sha` was established: `end_event` (the call's own end event
    /// carried the new head; 0.9), `next_head` (a later event in the session
    /// did, and its previous head matched; 0.7), or `unresolved` (0.4).
    pub linkage: String,
    /// The call's start/end events and, for `next_head`, the event that
    /// showed the new head.
    pub evidence: Vec<EventId>,
    pub confidence: f32,
    #[serde(default)]
    pub algorithm_version: AlgorithmVersion,
}
