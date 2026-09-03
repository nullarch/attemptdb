//! The query catalog: what every table and column means, for whoever — or
//! whatever — is writing a statement.
//!
//! Three things this crate otherwise keeps implicit are written down here:
//! a table's grain, a column's meaning, and the values a column may take.
//! The column *list* is deliberately not written here. Columns, types and
//! nullability are read from the real Arrow schema at call time, so the
//! catalog cannot describe a column that does not exist, and
//! `tests/catalog.rs` fails when a new column arrives with no description.
//!
//! Closed vocabularies are built from the Rust enums that produce them; the
//! exhaustiveness guards in the test module stop compiling when a variant is
//! added, so a new value cannot go undocumented either.
//!
//! Rendered by `attempt schema`, by the `attempt_schema` MCP tool, and into
//! `docs/query-context.md`, which `tests/catalog.rs` keeps in sync.

use crate::tables::{projection_schema, readable_events_schema, type_name};
use attemptdb_core::event::Provider;
use attemptdb_core::{CaptureMode, EventKind, OutcomeStatus, ToolCategory};
use attemptdb_project::{
    AttemptOutcome, CorrectionStatus, CorrectionType, CoverageGrade, DecisionKind, EdgeKind, Phase,
    RetractionReason, RetractionTargetType, TurnStatus, WorkUnitStatus,
};
use datafusion::arrow::datatypes::SchemaRef;
use serde_json::{Value, json};
use std::fmt::Write as _;

/// Whether a table holds observed facts or derived inference (RFC 0003).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layer {
    Fact,
    Inference,
}

impl Layer {
    pub fn as_str(self) -> &'static str {
        match self {
            Layer::Fact => "fact",
            Layer::Inference => "inference",
        }
    }
}

/// A foreign key: `column` of this table names a row of `target`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Join {
    pub column: &'static str,
    /// `table.column`.
    pub target: &'static str,
}

/// One column: its real type from the schema, its meaning from this module.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Column {
    pub name: String,
    pub data_type: String,
    pub nullable: bool,
    pub doc: &'static str,
    /// The values this column may take. Empty for free text and numbers.
    pub values: Vec<String>,
    /// True when `values` is a common sample rather than the whole
    /// vocabulary: the value comes from a provider or a heuristic, not an
    /// enum, so anything may appear.
    pub open: bool,
}

/// One queryable table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Table {
    pub name: &'static str,
    pub layer: Layer,
    /// What one row is.
    pub grain: &'static str,
    pub summary: &'static str,
    pub joins: &'static [Join],
    pub columns: Vec<Column>,
}

/// A question and the statement that answers it.
///
/// `statement` may contain the placeholders in [`PLACEHOLDERS`]; a caller
/// substitutes a real id before running it, and `tests/catalog.rs` does
/// exactly that for every example, so no example can rot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Example {
    pub question: &'static str,
    pub statement: &'static str,
    pub note: &'static str,
}

/// Placeholders an example may use, each replaced by a real id.
pub const PLACEHOLDERS: &[&str] = &["{session}", "{attempt}"];

/// The rules that decide whether a statement is right, not just valid.
/// Written for a reader with no other context.
pub const RULES: &[&str] = &[
    "Two languages share one surface. AttemptQL verbs (SHOW, WHY, TRACE, STATE, DIFF, WHAT IS, EXPLAIN) answer the common questions in one line; plain SQL in the DataFusion dialect answers everything else over the same tables. Prefer the verb when one fits: it applies the retraction and scope rules for you.",
    "The engine is read-only. Only SELECT, WITH, VALUES, EXPLAIN, DESCRIBE, SHOW and the AttemptQL verbs are accepted, one statement per call. There is no INSERT, no CREATE, no GRANT: history is appended by capture, never by a query.",
    "`events` is fact; every other table is inference. An event was observed and is immutable. A session, turn, tool call, attempt, work unit, decision, handoff, edge or conflict was derived by the projector, and each carries `evidence` (the event ids it was built from), `confidence` (0.0-1.0) and usually `algorithm_version`. Never present an inferred row as something the agent did; say what it was inferred from.",
    "Ids are readable prefixed strings, not UUIDs: `ev_` event, `ses_` session, `trn_` turn, `tc_` tool call, `att_` attempt, `wu_` work unit, `dec_` decision, `cmt_` commit, `prj_` project, `dev_` device. Compare them as text. `events_raw` is the same stream with the storage types instead (16-byte UUIDs, dictionary-encoded strings); read `events` unless you need the raw layout.",
    "Times are `timestamp(microsecond, UTC)`. `observed_at` is when the agent did it, `captured_at` when the hook recorded it, `ingested_at` when the database accepted it. Order history by `observed_at`; measure capture lag with the other two.",
    "Retracted rows are hidden by AttemptQL and visible to SQL. `SHOW` drops them unless the statement says `INCLUDING RETRACTED`; a bare `SELECT` does not, so filter `retracted = false` yourself on `events`, `sessions`, `turns`, `tool_calls` and `attempts`.",
    "Content may be absent by design. Under `capture_mode = 'metadata_only'` the columns that carry text — `objective`, `rationale`, `note`, `content_json`, `raw_json` — are null for every row, and that is a privacy setting, not missing data. Check `events.capture_mode` before concluding an agent had no objective.",
    "Counts belong to the projection, not to SQL aggregates you re-derive. `sessions.turn_count`, `attempts.tool_call_count` and the rest are computed with the retraction rules applied; recomputing them with COUNT(*) over the child table gives a different (and usually wrong) number.",
    "Scope is a filter, not a mode. There is one database per install holding every project; a question about one repository is `WHERE project_name = '…'` or the `--project` flag, never a different connection.",
];

// ---------------------------------------------------------------------------
// Closed vocabularies, built from the types that produce them
// ---------------------------------------------------------------------------

fn strs(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

fn providers() -> Vec<String> {
    [
        Provider::ClaudeCode,
        Provider::Codex,
        Provider::Cursor,
        Provider::GeminiCli,
    ]
    .iter()
    .map(|p: &Provider| p.as_str().to_string())
    .chain(["attemptdb".to_string()])
    .collect()
}

fn event_kinds() -> Vec<String> {
    EventKind::ALL
        .iter()
        .map(|k| k.as_str().to_string())
        .collect()
}

fn capture_modes() -> Vec<String> {
    [
        CaptureMode::MetadataOnly,
        CaptureMode::LocalSemantic,
        CaptureMode::FullSync,
    ]
    .iter()
    .map(|m| m.as_str().to_string())
    .collect()
}

fn tool_categories() -> Vec<String> {
    [
        ToolCategory::Shell,
        ToolCategory::FileRead,
        ToolCategory::FileWrite,
        ToolCategory::FileEdit,
        ToolCategory::Search,
        ToolCategory::Web,
        ToolCategory::Mcp,
        ToolCategory::Subagent,
        ToolCategory::Plan,
        ToolCategory::Notebook,
        ToolCategory::Other,
    ]
    .iter()
    .map(|c| c.as_str().to_string())
    .collect()
}

fn outcome_statuses() -> Vec<String> {
    [
        OutcomeStatus::Success,
        OutcomeStatus::Failure,
        OutcomeStatus::Denied,
        OutcomeStatus::Cancelled,
        OutcomeStatus::Unknown,
    ]
    .iter()
    .map(|o| o.as_str().to_string())
    .collect()
}

fn coverage_grades() -> Vec<String> {
    [
        CoverageGrade::Full,
        CoverageGrade::Partial,
        CoverageGrade::Minimal,
        CoverageGrade::Unknown,
    ]
    .iter()
    .map(|c| c.as_str().to_string())
    .collect()
}

fn turn_statuses() -> Vec<String> {
    [
        TurnStatus::Completed,
        TurnStatus::Failed,
        TurnStatus::InProgress,
        TurnStatus::Unknown,
    ]
    .iter()
    .map(|s| s.as_str().to_string())
    .collect()
}

fn attempt_outcomes() -> Vec<String> {
    [
        AttemptOutcome::Succeeded,
        AttemptOutcome::Failed,
        AttemptOutcome::Abandoned,
        AttemptOutcome::Superseded,
        AttemptOutcome::InProgress,
        AttemptOutcome::Unknown,
    ]
    .iter()
    .map(|o| o.as_str().to_string())
    .collect()
}

fn edge_kinds() -> Vec<String> {
    [
        EdgeKind::ParentOf,
        EdgeKind::Caused,
        EdgeKind::Triggered,
        EdgeKind::Blocked,
        EdgeKind::Resolved,
        EdgeKind::Superseded,
        EdgeKind::Produced,
        EdgeKind::Verified,
        EdgeKind::Contradicted,
        EdgeKind::HandedOff,
        EdgeKind::EvidenceFor,
    ]
    .iter()
    .map(|k| k.as_str().to_string())
    .collect()
}

fn phases() -> Vec<String> {
    Phase::ALL.iter().map(|p| p.as_str().to_string()).collect()
}

fn work_unit_statuses() -> Vec<String> {
    WorkUnitStatus::ALL
        .iter()
        .map(|s| s.as_str().to_string())
        .collect()
}

fn decision_kinds() -> Vec<String> {
    [
        DecisionKind::ApproachChange,
        DecisionKind::HumanIntervention,
    ]
    .iter()
    .map(|k| k.as_str().to_string())
    .collect()
}

fn correction_types() -> Vec<String> {
    CorrectionType::ALL
        .iter()
        .map(|c| c.as_str().to_string())
        .collect()
}

fn correction_statuses() -> Vec<String> {
    [
        CorrectionStatus::Applied,
        CorrectionStatus::TargetNotFound,
        CorrectionStatus::TargetRetracted,
        CorrectionStatus::Invalid,
    ]
    .iter()
    .map(|s| s.as_str().to_string())
    .collect()
}

fn retraction_reasons() -> Vec<String> {
    RetractionReason::ALL
        .iter()
        .map(|r| r.as_str().to_string())
        .collect()
}

fn retraction_target_types() -> Vec<String> {
    [
        RetractionTargetType::Session,
        RetractionTargetType::Event,
        RetractionTargetType::Attempt,
    ]
    .iter()
    .map(|t| t.as_str().to_string())
    .collect()
}

/// The vocabulary of `table.column`: the values and whether the list is the
/// whole vocabulary (`false`) or a common sample of an open one (`true`).
fn values_of(table: &str, column: &str) -> (Vec<String>, bool) {
    let closed = |v: Vec<String>| (v, false);
    let open = |v: &[&str]| (strs(v), true);
    match (table, column) {
        (_, "provider") | (_, "from_provider") | (_, "to_provider") => (providers(), true),
        ("events" | "events_raw", "kind") => closed(event_kinds()),
        ("events" | "events_raw", "capture_mode") => closed(capture_modes()),
        ("events" | "events_raw" | "tool_calls", "tool_category") => closed(tool_categories()),
        ("events" | "events_raw" | "tool_calls", "outcome_status") => closed(outcome_statuses()),
        ("sessions", "state") => closed(strs(&["open", "closed"])),
        ("sessions", "coverage") => closed(coverage_grades()),
        ("turns", "status") => closed(turn_statuses()),
        ("attempts", "outcome") | ("attempts", "inferred_outcome") | ("corrections", "outcome") => {
            closed(attempt_outcomes())
        }
        ("edges", "edge_kind") => closed(edge_kinds()),
        ("edges", "from_type") | ("edges", "to_type") => closed(strs(&[
            "event",
            "tool_call",
            "turn",
            "attempt",
            "session",
            "work_unit",
        ])),
        ("edges", "edge_source") => closed(strs(&["projection", "derived"])),
        ("work_units", "phase") => closed(phases()),
        ("work_units", "status") => closed(work_unit_statuses()),
        ("decisions", "kind") => closed(decision_kinds()),
        ("decisions", "rationale_source") => closed(strs(&["derived"])),
        ("signals", "kind") => closed(strs(&[
            "permission_requested",
            "permission_denied",
            "notification",
        ])),
        ("commits", "linkage") => closed(strs(&["end_event", "next_head", "unresolved"])),
        ("corrections", "correction_type") | ("attempts", "correction_type") => {
            closed(correction_types())
        }
        ("corrections", "status") => closed(correction_statuses()),
        ("corrections", "target_type") => closed(strs(&["attempt", "turn", "session"])),
        ("retractions", "target_type") => closed(retraction_target_types()),
        ("retractions", "reason") => closed(retraction_reasons()),
        // Open vocabularies: a provider or a heuristic produces the value,
        // so these are the values seen so far, not a closed set.
        (_, "command_category") => open(&[
            "git", "test", "build", "install", "network", "fs", "run", "other",
        ]),
        (_, "failure_class") | (_, "inferred_failure_class") => open(&[
            "test_failure",
            "compile_error",
            "permission_denied",
            "timeout",
            "not_found",
            "conflict",
            "other",
        ]),
        (_, "outcome_class") => open(&["exit_code", "denied", "timeout", "cancelled"]),
        ("attempts", "approach") => open(&["edit", "shell", "search", "read", "mixed"]),
        _ => (Vec::new(), false),
    }
}

// ---------------------------------------------------------------------------
// Column meanings
// ---------------------------------------------------------------------------

/// Meanings shared across tables. A table-specific entry wins.
const COMMON: &[(&str, &str)] = &[
    ("session_id", "The session this row belongs to (`ses_…`)."),
    (
        "provider",
        "Which coding agent produced the underlying events.",
    ),
    (
        "project_id",
        "Stable id of the repository (`prj_…`), derived from its root path and remote.",
    ),
    (
        "project_name",
        "`owner/repo` when a git remote is known, otherwise the directory name. This is what a human filters on.",
    ),
    (
        "turn_id",
        "The turn (`trn_…`) this row belongs to: one human prompt and everything the agent did in response.",
    ),
    ("attempt_id", "The attempt (`att_…`) this row belongs to."),
    (
        "tool_call_id",
        "The tool call (`tc_…`) this row belongs to.",
    ),
    (
        "work_unit_id",
        "The work unit (`wu_…`) this row was folded into, if any.",
    ),
    ("event_id", "The event (`ev_…`) this row was written from."),
    ("started_at", "When the row's first evidence was observed."),
    (
        "ended_at",
        "When the row's last evidence was observed. Null while it is still open.",
    ),
    (
        "updated_at",
        "When the newest evidence for this row was observed.",
    ),
    (
        "evidence",
        "The event ids this row was inferred from. The whole point of an inference: follow these to check the claim.",
    ),
    (
        "confidence",
        "0.0-1.0. How strongly the evidence supports the row, not how important the row is.",
    ),
    (
        "algorithm_version",
        "The projector version that produced the row (`tier1-v1`). Rows from different versions are not comparable.",
    ),
    (
        "retracted",
        "True when a Retraction removed the row. `SHOW` hides these; SQL does not.",
    ),
    (
        "paths",
        "Repository-relative paths this row touched, deduplicated.",
    ),
    ("commit_shas", "Commit shas produced under this row."),
    (
        "note",
        "Free text a human wrote. Content: null under `metadata_only`.",
    ),
    (
        "note_chars",
        "Length of `note` in characters. Metadata, so it survives `metadata_only` even when `note` does not.",
    ),
    (
        "corrected_by",
        "The Correction event (`ev_…`) that overrode this row's inference, if any.",
    ),
    ("corrected_at", "When that correction was written."),
    ("correction_type", "What the correction changed."),
    (
        "target",
        "The projected entity the row points at, as a prefixed id.",
    ),
    ("target_type", "Which kind of entity `target` names."),
    (
        "branch",
        "Git branch at the time, when the provider reported one.",
    ),
    (
        "tool_call_ids",
        "The tool calls making up this row, in order.",
    ),
    (
        "tool_call_count",
        "How many tool calls this row contains. Computed with the retraction rules applied — do not re-derive it with COUNT(*).",
    ),
    (
        "objective",
        "What the work was for, in the human's own words. Content: null under `metadata_only`.",
    ),
    (
        "first_event_id",
        "First event of the row, in observation order.",
    ),
    (
        "last_event_id",
        "Last event of the row, in observation order.",
    ),
    (
        "start_event_id",
        "The event that opened the row, when one was observed.",
    ),
    (
        "end_event_id",
        "The event that closed the row, when one was observed.",
    ),
    (
        "exit_code",
        "Process exit status, when the provider reported one.",
    ),
    ("duration_ms", "Wall-clock milliseconds."),
    (
        "sha",
        "The commit sha. Null when the commit could not be resolved to one.",
    ),
    ("previous_sha", "What `HEAD` pointed at before the commit."),
];

const EVENTS: &[(&str, &str)] = &[
    (
        "event_id",
        "UUIDv7 of the event (`ev_…`). Immutable, unique, and the only thing an inference ever cites.",
    ),
    (
        "schema_version",
        "Event schema version this row was written under (`spec/event-v1.schema.json`).",
    ),
    (
        "device_id",
        "The machine that captured it (`dev_…`). One database can hold several devices after a sync or an import.",
    ),
    (
        "source_seq",
        "Per-device monotonic sequence. Together with `device_id` it makes the event's arrival order total.",
    ),
    (
        "hlc",
        "Hybrid logical clock: orders events across devices when wall clocks disagree.",
    ),
    (
        "observed_at",
        "When the agent did the thing. Order history by this.",
    ),
    (
        "captured_at",
        "When the hook recorded it. `captured_at - observed_at` is capture lag.",
    ),
    (
        "ingested_at",
        "When the database accepted it. Null for events read straight from a segment.",
    ),
    (
        "provider",
        "Which coding agent. `attemptdb` marks events AttemptDB wrote itself (corrections, retractions, capture tests); any other adapter contributes its own identifier, so match the values you know rather than assuming the list is closed.",
    ),
    (
        "provider_version",
        "The agent's own version string, when it reported one.",
    ),
    (
        "adapter_version",
        "The AttemptDB adapter that normalised the payload. Changes here can change every derived row.",
    ),
    (
        "hook_version",
        "The hook binary that captured it. Null for events reconstructed from a transcript.",
    ),
    (
        "capture_mode",
        "The privacy mode in force when the event was written. Under `metadata_only` every content column below is null by design.",
    ),
    (
        "provider_event_name",
        "The provider's own name for the hook that fired, before normalisation.",
    ),
    (
        "kind",
        "The canonical event kind. This is the column to filter on; `provider_event_name` is provider-specific.",
    ),
    (
        "project_root",
        "Absolute path of the repository root on the capturing machine.",
    ),
    (
        "repo_remote",
        "Git remote URL, when the repository has one.",
    ),
    (
        "head",
        "The commit `HEAD` pointed at when the event was observed.",
    ),
    (
        "session_id",
        "The agent session (`ses_…`), derived from the provider's own session id.",
    ),
    (
        "provider_session_id",
        "The provider's own session identifier, as it wrote it.",
    ),
    (
        "provider_turn_id",
        "The provider's own turn identifier, when it has one.",
    ),
    (
        "span_id",
        "The tool call this event belongs to (`tc_…`): started and finished events of one call share it.",
    ),
    ("parent_span_id", "The enclosing span, for nested calls."),
    (
        "agent_id",
        "Which agent instance acted (`agt_…`): a subagent has its own.",
    ),
    (
        "agent_type",
        "The agent's role as the provider named it — the main loop, or a named subagent.",
    ),
    ("parent_agent_id", "The agent that spawned this one."),
    ("model", "Model name the provider reported for the turn."),
    ("provider_agent_id", "The provider's own agent identifier."),
    (
        "tool_name",
        "The tool as the provider named it (`Bash`, `Edit`, `shell`, …).",
    ),
    (
        "tool_category",
        "The normalised category. Filter on this to compare providers.",
    ),
    (
        "tool_call_id",
        "The provider's own call id, when it issues one.",
    ),
    (
        "path_logical",
        "The path as the agent wrote it, absolute or not.",
    ),
    (
        "path_relative",
        "The same path relative to the repository root. Filter on this: it is stable across machines.",
    ),
    (
        "paths_json",
        "Every path the call touched, as a JSON array of repository-relative strings.",
    ),
    ("outcome_status", "How the call ended."),
    (
        "outcome_class",
        "A coarser reason, when the provider gave one.",
    ),
    (
        "attrs_json",
        "The metadata allowlist as JSON (RFC 0006 §4). Content-free by construction: anything that could carry text is rejected before it is written.",
    ),
    (
        "content_json",
        "Prompt, message and command text. Content: null under `metadata_only`, and moved to an encrypted blob when a key exists.",
    ),
    (
        "raw_json",
        "The provider's original payload. Content, same rules as `content_json`.",
    ),
    (
        "content_ref",
        "Blob id holding `content_json` when it was written out of line and encrypted.",
    ),
    ("raw_ref", "Blob id holding `raw_json`, same."),
    (
        "unknown_json",
        "Fields the adapter did not recognise, kept verbatim so an upgrade can read them. Never silently dropped.",
    ),
    (
        "retracted",
        "True when a Retraction covers this event or its session.",
    ),
];

const SESSIONS: &[(&str, &str)] = &[
    ("session_id", "The session (`ses_…`)."),
    (
        "provider_session_id",
        "The provider's own session id, for cross-checking against its logs.",
    ),
    (
        "state",
        "Whether an end event was observed. `open` also covers a session that was killed without one.",
    ),
    ("end_reason", "Why it ended, as the provider reported it."),
    (
        "start_source",
        "How the session began, as the provider reported it — a fresh start, a resume, a compaction.",
    ),
    ("event_count", "Events in the session."),
    ("turn_count", "Turns in the session."),
    ("prompt_count", "Prompts the human submitted."),
    ("failure_count", "Tool calls that ended in failure."),
    (
        "agents",
        "Every agent instance that acted in the session, main loop and subagents.",
    ),
    (
        "coverage",
        "How complete the capture is. `full` means hooks recorded everything; `partial` and `minimal` mean some of this session was reconstructed from a transcript, so absence of a row is not evidence of absence.",
    ),
    (
        "last_event_at",
        "When the newest event of the session was observed. This, not `ended_at`, is what tells you a session is still live.",
    ),
];

const TURNS: &[(&str, &str)] = &[
    ("turn_id", "The turn (`trn_…`)."),
    ("turn_index", "Position of the turn in its session, from 0."),
    ("status", "How the turn ended."),
    ("prompt_event_id", "The prompt that opened the turn."),
    ("stop_event_id", "The event that closed it."),
    (
        "prompt_chars",
        "Length of the prompt in characters. Metadata: present even under `metadata_only`, where `objective` is null.",
    ),
    (
        "inferred_objective",
        "The objective as the projector read it, kept when a human correction replaced `objective`. The two together are the audit trail.",
    ),
];

const TOOL_CALLS: &[(&str, &str)] = &[
    ("tool_call_id", "The tool call (`tc_…`)."),
    ("agent_id", "Which agent instance made the call."),
    ("tool_name", "The tool as the provider named it."),
    (
        "tool_category",
        "The normalised category — compare providers on this, not on `tool_name`.",
    ),
    ("provider_call_id", "The provider's own call id."),
    (
        "started_at",
        "When the call started. Null when only its completion was observed.",
    ),
    (
        "finished_at",
        "When it returned. Null while it is still running.",
    ),
    (
        "duration_ms",
        "Wall-clock duration. Null unless both ends were observed.",
    ),
    (
        "outcome_status",
        "How it ended. Null while it is still running.",
    ),
    ("outcome_class", "A coarser reason for the outcome."),
    ("path_relative", "The primary path, repository-relative."),
    (
        "command_category",
        "What a shell command was doing, classified from the command line.",
    ),
    (
        "git_subcommand",
        "For a git call, the subcommand (`commit`, `push`, …).",
    ),
    (
        "lines_added",
        "Lines added, when the provider reported a diff.",
    ),
    ("lines_removed", "Lines removed, same."),
];

const ATTEMPTS: &[(&str, &str)] = &[
    ("attempt_id", "The attempt (`att_…`)."),
    (
        "turn_index",
        "Position of the enclosing turn in its session.",
    ),
    (
        "attempt_index",
        "Position of this attempt within its turn, from 0. Attempt 1 after a failed attempt 0 is a retry.",
    ),
    (
        "approach",
        "How the attempt went about it, classified from the tool calls it used.",
    ),
    (
        "outcome",
        "How the attempt ended. `superseded` means a later attempt in the same turn replaced it — that is a retry, not an independent failure.",
    ),
    (
        "failure_class",
        "What kind of failure, when it failed. Open vocabulary: two failures of the same class are the signal that something is stuck.",
    ),
    ("superseded_by", "The attempt that replaced this one."),
    ("supersedes", "The attempt this one replaced."),
    (
        "inferred_outcome",
        "The outcome the projector derived, kept when a human correction replaced `outcome`.",
    ),
    (
        "inferred_failure_class",
        "The failure class the projector derived, kept for the same reason.",
    ),
    (
        "note",
        "A human's note from a Correction. Content: null under `metadata_only`.",
    ),
];

const HANDOFFS: &[(&str, &str)] = &[
    ("from_session", "The session that stopped (`ses_…`)."),
    ("to_session", "The session that picked the work up."),
    ("from_provider", "Agent that stopped."),
    ("to_provider", "Agent that continued."),
    ("handoff_at", "When the second session started."),
    (
        "gap_ms",
        "Milliseconds between the last event of the first session and the first of the second. A large gap weakens the inference.",
    ),
    (
        "shared_paths",
        "Paths both sessions touched. This overlap is why the handoff was inferred at all.",
    ),
];

const EDGES: &[(&str, &str)] = &[
    (
        "ordinal",
        "Position in the edge list. A stable handle, not a meaning.",
    ),
    ("edge_kind", "What the edge asserts."),
    ("from_type", "Kind of entity the edge starts at."),
    ("from_id", "Prefixed id of that entity."),
    ("to_type", "Kind of entity the edge ends at."),
    ("to_id", "Prefixed id of that entity."),
    (
        "edge_source",
        "`projection` for edges the projector wrote; `derived` for edges the causal graph added on top of them.",
    ),
];

const SIGNALS: &[(&str, &str)] = &[
    ("event_id", "The event that raised the signal."),
    ("raised_at", "When it was raised."),
    (
        "kind",
        "What kind of signal. A permission request is the agent waiting on a human.",
    ),
    (
        "signal_type",
        "The provider's own label for a notification. Free text: there is no fixed set to match against.",
    ),
    (
        "cleared_at",
        "When the next event in the session arrived, which is what ends the wait. Null while it is still pending.",
    ),
    ("cleared_by", "The event that cleared it."),
    (
        "pending",
        "True while nothing has cleared it. A pending signal in an open session is a human being waited on.",
    ),
];

const WORK_UNITS: &[(&str, &str)] = &[
    (
        "work_unit_id",
        "The work unit (`wu_…`): one thread of work, which may span sessions, agents and days.",
    ),
    (
        "version",
        "How many times the unit has been revised. It grows as evidence arrives.",
    ),
    (
        "objective_event_id",
        "The prompt the objective was read from.",
    ),
    (
        "objective",
        "What the unit is for. Content: null under `metadata_only`.",
    ),
    (
        "phase",
        "Where the work stands. Inferred from the recent tool mix and outcomes, so read `phase_reason` with it.",
    ),
    (
        "phase_reason",
        "Why that phase was chosen, in one sentence.",
    ),
    ("status", "Whether the unit is still open."),
    ("status_reason", "Why that status was chosen."),
    ("sessions", "Every session that contributed."),
    ("session_count", "How many."),
    ("turns", "Every turn that contributed."),
    ("turn_count", "How many."),
    ("attempts", "Every attempt in the unit."),
    ("attempt_count", "How many."),
    (
        "failed_attempt_count",
        "How many of them failed. Two failures of the same class with no success after is the repeated-failure signal.",
    ),
    (
        "actors",
        "The agents that worked on it — more than one means the work was handed off.",
    ),
    ("last_attempt", "The most recent attempt (`att_…`)."),
    (
        "blocking_signal",
        "The event id of the signal holding the unit up, when one is pending.",
    ),
];

const DECISIONS: &[(&str, &str)] = &[
    ("decision_id", "The decision (`dec_…`)."),
    (
        "kind",
        "What kind of decision. `human_intervention` is a human changing the direction; `approach_change` is the agent abandoning one approach for another.",
    ),
    (
        "selected",
        "What was chosen, as a prefixed id or a short label.",
    ),
    (
        "alternatives",
        "What was not chosen, and had evidence behind it.",
    ),
    (
        "rationale",
        "Why, in one sentence, derived from what happened around it.",
    ),
    (
        "rationale_source",
        "How the rationale was produced. Always `derived`: nobody typed it.",
    ),
    ("decided_at", "When the decision was observed."),
];

const COMMITS: &[(&str, &str)] = &[
    (
        "commit_id",
        "The commit row (`cmt_…`). Not the sha: one row per observed `git commit` call, resolved or not.",
    ),
    ("committed_at", "When the commit call finished."),
    (
        "linkage",
        "How the sha was tied to the call. `end_event` means the call itself reported it; `next_head` means the sha was read from the next observed HEAD change, which is weaker; `unresolved` means no sha was found and `sha` is null.",
    ),
];

const CORRECTIONS: &[(&str, &str)] = &[
    (
        "event_id",
        "The Correction event (`ev_…`). A correction is itself an immutable fact, never an edit of the row it corrects.",
    ),
    ("corrected_at", "When the human wrote it."),
    ("session_id", "The session the correction was written into."),
    (
        "outcome",
        "The outcome the human asserted, for an `attempt_outcome` correction.",
    ),
    ("failure_class", "The failure class the human asserted."),
    (
        "status",
        "Whether the correction found its target and took effect.",
    ),
];

const RETRACTIONS: &[(&str, &str)] = &[
    ("event_id", "The Retraction event (`ev_…`)."),
    ("retracted_at", "When it was written."),
    ("reason", "Why the data was retracted."),
    ("matched", "Whether the target was found."),
    (
        "retracted_events",
        "How many events left the projections as a result. The facts stay in the log.",
    ),
];

const CONFLICTS: &[(&str, &str)] = &[
    ("conflict_id", "The conflict row."),
    (
        "first_work_unit",
        "The work unit that started first (`wu_…`).",
    ),
    ("second_work_unit", "The one that started later."),
    ("first_started_at", "When the first unit started."),
    ("second_started_at", "When the second started."),
    ("started_at", "When the overlap began."),
    (
        "updated_at",
        "When the newest evidence for the overlap arrived.",
    ),
    (
        "paths",
        "The files both units touched. This overlap is the conflict.",
    ),
    ("path_count", "How many."),
    (
        "overlapping",
        "True while both units are still open: two agents editing the same files right now.",
    ),
    (
        "first_committed",
        "Whether the first unit has committed the shared paths.",
    ),
    ("second_committed", "Whether the second has."),
    (
        "first_lines_added",
        "Lines the first unit added to the shared paths.",
    ),
    ("first_lines_removed", "Lines it removed."),
    ("second_lines_added", "Lines the second unit added."),
    ("second_lines_removed", "Lines it removed."),
];

fn table_docs(table: &str) -> &'static [(&'static str, &'static str)] {
    match table {
        "events" | "events_raw" => EVENTS,
        "sessions" => SESSIONS,
        "turns" => TURNS,
        "tool_calls" => TOOL_CALLS,
        "attempts" => ATTEMPTS,
        "handoffs" => HANDOFFS,
        "edges" => EDGES,
        "signals" => SIGNALS,
        "work_units" => WORK_UNITS,
        "decisions" => DECISIONS,
        "commits" => COMMITS,
        "corrections" => CORRECTIONS,
        "retractions" => RETRACTIONS,
        "conflicts" => CONFLICTS,
        _ => &[],
    }
}

/// The meaning of `table.column`, or `None` when nothing documents it.
fn doc_of(table: &str, column: &str) -> Option<&'static str> {
    let find = |m: &'static [(&'static str, &'static str)]| {
        m.iter().find(|(c, _)| *c == column).map(|(_, d)| *d)
    };
    find(table_docs(table)).or_else(|| find(COMMON))
}

// ---------------------------------------------------------------------------
// Table meanings
// ---------------------------------------------------------------------------

struct Meta {
    layer: Layer,
    grain: &'static str,
    summary: &'static str,
    joins: &'static [Join],
}

const J_EVENTS: &[Join] = &[
    Join {
        column: "session_id",
        target: "sessions.session_id",
    },
    Join {
        column: "span_id",
        target: "tool_calls.tool_call_id",
    },
];
const J_TURNS: &[Join] = &[Join {
    column: "session_id",
    target: "sessions.session_id",
}];
const J_TOOL_CALLS: &[Join] = &[
    Join {
        column: "session_id",
        target: "sessions.session_id",
    },
    Join {
        column: "turn_id",
        target: "turns.turn_id",
    },
];
const J_ATTEMPTS: &[Join] = &[
    Join {
        column: "session_id",
        target: "sessions.session_id",
    },
    Join {
        column: "turn_id",
        target: "turns.turn_id",
    },
    Join {
        column: "work_unit_id",
        target: "work_units.work_unit_id",
    },
    Join {
        column: "superseded_by",
        target: "attempts.attempt_id",
    },
];
const J_HANDOFFS: &[Join] = &[
    Join {
        column: "from_session",
        target: "sessions.session_id",
    },
    Join {
        column: "to_session",
        target: "sessions.session_id",
    },
];
const J_SIGNALS: &[Join] = &[
    Join {
        column: "session_id",
        target: "sessions.session_id",
    },
    Join {
        column: "event_id",
        target: "events.event_id",
    },
];
const J_DECISIONS: &[Join] = &[
    Join {
        column: "session_id",
        target: "sessions.session_id",
    },
    Join {
        column: "turn_id",
        target: "turns.turn_id",
    },
    Join {
        column: "work_unit_id",
        target: "work_units.work_unit_id",
    },
];
const J_COMMITS: &[Join] = &[
    Join {
        column: "session_id",
        target: "sessions.session_id",
    },
    Join {
        column: "turn_id",
        target: "turns.turn_id",
    },
    Join {
        column: "attempt_id",
        target: "attempts.attempt_id",
    },
    Join {
        column: "tool_call_id",
        target: "tool_calls.tool_call_id",
    },
];
const J_CORRECTIONS: &[Join] = &[
    Join {
        column: "event_id",
        target: "events.event_id",
    },
    Join {
        column: "session_id",
        target: "sessions.session_id",
    },
];
const J_RETRACTIONS: &[Join] = &[Join {
    column: "event_id",
    target: "events.event_id",
}];
const J_CONFLICTS: &[Join] = &[
    Join {
        column: "first_work_unit",
        target: "work_units.work_unit_id",
    },
    Join {
        column: "second_work_unit",
        target: "work_units.work_unit_id",
    },
];

fn meta(name: &str) -> Meta {
    match name {
        "events" => Meta {
            layer: Layer::Fact,
            grain: "observed event",
            summary: "The log. Every row was written by a hook (or reconstructed from an agent's own transcript) and is immutable: nothing in AttemptDB ever updates an event. Start here when a derived row looks wrong, and finish here when a claim needs proof — every inference cites these ids in its `evidence`.",
            joins: J_EVENTS,
        },
        "events_raw" => Meta {
            layer: Layer::Fact,
            grain: "observed event, in storage types",
            summary: "The same stream as `events` with the on-disk types instead of readable ones: 16-byte UUIDs rather than `ev_…` strings, dictionary-encoded providers and kinds. Read it when you are checking the storage layer or comparing against a segment; read `events` for everything else.",
            joins: &[],
        },
        "sessions" => Meta {
            layer: Layer::Inference,
            grain: "agent session",
            summary: "One run of a coding agent, from the first event that named a session id to the last. Whether it is still open is `state`; whether it is still alive is `last_event_at`, because agents are killed far more often than they exit.",
            joins: &[],
        },
        "turns" => Meta {
            layer: Layer::Inference,
            grain: "human prompt and the agent's response to it",
            summary: "The unit a human recognises: what was asked, and everything the agent did before it stopped. `objective` is the ask in the human's words when content was captured; `prompt_chars` is there when it was not.",
            joins: J_TURNS,
        },
        "tool_calls" => Meta {
            layer: Layer::Inference,
            grain: "tool invocation",
            summary: "A started and a finished event paired into one call, with its path, its duration and how it ended. A call with `finished_at IS NULL` is still running — or its completion was never captured, which `sessions.coverage` tells you.",
            joins: J_TOOL_CALLS,
        },
        "attempts" => Meta {
            layer: Layer::Inference,
            grain: "contiguous run of tool calls pursuing one objective",
            summary: "The table this database is named for: what the agent tried. Several attempts in one turn mean it tried, failed and tried again — `attempt_index`, `supersedes` and `superseded_by` are the retry chain. `outcome = 'superseded'` is a retry, not an independent failure; counting it as one double-counts.",
            joins: J_ATTEMPTS,
        },
        "handoffs" => Meta {
            layer: Layer::Inference,
            grain: "session picking up where another stopped",
            summary: "Two sessions, usually two different agents, touching the same files across a gap. `gap_ms` and `shared_paths` are the whole basis of the inference: a long gap with one shared file is weak evidence and the `confidence` says so.",
            joins: J_HANDOFFS,
        },
        "edges" => Meta {
            layer: Layer::Inference,
            grain: "causal or structural link between two entities",
            summary: "The graph `WHY` and `TRACE` walk. Endpoints are polymorphic: `from_type`/`to_type` name the table and `from_id`/`to_id` its prefixed id, so join by writing the type into the condition. `edge_source` separates edges the projector asserted from edges the causal layer derived on top of them.",
            joins: &[],
        },
        "signals" => Meta {
            layer: Layer::Inference,
            grain: "moment the agent needed a human",
            summary: "Permission requests, denials and notifications, each with the event that cleared it. A row with `pending = true` in an open session is an agent waiting right now — this is the fact behind Needs You.",
            joins: J_SIGNALS,
        },
        "work_units" => Meta {
            layer: Layer::Inference,
            grain: "thread of work, across sessions and agents",
            summary: "What a human would call a task: an objective, the sessions and attempts spent on it, where it stands. It survives session boundaries, agent switches and days, which is what makes it the right grain for \"what is going on in this repository\". `phase` and `status` are inferences with reasons attached — quote the reason, not just the label.",
            joins: &[],
        },
        "decisions" => Meta {
            layer: Layer::Inference,
            grain: "point where the direction changed",
            summary: "An agent abandoning one approach for another, or a human stepping in. `rationale` is derived from what happened around the change, never typed by anyone, and `rationale_source` says so.",
            joins: J_DECISIONS,
        },
        "commits" => Meta {
            layer: Layer::Inference,
            grain: "observed `git commit` call",
            summary: "Where the work landed. One row per commit call, resolved to a sha or not: `linkage` says how confident the tie is, and `sha IS NULL` means the call was seen but the sha never was.",
            joins: J_COMMITS,
        },
        "corrections" => Meta {
            layer: Layer::Inference,
            grain: "human correction of an inference",
            summary: "A human saying the projector got it wrong. The correction is itself an immutable event; it never edits the row it corrects, which keeps both readings — see `attempts.outcome` against `attempts.inferred_outcome`. `status` says whether it found its target.",
            joins: J_CORRECTIONS,
        },
        "retractions" => Meta {
            layer: Layer::Inference,
            grain: "human retraction",
            summary: "A session, attempt or event removed from every projection — benchmarks, tests, mistaken imports, privacy. The facts stay in the log; the projections behave as if they never happened. `retracted_events` counts what left.",
            joins: J_RETRACTIONS,
        },
        "conflicts" => Meta {
            layer: Layer::Inference,
            grain: "pair of work units touching the same files",
            summary: "Two threads of work over the same paths. `overlapping = true` means both are still open: two agents editing the same files right now, which is worth interrupting someone over. The committed flags and line counts say how far each has gone.",
            joins: J_CONFLICTS,
        },
        _ => Meta {
            layer: Layer::Inference,
            grain: "row",
            summary: "",
            joins: &[],
        },
    }
}

// ---------------------------------------------------------------------------
// Assembly
// ---------------------------------------------------------------------------

fn schema_of(name: &str) -> SchemaRef {
    match name {
        "events" => readable_events_schema(),
        "events_raw" => attemptdb_storage::segment::events_schema(),
        n => projection_schema(n),
    }
}

/// One table, its real schema merged with its documentation.
pub fn table(name: &str) -> Option<Table> {
    let name = crate::TABLE_NAMES.iter().find(|t| **t == name)?;
    let m = meta(name);
    let schema = schema_of(name);
    let columns = schema
        .fields()
        .iter()
        .map(|f| {
            let (values, open) = values_of(name, f.name());
            Column {
                name: f.name().clone(),
                data_type: type_name(f.data_type()),
                nullable: f.is_nullable(),
                doc: doc_of(name, f.name()).unwrap_or(""),
                values,
                open,
            }
        })
        .collect();
    Some(Table {
        name,
        layer: m.layer,
        grain: m.grain,
        summary: m.summary,
        joins: m.joins,
        columns,
    })
}

/// Every queryable table, in registration order.
pub fn catalog() -> Vec<Table> {
    crate::TABLE_NAMES.iter().filter_map(|n| table(n)).collect()
}

/// Questions people actually ask, and the statement that answers each.
pub fn examples() -> &'static [Example] {
    EXAMPLES
}

const EXAMPLES: &[Example] = &[
    Example {
        question: "What is going on in this repository right now?",
        statement: "WHAT IS project DOING NOW",
        note: "Open work units with their phase and latest attempt. Answers with `insufficient_evidence` rather than guessing when nothing is active.",
    },
    Example {
        question: "What did the agents try?",
        statement: "SHOW ATTEMPTS ORDER BY started_at DESC LIMIT 20",
        note: "The default view of the database. Add `INCLUDING RETRACTED` to see what a retraction removed.",
    },
    Example {
        question: "What failed?",
        statement: "SHOW FAILED ATTEMPTS",
        note: "`outcome = 'failed'` only. Superseded attempts are retries and are excluded here on purpose.",
    },
    Example {
        question: "What failed in one file?",
        statement: "SHOW FAILED ATTEMPTS FOR path = 'crates/*/src/*.rs'",
        note: "`path` matches against the attempt's `paths` list; `*` is a glob.",
    },
    Example {
        question: "Which attempts were retried?",
        statement: "SHOW SUPERSEDED ATTEMPTS",
        note: "Each row is an attempt a later one replaced, with the successor's outcome — the retry chain, not a list of failures.",
    },
    Example {
        question: "Where did work pass between agents?",
        statement: "SHOW HANDOFFS",
        note: "Read `gap_ms` and `shared_paths` before believing a handoff: they are the evidence.",
    },
    Example {
        question: "Why is this session stuck?",
        statement: "WHY session {session} STATUS BLOCKED",
        note: "Answers from pending signals and repeated same-class failures, and says `state_mismatch` when the session is not blocked at all.",
    },
    Example {
        question: "What caused this attempt?",
        statement: "TRACE attempt {attempt} CAUSES DEPTH 3",
        note: "Walks the causal edges upward. `DIRECTION DOWN` walks to consequences instead.",
    },
    Example {
        question: "What is this claim based on?",
        statement: "SHOW EVIDENCE FOR attempt {attempt}",
        note: "The events the inference was built from, in observation order. Every derived row can be opened this way.",
    },
    Example {
        question: "What did the repository look like yesterday?",
        statement: "STATE project AT '-1d'",
        note: "Sessions and work units as they stood at that moment, with outcomes known only up to then.",
    },
    Example {
        question: "What changed since yesterday?",
        statement: "DIFF STATE '-1d' NOW",
        note: "One row per changed field. Units that completed in between show as `removed` with their final state.",
    },
    Example {
        question: "Which failures repeat?",
        statement: "SELECT failure_class, count(*) AS failures FROM attempts WHERE outcome = 'failed' AND retracted = false GROUP BY failure_class ORDER BY failures DESC",
        note: "`failure_class` is an open vocabulary: two attempts sharing one is the signal that something is genuinely stuck.",
    },
    Example {
        question: "Which work is stuck?",
        statement: "SELECT work_unit_id, phase, phase_reason, failed_attempt_count FROM work_units WHERE status = 'open' AND failed_attempt_count >= 2 ORDER BY updated_at DESC",
        note: "Quote `phase_reason` with the phase: the label alone is an inference presented as fact.",
    },
    Example {
        question: "Is anyone waiting on me?",
        statement: "SELECT s.session_id, s.provider, g.kind, g.raised_at FROM signals g JOIN sessions s ON s.session_id = g.session_id WHERE g.pending = true AND s.state = 'open' ORDER BY g.raised_at",
        note: "A pending signal in an open session is an agent waiting on a human right now.",
    },
    Example {
        question: "How do the agents differ in what they run?",
        statement: "SELECT provider, tool_category, count(*) AS calls FROM tool_calls WHERE retracted = false GROUP BY provider, tool_category ORDER BY calls DESC",
        note: "Compare on `tool_category`, never on `tool_name`: every provider names its tools differently.",
    },
    Example {
        question: "What is slow?",
        statement: "SELECT tool_name, path_relative, duration_ms FROM tool_calls WHERE duration_ms IS NOT NULL ORDER BY duration_ms DESC LIMIT 10",
        note: "Null `duration_ms` means one end of the call was never observed, not that it was instant.",
    },
    Example {
        question: "How much of this history is actually captured?",
        statement: "SELECT provider, sum(CASE WHEN hook_version IS NULL THEN 1 ELSE 0 END) AS reconstructed, count(*) AS events FROM events WHERE retracted = false GROUP BY provider",
        note: "Rows with no `hook_version` were reconstructed from a transcript after the fact. Their absence of a detail is not evidence of absence.",
    },
    Example {
        question: "What did the agents commit?",
        statement: "SELECT committed_at, sha, branch, linkage FROM commits WHERE sha IS NOT NULL ORDER BY committed_at DESC LIMIT 20",
        note: "`linkage = 'next_head'` is a weaker tie than `end_event`; `sha IS NULL` means the commit call was seen but its sha never was.",
    },
    Example {
        question: "Are two agents editing the same files?",
        statement: "SELECT conflict_id, path_count, overlapping, first_committed, second_committed FROM conflicts WHERE overlapping = true",
        note: "`overlapping = true` means both work units are still open — the case worth interrupting someone over.",
    },
    Example {
        question: "What have humans corrected?",
        statement: "SELECT corrected_at, correction_type, target, status FROM corrections ORDER BY corrected_at DESC",
        note: "The audit trail of where the projector was wrong. `status` says whether the correction found its target.",
    },
    Example {
        question: "What kinds of events are in here at all?",
        statement: "SELECT kind, count(*) AS events FROM events WHERE retracted = false GROUP BY kind ORDER BY events DESC",
        note: "The first query to run against an unfamiliar database: it shows what the hooks actually captured.",
    },
    Example {
        question: "What columns does this table have?",
        statement: "DESCRIBE attempts",
        note: "Types straight from the schema. `attempt schema` adds what they mean.",
    },
    Example {
        question: "How will this query run?",
        statement: "EXPLAIN SELECT count(*) FROM events WHERE kind = 'tool_call_failed'",
        note: "The DataFusion plan, including which filters were pushed into the segment scan.",
    },
];

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// One-paragraph answer to "what is this database?", for a reader who
/// arrived with no context at all.
pub const OVERVIEW: &str = "AttemptDB records what coding agents tried. A hook on each agent (Claude Code, Codex, Cursor, Gemini CLI) appends immutable events; a deterministic projector derives sessions, turns, tool calls, attempts, work units, decisions, handoffs and a causal graph from them. Queries run over both layers at once: the facts in `events`, and the inferences everywhere else, each carrying the event ids it was built from.";

fn fence(s: &str) -> String {
    format!("```\n{s}\n```")
}

fn values_cell(c: &Column) -> String {
    if c.values.is_empty() {
        return String::new();
    }
    let list = c
        .values
        .iter()
        .map(|v| format!("`{v}`"))
        .collect::<Vec<_>>()
        .join(", ");
    if c.open {
        format!(" Common values: {list} (open vocabulary — others appear).")
    } else {
        format!(" Values: {list}.")
    }
}

fn table_section(t: &Table) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "### `{}`\n", t.name);
    let _ = writeln!(out, "**{}** · one row per {}\n", t.layer.as_str(), t.grain);
    if !t.summary.is_empty() {
        let _ = writeln!(out, "{}\n", t.summary);
    }
    if !t.joins.is_empty() {
        let j = t
            .joins
            .iter()
            .map(|j| format!("`{}` → `{}`", j.column, j.target))
            .collect::<Vec<_>>()
            .join(" · ");
        let _ = writeln!(out, "Joins: {j}\n");
    }
    let _ = writeln!(out, "| column | type | null | meaning |");
    let _ = writeln!(out, "|---|---|---|---|");
    for c in &t.columns {
        let _ = writeln!(
            out,
            "| `{}` | {} | {} | {}{} |",
            c.name,
            c.data_type,
            if c.nullable { "yes" } else { "" },
            c.doc,
            values_cell(c)
        );
    }
    out
}

/// The whole catalog as the document checked in at `docs/query-context.md`.
pub fn markdown() -> String {
    let mut out = String::new();
    let _ = writeln!(out, "# Query context\n");
    let _ = writeln!(
        out,
        "*Generated from the code by `attempt schema --format markdown`. \
         Do not edit by hand: `cargo test -p attemptdb-query --test catalog` \
         fails when this file and the schema disagree, and \
         `UPDATE_GOLDEN=1` regenerates it.*\n"
    );
    let _ = writeln!(out, "{OVERVIEW}\n");
    let _ = writeln!(out, "## Rules\n");
    for (i, r) in RULES.iter().enumerate() {
        let _ = writeln!(out, "{}. {r}\n", i + 1);
    }
    let _ = writeln!(out, "## Tables\n");
    for t in catalog() {
        let _ = writeln!(out, "{}", table_section(&t));
    }
    let _ = writeln!(out, "## Example questions\n");
    let _ = writeln!(
        out,
        "Placeholders ({}) stand for a real id; substitute one before running.\n",
        PLACEHOLDERS
            .iter()
            .map(|p| format!("`{p}`"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    for e in EXAMPLES {
        let _ = writeln!(out, "**{}**\n", e.question);
        let _ = writeln!(out, "{}\n", fence(e.statement));
        let _ = writeln!(out, "{}\n", e.note);
    }
    out
}

/// One table's section, for `attempt schema --table <name>`.
pub fn markdown_table(name: &str) -> Option<String> {
    table(name).map(|t| table_section(&t))
}

/// The catalog as JSON, for a caller that would rather parse than read.
pub fn json() -> Value {
    json!({
        "overview": OVERVIEW,
        "rules": RULES,
        "placeholders": PLACEHOLDERS,
        "tables": catalog().iter().map(|t| json!({
            "name": t.name,
            "layer": t.layer.as_str(),
            "grain": t.grain,
            "summary": t.summary,
            "joins": t.joins.iter().map(|j| json!({ "column": j.column, "target": j.target })).collect::<Vec<_>>(),
            "columns": t.columns.iter().map(|c| json!({
                "name": c.name,
                "type": c.data_type,
                "nullable": c.nullable,
                "doc": c.doc,
                "values": c.values,
                "open_vocabulary": c.open,
            })).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
        "examples": EXAMPLES.iter().map(|e| json!({
            "question": e.question,
            "statement": e.statement,
            "note": e.note,
        })).collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Exhaustiveness guards. Each of these stops compiling when a variant is
    // added to the enum, which is the signal to extend the list above it in
    // this module: a new value can never reach a user undocumented.
    #[allow(dead_code, clippy::too_many_arguments)]
    fn guards(
        p: Provider,
        k: EventKind,
        m: CaptureMode,
        c: ToolCategory,
        o: OutcomeStatus,
        g: CoverageGrade,
        t: TurnStatus,
        a: AttemptOutcome,
        e: EdgeKind,
        ph: Phase,
        w: WorkUnitStatus,
        d: DecisionKind,
        ct: CorrectionType,
        cs: CorrectionStatus,
        rr: RetractionReason,
        rt: RetractionTargetType,
    ) {
        match p {
            Provider::ClaudeCode
            | Provider::Codex
            | Provider::Cursor
            | Provider::GeminiCli
            // `Other` is why `provider` is an open vocabulary: any adapter
            // identifier can appear, including AttemptDB's own `attemptdb`.
            | Provider::Other(_) => {}
        }
        match k {
            EventKind::SessionStarted
            | EventKind::SessionEnded
            | EventKind::PromptSubmitted
            | EventKind::ToolCallStarted
            | EventKind::ToolCallFinished
            | EventKind::ToolCallFailed
            | EventKind::PermissionRequested
            | EventKind::PermissionDenied
            | EventKind::Notification
            | EventKind::AgentMessage
            | EventKind::TurnStopped
            | EventKind::TurnFailed
            | EventKind::SubagentStarted
            | EventKind::SubagentStopped
            | EventKind::TaskCreated
            | EventKind::TaskCompleted
            | EventKind::CompactionStarted
            | EventKind::CompactionFinished
            | EventKind::ConfigChanged
            | EventKind::CwdChanged
            | EventKind::FileChanged
            | EventKind::WorktreeCreated
            | EventKind::WorktreeRemoved
            | EventKind::Correction
            | EventKind::Retraction
            | EventKind::CaptureTest
            | EventKind::Unknown => {}
        }
        match m {
            CaptureMode::MetadataOnly | CaptureMode::LocalSemantic | CaptureMode::FullSync => {}
        }
        match c {
            ToolCategory::Shell
            | ToolCategory::FileRead
            | ToolCategory::FileWrite
            | ToolCategory::FileEdit
            | ToolCategory::Search
            | ToolCategory::Web
            | ToolCategory::Mcp
            | ToolCategory::Subagent
            | ToolCategory::Plan
            | ToolCategory::Notebook
            | ToolCategory::Other => {}
        }
        match o {
            OutcomeStatus::Success
            | OutcomeStatus::Failure
            | OutcomeStatus::Denied
            | OutcomeStatus::Cancelled
            | OutcomeStatus::Unknown => {}
        }
        match g {
            CoverageGrade::Full
            | CoverageGrade::Partial
            | CoverageGrade::Minimal
            | CoverageGrade::Unknown => {}
        }
        match t {
            TurnStatus::Completed
            | TurnStatus::Failed
            | TurnStatus::InProgress
            | TurnStatus::Unknown => {}
        }
        match a {
            AttemptOutcome::Succeeded
            | AttemptOutcome::Failed
            | AttemptOutcome::Abandoned
            | AttemptOutcome::Superseded
            | AttemptOutcome::InProgress
            | AttemptOutcome::Unknown => {}
        }
        match e {
            EdgeKind::ParentOf
            | EdgeKind::Caused
            | EdgeKind::Triggered
            | EdgeKind::Blocked
            | EdgeKind::Resolved
            | EdgeKind::Superseded
            | EdgeKind::Produced
            | EdgeKind::Verified
            | EdgeKind::Contradicted
            | EdgeKind::HandedOff
            | EdgeKind::EvidenceFor => {}
        }
        match ph {
            Phase::Explore
            | Phase::Plan
            | Phase::Implement
            | Phase::Debug
            | Phase::Verify
            | Phase::Review
            | Phase::Deliver
            | Phase::Blocked => {}
        }
        match w {
            WorkUnitStatus::Open
            | WorkUnitStatus::Completed
            | WorkUnitStatus::Abandoned
            | WorkUnitStatus::Unknown => {}
        }
        match d {
            DecisionKind::ApproachChange | DecisionKind::HumanIntervention => {}
        }
        match ct {
            CorrectionType::AttemptOutcome
            | CorrectionType::AttemptNote
            | CorrectionType::TurnObjective => {}
        }
        match cs {
            CorrectionStatus::Applied
            | CorrectionStatus::TargetNotFound
            | CorrectionStatus::TargetRetracted
            | CorrectionStatus::Invalid => {}
        }
        match rr {
            RetractionReason::Benchmark
            | RetractionReason::Test
            | RetractionReason::Duplicate
            | RetractionReason::MistakenImport
            | RetractionReason::Privacy
            | RetractionReason::Revoked
            | RetractionReason::Other => {}
        }
        match rt {
            RetractionTargetType::Session
            | RetractionTargetType::Event
            | RetractionTargetType::Attempt => {}
        }
    }

    #[test]
    fn the_lists_match_the_enums() {
        assert_eq!(event_kinds().len(), EventKind::ALL.len());
        assert_eq!(phases().len(), Phase::ALL.len());
        assert_eq!(work_unit_statuses().len(), WorkUnitStatus::ALL.len());
        assert_eq!(correction_types().len(), CorrectionType::ALL.len());
        assert_eq!(retraction_reasons().len(), RetractionReason::ALL.len());
        // `attemptdb` is not a Provider variant of its own: it arrives as
        // `Provider::Other("attemptdb")` on the events AttemptDB writes
        // itself, and a reader filtering `provider` has to know it exists.
        assert!(providers().contains(&"attemptdb".to_string()));
    }

    #[test]
    fn every_table_is_documented() {
        let c = catalog();
        assert_eq!(c.len(), crate::TABLE_NAMES.len());
        for t in &c {
            assert!(!t.summary.is_empty(), "{}: no summary", t.name);
            assert!(!t.grain.is_empty(), "{}: no grain", t.name);
            assert!(!t.columns.is_empty(), "{}: no columns", t.name);
        }
    }

    #[test]
    fn a_join_names_a_real_column_of_a_real_table() {
        let c = catalog();
        let has = |table: &str, column: &str| {
            c.iter()
                .find(|t| t.name == table)
                .is_some_and(|t| t.columns.iter().any(|col| col.name == column))
        };
        for t in &c {
            for j in t.joins {
                assert!(
                    has(t.name, j.column),
                    "{}.{}: no such column",
                    t.name,
                    j.column
                );
                let (table, column) = j.target.split_once('.').expect("table.column");
                assert!(
                    has(table, column),
                    "{}: no such target {}",
                    t.name,
                    j.target
                );
            }
        }
    }
}
