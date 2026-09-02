//! Tier 1 inference: deterministic grouping and state projection.
//!
//! This crate turns an ordered stream of immutable [`Event`](attemptdb_core::Event)s into the
//! projected entities the query layer exposes as tables: sessions, turns, tool
//! calls, attempts, handoffs, work units, decisions and causal edges.
//! Everything emitted here is an *inference*, never ground truth:
//!
//! - every entity carries the ids of the events it was derived from,
//! - attempts, handoffs, work units, decisions and explanations carry a
//!   confidence,
//! - the whole projection is stamped with [`ALGORITHM_VERSION`] so it can be
//!   discarded and rebuilt when the algorithm changes.
//!
//! # Determinism
//!
//! Identical event streams produce identical projections: the same entity ids,
//! in the same order, with the same field values. All projected ids are
//! derived (`XxxId::derive`) from stable inputs such as the session id and the
//! turn index; nothing is randomly generated. Input is sorted defensively
//! before projection (see [`Projector`]). Work-unit status depends on idle
//! time and is judged against the stream's latest timestamp unless the caller
//! passes a reference time ([`Projector::finish_at`], [`project_at`]).
//!
//! # Content
//!
//! The projection is usable in `metadata_only` capture mode. The only
//! content-bearing fields it ever reads are the prompt text, which becomes
//! [`Turn::objective`] / [`Attempt::objective`] / [`WorkUnit::objective`]
//! when available and `None` otherwise, and the note of a correction or
//! retraction. Approach summaries and decision rationales are built
//! exclusively from tool categories, failure classes and repository-relative
//! paths.
//!
//! # Corrections and retractions
//!
//! `Correction` and `Retraction` events (RFC 0003 §8) are facts about the
//! log written by AttemptDB itself. Corrections override a projected value
//! (latest wins, the inferred value is kept alongside); retractions remove a
//! session, event or attempt from every projection and from the sanitized
//! export ([`retracted_ids`]). See [`Projection::corrections`],
//! [`Projection::retractions`], [`Projection::retracted_ids`].
//!
//! # Heuristics (v0)
//!
//! The exact rules are documented on the types in [`model`], on the
//! [`Projector`], and in the `workunit` / `decision` module docs. In short:
//!
//! - **Session** = group by `session_id`; coverage graded from which
//!   lifecycle/activity events were observed.
//! - **Turn** = one `PromptSubmitted` up to the matching `TurnStopped` /
//!   `TurnFailed`, the next prompt, or `SessionEnded`. Tool events before any
//!   prompt form an implicit turn with index `0`.
//! - **ToolCall** = `ToolCallStarted` paired with `ToolCallFinished` /
//!   `ToolCallFailed` by `tool.call_id`, else FIFO by `(agent, tool name)`.
//! - **Attempt** = a turn's tool calls split at each failed file-mutating or
//!   shell call; later attempts on the same paths supersede earlier failed
//!   ones.
//! - **Handoff** = a session of a different provider starting shortly after
//!   another session in the same project went idle, preferably touching the
//!   same paths.
//! - **WorkUnit** = connected component of turns linked by shared mutated
//!   paths, by being consecutive turns of one session within ten minutes, or
//!   by a handoff; phase from the last five tool calls, status from the
//!   last attempt and idle time.
//! - **Decision** = a superseded → superseding attempt pair
//!   (`approach_change`) or a permission denial followed by a different
//!   tool (`human_intervention`).
//! - **Blocked** = the latest event is a pending permission/needs-input
//!   signal, or the last two attempts failed with the same failure class.

#![forbid(unsafe_code)]

mod approach;
mod attempts;
mod decision;
mod handoff;
mod meta;
pub mod model;
mod order;
mod projector;
mod state;
mod workunit;

/// Version stamp of the projection algorithm implemented by this crate.
///
/// Bump whenever any rule changes in a way that could alter the output for an
/// existing event stream; consumers use it to decide when to re-project.
pub const ALGORITHM_VERSION: &str = "tier1-v0";

pub use meta::CORRECTABLE_OUTCOMES;
pub use model::{
    AlgorithmVersion, Attempt, AttemptOutcome, CausalEdge, Commit, Correction, CorrectionRef,
    CorrectionStatus, CorrectionTarget, CorrectionType, CoverageGrade, Decision, DecisionKind,
    EdgeEndpoint, EdgeKind, Explanation, Handoff, Phase, ProjectStateSnapshot, Projection,
    ProjectionStats, RetractedEntities, RetractedSet, Retraction, RetractionReason,
    RetractionTarget, RetractionTargetType, Session, SessionState, Signal, ToolCall, ToolCallId,
    Turn, TurnStatus, WorkUnit, WorkUnitStatus, is_meta_kind,
};
pub use projector::{IncrementalProjector, Projector, attr_keys, project, project_at};

/// Whether the projector reads an event's `content` for this kind: the
/// prompt text of a submitted prompt, and the note of a correction or
/// retraction. Every other kind is projected from metadata alone, so a
/// reader feeding the projector can leave their content unresolved.
pub fn needs_content(kind: attemptdb_core::EventKind) -> bool {
    matches!(
        kind,
        attemptdb_core::EventKind::PromptSubmitted
            | attemptdb_core::EventKind::Correction
            | attemptdb_core::EventKind::Retraction
    )
}
pub use workunit::{
    ABANDON_IDLE_US, COMPLETE_IDLE_US, CONFIDENCE_CAP, LINK_WINDOW_US, PHASE_WINDOW,
};

// Re-exported for convenience so downstream crates can name the event types
// that appear in projected structs without depending on core directly.
pub use attemptdb_core::event::Provider;

use attemptdb_core::Event;

/// The ids every retraction in `events` removes: retracted session ids,
/// attempt ids, and event ids (explicitly retracted events plus the
/// tool-call events of retracted attempts). Attempt ids are resolved by
/// projecting the stream, so the result reflects the same rules as
/// [`Projection::retracted_ids`]. Use [`RetractedSet::is_retracted`] to
/// filter an event stream (for example before a sanitized export).
pub fn retracted_ids(events: &[Event]) -> RetractedSet {
    project(events).retracted_ids
}
