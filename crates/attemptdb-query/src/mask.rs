//! Retraction is not redaction: masked views for the agent and UI surfaces.
//!
//! `attempt retract` hides rows from AttemptQL and flags them in SQL; the
//! rows stay in the database, and so does their text. On the owner's CLI
//! that is the point (an audit trail, `INCLUDING RETRACTED`). On MCP and in
//! the local web UI a retracted prompt that is still one `SELECT
//! content_json FROM events WHERE retracted` away defeats the retraction for
//! the one reader that cannot be trusted to forget it.
//!
//! [`masked_context`] builds a second DataFusion context over the *same*
//! table providers in which every table that carries text and a `retracted`
//! flag is a view that returns the text columns as NULL for retracted rows.
//! The views are built from logical plans that hold the providers directly,
//! so the unmasked tables are not registered under any name a statement
//! could reach. Because the views sit below the statement, filters and
//! joins see the masked values too: `WHERE content_json LIKE '%secret%'`
//! cannot probe a retracted row's content.

use crate::error::{QueryError, Result};
use datafusion::arrow::datatypes::SchemaRef;
use datafusion::catalog::TableProvider;
use datafusion::common::Column;
use datafusion::datasource::{ViewTable, provider_as_source};
use datafusion::logical_expr::{Expr, LogicalPlanBuilder, when};
use datafusion::prelude::{SessionConfig, SessionContext, lit};
use datafusion::scalar::ScalarValue;
use std::sync::Arc;

/// The `retracted` flag column of a table that has one.
const RETRACTED: &str = "retracted";

/// Tables whose text columns are blanked for retracted rows.
const MASKED_BY_FLAG: &[(&str, &[&str])] = &[
    ("events", &["content_json", "raw_json", "unknown_json"]),
    ("turns", &["objective", "inferred_objective"]),
    ("attempts", &["objective", "note"]),
];

/// Tables with text and no `retracted` column to decide by: the text is not
/// served at all on these surfaces (`events` has the same text, flagged).
const MASKED_ALWAYS: &[(&str, &[&str])] =
    &[("events_raw", &["content_json", "raw_json", "unknown_json"])];

/// Whether `schema` has a column whose NULL can mean "retracted".
pub fn has_masked_column(schema: &SchemaRef) -> bool {
    MASKED_BY_FLAG
        .iter()
        .chain(MASKED_ALWAYS)
        .flat_map(|(_, cols)| cols.iter())
        .any(|c| schema.index_of(c).is_ok())
}

/// The note attached to a result that carries a masked column.
pub const MASK_NOTE: &str = "content columns (content_json, raw_json, unknown_json, objective, inferred_objective, note) are NULL for retracted rows on this surface, so a NULL there can mean a retracted row and not only capture_mode = 'metadata_only'; events_raw does not serve content here (use events)";

fn masked_view(
    name: &str,
    provider: Arc<dyn TableProvider>,
    columns: &[&str],
    always: bool,
) -> Result<Arc<dyn TableProvider>> {
    let schema = provider.schema();
    let mut exprs: Vec<Expr> = Vec::with_capacity(schema.fields().len());
    for f in schema.fields() {
        let column = Expr::Column(Column::from_name(f.name().as_str()));
        if columns.contains(&f.name().as_str()) {
            let null = lit(ScalarValue::try_from(f.data_type())?);
            let masked = if always {
                null
            } else {
                when(Expr::Column(Column::from_name(RETRACTED)), null).otherwise(column)?
            };
            exprs.push(masked.alias(f.name().as_str()));
        } else {
            exprs.push(column);
        }
    }
    let plan = LogicalPlanBuilder::scan(name, provider_as_source(provider), None)?
        .project(exprs)?
        .build()?;
    Ok(Arc::new(ViewTable::new(plan, None)))
}

/// A context over `providers` (the unmasked tables, shared with the
/// engine's own context) in which text of retracted rows reads as NULL.
pub(crate) fn masked_context(
    config: SessionConfig,
    providers: &[(String, Arc<dyn TableProvider>)],
) -> Result<SessionContext> {
    let ctx = SessionContext::new_with_config(config);
    for (name, provider) in providers {
        let table = if let Some((_, cols)) = MASKED_BY_FLAG.iter().find(|(t, _)| t == name) {
            masked_view(name, Arc::clone(provider), cols, false)?
        } else if let Some((_, cols)) = MASKED_ALWAYS.iter().find(|(t, _)| t == name) {
            masked_view(name, Arc::clone(provider), cols, true)?
        } else {
            Arc::clone(provider)
        };
        ctx.register_table(name.as_str(), table)
            .map_err(QueryError::from)?;
    }
    Ok(ctx)
}
