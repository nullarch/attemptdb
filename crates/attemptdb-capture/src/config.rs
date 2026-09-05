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
    /// Whether `content`/`raw` are moved into encrypted blobs at segment
    /// write (`crate::keys`). `attempt init --no-encryption` sets `Off`.
    #[serde(default)]
    pub encryption: EncryptionMode,
    /// Whether the daemon (and `attempt maintenance`) install releases on
    /// their own. `on`: releases the policy marks required at once, others
    /// within a day at a quiet moment. `required`: only the required ones.
    /// `off`: never — `attempt doctor` says one is available.
    /// `ATTEMPTDB_NO_AUTO_UPDATE=1` in the environment means `off` regardless.
    #[serde(default)]
    pub auto_update: AutoUpdate,
    #[serde(flatten, default)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// How far the client goes on its own when a release is out (see
/// `crate::update`). The policy — which releases are required — is the
/// release's, published beside its assets; this is the machine's answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AutoUpdate {
    #[default]
    On,
    Required,
    Off,
}

impl AutoUpdate {
    pub fn as_str(self) -> &'static str {
        match self {
            AutoUpdate::On => "on",
            AutoUpdate::Required => "required",
            AutoUpdate::Off => "off",
        }
    }
}

/// Content-blob encryption policy of the writer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum EncryptionMode {
    /// Encrypt when a key is available from any source (OS key store, key
    /// file, passphrase); write inline otherwise. The default.
    #[default]
    Auto,
    /// Never encrypt; content stays inline in segments.
    Off,
    /// Refuse to open the writer without a key.
    Required,
}

impl EncryptionMode {
    pub fn as_str(self) -> &'static str {
        match self {
            EncryptionMode::Auto => "auto",
            EncryptionMode::Off => "off",
            EncryptionMode::Required => "required",
        }
    }
}

impl std::fmt::Display for EncryptionMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for EncryptionMode {
    type Err = crate::CaptureError;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" => Ok(EncryptionMode::Auto),
            "off" | "none" | "disabled" => Ok(EncryptionMode::Off),
            "required" | "on" => Ok(EncryptionMode::Required),
            other => Err(crate::CaptureError::Other(format!(
                "unknown encryption mode '{other}' (expected auto, off, required)"
            ))),
        }
    }
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
            encryption: EncryptionMode::Auto,
            auto_update: AutoUpdate::On,
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
            && let Ok(rec) = serde_json::from_slice::<DeviceRecord>(&bytes)
        {
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
