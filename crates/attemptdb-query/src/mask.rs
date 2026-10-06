//! Retraction is not redaction: masked views for the agent and UI surfaces.
//!
//! `attempt retract` hides rows from AttemptQL and flags them in SQL; the
//! rows stay in the database, and so does their text and where it happened. On the owner's CLI
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
use datafusion::logical_expr::{Expr, JoinType, LogicalPlan, LogicalPlanBuilder, when};
use datafusion::prelude::{SessionConfig, SessionContext, lit};
use datafusion::scalar::ScalarValue;
use std::sync::Arc;

/// The `retracted` flag column of a table that has one.
const RETRACTED: &str = "retracted";
/// The column of the retracted-session-ids plan joined to `corrections`.
const RETRACTED_SESSION: &str = "retr_sid";

/// What decides that a row's text is withheld.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Withheld {
    /// The row's `retracted` flag.
    Flagged,
    /// Every row: the table has no column to decide by, so the text is not
    /// served at all on these surfaces (another table has the same text,
    /// flagged).
    Always,
    /// A correction of retracted work: its `status` is `target_retracted`, or
    /// it was written into a session that is retracted (the projection does
    /// not call a correction of an attempt inside a retracted session
    /// `target_retracted`: the attempt is simply gone, `target_not_found`).
    OfRetractedWork,
}

/// The columns of an event that name where it happened: absolute and
/// home-elided paths. A retracted event leaves its location behind too.
const EVENT_PATHS: [&str; 3] = ["paths_json", "path_logical", "path_relative"];

/// Tables whose text and path columns are blanked, and for which rows.
const MASKED: &[(&str, Withheld, &[&str])] = &[
    (
        "events",
        Withheld::Flagged,
        &[
            "content_json",
            "raw_json",
            "unknown_json",
            EVENT_PATHS[0],
            EVENT_PATHS[1],
            EVENT_PATHS[2],
        ],
    ),
    (
        "events_raw",
        Withheld::Always,
        &[
            "content_json",
            "raw_json",
            "unknown_json",
            EVENT_PATHS[0],
            EVENT_PATHS[1],
            EVENT_PATHS[2],
        ],
    ),
    (
        "turns",
        Withheld::Flagged,
        &["objective", "inferred_objective"],
    ),
    ("tool_calls", Withheld::Flagged, &["path_relative", "paths"]),
    (
        "attempts",
        Withheld::Flagged,
        &["objective", "note", "approach", "paths"],
    ),
    ("corrections", Withheld::OfRetractedWork, &["note"]),
];

/// Whether `schema` has a column whose NULL can mean "retracted".
pub fn has_masked_column(schema: &SchemaRef) -> bool {
    MASKED
        .iter()
        .flat_map(|(_, _, cols)| cols.iter())
        .any(|c| schema.index_of(c).is_ok())
}

/// The note attached to a result that carries a masked column.
pub const MASK_NOTE: &str = "content and location columns (content_json, raw_json, unknown_json, paths_json, path_logical, path_relative, paths, approach, objective, inferred_objective, note) are NULL for retracted rows on this surface, so a NULL there can mean a retracted row and not only capture_mode = 'metadata_only' or an event with no path; events_raw serves neither content nor paths here (use events)";

/// The distinct ids of the retracted sessions, as a one-column plan
/// (`retr_sid`) over the (unmasked) `sessions` provider.
fn retracted_session_ids(sessions: Arc<dyn TableProvider>) -> Result<LogicalPlan> {
    Ok(
        LogicalPlanBuilder::scan("sessions", provider_as_source(sessions), None)?
            .filter(Expr::Column(Column::from_name(RETRACTED)))?
            .project(vec![
                Expr::Column(Column::from_name("session_id")).alias(RETRACTED_SESSION),
            ])?
            .distinct()?
            .build()?,
    )
}

fn masked_view(
    name: &str,
    provider: Arc<dyn TableProvider>,
    columns: &[&str],
    withheld: Withheld,
    retracted_sessions: Option<&LogicalPlan>,
) -> Result<Arc<dyn TableProvider>> {
    let schema = provider.schema();
    let mut builder = LogicalPlanBuilder::scan(name, provider_as_source(provider), None)?;
    // A correction is withheld when its session is retracted: left-join the
    // retracted session ids (a join, because DataFusion does not rewrite an
    // `IN (subquery)` that sits inside a projection).
    let joined = withheld == Withheld::OfRetractedWork && retracted_sessions.is_some();
    if let (true, Some(ids)) = (joined, retracted_sessions) {
        builder = builder.join(
            ids.clone(),
            JoinType::Left,
            (vec!["session_id"], vec![RETRACTED_SESSION]),
            None,
        )?;
    }
    let mut exprs: Vec<Expr> = Vec::with_capacity(schema.fields().len());
    for f in schema.fields() {
        let column = Expr::Column(Column::from_name(f.name().as_str()));
        if columns.contains(&f.name().as_str()) {
            let null = lit(ScalarValue::try_from(f.data_type())?);
            let masked = match withheld {
                Withheld::Always => null,
                Withheld::Flagged => {
                    when(Expr::Column(Column::from_name(RETRACTED)), null).otherwise(column)?
                }
                Withheld::OfRetractedWork => {
                    let mut hidden =
                        Expr::Column(Column::from_name("status")).eq(lit("target_retracted"));
                    if joined {
                        hidden = hidden
                            .or(Expr::Column(Column::from_name(RETRACTED_SESSION)).is_not_null());
                    }
                    when(hidden, null).otherwise(column)?
                }
            };
            exprs.push(masked.alias(f.name().as_str()));
        } else {
            exprs.push(column);
        }
    }
    let plan = builder.project(exprs)?.build()?;
    Ok(Arc::new(ViewTable::new(plan, None)))
}

/// A context over `providers` (the unmasked tables, shared with the
/// engine's own context) in which text of retracted rows reads as NULL.
pub(crate) fn masked_context(
    config: SessionConfig,
    providers: &[(String, Arc<dyn TableProvider>)],
) -> Result<SessionContext> {
    let ctx = SessionContext::new_with_config(config);
    let retracted_sessions = providers
        .iter()
        .find(|(n, _)| n == "sessions")
        .map(|(_, p)| retracted_session_ids(Arc::clone(p)))
        .transpose()?;
    for (name, provider) in providers {
        let table = match MASKED.iter().find(|(t, _, _)| t == name) {
            Some((_, withheld, cols)) => masked_view(
                name,
                Arc::clone(provider),
                cols,
                *withheld,
                retracted_sessions.as_ref(),
            )?,
            None => Arc::clone(provider),
        };
        ctx.register_table(name.as_str(), table)
            .map_err(QueryError::from)?;
    }
    Ok(ctx)
}
