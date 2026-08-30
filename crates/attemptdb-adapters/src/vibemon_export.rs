//! Legacy input, batch form: one row of VibeMon's `hook_events` table →
//! canonical [`Event`], for `attempt import vibemon-export`.
//!
//! # What the table holds
//!
//! The legacy Edge Function (`vibemon-app/supabase/functions/hook`) did not
//! store the envelope v2 it received; it wrote one row per event and kept
//! only some of the envelope. The column set, from the migrations
//! (`20260129083723_monster_system.sql`, `20260320000000_hook_events_unified.sql`,
//! `20260422000000_signal_pipeline_and_narrative.sql`,
//! `20260510130000_rename_drops_to_xp.sql`) and the function's inserts:
//!
//! | column | type | written for | envelope v2 origin |
//! |---|---|---|---|
//! | `id` | uuid | every row | none — primary key, the only identity a row has |
//! | `user_id` | uuid | every row | none — the account the API key belonged to |
//! | `created_at` | timestamptz | every row | none — server receipt time (`default now()`); the client's second-precision `timestamp` was **not** stored |
//! | `event_type` | text | every row | `event`, except `activity` was stored as `tool_use` |
//! | `agent` | text | every row | `agent` (server default `claude_code`) |
//! | `session_id` | text, nullable | every row | `session_id` (top level, or `payload.session_id` for v1 clients) |
//! | `signals` | jsonb | every row | `signals`, merged over server-derived fallbacks |
//! | `local_hour`, `local_dow` | smallint | every row | `local_hour`, `local_dow` |
//! | `envelope_version` | smallint | every row | `v` (1 for pre-signal clients) |
//! | `payload` | jsonb | `prompt stop permission tool_failure bash` | `payload` with body keys stripped (`stripBodies`) |
//! | `payload` | jsonb | `session_start session_end` | `{cwd, timestamp, client_version}` — the only rows that kept `cwd` |
//! | `project_id` | uuid | `tool_use` | none — the server's `projects` row (its identifier is not in this table) |
//! | `tool` | text | `tool_use` | `payload.tool` / `payload.tool_name` |
//! | `file_path` | text | `tool_use` | `payload.tool_input.file_path` |
//! | `lines_added`, `lines_removed` | int | `tool_use` | `signals["lines.added"]` / `["lines.removed"]` (or server-computed) |
//! | `xp` | int | `tool_use` | none — gamification credit, ignored here |
//!
//! There is no `cwd` column and no device or machine column. Everything a
//! row lost is either recovered from sibling rows by the importer (the
//! session's `session_start` row carries the working directory; see
//! [`ProjectHint`]) or left unknown — never invented.
//!
//! # Mapping back
//!
//! [`parse_row`] reads a row (unknown columns ignored) and [`normalise_row`]
//! rebuilds the envelope the client would have sent —
//! `{v: 2, event, agent, session_id, cwd, project_root, timestamp, local_hour,
//! local_dow, payload, signals}` — and hands it to
//! [`crate::vibemon::normalise_envelope`]. For `tool_use` rows the payload
//! is rebuilt from the columns (`tool_name`, `tool_input.file_path`) and the
//! line counts are put back under `signals` when the client did not send
//! them. `v` is always 2 because the *table* shape is the v2-era shape;
//! the client's own version survives as `hook_version`
//! (`vibemon-envelope-v1` / `-v2`).
//!
//! After normalisation the importer-specific facts are set:
//!
//! - `event_id = EventId::derive(["vibemon-export", id])` — the row's primary
//!   key, so re-importing the same export (or an overlapping later one)
//!   stores nothing twice. Two rows collide only when they are the same row.
//! - `observed_at = captured_at = created_at` — the closest fact to when the
//!   hook fired; the legacy client captured live, so the events are not
//!   marked `reconstructed`.
//! - `capture_mode = metadata_only` — the legacy client never captured
//!   content; `commit.message` (a content signal) goes to `content` and is
//!   stripped, as the envelope adapter documents.
//! - `attrs.x_vibemon_import = "hook_events"`, `attrs.x_vibemon_row_id`,
//!   `attrs.x_vibemon_project_id`, `attrs.x_vibemon_envelope_version`,
//!   `attrs.x_vibemon_client_version` — provenance in the provider
//!   extension namespace (RFC 0006 §4.1); identifiers are metadata.

use crate::vibemon::normalise_envelope;
use crate::{ADAPTER_VERSION, AdapterError};
use attemptdb_core::{CaptureMode, DeviceId, Event, EventId, Timestamp};
use serde::Serialize;
use serde_json::{Map, Value, json};

/// Namespace for every id derived from an exported row.
pub const ID_NAMESPACE: &str = "vibemon-export";

/// Value of `attrs.x_vibemon_import` on every event this module produces.
pub const IMPORT_MARKER: &str = "hook_events";

/// Columns that may carry a device or machine identifier, in the order
/// they are tried. The real `hook_events` table has none of them; an export
/// joined with an installs table might.
pub const DEVICE_COLUMNS: &[&str] = &["device_id", "machine_id", "install_id"];

/// Stored `event_type` values and the envelope event each maps to.
pub const EVENT_TYPES: &[(&str, &str)] = &[
    ("tool_use", "activity"),
    ("activity", "activity"),
    ("bash", "bash"),
    ("prompt", "prompt"),
    ("stop", "stop"),
    ("permission", "permission"),
    ("tool_failure", "tool_failure"),
    ("session_start", "session_start"),
    ("session_end", "session_end"),
];

/// Why a row was not imported. Never fatal: the importer counts these.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RejectReason {
    /// The line (or array element) is not valid JSON.
    InvalidJson,
    /// Valid JSON, but not an object.
    NotAnObject,
    /// No `id` column: without the primary key the event id cannot be
    /// made deterministic, so the row cannot be imported idempotently.
    MissingId,
    /// No `created_at` (or `timestamp`) column.
    MissingCreatedAt,
    /// `created_at` is not a timestamp this importer can read.
    InvalidCreatedAt,
    /// No `event_type` (or `event`) column.
    MissingEventType,
    /// An `event_type` the legacy server never stored.
    UnknownEventType,
    /// Neither a device column, a `user_id`, nor `--device`: no device to
    /// attribute the event to.
    NoDeviceIdentity,
    /// The envelope adapter refused the rebuilt envelope.
    Adapter,
}

impl RejectReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InvalidJson => "invalid_json",
            Self::NotAnObject => "not_an_object",
            Self::MissingId => "missing_id",
            Self::MissingCreatedAt => "missing_created_at",
            Self::InvalidCreatedAt => "invalid_created_at",
            Self::MissingEventType => "missing_event_type",
            Self::UnknownEventType => "unknown_event_type",
            Self::NoDeviceIdentity => "no_device_identity",
            Self::Adapter => "adapter",
        }
    }
}

impl std::fmt::Display for RejectReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One `hook_events` row, read and validated but not yet normalised.
#[derive(Clone, Debug, PartialEq)]
pub struct ExportRow {
    /// Primary key, as text.
    pub id: String,
    pub user_id: Option<String>,
    /// Value of the first [`DEVICE_COLUMNS`] entry present.
    pub device_hint: Option<String>,
    /// Server receipt time.
    pub created_at: Timestamp,
    /// The stored `event_type`.
    pub event_type: String,
    /// The envelope event name `event_type` maps to.
    pub event: &'static str,
    pub agent: String,
    pub session_id: Option<String>,
    pub project_id: Option<String>,
    pub tool: Option<String>,
    pub file_path: Option<String>,
    pub lines_added: Option<u64>,
    pub lines_removed: Option<u64>,
    pub local_hour: Option<u64>,
    pub local_dow: Option<u64>,
    pub envelope_version: u64,
    /// Stored payload (empty for `tool_use` rows).
    pub payload: Map<String, Value>,
    pub signals: Map<String, Value>,
    /// A `cwd` column, when the export carried one (the table does not).
    pub cwd_column: Option<String>,
}

impl ExportRow {
    /// Working directory the row itself knows: a `cwd` column, or
    /// `payload.cwd` (session rows).
    pub fn cwd(&self) -> Option<&str> {
        self.cwd_column
            .as_deref()
            .or_else(|| string_of(self.payload.get("cwd")))
            .filter(|s| is_absolute(s))
    }

    /// Repository identifier the row itself knows (`payload.project_root`
    /// or `payload.repo_identifier`, `owner/repo` or an absolute path).
    pub fn identifier(&self) -> Option<&str> {
        string_of(self.payload.get("repo_identifier"))
            .or_else(|| string_of(self.payload.get("project_root")))
    }

    /// `payload.client_version` of session rows.
    pub fn client_version(&self) -> Option<&str> {
        string_of(self.payload.get("client_version")).filter(|s| s.len() <= 32)
    }

    /// The device the row is attributed to when no override is given: a
    /// device column when present, else the account (`user_id`), each
    /// hashed under [`ID_NAMESPACE`].
    pub fn derived_device(&self) -> Option<DeviceId> {
        self.device_hint
            .as_deref()
            .or(self.user_id.as_deref())
            .map(|v| DeviceId::derive(&[ID_NAMESPACE, v]))
    }

    /// The deterministic event id of this row.
    pub fn event_id(&self) -> EventId {
        EventId::derive(&[ID_NAMESPACE, &self.id])
    }
}

/// What the importer recovered about a row's project from sibling rows.
/// Either may be absent; [`normalise_row`] then falls back in this order:
/// `cwd` → `identifier` (as the root when it is a path, as the display
/// name otherwise) → `vibemon-project/<project_id>` → `/`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProjectHint {
    pub cwd: Option<String>,
    pub identifier: Option<String>,
}

/// Read one exported row. Unknown columns are ignored; a `null` column is
/// the same as an absent one.
pub fn parse_row(value: &Value) -> Result<ExportRow, RejectReason> {
    let Some(obj) = value.as_object() else {
        return Err(RejectReason::NotAnObject);
    };
    let id = id_text(obj.get("id")).ok_or(RejectReason::MissingId)?;
    let created_raw = obj
        .get("created_at")
        .filter(|v| !v.is_null())
        .or_else(|| obj.get("timestamp").filter(|v| !v.is_null()))
        .ok_or(RejectReason::MissingCreatedAt)?;
    let created_at = parse_timestamp(created_raw).ok_or(RejectReason::InvalidCreatedAt)?;
    let event_type = string_of(obj.get("event_type"))
        .or_else(|| string_of(obj.get("event")))
        .ok_or(RejectReason::MissingEventType)?;
    let event = EVENT_TYPES
        .iter()
        .find(|(stored, _)| *stored == event_type)
        .map(|(_, envelope)| *envelope)
        .ok_or(RejectReason::UnknownEventType)?;
    let payload = obj
        .get("payload")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let signals = obj
        .get("signals")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    Ok(ExportRow {
        id,
        user_id: id_text(obj.get("user_id")),
        device_hint: DEVICE_COLUMNS.iter().find_map(|c| id_text(obj.get(*c))),
        created_at,
        event_type: event_type.to_string(),
        event,
        agent: string_of(obj.get("agent"))
            .unwrap_or("claude_code")
            .to_string(),
        session_id: string_of(obj.get("session_id"))
            .or_else(|| string_of(payload.get("session_id")))
            .map(str::to_string),
        project_id: id_text(obj.get("project_id")),
        tool: string_of(obj.get("tool")).map(str::to_string),
        file_path: string_of(obj.get("file_path")).map(str::to_string),
        lines_added: obj.get("lines_added").and_then(Value::as_u64),
        lines_removed: obj.get("lines_removed").and_then(Value::as_u64),
        local_hour: obj.get("local_hour").and_then(Value::as_u64),
        local_dow: obj.get("local_dow").and_then(Value::as_u64),
        envelope_version: obj
            .get("envelope_version")
            .and_then(Value::as_u64)
            .unwrap_or(1),
        payload,
        signals,
        cwd_column: string_of(obj.get("cwd")).map(str::to_string),
    })
}

/// Normalise one row for `device` (see [`ExportRow::derived_device`] for
/// the default choice). `hint` supplies what the row itself lacks.
pub fn normalise_row(
    device: DeviceId,
    row: &ExportRow,
    hint: &ProjectHint,
) -> Result<Event, AdapterError> {
    let envelope = rebuild_envelope(row, hint);
    let mut ev = normalise_envelope(device, CaptureMode::MetadataOnly, &envelope)?;
    ev.event_id = row.event_id();
    ev.observed_at = row.created_at;
    ev.captured_at = row.created_at;
    ev.adapter_version = format!("vibemon-export/{ADAPTER_VERSION}");
    ev.hook_version = Some(format!("vibemon-envelope-v{}", row.envelope_version));
    ev.attrs
        .insert("x_vibemon_import".into(), json!(IMPORT_MARKER));
    ev.attrs.insert("x_vibemon_row_id".into(), json!(row.id));
    ev.attrs.insert(
        "x_vibemon_envelope_version".into(),
        json!(row.envelope_version),
    );
    if let Some(p) = &row.project_id {
        ev.attrs.insert("x_vibemon_project_id".into(), json!(p));
    }
    if let Some(v) = row.client_version() {
        ev.attrs.insert("x_vibemon_client_version".into(), json!(v));
    }
    ev.apply_capture_mode();
    Ok(ev)
}

/// The envelope v2 object the client would have sent for this row.
pub fn rebuild_envelope(row: &ExportRow, hint: &ProjectHint) -> Value {
    let identifier = row
        .identifier()
        .map(str::to_string)
        .or_else(|| hint.identifier.clone());
    let cwd = row
        .cwd()
        .map(str::to_string)
        .or_else(|| hint.cwd.clone())
        .or_else(|| identifier.clone())
        .or_else(|| {
            row.project_id
                .as_ref()
                .map(|p| format!("vibemon-project/{p}"))
        });

    let mut payload = row.payload.clone();
    let mut signals = row.signals.clone();
    if row.event_type == "tool_use" {
        if let Some(tool) = &row.tool
            && !payload.contains_key("tool_name")
        {
            payload.insert("tool_name".into(), json!(tool));
        }
        if let Some(path) = &row.file_path {
            let input = payload
                .entry("tool_input")
                .or_insert_with(|| Value::Object(Map::new()));
            if let Some(input) = input.as_object_mut()
                && !input.contains_key("file_path")
            {
                input.insert("file_path".into(), json!(path));
            }
        }
        for (column, key) in [
            (row.lines_added, "lines.added"),
            (row.lines_removed, "lines.removed"),
        ] {
            if let Some(n) = column
                && !signals.contains_key(key)
            {
                signals.insert(key.into(), json!(n));
            }
        }
    }
    if let Some(s) = &row.session_id
        && !payload.contains_key("session_id")
    {
        payload.insert("session_id".into(), json!(s));
    }

    let mut envelope = Map::new();
    envelope.insert("v".into(), json!(crate::vibemon::ENVELOPE_VERSION));
    envelope.insert("event".into(), json!(row.event));
    envelope.insert("agent".into(), json!(row.agent));
    if let Some(s) = &row.session_id {
        envelope.insert("session_id".into(), json!(s));
    }
    if let Some(c) = cwd {
        envelope.insert("cwd".into(), json!(c));
    }
    if let Some(i) = identifier {
        envelope.insert("project_root".into(), json!(i));
    }
    envelope.insert("timestamp".into(), json!(row.created_at.to_rfc3339()));
    if let Some(h) = row.local_hour {
        envelope.insert("local_hour".into(), json!(h));
    }
    if let Some(d) = row.local_dow {
        envelope.insert("local_dow".into(), json!(d));
    }
    envelope.insert("payload".into(), Value::Object(payload));
    envelope.insert("signals".into(), Value::Object(signals));
    Value::Object(envelope)
}

/// `created_at` as PostgreSQL and Supabase write it: RFC 3339, or the
/// `YYYY-MM-DD HH:MM:SS[.ffffff]+HH` text form, or an epoch number.
pub fn parse_timestamp(v: &Value) -> Option<Timestamp> {
    match v {
        Value::String(s) => parse_timestamp_text(s),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Timestamp::parse(&i.to_string())
            } else {
                n.as_f64()
                    .filter(|f| f.is_finite() && *f > 0.0)
                    .map(|f| Timestamp::from_micros((f * 1_000_000.0) as i64))
            }
        }
        _ => None,
    }
}

fn parse_timestamp_text(s: &str) -> Option<Timestamp> {
    let s = s.trim();
    if let Some(t) = Timestamp::parse(s) {
        return Some(t);
    }
    // `2026-08-21 10:00:30.5+00` / `...-05` (offset without minutes) and
    // the space separator: bring them to RFC 3339 and try again.
    let mut fixed = s.replacen(' ', "T", 1);
    let offset_without_minutes = fixed
        .get(fixed.len().saturating_sub(3)..)
        .is_some_and(|tail| {
            (tail.starts_with('+') || tail.starts_with('-'))
                && tail.len() == 3
                && tail[1..].bytes().all(|b| b.is_ascii_digit())
        });
    if offset_without_minutes {
        fixed.push_str(":00");
    }
    Timestamp::parse(&fixed)
}

fn string_of(v: Option<&Value>) -> Option<&str> {
    v.and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// Identifier columns as text: a string, or a JSON number (bigint keys).
fn id_text(v: Option<&Value>) -> Option<String> {
    match v? {
        Value::String(s) => {
            let s = s.trim();
            (!s.is_empty()).then(|| s.to_string())
        }
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn is_absolute(p: &str) -> bool {
    p.starts_with('/') || p.get(1..3) == Some(":/") || p.get(1..3) == Some(":\\")
}

#[cfg(test)]
mod tests {
    use super::*;
    use attemptdb_core::EventKind;

    fn device() -> DeviceId {
        DeviceId::derive(&["vibemon-export-tests", "d"])
    }

    fn tool_use_row() -> Value {
        json!({
            "id": "11111111-1111-4111-8111-000000000003",
            "user_id": "00000000-0000-4000-8000-00000000000a",
            "project_id": "7c9e6679-7425-40de-944b-e07fc1f90ae7",
            "tool": "Edit",
            "file_path": "/home/dev/example/project/src/lib.rs",
            "xp": 1,
            "lines_added": 4,
            "lines_removed": 1,
            "created_at": "2026-08-20T09:00:20.250000+00:00",
            "event_type": "tool_use",
            "agent": "claude_code",
            "payload": null,
            "session_id": "sess-1",
            "signals": {"tool.name": "edit", "file.ext": "rs", "tool.duration_ms": 1250},
            "local_hour": 18,
            "local_dow": 3,
            "envelope_version": 2,
            "_unknown_column": "ignored"
        })
    }

    #[test]
    fn tool_use_rows_rebuild_the_activity_payload() {
        let row = parse_row(&tool_use_row()).unwrap();
        assert_eq!(row.event, "activity");
        assert_eq!(row.tool.as_deref(), Some("Edit"));
        assert!(row.cwd().is_none(), "tool_use rows carry no cwd");
        let hint = ProjectHint {
            cwd: Some("/home/dev/example/project".into()),
            identifier: Some("example/project".into()),
        };
        let envelope = rebuild_envelope(&row, &hint);
        assert_eq!(envelope["v"], 2);
        assert_eq!(envelope["event"], "activity");
        assert_eq!(envelope["cwd"], "/home/dev/example/project");
        assert_eq!(envelope["project_root"], "example/project");
        assert_eq!(envelope["payload"]["tool_name"], "Edit");
        assert_eq!(
            envelope["payload"]["tool_input"]["file_path"],
            "/home/dev/example/project/src/lib.rs"
        );
        assert_eq!(envelope["signals"]["lines.added"], 4);
        assert_eq!(envelope["signals"]["lines.removed"], 1);
        assert_eq!(envelope["signals"]["tool.name"], "edit");

        let ev = normalise_row(device(), &row, &hint).unwrap();
        assert_eq!(ev.kind, EventKind::ToolCallFinished);
        assert_eq!(ev.tool.as_ref().unwrap().name, "Edit");
        assert_eq!(ev.attrs["file_ext"], "rs");
        assert_eq!(ev.attrs["lines_added"], 4);
        assert_eq!(ev.attrs["cwd"], "~/example/project");
        assert_eq!(ev.project.name, "example/project");
        assert_eq!(ev.duration_ms, Some(1250));
        assert_eq!(ev.event_id, row.event_id());
        assert_eq!(
            ev.event_id,
            EventId::derive(&["vibemon-export", "11111111-1111-4111-8111-000000000003"])
        );
        assert_eq!(ev.observed_at, row.created_at);
        assert_eq!(ev.captured_at, row.created_at);
        assert_eq!(ev.capture_mode, CaptureMode::MetadataOnly);
        assert_eq!(ev.hook_version.as_deref(), Some("vibemon-envelope-v2"));
        assert!(ev.adapter_version.starts_with("vibemon-export/"));
        assert_eq!(ev.attrs["x_vibemon_import"], IMPORT_MARKER);
        assert_eq!(
            ev.attrs["x_vibemon_row_id"],
            "11111111-1111-4111-8111-000000000003"
        );
        assert_eq!(
            ev.attrs["x_vibemon_project_id"],
            "7c9e6679-7425-40de-944b-e07fc1f90ae7"
        );
        assert_eq!(ev.attrs["x_vibemon_envelope_version"], 2);
        assert!(!ev.attrs.contains_key("reconstructed"));
        assert!(ev.raw.is_none() && ev.content.is_none());
        assert_eq!(
            row.derived_device(),
            Some(DeviceId::derive(&[
                "vibemon-export",
                "00000000-0000-4000-8000-00000000000a"
            ]))
        );
    }

    #[test]
    fn the_same_row_always_gets_the_same_id_and_a_different_row_does_not() {
        let a = parse_row(&tool_use_row()).unwrap();
        let mut other = tool_use_row();
        other["id"] = json!("11111111-1111-4111-8111-000000000004");
        let b = parse_row(&other).unwrap();
        assert_eq!(a.event_id(), parse_row(&tool_use_row()).unwrap().event_id());
        assert_ne!(a.event_id(), b.event_id());
        // A numeric key is fine too.
        let mut numeric = tool_use_row();
        numeric["id"] = json!(42);
        assert_eq!(parse_row(&numeric).unwrap().id, "42");
    }

    #[test]
    fn session_rows_keep_their_cwd_and_client_version() {
        let row = parse_row(&json!({
            "id": "11111111-1111-4111-8111-000000000001",
            "user_id": "00000000-0000-4000-8000-00000000000a",
            "created_at": "2026-08-20 09:00:00.1+00",
            "event_type": "session_start",
            "agent": "claude_code",
            "session_id": "sess-1",
            "payload": {"cwd": "/home/dev/example/project", "timestamp": "2026-08-20T08:59:59Z", "client_version": "29"},
            "signals": {},
            "envelope_version": 2
        }))
        .unwrap();
        assert_eq!(row.cwd(), Some("/home/dev/example/project"));
        assert_eq!(row.client_version(), Some("29"));
        assert_eq!(
            row.created_at,
            Timestamp::parse("2026-08-20T09:00:00.1Z").unwrap(),
            "postgres text timestamps parse"
        );
        let ev = normalise_row(device(), &row, &ProjectHint::default()).unwrap();
        assert_eq!(ev.kind, EventKind::SessionStarted);
        assert_eq!(ev.project.root, "/home/dev/example/project");
        assert_eq!(ev.project.name, "project");
        assert_eq!(ev.attrs["x_vibemon_client_version"], "29");
        assert!(ev.outcome.is_none());
    }

    #[test]
    fn fallbacks_when_nothing_knows_the_directory() {
        let base = json!({
            "id": "11111111-1111-4111-8111-000000000009",
            "user_id": "00000000-0000-4000-8000-00000000000a",
            "created_at": 1_766_224_800,
            "event_type": "prompt",
            "session_id": "sess-9",
            "payload": {"project_root": "example/project"},
            "signals": {"prompt.chars": 12}
        });
        let row = parse_row(&base).unwrap();
        assert_eq!(row.agent, "claude_code", "server default");
        assert_eq!(row.envelope_version, 1, "column default");
        // Identifier only: it becomes both root and name.
        let ev = normalise_row(device(), &row, &ProjectHint::default()).unwrap();
        assert_eq!(ev.project.root, "example/project");
        assert_eq!(ev.project.name, "example/project");
        assert_eq!(ev.hook_version.as_deref(), Some("vibemon-envelope-v1"));
        // Hint wins for the root; the row's identifier is still the name.
        let ev = normalise_row(
            device(),
            &row,
            &ProjectHint {
                cwd: Some("/home/dev/example/project".into()),
                identifier: None,
            },
        )
        .unwrap();
        assert_eq!(ev.project.root, "/home/dev/example/project");
        assert_eq!(ev.project.name, "example/project");
        // Nothing at all but a project uuid.
        let mut bare = base.clone();
        bare["payload"] = json!({});
        bare["event_type"] = json!("tool_use");
        bare["tool"] = json!("Write");
        bare["project_id"] = json!("7c9e6679-7425-40de-944b-e07fc1f90ae7");
        let ev = normalise_row(
            device(),
            &parse_row(&bare).unwrap(),
            &ProjectHint::default(),
        )
        .unwrap();
        assert_eq!(
            ev.project.root,
            "vibemon-project/7c9e6679-7425-40de-944b-e07fc1f90ae7"
        );
        // Not even that.
        bare["project_id"] = Value::Null;
        let ev = normalise_row(
            device(),
            &parse_row(&bare).unwrap(),
            &ProjectHint::default(),
        )
        .unwrap();
        assert_eq!(ev.project.root, "/");
        assert!(!ev.project.name.is_empty());
    }

    #[test]
    fn rejections_name_the_missing_piece() {
        let ok = tool_use_row();
        let without = |key: &str| {
            let mut v = ok.clone();
            v.as_object_mut().unwrap().remove(key);
            v
        };
        assert_eq!(parse_row(&json!([1])), Err(RejectReason::NotAnObject));
        assert_eq!(parse_row(&without("id")), Err(RejectReason::MissingId));
        assert_eq!(
            parse_row(&without("created_at")),
            Err(RejectReason::MissingCreatedAt)
        );
        let mut bad_ts = ok.clone();
        bad_ts["created_at"] = json!("yesterday-ish");
        assert_eq!(parse_row(&bad_ts), Err(RejectReason::InvalidCreatedAt));
        assert_eq!(
            parse_row(&without("event_type")),
            Err(RejectReason::MissingEventType)
        );
        let mut unknown = ok.clone();
        unknown["event_type"] = json!("coffee_break");
        assert_eq!(parse_row(&unknown), Err(RejectReason::UnknownEventType));
        // A `null` id is a missing id.
        let mut null_id = ok.clone();
        null_id["id"] = Value::Null;
        assert_eq!(parse_row(&null_id), Err(RejectReason::MissingId));
        // Aliases: `event` and `timestamp` columns are accepted.
        let mut aliased = without("event_type");
        aliased["event"] = json!("bash");
        aliased.as_object_mut().unwrap().remove("created_at");
        aliased["timestamp"] = json!("2026-08-20T09:00:20Z");
        let row = parse_row(&aliased).unwrap();
        assert_eq!(row.event, "bash");
        assert_eq!(
            row.user_id.as_deref(),
            Some("00000000-0000-4000-8000-00000000000a")
        );
        // Device columns take precedence over the account for identity.
        let mut with_device = ok.clone();
        with_device["machine_id"] = json!("mac-7");
        assert_eq!(
            parse_row(&with_device).unwrap().derived_device(),
            Some(DeviceId::derive(&["vibemon-export", "mac-7"]))
        );
        let mut nobody = without("user_id");
        nobody.as_object_mut().unwrap().remove("machine_id");
        assert_eq!(parse_row(&nobody).unwrap().derived_device(), None);
    }

    #[test]
    fn timestamps_in_every_shape_postgres_writes() {
        let t = |s: &str| parse_timestamp(&json!(s)).map(|t| t.to_rfc3339());
        assert_eq!(
            t("2026-08-20T09:00:00.123456+00:00"),
            Some("2026-08-20T09:00:00.123456Z".into())
        );
        assert_eq!(
            t("2026-08-20 09:00:00.5+00"),
            Some("2026-08-20T09:00:00.500000Z".into())
        );
        assert_eq!(
            t("2026-08-20 18:00:00+09"),
            Some("2026-08-20T09:00:00.000000Z".into())
        );
        assert_eq!(
            t("2026-08-20T09:00:00Z"),
            Some("2026-08-20T09:00:00.000000Z".into())
        );
        assert_eq!(t("nope"), None);
        assert_eq!(
            parse_timestamp(&json!(1_766_224_800)).map(|t| t.to_rfc3339()),
            Some("2025-12-20T10:00:00.000000Z".into())
        );
        assert_eq!(parse_timestamp(&json!(true)), None);
    }
}
