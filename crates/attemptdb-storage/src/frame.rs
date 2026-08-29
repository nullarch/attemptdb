//! Framed append-only files shared by the WAL and the spool.
//!
//! ```text
//! file header (32 bytes)
//!   0..4    magic ("ATWL" | "ATSP")
//!   4..6    format_version   u16 LE
//!   6..8    schema_version   u16 LE
//!   8..24   file_id          UUID bytes
//!   24..32  created_at       i64 LE, micros since epoch
//! record (12-byte header + payload)
//!   0..4    payload_len      u32 LE
//!   4..8    crc32c           u32 LE over (record_type, codec, flags, payload)
//!   8       record_type      u8
//!   9       codec            u8
//!   10..12  flags            u16 LE (reserved, 0)
//!   12..    payload
//! ```
//!
//! Readers stop at the first record that is truncated or fails its CRC and
//! report the byte offset of the last good record so writers can truncate.

use crate::format::*;
use crate::{IoAt, Result, StorageError};
use attemptdb_core::codec::{CodecId, decode_event, encode_event, frame_checksum};
use attemptdb_core::schema::CANONICAL_SCHEMA_VERSION;
use attemptdb_core::{Event, Timestamp};
use std::fs::{File, OpenOptions};
use std::io::{BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileHeader {
    pub magic: [u8; 4],
    pub format_version: u16,
    pub schema_version: u16,
    pub file_id: Uuid,
    pub created_at: Timestamp,
}

impl FileHeader {
    pub fn new(magic: [u8; 4]) -> Self {
        Self {
            magic,
            format_version: FRAME_FORMAT_VERSION,
            schema_version: CANONICAL_SCHEMA_VERSION,
            file_id: Uuid::now_v7(),
            created_at: Timestamp::now(),
        }
    }

    pub fn encode(&self) -> [u8; FILE_HEADER_LEN] {
        let mut b = [0u8; FILE_HEADER_LEN];
        b[0..4].copy_from_slice(&self.magic);
        b[4..6].copy_from_slice(&self.format_version.to_le_bytes());
        b[6..8].copy_from_slice(&self.schema_version.to_le_bytes());
        b[8..24].copy_from_slice(self.file_id.as_bytes());
        b[24..32].copy_from_slice(&self.created_at.as_micros().to_le_bytes());
        b
    }

    pub fn decode(b: &[u8], expected_magic: [u8; 4], path: &Path) -> Result<Self> {
        if b.len() < FILE_HEADER_LEN {
            return Err(StorageError::Corrupt {
                what: "file header",
                path: path.to_path_buf(),
                detail: format!("short header ({} bytes)", b.len()),
            });
        }
        let mut magic = [0u8; 4];
        magic.copy_from_slice(&b[0..4]);
        if magic != expected_magic {
            return Err(StorageError::Corrupt {
                what: "file header",
                path: path.to_path_buf(),
                detail: format!("bad magic {:?}", String::from_utf8_lossy(&magic)),
            });
        }
        let format_version = u16_le(&b[4..6]);
        if format_version != FRAME_FORMAT_VERSION {
            return Err(StorageError::UnsupportedFormat {
                what: "framed file",
                found: format_version,
                supported: FRAME_FORMAT_VERSION,
            });
        }
        let mut id = [0u8; 16];
        id.copy_from_slice(&b[8..24]);
        Ok(Self {
            magic,
            format_version,
            schema_version: u16_le(&b[6..8]),
            file_id: Uuid::from_bytes(id),
            created_at: Timestamp::from_micros(i64_le(&b[24..32])),
        })
    }
}

/// A decoded record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    pub record_type: u8,
    pub codec: u8,
    pub flags: u16,
    pub payload: Vec<u8>,
    /// Byte offset of the record header in the file.
    pub offset: u64,
}

impl Record {
    pub fn event(ev: &Event) -> Result<Self> {
        Ok(Self {
            record_type: record_type::EVENT,
            codec: CodecId::Json as u8,
            flags: 0,
            payload: encode_event(ev)?,
            offset: 0,
        })
    }

    pub fn checkpoint(payload: Vec<u8>) -> Self {
        Self {
            record_type: record_type::CHECKPOINT,
            codec: CodecId::Json as u8,
            flags: 0,
            payload,
            offset: 0,
        }
    }

    pub fn decode_event(&self) -> Result<Event> {
        let codec = CodecId::from_u8(self.codec).ok_or_else(|| StorageError::Other(format!(
            "unknown codec id {}",
            self.codec
        )))?;
        Ok(decode_event(codec, &self.payload)?)
    }

    /// Encode this record (header + payload) into `out`.
    pub fn encode_into(&self, out: &mut Vec<u8>) {
        let mut body = Vec::with_capacity(4 + self.payload.len());
        body.push(self.record_type);
        body.push(self.codec);
        body.extend_from_slice(&self.flags.to_le_bytes());
        body.extend_from_slice(&self.payload);
        let crc = frame_checksum(&body);
        out.extend_from_slice(&(self.payload.len() as u32).to_le_bytes());
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&body);
    }

    pub fn encoded_len(&self) -> usize {
        RECORD_HEADER_LEN + self.payload.len()
    }
}

/// Append-only writer. Creates the file with a header if it does not exist,
/// otherwise validates the header and positions at the end of the last valid
/// record (truncating a corrupt tail).
pub struct FrameWriter {
    file: File,
    path: PathBuf,
    header: FileHeader,
    len: u64,
}

impl FrameWriter {
    pub fn open(path: &Path, magic: [u8; 4]) -> Result<Self> {
        Self::open_trusted(path, magic, None)
    }

    /// Open for appending, trusting that every record before `committed_len`
    /// was already validated (e.g. by the previous appender, which recorded
    /// the length after a successful write). Only the tail after that offset
    /// is scanned, so the cost of opening stays proportional to what changed
    /// since the last append, not to the file size.
    ///
    /// The hint is never trusted blindly: if the tail scan does not start on
    /// a valid record boundary, the whole file is scanned instead, so a wrong
    /// hint can only cost time, never data.
    pub fn open_trusted(path: &Path, magic: [u8; 4], committed_len: Option<u64>) -> Result<Self> {
        let file_len = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        let exists = file_len > 0;
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .append(false)
            .write(true)
            .open(path)
            .at(path)?;
        let (header, len) = if exists {
            let hinted = committed_len
                .filter(|&l| l >= FILE_HEADER_LEN as u64 && l <= file_len)
                .and_then(|l| match FrameReader::scan_from(path, magic, l) {
                    // A hint that lands inside a record shows up as an
                    // immediate corruption at the hinted offset.
                    Ok(scan) if scan.truncated_at != Some(l) || l == file_len => Some(scan),
                    _ => None,
                });
            let scan = match hinted {
                Some(scan) => scan,
                None => FrameReader::scan(path, magic)?,
            };
            if scan.truncated_at.is_some() {
                file.set_len(scan.valid_len).at(path)?;
            }
            (scan.header, scan.valid_len)
        } else {
            let header = FileHeader::new(magic);
            file.write_all(&header.encode()).at(path)?;
            file.sync_all().at(path)?;
            (header, FILE_HEADER_LEN as u64)
        };
        file.seek(SeekFrom::Start(len)).at(path)?;
        Ok(Self { file, path: path.to_path_buf(), header, len })
    }

    pub fn header(&self) -> &FileHeader {
        &self.header
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len <= FILE_HEADER_LEN as u64
    }

    /// Append records without syncing. Returns the offset of the first record.
    pub fn append(&mut self, records: &[Record]) -> Result<u64> {
        let start = self.len;
        let mut buf = Vec::with_capacity(records.iter().map(Record::encoded_len).sum());
        for r in records {
            r.encode_into(&mut buf);
        }
        self.file.write_all(&buf).at(&self.path)?;
        self.len += buf.len() as u64;
        Ok(start)
    }

    /// Durably flush appended records.
    pub fn sync(&mut self) -> Result<()> {
        self.file.sync_data().at(&self.path)
    }

    pub fn sync_all(&mut self) -> Result<()> {
        self.file.sync_all().at(&self.path)
    }
}

/// Result of scanning a framed file.
#[derive(Debug)]
pub struct ScanResult {
    pub header: FileHeader,
    pub records: Vec<Record>,
    /// Length of the valid prefix (header + all good records).
    pub valid_len: u64,
    /// Offset at which a truncated or corrupt record was found, if any.
    pub truncated_at: Option<u64>,
    pub total_len: u64,
}

pub struct FrameReader;

impl FrameReader {
    /// Scan an entire file, returning every valid record and recovery info.
    pub fn scan(path: &Path, magic: [u8; 4]) -> Result<ScanResult> {
        Self::scan_from(path, magic, FILE_HEADER_LEN as u64)
    }

    /// Scan from `start` (which must be a record boundary at or after the
    /// header). Records before `start` are not returned.
    pub fn scan_from(path: &Path, magic: [u8; 4], start: u64) -> Result<ScanResult> {
        let file = File::open(path).at(path)?;
        let total_len = file.metadata().at(path)?.len();
        let mut reader = BufReader::with_capacity(1 << 16, file);
        let mut hdr = [0u8; FILE_HEADER_LEN];
        reader.read_exact(&mut hdr).at(path)?;
        let header = FileHeader::decode(&hdr, magic, path)?;
        let start = start.max(FILE_HEADER_LEN as u64);
        if start > FILE_HEADER_LEN as u64 {
            reader.seek(SeekFrom::Start(start)).at(path)?;
        }
        let mut records = Vec::new();
        let mut offset = start;
        let mut truncated_at = None;
        let mut head = [0u8; RECORD_HEADER_LEN];
        loop {
            match read_fully(&mut reader, &mut head) {
                Ok(true) => {}
                Ok(false) => break,
                Err(_) => {
                    truncated_at = Some(offset);
                    break;
                }
            }
            let payload_len = u32_le(&head[0..4]);
            let crc = u32_le(&head[4..8]);
            let record_type = head[8];
            let codec = head[9];
            let flags = u16_le(&head[10..12]);
            if payload_len > MAX_RECORD_PAYLOAD || payload_len == 0 && record_type == 0 {
                truncated_at = Some(offset);
                break;
            }
            let mut body = Vec::with_capacity(4 + payload_len as usize);
            body.extend_from_slice(&head[8..12]);
            body.resize(4 + payload_len as usize, 0);
            match read_fully(&mut reader, &mut body[4..]) {
                Ok(true) => {}
                _ => {
                    truncated_at = Some(offset);
                    break;
                }
            }
            if frame_checksum(&body) != crc {
                truncated_at = Some(offset);
                break;
            }
            let payload = body.split_off(4);
            records.push(Record { record_type, codec, flags, payload, offset });
            offset += RECORD_HEADER_LEN as u64 + payload_len as u64;
        }
        Ok(ScanResult { header, records, valid_len: offset, truncated_at, total_len })
    }
}

/// Read exactly `buf.len()` bytes. Returns Ok(false) on clean EOF at the
/// start, Err on a partial read.
fn read_fully<R: Read>(r: &mut R, buf: &mut [u8]) -> std::io::Result<bool> {
    let mut filled = 0;
    while filled < buf.len() {
        let n = r.read(&mut buf[filled..])?;
        if n == 0 {
            if filled == 0 {
                return Ok(false);
            }
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "partial record",
            ));
        }
        filled += n;
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use attemptdb_core::{CaptureMode, DeviceId, EventKind, ProjectRef, event::Provider};

    fn sample_event(i: u32) -> Event {
        let dev = DeviceId::nil();
        let mut ev = Event::new(
            dev,
            Provider::ClaudeCode,
            "PostToolUse",
            EventKind::ToolCallFinished,
            ProjectRef::derive("/p", None, &dev),
            "s",
            CaptureMode::LocalSemantic,
            "t",
        );
        ev.attrs.insert("i".into(), serde_json::json!(i));
        ev
    }

    #[test]
    fn roundtrip_and_recovery_from_torn_tail() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.wal");
        let events: Vec<Event> = (0..5).map(sample_event).collect();
        {
            let mut w = FrameWriter::open(&path, MAGIC_WAL).unwrap();
            let recs: Vec<Record> = events.iter().map(|e| Record::event(e).unwrap()).collect();
            w.append(&recs).unwrap();
            w.sync().unwrap();
        }
        let full_len = std::fs::metadata(&path).unwrap().len();
        // Tear the last record in half.
        {
            let f = OpenOptions::new().write(true).open(&path).unwrap();
            f.set_len(full_len - 7).unwrap();
        }
        let scan = FrameReader::scan(&path, MAGIC_WAL).unwrap();
        assert_eq!(scan.records.len(), 4);
        assert!(scan.truncated_at.is_some());
        let decoded: Vec<Event> = scan.records.iter().map(|r| r.decode_event().unwrap()).collect();
        assert_eq!(decoded, events[..4].to_vec());
        // Re-opening the writer truncates and lets us append again.
        {
            let mut w = FrameWriter::open(&path, MAGIC_WAL).unwrap();
            assert_eq!(w.len(), scan.valid_len);
            w.append(&[Record::event(&events[4]).unwrap()]).unwrap();
            w.sync().unwrap();
        }
        let scan = FrameReader::scan(&path, MAGIC_WAL).unwrap();
        assert_eq!(scan.records.len(), 5);
        assert!(scan.truncated_at.is_none());
    }

    #[test]
    fn crc_mismatch_stops_scan_without_losing_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("b.wal");
        {
            let mut w = FrameWriter::open(&path, MAGIC_WAL).unwrap();
            let recs: Vec<Record> = (0..3).map(|i| Record::event(&sample_event(i)).unwrap()).collect();
            w.append(&recs).unwrap();
            w.sync().unwrap();
        }
        let mut bytes = std::fs::read(&path).unwrap();
        let second = FrameReader::scan(&path, MAGIC_WAL).unwrap().records[1].offset as usize;
        bytes[second + RECORD_HEADER_LEN + 3] ^= 0xff; // flip a payload byte of record 2
        std::fs::write(&path, &bytes).unwrap();
        let scan = FrameReader::scan(&path, MAGIC_WAL).unwrap();
        assert_eq!(scan.records.len(), 1);
        assert_eq!(scan.truncated_at, Some(second as u64));
    }

    #[test]
    fn trusted_open_scans_only_the_tail_and_rejects_bad_hints() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("d.spool");
        let mut w = FrameWriter::open(&path, MAGIC_SPOOL).unwrap();
        w.append(&(0..3).map(|i| Record::event(&sample_event(i)).unwrap()).collect::<Vec<_>>()).unwrap();
        w.sync().unwrap();
        let committed = w.len();
        drop(w);
        // Append a torn record after the committed length.
        {
            let mut f = OpenOptions::new().append(true).open(&path).unwrap();
            let mut buf = Vec::new();
            Record::event(&sample_event(9)).unwrap().encode_into(&mut buf);
            f.write_all(&buf[..buf.len() - 3]).unwrap();
        }
        // Good hint: tail scanned, torn record truncated, nothing lost.
        let w = FrameWriter::open_trusted(&path, MAGIC_SPOOL, Some(committed)).unwrap();
        assert_eq!(w.len(), committed);
        drop(w);
        assert_eq!(FrameReader::scan(&path, MAGIC_SPOOL).unwrap().records.len(), 3);
        // Bad hint (inside a record): falls back to a full scan, keeps all 3.
        let w = FrameWriter::open_trusted(&path, MAGIC_SPOOL, Some(committed - 5)).unwrap();
        assert_eq!(w.len(), committed);
        drop(w);
        assert_eq!(FrameReader::scan(&path, MAGIC_SPOOL).unwrap().records.len(), 3);
        // Hint beyond the file: ignored.
        let w = FrameWriter::open_trusted(&path, MAGIC_SPOOL, Some(committed + 1000)).unwrap();
        assert_eq!(w.len(), committed);
    }

    #[test]
    fn rejects_wrong_magic() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.spool");
        FrameWriter::open(&path, MAGIC_SPOOL).unwrap();
        assert!(FrameReader::scan(&path, MAGIC_WAL).is_err());
        assert!(FrameReader::scan(&path, MAGIC_SPOOL).is_ok());
    }
}
