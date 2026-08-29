//! Error type for the query layer.

use datafusion::arrow::error::ArrowError;
use datafusion::error::DataFusionError;

/// Errors produced while parsing, planning, or executing a query.
#[derive(Debug, thiserror::Error)]
pub enum QueryError {
    /// The AttemptQL text could not be parsed. `position` is a byte offset
    /// into the statement text; see [`crate::format_parse_error`] for a
    /// caret-style rendering.
    #[error("parse error at position {position}: {message}")]
    Parse { message: String, position: usize },
    /// The statement parsed but could not be compiled into a plan (unknown
    /// column, unsupported filter for the target, invalid time range, ...).
    #[error("plan error: {0}")]
    Plan(String),
    /// The plan failed while executing.
    #[error("execution error: {0}")]
    Exec(String),
    /// The underlying database could not be read.
    #[error(transparent)]
    Storage(#[from] attemptdb_storage::StorageError),
    /// An id or name in the statement does not resolve to anything loaded.
    #[error("not found: {0}")]
    NotFound(String),
}

impl QueryError {
    pub(crate) fn parse(message: impl Into<String>, position: usize) -> Self {
        QueryError::Parse {
            message: message.into(),
            position,
        }
    }

    pub(crate) fn plan(message: impl Into<String>) -> Self {
        QueryError::Plan(message.into())
    }

    pub(crate) fn not_found(message: impl Into<String>) -> Self {
        QueryError::NotFound(message.into())
    }
}

impl From<DataFusionError> for QueryError {
    fn from(e: DataFusionError) -> Self {
        match e {
            DataFusionError::Plan(m) => QueryError::Plan(m),
            DataFusionError::SQL(e, _) => QueryError::Plan(e.to_string()),
            DataFusionError::SchemaError(e, _) => QueryError::Plan(e.to_string()),
            DataFusionError::NotImplemented(m) => QueryError::Plan(format!("not supported: {m}")),
            DataFusionError::Diagnostic(_, inner) => QueryError::from(*inner),
            DataFusionError::Context(ctx, inner) => match QueryError::from(*inner) {
                QueryError::Plan(m) => QueryError::Plan(format!("{ctx}: {m}")),
                QueryError::Exec(m) => QueryError::Exec(format!("{ctx}: {m}")),
                other => other,
            },
            other => QueryError::Exec(other.to_string()),
        }
    }
}

impl From<ArrowError> for QueryError {
    fn from(e: ArrowError) -> Self {
        QueryError::Exec(e.to_string())
    }
}

/// Result alias used throughout the crate.
pub type Result<T> = std::result::Result<T, QueryError>;
