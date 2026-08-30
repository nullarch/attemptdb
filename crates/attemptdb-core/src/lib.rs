//! AttemptDB core: the canonical data model shared by every other crate.
//!
//! This crate deliberately has no I/O, no async runtime, and no storage
//! knowledge. It defines:
//!
//! - stable identifiers ([`ids`]) and their derivation rules,
//! - the hybrid logical clock ([`clock`]) used for causal ordering,
//! - the canonical event model ([`event`]) that every provider adapter
//!   normalises into,
//! - schema versioning constants ([`schema`]),
//! - portable path representation ([`paths`]),
//! - capture/privacy modes ([`privacy`]).
//!
//! The on-disk encodings live in `attemptdb-storage`; the crate here only
//! guarantees that the *logical* model is platform neutral: no pointers, no
//! platform-sized integers, UTF-8 text everywhere.

pub mod attrs;
pub mod clock;
pub mod codec;
pub mod conformance;
pub mod event;
pub mod ids;
pub mod paths;
pub mod privacy;
pub mod schema;
pub mod secrets;
pub mod time;

pub use clock::Hlc;
pub use event::{
    AgentRef, Event, EventKind, Outcome, OutcomeStatus, ProjectRef, ToolCategory, ToolRef,
};
pub use ids::{
    AgentId, ArtifactId, AttemptId, CommitId, DecisionId, DeviceId, EventId, ProjectId, SessionId,
    SpanId, TurnId, WorkUnitId,
};
pub use paths::{PortablePath, elide_home};
pub use privacy::CaptureMode;
pub use time::Timestamp;

/// Errors produced by the core crate.
#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("invalid identifier: {0}")]
    InvalidId(String),
    #[error("unsupported schema version {found} (this build supports {supported})")]
    UnsupportedSchema { found: u16, supported: u16 },
    #[error("codec error: {0}")]
    Codec(String),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, CoreError>;
