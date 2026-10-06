//! The `events` and `events_raw` tables, with content resolved on demand.
//!
//! A format 2 segment keeps `content`/`raw` in one encrypted blob file per
//! row and stores only the blob ids in the batch. Filling `content_json`
//! and `raw_json` means opening every one of those files — 15,000 of them
//! for an 8,800-event database — which is exactly what a `count(*)`, a
//! timeline or a `GROUP BY kind` never needs. [`EventsTable`] is a
//! `TableProvider` over the cached batches that resolves the two content
//! columns only when a statement projects them.
//!
//! A statement that names its columns costs those columns: only the
//! projected ones are converted to their readable form (a `count(*)` converts
//! none, a `GROUP BY kind` one), and content is resolved for as many rows as
//! the statement can use (a `LIMIT 3` resolves three rows' blobs, not the
//! whole scope's). Only `SELECT *` builds, and keeps, the whole table.

use crate::parts::SegmentParts;
use crate::tables;
use async_trait::async_trait;
use attemptdb_project::RetractedSet;
use attemptdb_storage::ContentResolver;
use attemptdb_storage::segment::col;
use datafusion::arrow::array::RecordBatch;
use datafusion::arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use datafusion::catalog::Session;
use datafusion::datasource::{MemTable, TableProvider, TableType};
use datafusion::error::{DataFusionError, Result as DfResult};
use datafusion::logical_expr::Expr;
use datafusion::physical_plan::ExecutionPlan;
use std::sync::{Arc, OnceLock};

/// `events` (readable ids, `retracted` flag) or `events_raw` (the storage
/// schema as is) over the engine's parts.
#[derive(Debug)]
pub(crate) struct EventsTable {
    schema: SchemaRef,
    parts: Vec<Arc<SegmentParts>>,
    readable: bool,
    retracted: Arc<RetractedSet>,
    resolver: Option<ContentResolver>,
    /// The batches without content resolved, built on first scan.
    plain: OnceLock<std::result::Result<Vec<RecordBatch>, String>>,
    /// The batches with `content_json`/`raw_json` filled, built on the
    /// first scan that asks for either.
    with_content: OnceLock<std::result::Result<Vec<RecordBatch>, String>>,
}

impl EventsTable {
    pub fn new(
        schema: SchemaRef,
        parts: Vec<Arc<SegmentParts>>,
        readable: bool,
        retracted: Arc<RetractedSet>,
        resolver: Option<ContentResolver>,
    ) -> Self {
        Self {
            schema,
            parts,
            readable,
            retracted,
            resolver,
            plain: OnceLock::new(),
            with_content: OnceLock::new(),
        }
    }

    /// The whole table (every column), or its first `limit` rows' worth of
    /// batches.
    fn build(&self, resolve: bool, limit: Option<usize>) -> crate::Result<Vec<RecordBatch>> {
        let mut out = Vec::new();
        let mut rows = 0;
        'parts: for part in &self.parts {
            let readable = if self.readable {
                Some(part.readable()?)
            } else {
                None
            };
            for (i, storage) in part.batches.iter().enumerate() {
                if limit.is_some_and(|l| rows >= l) {
                    break 'parts;
                }
                rows += storage.num_rows();
                let storage = match (&self.resolver, resolve) {
                    (Some(r), true) if r.has_keys() => {
                        std::borrow::Cow::Owned(r.resolve_batch(storage)?)
                    }
                    _ => std::borrow::Cow::Borrowed(storage),
                };
                out.push(match readable {
                    Some(r) => {
                        let mut readable =
                            tables::with_retracted(&r[i], &storage, &self.retracted)?;
                        if resolve {
                            readable = tables::replace_content_columns(&readable, &storage)?;
                        }
                        readable
                    }
                    None => storage.into_owned(),
                });
            }
        }
        Ok(out)
    }

    /// The whole table, built once and kept when nothing narrower will do.
    fn batches(&self, resolve: bool, limit: Option<usize>) -> DfResult<Vec<RecordBatch>> {
        let cell = if resolve {
            &self.with_content
        } else {
            &self.plain
        };
        if let Some(done) = cell.get() {
            return done.clone().map_err(DataFusionError::Execution);
        }
        if limit.is_some() {
            // A `LIMIT` the statement can stop at: build what it will read and
            // leave the cache empty (a later statement may want it all).
            return self
                .build(resolve, limit)
                .map_err(|e| DataFusionError::Execution(e.to_string()));
        }
        cell.get_or_init(|| self.build(resolve, None).map_err(|e| e.to_string()))
            .clone()
            .map_err(DataFusionError::Execution)
    }

    /// Only the columns a statement projects, in its order: each converted
    /// to its readable form from the storage column alone, content resolved
    /// only when asked for and only for the rows `limit` allows. Nothing is
    /// kept.
    fn projected(
        &self,
        cols: &[usize],
        limit: Option<usize>,
    ) -> crate::Result<(SchemaRef, Vec<RecordBatch>)> {
        // The table's own schema narrowed to the projection, metadata kept
        // (DataFusion compares it with what the plan expects).
        let schema: SchemaRef = Arc::new(self.schema.project(cols)?);
        let fields: Vec<_> = schema.fields().iter().map(|f| f.as_ref().clone()).collect();
        let names: Vec<&str> = fields.iter().map(|f| f.name().as_str()).collect();
        let want_retracted = self.readable && names.contains(&tables::RETRACTED_COLUMN);
        let want_content = names
            .iter()
            .any(|n| *n == col::CONTENT_JSON || *n == col::RAW_JSON);
        // Storage columns to convert, in the statement's order, and the three
        // the `retracted` flag is read from.
        let storage_names: Vec<&str> = names
            .iter()
            .copied()
            .filter(|n| *n != tables::RETRACTED_COLUMN)
            .collect();
        let flag_names = [col::EVENT_ID, col::SESSION_ID, col::KIND];
        let mut out = Vec::new();
        let mut rows = 0;
        'parts: for part in &self.parts {
            for storage in &part.batches {
                if limit.is_some_and(|l| rows >= l) {
                    break 'parts;
                }
                let n = storage.num_rows();
                rows += n;
                let storage = match &self.resolver {
                    Some(r) if want_content && r.has_keys() => {
                        std::borrow::Cow::Owned(r.resolve_batch(storage)?)
                    }
                    _ => std::borrow::Cow::Borrowed(storage),
                };
                let project = |wanted: &[&str]| -> crate::Result<RecordBatch> {
                    let idx: Vec<usize> = wanted
                        .iter()
                        .filter_map(|n| storage.schema().index_of(n).ok())
                        .collect();
                    Ok(storage.project(&idx)?)
                };
                let converted = if storage_names.is_empty() {
                    None
                } else if self.readable {
                    Some(tables::readable_columns(&project(&storage_names)?)?)
                } else {
                    Some(project(&storage_names)?)
                };
                let flag = if want_retracted {
                    let ids = project(&flag_names)?;
                    let with = tables::with_retracted(
                        &tables::readable_columns(&ids)?,
                        &ids,
                        &self.retracted,
                    )?;
                    Some(with.column(with.num_columns() - 1).clone())
                } else {
                    None
                };
                let mut columns = Vec::with_capacity(names.len());
                for name in &names {
                    if *name == tables::RETRACTED_COLUMN && self.readable {
                        columns.push(flag.clone().expect("flag built when projected"));
                    } else {
                        columns.push(
                            converted
                                .as_ref()
                                .and_then(|c| c.column_by_name(name))
                                .cloned()
                                .ok_or_else(|| {
                                    crate::QueryError::Exec(format!(
                                        "the events table has no column {name:?}"
                                    ))
                                })?,
                        );
                    }
                }
                out.push(RecordBatch::try_new_with_options(
                    Arc::clone(&schema),
                    columns,
                    &datafusion::arrow::array::RecordBatchOptions::new().with_row_count(Some(n)),
                )?);
            }
        }
        Ok((schema, out))
    }

    /// Row count without building anything: the parts know.
    pub fn row_count(&self) -> usize {
        self.parts
            .iter()
            .map(|p| p.batches.iter().map(RecordBatch::num_rows).sum::<usize>())
            .sum()
    }
}

#[async_trait]
impl TableProvider for EventsTable {
    fn schema(&self) -> SchemaRef {
        Arc::clone(&self.schema)
    }

    fn table_type(&self) -> TableType {
        TableType::Base
    }

    async fn scan(
        &self,
        state: &dyn Session,
        projection: Option<&Vec<usize>>,
        filters: &[Expr],
        limit: Option<usize>,
    ) -> DfResult<Arc<dyn ExecutionPlan>> {
        // A statement that names its columns gets those columns; only
        // `SELECT *` takes the whole (cached) table.
        if let Some(cols) = projection {
            if cols.is_empty() {
                // `count(*)`: no column at all, only how many rows. DataFusion
                // wants a table to project nothing out of, so give it one
                // Boolean column per batch and have it drop that.
                let schema = Arc::new(Schema::new_with_metadata(
                    vec![Field::new("rows", DataType::Boolean, false)],
                    self.schema.metadata().clone(),
                ));
                let mut batches = Vec::new();
                let mut rows = 0;
                'parts: for part in &self.parts {
                    for b in &part.batches {
                        if limit.is_some_and(|l| rows >= l) {
                            break 'parts;
                        }
                        rows += b.num_rows();
                        batches.push(RecordBatch::try_new(
                            Arc::clone(&schema),
                            vec![Arc::new(datafusion::arrow::array::BooleanArray::from(
                                vec![false; b.num_rows()],
                            ))],
                        )?);
                    }
                }
                let mem = MemTable::try_new(schema, vec![batches])?;
                return mem.scan(state, Some(cols), filters, limit).await;
            }
            let (schema, batches) = self
                .projected(cols, limit)
                .map_err(|e| DataFusionError::Execution(e.to_string()))?;
            let mem = MemTable::try_new(schema, vec![batches])?;
            return mem.scan(state, None, filters, limit).await;
        }
        let batches = self.batches(true, limit)?;
        let mem = MemTable::try_new(Arc::clone(&self.schema), vec![batches])?;
        mem.scan(state, None, filters, limit).await
    }
}

/// One projection table, built on the first statement that scans it. A
/// `SHOW SESSIONS` builds `sessions`; the 180 k-row `edges` table over
/// 200 k events is never built unless something reads it.
#[derive(Debug)]
pub(crate) struct LazyProjectionTable {
    name: &'static str,
    schema: SchemaRef,
    projection: Arc<attemptdb_project::Projection>,
    graph: Arc<OnceLock<Arc<crate::graph::Graph>>>,
    batch: OnceLock<std::result::Result<RecordBatch, String>>,
}

impl LazyProjectionTable {
    pub fn new(
        name: &'static str,
        projection: Arc<attemptdb_project::Projection>,
        graph: Arc<OnceLock<Arc<crate::graph::Graph>>>,
    ) -> Self {
        Self {
            name,
            schema: tables::projection_schema(name),
            projection,
            graph,
            batch: OnceLock::new(),
        }
    }

    fn graph(&self) -> Arc<crate::graph::Graph> {
        Arc::clone(
            self.graph
                .get_or_init(|| Arc::new(crate::graph::Graph::build(&self.projection))),
        )
    }

    /// Rows the table will hold, without building it.
    pub fn row_count(&self) -> usize {
        tables::projection_table_rows(self.name, &self.projection, &|| self.graph())
    }

    fn batch(&self) -> DfResult<RecordBatch> {
        self.batch
            .get_or_init(|| {
                tables::projection_table(self.name, &self.projection, &|| self.graph())
                    .map_err(|e| e.to_string())
            })
            .clone()
            .map_err(DataFusionError::Execution)
    }
}

#[async_trait]
impl TableProvider for LazyProjectionTable {
    fn schema(&self) -> SchemaRef {
        Arc::clone(&self.schema)
    }

    fn table_type(&self) -> TableType {
        TableType::Base
    }

    async fn scan(
        &self,
        state: &dyn Session,
        projection: Option<&Vec<usize>>,
        filters: &[Expr],
        limit: Option<usize>,
    ) -> DfResult<Arc<dyn ExecutionPlan>> {
        let batch = self.batch()?;
        let mem = MemTable::try_new(Arc::clone(&self.schema), vec![vec![batch]])?;
        mem.scan(state, projection, filters, limit).await
    }
}
