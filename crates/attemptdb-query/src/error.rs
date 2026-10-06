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

/// An error and the causes under it as one message, with what a person can
/// do about the ones that have a known remedy.
///
/// Walking `source()` and joining the texts repeats a cause whose parent
/// already prints it (`io error at …: File exists (os error 17)` listing
/// `File exists (os error 17)` as its source again), so a cause the text so
/// far already ends with is not repeated. A database written by a newer
/// `attempt` says to update, and a directory that cannot be written says so:
/// `attempt` takes the writer lock in the database directory even to read.
pub fn chain_message(err: &(dyn std::error::Error + 'static)) -> String {
    chain_message_with(err, &|_| None)
}

/// [`chain_message`] for a caller that wraps [`StorageError`] in an error
/// type this crate does not know (`attemptdb-capture`'s, whose `transparent`
/// variant hides the storage error from `source()`): `peel` returns the
/// storage error inside such a wrapper.
pub fn chain_message_with(
    err: &(dyn std::error::Error + 'static),
    peel: &dyn for<'a> Fn(&'a (dyn std::error::Error + 'static)) -> Option<&'a attemptdb_storage::StorageError>,
) -> String {
    use attemptdb_storage::StorageError;
    let mut parts: Vec<String> = Vec::new();
    let mut format_versions: Option<(u16, u16)> = None;
    let mut unwritable: Option<std::path::PathBuf> = None;
    let mut current: Option<&(dyn std::error::Error + 'static)> = Some(err);
    while let Some(e) = current {
        match e.downcast_ref::<StorageError>().or_else(|| peel(e)) {
            Some(StorageError::UnsupportedFormat {
                found, supported, ..
            }) => format_versions = Some((*found, *supported)),
            Some(StorageError::Io { path, source })
                if source.kind() == std::io::ErrorKind::PermissionDenied
                    || source.kind() == std::io::ErrorKind::ReadOnlyFilesystem =>
            {
                unwritable = Some(path.parent().unwrap_or(path).to_path_buf());
            }
            _ => {}
        }
        let text = e.to_string();
        if !parts.last().is_some_and(|prev| prev.ends_with(&text)) {
            parts.push(text);
        }
        current = e.source();
    }
    let mut out = parts.join(": ");
    if let Some((found, supported)) = format_versions {
        if found > supported {
            out.push_str(
                "\n  this database was written by a newer attempt than this one: update attempt (`attempt update`, or run the installer again), then open it",
            );
        } else {
            out.push_str(
                "\n  this database was written in an older format than this attempt reads: open it with the attempt that wrote it, or restore a snapshot",
            );
        }
    }
    if let Some(dir) = unwritable {
        out.push_str(&format!(
            "\n  {} is not writable (a read-only directory or file system?): attempt imports spooled events and takes its writer lock there, even to read; make it writable, or read a copy with --snapshot",
            dir.display()
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use attemptdb_storage::StorageError;

    #[test]
    fn a_cause_the_parent_already_prints_is_not_repeated() {
        let io = std::io::Error::from_raw_os_error(17);
        let e = StorageError::io("/db/.attemptdb/manifest", io);
        let one = e.to_string();
        let msg = chain_message(&e);
        assert_eq!(msg.matches("File exists").count(), 1, "{msg}");
        assert!(msg.starts_with(&one), "{msg}");
        // A chain whose parent does not print the cause keeps both.
        #[derive(Debug, thiserror::Error)]
        #[error("opening the thing")]
        struct Outer(#[source] std::io::Error);
        let msg = chain_message(&Outer(std::io::Error::from_raw_os_error(2)));
        assert!(msg.starts_with("opening the thing: "), "{msg}");
    }

    #[test]
    fn a_newer_database_says_to_update() {
        let e = StorageError::UnsupportedFormat {
            what: "identity file",
            found: 7,
            supported: 1,
        };
        let msg = chain_message(&e);
        assert!(msg.contains("unsupported format version 7"), "{msg}");
        assert!(msg.contains("newer attempt") && msg.contains("update attempt"), "{msg}");
        let older = StorageError::UnsupportedFormat {
            what: "manifest",
            found: 0,
            supported: 1,
        };
        assert!(chain_message(&older).contains("older format"));
    }

    #[test]
    fn an_unwritable_directory_is_named() {
        let io = std::io::Error::from_raw_os_error(13);
        let e = StorageError::io("/ro/db/LOCK", io);
        let msg = chain_message(&e);
        assert!(msg.contains("/ro/db is not writable"), "{msg}");
        assert!(msg.contains("--snapshot"), "{msg}");
    }
}
