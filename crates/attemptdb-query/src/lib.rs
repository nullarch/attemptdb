//! Query layer: DataFusion-backed SQL and AttemptQL (RFC 0004).
//!
//! [`QueryEngine`] loads an event stream (from a [`Database`] or a plain
//! `Vec<Event>`), projects it with `attemptdb-project`, and registers
//! everything as tables in a DataFusion [`SessionContext`]:
//!
//! | table | grain |
//! |---|---|
//! | `events` | one canonical event, readable ids (`ev_…`, `ses_…`), decoded dictionaries, and a `retracted` flag |
//! | `events_raw` | the same rows in the exact storage schema (`FixedSizeBinary(16)` ids, dictionary columns) |
//! | `sessions`, `turns`, `tool_calls`, `attempts`, `handoffs`, `edges`, `signals` | Tier 1 projection entities (the first four also hold retracted rows, flagged `retracted`) |
//! | `work_units`, `decisions` | Tier 1 work units and derived decisions |
//! | `corrections`, `retractions` | the human-written correction / retraction events and how they applied |
//!
//! Plain SQL runs over all of them; AttemptQL statements compile to SQL over
//! the same tables or evaluate the projection directly (`WHY`, `TRACE`,
//! `STATE`, `DIFF`). `SHOW` hides retracted rows unless `INCLUDING
//! RETRACTED` is given. Every `WHY` / `TRACE` / `STATE` result carries an
//! `evidence` column with event ids plus a confidence and an uncertainty
//! note — never prose alone.

#![forbid(unsafe_code)]

pub mod attemptql;
mod cache;
mod error;
mod exec;
pub mod facts;
mod graph;
mod ids;
mod lazy;
mod parts;
mod result;
mod tables;
mod timeexpr;

pub use cache::{CacheStats, EngineCache};
pub use error::{QueryError, Result};
pub use facts::{DeviceFacts, LastEvent, ProjectFacts, ProviderFacts, SessionFacts, StreamFacts};
pub use graph::Direction;
pub use ids::PrefixedId;
pub use result::{QueryResult, ResultKind};
pub use timeexpr::TimeExpr;

use attemptdb_core::{Event, EventId, SessionId};
use attemptdb_project::{Projection, project};
use attemptdb_storage::segment::{events_schema, events_to_batches};
use attemptdb_storage::{ContentResolver, Database, ScanFilter};
use datafusion::arrow::array::RecordBatch;
use datafusion::arrow::datatypes::SchemaRef;
use datafusion::datasource::TableProvider;
use datafusion::prelude::{SQLOptions, SessionConfig, SessionContext};
use graph::Graph;
use std::sync::{Arc, OnceLock};

/// A registered table, for `attempt tables` style listings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TableInfo {
    pub name: String,
    /// `(column, type)` pairs in schema order.
    pub columns: Vec<(String, String)>,
    pub rows: usize,
}

/// Registration order of the tables.
pub const TABLE_NAMES: &[&str] = &[
    "events",
    "events_raw",
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
];

/// SQL + AttemptQL over one loaded event stream.
///
/// The projection is built eagerly; everything SQL needs — the DataFusion
/// context, the `events` table with readable ids, the twelve projection
/// tables — is built on the first statement that runs over it, and the
/// causal graph on the first `WHY`/`TRACE`. Most readers of an engine (the
/// server's JSON endpoints, the UI's pages, the MCP tools) only read the
/// projection; measured at 200 k events, the tables were 71 % of a view
/// rebuild that those readers never used.
pub struct QueryEngine {
    /// The stream in manifest order — one part per segment (shared with
    /// the [`EngineCache`] that derived it) and, last, the WAL.
    parts: Vec<Arc<parts::SegmentParts>>,
    /// Resolves encrypted content for the `events` tables' content columns
    /// when a statement asks for them; `None` when content is inline.
    resolver: Option<ContentResolver>,
    sql: OnceLock<std::result::Result<SqlLayer, String>>,
    projection: Arc<Projection>,
    /// The causal graph, built on first use and shared with the lazy
    /// `edges` table.
    graph: Arc<OnceLock<Arc<Graph>>>,
    event_count: usize,
    /// Every loaded event id, in stream order (short-id resolution);
    /// concatenated from the parts on first use.
    event_ids: OnceLock<Vec<EventId>>,
    /// The parts' facts merged in stream order, on first use.
    facts: OnceLock<StreamFacts>,
}

/// The DataFusion side of an engine: built once, on first use.
struct SqlLayer {
    ctx: SessionContext,
    tables: Vec<TableInfo>,
}

/// Queries only: no DDL, no DML, no `SET`/`COPY`/transactions.
fn read_only_sql() -> SQLOptions {
    SQLOptions::new()
        .with_allow_ddl(false)
        .with_allow_dml(false)
        .with_allow_statements(false)
}

impl QueryEngine {
    /// Load from a database: one refresh of a throwaway [`EngineCache`],
    /// then the engine over `filter`'s scope. No event is decoded with its
    /// content unless the projector needs it, and no blob is opened for
    /// the `events` table until a statement projects a content column.
    pub async fn from_database(db: &Database, filter: &ScanFilter) -> Result<Self> {
        let mut cache = EngineCache::new();
        let refreshed = cache.refresh(db, &db.root().display().to_string())?;
        cache.engine_scoped(&refreshed, filter)
    }

    /// Load from an in-memory event stream.
    pub async fn from_events(events: Vec<Event>) -> Result<Self> {
        let raw = events_to_batches(&events)?;
        Self::build(raw, events).await
    }

    async fn build(raw: Vec<RecordBatch>, events: Vec<Event>) -> Result<Self> {
        let projection = project(&events);
        Self::from_parts(raw, projection, events.iter()).await
    }

    /// Build from parts a caller already holds: Arrow batches (typically
    /// from a `ScanCache`), a projection (typically from an
    /// `IncrementalProjector`), and the events for id resolution. This is
    /// the refresh path: nothing here decodes a segment or re-projects.
    pub async fn from_parts<'a>(
        raw: Vec<RecordBatch>,
        projection: Projection,
        events: impl IntoIterator<Item = &'a Event>,
    ) -> Result<Self> {
        let part = parts::SegmentParts::from_batches_and_events(raw, events);
        Ok(Self::over(vec![Arc::new(part)], projection, None))
    }

    /// Build over already-derived parts (the [`EngineCache`] path).
    pub(crate) fn over(
        parts: Vec<Arc<parts::SegmentParts>>,
        projection: Projection,
        resolver: Option<ContentResolver>,
    ) -> Self {
        let event_count = parts.iter().map(|p| p.ids.event_ids.len()).sum();
        Self {
            parts,
            resolver,
            sql: OnceLock::new(),
            projection: Arc::new(projection),
            graph: Arc::new(OnceLock::new()),
            event_count,
            event_ids: OnceLock::new(),
            facts: OnceLock::new(),
        }
    }

    /// Projects, providers, sessions and devices as the loaded events
    /// describe them (merged from the parts, once).
    pub fn facts(&self) -> &StreamFacts {
        self.facts.get_or_init(|| {
            let mut merged = StreamFacts::default();
            for p in &self.parts {
                merged.absorb(&p.facts);
            }
            merged
        })
    }

    /// The DataFusion context and table listing, built on first use. A
    /// build failure is remembered and returned again rather than retried.
    fn sql_layer(&self) -> Result<&SqlLayer> {
        self.sql
            .get_or_init(|| {
                build_sql_layer(
                    &self.parts,
                    Arc::clone(&self.projection),
                    Arc::clone(&self.graph),
                    self.resolver.clone(),
                )
                .map_err(|e| e.to_string())
            })
            .as_ref()
            .map_err(|m| QueryError::Exec(m.clone()))
    }

    pub fn projection(&self) -> &Projection {
        &self.projection
    }

    pub fn event_count(&self) -> usize {
        self.event_count
    }

    /// The DataFusion context, for callers that want to register more.
    /// Builds the SQL layer if no statement has run yet.
    pub fn session_context(&self) -> Result<&SessionContext> {
        self.sql_layer().map(|l| &l.ctx)
    }

    /// Registered tables with their columns and row counts. Builds the SQL
    /// layer if no statement has run yet.
    pub fn tables(&self) -> Result<Vec<TableInfo>> {
        self.sql_layer().map(|l| l.tables.clone())
    }

    /// Run plain SQL over all tables.
    ///
    /// Read-only at the engine layer: DDL, DML and statements are refused by
    /// DataFusion itself, not by a keyword check in a caller. Without this,
    /// `CREATE EXTERNAL TABLE t STORED AS CSV LOCATION '/etc/hosts'` reads
    /// any file the process can, and `COPY … TO` writes one. The UI and MCP
    /// keep their own prefix checks for a friendlier message, but this is the
    /// guarantee.
    pub async fn sql(&self, sql: &str) -> Result<QueryResult> {
        let ctx = self.session_context()?;
        let df = ctx.sql_with_options(sql, read_only_sql()).await?;
        let schema: SchemaRef = Arc::clone(df.schema().inner());
        let batches = df.collect().await?;
        let rows: usize = batches.iter().map(RecordBatch::num_rows).sum();
        let is_explain = sql
            .trim_start()
            .get(..7)
            .is_some_and(|s| s.eq_ignore_ascii_case("EXPLAIN"));
        let kind = if is_explain {
            ResultKind::Explanation
        } else if rows == 0 {
            ResultKind::Empty
        } else {
            ResultKind::Rows
        };
        Ok(QueryResult::new(schema, batches, kind, Vec::new()))
    }

    /// Run an AttemptQL statement, or plain SQL when the text starts with
    /// `SELECT` / `WITH` / `EXPLAIN <sql>` (see [`attemptql::is_sql`]).
    pub async fn query(&self, text: &str) -> Result<QueryResult> {
        if attemptql::is_sql(text) {
            return self.sql(text).await;
        }
        let stmt = attemptql::parse(text)?;
        self.execute(stmt).await
    }

    /// DataFusion's logical and physical plan for a SQL query.
    pub async fn explain(&self, sql: &str) -> Result<QueryResult> {
        let df = self
            .session_context()?
            .sql_with_options(sql, read_only_sql())
            .await?
            .explain(false, false)?;
        let schema: SchemaRef = Arc::clone(df.schema().inner());
        let batches = df.collect().await?;
        Ok(QueryResult::new(
            schema,
            batches,
            ResultKind::Explanation,
            Vec::new(),
        ))
    }

    /// The causal graph, built from the projection on first use.
    pub(crate) fn graph(&self) -> &Graph {
        self.graph
            .get_or_init(|| Arc::new(Graph::build(&self.projection)))
    }

    pub(crate) fn event_ids(&self) -> &[EventId] {
        self.event_ids.get_or_init(|| {
            let mut all = Vec::with_capacity(self.event_count);
            for p in &self.parts {
                all.extend_from_slice(&p.ids.event_ids);
            }
            all
        })
    }

    pub(crate) fn has_event(&self, id: &EventId) -> bool {
        self.parts.iter().any(|p| p.ids.set.contains(id))
    }

    /// A session's event ids in stream order (for callers outside the
    /// crate that need an evidence handle into the session).
    pub fn session_event_ids_public(&self, sid: SessionId) -> Vec<EventId> {
        self.session_event_ids(sid)
    }

    /// A session's event ids in stream order.
    pub(crate) fn session_event_ids(&self, sid: SessionId) -> Vec<EventId> {
        let mut out = Vec::new();
        for p in &self.parts {
            if let Some(ids) = p.ids.session_events.get(&sid) {
                out.extend_from_slice(ids);
            }
        }
        out
    }
}

/// Register every table: `events` (readable ids, `retracted` flag),
/// `events_raw` (the storage schema as is), then the projection tables.
fn build_sql_layer(
    parts: &[Arc<parts::SegmentParts>],
    projection: Arc<Projection>,
    graph: Arc<OnceLock<Arc<Graph>>>,
    resolver: Option<ContentResolver>,
) -> Result<SqlLayer> {
    let config = SessionConfig::new()
        .with_information_schema(true)
        .with_target_partitions(1);
    let ctx = SessionContext::new_with_config(config);
    let mut tables = Vec::new();

    // The two events tables are lazy: a segment's readable columns are
    // derived once and shared, the `retracted` flag is added per engine,
    // and content is resolved only for a statement that projects it.
    let retracted = Arc::new(projection.retracted_ids.clone());
    let raw_schema = parts
        .iter()
        .flat_map(|p| p.batches.first())
        .map(|b| b.schema())
        .next()
        .unwrap_or_else(events_schema);
    for (name, schema, readable) in [
        ("events", tables::readable_events_schema(), true),
        ("events_raw", raw_schema, false),
    ] {
        let table = lazy::EventsTable::new(
            Arc::clone(&schema),
            parts.to_vec(),
            readable,
            Arc::clone(&retracted),
            resolver.clone(),
        );
        let rows = table.row_count();
        ctx.register_table(name, Arc::new(table))?;
        tables.push(table_info(name, &schema, rows));
    }
    // Every projection table is built on the first statement that scans
    // it; registering costs a schema and a row count.
    for name in tables::PROJECTION_TABLES {
        let table =
            lazy::LazyProjectionTable::new(name, Arc::clone(&projection), Arc::clone(&graph));
        let schema = TableProvider::schema(&table);
        let rows = table.row_count();
        ctx.register_table(*name, Arc::new(table))?;
        tables.push(table_info(name, &schema, rows));
    }
    Ok(SqlLayer { ctx, tables })
}

fn table_info(name: &str, schema: &SchemaRef, rows: usize) -> TableInfo {
    TableInfo {
        name: name.to_string(),
        columns: schema
            .fields()
            .iter()
            .map(|f| (f.name().clone(), tables::type_name(f.data_type())))
            .collect(),
        rows,
    }
}

/// Caret-style rendering of a parse error against the statement text.
///
/// ```text
/// error: unexpected token 'FOO' at position 5; expected ATTEMPTS, ...
///   |
///   | SHOW FOO
///   |      ^
/// ```
///
/// Other errors render as their `Display` form.
pub fn format_parse_error(text: &str, err: &QueryError) -> String {
    let QueryError::Parse { message, position } = err else {
        return format!("error: {err}");
    };
    let pos = (*position).min(text.len());
    let line_start = text[..pos].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let line_end = text[pos..]
        .find('\n')
        .map(|i| pos + i)
        .unwrap_or(text.len());
    let line = &text[line_start..line_end];
    let column = text[line_start..pos].chars().count();
    let mut out = format!("error: {message} at position {position}\n  |\n  | {line}\n  | ");
    out.push_str(&" ".repeat(column));
    out.push('^');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caret_points_at_position() {
        let err = QueryError::Parse {
            message: "unexpected token 'FOO'".into(),
            position: 5,
        };
        let s = format_parse_error("SHOW FOO", &err);
        assert!(s.contains("  | SHOW FOO\n  |      ^"), "{s}");
        let other = QueryError::Plan("nope".into());
        assert_eq!(format_parse_error("x", &other), "error: plan error: nope");
    }
}
