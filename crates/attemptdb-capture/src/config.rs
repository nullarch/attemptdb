//! User configuration and the persistent device identity.
//!
//! Both files are small JSON documents so the hook path can read them in
//! microseconds without a TOML parser. Unknown keys are preserved.

use crate::{Result, io_at};
use attemptdb_core::{CaptureMode, DeviceId, Timestamp};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const CONFIG_FILE: &str = "config.json";
pub const DEVICE_FILE: &str = "device.json";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Config {
    /// Capture (privacy) mode applied to every new event.
    #[serde(default)]
    pub capture_mode: CaptureMode,
    /// Keep the original provider payload (`raw`) when the mode allows.
    #[serde(default = "default_true")]
    pub keep_raw_payload: bool,
    /// fsync every spool append. Off by default: the spool is a transport
    /// and the WAL is the durability boundary; fsync dominates hook latency.
    #[serde(default)]
    pub spool_sync: bool,
    /// Where HN/GitHub/CLI installs came from, for attribution. Never sent
    /// anywhere by the local product.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub install_source: Option<String>,
    #[serde(flatten, default)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

fn default_true() -> bool {
    true
}

impl Default for Config {
    fn default() -> Self {
        Self {
            capture_mode: CaptureMode::LocalSemantic,
            keep_raw_payload: true,
            spool_sync: false,
            install_source: None,
            extra: Default::default(),
        }
    }
}

impl Config {
    pub fn path(config_dir: &Path) -> PathBuf {
        config_dir.join(CONFIG_FILE)
    }

    /// Load the config, falling back to defaults when the file is absent or
    /// unreadable (the hook path must never fail because of config).
    pub fn load_or_default(config_dir: &Path) -> Self {
        let path = Self::path(config_dir);
        std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, config_dir: &Path) -> Result<()> {
        std::fs::create_dir_all(config_dir).map_err(|e| io_at(config_dir, e))?;
        let path = Self::path(config_dir);
        let tmp = config_dir.join(format!("{CONFIG_FILE}.tmp"));
        let bytes = serde_json::to_vec_pretty(self)?;
        std::fs::write(&tmp, bytes).map_err(|e| io_at(&tmp, e))?;
        std::fs::rename(&tmp, &path).map_err(|e| io_at(&path, e))?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeviceRecord {
    pub device_id: DeviceId,
    pub created_at: Timestamp,
    #[serde(default)]
    pub os: String,
    #[serde(default)]
    pub arch: String,
    #[serde(flatten, default)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl DeviceRecord {
    pub fn path(data_dir: &Path) -> PathBuf {
        data_dir.join(DEVICE_FILE)
    }

    /// Load the device identity, creating it on first use.
    pub fn load_or_create(data_dir: &Path) -> Result<Self> {
        let path = Self::path(data_dir);
        if let Ok(bytes) = std::fs::read(&path)
            && let Ok(rec) = serde_json::from_slice::<DeviceRecord>(&bytes) {
                return Ok(rec);
            }
        std::fs::create_dir_all(data_dir).map_err(|e| io_at(data_dir, e))?;
        let rec = DeviceRecord {
            device_id: DeviceId::new(),
            created_at: Timestamp::now(),
            os: std::env::consts::OS.to_string(),
            arch: std::env::consts::ARCH.to_string(),
            extra: Default::default(),
        };
        let tmp = data_dir.join(format!("{DEVICE_FILE}.tmp-{}", std::process::id()));
        std::fs::write(&tmp, serde_json::to_vec_pretty(&rec)?).map_err(|e| io_at(&tmp, e))?;
        // Two hook processes may race on first use; whichever renames last
        // wins and the other re-reads. Use rename (atomic replace) and then
        // reload to converge on one id.
        std::fs::rename(&tmp, &path).map_err(|e| io_at(&path, e))?;
        let bytes = std::fs::read(&path).map_err(|e| io_at(&path, e))?;
        Ok(serde_json::from_slice(&bytes)?)
    }
}
