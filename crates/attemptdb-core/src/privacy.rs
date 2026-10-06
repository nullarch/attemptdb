//! Capture (privacy) modes.
//!
//! The mode is a storage property: it decides what may be persisted locally
//! and what may ever leave the machine. It is recorded on every event so that
//! later readers know which fields can legitimately be absent.

use serde::{Deserialize, Deserializer, Serialize};
use std::fmt;
use std::str::FromStr;

/// What may be persisted locally and what may leave the machine.
///
/// Deserialization is tolerant in the safe direction only: a mode this build
/// does not know (written by a newer build) reads as
/// [`CaptureMode::MetadataOnly`], the most restrictive mode, never as a more
/// permissive one. See [`CaptureMode::from_stored`].
#[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Debug, Default)]
#[serde(rename_all = "snake_case")]
pub enum CaptureMode {
    /// Only allowlisted metadata (tool names, timestamps, path shapes, sizes,
    /// outcomes). No prompts, commands, file contents, or tool output are
    /// persisted anywhere. This is the compatibility mode for existing
    /// VibeMon users.
    MetadataOnly,
    /// Detailed local evidence (prompts, commands, tool output) is stored in
    /// encrypted local blobs for local inference and display; nothing content
    /// bearing is synced. This is the default for new AttemptDB installs.
    #[default]
    LocalSemantic,
    /// Content may be synced to a hosted service. Requires explicit opt-in by
    /// the user or organisation policy.
    FullSync,
}

impl CaptureMode {
    /// Read a persisted mode name. Only the three canonical names are
    /// recognised; anything else (a mode from a newer build, a corrupt
    /// value) fails closed to `metadata_only`.
    pub fn from_stored(s: &str) -> Self {
        match s {
            "metadata_only" => CaptureMode::MetadataOnly,
            "local_semantic" => CaptureMode::LocalSemantic,
            "full_sync" => CaptureMode::FullSync,
            _ => CaptureMode::MetadataOnly,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            CaptureMode::MetadataOnly => "metadata_only",
            CaptureMode::LocalSemantic => "local_semantic",
            CaptureMode::FullSync => "full_sync",
        }
    }

    /// Whether content-bearing fields may be persisted locally at all.
    pub fn persists_content_locally(self) -> bool {
        !matches!(self, CaptureMode::MetadataOnly)
    }

    /// Whether content-bearing fields may leave the device.
    pub fn syncs_content(self) -> bool {
        matches!(self, CaptureMode::FullSync)
    }
}

impl<'de> Deserialize<'de> for CaptureMode {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Ok(CaptureMode::from_stored(&s))
    }
}

impl fmt::Display for CaptureMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for CaptureMode {
    type Err = crate::CoreError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().replace('-', "_").as_str() {
            "metadata_only" | "metadata" => Ok(CaptureMode::MetadataOnly),
            "local_semantic" | "local" => Ok(CaptureMode::LocalSemantic),
            "full_sync" | "full" => Ok(CaptureMode::FullSync),
            other => Err(crate::CoreError::InvalidId(format!(
                "unknown capture mode '{other}' (expected metadata_only, local_semantic, full_sync)"
            ))),
        }
    }
}
