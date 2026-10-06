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

use crate::failpoint;
use crate::format::*;
use crate::{IoAt, Result, StorageError};
use attemptdb_core::codec::{CodecId, decode_event, encode_event, frame_checksum};
use attemptdb_core::schema::CANONICAL_SCHEMA_VERSION;
use attemptdb_core::{Event, Timestamp};
use std::fs::File;
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

/// Refuse a payload the scanner would later treat as corruption. The reader
/// stops at a record over [`MAX_RECORD_PAYLOAD`] and the writer then truncates
/// the file there, taking every later record with it, so the cap has to hold
/// on the write side too, before anything is acknowledged.
pub fn check_payload_len(len: usize) -> Result<()> {
    if len > MAX_RECORD_PAYLOAD as usize {
        return Err(StorageError::RecordTooLarge {
            len,
            max: MAX_RECORD_PAYLOAD as usize,
        });
    }
    Ok(())
}

impl Record {
    /// Frame one event. Fails with [`StorageError::RecordTooLarge`] when the
    /// encoded event does not fit in a record.
    pub fn event(ev: &Event) -> Result<Self> {
        let payload = encode_event(ev)?;
        check_payload_len(payload.len())?;
        Ok(Self {
            record_type: record_type::EVENT,
            codec: CodecId::Json as u8,
            flags: 0,
            payload,
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
        let codec = CodecId::from_u8(self.codec)
            .ok_or_else(|| StorageError::Other(format!("unknown codec id {}", self.codec)))?;
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
    /// I/O failpoint consulted by every append (`wal.write` / `spool.write`).
    fault: &'static str,
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
        // Symlink-safe: the file is created with `create_new` or opened with
        // `O_NOFOLLOW` and checked to be regular, so a planted link in the
        // spool directory can never get truncated or overwritten below.
        let mut file = crate::safe_fs::open_rw(path).at(path)?;
        let file_len = file.metadata().at(path)?.len();
        // A file shorter than its header cannot hold a record. It is what a
        // crash between creating the file and writing the header leaves
        // behind, so it is started over rather than treated as corrupt.
        let exists = file_len >= FILE_HEADER_LEN as u64;
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
            if file_len > 0 {
                file.set_len(0).at(path)?;
            }
            let header = FileHeader::new(magic);
            file.write_all(&header.encode()).at(path)?;
            // A WAL header is part of the durability boundary. A spool file is
            // a transport that is only as durable as `spool_sync` makes its
            // records: syncing every new private file's header cost a hook
            // 5-50 ms under contention (100 parallel hooks: p50 79 -> 269 ms).
            if magic == MAGIC_WAL {
                file.sync_all().at(path)?;
            }
            (header, FILE_HEADER_LEN as u64)
        };
        file.seek(SeekFrom::Start(len)).at(path)?;
        let fault = if magic == MAGIC_WAL {
            failpoint::WAL_WRITE
        } else {
            failpoint::SPOOL_WRITE
        };
        Ok(Self {
            file,
            path: path.to_path_buf(),
            header,
            len,
            fault,
        })
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
    ///
    /// A failed write (`ENOSPC`, `EIO`) can leave a prefix of the batch in
    /// the file. That prefix is discarded before the error is returned, so
    /// the next append starts on a record boundary again; otherwise a torn
    /// record would sit in the middle of the file and every record written
    /// after it would be unreachable to the recovery scan.
    pub fn append(&mut self, records: &[Record]) -> Result<u64> {
        // Refuse the whole batch before a byte is written: a record the
        // reader would call corrupt must never be acknowledged.
        for r in records {
            check_payload_len(r.payload.len())?;
        }
        let start = self.len;
        let mut buf = Vec::with_capacity(records.iter().map(Record::encoded_len).sum());
        for r in records {
            r.encode_into(&mut buf);
        }
        if let Err(e) = self.write_tail(&buf) {
            let _ = self.file.set_len(self.len);
            let _ = self.file.seek(SeekFrom::Start(self.len));
            return Err(StorageError::io(&self.path, e));
        }
        self.len += buf.len() as u64;
        Ok(start)
    }

    fn write_tail(&mut self, buf: &[u8]) -> std::io::Result<()> {
        if let Err(e) = failpoint::io(self.fault) {
            // Model ENOSPC striking mid-batch: a prefix lands, then the error.
            self.file.write_all(&buf[..buf.len() / 2])?;
            return Err(e);
        }
        self.file.write_all(buf)
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
        let file = crate::safe_fs::open_read(path).at(path)?;
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
            records.push(Record {
                record_type,
                codec,
                flags,
                payload,
                offset,
            });
            offset += RECORD_HEADER_LEN as u64 + payload_len as u64;
        }
        Ok(ScanResult {
            header,
            records,
            valid_len: offset,
            truncated_at,
            total_len,
        })
    }
}

/// A byte range of a framed file that belongs to no valid record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DamagedRange {
    /// Offset of the first byte.
    pub offset: u64,
    pub len: u64,
    /// The range is the end of the file and what it starts is a record that
    /// simply continues past the end: the torn tail a crashed append leaves.
    /// Anything else is damage (a flipped bit, overwritten bytes).
    pub torn_tail: bool,
}

/// [`FrameReader::scan_resync`]: every valid record of a file, also those
/// that follow damage, and where the damage is.
#[derive(Debug)]
pub struct ResyncScan {
    pub header: FileHeader,
    pub records: Vec<Record>,
    pub damaged: Vec<DamagedRange>,
    pub total_len: u64,
}

impl ResyncScan {
    /// Damage with intact records after it: bytes in the middle of the file,
    /// as opposed to a torn or damaged tail.
    pub fn mid_file_damage(&self) -> impl Iterator<Item = &DamagedRange> {
        let last_record_end = self
            .records
            .last()
            .map(|r| r.offset + RECORD_HEADER_LEN as u64 + r.payload.len() as u64)
            .unwrap_or(0);
        self.damaged
            .iter()
            .filter(move |d| d.offset < last_record_end)
    }
}

impl FrameReader {
    /// [`FrameReader::scan`] that does not stop at the first record that
    /// fails its checksum: it looks for the next place a valid record starts
    /// and goes on from there, so one flipped byte costs the record it is in
    /// and not every record after it.
    ///
    /// The frame format has no sync marker; it does not need one. A candidate
    /// start must declare a payload within [`MAX_RECORD_PAYLOAD`], carry
    /// `flags = 0`, a known record type and codec, fit in the file, and pass
    /// its CRC-32C: about one chance in 2^50 for bytes that are not a record.
    /// If the damaged record's own length is intact its end is tried first,
    /// which is the usual case (a flipped bit in a payload). Otherwise every
    /// byte after it is a candidate. The common case, a file with no damage,
    /// is the plain scan; only the bytes after the first bad record are read
    /// again.
    pub fn scan_resync(path: &Path, magic: [u8; 4]) -> Result<ResyncScan> {
        let scan = Self::scan(path, magic)?;
        let Some(at) = scan.truncated_at else {
            return Ok(ResyncScan {
                header: scan.header,
                records: scan.records,
                damaged: Vec::new(),
                total_len: scan.total_len,
            });
        };
        let mut file = crate::safe_fs::open_read(path).at(path)?;
        file.seek(SeekFrom::Start(at)).at(path)?;
        let mut tail = Vec::new();
        file.read_to_end(&mut tail).at(path)?;
        let (more, damaged) = resync_bytes(&tail, at);
        let mut records = scan.records;
        records.extend(more);
        Ok(ResyncScan {
            header: scan.header,
            records,
            damaged,
            total_len: scan.total_len,
        })
    }
}

/// A record that starts at `pos` of `bytes`: its end, and the record, when it
/// is complete, well formed and passes its checksum. Stricter than the plain
/// scan on the header fields, because here a false match costs a wrongly
/// resynchronised record rather than a stop.
fn record_at(bytes: &[u8], pos: usize, base: u64, budget: &mut usize) -> Option<(Record, usize)> {
    let head = bytes.get(pos..pos + RECORD_HEADER_LEN)?;
    let payload_len = u32_le(&head[0..4]);
    if payload_len > MAX_RECORD_PAYLOAD {
        return None;
    }
    let (record_type, codec, flags) = (head[8], head[9], u16_le(&head[10..12]));
    if flags != 0
        || !matches!(record_type, record_type::EVENT | record_type::CHECKPOINT)
        || codec != CodecId::Json as u8
    {
        return None;
    }
    let end = pos + RECORD_HEADER_LEN + payload_len as usize;
    let body = bytes.get(pos + 8..end)?;
    // Checking a candidate costs its length. A file made of headers that all
    // look plausible and declare long payloads could otherwise make the search
    // quadratic; the work is capped at a few times the file, and what is left
    // when it runs out is reported as damage.
    if *budget < body.len() {
        return None;
    }
    *budget -= body.len();
    if frame_checksum(body) != u32_le(&head[4..8]) {
        return None;
    }
    *budget += body.len();
    Some((
        Record {
            record_type,
            codec,
            flags,
            payload: bytes[pos + RECORD_HEADER_LEN..end].to_vec(),
            offset: base + pos as u64,
        },
        end,
    ))
}

/// Whether the bytes at `pos` could be the start of a record, judging by the
/// fixed header fields alone (cheap: no checksum).
fn plausible_header(bytes: &[u8], pos: usize) -> bool {
    match bytes.get(pos..pos + RECORD_HEADER_LEN) {
        Some(h) => {
            u16_le(&h[10..12]) == 0
                && matches!(h[8], record_type::EVENT | record_type::CHECKPOINT)
                && h[9] == CodecId::Json as u8
                && u32_le(&h[0..4]) <= MAX_RECORD_PAYLOAD
        }
        None => false,
    }
}

/// The records of `bytes` (the file from offset `base` on), skipping damage.
fn resync_bytes(bytes: &[u8], base: u64) -> (Vec<Record>, Vec<DamagedRange>) {
    let mut records = Vec::new();
    let mut damaged = Vec::new();
    let mut budget = bytes.len().saturating_mul(4).saturating_add(1 << 20);
    let mut pos = 0usize;
    let mut bad_from: Option<usize> = None;
    let close = |bad_from: &mut Option<usize>, until: usize, damaged: &mut Vec<DamagedRange>| {
        if let Some(from) = bad_from.take() {
            damaged.push(DamagedRange {
                offset: base + from as u64,
                len: (until - from) as u64,
                torn_tail: false,
            });
        }
    };
    while pos < bytes.len() {
        if let Some((record, end)) = record_at(bytes, pos, base, &mut budget) {
            close(&mut bad_from, pos, &mut damaged);
            records.push(record);
            pos = end;
            continue;
        }
        if bad_from.is_none() {
            bad_from = Some(pos);
            // The damaged record's own length is usually intact (a flipped
            // bit in its payload): the next record starts right after it.
            if plausible_header(bytes, pos) {
                let end = pos + RECORD_HEADER_LEN + u32_le(&bytes[pos..pos + 4]) as usize;
                if end <= bytes.len() && record_at(bytes, end, base, &mut budget).is_some() {
                    close(&mut bad_from, end, &mut damaged);
                    pos = end;
                    continue;
                }
            }
        }
        // Otherwise the next byte that could start a record.
        pos += 1;
        while pos < bytes.len()
            && !(plausible_header(bytes, pos) && record_at(bytes, pos, base, &mut budget).is_some())
        {
            pos += 1;
        }
    }
    if let Some(from) = bad_from {
        let rest = bytes.len() - from;
        // A torn append: the record that starts here goes on past the end.
        let torn = rest < RECORD_HEADER_LEN
            || (u32_le(&bytes[from..from + 4]) <= MAX_RECORD_PAYLOAD
                && rest < RECORD_HEADER_LEN + u32_le(&bytes[from..from + 4]) as usize
                && plausible_header(bytes, from));
        damaged.push(DamagedRange {
            offset: base + from as u64,
            len: rest as u64,
            torn_tail: torn,
        });
    }
    (records, damaged)
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
    use std::fs::OpenOptions;

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
        let decoded: Vec<Event> = scan
            .records
            .iter()
            .map(|r| r.decode_event().unwrap())
            .collect();
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
            let recs: Vec<Record> = (0..3)
                .map(|i| Record::event(&sample_event(i)).unwrap())
                .collect();
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
        w.append(
            &(0..3)
                .map(|i| Record::event(&sample_event(i)).unwrap())
                .collect::<Vec<_>>(),
        )
        .unwrap();
        w.sync().unwrap();
        let committed = w.len();
        drop(w);
        // Append a torn record after the committed length.
        {
            let mut f = OpenOptions::new().append(true).open(&path).unwrap();
            let mut buf = Vec::new();
            Record::event(&sample_event(9))
                .unwrap()
                .encode_into(&mut buf);
            f.write_all(&buf[..buf.len() - 3]).unwrap();
        }
        // Good hint: tail scanned, torn record truncated, nothing lost.
        let w = FrameWriter::open_trusted(&path, MAGIC_SPOOL, Some(committed)).unwrap();
        assert_eq!(w.len(), committed);
        drop(w);
        assert_eq!(
            FrameReader::scan(&path, MAGIC_SPOOL).unwrap().records.len(),
            3
        );
        // Bad hint (inside a record): falls back to a full scan, keeps all 3.
        let w = FrameWriter::open_trusted(&path, MAGIC_SPOOL, Some(committed - 5)).unwrap();
        assert_eq!(w.len(), committed);
        drop(w);
        assert_eq!(
            FrameReader::scan(&path, MAGIC_SPOOL).unwrap().records.len(),
            3
        );
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
