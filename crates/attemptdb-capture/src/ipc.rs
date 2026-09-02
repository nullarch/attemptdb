//! Local IPC between hook processes / the CLI and the capture daemon.
//!
//! # Transport (RFC 0005 §4.1)
//!
//! - **macOS, Linux**: a Unix domain socket at `<runtime_dir>/attemptdb.sock`.
//!   The runtime directory is created `0700` and must be owned by the
//!   current user; the socket file is `0600`; the daemon additionally checks
//!   the peer uid (`SO_PEERCRED` / `LOCAL_PEERCRED`) on every accept. When
//!   the socket path would not fit `sun_path` (macOS: 104 bytes) both sides
//!   deterministically fall back to `<temp_dir>/attemptdb-<uid>/<hash>.sock`,
//!   so no lookup file is needed on the hook's hot path.
//! - **Windows**: a named pipe `\\.\pipe\attemptdb-<hash>` where `<hash>` is
//!   derived from the runtime directory, which is per-user by construction
//!   (`%LOCALAPPDATA%`). Remote clients are rejected. A DACL restricted to
//!   the user's SID is planned (needs a Windows API binding).
//! - **Loopback TCP**: not implemented in this version. [`Endpoint`] is the
//!   extension point: add a `Tcp` variant, teach [`Client::connect_endpoint`]
//!   and [`Listener::bind`] about it, and carry the RFC token in [`Hello`].
//!
//! # Wire format (RFC 0005 §4.2, with a CRC32C per frame)
//!
//! ```text
//! prelude (8 bytes, client -> daemon, once per connection)
//!   0..4   magic "ATIP"
//!   4..6   protocol_version  u16 LE  (= 1)
//!   6..8   flags             u16 LE  (= 0, reserved)
//!
//! frame (12-byte header + payload, both directions)
//!   0..4   payload_len       u32 LE  (<= 16 MiB)
//!   4..8   crc32c            u32 LE  over (type, codec, flags, payload)
//!   8      type              u8
//!   9      codec             u8      (1 = JSON, shared with WAL/spool records)
//!   10..12 flags             u16 LE  (reserved, 0)
//!   12..   payload
//! ```
//!
//! The frame header is byte-for-byte the WAL record header from
//! `docs/storage-format.md` with the record type reinterpreted as a message
//! type. A receiver rejects a frame whose length exceeds [`MAX_PAYLOAD`]
//! *before* allocating, and one whose CRC does not match.
//!
//! Message types and JSON payloads:
//!
//! | type | name        | direction        | payload |
//! |------|-------------|------------------|---------|
//! | 1    | `HELLO`     | client -> daemon | [`Hello`] |
//! | 2    | `INGEST`    | client -> daemon | JSON array of canonical events (`source_seq = 0`, `hlc = 0`) |
//! | 3    | `ACK`       | daemon -> client | [`IngestAck`] (for `INGEST` and `SHUTDOWN`) |
//! | 4    | `NACK`      | daemon -> client | [`Nack`] |
//! | 5    | `PING`      | client -> daemon | empty |
//! | 6    | `PONG`      | daemon -> client | [`DaemonStatus`] |
//! | 7    | `QUERY`     | client -> daemon | [`ReadRequest`]: a statement or a timeline over a scope, answered from the daemon's resident engine |
//! | 8    | `HELLO_ACK` | daemon -> client | [`HelloAck`] |
//! | 9    | `SHUTDOWN`  | client -> daemon | empty; acknowledged with `ACK` before the daemon flushes and exits |
//! | 10   | `RESULT`    | daemon -> client | [`ReadResponse`] (for `QUERY`); Arrow IPC rows travel base64-encoded inside the JSON |
//!
//! `QUERY` needs a prior `HELLO` on the connection (the database directory
//! must match) and a daemon started with a read service; a daemon without
//! one answers `NACK read_unavailable`, and a result over [`MAX_PAYLOAD`]
//! is `NACK result_too_large` — the client then opens the database itself.
//!
//! A hook writes the prelude, `HELLO` and one `INGEST` in a single write, then
//! reads `HELLO_ACK` and `ACK`: one round trip. `ACK` is sent only after the
//! writer's WAL durability policy is satisfied. `INGEST` without a prior
//! `HELLO` is refused (`hello_required`) because `HELLO` carries the database
//! directory the hook resolved; a daemon serving a different database answers
//! `wrong_database` and the hook spools locally instead.
//!
//! The client half is synchronous `std` (it runs inside the hook process, no
//! async runtime); the server half is `tokio`.

use crate::locator::Locator;
use attemptdb_core::codec::{CodecId, frame_checksum};
use attemptdb_core::{DeviceId, Event, EventId, Timestamp};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Magic bytes of the connection prelude.
pub const MAGIC: [u8; 4] = *b"ATIP";
/// Protocol version spoken by this build.
pub const PROTOCOL_VERSION: u16 = 1;
/// Length of the connection prelude.
pub const PRELUDE_LEN: usize = 8;
/// Length of a frame header.
pub const FRAME_HEADER_LEN: usize = 12;
/// Largest payload a receiver accepts (16 MiB).
pub const MAX_PAYLOAD: u32 = 16 * 1024 * 1024;
/// Codec id of JSON payloads (shared with the WAL/spool codec space).
pub const CODEC_JSON: u8 = CodecId::Json as u8;

/// Socket file name under the runtime directory (Unix).
pub const SOCKET_FILE: &str = "attemptdb.sock";
/// Pid file name under the runtime directory.
pub const PID_FILE: &str = "attemptdb.pid";
/// Endpoint record the daemon publishes (RFC 0005 §4.1). Diagnostic only:
/// clients compute the endpoint deterministically and never read this file
/// on the hook's hot path.
pub const ENDPOINT_FILE: &str = "endpoint.json";

/// Default budget for establishing the connection.
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_millis(25);
/// Default budget for the whole request/response exchange after connecting.
pub const DEFAULT_ROUNDTRIP_TIMEOUT: Duration = Duration::from_millis(100);

/// Longest Unix socket path we are willing to bind. macOS allows 104 bytes
/// including the terminating NUL, Linux 108; stay under both with margin.
const MAX_SUN_PATH: usize = 100;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum IpcError {
    #[error("daemon is not running")]
    NotRunning,
    #[error("timed out waiting for the daemon")]
    Timeout,
    #[error("connection closed by peer")]
    Closed,
    #[error("io error: {0}")]
    Io(#[from] io::Error),
    #[error("bad prelude magic (expected ATIP)")]
    BadMagic,
    #[error("unsupported protocol version {0} (this build speaks {PROTOCOL_VERSION})")]
    UnsupportedProtocol(u16),
    #[error("frame checksum mismatch (expected {expected:#010x}, found {found:#010x})")]
    CrcMismatch { expected: u32, found: u32 },
    #[error("frame payload of {0} bytes exceeds the {MAX_PAYLOAD} byte limit")]
    FrameTooLarge(u32),
    #[error("unsupported payload codec {0}")]
    UnsupportedCodec(u8),
    #[error("unexpected message type {0}")]
    Unexpected(u8),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("daemon refused the request: {0}")]
    Nack(Nack),
}

impl IpcError {
    /// Whether the error means "no daemon is listening" rather than a
    /// protocol or transport failure while talking to one.
    pub fn is_not_running(&self) -> bool {
        matches!(self, IpcError::NotRunning)
    }
}

pub type IpcResult<T> = std::result::Result<T, IpcError>;

fn map_io(e: io::Error) -> IpcError {
    match e.kind() {
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => IpcError::Timeout,
        io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound => IpcError::NotRunning,
        _ => IpcError::Io(e),
    }
}

/// [`map_io`] with the failing operation named, so a hook log line says
/// *what* failed (`connect`, `write`, `read header`, ...).
fn map_io_at(op: &'static str) -> impl Fn(io::Error) -> IpcError {
    move |e| match map_io(e) {
        IpcError::Io(e) => IpcError::Io(io::Error::new(e.kind(), format!("{op}: {e}"))),
        other => other,
    }
}

// ---------------------------------------------------------------------------
// Message types and frames
// ---------------------------------------------------------------------------

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MsgType {
    Hello = 1,
    Ingest = 2,
    Ack = 3,
    Nack = 4,
    Ping = 5,
    Pong = 6,
    Query = 7,
    HelloAck = 8,
    Shutdown = 9,
    Result = 10,
}

impl MsgType {
    pub fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            1 => MsgType::Hello,
            2 => MsgType::Ingest,
            3 => MsgType::Ack,
            4 => MsgType::Nack,
            5 => MsgType::Ping,
            6 => MsgType::Pong,
            7 => MsgType::Query,
            8 => MsgType::HelloAck,
            9 => MsgType::Shutdown,
            10 => MsgType::Result,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            MsgType::Hello => "HELLO",
            MsgType::Ingest => "INGEST",
            MsgType::Ack => "ACK",
            MsgType::Nack => "NACK",
            MsgType::Ping => "PING",
            MsgType::Pong => "PONG",
            MsgType::Query => "QUERY",
            MsgType::HelloAck => "HELLO_ACK",
            MsgType::Shutdown => "SHUTDOWN",
            MsgType::Result => "RESULT",
        }
    }
}

/// Encode the connection prelude.
pub fn encode_prelude(protocol_version: u16, flags: u16) -> [u8; PRELUDE_LEN] {
    let mut b = [0u8; PRELUDE_LEN];
    b[0..4].copy_from_slice(&MAGIC);
    b[4..6].copy_from_slice(&protocol_version.to_le_bytes());
    b[6..8].copy_from_slice(&flags.to_le_bytes());
    b
}

/// Decode the connection prelude into `(protocol_version, flags)`.
pub fn decode_prelude(b: &[u8; PRELUDE_LEN]) -> IpcResult<(u16, u16)> {
    if b[0..4] != MAGIC {
        return Err(IpcError::BadMagic);
    }
    Ok((
        u16::from_le_bytes([b[4], b[5]]),
        u16::from_le_bytes([b[6], b[7]]),
    ))
}

/// One protocol frame. `msg_type` is kept raw so unknown types can be
/// represented (and answered with a `NACK`) instead of failing to decode.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub msg_type: u8,
    pub codec: u8,
    pub flags: u16,
    pub payload: Vec<u8>,
}

fn checksum(msg_type: u8, codec: u8, flags: u16, payload: &[u8]) -> u32 {
    let mut buf = Vec::with_capacity(payload.len() + 4);
    buf.push(msg_type);
    buf.push(codec);
    buf.extend_from_slice(&flags.to_le_bytes());
    buf.extend_from_slice(payload);
    frame_checksum(&buf)
}

impl Frame {
    pub fn new(msg_type: MsgType, payload: Vec<u8>) -> Self {
        Self {
            msg_type: msg_type as u8,
            codec: CODEC_JSON,
            flags: 0,
            payload,
        }
    }

    pub fn empty(msg_type: MsgType) -> Self {
        Self::new(msg_type, Vec::new())
    }

    pub fn json<T: Serialize>(msg_type: MsgType, value: &T) -> IpcResult<Self> {
        Ok(Self::new(msg_type, serde_json::to_vec(value)?))
    }

    pub fn kind(&self) -> Option<MsgType> {
        MsgType::from_u8(self.msg_type)
    }

    /// Decode the JSON payload. Fails for non-JSON codecs.
    pub fn parse_json<T: DeserializeOwned>(&self) -> IpcResult<T> {
        if self.codec != CODEC_JSON {
            return Err(IpcError::UnsupportedCodec(self.codec));
        }
        Ok(serde_json::from_slice(&self.payload)?)
    }

    /// Header + payload as one buffer.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(FRAME_HEADER_LEN + self.payload.len());
        self.encode_into(&mut out);
        out
    }

    pub fn encode_into(&self, out: &mut Vec<u8>) {
        let crc = checksum(self.msg_type, self.codec, self.flags, &self.payload);
        out.extend_from_slice(&(self.payload.len() as u32).to_le_bytes());
        out.extend_from_slice(&crc.to_le_bytes());
        out.push(self.msg_type);
        out.push(self.codec);
        out.extend_from_slice(&self.flags.to_le_bytes());
        out.extend_from_slice(&self.payload);
    }

    /// Decode the header; returns `(payload_len, crc, msg_type, codec, flags)`.
    fn decode_header(h: &[u8; FRAME_HEADER_LEN]) -> IpcResult<(usize, u32, u8, u8, u16)> {
        let len = u32::from_le_bytes([h[0], h[1], h[2], h[3]]);
        if len > MAX_PAYLOAD {
            return Err(IpcError::FrameTooLarge(len));
        }
        let crc = u32::from_le_bytes([h[4], h[5], h[6], h[7]]);
        Ok((
            len as usize,
            crc,
            h[8],
            h[9],
            u16::from_le_bytes([h[10], h[11]]),
        ))
    }

    fn assemble(
        crc: u32,
        msg_type: u8,
        codec: u8,
        flags: u16,
        payload: Vec<u8>,
    ) -> IpcResult<Self> {
        let expected = checksum(msg_type, codec, flags, &payload);
        if expected != crc {
            return Err(IpcError::CrcMismatch {
                expected,
                found: crc,
            });
        }
        Ok(Self {
            msg_type,
            codec,
            flags,
            payload,
        })
    }

    /// Decode one frame from a buffer. `Ok(None)` means more bytes are
    /// needed; `Ok(Some((frame, consumed)))` on success.
    pub fn decode(buf: &[u8]) -> IpcResult<Option<(Self, usize)>> {
        if buf.len() < FRAME_HEADER_LEN {
            return Ok(None);
        }
        let mut h = [0u8; FRAME_HEADER_LEN];
        h.copy_from_slice(&buf[..FRAME_HEADER_LEN]);
        let (len, crc, msg_type, codec, flags) = Self::decode_header(&h)?;
        let total = FRAME_HEADER_LEN + len;
        if buf.len() < total {
            return Ok(None);
        }
        let frame = Self::assemble(
            crc,
            msg_type,
            codec,
            flags,
            buf[FRAME_HEADER_LEN..total].to_vec(),
        )?;
        Ok(Some((frame, total)))
    }

    /// Blocking read of one frame. [`IpcError::Closed`] when the peer closed
    /// the connection cleanly between frames.
    pub fn read_from<R: Read>(r: &mut R) -> IpcResult<Self> {
        let mut h = [0u8; FRAME_HEADER_LEN];
        read_full(r, &mut h)?;
        let (len, crc, msg_type, codec, flags) = Self::decode_header(&h)?;
        let mut payload = vec![0u8; len];
        r.read_exact(&mut payload)?;
        Self::assemble(crc, msg_type, codec, flags, payload)
    }

    pub fn write_to<W: Write>(&self, w: &mut W) -> io::Result<()> {
        w.write_all(&self.encode())
    }

    /// Async read of one frame (see [`Frame::read_from`]).
    pub async fn read_async<R: AsyncRead + Unpin>(r: &mut R) -> IpcResult<Self> {
        let mut h = [0u8; FRAME_HEADER_LEN];
        read_full_async(r, &mut h).await?;
        let (len, crc, msg_type, codec, flags) = Self::decode_header(&h)?;
        let mut payload = vec![0u8; len];
        r.read_exact(&mut payload).await?;
        Self::assemble(crc, msg_type, codec, flags, payload)
    }

    pub async fn write_async<W: AsyncWrite + Unpin>(&self, w: &mut W) -> io::Result<()> {
        w.write_all(&self.encode()).await?;
        w.flush().await
    }
}

/// `read_exact` that distinguishes a clean close before the first byte
/// ([`IpcError::Closed`]) from a torn frame (`UnexpectedEof`).
fn read_full<R: Read>(r: &mut R, buf: &mut [u8]) -> IpcResult<()> {
    let mut got = 0;
    while got < buf.len() {
        match r.read(&mut buf[got..]) {
            Ok(0) if got == 0 => return Err(IpcError::Closed),
            Ok(0) => return Err(io::Error::from(io::ErrorKind::UnexpectedEof).into()),
            Ok(n) => got += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

async fn read_full_async<R: AsyncRead + Unpin>(r: &mut R, buf: &mut [u8]) -> IpcResult<()> {
    let mut got = 0;
    while got < buf.len() {
        match r.read(&mut buf[got..]).await {
            Ok(0) if got == 0 => return Err(IpcError::Closed),
            Ok(0) => return Err(io::Error::from(io::ErrorKind::UnexpectedEof).into()),
            Ok(n) => got += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Payloads
// ---------------------------------------------------------------------------

/// `HELLO` payload (client -> daemon).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    /// `hook`, `cli`, `mcp`, `ui`, ...
    pub client: String,
    pub client_version: String,
    #[serde(default = "default_protocol_version")]
    pub protocol_version: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_id: Option<DeviceId>,
    /// The live database directory the client resolved. The daemon refuses
    /// ingestion for any other database (`wrong_database`).
    pub db_dir: PathBuf,
    /// The client has (or suspects) pending spool data; the daemon imports
    /// promptly instead of waiting for the periodic sweep.
    #[serde(default)]
    pub spooled: bool,
    /// Loopback TCP token (planned; ignored by this version).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    #[serde(flatten, default)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

fn default_protocol_version() -> u16 {
    PROTOCOL_VERSION
}

impl Hello {
    pub fn new(client: &str, db_dir: &Path, device_id: Option<DeviceId>) -> Self {
        Self {
            client: client.to_string(),
            client_version: env!("CARGO_PKG_VERSION").to_string(),
            protocol_version: PROTOCOL_VERSION,
            device_id,
            db_dir: db_dir.to_path_buf(),
            spooled: false,
            token: None,
            extra: Default::default(),
        }
    }
}

/// `HELLO_ACK` payload (daemon -> client).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelloAck {
    pub daemon_version: String,
    pub protocol_version: u16,
    pub pid: u32,
    pub db_id: uuid::Uuid,
    pub device_id: DeviceId,
    pub schema_version: u16,
    pub format_version: u16,
    pub capture_mode: String,
    pub db_dir: PathBuf,
    #[serde(flatten, default)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// One event the daemon refused inside an otherwise accepted batch.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rejected {
    pub event_id: EventId,
    pub reason: String,
}

/// `ACK` payload. Sent only after the WAL durability policy is satisfied for
/// every accepted event. A client treats `duplicate` as success: the event
/// is already durable.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IngestAck {
    #[serde(default)]
    pub accepted: Vec<EventId>,
    #[serde(default)]
    pub duplicate: Vec<EventId>,
    #[serde(default)]
    pub rejected: Vec<Rejected>,
    /// Highest `source_seq` known to be durable on disk.
    #[serde(default)]
    pub durable_source_seq: u64,
}

impl IngestAck {
    /// Every event in the batch is durable (accepted or already known).
    pub fn all_durable(&self) -> bool {
        self.rejected.is_empty()
    }
}

/// `NACK` payload.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Nack {
    pub code: String,
    pub message: String,
    pub retryable: bool,
}

impl Nack {
    pub fn new(code: &str, message: impl Into<String>, retryable: bool) -> Self {
        Self {
            code: code.to_string(),
            message: message.into(),
            retryable,
        }
    }
}

impl fmt::Display for Nack {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.message, self.code)
    }
}

/// The scope of a `QUERY`: what `attempt`'s `--project`, `--all-projects`,
/// `--session`, `--since`, `--until` and `--captured-only` flags say, plus
/// the repository the client runs in (its logical root and normalised
/// remote) for the default per-repository scope. Names and specs are
/// resolved by the daemon against the database's facts.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReadScope {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(default)]
    pub all_projects: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// Microseconds since the Unix epoch, inclusive.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since_micros: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until_micros: Option<i64>,
    #[serde(default)]
    pub captured_only: bool,
    /// The client's repository, when it runs inside one: the default scope
    /// unless `project` or `all_projects` says otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_root: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_remote: Option<String>,
}

/// What a `QUERY` asks for.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReadKind {
    /// Run `statement` (SQL or AttemptQL); rows come back as Arrow IPC.
    Query,
    /// The projection, trimmed to the newest `session_limit` sessions.
    Timeline,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReadRequest {
    pub kind: ReadKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub statement: Option<String>,
    #[serde(default)]
    pub scope: ReadScope,
    /// Timeline: how many sessions to keep (newest first); `None` keeps all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_limit: Option<usize>,
    /// Timeline: keep sessions with no prompt and no tool call too.
    #[serde(default)]
    pub all_sessions: bool,
}

/// Counts of the whole projection, for a timeline that was trimmed.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProjectionTotals {
    pub sessions: usize,
    pub turns: usize,
    pub attempts: usize,
    pub handoffs: usize,
    /// Sessions the timeline would list before the limit (with a prompt
    /// or a tool call, or all of them with `all_sessions`).
    #[serde(default)]
    pub listed: usize,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReadResponse {
    /// Events in the scope the answer was computed over.
    pub event_count: usize,
    /// `rows`, `explanation` or `empty` for a query.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_kind: Option<String>,
    #[serde(default)]
    pub notes: Vec<String>,
    /// The result rows as an Arrow IPC stream, base64 (standard alphabet,
    /// padded).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arrow_ipc_base64: Option<String>,
    /// The (trimmed) projection as `attemptdb_project::Projection` JSON.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub projection: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub totals: Option<ProjectionTotals>,
}

/// Base64 (standard alphabet, padded) without a dependency: results are
/// bounded by the statement's row limit, so 4/3 is the whole cost.
pub fn base64_encode(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            T[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

pub fn base64_decode(text: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => u32::from(c - b'A'),
            b'a'..=b'z' => u32::from(c - b'a') + 26,
            b'0'..=b'9' => u32::from(c - b'0') + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        })
    }
    let bytes = text.as_bytes();
    if !bytes.len().is_multiple_of(4) {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for chunk in bytes.chunks(4) {
        let pad = chunk.iter().rev().take_while(|&&c| c == b'=').count();
        if pad > 2 {
            return None;
        }
        let mut n = 0u32;
        for (i, &c) in chunk.iter().enumerate() {
            let v = if c == b'=' && i >= 4 - pad {
                0
            } else {
                val(c)?
            };
            n = (n << 6) | v;
        }
        out.push((n >> 16) as u8);
        if pad < 2 {
            out.push((n >> 8) as u8);
        }
        if pad < 1 {
            out.push(n as u8);
        }
    }
    Some(out)
}

/// `PONG` payload: a snapshot of the daemon.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonStatus {
    pub pid: u32,
    pub version: String,
    pub protocol_version: u16,
    /// Human-readable endpoint (`unix:/path` or `pipe:\\.\pipe\name`).
    pub endpoint: String,
    pub db_dir: PathBuf,
    pub data_dir: PathBuf,
    pub log_path: PathBuf,
    pub device_id: DeviceId,
    pub capture_mode: String,
    pub durability: String,
    pub started_at: Timestamp,
    pub uptime_secs: u64,
    pub connections: u64,
    pub rejected_connections: u64,
    pub batches: u64,
    /// WAL appends (each one fsync under strict durability); lower than
    /// `batches` when concurrent batches were group-committed.
    #[serde(default)]
    pub wal_commits: u64,
    pub events_ingested: u64,
    pub duplicates: u64,
    pub rejected_events: u64,
    pub spool_files_imported: u64,
    pub spool_events_imported: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_spool_import_at: Option<Timestamp>,
    pub spool_pending: bool,
    pub flushes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_flush_at: Option<Timestamp>,
    pub last_source_seq: u64,
    pub generation: u64,
    pub segments: u64,
    pub memtable_rows: u64,
    #[serde(flatten, default)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// Contents of `<runtime_dir>/endpoint.json` (RFC 0005 §4.1).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EndpointRecord {
    #[serde(flatten)]
    pub endpoint: Endpoint,
    pub protocol_version: u16,
    pub pid: u32,
}

// ---------------------------------------------------------------------------
// Endpoints
// ---------------------------------------------------------------------------

/// Where the daemon listens. Computed deterministically from the locator so
/// hooks never need to read a lookup file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "transport", rename_all = "snake_case")]
pub enum Endpoint {
    Unix { path: PathBuf },
    NamedPipe { name: String },
    // Planned: `Tcp { addr: SocketAddr, token_path: PathBuf }` (RFC 0005 §4.1).
}

impl Endpoint {
    /// One `stat`: is there something at the endpoint right now? On Unix a
    /// stale socket file after a crash also answers yes; the connect that
    /// follows then fails fast with `ECONNREFUSED`. On Windows the metadata
    /// call briefly opens a client handle on the pipe, which the server sees
    /// as an empty connection and ignores.
    pub fn is_present(&self) -> bool {
        match self {
            Endpoint::Unix { path } => {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::FileTypeExt;
                    std::fs::metadata(path)
                        .map(|m| m.file_type().is_socket())
                        .unwrap_or(false)
                }
                #[cfg(not(unix))]
                {
                    let _ = path;
                    false
                }
            }
            Endpoint::NamedPipe { name } => std::fs::metadata(name).is_ok(),
        }
    }

    /// Path of the socket file when the transport has one on disk.
    pub fn socket_path(&self) -> Option<&Path> {
        match self {
            Endpoint::Unix { path } => Some(path),
            Endpoint::NamedPipe { .. } => None,
        }
    }
}

impl fmt::Display for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Endpoint::Unix { path } => write!(f, "unix:{}", path.display()),
            Endpoint::NamedPipe { name } => write!(f, "pipe:{name}"),
        }
    }
}

/// The endpoint for a locator's runtime directory.
pub fn endpoint(locator: &Locator) -> Endpoint {
    endpoint_for_runtime_dir(&locator.paths.runtime_dir)
}

/// Deterministic endpoint for a runtime directory (see the module docs for
/// the platform rules).
pub fn endpoint_for_runtime_dir(runtime_dir: &Path) -> Endpoint {
    if cfg!(windows) {
        let key = runtime_dir.to_string_lossy().to_lowercase();
        return Endpoint::NamedPipe {
            name: format!(r"\\.\pipe\attemptdb-{}", short_hash(key.as_bytes())),
        };
    }
    let path = runtime_dir.join(SOCKET_FILE);
    if path.as_os_str().len() <= MAX_SUN_PATH {
        return Endpoint::Unix { path };
    }
    let fallback = std::env::temp_dir()
        .join(format!(
            "attemptdb-{}",
            current_uid()
                .map(|u| u.to_string())
                .unwrap_or_else(|| "user".into())
        ))
        .join(format!(
            "{}.sock",
            short_hash(runtime_dir.as_os_str().as_encoded_bytes())
        ));
    Endpoint::Unix { path: fallback }
}

fn short_hash(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(bytes);
    digest[..8].iter().map(|b| format!("{b:02x}")).collect()
}

/// `<runtime_dir>/attemptdb.pid`.
pub fn pid_path(locator: &Locator) -> PathBuf {
    locator.paths.runtime_dir.join(PID_FILE)
}

/// `<runtime_dir>/endpoint.json`.
pub fn endpoint_record_path(locator: &Locator) -> PathBuf {
    locator.paths.runtime_dir.join(ENDPOINT_FILE)
}

/// Cheap presence check for the hook hot path: exactly one `stat`, no
/// connection attempt. `true` does not guarantee the daemon answers.
pub fn daemon_reachable(locator: &Locator) -> bool {
    endpoint(locator).is_present()
}

/// Current numeric user id (Unix only).
pub fn current_uid() -> Option<u32> {
    #[cfg(unix)]
    {
        // SAFETY: getuid has no preconditions and cannot fail.
        Some(unsafe { libc::getuid() })
    }
    #[cfg(not(unix))]
    {
        None
    }
}

// ---------------------------------------------------------------------------
// Synchronous client (hook process, CLI)
// ---------------------------------------------------------------------------

/// Budgets for one client exchange.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timeouts {
    /// Budget for establishing the connection. Unix socket connects either
    /// succeed immediately or fail with `ECONNREFUSED`; the budget only
    /// matters when the daemon's accept backlog is full.
    pub connect: Duration,
    /// Budget for everything after connect (writes and reads combined).
    pub roundtrip: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            connect: DEFAULT_CONNECT_TIMEOUT,
            roundtrip: DEFAULT_ROUNDTRIP_TIMEOUT,
        }
    }
}

impl Timeouts {
    /// Generous budgets for interactive CLI use (status, stop).
    pub fn interactive() -> Self {
        Self {
            connect: Duration::from_millis(250),
            roundtrip: Duration::from_secs(5),
        }
    }

    /// A read from the daemon's engine: a view over a large database may
    /// take a second to build the first time.
    pub fn read() -> Self {
        Self {
            connect: Duration::from_millis(250),
            roundtrip: Duration::from_secs(30),
        }
    }
}

enum Stream {
    #[cfg(unix)]
    Unix(std::os::unix::net::UnixStream),
    #[cfg(windows)]
    Pipe(std::fs::File),
}

impl Stream {
    fn set_read_timeout(&self, d: Option<Duration>) -> io::Result<()> {
        match self {
            #[cfg(unix)]
            Stream::Unix(s) => s.set_read_timeout(d),
            // Synchronous pipe handles have no per-read timeout without
            // overlapped I/O; the daemon answers or closes.
            #[cfg(windows)]
            Stream::Pipe(_) => {
                let _ = d;
                Ok(())
            }
        }
    }

    fn set_write_timeout(&self, d: Option<Duration>) -> io::Result<()> {
        match self {
            #[cfg(unix)]
            Stream::Unix(s) => s.set_write_timeout(d),
            #[cfg(windows)]
            Stream::Pipe(_) => {
                let _ = d;
                Ok(())
            }
        }
    }
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            #[cfg(unix)]
            Stream::Unix(s) => s.read(buf),
            #[cfg(windows)]
            Stream::Pipe(f) => f.read(buf),
        }
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            #[cfg(unix)]
            Stream::Unix(s) => s.write(buf),
            #[cfg(windows)]
            Stream::Pipe(f) => f.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            #[cfg(unix)]
            Stream::Unix(s) => s.flush(),
            #[cfg(windows)]
            Stream::Pipe(f) => f.flush(),
        }
    }
}

/// A synchronous connection to the daemon. Every operation is bounded by the
/// deadline fixed at connect time; the hook never waits longer than
/// `connect + roundtrip`.
pub struct Client {
    stream: Stream,
    deadline: Instant,
    prelude_sent: bool,
}

impl Client {
    /// Connect to the daemon for `locator`. Fails fast with
    /// [`IpcError::NotRunning`] (one `stat`) when nothing is listening.
    pub fn connect(locator: &Locator, timeouts: Timeouts) -> IpcResult<Self> {
        Self::connect_endpoint(&endpoint(locator), timeouts)
    }

    pub fn connect_endpoint(endpoint: &Endpoint, timeouts: Timeouts) -> IpcResult<Self> {
        if !endpoint.is_present() {
            return Err(IpcError::NotRunning);
        }
        let start = Instant::now();
        let stream = match endpoint {
            Endpoint::Unix { path } => {
                #[cfg(unix)]
                {
                    Stream::Unix(
                        std::os::unix::net::UnixStream::connect(path)
                            .map_err(map_io_at("connect"))?,
                    )
                }
                #[cfg(not(unix))]
                {
                    let _ = path;
                    return Err(IpcError::NotRunning);
                }
            }
            Endpoint::NamedPipe { name } => {
                #[cfg(windows)]
                {
                    Stream::Pipe(
                        std::fs::OpenOptions::new()
                            .read(true)
                            .write(true)
                            .open(name)
                            .map_err(map_io_at("open pipe"))?,
                    )
                }
                #[cfg(not(windows))]
                {
                    let _ = name;
                    return Err(IpcError::NotRunning);
                }
            }
        };
        if start.elapsed() > timeouts.connect {
            return Err(IpcError::Timeout);
        }
        // Every read/write is bounded by the round-trip budget from here on;
        // the per-operation re-arming below only tightens it.
        stream
            .set_read_timeout(Some(timeouts.roundtrip))
            .map_err(map_io_at("set read timeout"))?;
        stream
            .set_write_timeout(Some(timeouts.roundtrip))
            .map_err(map_io_at("set write timeout"))?;
        Ok(Self {
            stream,
            deadline: Instant::now() + timeouts.roundtrip,
            prelude_sent: false,
        })
    }

    fn remaining(&self) -> IpcResult<Duration> {
        let now = Instant::now();
        if now >= self.deadline {
            return Err(IpcError::Timeout);
        }
        Ok(self.deadline - now)
    }

    /// Tighten a socket timeout to what is left of the deadline. Best effort:
    /// macOS answers `setsockopt` with `EINVAL` once the peer has closed the
    /// connection, even though buffered data is still readable, and the
    /// timeout set at connect time still bounds the operation.
    fn arm(&self, rem: Duration, read: bool) {
        let _ = if read {
            self.stream.set_read_timeout(Some(rem))
        } else {
            self.stream.set_write_timeout(Some(rem))
        };
    }

    fn send_bytes(&mut self, bytes: &[u8]) -> IpcResult<()> {
        let rem = self.remaining()?;
        self.arm(rem, false);
        self.stream.write_all(bytes).map_err(map_io_at("write"))?;
        self.stream.flush().map_err(map_io_at("flush"))?;
        Ok(())
    }

    fn prelude_bytes(&mut self, out: &mut Vec<u8>) {
        if !self.prelude_sent {
            out.extend_from_slice(&encode_prelude(PROTOCOL_VERSION, 0));
            self.prelude_sent = true;
        }
    }

    fn send_frame(&mut self, frame: &Frame) -> IpcResult<()> {
        let mut buf = Vec::with_capacity(PRELUDE_LEN + FRAME_HEADER_LEN + frame.payload.len());
        self.prelude_bytes(&mut buf);
        frame.encode_into(&mut buf);
        self.send_bytes(&buf)
    }

    fn recv_frame(&mut self) -> IpcResult<Frame> {
        // Re-arm the timeout before each read so a slow peer cannot stretch
        // the exchange past the deadline.
        let rem = self.remaining()?;
        self.arm(rem, true);
        let mut h = [0u8; FRAME_HEADER_LEN];
        read_full(&mut self.stream, &mut h).map_err(map_ipc_io("read header"))?;
        let (len, crc, msg_type, codec, flags) = Frame::decode_header(&h)?;
        let rem = self.remaining()?;
        self.arm(rem, true);
        let mut payload = vec![0u8; len];
        self.stream
            .read_exact(&mut payload)
            .map_err(map_io_at("read payload"))?;
        Frame::assemble(crc, msg_type, codec, flags, payload)
    }

    /// Read a frame and require `expected`; a `NACK` becomes
    /// [`IpcError::Nack`], anything else [`IpcError::Unexpected`].
    fn expect(&mut self, expected: MsgType) -> IpcResult<Frame> {
        let frame = self.recv_frame()?;
        match frame.kind() {
            Some(t) if t == expected => Ok(frame),
            Some(MsgType::Nack) => Err(IpcError::Nack(frame.parse_json()?)),
            _ => Err(IpcError::Unexpected(frame.msg_type)),
        }
    }

    pub fn hello(&mut self, hello: &Hello) -> IpcResult<HelloAck> {
        self.send_frame(&Frame::json(MsgType::Hello, hello)?)?;
        self.expect(MsgType::HelloAck)?.parse_json()
    }

    /// Send one batch (requires a prior [`Client::hello`] on this connection).
    pub fn ingest(&mut self, events: &[Event]) -> IpcResult<IngestAck> {
        self.send_frame(&Frame::json(MsgType::Ingest, &events)?)?;
        self.expect(MsgType::Ack)?.parse_json()
    }

    pub fn ping(&mut self) -> IpcResult<DaemonStatus> {
        self.send_frame(&Frame::empty(MsgType::Ping))?;
        self.expect(MsgType::Pong)?.parse_json()
    }

    /// Ask the daemon's resident engine (requires a prior [`Client::hello`]).
    pub fn query(&mut self, req: &ReadRequest) -> IpcResult<ReadResponse> {
        self.send_frame(&Frame::json(MsgType::Query, req)?)?;
        self.expect(MsgType::Result)?.parse_json()
    }

    /// One read from the daemon serving `locator`'s database: connect,
    /// `HELLO`, `QUERY`. Any failure — no daemon, another database, no read
    /// service, a result too large — is the error; callers open the
    /// database themselves then.
    pub fn read(locator: &Locator, req: &ReadRequest) -> IpcResult<ReadResponse> {
        Self::read_with(locator, req, Timeouts::read())
    }

    pub fn read_with(
        locator: &Locator,
        req: &ReadRequest,
        timeouts: Timeouts,
    ) -> IpcResult<ReadResponse> {
        let mut client = Self::connect(locator, timeouts)?;
        client.hello(&Hello::new("cli", &locator.db_dir, None))?;
        client.query(req)
    }

    /// Ask the daemon to flush and exit. Returns once the daemon acknowledged
    /// the request (the flush happens after the acknowledgement).
    pub fn shutdown(&mut self) -> IpcResult<()> {
        self.send_frame(&Frame::empty(MsgType::Shutdown))?;
        self.expect(MsgType::Ack)?;
        Ok(())
    }

    /// The hook path: connect, write prelude + `HELLO` + `INGEST` in a single
    /// write, read `HELLO_ACK` + `ACK`. One round trip, bounded by the
    /// default [`Timeouts`].
    pub fn send_events(locator: &Locator, events: &[Event]) -> IpcResult<IngestAck> {
        Self::send_events_with(locator, events, Timeouts::default())
    }

    pub fn send_events_with(
        locator: &Locator,
        events: &[Event],
        timeouts: Timeouts,
    ) -> IpcResult<IngestAck> {
        let mut client = Self::connect(locator, timeouts)?;
        let hello = Hello::new("hook", &locator.db_dir, events.first().map(|e| e.device_id));
        let hello_frame = Frame::json(MsgType::Hello, &hello)?;
        let ingest_frame = Frame::json(MsgType::Ingest, &events)?;
        let mut buf = Vec::with_capacity(
            PRELUDE_LEN
                + 2 * FRAME_HEADER_LEN
                + hello_frame.payload.len()
                + ingest_frame.payload.len(),
        );
        client.prelude_bytes(&mut buf);
        hello_frame.encode_into(&mut buf);
        ingest_frame.encode_into(&mut buf);
        client.send_bytes(&buf)?;
        client.expect(MsgType::HelloAck)?;
        let ack: IngestAck = client.expect(MsgType::Ack)?.parse_json()?;
        if !ack.all_durable() {
            // Partial acceptance: report it as a NACK so the caller spools
            // the whole batch (duplicates are harmless on import).
            let reasons: Vec<String> = ack
                .rejected
                .iter()
                .map(|r| format!("{}: {}", r.event_id, r.reason))
                .collect();
            return Err(IpcError::Nack(Nack::new(
                "partially_rejected",
                reasons.join("; "),
                false,
            )));
        }
        Ok(ack)
    }

    /// `PING` with interactive timeouts.
    pub fn status(locator: &Locator) -> IpcResult<DaemonStatus> {
        Self::status_with(locator, Timeouts::interactive())
    }

    pub fn status_with(locator: &Locator, timeouts: Timeouts) -> IpcResult<DaemonStatus> {
        Self::connect(locator, timeouts)?.ping()
    }

    /// `SHUTDOWN` with interactive timeouts.
    pub fn request_shutdown(locator: &Locator) -> IpcResult<()> {
        Self::connect(locator, Timeouts::interactive())?.shutdown()
    }
}

fn map_ipc_io(op: &'static str) -> impl Fn(IpcError) -> IpcError {
    move |e| match e {
        IpcError::Io(io) => map_io_at(op)(io),
        other => other,
    }
}

// ---------------------------------------------------------------------------
// Async server side (daemon)
// ---------------------------------------------------------------------------

/// Object-safe async byte stream.
pub trait AsyncStream: AsyncRead + AsyncWrite + Send + Unpin {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin> AsyncStream for T {}

/// An accepted connection.
pub struct Connection {
    /// Peer uid where the OS reports it (Unix); `None` on Windows.
    pub peer_uid: Option<u32>,
    pub stream: Box<dyn AsyncStream>,
}

enum ListenerInner {
    #[cfg(unix)]
    Unix(tokio::net::UnixListener),
    #[cfg(windows)]
    Pipe {
        name: String,
        next: Option<tokio::net::windows::named_pipe::NamedPipeServer>,
    },
}

/// The daemon's listening socket / pipe. Must be created inside a tokio
/// runtime.
pub struct Listener {
    inner: ListenerInner,
    endpoint: Endpoint,
    owner_uid: Option<u32>,
}

impl Listener {
    /// Bind the endpoint. On Unix this prepares the runtime directory
    /// (`0700`, owned by us), removes a stale socket file, binds, and sets
    /// the socket to `0600`. On Windows it creates the first pipe instance
    /// (failing if another process already owns the name).
    pub fn bind(endpoint: &Endpoint) -> io::Result<Self> {
        let inner = match endpoint {
            Endpoint::Unix { path } => {
                #[cfg(unix)]
                {
                    prepare_unix_socket_path(path)?;
                    let listener = tokio::net::UnixListener::bind(path)?;
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
                    ListenerInner::Unix(listener)
                }
                #[cfg(not(unix))]
                {
                    let _ = path;
                    return Err(io::Error::new(
                        io::ErrorKind::Unsupported,
                        "unix sockets are not available on this platform",
                    ));
                }
            }
            Endpoint::NamedPipe { name } => {
                #[cfg(windows)]
                {
                    use tokio::net::windows::named_pipe::ServerOptions;
                    let first = ServerOptions::new()
                        .first_pipe_instance(true)
                        .reject_remote_clients(true)
                        .create(name)?;
                    ListenerInner::Pipe {
                        name: name.clone(),
                        next: Some(first),
                    }
                }
                #[cfg(not(windows))]
                {
                    let _ = name;
                    return Err(io::Error::new(
                        io::ErrorKind::Unsupported,
                        "named pipes are not available on this platform",
                    ));
                }
            }
        };
        Ok(Self {
            inner,
            endpoint: endpoint.clone(),
            owner_uid: current_uid(),
        })
    }

    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    /// The uid that owns this listener (Unix), for peer checks.
    pub fn owner_uid(&self) -> Option<u32> {
        self.owner_uid
    }

    pub async fn accept(&mut self) -> io::Result<Connection> {
        match &mut self.inner {
            #[cfg(unix)]
            ListenerInner::Unix(l) => {
                let (stream, _) = l.accept().await?;
                let peer_uid = stream.peer_cred().ok().map(|c| c.uid());
                Ok(Connection {
                    peer_uid,
                    stream: Box::new(stream),
                })
            }
            #[cfg(windows)]
            ListenerInner::Pipe { name, next } => {
                use tokio::net::windows::named_pipe::ServerOptions;
                let server = match next.take() {
                    Some(s) => s,
                    None => ServerOptions::new()
                        .reject_remote_clients(true)
                        .create(&*name)?,
                };
                server.connect().await?;
                // Create the next instance before handing this one out so a
                // client arriving meanwhile finds a listener.
                *next = ServerOptions::new()
                    .reject_remote_clients(true)
                    .create(&*name)
                    .ok();
                Ok(Connection {
                    peer_uid: None,
                    stream: Box::new(server),
                })
            }
        }
    }

    /// Stop listening and remove the socket file (Unix). Hooks arriving
    /// afterwards see no socket and spool.
    pub fn close(self) {
        let Listener {
            inner, endpoint, ..
        } = self;
        drop(inner);
        if let Endpoint::Unix { path } = endpoint {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[cfg(unix)]
fn prepare_unix_socket_path(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
    let dir = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "socket path has no parent directory",
        )
    })?;
    std::fs::create_dir_all(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    let meta = std::fs::metadata(dir)?;
    if let Some(me) = current_uid()
        && meta.uid() != me
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "runtime directory {} is owned by uid {} (we are uid {me}); refusing to share a socket directory",
                dir.display(),
                meta.uid()
            ),
        ));
    }
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_socket() => std::fs::remove_file(path)?,
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("{} exists and is not a socket", path.display()),
            ));
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn base64_round_trips_every_padding() {
        for n in 0..40usize {
            let bytes: Vec<u8> = (0..n).map(|i| (i * 37 % 251) as u8).collect();
            let text = super::base64_encode(&bytes);
            assert!(text.len().is_multiple_of(4));
            assert_eq!(super::base64_decode(&text).unwrap(), bytes, "{n} bytes");
        }
        assert_eq!(super::base64_encode(b"Man"), "TWFu");
        assert_eq!(super::base64_encode(b"Ma"), "TWE=");
        assert_eq!(super::base64_encode(b"M"), "TQ==");
        assert!(super::base64_decode("TQ=").is_none());
        assert!(super::base64_decode("T*==").is_none());
    }

    #[test]
    fn read_request_json_is_stable() {
        let req = super::ReadRequest {
            kind: super::ReadKind::Timeline,
            statement: None,
            scope: super::ReadScope {
                repo_root: Some("/home/dev/example/project".into()),
                ..Default::default()
            },
            session_limit: Some(10),
            all_sessions: false,
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"kind\":\"timeline\""), "{json}");
        assert!(
            !json.contains("statement"),
            "absent fields are omitted: {json}"
        );
        let back: super::ReadRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back, req);
    }

    use super::*;

    #[test]
    fn frame_roundtrip() {
        let f = Frame::json(MsgType::Nack, &Nack::new("x", "why", true)).unwrap();
        let bytes = f.encode();
        assert_eq!(bytes.len(), FRAME_HEADER_LEN + f.payload.len());
        let (back, used) = Frame::decode(&bytes).unwrap().unwrap();
        assert_eq!(back, f);
        assert_eq!(used, bytes.len());
        assert_eq!(back.kind(), Some(MsgType::Nack));
        let n: Nack = back.parse_json().unwrap();
        assert_eq!(n.code, "x");
        // Incomplete input asks for more.
        assert!(Frame::decode(&bytes[..5]).unwrap().is_none());
        assert!(Frame::decode(&bytes[..bytes.len() - 1]).unwrap().is_none());
        // Sync reader.
        let mut cursor = io::Cursor::new(bytes.clone());
        assert_eq!(Frame::read_from(&mut cursor).unwrap(), f);
        assert!(matches!(
            Frame::read_from(&mut cursor),
            Err(IpcError::Closed)
        ));
    }

    #[test]
    fn crc_mismatch_and_oversize_are_rejected() {
        let mut bytes = Frame::empty(MsgType::Ping).encode();
        bytes[FRAME_HEADER_LEN - 1] ^= 0x01; // flip a flags bit -> crc no longer matches
        assert!(matches!(
            Frame::decode(&bytes),
            Err(IpcError::CrcMismatch { .. })
        ));
        let mut big = Frame::empty(MsgType::Ping).encode();
        big[0..4].copy_from_slice(&(MAX_PAYLOAD + 1).to_le_bytes());
        assert!(matches!(
            Frame::decode(&big),
            Err(IpcError::FrameTooLarge(_))
        ));
        let mut cursor = io::Cursor::new(big);
        assert!(matches!(
            Frame::read_from(&mut cursor),
            Err(IpcError::FrameTooLarge(_))
        ));
    }

    #[test]
    fn prelude_roundtrip() {
        let p = encode_prelude(PROTOCOL_VERSION, 0);
        assert_eq!(decode_prelude(&p).unwrap(), (PROTOCOL_VERSION, 0));
        let mut bad = p;
        bad[0] = b'X';
        assert!(matches!(decode_prelude(&bad), Err(IpcError::BadMagic)));
    }

    #[test]
    fn unknown_type_is_representable() {
        let f = Frame {
            msg_type: 200,
            codec: CODEC_JSON,
            flags: 0,
            payload: vec![],
        };
        let (back, _) = Frame::decode(&f.encode()).unwrap().unwrap();
        assert_eq!(back.kind(), None);
        assert_eq!(back.msg_type, 200);
    }

    #[test]
    fn endpoint_is_deterministic_and_short() {
        let short = endpoint_for_runtime_dir(Path::new("/tmp/x"));
        assert_eq!(short, endpoint_for_runtime_dir(Path::new("/tmp/x")));
        if cfg!(unix) {
            assert_eq!(
                short,
                Endpoint::Unix {
                    path: PathBuf::from("/tmp/x").join(SOCKET_FILE)
                }
            );
            let long = PathBuf::from(format!("/{}", "very-long-directory-name/".repeat(8)));
            let ep = endpoint_for_runtime_dir(&long);
            let p = ep.socket_path().unwrap();
            assert!(p.as_os_str().len() <= MAX_SUN_PATH, "{}", p.display());
            assert!(p.to_string_lossy().contains("attemptdb-"));
            assert_eq!(ep, endpoint_for_runtime_dir(&long));
        }
    }
}
