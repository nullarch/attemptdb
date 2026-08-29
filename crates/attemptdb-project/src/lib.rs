//! Tier 1 inference: deterministic grouping and state projection.
//!
//! This crate turns an ordered stream of immutable [`Event`](attemptdb_core::Event)s into the
//! projected entities the query layer exposes as tables: sessions, turns, tool
//! calls, attempts, handoffs and causal edges. Everything emitted here is an
//! *inference*, never ground truth:
//!
//! - every entity carries the ids of the events it was derived from,
//! - attempts, handoffs and explanations carry a confidence,
//! - the whole projection is stamped with [`ALGORITHM_VERSION`] so it can be
//!   discarded and rebuilt when the algorithm changes.
//!
//! # Determinism
//!
//! Identical event streams produce identical projections: the same entity ids,
//! in the same order, with the same field values. All projected ids are
//! derived (`XxxId::derive`) from stable inputs such as the session id and the
//! turn index; nothing is randomly generated. Input is sorted defensively
//! before projection (see [`Projector`]).
//!
//! # Content
//!
//! The projection is usable in `metadata_only` capture mode. The only
//! content-bearing field it ever reads is the prompt text, which becomes
//! [`Turn::objective`] / [`Attempt::objective`] when available and `None`
//! otherwise. Approach summaries are built exclusively from tool categories
//! and repository-relative paths.
//!
//! # Heuristics (v0)
//!
//! The exact rules are documented on the types in [`model`] and on the
//! [`Projector`]. In short:
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
//! - **Blocked** = the latest event is a pending permission/needs-input
//!   signal, or the last two attempts failed with the same failure class.

#![forbid(unsafe_code)]

mod approach;
mod attempts;
mod handoff;
pub mod model;
mod order;
mod projector;
mod state;

/// Version stamp of the projection algorithm implemented by this crate.
///
/// Bump whenever any rule changes in a way that could alter the output for an
/// existing event stream; consumers use it to decide when to re-project.
pub const ALGORITHM_VERSION: &str = "tier1-v0";

pub use model::{
    AlgorithmVersion, Attempt, AttemptOutcome, CausalEdge, CoverageGrade, EdgeEndpoint, EdgeKind,
    Explanation, Handoff, ProjectStateSnapshot, Projection, ProjectionStats, Session, SessionState,
    Signal, ToolCall, ToolCallId, Turn, TurnStatus,
};
pub use projector::{Projector, attr_keys, project};

// Re-exported for convenience so downstream crates can name the event types
// that appear in projected structs without depending on core directly.
pub use attemptdb_core::event::Provider;
