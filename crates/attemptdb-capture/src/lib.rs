//! Capture runtime: locating the database, the hook entrypoint, the spool
//! importer, the installer, and the doctor.
//!
//! Two processes touch the database directory:
//!
//! - **Hook processes** (`attempt hook <provider>`), spawned by coding agents
//!   for every lifecycle/tool event. They must finish in milliseconds and
//!   therefore never open the database: they normalise the payload and
//!   append it to the spool ([`hook`]).
//! - **The writer** (`attempt` CLI commands or the daemon), which claims the
//!   spool and ingests it into the WAL/segments ([`ingest`]).

pub mod agents;
pub mod config;
pub mod doctor;
pub mod git;
pub mod hook;
pub mod ingest;
pub mod install;
pub mod locator;
pub mod platform;

pub use config::{Config, DeviceRecord};
pub use hook::{HookOutcome, run_hook};
pub use locator::{DbSource, Locator};

/// Errors produced by the capture runtime.
#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    #[error("io error at {path}: {source}")]
    Io {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    Storage(#[from] attemptdb_storage::StorageError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, CaptureError>;

pub(crate) fn io_at(path: &std::path::Path, source: std::io::Error) -> CaptureError {
    CaptureError::Io { path: path.to_path_buf(), source }
}
