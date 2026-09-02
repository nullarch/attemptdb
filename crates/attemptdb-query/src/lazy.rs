//! The `events` and `events_raw` tables, with content resolved on demand.
//!
//! A format 2 segment keeps `content`/`raw` in one encrypted blob file per
//! row and stores only the blob ids in the batch. Filling `content_json`
//! and `raw_json` means opening every one of those files — 15,000 of them
//! for an 8,800-event database — which is exactly what a `count(*)`, a
//! timeline or a `GROUP BY kind` never needs. [`EventsTable`] is a
//! `TableProvider` over the cached batches that resolves the two content
//! columns only when a statement projects them, and remembers the result
//! for the engine's lifetime.

use crate::parts::SegmentParts;
use crate::tables;
use async_trait::async_trait;
use attemptdb_project::RetractedSet;
use attemptdb_storage::ContentResolver;
use attemptdb_storage::segment::col;
use datafusion::arrow::array::RecordBatch;
use datafusion::arrow::datatypes::SchemaRef;
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

    fn build(&self, resolve: bool) -> crate::Result<Vec<RecordBatch>> {
        let mut out = Vec::new();
        for part in &self.parts {
            let readable = if self.readable {
                Some(part.readable()?)
            } else {
                None
            };
            for (i, storage) in part.batches.iter().enumerate() {
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

    fn batches(&self, resolve: bool) -> DfResult<Vec<RecordBatch>> {
        let cell = if resolve {
            &self.with_content
        } else {
            &self.plain
        };
        cell.get_or_init(|| self.build(resolve).map_err(|e| e.to_string()))
            .clone()
            .map_err(DataFusionError::Execution)
    }

    /// Row count without building anything: the parts know.
    pub fn row_count(&self) -> usize {
        self.parts
            .iter()
            .map(|p| p.batches.iter().map(RecordBatch::num_rows).sum::<usize>())
            .sum()
    }

    fn wants_content(&self, projection: Option<&Vec<usize>>) -> bool {
        let content = self.schema.index_of(col::CONTENT_JSON).ok();
        let raw = self.schema.index_of(col::RAW_JSON).ok();
        match projection {
            None => true,
            Some(cols) => cols.iter().any(|c| Some(*c) == content || Some(*c) == raw),
        }
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
        let batches = self.batches(self.wants_content(projection))?;
        let mem = MemTable::try_new(Arc::clone(&self.schema), vec![batches])?;
        mem.scan(state, projection, filters, limit).await
    }
}
