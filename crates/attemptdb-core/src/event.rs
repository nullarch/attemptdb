//! The canonical event model.
//!
//! An [`Event`] is an immutable observed fact. Adapters normalise provider
//! hook payloads into this shape; nothing downstream ever needs provider
//! specific knowledge to read the timeline. Provider details that do not fit
//! the common schema are preserved in [`Event::attrs`] (metadata) or
//! [`Event::content`] (content-bearing, capture-mode gated), and the original
//! payload can be retained through [`Event::raw`].
//!
//! Two invariants matter more than any individual field:
//!
//! 1. **Metadata and content are separated.** `attrs` only ever holds
//!    allowlisted, content-free metadata. Everything that could contain a
//!    prompt, a command line, file contents, or tool output lives in
//!    `content`, which is `None` in `metadata_only` capture mode.
//! 2. **Unknown fields survive.** Fields written by a newer schema are kept in
//!    [`Event::unknown`] and re-emitted on export.

use crate::clock::Hlc;
use crate::ids::{AgentId, DeviceId, EventId, ProjectId, SessionId, SpanId};
use crate::paths::PortablePath;
use crate::privacy::CaptureMode;
use crate::schema::CANONICAL_SCHEMA_VERSION;
use crate::time::Timestamp;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::fmt;
use std::str::FromStr;

/// The coding agent product that produced an event.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Provider {
    ClaudeCode,
    Codex,
    Cursor,
    GeminiCli,
    /// Any other provider; the string is the adapter's stable identifier.
    Other(String),
}

impl Provider {
    pub fn as_str(&self) -> &str {
        match self {
            Provider::ClaudeCode => "claude_code",
            Provider::Codex => "codex",
            Provider::Cursor => "cursor",
            Provider::GeminiCli => "gemini_cli",
            Provider::Other(s) => s.as_str(),
        }
    }

    /// Human-facing display name.
    pub fn display_name(&self) -> &str {
        match self {
            Provider::ClaudeCode => "Claude Code",
            Provider::Codex => "Codex",
            Provider::Cursor => "Cursor",
            Provider::GeminiCli => "Gemini CLI",
            Provider::Other(s) => s.as_str(),
        }
    }
}

impl fmt::Display for Provider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Provider {
    type Err = std::convert::Infallible;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(
            match s.trim().to_ascii_lowercase().replace('-', "_").as_str() {
                "claude_code" | "claude" | "claudecode" => Provider::ClaudeCode,
                "codex" | "codex_cli" => Provider::Codex,
                "cursor" => Provider::Cursor,
                "gemini_cli" | "gemini" => Provider::GeminiCli,
                other => Provider::Other(other.to_string()),
            },
        )
    }
}

impl Serialize for Provider {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for Provider {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Ok(s.parse().expect("infallible"))
    }
}

/// Canonical event kinds. Provider-specific names are mapped onto these; the
/// original name is always kept in [`Event::provider_event_name`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    SessionStarted,
    SessionEnded,
    PromptSubmitted,
    ToolCallStarted,
    ToolCallFinished,
    ToolCallFailed,
    PermissionRequested,
    PermissionDenied,
    Notification,
    AgentMessage,
    TurnStopped,
    TurnFailed,
    SubagentStarted,
    SubagentStopped,
    TaskCreated,
    TaskCompleted,
    CompactionStarted,
    CompactionFinished,
    ConfigChanged,
    CwdChanged,
    FileChanged,
    WorktreeCreated,
    WorktreeRemoved,
    /// A human correction of an inference, written by AttemptDB itself
    /// (`provider = "attemptdb"`). `attrs.correction_type` names what is
    /// corrected (`attempt_outcome`, `attempt_note`, `turn_objective`),
    /// `attrs.target` the projected entity; free text lives in
    /// `content.note` and is capture-mode gated like any content.
    Correction,
    /// A human retraction of a session, event, or attempt, written by
    /// AttemptDB itself. `attrs.target_type`, `attrs.target` and
    /// `attrs.reason` (a fixed enum: `benchmark`, `test`, `duplicate`,
    /// `mistaken_import`, `privacy`, `other`) are metadata; `content.note`
    /// is content. Retracted facts stay in the log but leave every
    /// projection and the sanitized export.
    Retraction,
    /// Emitted by `attempt hook install` / `attempt doctor` to verify wiring.
    CaptureTest,
    /// A provider event the adapter recognised as real but has no canonical
    /// mapping for. Never silently dropped.
    Unknown,
}

impl EventKind {
    pub fn as_str(self) -> &'static str {
        match self {
            EventKind::SessionStarted => "session_started",
            EventKind::SessionEnded => "session_ended",
            EventKind::PromptSubmitted => "prompt_submitted",
            EventKind::ToolCallStarted => "tool_call_started",
            EventKind::ToolCallFinished => "tool_call_finished",
            EventKind::ToolCallFailed => "tool_call_failed",
            EventKind::PermissionRequested => "permission_requested",
            EventKind::PermissionDenied => "permission_denied",
            EventKind::Notification => "notification",
            EventKind::AgentMessage => "agent_message",
            EventKind::TurnStopped => "turn_stopped",
            EventKind::TurnFailed => "turn_failed",
            EventKind::SubagentStarted => "subagent_started",
            EventKind::SubagentStopped => "subagent_stopped",
            EventKind::TaskCreated => "task_created",
            EventKind::TaskCompleted => "task_completed",
            EventKind::CompactionStarted => "compaction_started",
            EventKind::CompactionFinished => "compaction_finished",
            EventKind::ConfigChanged => "config_changed",
            EventKind::CwdChanged => "cwd_changed",
            EventKind::FileChanged => "file_changed",
            EventKind::WorktreeCreated => "worktree_created",
            EventKind::WorktreeRemoved => "worktree_removed",
            EventKind::Correction => "correction",
            EventKind::Retraction => "retraction",
            EventKind::CaptureTest => "capture_test",
            EventKind::Unknown => "unknown",
        }
    }

    pub const ALL: &'static [EventKind] = &[
        EventKind::SessionStarted,
        EventKind::SessionEnded,
        EventKind::PromptSubmitted,
        EventKind::ToolCallStarted,
        EventKind::ToolCallFinished,
        EventKind::ToolCallFailed,
        EventKind::PermissionRequested,
        EventKind::PermissionDenied,
        EventKind::Notification,
        EventKind::AgentMessage,
        EventKind::TurnStopped,
        EventKind::TurnFailed,
        EventKind::SubagentStarted,
        EventKind::SubagentStopped,
        EventKind::TaskCreated,
        EventKind::TaskCompleted,
        EventKind::CompactionStarted,
        EventKind::CompactionFinished,
        EventKind::ConfigChanged,
        EventKind::CwdChanged,
        EventKind::FileChanged,
        EventKind::WorktreeCreated,
        EventKind::WorktreeRemoved,
        EventKind::Correction,
        EventKind::Retraction,
        EventKind::CaptureTest,
        EventKind::Unknown,
    ];

    pub fn parse(s: &str) -> Option<Self> {
        EventKind::ALL.iter().copied().find(|k| k.as_str() == s)
    }
}

impl fmt::Display for EventKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Coarse tool classification shared across providers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCategory {
    Shell,
    FileRead,
    FileWrite,
    FileEdit,
    Search,
    Web,
    Mcp,
    Subagent,
    Plan,
    Notebook,
    Other,
}

impl ToolCategory {
    pub fn as_str(self) -> &'static str {
        match self {
            ToolCategory::Shell => "shell",
            ToolCategory::FileRead => "file_read",
            ToolCategory::FileWrite => "file_write",
            ToolCategory::FileEdit => "file_edit",
            ToolCategory::Search => "search",
            ToolCategory::Web => "web",
            ToolCategory::Mcp => "mcp",
            ToolCategory::Subagent => "subagent",
            ToolCategory::Plan => "plan",
            ToolCategory::Notebook => "notebook",
            ToolCategory::Other => "other",
        }
    }

    /// Whether the category mutates the working tree.
    pub fn mutates_files(self) -> bool {
        matches!(
            self,
            ToolCategory::FileWrite | ToolCategory::FileEdit | ToolCategory::Notebook
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolRef {
    /// Provider tool name exactly as reported (`Bash`, `apply_patch`, ...).
    pub name: String,
    pub category: ToolCategory,
    /// Provider tool-call identifier, when available (`tool_use_id`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutcomeStatus {
    Success,
    Failure,
    Denied,
    Cancelled,
    Unknown,
}

impl OutcomeStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            OutcomeStatus::Success => "success",
            OutcomeStatus::Failure => "failure",
            OutcomeStatus::Denied => "denied",
            OutcomeStatus::Cancelled => "cancelled",
            OutcomeStatus::Unknown => "unknown",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Outcome {
    pub status: OutcomeStatus,
    /// Content-free failure classification (`string_mismatch`,
    /// `file_not_found`, `nonzero_exit`, `timeout`, `interrupted`, ...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub class: Option<String>,
    /// Process exit code when the tool was a shell command.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
}

impl Outcome {
    pub fn success() -> Self {
        Self {
            status: OutcomeStatus::Success,
            class: None,
            exit_code: None,
        }
    }

    pub fn failure(class: impl Into<Option<String>>) -> Self {
        Self {
            status: OutcomeStatus::Failure,
            class: class.into(),
            exit_code: None,
        }
    }

    pub fn denied() -> Self {
        Self {
            status: OutcomeStatus::Denied,
            class: None,
            exit_code: None,
        }
    }
}

/// Which agent (or subagent) acted.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AgentRef {
    /// Canonical agent id. For the top-level agent of a session this is
    /// derived from the session id; for subagents from the provider agent id.
    #[serde(default, skip_serializing_if = "AgentId::is_nil")]
    pub agent_id: AgentId,
    /// Provider agent/subagent identifier (`agent_id` in Claude Code).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_agent_id: Option<String>,
    /// Agent type (`general-purpose`, `Explore`, a custom agent name...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_agent_id: Option<AgentId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

/// Project (repository) the event belongs to.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ProjectRef {
    pub project_id: ProjectId,
    /// Normalised logical root path (repository root or cwd).
    pub root: String,
    /// Human name: `owner/repo` from the remote, else the root basename.
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_remote: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
}

impl ProjectRef {
    /// Derive a project reference. Identity prefers the repository remote so
    /// the same repository cloned to two places (or two devices) maps to the
    /// same project; otherwise the normalised root path scoped by device.
    pub fn derive(root: &str, repo_remote: Option<&str>, device_id: &DeviceId) -> Self {
        let root_logical = PortablePath::from_raw(root, None).logical;
        let (project_id, name) = match repo_remote.map(normalise_remote) {
            Some(Some(remote)) => (
                ProjectId::derive(&["remote", &remote]),
                remote
                    .rsplit('/')
                    .take(2)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect::<Vec<_>>()
                    .join("/"),
            ),
            _ => (
                ProjectId::derive(&["root", &device_id.to_string(), &root_logical]),
                root_logical
                    .trim_end_matches('/')
                    .rsplit('/')
                    .next()
                    .filter(|s| !s.is_empty())
                    .unwrap_or("project")
                    .to_string(),
            ),
        };
        Self {
            project_id,
            root: root_logical,
            name,
            repo_remote: repo_remote.and_then(normalise_remote),
            branch: None,
            head: None,
        }
    }
}

/// Canonical `host/owner/repo` form of a Git remote URL, without scheme,
/// credentials, or `.git` suffix. Returns `None` for unparseable input.
pub fn normalise_remote(url: &str) -> Option<String> {
    let url = url.trim();
    if url.is_empty() {
        return None;
    }
    let without_scheme = if let Some(idx) = url.find("://") {
        &url[idx + 3..]
    } else if let Some((_, rest)) = url.split_once('@') {
        // scp-like: git@github.com:owner/repo.git
        rest
    } else {
        url
    };
    let without_user = without_scheme.rsplit('@').next().unwrap_or(without_scheme);
    let mut s = without_user.replacen(':', "/", 1);
    s = s.trim_end_matches('/').to_string();
    if let Some(stripped) = s.strip_suffix(".git") {
        s = stripped.to_string();
    }
    let s = s.trim_end_matches('/').to_ascii_lowercase();
    if s.matches('/').count() < 2 {
        return None;
    }
    Some(s)
}

/// Content-bearing fields, present only when the capture mode permits.
///
/// The physical placement of this data (inline, or in encrypted
/// content-addressed blobs) is a storage decision; the logical model only
/// promises that these fields never appear in `attrs`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct EventContent {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_input: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_output: Option<Value>,
    /// Anything else content-bearing the adapter chose to keep.
    #[serde(default, skip_serializing_if = "Map::is_empty", flatten)]
    pub extra: Map<String, Value>,
}

impl EventContent {
    pub fn is_empty(&self) -> bool {
        self.prompt.is_none()
            && self.command.is_none()
            && self.message.is_none()
            && self.error.is_none()
            && self.tool_input.is_none()
            && self.tool_output.is_none()
            && self.extra.is_empty()
    }
}

/// An immutable observed fact.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Event {
    // --- identity and ordering -------------------------------------------
    pub event_id: EventId,
    pub schema_version: u16,
    pub device_id: DeviceId,
    /// Device-local monotonically increasing sequence assigned by the single
    /// database writer at ingestion. `0` means "not yet ingested".
    #[serde(default)]
    pub source_seq: u64,
    /// Hybrid logical clock assigned at ingestion. `0` means "not yet
    /// ingested".
    #[serde(default)]
    pub hlc: Hlc,
    /// When the provider says the fact happened (falls back to
    /// `captured_at`).
    pub observed_at: Timestamp,
    /// When the hook process observed it.
    pub captured_at: Timestamp,
    /// When the database accepted it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ingested_at: Option<Timestamp>,

    // --- provenance ---------------------------------------------------------
    pub provider: Provider,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_version: Option<String>,
    pub adapter_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hook_version: Option<String>,
    pub capture_mode: CaptureMode,
    /// The provider's own event name (`PostToolUse`, `afterFileEdit`, ...).
    pub provider_event_name: String,

    // --- semantics ----------------------------------------------------------
    pub kind: EventKind,
    pub project: ProjectRef,
    pub session_id: SessionId,
    pub provider_session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_turn_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span_id: Option<SpanId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_span_id: Option<SpanId>,
    #[serde(default)]
    pub agent: AgentRef,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<ToolRef>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<PortablePath>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<Outcome>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// Allowlisted, content-free metadata.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub attrs: Map<String, Value>,
    /// Content-bearing fields; absent in `metadata_only` mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<EventContent>,
    /// Original provider payload, when retained.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw: Option<Value>,
    /// Fields from a newer schema this build does not understand.
    #[serde(default, flatten, skip_serializing_if = "Map::is_empty")]
    pub unknown: Map<String, Value>,
}

impl Event {
    /// Minimal constructor for adapters. Ordering fields are left unassigned.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        device_id: DeviceId,
        provider: Provider,
        provider_event_name: impl Into<String>,
        kind: EventKind,
        project: ProjectRef,
        provider_session_id: impl Into<String>,
        capture_mode: CaptureMode,
        adapter_version: impl Into<String>,
    ) -> Self {
        let now = Timestamp::now();
        let provider_session_id = provider_session_id.into();
        let session_id = SessionId::derive(&[provider.as_str(), &provider_session_id]);
        Self {
            event_id: EventId::new(),
            schema_version: CANONICAL_SCHEMA_VERSION,
            device_id,
            source_seq: 0,
            hlc: Hlc::default(),
            observed_at: now,
            captured_at: now,
            ingested_at: None,
            provider,
            provider_version: None,
            adapter_version: adapter_version.into(),
            hook_version: None,
            capture_mode,
            provider_event_name: provider_event_name.into(),
            kind,
            project,
            agent: AgentRef {
                agent_id: AgentId::derive(&["session", &session_id.to_string()]),
                ..Default::default()
            },
            session_id,
            provider_session_id,
            provider_turn_id: None,
            span_id: None,
            parent_span_id: None,
            tool: None,
            paths: Vec::new(),
            outcome: None,
            duration_ms: None,
            attrs: Map::new(),
            content: None,
            raw: None,
            unknown: Map::new(),
        }
    }

    /// True once the single writer has assigned ordering fields.
    pub fn is_ingested(&self) -> bool {
        self.source_seq != 0 && self.hlc.as_u64() != 0
    }

    /// Enforce the capture-mode invariant: strip content-bearing data when
    /// the mode forbids persisting it.
    pub fn apply_capture_mode(&mut self) {
        if !self.capture_mode.persists_content_locally() {
            self.content = None;
            self.raw = None;
        }
    }

    /// Enforce the `attrs` contract (RFC 0006 §4.3): drop unknown keys and
    /// content-shaped values, counting each drop in `attrs.redactions`.
    /// Returns the number dropped. The single writer calls this at
    /// ingestion, so it is the engine-level guarantee behind the adapters'
    /// allowlist.
    pub fn sanitise_attrs(&mut self) -> usize {
        crate::attrs::sanitise(&mut self.attrs)
    }

    /// Replace every secret span in content-bearing fields (`content`,
    /// `raw`) with `[REDACTED:<rule>]` (RFC 0006 §5). Metadata is untouched:
    /// `attrs` never holds secrets after ingestion. Returns the number of
    /// spans redacted.
    pub fn redact_secrets(&mut self) -> usize {
        let mut n = 0;
        if let Some(c) = &mut self.content {
            for s in [&mut c.prompt, &mut c.command, &mut c.message, &mut c.error]
                .into_iter()
                .flatten()
            {
                let (r, k) = crate::secrets::redact(s);
                if k > 0 {
                    *s = r;
                    n += k;
                }
            }
            for v in [&mut c.tool_input, &mut c.tool_output]
                .into_iter()
                .flatten()
            {
                n += crate::secrets::redact_value(v);
            }
            for v in c.extra.values_mut() {
                n += crate::secrets::redact_value(v);
            }
        }
        if let Some(raw) = &mut self.raw {
            n += crate::secrets::redact_value(raw);
        }
        n
    }

    pub fn attr_str(&self, key: &str) -> Option<&str> {
        self.attrs.get(key).and_then(Value::as_str)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalises_remotes() {
        assert_eq!(
            normalise_remote("git@github.com:Streamize/AttemptDB.git").as_deref(),
            Some("github.com/streamize/attemptdb")
        );
        assert_eq!(
            normalise_remote("https://user:tok@github.com/streamize/attemptdb.git/").as_deref(),
            Some("github.com/streamize/attemptdb")
        );
        assert_eq!(
            normalise_remote("ssh://git@gitlab.com:2222/group/sub/repo.git").as_deref(),
            Some("gitlab.com/2222/group/sub/repo")
        );
        assert_eq!(normalise_remote("nonsense"), None);
    }

    #[test]
    fn project_identity_prefers_remote() {
        let dev = DeviceId::new();
        let a = ProjectRef::derive(
            "/Users/a/attemptdb",
            Some("git@github.com:s/attemptdb.git"),
            &dev,
        );
        let b = ProjectRef::derive(
            "C:\\code\\attemptdb",
            Some("https://github.com/s/attemptdb"),
            &DeviceId::new(),
        );
        assert_eq!(a.project_id, b.project_id);
        assert_eq!(a.name, "s/attemptdb");
        let c = ProjectRef::derive("/Users/a/attemptdb", None, &dev);
        assert_ne!(c.project_id, a.project_id);
        assert_eq!(c.name, "attemptdb");
    }

    #[test]
    fn unknown_fields_roundtrip() {
        let dev = DeviceId::new();
        let ev = Event::new(
            dev,
            Provider::ClaudeCode,
            "PostToolUse",
            EventKind::ToolCallFinished,
            ProjectRef::derive("/p", None, &dev),
            "sess-1",
            CaptureMode::LocalSemantic,
            "test",
        );
        let mut v = serde_json::to_value(&ev).unwrap();
        v["future_field"] = Value::String("kept".into());
        let back: Event = serde_json::from_value(v).unwrap();
        assert_eq!(
            back.unknown.get("future_field").and_then(Value::as_str),
            Some("kept")
        );
        let again = serde_json::to_value(&back).unwrap();
        assert_eq!(again["future_field"], "kept");
        assert_eq!(back.session_id, ev.session_id);
    }

    #[test]
    fn metadata_only_strips_content() {
        let dev = DeviceId::new();
        let mut ev = Event::new(
            dev,
            Provider::Codex,
            "UserPromptSubmit",
            EventKind::PromptSubmitted,
            ProjectRef::derive("/p", None, &dev),
            "s",
            CaptureMode::MetadataOnly,
            "test",
        );
        ev.content = Some(EventContent {
            prompt: Some("secret".into()),
            ..Default::default()
        });
        ev.raw = Some(serde_json::json!({"prompt": "secret"}));
        ev.apply_capture_mode();
        assert!(ev.content.is_none());
        assert!(ev.raw.is_none());
        let s = serde_json::to_string(&ev).unwrap();
        assert!(!s.contains("secret"));
    }
}
