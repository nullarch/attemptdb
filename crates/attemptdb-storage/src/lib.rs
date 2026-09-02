//! AttemptDB storage engine.
//!
//! Physical layout of a live database directory (`.attemptdb/`):
//!
//! ```text
//! .attemptdb/
//! ├── ATTEMPTDB              identity file (JSON)
//! ├── LOCK                   advisory single-writer lock
//! ├── wal/NNNNNN.wal         framed append-only records (recent writes)
//! ├── segments/seg-*.arrow   immutable Arrow IPC columnar segments (history)
//! ├── manifest/gen-NNNNNN.json  generation snapshots; newest valid wins
//! ├── spool/*.spool          inbox written by hook processes (same framing)
//! └── blobs/xx/<id>.blob     encrypted content blobs (`content`/`raw`), keyed by
//!                            HMAC of the plaintext, referenced from v2 segments
//! ```
//!
//! The byte-level contract is documented in `docs/storage-format.md`; this
//! crate is written to that document. Nothing here persists native Rust
//! layouts: every integer is little-endian, every string is UTF-8, every
//! frame carries a length and a CRC32C.

pub mod blobs;
pub mod cache;
pub mod compaction;
pub mod db;
pub mod failpoint;
pub mod format;
pub mod frame;
pub mod identity;
pub mod manifest;
pub mod memtable;
pub mod repair;
pub mod segment;
pub mod snapshot;
pub mod spool;
pub mod wal;

pub use blobs::{BlobId, KeyId, KeyProvider, MasterKey, StaticKeyProvider};
pub use cache::{CachedSegment, ContentResolver, Refreshed, ScanCache};
pub use compaction::{CompactionPlan, CompactionPolicy, CompactionReport, PlannedRun};
pub use db::{Database, DbStats, DurabilityPolicy, IngestReport, OpenOptions, ScanFilter};
pub use identity::Identity;
pub use spool::{SpoolReader, SpoolWriter};

/// Errors produced by the storage engine.
#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("io error at {path}: {source}")]
    Io {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("database is locked by another writer: {0}")]
    Locked(std::path::PathBuf),
    #[error("not an AttemptDB database: {0}")]
    NotADatabase(std::path::PathBuf),
    #[error("corrupt {what} at {path}: {detail}")]
    Corrupt {
        what: &'static str,
        path: std::path::PathBuf,
        detail: String,
    },
    #[error("unsupported format version {found} for {what} (this build supports {supported})")]
    UnsupportedFormat {
        what: &'static str,
        found: u16,
        supported: u16,
    },
    /// A blob is encrypted under a key the provider does not hold. Readers
    /// treat this as "content unavailable", never as corruption.
    #[error("encrypted content unavailable (no key for key_id {key_id})")]
    NoKey { key_id: uuid::Uuid },
    #[error(transparent)]
    Core(#[from] attemptdb_core::CoreError),
    #[error("arrow error: {0}")]
    Arrow(#[from] arrow::error::ArrowError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("{0}")]
    Other(String),
}

impl StorageError {
    pub fn io(path: impl Into<std::path::PathBuf>, source: std::io::Error) -> Self {
        StorageError::Io {
            path: path.into(),
            source,
        }
    }
}

pub type Result<T> = std::result::Result<T, StorageError>;

/// Helper to attach a path to an io error.
pub(crate) trait IoAt<T> {
    fn at(self, path: &std::path::Path) -> Result<T>;
}

impl<T> IoAt<T> for std::io::Result<T> {
    fn at(self, path: &std::path::Path) -> Result<T> {
        self.map_err(|e| StorageError::io(path, e))
    }
}
