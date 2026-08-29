//! Logical codecs for the canonical model.
//!
//! Codec identifiers are persisted in storage frame headers so the physical
//! layer can evolve encodings without breaking old files.

use crate::event::Event;
use crate::schema::{CANONICAL_SCHEMA_VERSION, MIN_READABLE_SCHEMA_VERSION};
use crate::{CoreError, Result};

/// Codec identifiers persisted in WAL/spool frame headers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum CodecId {
    /// UTF-8 JSON with string field names; self-describing; preserves unknown
    /// fields. The v1 default.
    Json = 1,
}

impl CodecId {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            1 => Some(CodecId::Json),
            _ => None,
        }
    }
}

pub fn encode_event(ev: &Event) -> Result<Vec<u8>> {
    Ok(serde_json::to_vec(ev)?)
}

pub fn decode_event(codec: CodecId, bytes: &[u8]) -> Result<Event> {
    match codec {
        CodecId::Json => {
            let ev: Event = serde_json::from_slice(bytes)?;
            if ev.schema_version < MIN_READABLE_SCHEMA_VERSION
                || ev.schema_version > CANONICAL_SCHEMA_VERSION + 100
            {
                return Err(CoreError::UnsupportedSchema {
                    found: ev.schema_version,
                    supported: CANONICAL_SCHEMA_VERSION,
                });
            }
            Ok(ev)
        }
    }
}

/// Content identity hash (SHA-256, hex). Used for content-addressed blobs and
/// for deduplication without exposing plaintext.
pub fn content_hash(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(bytes))
}

/// CRC32C frame checksum shared by WAL and spool frames.
pub fn frame_checksum(bytes: &[u8]) -> u32 {
    crc32c::crc32c(bytes)
}
