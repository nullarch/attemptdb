//! Immutable columnar segments: Arrow IPC files with AttemptDB metadata.
//!
//! One segment is written per memtable flush and never modified. The Arrow
//! schema is the public columnar contract (`docs/storage-format.md`);
//! every field carries `attemptdb.field_id` metadata so columns can be
//! renamed without breaking readers.
//!
//! Two on-disk layouts exist:
//!
//! - **version 1** (no encryption key): `content` and `raw` are inline JSON
//!   in `content_json` / `raw_json`;
//! - **version 2** (written with a key): those two columns are always null
//!   and `content_ref` / `raw_ref` hold the ids of encrypted blobs
//!   (`crate::blobs`).
//!
//! Readers accept both and normalise every batch to the canonical
//! (version 2) schema, so the query layer sees one column set. Refs are
//! resolved through a [`BlobReader`] when a key is available; without one,
//! `content`/`raw` decode as `None`.

use crate::blobs::{BlobId, BlobReader, BlobSink};
use crate::failpoint;
use crate::format::{
    MIN_SEGMENT_FORMAT_VERSION, SEGMENT_FORMAT_VERSION, SEGMENT_FORMAT_VERSION_INLINE, SEGMENTS_DIR,
};
use crate::manifest::SegmentMeta;
use crate::{IoAt, Result, StorageError};
use arrow::array::{
    Array, ArrayRef, AsArray, FixedSizeBinaryBuilder, Int32Builder, RecordBatch, StringBuilder,
    StringDictionaryBuilder, TimestampMicrosecondArray, UInt16Builder, UInt64Builder,
    new_null_array,
};
use arrow::datatypes::{
    DataType, Field, Int32Type, Schema, SchemaRef, TimeUnit, TimestampMicrosecondType, UInt16Type,
    UInt64Type,
};
use arrow::ipc::CompressionType;
use arrow::ipc::reader::FileReader;
use arrow::ipc::writer::{FileWriter, IpcWriteOptions};
use attemptdb_core::event::{
    AgentRef, EventContent, Outcome, OutcomeStatus, ProjectRef, Provider, ToolCategory, ToolRef,
};
use attemptdb_core::schema::{CANONICAL_SCHEMA_VERSION, field_id};
use attemptdb_core::{
    AgentId, CaptureMode, DeviceId, Event, EventId, EventKind, Hlc, PortablePath, ProjectId,
    SessionId, SpanId, Timestamp,
};
use std::collections::{BTreeSet, HashMap};
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use uuid::Uuid;

fn dict() -> DataType {
    DataType::Dictionary(Box::new(DataType::Int32), Box::new(DataType::Utf8))
}

fn ts() -> DataType {
    DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into()))
}

fn field(name: &str, dt: DataType, nullable: bool, id: u16) -> Field {
    let mut md = HashMap::new();
    md.insert("attemptdb.field_id".to_string(), id.to_string());
    Field::new(name, dt, nullable).with_metadata(md)
}

/// A column that is one projection of a structured field (`derivation`
/// distinguishes columns sharing a field id).
fn field_derived(name: &str, dt: DataType, nullable: bool, id: u16, derivation: &str) -> Field {
    let mut md = HashMap::new();
    md.insert("attemptdb.field_id".to_string(), id.to_string());
    md.insert("attemptdb.derivation".to_string(), derivation.to_string());
    Field::new(name, dt, nullable).with_metadata(md)
}

/// Column names in order. Kept as constants so the query layer can refer to
/// them without string literals scattered around.
pub mod col {
    pub const EVENT_ID: &str = "event_id";
    pub const SCHEMA_VERSION: &str = "schema_version";
    pub const DEVICE_ID: &str = "device_id";
    pub const SOURCE_SEQ: &str = "source_seq";
    pub const HLC: &str = "hlc";
    pub const OBSERVED_AT: &str = "observed_at";
    pub const CAPTURED_AT: &str = "captured_at";
    pub const INGESTED_AT: &str = "ingested_at";
    pub const PROVIDER: &str = "provider";
    pub const PROVIDER_VERSION: &str = "provider_version";
    pub const ADAPTER_VERSION: &str = "adapter_version";
    pub const HOOK_VERSION: &str = "hook_version";
    pub const CAPTURE_MODE: &str = "capture_mode";
    pub const PROVIDER_EVENT_NAME: &str = "provider_event_name";
    pub const KIND: &str = "kind";
    pub const PROJECT_ID: &str = "project_id";
    pub const PROJECT_ROOT: &str = "project_root";
    pub const PROJECT_NAME: &str = "project_name";
    pub const REPO_REMOTE: &str = "repo_remote";
    pub const BRANCH: &str = "branch";
    pub const HEAD: &str = "head";
    pub const SESSION_ID: &str = "session_id";
    pub const PROVIDER_SESSION_ID: &str = "provider_session_id";
    pub const PROVIDER_TURN_ID: &str = "provider_turn_id";
    pub const SPAN_ID: &str = "span_id";
    pub const PARENT_SPAN_ID: &str = "parent_span_id";
    pub const AGENT_ID: &str = "agent_id";
    pub const AGENT_TYPE: &str = "agent_type";
    pub const PARENT_AGENT_ID: &str = "parent_agent_id";
    pub const MODEL: &str = "model";
    pub const PROVIDER_AGENT_ID: &str = "provider_agent_id";
    pub const TOOL_NAME: &str = "tool_name";
    pub const TOOL_CATEGORY: &str = "tool_category";
    pub const TOOL_CALL_ID: &str = "tool_call_id";
    pub const PATH_LOGICAL: &str = "path_logical";
    pub const PATH_RELATIVE: &str = "path_relative";
    pub const PATHS_JSON: &str = "paths_json";
    pub const OUTCOME_STATUS: &str = "outcome_status";
    pub const OUTCOME_CLASS: &str = "outcome_class";
    pub const EXIT_CODE: &str = "exit_code";
    pub const DURATION_MS: &str = "duration_ms";
    pub const ATTRS_JSON: &str = "attrs_json";
    pub const CONTENT_JSON: &str = "content_json";
    pub const RAW_JSON: &str = "raw_json";
    /// Blob id (64 hex) of the encrypted `content`; segment format 2 only.
    pub const CONTENT_REF: &str = "content_ref";
    /// Blob id (64 hex) of the encrypted `raw`; segment format 2 only.
    pub const RAW_REF: &str = "raw_ref";
    pub const UNKNOWN_JSON: &str = "unknown_json";
}

/// Physical layout of a segment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Layout {
    /// Format 1: content inline, no ref columns.
    Inline,
    /// Format 2: content in blobs, ref columns present.
    Refs,
}

/// The canonical `events` Arrow schema (segment format 2: inline columns
/// plus `content_ref`/`raw_ref`). Every batch handed out by this module
/// uses it, whatever the file's format version.
pub fn events_schema() -> SchemaRef {
    static SCHEMA: OnceLock<SchemaRef> = OnceLock::new();
    SCHEMA.get_or_init(|| build_schema(Layout::Refs)).clone()
}

/// The segment format 1 schema (no ref columns), used for files written
/// without an encryption key.
pub fn events_schema_v1() -> SchemaRef {
    static SCHEMA: OnceLock<SchemaRef> = OnceLock::new();
    SCHEMA.get_or_init(|| build_schema(Layout::Inline)).clone()
}

fn build_schema(layout: Layout) -> SchemaRef {
    let mut fields = vec![
        field(
            col::EVENT_ID,
            DataType::FixedSizeBinary(16),
            false,
            field_id::EVENT_ID,
        ),
        field(
            col::SCHEMA_VERSION,
            DataType::UInt16,
            false,
            field_id::SCHEMA_VERSION,
        ),
        field(
            col::DEVICE_ID,
            DataType::FixedSizeBinary(16),
            false,
            field_id::DEVICE_ID,
        ),
        field(
            col::SOURCE_SEQ,
            DataType::UInt64,
            false,
            field_id::SOURCE_SEQ,
        ),
        field(col::HLC, DataType::UInt64, false, field_id::HLC),
        field(col::OBSERVED_AT, ts(), false, field_id::OBSERVED_AT),
        field(col::CAPTURED_AT, ts(), false, field_id::CAPTURED_AT),
        field(col::INGESTED_AT, ts(), true, field_id::INGESTED_AT),
        field(col::PROVIDER, dict(), false, field_id::PROVIDER),
        field(
            col::PROVIDER_VERSION,
            DataType::Utf8,
            true,
            field_id::PROVIDER_VERSION,
        ),
        field(
            col::ADAPTER_VERSION,
            DataType::Utf8,
            false,
            field_id::ADAPTER_VERSION,
        ),
        field(
            col::HOOK_VERSION,
            DataType::Utf8,
            true,
            field_id::HOOK_VERSION,
        ),
        field(col::CAPTURE_MODE, dict(), false, field_id::CAPTURE_MODE),
        field(
            col::PROVIDER_EVENT_NAME,
            dict(),
            false,
            field_id::PROVIDER_EVENT_NAME,
        ),
        field(col::KIND, dict(), false, field_id::KIND),
        field(
            col::PROJECT_ID,
            DataType::FixedSizeBinary(16),
            false,
            field_id::PROJECT_ID,
        ),
        field(col::PROJECT_ROOT, dict(), false, field_id::PROJECT_ROOT),
        field(col::PROJECT_NAME, dict(), false, field_id::PROJECT_NAME),
        field(
            col::REPO_REMOTE,
            DataType::Utf8,
            true,
            field_id::REPO_REMOTE,
        ),
        field(col::BRANCH, DataType::Utf8, true, field_id::GIT_BRANCH),
        field(col::HEAD, DataType::Utf8, true, field_id::GIT_HEAD),
        field(
            col::SESSION_ID,
            DataType::FixedSizeBinary(16),
            false,
            field_id::SESSION_ID,
        ),
        field(
            col::PROVIDER_SESSION_ID,
            DataType::Utf8,
            false,
            field_id::PROVIDER_SESSION_ID,
        ),
        field(
            col::PROVIDER_TURN_ID,
            DataType::Utf8,
            true,
            field_id::PROVIDER_TURN_ID,
        ),
        field(
            col::SPAN_ID,
            DataType::FixedSizeBinary(16),
            true,
            field_id::SPAN_ID,
        ),
        field(
            col::PARENT_SPAN_ID,
            DataType::FixedSizeBinary(16),
            true,
            field_id::PARENT_SPAN_ID,
        ),
        field(
            col::AGENT_ID,
            DataType::FixedSizeBinary(16),
            false,
            field_id::AGENT_ID,
        ),
        field(col::AGENT_TYPE, DataType::Utf8, true, field_id::AGENT_TYPE),
        field(
            col::PARENT_AGENT_ID,
            DataType::FixedSizeBinary(16),
            true,
            field_id::PARENT_AGENT_ID,
        ),
        field(col::MODEL, DataType::Utf8, true, field_id::MODEL),
        field(
            col::PROVIDER_AGENT_ID,
            DataType::Utf8,
            true,
            field_id::PROVIDER_AGENT_ID,
        ),
        field(col::TOOL_NAME, dict(), true, field_id::TOOL_NAME),
        field(col::TOOL_CATEGORY, dict(), true, field_id::TOOL_CATEGORY),
        field(
            col::TOOL_CALL_ID,
            DataType::Utf8,
            true,
            field_id::TOOL_CALL_ID,
        ),
        field(
            col::PATH_LOGICAL,
            DataType::Utf8,
            true,
            field_id::PATH_LOGICAL,
        ),
        field(
            col::PATH_RELATIVE,
            DataType::Utf8,
            true,
            field_id::PATH_RELATIVE,
        ),
        field(col::PATHS_JSON, DataType::Utf8, true, field_id::PATHS),
        field(col::OUTCOME_STATUS, dict(), true, field_id::OUTCOME_STATUS),
        field(
            col::OUTCOME_CLASS,
            DataType::Utf8,
            true,
            field_id::OUTCOME_CLASS,
        ),
        field(col::EXIT_CODE, DataType::Int32, true, field_id::EXIT_CODE),
        field(
            col::DURATION_MS,
            DataType::UInt64,
            true,
            field_id::DURATION_MS,
        ),
        field(col::ATTRS_JSON, DataType::Utf8, false, field_id::ATTRS),
    ];
    let format_version = match layout {
        Layout::Inline => {
            fields.push(field(
                col::CONTENT_JSON,
                DataType::Utf8,
                true,
                field_id::CONTENT_REF,
            ));
            fields.push(field(
                col::RAW_JSON,
                DataType::Utf8,
                true,
                field_id::RAW_REF,
            ));
            SEGMENT_FORMAT_VERSION_INLINE
        }
        Layout::Refs => {
            fields.push(field_derived(
                col::CONTENT_JSON,
                DataType::Utf8,
                true,
                field_id::CONTENT_REF,
                "inline",
            ));
            fields.push(field_derived(
                col::RAW_JSON,
                DataType::Utf8,
                true,
                field_id::RAW_REF,
                "inline",
            ));
            fields.push(field_derived(
                col::CONTENT_REF,
                DataType::Utf8,
                true,
                field_id::CONTENT_REF,
                "ref",
            ));
            fields.push(field_derived(
                col::RAW_REF,
                DataType::Utf8,
                true,
                field_id::RAW_REF,
                "ref",
            ));
            SEGMENT_FORMAT_VERSION
        }
    };
    fields.push(field(
        col::UNKNOWN_JSON,
        DataType::Utf8,
        true,
        field_id::UNKNOWN,
    ));
    let mut md = HashMap::new();
    md.insert(
        "attemptdb.format_version".to_string(),
        format_version.to_string(),
    );
    md.insert(
        "attemptdb.schema_version".to_string(),
        CANONICAL_SCHEMA_VERSION.to_string(),
    );
    Arc::new(Schema::new_with_metadata(fields, md))
}

struct Builders {
    event_id: FixedSizeBinaryBuilder,
    schema_version: UInt16Builder,
    device_id: FixedSizeBinaryBuilder,
    source_seq: UInt64Builder,
    hlc: UInt64Builder,
    observed_at: Vec<i64>,
    captured_at: Vec<i64>,
    ingested_at: Vec<Option<i64>>,
    provider: StringDictionaryBuilder<Int32Type>,
    provider_version: StringBuilder,
    adapter_version: StringBuilder,
    hook_version: StringBuilder,
    capture_mode: StringDictionaryBuilder<Int32Type>,
    provider_event_name: StringDictionaryBuilder<Int32Type>,
    kind: StringDictionaryBuilder<Int32Type>,
    project_id: FixedSizeBinaryBuilder,
    project_root: StringDictionaryBuilder<Int32Type>,
    project_name: StringDictionaryBuilder<Int32Type>,
    repo_remote: StringBuilder,
    branch: StringBuilder,
    head: StringBuilder,
    session_id: FixedSizeBinaryBuilder,
    provider_session_id: StringBuilder,
    provider_turn_id: StringBuilder,
    span_id: FixedSizeBinaryBuilder,
    parent_span_id: FixedSizeBinaryBuilder,
    agent_id: FixedSizeBinaryBuilder,
    agent_type: StringBuilder,
    parent_agent_id: FixedSizeBinaryBuilder,
    model: StringBuilder,
    provider_agent_id: StringBuilder,
    tool_name: StringDictionaryBuilder<Int32Type>,
    tool_category: StringDictionaryBuilder<Int32Type>,
    tool_call_id: StringBuilder,
    path_logical: StringBuilder,
    path_relative: StringBuilder,
    paths_json: StringBuilder,
    outcome_status: StringDictionaryBuilder<Int32Type>,
    outcome_class: StringBuilder,
    exit_code: Int32Builder,
    duration_ms: UInt64Builder,
    attrs_json: StringBuilder,
    content_json: StringBuilder,
    raw_json: StringBuilder,
    content_ref: StringBuilder,
    raw_ref: StringBuilder,
    unknown_json: StringBuilder,
    layout: Layout,
}

impl Builders {
    fn new(n: usize, layout: Layout) -> Self {
        Self {
            event_id: FixedSizeBinaryBuilder::with_capacity(n, 16),
            schema_version: UInt16Builder::with_capacity(n),
            device_id: FixedSizeBinaryBuilder::with_capacity(n, 16),
            source_seq: UInt64Builder::with_capacity(n),
            hlc: UInt64Builder::with_capacity(n),
            observed_at: Vec::with_capacity(n),
            captured_at: Vec::with_capacity(n),
            ingested_at: Vec::with_capacity(n),
            provider: StringDictionaryBuilder::new(),
            provider_version: StringBuilder::new(),
            adapter_version: StringBuilder::new(),
            hook_version: StringBuilder::new(),
            capture_mode: StringDictionaryBuilder::new(),
            provider_event_name: StringDictionaryBuilder::new(),
            kind: StringDictionaryBuilder::new(),
            project_id: FixedSizeBinaryBuilder::with_capacity(n, 16),
            project_root: StringDictionaryBuilder::new(),
            project_name: StringDictionaryBuilder::new(),
            repo_remote: StringBuilder::new(),
            branch: StringBuilder::new(),
            head: StringBuilder::new(),
            session_id: FixedSizeBinaryBuilder::with_capacity(n, 16),
            provider_session_id: StringBuilder::new(),
            provider_turn_id: StringBuilder::new(),
            span_id: FixedSizeBinaryBuilder::with_capacity(n, 16),
            parent_span_id: FixedSizeBinaryBuilder::with_capacity(n, 16),
            agent_id: FixedSizeBinaryBuilder::with_capacity(n, 16),
            agent_type: StringBuilder::new(),
            parent_agent_id: FixedSizeBinaryBuilder::with_capacity(n, 16),
            model: StringBuilder::new(),
            provider_agent_id: StringBuilder::new(),
            tool_name: StringDictionaryBuilder::new(),
            tool_category: StringDictionaryBuilder::new(),
            tool_call_id: StringBuilder::new(),
            path_logical: StringBuilder::new(),
            path_relative: StringBuilder::new(),
            paths_json: StringBuilder::new(),
            outcome_status: StringDictionaryBuilder::new(),
            outcome_class: StringBuilder::new(),
            exit_code: Int32Builder::with_capacity(n),
            duration_ms: UInt64Builder::with_capacity(n),
            attrs_json: StringBuilder::new(),
            content_json: StringBuilder::new(),
            raw_json: StringBuilder::new(),
            content_ref: StringBuilder::new(),
            raw_ref: StringBuilder::new(),
            unknown_json: StringBuilder::new(),
            layout,
        }
    }

    fn push(&mut self, ev: &Event, sink: Option<&BlobSink>) -> Result<()> {
        self.event_id.append_value(ev.event_id.as_bytes())?;
        self.schema_version.append_value(ev.schema_version);
        self.device_id.append_value(ev.device_id.as_bytes())?;
        self.source_seq.append_value(ev.source_seq);
        self.hlc.append_value(ev.hlc.as_u64());
        self.observed_at.push(ev.observed_at.as_micros());
        self.captured_at.push(ev.captured_at.as_micros());
        self.ingested_at
            .push(ev.ingested_at.map(Timestamp::as_micros));
        self.provider.append_value(ev.provider.as_str());
        self.provider_version
            .append_option(ev.provider_version.as_deref());
        self.adapter_version.append_value(&ev.adapter_version);
        self.hook_version.append_option(ev.hook_version.as_deref());
        self.capture_mode.append_value(ev.capture_mode.as_str());
        self.provider_event_name
            .append_value(&ev.provider_event_name);
        self.kind.append_value(ev.kind.as_str());
        self.project_id
            .append_value(ev.project.project_id.as_bytes())?;
        self.project_root.append_value(&ev.project.root);
        self.project_name.append_value(&ev.project.name);
        self.repo_remote
            .append_option(ev.project.repo_remote.as_deref());
        self.branch.append_option(ev.project.branch.as_deref());
        self.head.append_option(ev.project.head.as_deref());
        self.session_id.append_value(ev.session_id.as_bytes())?;
        self.provider_session_id
            .append_value(&ev.provider_session_id);
        self.provider_turn_id
            .append_option(ev.provider_turn_id.as_deref());
        append_opt_fsb(&mut self.span_id, ev.span_id.as_ref().map(|s| s.as_bytes()))?;
        append_opt_fsb(
            &mut self.parent_span_id,
            ev.parent_span_id.as_ref().map(|s| s.as_bytes()),
        )?;
        self.agent_id.append_value(ev.agent.agent_id.as_bytes())?;
        self.agent_type
            .append_option(ev.agent.agent_type.as_deref());
        append_opt_fsb(
            &mut self.parent_agent_id,
            ev.agent.parent_agent_id.as_ref().map(|s| s.as_bytes()),
        )?;
        self.model.append_option(ev.agent.model.as_deref());
        self.provider_agent_id
            .append_option(ev.agent.provider_agent_id.as_deref());
        match &ev.tool {
            Some(t) => {
                self.tool_name.append_value(&t.name);
                self.tool_category.append_value(t.category.as_str());
                self.tool_call_id.append_option(t.call_id.as_deref());
            }
            None => {
                self.tool_name.append_null();
                self.tool_category.append_null();
                self.tool_call_id.append_null();
            }
        }
        match ev.paths.first() {
            Some(p) => {
                self.path_logical.append_value(&p.logical);
                self.path_relative.append_option(p.repo_relative.as_deref());
                self.paths_json
                    .append_value(serde_json::to_string(&ev.paths)?);
            }
            None => {
                self.path_logical.append_null();
                self.path_relative.append_null();
                self.paths_json.append_null();
            }
        }
        match &ev.outcome {
            Some(o) => {
                self.outcome_status.append_value(o.status.as_str());
                self.outcome_class.append_option(o.class.as_deref());
                self.exit_code.append_option(o.exit_code);
            }
            None => {
                self.outcome_status.append_null();
                self.outcome_class.append_null();
                self.exit_code.append_null();
            }
        }
        self.duration_ms.append_option(ev.duration_ms);
        self.attrs_json
            .append_value(serde_json::to_string(&ev.attrs)?);
        match (&ev.content, sink) {
            (Some(c), Some(sink)) if !c.is_empty() => {
                let id = sink.put(&serde_json::to_vec(c)?)?;
                self.content_json.append_null();
                self.content_ref.append_value(id.to_hex());
            }
            (Some(c), None) if !c.is_empty() => {
                self.content_json.append_value(serde_json::to_string(c)?);
                self.content_ref.append_null();
            }
            _ => {
                self.content_json.append_null();
                self.content_ref.append_null();
            }
        }
        match (&ev.raw, sink) {
            (Some(r), Some(sink)) => {
                let id = sink.put(&serde_json::to_vec(r)?)?;
                self.raw_json.append_null();
                self.raw_ref.append_value(id.to_hex());
            }
            (Some(r), None) => {
                self.raw_json.append_value(serde_json::to_string(r)?);
                self.raw_ref.append_null();
            }
            (None, _) => {
                self.raw_json.append_null();
                self.raw_ref.append_null();
            }
        }
        if ev.unknown.is_empty() {
            self.unknown_json.append_null();
        } else {
            self.unknown_json
                .append_value(serde_json::to_string(&ev.unknown)?);
        }
        Ok(())
    }

    fn finish(mut self) -> Result<RecordBatch> {
        let mut columns: Vec<ArrayRef> = vec![
            Arc::new(self.event_id.finish()),
            Arc::new(self.schema_version.finish()),
            Arc::new(self.device_id.finish()),
            Arc::new(self.source_seq.finish()),
            Arc::new(self.hlc.finish()),
            Arc::new(TimestampMicrosecondArray::from(self.observed_at).with_timezone("UTC")),
            Arc::new(TimestampMicrosecondArray::from(self.captured_at).with_timezone("UTC")),
            Arc::new(TimestampMicrosecondArray::from(self.ingested_at).with_timezone("UTC")),
            Arc::new(self.provider.finish()),
            Arc::new(self.provider_version.finish()),
            Arc::new(self.adapter_version.finish()),
            Arc::new(self.hook_version.finish()),
            Arc::new(self.capture_mode.finish()),
            Arc::new(self.provider_event_name.finish()),
            Arc::new(self.kind.finish()),
            Arc::new(self.project_id.finish()),
            Arc::new(self.project_root.finish()),
            Arc::new(self.project_name.finish()),
            Arc::new(self.repo_remote.finish()),
            Arc::new(self.branch.finish()),
            Arc::new(self.head.finish()),
            Arc::new(self.session_id.finish()),
            Arc::new(self.provider_session_id.finish()),
            Arc::new(self.provider_turn_id.finish()),
            Arc::new(self.span_id.finish()),
            Arc::new(self.parent_span_id.finish()),
            Arc::new(self.agent_id.finish()),
            Arc::new(self.agent_type.finish()),
            Arc::new(self.parent_agent_id.finish()),
            Arc::new(self.model.finish()),
            Arc::new(self.provider_agent_id.finish()),
            Arc::new(self.tool_name.finish()),
            Arc::new(self.tool_category.finish()),
            Arc::new(self.tool_call_id.finish()),
            Arc::new(self.path_logical.finish()),
            Arc::new(self.path_relative.finish()),
            Arc::new(self.paths_json.finish()),
            Arc::new(self.outcome_status.finish()),
            Arc::new(self.outcome_class.finish()),
            Arc::new(self.exit_code.finish()),
            Arc::new(self.duration_ms.finish()),
            Arc::new(self.attrs_json.finish()),
            Arc::new(self.content_json.finish()),
            Arc::new(self.raw_json.finish()),
        ];
        let schema = match self.layout {
            Layout::Inline => events_schema_v1(),
            Layout::Refs => {
                columns.push(Arc::new(self.content_ref.finish()));
                columns.push(Arc::new(self.raw_ref.finish()));
                events_schema()
            }
        };
        columns.push(Arc::new(self.unknown_json.finish()));
        Ok(RecordBatch::try_new(schema, columns)?)
    }
}

fn append_opt_fsb(b: &mut FixedSizeBinaryBuilder, v: Option<&[u8; 16]>) -> Result<()> {
    match v {
        Some(bytes) => b.append_value(bytes)?,
        None => b.append_null(),
    }
    Ok(())
}

/// Convert events into one `RecordBatch` with the canonical schema.
/// `content`/`raw` stay inline (this is what the memtable and the query
/// layer use); the ref columns are present and null.
pub fn events_to_batch(events: &[Event]) -> Result<RecordBatch> {
    build_batch(events, None, Layout::Refs)
}

fn build_batch(events: &[Event], sink: Option<&BlobSink>, layout: Layout) -> Result<RecordBatch> {
    let mut b = Builders::new(events.len(), layout);
    for ev in events {
        b.push(ev, sink)?;
    }
    b.finish()
}

/// Bring a batch read from any supported segment version onto the
/// canonical schema: columns are matched by name, missing (nullable) ones
/// are filled with nulls.
pub fn normalize_batch(batch: RecordBatch) -> Result<RecordBatch> {
    let schema = events_schema();
    if batch.schema() == schema {
        return Ok(batch);
    }
    let n = batch.num_rows();
    let mut columns = Vec::with_capacity(schema.fields().len());
    for f in schema.fields() {
        match batch.schema().index_of(f.name()) {
            Ok(i) => columns.push(batch.column(i).clone()),
            Err(_) if f.is_nullable() => columns.push(new_null_array(f.data_type(), n)),
            Err(e) => return Err(StorageError::Arrow(e)),
        }
    }
    Ok(RecordBatch::try_new(schema, columns)?)
}

// ---------------------------------------------------------------------------
// Decoding
// ---------------------------------------------------------------------------

fn str_col(batch: &RecordBatch, name: &str) -> Result<Option<Arc<dyn Array>>> {
    let Some(idx) = batch.schema().index_of(name).ok() else {
        return Ok(None);
    };
    let col = batch.column(idx);
    let arr = if matches!(col.data_type(), DataType::Dictionary(_, _)) {
        arrow::compute::cast(col, &DataType::Utf8)?
    } else {
        col.clone()
    };
    Ok(Some(arr))
}

struct Cols {
    strings: HashMap<&'static str, Arc<dyn Array>>,
    batch: RecordBatch,
}

impl Cols {
    fn new(batch: RecordBatch) -> Result<Self> {
        let names: &[&'static str] = &[
            col::PROVIDER,
            col::PROVIDER_VERSION,
            col::ADAPTER_VERSION,
            col::HOOK_VERSION,
            col::CAPTURE_MODE,
            col::PROVIDER_EVENT_NAME,
            col::KIND,
            col::PROJECT_ROOT,
            col::PROJECT_NAME,
            col::REPO_REMOTE,
            col::BRANCH,
            col::HEAD,
            col::PROVIDER_SESSION_ID,
            col::PROVIDER_TURN_ID,
            col::AGENT_TYPE,
            col::MODEL,
            col::PROVIDER_AGENT_ID,
            col::TOOL_NAME,
            col::TOOL_CATEGORY,
            col::TOOL_CALL_ID,
            col::PATHS_JSON,
            col::OUTCOME_STATUS,
            col::OUTCOME_CLASS,
            col::ATTRS_JSON,
            col::CONTENT_JSON,
            col::RAW_JSON,
            col::CONTENT_REF,
            col::RAW_REF,
            col::UNKNOWN_JSON,
        ];
        let mut strings = HashMap::new();
        for n in names {
            if let Some(a) = str_col(&batch, n)? {
                strings.insert(*n, a);
            }
        }
        Ok(Self { strings, batch })
    }

    fn s(&self, name: &str, row: usize) -> Option<String> {
        let a = self.strings.get(name)?;
        let a = a.as_string::<i32>();
        if a.is_null(row) {
            None
        } else {
            Some(a.value(row).to_string())
        }
    }

    fn fsb(&self, name: &str, row: usize) -> Option<[u8; 16]> {
        let idx = self.batch.schema().index_of(name).ok()?;
        let a = self.batch.column(idx).as_fixed_size_binary();
        if a.is_null(row) {
            return None;
        }
        let mut out = [0u8; 16];
        out.copy_from_slice(a.value(row));
        Some(out)
    }

    fn u64(&self, name: &str, row: usize) -> Option<u64> {
        let idx = self.batch.schema().index_of(name).ok()?;
        let a = self.batch.column(idx).as_primitive::<UInt64Type>();
        if a.is_null(row) {
            None
        } else {
            Some(a.value(row))
        }
    }

    fn u16(&self, name: &str, row: usize) -> Option<u16> {
        let idx = self.batch.schema().index_of(name).ok()?;
        let a = self.batch.column(idx).as_primitive::<UInt16Type>();
        if a.is_null(row) {
            None
        } else {
            Some(a.value(row))
        }
    }

    fn i32(&self, name: &str, row: usize) -> Option<i32> {
        let idx = self.batch.schema().index_of(name).ok()?;
        let a = self
            .batch
            .column(idx)
            .as_primitive::<arrow::datatypes::Int32Type>();
        if a.is_null(row) {
            None
        } else {
            Some(a.value(row))
        }
    }

    fn ts(&self, name: &str, row: usize) -> Option<Timestamp> {
        let idx = self.batch.schema().index_of(name).ok()?;
        let a = self
            .batch
            .column(idx)
            .as_primitive::<TimestampMicrosecondType>();
        if a.is_null(row) {
            None
        } else {
            Some(Timestamp::from_micros(a.value(row)))
        }
    }

    fn json<T: serde::de::DeserializeOwned>(&self, name: &str, row: usize) -> Option<T> {
        self.s(name, row)
            .and_then(|s| serde_json::from_str(&s).ok())
    }
}

/// Decode a batch with the canonical schema back into events. Blob refs are
/// left unresolved (`content`/`raw` come back `None`); use
/// [`batch_to_events_with`] to decrypt them.
pub fn batch_to_events(batch: &RecordBatch) -> Result<Vec<Event>> {
    batch_to_events_with(batch, None)
}

/// Decode a batch, resolving `content_ref`/`raw_ref` through `reader` when
/// one is given. Rows whose blob cannot be read keep `None`; the reader
/// records why.
pub fn batch_to_events_with(
    batch: &RecordBatch,
    reader: Option<&BlobReader<'_>>,
) -> Result<Vec<Event>> {
    let n = batch.num_rows();
    let c = Cols::new(batch.clone())?;
    let mut out = Vec::with_capacity(n);
    for row in 0..n {
        let provider: Provider = c
            .s(col::PROVIDER, row)
            .unwrap_or_default()
            .parse()
            .expect("infallible");
        let kind = c
            .s(col::KIND, row)
            .and_then(|k| EventKind::parse(&k))
            .unwrap_or(EventKind::Unknown);
        let capture_mode: CaptureMode = c
            .s(col::CAPTURE_MODE, row)
            .and_then(|m| m.parse().ok())
            .unwrap_or_default();
        let tool = c.s(col::TOOL_NAME, row).map(|name| ToolRef {
            name,
            category: c
                .s(col::TOOL_CATEGORY, row)
                .and_then(|s| parse_category(&s))
                .unwrap_or(ToolCategory::Other),
            call_id: c.s(col::TOOL_CALL_ID, row),
        });
        let outcome = c.s(col::OUTCOME_STATUS, row).map(|s| Outcome {
            status: parse_status(&s),
            class: c.s(col::OUTCOME_CLASS, row),
            exit_code: c.i32(col::EXIT_CODE, row),
        });
        let paths: Vec<PortablePath> = c.json(col::PATHS_JSON, row).unwrap_or_default();
        let attrs: serde_json::Map<String, serde_json::Value> =
            c.json(col::ATTRS_JSON, row).unwrap_or_default();
        let content: Option<EventContent> = match c.s(col::CONTENT_JSON, row) {
            Some(inline) => serde_json::from_str(&inline).ok(),
            None => c
                .s(col::CONTENT_REF, row)
                .and_then(|r| resolve_ref(reader, &r))
                .and_then(|bytes| serde_json::from_slice(&bytes).ok()),
        };
        let raw: Option<serde_json::Value> = match c.s(col::RAW_JSON, row) {
            Some(inline) => serde_json::from_str(&inline).ok(),
            None => c
                .s(col::RAW_REF, row)
                .and_then(|r| resolve_ref(reader, &r))
                .and_then(|bytes| serde_json::from_slice(&bytes).ok()),
        };
        let unknown: serde_json::Map<String, serde_json::Value> =
            c.json(col::UNKNOWN_JSON, row).unwrap_or_default();
        let ev = Event {
            event_id: EventId::from_bytes(c.fsb(col::EVENT_ID, row).unwrap_or([0; 16])),
            schema_version: c
                .u16(col::SCHEMA_VERSION, row)
                .unwrap_or(CANONICAL_SCHEMA_VERSION),
            device_id: DeviceId::from_bytes(c.fsb(col::DEVICE_ID, row).unwrap_or([0; 16])),
            source_seq: c.u64(col::SOURCE_SEQ, row).unwrap_or(0),
            hlc: Hlc(c.u64(col::HLC, row).unwrap_or(0)),
            observed_at: c.ts(col::OBSERVED_AT, row).unwrap_or_default(),
            captured_at: c.ts(col::CAPTURED_AT, row).unwrap_or_default(),
            ingested_at: c.ts(col::INGESTED_AT, row),
            provider,
            provider_version: c.s(col::PROVIDER_VERSION, row),
            adapter_version: c.s(col::ADAPTER_VERSION, row).unwrap_or_default(),
            hook_version: c.s(col::HOOK_VERSION, row),
            capture_mode,
            provider_event_name: c.s(col::PROVIDER_EVENT_NAME, row).unwrap_or_default(),
            kind,
            project: ProjectRef {
                project_id: ProjectId::from_bytes(c.fsb(col::PROJECT_ID, row).unwrap_or([0; 16])),
                root: c.s(col::PROJECT_ROOT, row).unwrap_or_default(),
                name: c.s(col::PROJECT_NAME, row).unwrap_or_default(),
                repo_remote: c.s(col::REPO_REMOTE, row),
                branch: c.s(col::BRANCH, row),
                head: c.s(col::HEAD, row),
            },
            session_id: SessionId::from_bytes(c.fsb(col::SESSION_ID, row).unwrap_or([0; 16])),
            provider_session_id: c.s(col::PROVIDER_SESSION_ID, row).unwrap_or_default(),
            provider_turn_id: c.s(col::PROVIDER_TURN_ID, row),
            span_id: c.fsb(col::SPAN_ID, row).map(SpanId::from_bytes),
            parent_span_id: c.fsb(col::PARENT_SPAN_ID, row).map(SpanId::from_bytes),
            agent: AgentRef {
                agent_id: AgentId::from_bytes(c.fsb(col::AGENT_ID, row).unwrap_or([0; 16])),
                provider_agent_id: c.s(col::PROVIDER_AGENT_ID, row),
                agent_type: c.s(col::AGENT_TYPE, row),
                parent_agent_id: c.fsb(col::PARENT_AGENT_ID, row).map(AgentId::from_bytes),
                model: c.s(col::MODEL, row),
            },
            tool,
            paths,
            outcome,
            duration_ms: c.u64(col::DURATION_MS, row),
            attrs,
            content,
            raw,
            unknown,
        };
        out.push(ev);
    }
    Ok(out)
}

fn resolve_ref(reader: Option<&BlobReader<'_>>, hex: &str) -> Option<Vec<u8>> {
    let id = BlobId::from_hex(hex)?;
    reader?.resolve(&id)
}

/// Every blob id referenced by a batch (`content_ref` and `raw_ref`).
pub fn collect_blob_refs(batch: &RecordBatch) -> Vec<BlobId> {
    let mut out = Vec::new();
    for name in [col::CONTENT_REF, col::RAW_REF] {
        let Ok(idx) = batch.schema().index_of(name) else {
            continue;
        };
        let a = batch.column(idx).as_string::<i32>();
        for i in 0..a.len() {
            if !a.is_null(i)
                && let Some(id) = BlobId::from_hex(a.value(i))
            {
                out.push(id);
            }
        }
    }
    out
}

/// Fill `content_json`/`raw_json` from the ref columns so SQL consumers see
/// content exactly as with inline segments. Rows that cannot be resolved
/// stay null; the ref columns are kept.
pub fn resolve_batch(batch: &RecordBatch, reader: &BlobReader<'_>) -> Result<RecordBatch> {
    let schema = batch.schema();
    let (Ok(cj), Ok(rj), Ok(cr), Ok(rr)) = (
        schema.index_of(col::CONTENT_JSON),
        schema.index_of(col::RAW_JSON),
        schema.index_of(col::CONTENT_REF),
        schema.index_of(col::RAW_REF),
    ) else {
        return Ok(batch.clone());
    };
    let resolve_col = |inline: usize, refs: usize| -> Option<ArrayRef> {
        let refs = batch.column(refs).as_string::<i32>();
        if refs.null_count() == refs.len() {
            return None;
        }
        let inline = batch.column(inline).as_string::<i32>();
        let mut b = StringBuilder::new();
        for row in 0..refs.len() {
            if !inline.is_null(row) {
                b.append_value(inline.value(row));
            } else if !refs.is_null(row)
                && let Some(bytes) = resolve_ref(Some(reader), refs.value(row))
                && let Ok(text) = String::from_utf8(bytes)
            {
                b.append_value(text);
            } else {
                b.append_null();
            }
        }
        Some(Arc::new(b.finish()))
    };
    let content = resolve_col(cj, cr);
    let raw = resolve_col(rj, rr);
    if content.is_none() && raw.is_none() {
        return Ok(batch.clone());
    }
    let mut columns = batch.columns().to_vec();
    if let Some(c) = content {
        columns[cj] = c;
    }
    if let Some(r) = raw {
        columns[rj] = r;
    }
    Ok(RecordBatch::try_new(schema, columns)?)
}

fn parse_category(s: &str) -> Option<ToolCategory> {
    Some(match s {
        "shell" => ToolCategory::Shell,
        "file_read" => ToolCategory::FileRead,
        "file_write" => ToolCategory::FileWrite,
        "file_edit" => ToolCategory::FileEdit,
        "search" => ToolCategory::Search,
        "web" => ToolCategory::Web,
        "mcp" => ToolCategory::Mcp,
        "subagent" => ToolCategory::Subagent,
        "plan" => ToolCategory::Plan,
        "notebook" => ToolCategory::Notebook,
        "other" => ToolCategory::Other,
        _ => return None,
    })
}

fn parse_status(s: &str) -> OutcomeStatus {
    match s {
        "success" => OutcomeStatus::Success,
        "failure" => OutcomeStatus::Failure,
        "denied" => OutcomeStatus::Denied,
        "cancelled" => OutcomeStatus::Cancelled,
        _ => OutcomeStatus::Unknown,
    }
}

// ---------------------------------------------------------------------------
// Files
// ---------------------------------------------------------------------------

pub fn segments_dir(root: &Path) -> PathBuf {
    root.join(SEGMENTS_DIR)
}

/// Write `events` (already ingested, sorted by source_seq) as a new format 1
/// segment with `content`/`raw` inline. See [`write_segment_with`].
pub fn write_segment(root: &Path, events: &[Event]) -> Result<SegmentMeta> {
    write_segment_with(root, events, None)
}

/// Write `events` as a new segment. With a [`BlobSink`], `content` and `raw`
/// are encrypted into blobs first (each durable before the segment is
/// published) and the file is format 2 with ref columns; without one it is
/// format 1 with inline JSON. The file is fully written and fsynced before
/// the returned metadata can be referenced by a manifest generation.
pub fn write_segment_with(
    root: &Path,
    events: &[Event],
    sink: Option<&BlobSink>,
) -> Result<SegmentMeta> {
    if events.is_empty() {
        return Err(StorageError::Other(
            "refusing to write an empty segment".into(),
        ));
    }
    let dir = segments_dir(root);
    std::fs::create_dir_all(&dir).at(&dir)?;
    let segment_id = Uuid::now_v7();
    let file_name = format!("seg-{}.arrow", segment_id.simple());
    let layout = if sink.is_some() {
        Layout::Refs
    } else {
        Layout::Inline
    };
    let batch = build_batch(events, sink, layout)?;

    let mut buf = Cursor::new(Vec::with_capacity(events.len() * 512));
    {
        let opts = IpcWriteOptions::default().try_with_compression(Some(CompressionType::ZSTD))?;
        let mut writer = FileWriter::try_new_with_options(&mut buf, &batch.schema(), opts)?;
        writer.write(&batch)?;
        writer.finish()?;
    }
    let bytes = buf.into_inner();
    let sha256 = {
        use sha2::{Digest, Sha256};
        hex::encode(Sha256::digest(&bytes))
    };
    let path = dir.join(&file_name);
    let tmp = dir.join(format!("{file_name}.tmp"));
    crate::manifest::write_tmp_synced(&tmp, &bytes, Some(failpoint::SEGMENT_WRITE))?;
    failpoint::hit(failpoint::SEGMENT_AFTER_TMP_WRITE);
    crate::manifest::publish_tmp(&tmp, &path)?;
    failpoint::hit(failpoint::SEGMENT_AFTER_RENAME);

    let mut providers = BTreeSet::new();
    let mut projects = BTreeSet::new();
    let mut sessions = BTreeSet::new();
    let mut min_obs = i64::MAX;
    let mut max_obs = i64::MIN;
    let mut min_hlc = u64::MAX;
    let mut max_hlc = 0u64;
    let mut min_seq = u64::MAX;
    let mut max_seq = 0u64;
    let mut min_id = events[0].event_id;
    let mut max_id = events[0].event_id;
    for ev in events {
        providers.insert(ev.provider.as_str().to_string());
        projects.insert(ev.project.project_id);
        sessions.insert(ev.session_id);
        min_obs = min_obs.min(ev.observed_at.as_micros());
        max_obs = max_obs.max(ev.observed_at.as_micros());
        min_hlc = min_hlc.min(ev.hlc.as_u64());
        max_hlc = max_hlc.max(ev.hlc.as_u64());
        min_seq = min_seq.min(ev.source_seq);
        max_seq = max_seq.max(ev.source_seq);
        if ev.event_id < min_id {
            min_id = ev.event_id;
        }
        if ev.event_id > max_id {
            max_id = ev.event_id;
        }
    }
    Ok(SegmentMeta {
        segment_id,
        file: file_name,
        rows: events.len() as u64,
        bytes: bytes.len() as u64,
        min_observed_at: Timestamp::from_micros(min_obs),
        max_observed_at: Timestamp::from_micros(max_obs),
        min_hlc: Hlc(min_hlc),
        max_hlc: Hlc(max_hlc),
        min_source_seq: min_seq,
        max_source_seq: max_seq,
        min_event_id: min_id,
        max_event_id: max_id,
        providers: providers.into_iter().collect(),
        project_ids: projects.into_iter().collect(),
        session_count: sessions.len() as u64,
        sha256,
    })
}

fn open_reader(path: &Path) -> Result<(FileReader<std::io::BufReader<std::fs::File>>, u16)> {
    // Anything Arrow rejects in a segment file is damage to an immutable
    // file, so it is reported as corruption of that path rather than as an
    // opaque Arrow error.
    let corrupt = |e: arrow::error::ArrowError| StorageError::Corrupt {
        what: "segment",
        path: path.to_path_buf(),
        detail: e.to_string(),
    };
    let file = std::fs::File::open(path).at(path)?;
    let reader = FileReader::try_new(std::io::BufReader::new(file), None).map_err(corrupt)?;
    let format_version = reader
        .schema()
        .metadata()
        .get("attemptdb.format_version")
        .and_then(|v| v.parse::<u16>().ok())
        .unwrap_or(0);
    if !(MIN_SEGMENT_FORMAT_VERSION..=SEGMENT_FORMAT_VERSION).contains(&format_version) {
        return Err(StorageError::UnsupportedFormat {
            what: "segment",
            found: format_version,
            supported: SEGMENT_FORMAT_VERSION,
        });
    }
    Ok((reader, format_version))
}

/// The `attemptdb.format_version` of a segment file (reads only the footer).
pub fn segment_format_version(path: &Path) -> Result<u16> {
    open_reader(path).map(|(_, v)| v)
}

/// Read all batches of a segment file, normalised to the canonical schema.
pub fn read_segment_batches(path: &Path) -> Result<Vec<RecordBatch>> {
    let corrupt = |e: arrow::error::ArrowError| StorageError::Corrupt {
        what: "segment",
        path: path.to_path_buf(),
        detail: e.to_string(),
    };
    let (reader, _) = open_reader(path)?;
    let mut out = Vec::new();
    for batch in reader {
        out.push(normalize_batch(batch.map_err(corrupt)?)?);
    }
    Ok(out)
}

/// Read every event of a segment without resolving blob refs.
pub fn read_segment_events(path: &Path) -> Result<Vec<Event>> {
    read_segment_events_with(path, None)
}

/// Read every event of a segment, decrypting `content`/`raw` through
/// `reader` when one is given.
pub fn read_segment_events_with(
    path: &Path,
    reader: Option<&BlobReader<'_>>,
) -> Result<Vec<Event>> {
    let mut out = Vec::new();
    for b in read_segment_batches(path)? {
        out.extend(batch_to_events_with(&b, reader)?);
    }
    Ok(out)
}

/// Read only the event ids of a segment (for deduplication).
pub fn read_segment_event_ids(path: &Path) -> Result<Vec<EventId>> {
    let mut out = Vec::new();
    for b in read_segment_batches(path)? {
        let idx = b.schema().index_of(col::EVENT_ID)?;
        let a = b.column(idx).as_fixed_size_binary();
        for i in 0..a.len() {
            let mut bytes = [0u8; 16];
            bytes.copy_from_slice(a.value(i));
            out.push(EventId::from_bytes(bytes));
        }
    }
    Ok(out)
}

/// Verify a segment file against its manifest metadata.
pub fn verify_segment(root: &Path, meta: &SegmentMeta) -> Result<()> {
    let path = segments_dir(root).join(&meta.file);
    let bytes = std::fs::read(&path).at(&path)?;
    let sha = {
        use sha2::{Digest, Sha256};
        hex::encode(Sha256::digest(&bytes))
    };
    if sha != meta.sha256 {
        return Err(StorageError::Corrupt {
            what: "segment",
            path,
            detail: "sha256 mismatch".into(),
        });
    }
    let rows: usize = read_segment_batches(&path)?
        .iter()
        .map(|b| b.num_rows())
        .sum();
    if rows as u64 != meta.rows {
        return Err(StorageError::Corrupt {
            what: "segment",
            path,
            detail: format!("row count {rows} != manifest {}", meta.rows),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use attemptdb_core::event::{EventContent, Outcome};

    fn sample(i: u64) -> Event {
        let dev = DeviceId::nil();
        let mut ev = Event::new(
            dev,
            if i.is_multiple_of(2) {
                Provider::ClaudeCode
            } else {
                Provider::Codex
            },
            "PostToolUse",
            EventKind::ToolCallFinished,
            ProjectRef::derive("/Users/dev/proj", Some("git@github.com:o/r.git"), &dev),
            format!("sess-{}", i / 3),
            CaptureMode::LocalSemantic,
            "0.1.0",
        );
        ev.source_seq = i + 1;
        ev.hlc = Hlc::new(1_000 + i, 0);
        ev.ingested_at = Some(Timestamp::now());
        ev.tool = Some(ToolRef {
            name: "Edit".into(),
            category: ToolCategory::FileEdit,
            call_id: Some(format!("tu_{i}")),
        });
        ev.paths.push(PortablePath::from_raw(
            "/Users/dev/proj/src/한글.rs",
            Some("/Users/dev/proj"),
        ));
        ev.outcome = Some(Outcome {
            status: OutcomeStatus::Success,
            class: None,
            exit_code: Some(0),
        });
        ev.duration_ms = Some(12);
        ev.attrs.insert("file_ext".into(), serde_json::json!("rs"));
        ev.content = Some(EventContent {
            command: Some("cargo test".into()),
            ..Default::default()
        });
        ev.unknown
            .insert("future".into(), serde_json::json!({"x": 1}));
        ev
    }

    #[test]
    fn batch_roundtrip_is_lossless() {
        let events: Vec<Event> = (0..10).map(sample).collect();
        let batch = events_to_batch(&events).unwrap();
        assert_eq!(batch.num_rows(), 10);
        let back = batch_to_events(&batch).unwrap();
        assert_eq!(back, events);
    }

    #[test]
    fn segment_file_roundtrip_and_verify() {
        let dir = tempfile::tempdir().unwrap();
        let events: Vec<Event> = (0..25).map(sample).collect();
        let meta = write_segment(dir.path(), &events).unwrap();
        assert_eq!(meta.rows, 25);
        assert_eq!(meta.min_source_seq, 1);
        assert_eq!(meta.max_source_seq, 25);
        assert_eq!(
            meta.providers,
            vec!["claude_code".to_string(), "codex".to_string()]
        );
        let path = segments_dir(dir.path()).join(&meta.file);
        let back = read_segment_events(&path).unwrap();
        assert_eq!(back, events);
        verify_segment(dir.path(), &meta).unwrap();
        let ids = read_segment_event_ids(&path).unwrap();
        assert_eq!(ids.len(), 25);
        // Tamper → verify fails.
        let mut bytes = std::fs::read(&path).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 1;
        std::fs::write(&path, bytes).unwrap();
        assert!(verify_segment(dir.path(), &meta).is_err());
    }

    #[test]
    fn schema_has_field_ids_everywhere() {
        for f in events_schema().fields() {
            assert!(
                f.metadata().contains_key("attemptdb.field_id"),
                "{} lacks field id",
                f.name()
            );
        }
    }
}
