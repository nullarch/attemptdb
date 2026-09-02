//! Arrow tables built from the projection, plus the readable `events` view.
//!
//! Every projected entity becomes one row with readable prefixed ids,
//! `Timestamp(µs, UTC)` times, `List<Utf8>` id/path lists, `Float32`
//! confidence and enums as text. Tables are built with explicit builders so
//! an empty projection still registers every table with its full schema.
//!
//! Retractions: `sessions`, `turns`, `tool_calls` and `attempts` also carry
//! the rows the projector removed for a retraction, flagged
//! `retracted = true`; `SHOW` hides them unless `INCLUDING RETRACTED` is
//! given. The `events` view flags retracted events (and every event of a
//! retracted session) the same way.

use crate::error::{QueryError, Result};
use crate::graph::{Graph, endpoint_id, endpoint_type};
use crate::ids::{hyphenated, prefix_for_column, readable, readable_list, readable_opt};
use attemptdb_core::{EventId, EventKind, OutcomeStatus, SessionId, SpanId, Timestamp};
use attemptdb_project::{
    Attempt, CorrectionRef, CorrectionTarget, Projection, RetractedSet, RetractionTarget, Session,
    ToolCall, Turn, is_meta_kind,
};
use attemptdb_storage::segment::{col, events_schema};
use datafusion::arrow::array::{
    Array, ArrayRef, AsArray, BooleanArray, BooleanBuilder, Float32Builder, Int32Builder,
    Int64Builder, ListBuilder, RecordBatch, StringBuilder, TimestampMicrosecondBuilder,
};
use datafusion::arrow::compute::cast;
use datafusion::arrow::datatypes::{DataType, Field, Schema, SchemaRef, TimeUnit};
use std::collections::HashMap;
use std::sync::Arc;

pub fn ts_type() -> DataType {
    DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into()))
}

pub fn list_utf8_type() -> DataType {
    DataType::List(Arc::new(Field::new("item", DataType::Utf8, true)))
}

/// Column kinds supported by [`TableBuilder`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Utf8,
    Int64,
    Int32,
    Float32,
    Bool,
    Ts,
    ListUtf8,
}

impl Kind {
    fn data_type(self) -> DataType {
        match self {
            Kind::Utf8 => DataType::Utf8,
            Kind::Int64 => DataType::Int64,
            Kind::Int32 => DataType::Int32,
            Kind::Float32 => DataType::Float32,
            Kind::Bool => DataType::Boolean,
            Kind::Ts => ts_type(),
            Kind::ListUtf8 => list_utf8_type(),
        }
    }
}

/// One cell value handed to [`TableBuilder::push`].
#[derive(Clone, Debug)]
pub enum Val {
    Null,
    Str(String),
    I64(i64),
    I32(i32),
    F32(f32),
    Bool(bool),
    Ts(Timestamp),
    List(Vec<String>),
}

impl From<String> for Val {
    fn from(v: String) -> Self {
        Val::Str(v)
    }
}
impl From<&str> for Val {
    fn from(v: &str) -> Self {
        Val::Str(v.to_string())
    }
}
impl From<Option<String>> for Val {
    fn from(v: Option<String>) -> Self {
        v.map(Val::Str).unwrap_or(Val::Null)
    }
}
impl From<i64> for Val {
    fn from(v: i64) -> Self {
        Val::I64(v)
    }
}
impl From<u32> for Val {
    fn from(v: u32) -> Self {
        Val::I64(i64::from(v))
    }
}
impl From<u64> for Val {
    fn from(v: u64) -> Self {
        Val::I64(i64::try_from(v).unwrap_or(i64::MAX))
    }
}
impl From<Option<u64>> for Val {
    fn from(v: Option<u64>) -> Self {
        v.map(Val::from).unwrap_or(Val::Null)
    }
}
impl From<Option<u32>> for Val {
    fn from(v: Option<u32>) -> Self {
        v.map(Val::from).unwrap_or(Val::Null)
    }
}
impl From<Option<i32>> for Val {
    fn from(v: Option<i32>) -> Self {
        v.map(Val::I32).unwrap_or(Val::Null)
    }
}
impl From<f32> for Val {
    fn from(v: f32) -> Self {
        Val::F32(v)
    }
}
impl From<bool> for Val {
    fn from(v: bool) -> Self {
        Val::Bool(v)
    }
}
impl From<Timestamp> for Val {
    fn from(v: Timestamp) -> Self {
        Val::Ts(v)
    }
}
impl From<Option<Timestamp>> for Val {
    fn from(v: Option<Timestamp>) -> Self {
        v.map(Val::Ts).unwrap_or(Val::Null)
    }
}
impl From<Vec<String>> for Val {
    fn from(v: Vec<String>) -> Self {
        Val::List(v)
    }
}

enum ColBuilder {
    Utf8(StringBuilder),
    Int64(Int64Builder),
    Int32(Int32Builder),
    Float32(Float32Builder),
    Bool(BooleanBuilder),
    Ts(TimestampMicrosecondBuilder),
    ListUtf8(ListBuilder<StringBuilder>),
}

impl ColBuilder {
    fn new(kind: Kind) -> Self {
        match kind {
            Kind::Utf8 => ColBuilder::Utf8(StringBuilder::new()),
            Kind::Int64 => ColBuilder::Int64(Int64Builder::new()),
            Kind::Int32 => ColBuilder::Int32(Int32Builder::new()),
            Kind::Float32 => ColBuilder::Float32(Float32Builder::new()),
            Kind::Bool => ColBuilder::Bool(BooleanBuilder::new()),
            Kind::Ts => ColBuilder::Ts(TimestampMicrosecondBuilder::new().with_timezone("UTC")),
            Kind::ListUtf8 => ColBuilder::ListUtf8(ListBuilder::new(StringBuilder::new())),
        }
    }

    fn push(&mut self, val: Val, name: &str) -> Result<()> {
        match (self, val) {
            (ColBuilder::Utf8(b), Val::Str(s)) => b.append_value(s),
            (ColBuilder::Utf8(b), Val::Null) => b.append_null(),
            (ColBuilder::Int64(b), Val::I64(v)) => b.append_value(v),
            (ColBuilder::Int64(b), Val::I32(v)) => b.append_value(i64::from(v)),
            (ColBuilder::Int64(b), Val::Null) => b.append_null(),
            (ColBuilder::Int32(b), Val::I32(v)) => b.append_value(v),
            (ColBuilder::Int32(b), Val::Null) => b.append_null(),
            (ColBuilder::Float32(b), Val::F32(v)) => b.append_value(v),
            (ColBuilder::Float32(b), Val::Null) => b.append_null(),
            (ColBuilder::Bool(b), Val::Bool(v)) => b.append_value(v),
            (ColBuilder::Bool(b), Val::Null) => b.append_null(),
            (ColBuilder::Ts(b), Val::Ts(t)) => b.append_value(t.as_micros()),
            (ColBuilder::Ts(b), Val::Null) => b.append_null(),
            (ColBuilder::ListUtf8(b), Val::List(items)) => {
                for item in items {
                    b.values().append_value(item);
                }
                b.append(true);
            }
            (ColBuilder::ListUtf8(b), Val::Null) => b.append_null(),
            (_, other) => {
                return Err(QueryError::Exec(format!(
                    "internal: value {other:?} does not fit column '{name}'"
                )));
            }
        }
        Ok(())
    }

    fn finish(self) -> ArrayRef {
        match self {
            ColBuilder::Utf8(mut b) => Arc::new(b.finish()),
            ColBuilder::Int64(mut b) => Arc::new(b.finish()),
            ColBuilder::Int32(mut b) => Arc::new(b.finish()),
            ColBuilder::Float32(mut b) => Arc::new(b.finish()),
            ColBuilder::Bool(mut b) => Arc::new(b.finish()),
            ColBuilder::Ts(mut b) => Arc::new(b.finish()),
            ColBuilder::ListUtf8(mut b) => Arc::new(b.finish()),
        }
    }
}

/// Row-oriented builder for a fixed column specification.
pub struct TableBuilder {
    fields: Vec<Field>,
    cols: Vec<ColBuilder>,
    rows: usize,
}

impl TableBuilder {
    /// `spec` is `(name, kind, nullable)` per column.
    pub fn new(spec: &[(&str, Kind, bool)]) -> Self {
        let fields = spec
            .iter()
            .map(|(name, kind, nullable)| Field::new(*name, kind.data_type(), *nullable))
            .collect();
        let cols = spec
            .iter()
            .map(|(_, kind, _)| ColBuilder::new(*kind))
            .collect();
        Self {
            fields,
            cols,
            rows: 0,
        }
    }

    pub fn schema(&self) -> SchemaRef {
        Arc::new(Schema::new(self.fields.clone()))
    }

    pub fn push(&mut self, row: Vec<Val>) -> Result<()> {
        if row.len() != self.cols.len() {
            return Err(QueryError::Exec(format!(
                "internal: row has {} values for {} columns",
                row.len(),
                self.cols.len()
            )));
        }
        for ((col, val), field) in self.cols.iter_mut().zip(row).zip(&self.fields) {
            col.push(val, field.name())?;
        }
        self.rows += 1;
        Ok(())
    }

    pub fn len(&self) -> usize {
        self.rows
    }

    pub fn is_empty(&self) -> bool {
        self.rows == 0
    }

    pub fn finish(self) -> Result<RecordBatch> {
        let schema = Arc::new(Schema::new(self.fields));
        let columns: Vec<ArrayRef> = self.cols.into_iter().map(ColBuilder::finish).collect();
        Ok(RecordBatch::try_new(schema, columns)?)
    }
}

// ---------------------------------------------------------------------------
// Projection tables
// ---------------------------------------------------------------------------

fn dedup_push(v: &mut Vec<EventId>, id: Option<EventId>) {
    if let Some(id) = id
        && !v.contains(&id)
    {
        v.push(id);
    }
}

/// Provider / project columns copied from the owning session so every table
/// can be filtered by project or provider without a join.
struct SessionMeta {
    provider: String,
    project_id: String,
    project_name: String,
}

fn session_meta(p: &Projection, sid: SessionId) -> SessionMeta {
    match p
        .session(sid)
        .or_else(|| p.retracted.sessions.iter().find(|s| s.session_id == sid))
    {
        Some(s) => SessionMeta {
            provider: s.provider.as_str().to_string(),
            project_id: readable(&s.project_id),
            project_name: s.project_name.clone(),
        },
        None => SessionMeta {
            provider: "unknown".to_string(),
            project_id: String::new(),
            project_name: String::new(),
        },
    }
}

fn path_text(p: &attemptdb_core::PortablePath) -> String {
    p.repo_relative.clone().unwrap_or_else(|| p.logical.clone())
}

/// Tables whose rows carry a `retracted` flag (and may hold retracted rows).
pub fn has_retracted_column(table: &str) -> bool {
    matches!(
        table,
        "events" | "sessions" | "turns" | "tool_calls" | "attempts"
    )
}

/// Number of retracted rows a table holds.
pub fn retracted_rows(p: &Projection, table: &str) -> usize {
    match table {
        "sessions" => p.retracted.sessions.len(),
        "turns" => p.retracted.turns.len(),
        "tool_calls" => p.retracted.tool_calls.len(),
        "attempts" => p.retracted.attempts.len(),
        _ => 0,
    }
}

/// The projection tables in registration order.
pub const PROJECTION_TABLES: &[&str] = &[
    "sessions",
    "turns",
    "tool_calls",
    "attempts",
    "handoffs",
    "edges",
    "signals",
    "work_units",
    "decisions",
    "commits",
    "corrections",
    "retractions",
    "conflicts",
];

/// Build one projection table by name. `graph` is only read for `edges`.
pub fn projection_table(
    name: &str,
    p: &Projection,
    graph: &dyn Fn() -> Arc<Graph>,
) -> Result<RecordBatch> {
    match name {
        "sessions" => sessions_table(p),
        "turns" => turns_table(p),
        "tool_calls" => tool_calls_table(p),
        "attempts" => attempts_table(p),
        "handoffs" => handoffs_table(p),
        "edges" => edges_table(&graph()),
        "signals" => signals_table(p),
        "work_units" => work_units_table(p),
        "decisions" => decisions_table(p),
        "commits" => commits_table(p),
        "corrections" => corrections_table(p),
        "retractions" => retractions_table(p),
        "conflicts" => conflicts_table(p),
        other => Err(QueryError::Plan(format!(
            "no projection table named {other}"
        ))),
    }
}

/// Rows a projection table will hold, without building it.
pub fn projection_table_rows(name: &str, p: &Projection, graph: &dyn Fn() -> Arc<Graph>) -> usize {
    let base = match name {
        "sessions" => p.sessions.len(),
        "turns" => p.turns.len(),
        "tool_calls" => p.tool_calls.len(),
        "attempts" => p.attempts.len(),
        "handoffs" => p.handoffs.len(),
        "edges" => graph().edges.len(),
        "signals" => p.signals.len(),
        "work_units" => p.work_units.len(),
        "decisions" => p.decisions.len(),
        "commits" => p.commits.len(),
        "corrections" => p.corrections.len(),
        "retractions" => p.retractions.len(),
        "conflicts" => p.conflicts.len(),
        _ => 0,
    };
    base + retracted_rows(p, name)
}

/// The schema of each projection table, taken once from the tables built
/// over an empty projection: the builders define their columns inline, and
/// a lazy table has to announce its schema before it builds anything.
pub fn projection_schema(name: &str) -> SchemaRef {
    static SCHEMAS: std::sync::OnceLock<HashMap<&'static str, SchemaRef>> =
        std::sync::OnceLock::new();
    let all = SCHEMAS.get_or_init(|| {
        let empty = attemptdb_project::project(std::iter::empty());
        let graph = Arc::new(Graph::build(&empty));
        PROJECTION_TABLES
            .iter()
            .map(|n| {
                let b = projection_table(n, &empty, &|| Arc::clone(&graph))
                    .expect("an empty projection builds every table");
                (*n, b.schema())
            })
            .collect()
    });
    Arc::clone(&all[name])
}

pub fn session_state(s: &Session) -> &'static str {
    if s.ended_at.is_some() {
        "closed"
    } else {
        "open"
    }
}

fn sessions_table(p: &Projection) -> Result<RecordBatch> {
    let mut b = TableBuilder::new(&[
        ("session_id", Kind::Utf8, false),
        ("provider", Kind::Utf8, false),
        ("provider_session_id", Kind::Utf8, false),
        ("project_id", Kind::Utf8, false),
        ("project_name", Kind::Utf8, false),
        ("state", Kind::Utf8, false),
        ("started_at", Kind::Ts, false),
        ("ended_at", Kind::Ts, true),
        ("end_reason", Kind::Utf8, true),
        ("start_source", Kind::Utf8, true),
        ("event_count", Kind::Int64, false),
        ("turn_count", Kind::Int64, false),
        ("prompt_count", Kind::Int64, false),
        ("tool_call_count", Kind::Int64, false),
        ("failure_count", Kind::Int64, false),
        ("agents", Kind::ListUtf8, false),
        ("coverage", Kind::Utf8, false),
        ("first_event_id", Kind::Utf8, false),
        ("last_event_id", Kind::Utf8, false),
        ("last_event_at", Kind::Ts, false),
        ("start_event_id", Kind::Utf8, true),
        ("end_event_id", Kind::Utf8, true),
        ("evidence", Kind::ListUtf8, false),
        ("confidence", Kind::Float32, false),
        ("retracted", Kind::Bool, false),
    ]);
    let rows = p
        .sessions
        .iter()
        .map(|s| (s, false))
        .chain(p.retracted.sessions.iter().map(|s| (s, true)));
    for (s, retracted) in rows {
        let mut evidence = Vec::new();
        dedup_push(&mut evidence, s.start_event_id);
        dedup_push(&mut evidence, Some(s.first_event_id));
        dedup_push(&mut evidence, Some(s.last_event_id));
        dedup_push(&mut evidence, s.end_event_id);
        b.push(vec![
            readable(&s.session_id).into(),
            s.provider.as_str().into(),
            s.provider_session_id.clone().into(),
            readable(&s.project_id).into(),
            s.project_name.clone().into(),
            session_state(s).into(),
            s.started_at.into(),
            s.ended_at.into(),
            s.end_reason.clone().into(),
            s.start_source.clone().into(),
            s.event_count.into(),
            s.turn_count.into(),
            s.prompt_count.into(),
            s.tool_call_count.into(),
            s.failure_count.into(),
            readable_list(&s.agents).into(),
            s.coverage.as_str().into(),
            readable(&s.first_event_id).into(),
            readable(&s.last_event_id).into(),
            s.last_event_at.into(),
            readable_opt(&s.start_event_id).into(),
            readable_opt(&s.end_event_id).into(),
            readable_list(&evidence).into(),
            1.0f32.into(),
            retracted.into(),
        ])?;
    }
    b.finish()
}

fn calls_by_id(p: &Projection) -> HashMap<SpanId, &ToolCall> {
    p.tool_calls
        .iter()
        .chain(p.retracted.tool_calls.iter())
        .map(|c| (c.tool_call_id, c))
        .collect()
}

/// Evidence for a turn: prompt, every tool call start/end, stop, first and
/// last event, deduplicated in that order.
pub fn turn_evidence(t: &Turn, calls: &HashMap<SpanId, &ToolCall>) -> Vec<EventId> {
    let mut evidence = Vec::new();
    dedup_push(&mut evidence, t.prompt_event_id);
    dedup_push(&mut evidence, Some(t.first_event_id));
    for id in &t.tool_call_ids {
        if let Some(c) = calls.get(id) {
            dedup_push(&mut evidence, c.start_event_id);
            dedup_push(&mut evidence, c.end_event_id);
        }
    }
    dedup_push(&mut evidence, t.stop_event_id);
    dedup_push(&mut evidence, Some(t.last_event_id));
    evidence
}

fn correction_cols(c: &Option<CorrectionRef>) -> (Val, Val, Val) {
    match c {
        Some(c) => (
            readable(&c.event_id).into(),
            c.at.into(),
            c.correction_type.as_str().into(),
        ),
        None => (Val::Null, Val::Null, Val::Null),
    }
}

fn turns_table(p: &Projection) -> Result<RecordBatch> {
    let calls = calls_by_id(p);
    let mut b = TableBuilder::new(&[
        ("turn_id", Kind::Utf8, false),
        ("session_id", Kind::Utf8, false),
        ("provider", Kind::Utf8, false),
        ("project_id", Kind::Utf8, false),
        ("project_name", Kind::Utf8, false),
        ("turn_index", Kind::Int64, false),
        ("started_at", Kind::Ts, false),
        ("ended_at", Kind::Ts, true),
        ("status", Kind::Utf8, false),
        ("prompt_event_id", Kind::Utf8, true),
        ("stop_event_id", Kind::Utf8, true),
        ("tool_call_ids", Kind::ListUtf8, false),
        ("tool_call_count", Kind::Int64, false),
        ("objective", Kind::Utf8, true),
        ("prompt_chars", Kind::Int64, true),
        ("first_event_id", Kind::Utf8, false),
        ("last_event_id", Kind::Utf8, false),
        ("evidence", Kind::ListUtf8, false),
        ("confidence", Kind::Float32, false),
        ("corrected_by", Kind::Utf8, true),
        ("corrected_at", Kind::Ts, true),
        ("inferred_objective", Kind::Utf8, true),
        ("retracted", Kind::Bool, false),
    ]);
    let rows = p
        .turns
        .iter()
        .map(|t| (t, false))
        .chain(p.retracted.turns.iter().map(|t| (t, true)));
    for (t, retracted) in rows {
        let meta = session_meta(p, t.session_id);
        let evidence = turn_evidence(t, &calls);
        let confidence: f32 = if t.stop_event_id.is_some() { 1.0 } else { 0.7 };
        let (corrected_by, corrected_at, _) = correction_cols(&t.corrected);
        b.push(vec![
            readable(&t.turn_id).into(),
            readable(&t.session_id).into(),
            meta.provider.into(),
            meta.project_id.into(),
            meta.project_name.into(),
            t.index.into(),
            t.started_at.into(),
            t.ended_at.into(),
            t.status.as_str().into(),
            readable_opt(&t.prompt_event_id).into(),
            readable_opt(&t.stop_event_id).into(),
            readable_list(&t.tool_call_ids).into(),
            (t.tool_call_ids.len() as u64).into(),
            t.objective.clone().into(),
            t.prompt_chars.into(),
            readable(&t.first_event_id).into(),
            readable(&t.last_event_id).into(),
            readable_list(&evidence).into(),
            confidence.into(),
            corrected_by,
            corrected_at,
            t.inferred_objective.clone().into(),
            retracted.into(),
        ])?;
    }
    b.finish()
}

fn tool_calls_table(p: &Projection) -> Result<RecordBatch> {
    let mut b = TableBuilder::new(&[
        ("tool_call_id", Kind::Utf8, false),
        ("session_id", Kind::Utf8, false),
        ("provider", Kind::Utf8, false),
        ("project_id", Kind::Utf8, false),
        ("project_name", Kind::Utf8, false),
        ("turn_id", Kind::Utf8, true),
        ("agent_id", Kind::Utf8, false),
        ("tool_name", Kind::Utf8, false),
        ("tool_category", Kind::Utf8, false),
        ("provider_call_id", Kind::Utf8, true),
        ("started_at", Kind::Ts, true),
        ("finished_at", Kind::Ts, true),
        ("duration_ms", Kind::Int64, true),
        ("outcome_status", Kind::Utf8, true),
        ("outcome_class", Kind::Utf8, true),
        ("exit_code", Kind::Int32, true),
        ("path_relative", Kind::Utf8, true),
        ("paths", Kind::ListUtf8, false),
        ("command_category", Kind::Utf8, true),
        ("git_subcommand", Kind::Utf8, true),
        ("lines_added", Kind::Int64, true),
        ("lines_removed", Kind::Int64, true),
        ("start_event_id", Kind::Utf8, true),
        ("end_event_id", Kind::Utf8, true),
        ("evidence", Kind::ListUtf8, false),
        ("confidence", Kind::Float32, false),
        ("retracted", Kind::Bool, false),
    ]);
    let rows = p
        .tool_calls
        .iter()
        .map(|c| (c, false))
        .chain(p.retracted.tool_calls.iter().map(|c| (c, true)));
    for (c, retracted) in rows {
        let meta = session_meta(p, c.session_id);
        let mut evidence = Vec::new();
        dedup_push(&mut evidence, c.start_event_id);
        dedup_push(&mut evidence, c.end_event_id);
        let confidence: f32 = if c.start_event_id.is_some() && c.end_event_id.is_some() {
            1.0
        } else {
            0.5
        };
        b.push(vec![
            readable(&c.tool_call_id).into(),
            readable(&c.session_id).into(),
            meta.provider.into(),
            meta.project_id.into(),
            meta.project_name.into(),
            readable_opt(&c.turn_id).into(),
            readable(&c.agent_id).into(),
            c.tool.name.clone().into(),
            c.tool.category.as_str().into(),
            c.tool.call_id.clone().into(),
            c.started_at.into(),
            c.finished_at.into(),
            c.duration_ms.into(),
            c.outcome
                .as_ref()
                .map(|o| o.status.as_str().to_string())
                .into(),
            c.outcome.as_ref().and_then(|o| o.class.clone()).into(),
            c.outcome.as_ref().and_then(|o| o.exit_code).into(),
            c.paths.first().map(path_text).into(),
            c.paths.iter().map(path_text).collect::<Vec<_>>().into(),
            c.command_category.clone().into(),
            c.git_subcommand.clone().into(),
            c.lines_added.into(),
            c.lines_removed.into(),
            readable_opt(&c.start_event_id).into(),
            readable_opt(&c.end_event_id).into(),
            readable_list(&evidence).into(),
            confidence.into(),
            retracted.into(),
        ])?;
    }
    b.finish()
}

fn attempt_row(p: &Projection, a: &Attempt, retracted: bool) -> Vec<Val> {
    let meta = session_meta(p, a.session_id);
    let (corrected_by, corrected_at, correction_type) = correction_cols(&a.corrected);
    vec![
        readable(&a.attempt_id).into(),
        readable(&a.session_id).into(),
        meta.provider.into(),
        meta.project_id.into(),
        meta.project_name.into(),
        readable(&a.turn_id).into(),
        a.turn_index.into(),
        a.index.into(),
        a.objective.clone().into(),
        a.approach.clone().into(),
        a.started_at.into(),
        a.ended_at.into(),
        a.outcome.as_str().into(),
        a.failure_class.clone().into(),
        readable_list(&a.tool_call_ids).into(),
        (a.tool_call_ids.len() as u64).into(),
        a.paths.clone().into(),
        a.commit_shas.clone().into(),
        readable_opt(&a.superseded_by).into(),
        readable_opt(&a.supersedes).into(),
        readable_list(&a.evidence).into(),
        a.confidence.into(),
        a.algorithm_version.as_str().into(),
        readable_opt(&a.work_unit_id).into(),
        corrected_by,
        corrected_at,
        correction_type,
        a.inferred_outcome.map(|o| o.as_str().to_string()).into(),
        a.inferred_failure_class.clone().into(),
        a.note.clone().into(),
        retracted.into(),
    ]
}

fn attempts_table(p: &Projection) -> Result<RecordBatch> {
    let mut b = TableBuilder::new(&[
        ("attempt_id", Kind::Utf8, false),
        ("session_id", Kind::Utf8, false),
        ("provider", Kind::Utf8, false),
        ("project_id", Kind::Utf8, false),
        ("project_name", Kind::Utf8, false),
        ("turn_id", Kind::Utf8, false),
        ("turn_index", Kind::Int64, false),
        ("attempt_index", Kind::Int64, false),
        ("objective", Kind::Utf8, true),
        ("approach", Kind::Utf8, false),
        ("started_at", Kind::Ts, false),
        ("ended_at", Kind::Ts, true),
        ("outcome", Kind::Utf8, false),
        ("failure_class", Kind::Utf8, true),
        ("tool_call_ids", Kind::ListUtf8, false),
        ("tool_call_count", Kind::Int64, false),
        ("paths", Kind::ListUtf8, false),
        ("commit_shas", Kind::ListUtf8, false),
        ("superseded_by", Kind::Utf8, true),
        ("supersedes", Kind::Utf8, true),
        ("evidence", Kind::ListUtf8, false),
        ("confidence", Kind::Float32, false),
        ("algorithm_version", Kind::Utf8, false),
        ("work_unit_id", Kind::Utf8, true),
        ("corrected_by", Kind::Utf8, true),
        ("corrected_at", Kind::Ts, true),
        ("correction_type", Kind::Utf8, true),
        ("inferred_outcome", Kind::Utf8, true),
        ("inferred_failure_class", Kind::Utf8, true),
        ("note", Kind::Utf8, true),
        ("retracted", Kind::Bool, false),
    ]);
    for a in &p.attempts {
        b.push(attempt_row(p, a, false))?;
    }
    for a in &p.retracted.attempts {
        b.push(attempt_row(p, a, true))?;
    }
    b.finish()
}

fn handoffs_table(p: &Projection) -> Result<RecordBatch> {
    let mut b = TableBuilder::new(&[
        ("from_session", Kind::Utf8, false),
        ("to_session", Kind::Utf8, false),
        ("from_provider", Kind::Utf8, false),
        ("to_provider", Kind::Utf8, false),
        ("project_id", Kind::Utf8, false),
        ("handoff_at", Kind::Ts, false),
        ("gap_ms", Kind::Int64, false),
        ("shared_paths", Kind::ListUtf8, false),
        ("evidence", Kind::ListUtf8, false),
        ("confidence", Kind::Float32, false),
    ]);
    for h in &p.handoffs {
        b.push(vec![
            readable(&h.from_session).into(),
            readable(&h.to_session).into(),
            h.from_provider.as_str().into(),
            h.to_provider.as_str().into(),
            readable(&h.project_id).into(),
            h.at.into(),
            h.gap_ms.into(),
            h.shared_paths.clone().into(),
            readable_list(&h.evidence).into(),
            h.confidence.into(),
        ])?;
    }
    b.finish()
}

fn conflicts_table(p: &Projection) -> Result<RecordBatch> {
    let mut b = TableBuilder::new(&[
        ("conflict_id", Kind::Utf8, false),
        ("project_id", Kind::Utf8, false),
        ("first_work_unit", Kind::Utf8, false),
        ("second_work_unit", Kind::Utf8, false),
        ("first_started_at", Kind::Ts, false),
        ("second_started_at", Kind::Ts, false),
        ("started_at", Kind::Ts, false),
        ("updated_at", Kind::Ts, false),
        ("paths", Kind::ListUtf8, false),
        ("path_count", Kind::Int64, false),
        ("overlapping", Kind::Bool, false),
        ("first_committed", Kind::Bool, false),
        ("second_committed", Kind::Bool, false),
        ("first_lines_added", Kind::Int64, false),
        ("first_lines_removed", Kind::Int64, false),
        ("second_lines_added", Kind::Int64, false),
        ("second_lines_removed", Kind::Int64, false),
        ("evidence", Kind::ListUtf8, false),
        ("confidence", Kind::Float32, false),
        ("algorithm_version", Kind::Utf8, false),
    ]);
    for c in &p.conflicts {
        let sum = |f: &dyn Fn(&attemptdb_project::ConflictPath) -> u64| -> i64 {
            c.paths.iter().map(|x| f(x) as i64).sum()
        };
        b.push(vec![
            readable(&c.conflict_id).into(),
            readable(&c.project_id).into(),
            readable(&c.first).into(),
            readable(&c.second).into(),
            c.first_started_at.into(),
            c.second_started_at.into(),
            c.started_at.into(),
            c.updated_at.into(),
            c.paths
                .iter()
                .map(|x| x.path.clone())
                .collect::<Vec<_>>()
                .into(),
            (c.paths.len() as i64).into(),
            c.paths.iter().any(|x| x.overlapping).into(),
            c.paths.iter().all(|x| x.first_committed).into(),
            c.paths.iter().all(|x| x.second_committed).into(),
            sum(&|x| x.first_added).into(),
            sum(&|x| x.first_removed).into(),
            sum(&|x| x.second_added).into(),
            sum(&|x| x.second_removed).into(),
            readable_list(&c.evidence).into(),
            c.confidence.into(),
            c.algorithm_version.clone().into(),
        ])?;
    }
    b.finish()
}

fn edges_table(graph: &Graph) -> Result<RecordBatch> {
    let mut b = TableBuilder::new(&[
        ("ordinal", Kind::Int64, false),
        ("edge_kind", Kind::Utf8, false),
        ("from_type", Kind::Utf8, false),
        ("from_id", Kind::Utf8, false),
        ("to_type", Kind::Utf8, false),
        ("to_id", Kind::Utf8, false),
        ("evidence", Kind::ListUtf8, false),
        ("confidence", Kind::Float32, false),
        ("edge_source", Kind::Utf8, false),
    ]);
    for (i, e) in graph.edges.iter().enumerate() {
        b.push(vec![
            (i as u64).into(),
            e.kind.as_str().into(),
            endpoint_type(&e.from).into(),
            endpoint_id(&e.from).into(),
            endpoint_type(&e.to).into(),
            endpoint_id(&e.to).into(),
            readable_list(&e.evidence).into(),
            e.confidence.into(),
            if e.derived { "derived" } else { "projection" }.into(),
        ])?;
    }
    b.finish()
}

fn signals_table(p: &Projection) -> Result<RecordBatch> {
    let mut b = TableBuilder::new(&[
        ("session_id", Kind::Utf8, false),
        ("event_id", Kind::Utf8, false),
        ("raised_at", Kind::Ts, false),
        ("kind", Kind::Utf8, false),
        ("signal_type", Kind::Utf8, true),
        ("cleared_at", Kind::Ts, true),
        ("cleared_by", Kind::Utf8, true),
        ("pending", Kind::Bool, false),
        ("evidence", Kind::ListUtf8, false),
        ("confidence", Kind::Float32, false),
    ]);
    for g in &p.signals {
        let mut evidence = vec![g.event_id];
        dedup_push(&mut evidence, g.cleared_by);
        b.push(vec![
            readable(&g.session_id).into(),
            readable(&g.event_id).into(),
            g.at.into(),
            g.kind.as_str().into(),
            g.signal_type.clone().into(),
            g.cleared_at.into(),
            readable_opt(&g.cleared_by).into(),
            g.cleared_at.is_none().into(),
            readable_list(&evidence).into(),
            1.0f32.into(),
        ])?;
    }
    b.finish()
}

/// Column specification of the `work_units` table (shared with `STATE`).
pub const WORK_UNIT_COLUMNS: &[(&str, Kind, bool)] = &[
    ("work_unit_id", Kind::Utf8, false),
    ("version", Kind::Int64, false),
    ("project_id", Kind::Utf8, false),
    ("project_name", Kind::Utf8, false),
    ("objective_event_id", Kind::Utf8, true),
    ("objective", Kind::Utf8, true),
    ("phase", Kind::Utf8, false),
    ("phase_reason", Kind::Utf8, false),
    ("status", Kind::Utf8, false),
    ("status_reason", Kind::Utf8, false),
    ("started_at", Kind::Ts, false),
    ("updated_at", Kind::Ts, false),
    ("ended_at", Kind::Ts, true),
    ("sessions", Kind::ListUtf8, false),
    ("session_count", Kind::Int64, false),
    ("turns", Kind::ListUtf8, false),
    ("turn_count", Kind::Int64, false),
    ("attempts", Kind::ListUtf8, false),
    ("attempt_count", Kind::Int64, false),
    ("failed_attempt_count", Kind::Int64, false),
    ("paths", Kind::ListUtf8, false),
    ("commit_shas", Kind::ListUtf8, false),
    ("actors", Kind::ListUtf8, false),
    ("last_attempt", Kind::Utf8, true),
    ("blocking_signal", Kind::Utf8, true),
    ("evidence", Kind::ListUtf8, false),
    ("confidence", Kind::Float32, false),
    ("algorithm_version", Kind::Utf8, false),
];

pub fn work_unit_row(u: &attemptdb_project::WorkUnit) -> Vec<Val> {
    vec![
        readable(&u.work_unit_id).into(),
        u.version.into(),
        readable(&u.project_id).into(),
        u.project_name.clone().into(),
        readable_opt(&u.objective_event_id).into(),
        u.objective.clone().into(),
        u.phase.as_str().into(),
        u.phase_reason.clone().into(),
        u.status.as_str().into(),
        u.status_reason.clone().into(),
        u.started_at.into(),
        u.updated_at.into(),
        u.ended_at.into(),
        readable_list(&u.sessions).into(),
        (u.sessions.len() as u64).into(),
        readable_list(&u.turns).into(),
        (u.turns.len() as u64).into(),
        readable_list(&u.attempts).into(),
        (u.attempts.len() as u64).into(),
        u.failure_count.into(),
        u.paths.clone().into(),
        u.commit_shas.clone().into(),
        u.actors
            .iter()
            .map(|a| a.as_str().to_string())
            .collect::<Vec<_>>()
            .into(),
        readable_opt(&u.last_attempt).into(),
        readable_opt(&u.blocking_signal).into(),
        readable_list(&u.evidence).into(),
        u.confidence.into(),
        u.algorithm_version.as_str().into(),
    ]
}

fn work_units_table(p: &Projection) -> Result<RecordBatch> {
    let mut b = TableBuilder::new(WORK_UNIT_COLUMNS);
    for u in &p.work_units {
        b.push(work_unit_row(u))?;
    }
    b.finish()
}

/// The `commits` table: every `git commit` call tied to the sha `HEAD`
/// moved to (RFC 0001 artifact linkage; `sha` is null when unresolved).
fn commits_table(p: &Projection) -> Result<RecordBatch> {
    let mut b = TableBuilder::new(&[
        ("commit_id", Kind::Utf8, false),
        ("session_id", Kind::Utf8, false),
        ("provider", Kind::Utf8, false),
        ("project_id", Kind::Utf8, false),
        ("project_name", Kind::Utf8, false),
        ("turn_id", Kind::Utf8, true),
        ("attempt_id", Kind::Utf8, true),
        ("tool_call_id", Kind::Utf8, false),
        ("sha", Kind::Utf8, true),
        ("previous_sha", Kind::Utf8, true),
        ("branch", Kind::Utf8, true),
        ("committed_at", Kind::Ts, false),
        ("linkage", Kind::Utf8, false),
        ("evidence", Kind::ListUtf8, false),
        ("confidence", Kind::Float32, false),
        ("algorithm_version", Kind::Utf8, false),
    ]);
    for c in &p.commits {
        let meta = session_meta(p, c.session_id);
        b.push(vec![
            readable(&c.commit_id).into(),
            readable(&c.session_id).into(),
            meta.provider.into(),
            meta.project_id.into(),
            meta.project_name.into(),
            readable_opt(&c.turn_id).into(),
            readable_opt(&c.attempt_id).into(),
            readable(&c.tool_call_id).into(),
            c.sha.clone().into(),
            c.previous_sha.clone().into(),
            c.branch.clone().into(),
            c.at.into(),
            c.linkage.clone().into(),
            readable_list(&c.evidence).into(),
            c.confidence.into(),
            c.algorithm_version.as_str().into(),
        ])?;
    }
    b.finish()
}

fn decisions_table(p: &Projection) -> Result<RecordBatch> {
    let mut b = TableBuilder::new(&[
        ("decision_id", Kind::Utf8, false),
        ("kind", Kind::Utf8, false),
        ("work_unit_id", Kind::Utf8, true),
        ("session_id", Kind::Utf8, false),
        ("provider", Kind::Utf8, false),
        ("project_id", Kind::Utf8, false),
        ("project_name", Kind::Utf8, false),
        ("turn_id", Kind::Utf8, false),
        ("selected", Kind::Utf8, false),
        ("alternatives", Kind::ListUtf8, false),
        ("rationale", Kind::Utf8, false),
        ("rationale_source", Kind::Utf8, false),
        ("decided_at", Kind::Ts, false),
        ("evidence", Kind::ListUtf8, false),
        ("confidence", Kind::Float32, false),
        ("algorithm_version", Kind::Utf8, false),
    ]);
    for d in &p.decisions {
        let meta = session_meta(p, d.session_id);
        b.push(vec![
            readable(&d.decision_id).into(),
            d.kind.as_str().into(),
            readable_opt(&d.work_unit_id).into(),
            readable(&d.session_id).into(),
            meta.provider.into(),
            meta.project_id.into(),
            meta.project_name.into(),
            readable(&d.turn_id).into(),
            readable(&d.selected).into(),
            readable_list(&d.alternatives).into(),
            d.rationale.clone().into(),
            d.rationale_source.clone().into(),
            d.decided_at.into(),
            readable_list(&d.evidence).into(),
            d.confidence.into(),
            d.algorithm_version.as_str().into(),
        ])?;
    }
    b.finish()
}

fn correction_target_text(c: &attemptdb_project::Correction) -> (Option<String>, String) {
    match c.target {
        Some(CorrectionTarget::Attempt(id)) => (Some("attempt".into()), readable(&id)),
        Some(CorrectionTarget::Turn(id)) => (Some("turn".into()), readable(&id)),
        Some(CorrectionTarget::Session(id)) => (Some("session".into()), readable(&id)),
        None => (None, c.target_text.clone()),
    }
}

fn corrections_table(p: &Projection) -> Result<RecordBatch> {
    let mut b = TableBuilder::new(&[
        ("event_id", Kind::Utf8, false),
        ("corrected_at", Kind::Ts, false),
        ("session_id", Kind::Utf8, false),
        ("project_id", Kind::Utf8, false),
        ("correction_type", Kind::Utf8, true),
        ("target_type", Kind::Utf8, true),
        ("target", Kind::Utf8, false),
        ("outcome", Kind::Utf8, true),
        ("failure_class", Kind::Utf8, true),
        ("note", Kind::Utf8, true),
        ("note_chars", Kind::Int64, true),
        ("status", Kind::Utf8, false),
        ("evidence", Kind::ListUtf8, false),
        ("confidence", Kind::Float32, false),
    ]);
    for c in &p.corrections {
        let (target_type, target) = correction_target_text(c);
        b.push(vec![
            readable(&c.event_id).into(),
            c.at.into(),
            readable(&c.session_id).into(),
            readable(&c.project_id).into(),
            c.correction_type.map(|t| t.as_str().to_string()).into(),
            target_type.into(),
            target.into(),
            c.outcome.map(|o| o.as_str().to_string()).into(),
            c.failure_class.clone().into(),
            c.note.clone().into(),
            c.note_chars.into(),
            c.status.as_str().into(),
            readable_list(&[c.event_id]).into(),
            1.0f32.into(),
        ])?;
    }
    b.finish()
}

fn retractions_table(p: &Projection) -> Result<RecordBatch> {
    let mut b = TableBuilder::new(&[
        ("event_id", Kind::Utf8, false),
        ("retracted_at", Kind::Ts, false),
        ("project_id", Kind::Utf8, false),
        ("target_type", Kind::Utf8, true),
        ("target", Kind::Utf8, false),
        ("reason", Kind::Utf8, false),
        ("note", Kind::Utf8, true),
        ("note_chars", Kind::Int64, true),
        ("matched", Kind::Bool, false),
        ("retracted_events", Kind::Int64, false),
        ("evidence", Kind::ListUtf8, false),
        ("confidence", Kind::Float32, false),
    ]);
    for r in &p.retractions {
        let target = match r.target {
            Some(RetractionTarget::Session(id)) => readable(&id),
            Some(RetractionTarget::Event(id)) => readable(&id),
            Some(RetractionTarget::Attempt(id)) => readable(&id),
            None => r.target_text.clone(),
        };
        b.push(vec![
            readable(&r.event_id).into(),
            r.at.into(),
            readable(&r.project_id).into(),
            r.target_type.map(|t| t.as_str().to_string()).into(),
            target.into(),
            r.reason.as_str().into(),
            r.note.clone().into(),
            r.note_chars.into(),
            r.matched.into(),
            r.retracted_events.into(),
            readable_list(&[r.event_id]).into(),
            1.0f32.into(),
        ])?;
    }
    b.finish()
}

/// Whether a tool call outcome counts as a failure for causal purposes.
pub fn is_failed_status(status: OutcomeStatus) -> bool {
    matches!(status, OutcomeStatus::Failure | OutcomeStatus::Denied)
}

// ---------------------------------------------------------------------------
// Readable events view
// ---------------------------------------------------------------------------

/// Name of the Boolean column appended to the `events` view.
pub const RETRACTED_COLUMN: &str = "retracted";

fn readable_field(f: &Field) -> Field {
    let dt = match f.data_type() {
        DataType::FixedSizeBinary(16) => DataType::Utf8,
        DataType::Dictionary(_, value) => value.as_ref().clone(),
        other => other.clone(),
    };
    Field::new(f.name(), dt, f.is_nullable())
}

/// The `events` schema with binary ids as prefixed text, dictionary
/// columns decoded to plain text, and a trailing `retracted` flag.
pub fn readable_events_schema() -> SchemaRef {
    readable_schema(events_schema().as_ref())
}

fn readable_schema(s: &Schema) -> SchemaRef {
    let mut fields: Vec<Field> = s.fields().iter().map(|f| readable_field(f)).collect();
    fields.push(Field::new(RETRACTED_COLUMN, DataType::Boolean, false));
    Arc::new(Schema::new(fields))
}

/// [`readable_schema`] without the `retracted` column.
fn readable_schema_base(s: &Schema) -> SchemaRef {
    let fields: Vec<Field> = s.fields().iter().map(|f| readable_field(f)).collect();
    Arc::new(Schema::new(fields))
}

fn fsb_to_readable(col: &ArrayRef, prefix: &str) -> ArrayRef {
    let a = col.as_fixed_size_binary();
    let mut b = StringBuilder::with_capacity(a.len(), a.len() * (prefix.len() + 36));
    for i in 0..a.len() {
        if a.is_null(i) {
            b.append_null();
        } else {
            let mut bytes = [0u8; 16];
            let v = a.value(i);
            if v.len() == 16 {
                bytes.copy_from_slice(v);
                b.append_value(format!("{prefix}{}", hyphenated(bytes)));
            } else {
                b.append_null();
            }
        }
    }
    Arc::new(b.finish())
}

fn fsb_uuid(col: Option<&ArrayRef>, row: usize) -> Option<[u8; 16]> {
    let a = col?.as_fixed_size_binary_opt()?;
    if a.is_null(row) {
        return None;
    }
    let v = a.value(row);
    (v.len() == 16).then(|| {
        let mut bytes = [0u8; 16];
        bytes.copy_from_slice(v);
        bytes
    })
}

/// The `retracted` flag per row of a storage batch.
fn retracted_column(batch: &RecordBatch, retracted: &RetractedSet) -> Result<ArrayRef> {
    let n = batch.num_rows();
    if retracted.is_empty() {
        return Ok(Arc::new(BooleanArray::from(vec![false; n])));
    }
    let event_col = batch.column_by_name(col::EVENT_ID);
    let session_col = batch.column_by_name(col::SESSION_ID);
    let kind_col = match batch.column_by_name(col::KIND) {
        Some(c) => Some(cast(c, &DataType::Utf8)?),
        None => None,
    };
    let mut b = BooleanBuilder::with_capacity(n);
    for row in 0..n {
        let kind = kind_col
            .as_ref()
            .and_then(|c| c.as_string_opt::<i32>())
            .filter(|c| !c.is_null(row))
            .map(|c| c.value(row))
            .and_then(EventKind::parse)
            .unwrap_or(EventKind::Unknown);
        let flag = !is_meta_kind(kind)
            && (fsb_uuid(event_col, row)
                .is_some_and(|b| retracted.contains_event(&EventId::from_bytes(b)))
                || fsb_uuid(session_col, row)
                    .is_some_and(|b| retracted.contains_session(&SessionId::from_bytes(b))));
        b.append_value(flag);
    }
    Ok(Arc::new(b.finish()))
}

/// The readable form of one storage batch without the `retracted` flag:
/// ids as prefixed text, dictionaries decoded. A pure function of the
/// batch, so a segment's result can be cached across engines.
pub fn readable_columns(batch: &RecordBatch) -> Result<RecordBatch> {
    let schema = readable_schema_base(batch.schema().as_ref());
    let mut columns = Vec::with_capacity(batch.num_columns());
    for (i, col) in batch.columns().iter().enumerate() {
        let name = batch.schema().field(i).name().clone();
        let arr: ArrayRef = match col.data_type() {
            DataType::FixedSizeBinary(16) => fsb_to_readable(col, prefix_for_column(&name)),
            DataType::Dictionary(_, value) => cast(col, value)?,
            _ => Arc::clone(col),
        };
        columns.push(arr);
    }
    Ok(RecordBatch::try_new(schema, columns)?)
}

/// A readable batch with `content_json`/`raw_json` taken from `storage`
/// (a storage batch whose blob refs were resolved).
pub fn replace_content_columns(
    readable: &RecordBatch,
    storage: &RecordBatch,
) -> Result<RecordBatch> {
    let mut columns = readable.columns().to_vec();
    for name in [col::CONTENT_JSON, col::RAW_JSON] {
        if let (Ok(i), Some(src)) = (
            readable.schema().index_of(name),
            storage.column_by_name(name),
        ) {
            columns[i] = Arc::clone(src);
        }
    }
    Ok(RecordBatch::try_new(readable.schema(), columns)?)
}

/// Append the `retracted` flag to a batch from [`readable_columns`].
/// `storage` is the batch it was derived from (the flag reads its raw id
/// columns).
pub fn with_retracted(
    readable: &RecordBatch,
    storage: &RecordBatch,
    retracted: &RetractedSet,
) -> Result<RecordBatch> {
    let schema = readable_schema(storage.schema().as_ref());
    let mut columns = readable.columns().to_vec();
    columns.push(retracted_column(storage, retracted)?);
    Ok(RecordBatch::try_new(schema, columns)?)
}

/// Compact type name for `tables()` listings.
pub fn type_name(dt: &DataType) -> String {
    match dt {
        DataType::Utf8 | DataType::LargeUtf8 | DataType::Utf8View => "text".into(),
        DataType::Int64 => "int64".into(),
        DataType::Int32 => "int32".into(),
        DataType::UInt64 => "uint64".into(),
        DataType::UInt16 => "uint16".into(),
        DataType::Float32 => "float32".into(),
        DataType::Float64 => "float64".into(),
        DataType::Boolean => "bool".into(),
        DataType::Timestamp(_, _) => "timestamp".into(),
        DataType::FixedSizeBinary(16) => "uuid".into(),
        DataType::Dictionary(_, v) => format!("dict<{}>", type_name(v)),
        DataType::List(f) => format!("list<{}>", type_name(f.data_type())),
        other => other.to_string(),
    }
}
