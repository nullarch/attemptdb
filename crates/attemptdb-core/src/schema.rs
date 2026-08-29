//! Schema versioning.
//!
//! Two independent version numbers exist on purpose:
//!
//! - **Canonical schema version** (`CANONICAL_SCHEMA_VERSION`): the shape and
//!   meaning of the logical [`crate::Event`] model. Bumped when fields are
//!   added/renamed/reinterpreted. Readers must preserve unknown fields from
//!   newer minor versions.
//! - **Storage format versions** live in `attemptdb-storage` and describe
//!   physical layouts (WAL frames, segments, manifest). They can change without
//!   touching the logical schema and vice versa.

/// Current canonical event schema version written by this build.
pub const CANONICAL_SCHEMA_VERSION: u16 = 1;

/// Oldest canonical schema version this build can read.
pub const MIN_READABLE_SCHEMA_VERSION: u16 = 1;

/// Stable numeric field identifiers for the canonical event model.
///
/// These are the *only* identifiers that binary encodings and columnar
/// segments are allowed to use for canonical fields. Names may be renamed;
/// numbers may never be reused. Gaps are intentional so related fields can be
/// added near each other.
pub mod field_id {
    pub const EVENT_ID: u16 = 1;
    pub const SCHEMA_VERSION: u16 = 2;
    pub const DEVICE_ID: u16 = 3;
    pub const SOURCE_SEQ: u16 = 4;
    pub const HLC: u16 = 5;
    pub const OBSERVED_AT: u16 = 6;
    pub const CAPTURED_AT: u16 = 7;
    pub const INGESTED_AT: u16 = 8;

    pub const PROVIDER: u16 = 20;
    pub const PROVIDER_VERSION: u16 = 21;
    pub const ADAPTER_VERSION: u16 = 22;
    pub const HOOK_VERSION: u16 = 23;
    pub const CAPTURE_MODE: u16 = 24;
    pub const PROVIDER_EVENT_NAME: u16 = 25;

    pub const KIND: u16 = 40;
    pub const PROJECT_ID: u16 = 41;
    pub const PROJECT_ROOT: u16 = 42;
    pub const PROJECT_NAME: u16 = 43;
    pub const REPO_REMOTE: u16 = 44;
    pub const GIT_BRANCH: u16 = 45;
    pub const GIT_HEAD: u16 = 46;

    pub const SESSION_ID: u16 = 60;
    pub const PROVIDER_SESSION_ID: u16 = 61;
    pub const PROVIDER_TURN_ID: u16 = 62;
    pub const SPAN_ID: u16 = 63;
    pub const PARENT_SPAN_ID: u16 = 64;

    pub const AGENT_ID: u16 = 80;
    pub const AGENT_TYPE: u16 = 81;
    pub const PARENT_AGENT_ID: u16 = 82;
    pub const MODEL: u16 = 83;
    pub const PROVIDER_AGENT_ID: u16 = 84;

    pub const TOOL_NAME: u16 = 100;
    pub const TOOL_CATEGORY: u16 = 101;
    pub const TOOL_CALL_ID: u16 = 102;

    pub const PATHS: u16 = 120;
    pub const PATH_LOGICAL: u16 = 121;
    pub const PATH_RELATIVE: u16 = 122;
    pub const OUTCOME_STATUS: u16 = 130;
    pub const OUTCOME_CLASS: u16 = 131;
    pub const EXIT_CODE: u16 = 132;
    pub const DURATION_MS: u16 = 140;

    pub const ATTRS: u16 = 200;
    pub const CONTENT_REF: u16 = 210;
    pub const RAW_REF: u16 = 211;
    pub const UNKNOWN: u16 = 250;
}
