//! The `ATTEMPTDB` identity file: a small JSON document that marks a
//! directory as a database and records the versions needed to open it.

use crate::format::{IDENTITY_FILE, IDENTITY_FORMAT_VERSION};
use crate::{IoAt, Result, StorageError};
use attemptdb_core::schema::CANONICAL_SCHEMA_VERSION;
use attemptdb_core::{DeviceId, Timestamp};
use serde::{Deserialize, Serialize};
use std::path::Path;
use uuid::Uuid;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Identity {
    pub format_version: u16,
    pub schema_version: u16,
    pub db_id: Uuid,
    pub device_id: DeviceId,
    pub created_at: Timestamp,
    #[serde(default)]
    pub created_by: String,
    /// Fields from newer builds are preserved.
    #[serde(flatten, default)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl Identity {
    pub fn new(device_id: DeviceId) -> Self {
        Self {
            format_version: IDENTITY_FORMAT_VERSION,
            schema_version: CANONICAL_SCHEMA_VERSION,
            db_id: Uuid::now_v7(),
            device_id,
            created_at: Timestamp::now(),
            created_by: format!("attemptdb {}", env!("CARGO_PKG_VERSION")),
            extra: Default::default(),
        }
    }

    pub fn path(dir: &Path) -> std::path::PathBuf {
        dir.join(IDENTITY_FILE)
    }

    pub fn load(dir: &Path) -> Result<Self> {
        let path = Self::path(dir);
        if !path.exists() {
            return Err(StorageError::NotADatabase(dir.to_path_buf()));
        }
        let bytes = std::fs::read(&path).at(&path)?;
        let id: Identity = serde_json::from_slice(&bytes).map_err(|e| StorageError::Corrupt {
            what: "identity file",
            path: path.clone(),
            detail: e.to_string(),
        })?;
        if id.format_version != IDENTITY_FORMAT_VERSION {
            return Err(StorageError::UnsupportedFormat {
                what: "identity file",
                found: id.format_version,
                supported: IDENTITY_FORMAT_VERSION,
            });
        }
        Ok(id)
    }

    pub fn write(&self, dir: &Path) -> Result<()> {
        let path = Self::path(dir);
        let tmp = dir.join(format!("{IDENTITY_FILE}.tmp"));
        let bytes = serde_json::to_vec_pretty(self)?;
        crate::manifest::write_atomically(&tmp, &path, &bytes)
    }
}
