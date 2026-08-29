//! Physical format constants and low-level byte helpers.
//!
//! See `docs/storage-format.md`. Every constant here is part of the on-disk
//! contract; change them only together with a format version bump.

/// Magic for WAL files.
pub const MAGIC_WAL: [u8; 4] = *b"ATWL";
/// Magic for spool files (same framing as WAL, different producer).
pub const MAGIC_SPOOL: [u8; 4] = *b"ATSP";
/// Magic for `.atdb` snapshot containers.
pub const MAGIC_SNAPSHOT: [u8; 4] = *b"ATDB";

/// Format version of framed files (WAL/spool).
pub const FRAME_FORMAT_VERSION: u16 = 1;
/// Format version of the segment files (Arrow IPC + AttemptDB metadata).
pub const SEGMENT_FORMAT_VERSION: u16 = 1;
/// Format version of the manifest document.
pub const MANIFEST_FORMAT_VERSION: u16 = 1;
/// Format version of the identity file.
pub const IDENTITY_FORMAT_VERSION: u16 = 1;
/// Format version of the snapshot container.
pub const SNAPSHOT_FORMAT_VERSION: u16 = 1;

/// Size of the fixed file header of framed files.
pub const FILE_HEADER_LEN: usize = 32;
/// Size of the fixed record header (len + crc + type + codec + flags).
pub const RECORD_HEADER_LEN: usize = 12;
/// Hard cap on a single record payload (64 MiB). Larger payloads indicate
/// corruption rather than legitimate data.
pub const MAX_RECORD_PAYLOAD: u32 = 64 * 1024 * 1024;

/// Record types inside framed files.
pub mod record_type {
    /// One canonical event.
    pub const EVENT: u8 = 1;
    /// Writer checkpoint marker (payload: JSON `{"source_seq":..,"hlc":..}`).
    pub const CHECKPOINT: u8 = 2;
}

pub const IDENTITY_FILE: &str = "ATTEMPTDB";
pub const LOCK_FILE: &str = "LOCK";
pub const WAL_DIR: &str = "wal";
pub const SEGMENTS_DIR: &str = "segments";
pub const MANIFEST_DIR: &str = "manifest";
pub const SPOOL_DIR: &str = "spool";
pub const BLOBS_DIR: &str = "blobs";

pub fn u16_le(b: &[u8]) -> u16 {
    u16::from_le_bytes([b[0], b[1]])
}

pub fn u32_le(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

pub fn u64_le(b: &[u8]) -> u64 {
    u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
}

pub fn i64_le(b: &[u8]) -> i64 {
    i64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
}
