//! The hook entrypoint: `attempt hook <provider> [--event NAME]`.
//!
//! Design constraints, in priority order:
//!
//! 1. **Never harm the coding agent.** Always exit 0 with empty stdout (except
//!    the provider-specific "allow" acknowledgement Gemini expects). Any
//!    failure is written to `hook.log` under the log directory.
//! 2. **Be fast.** No database open, no async runtime, no subprocess. One
//!    JSON parse, a few small file reads, then either one bounded IPC round
//!    trip to the daemon (when its socket exists: one `stat` to find out) or
//!    one locked append to the spool.
//! 3. **Never drop an observation silently.** Undecodable payloads still
//!    produce an `unknown` event carrying the parse error class.

use crate::config::{Config, DeviceRecord};
use crate::git::git_info;
use crate::ipc;
use crate::locator::Locator;
use attemptdb_adapters::{ADAPTER_VERSION, CaptureContext, adapter_for};
use attemptdb_core::event::{ProjectRef, Provider};
use attemptdb_core::{Event, EventKind, Timestamp};
use attemptdb_storage::SpoolWriter;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::mpsc;
use std::time::{Duration, Instant};

thread_local! {
    static HOOK_STARTED: std::cell::Cell<Option<Instant>> = const { std::cell::Cell::new(None) };
}

/// Upper bound on the payload read from stdin (tool outputs can be large).
pub const MAX_STDIN_BYTES: usize = 16 * 1024 * 1024;

/// How the event left the hook process.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Delivery {
    /// Acknowledged by the daemon over IPC (durable in the WAL).
    Daemon,
    /// Appended to the spool; the daemon or the next CLI command imports it.
    Spool,
    /// Neither path succeeded; see `HookOutcome::error`.
    Failed,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct HookOutcome {
    pub provider: String,
    pub event_kind: String,
    pub provider_event_name: String,
    /// Spool file written, when the event went to the spool.
    pub spool_path: Option<PathBuf>,
    pub delivered: Delivery,
    pub db_dir: PathBuf,
    pub elapsed_us: u128,
    /// The text to print on stdout (provider acknowledgement), if any.
    pub stdout: Option<String>,
    pub error: Option<String>,
}

pub struct HookInput<'a> {
    pub provider_id: &'a str,
    pub event_hint: Option<&'a str>,
    pub payload_bytes: Vec<u8>,
    /// `cwd` fallback when the payload has none (e.g. `CLAUDE_PROJECT_DIR`).
    pub cwd_hint: Option<PathBuf>,
    pub data_dir_override: Option<PathBuf>,
    pub db_override: Option<PathBuf>,
}

/// A reader that makes no progress for this long has stalled: the agent
/// never closed stdin, or died holding it open.
pub const STDIN_IDLE_TIMEOUT: Duration = Duration::from_secs(2);
/// Upper bound on the whole read, however slowly it progresses.
pub const STDIN_TOTAL_TIMEOUT: Duration = Duration::from_secs(4);
/// Once a complete JSON object has arrived, how long to wait for the
/// writer's EOF before going on without it. Agents that write the payload
/// and then close stdin (all of them, normally) never reach this.
const STDIN_EOF_GRACE: Duration = Duration::from_millis(50);

/// What [`read_bounded`] found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StdinRead {
    /// Up to `max + 1` bytes: one lookahead byte tells an oversized payload
    /// from an exact fit.
    pub bytes: Vec<u8>,
    /// Bytes past the cap that were read and thrown away.
    pub dropped: u64,
    /// The reader stalled or ran out of time before EOF and before a whole
    /// JSON object had arrived; `bytes` is what came in until then.
    pub timed_out: bool,
    /// EOF was reached.
    pub eof: bool,
}

impl StdinRead {
    /// Size of the payload as the agent sent it (as far as it was read).
    pub fn total_bytes(&self) -> u64 {
        self.bytes.len() as u64 + self.dropped
    }
}

/// Tracks whether the bytes fed so far form one complete JSON object, so a
/// writer that never closes stdin does not cost the hook its idle timeout.
/// A string-and-escape aware bracket counter; input that does not start
/// with `{` is never reported complete (EOF or the timeouts decide).
#[derive(Default)]
struct ObjectEnd {
    state: ObjectState,
    depth: usize,
    in_string: bool,
    escaped: bool,
}

#[derive(Default, PartialEq, Eq)]
enum ObjectState {
    /// Only whitespace so far.
    #[default]
    Waiting,
    Inside,
    Complete,
    NotAnObject,
}

impl ObjectEnd {
    fn complete(&self) -> bool {
        self.state == ObjectState::Complete
    }

    fn feed(&mut self, chunk: &[u8]) {
        for &b in chunk {
            match self.state {
                ObjectState::Complete | ObjectState::NotAnObject => return,
                ObjectState::Waiting => match b {
                    b' ' | b'\t' | b'\r' | b'\n' => {}
                    b'{' => {
                        self.state = ObjectState::Inside;
                        self.depth = 1;
                    }
                    _ => self.state = ObjectState::NotAnObject,
                },
                ObjectState::Inside if self.in_string => {
                    if self.escaped {
                        self.escaped = false;
                    } else if b == b'\\' {
                        self.escaped = true;
                    } else if b == b'"' {
                        self.in_string = false;
                    }
                }
                ObjectState::Inside => match b {
                    b'"' => self.in_string = true,
                    b'{' | b'[' => self.depth += 1,
                    b'}' | b']' => {
                        self.depth = self.depth.saturating_sub(1);
                        if self.depth == 0 {
                            self.state = ObjectState::Complete;
                        }
                    }
                    _ => {}
                },
            }
        }
    }
}

/// Read `reader` to EOF without ever blocking the caller for long.
///
/// A thread does the reading and the caller waits on a channel, so a stdin
/// that never closes costs at most `idle` of silence or `total` in all; what
/// arrived until then is kept. At most `max + 1` bytes are kept; the rest of
/// an oversized payload is still read, and discarded, so the agent's write
/// does not fail with `EPIPE`. The reading thread may stay blocked in
/// `read` after a timeout; the hook process exits right after.
pub fn read_bounded<R: Read + Send + 'static>(
    reader: R,
    max: usize,
    idle: Duration,
    total: Duration,
) -> StdinRead {
    enum Msg {
        Data(Vec<u8>),
        Dropped(usize),
        Done,
    }
    let (tx, rx) = mpsc::channel::<Msg>();
    let spawned = std::thread::Builder::new()
        .name("attemptdb-hook-stdin".into())
        .spawn(move || {
            let mut reader = reader;
            let mut kept = 0usize;
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                let n = match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                };
                let take = n.min((max + 1).saturating_sub(kept));
                let mut open = true;
                if take > 0 {
                    kept += take;
                    open &= tx.send(Msg::Data(buf[..take].to_vec())).is_ok();
                }
                if n > take {
                    open &= tx.send(Msg::Dropped(n - take)).is_ok();
                }
                if !open {
                    return;
                }
            }
            let _ = tx.send(Msg::Done);
        });
    let mut out = StdinRead::default();
    if spawned.is_err() {
        // No thread to read with (the process is out of resources): report
        // a stalled, empty read; the event is still recorded as a gap.
        out.timed_out = true;
        return out;
    }
    let started = Instant::now();
    let mut object = ObjectEnd::default();
    loop {
        let Some(left) = total.checked_sub(started.elapsed()) else {
            out.timed_out = !object.complete();
            break;
        };
        let wait = if object.complete() {
            STDIN_EOF_GRACE.min(left)
        } else {
            idle.min(left)
        };
        match rx.recv_timeout(wait) {
            Ok(Msg::Data(d)) => {
                if out.dropped == 0 {
                    object.feed(&d);
                }
                out.bytes.extend_from_slice(&d);
            }
            Ok(Msg::Dropped(n)) => out.dropped += n as u64,
            Ok(Msg::Done) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                out.eof = true;
                break;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                out.timed_out = !object.complete();
                break;
            }
        }
    }
    out
}

/// What the last [`read_stdin`] saw, handed to the [`run_hook`] that
/// follows in the same one-event process. A hook process reads one payload,
/// so a process-wide slot is the whole protocol; it keeps `HookInput` (built
/// by `attempt hook` and `attempt-hook`) unchanged.
static STDIN_OUTCOME: Mutex<Option<StdinRead>> = Mutex::new(None);

/// Read stdin (bounded in size and in time) for the hook entrypoint:
/// [`MAX_STDIN_BYTES`] kept, about [`STDIN_IDLE_TIMEOUT`] of silence or
/// [`STDIN_TOTAL_TIMEOUT`] in all waited, see [`read_bounded`].
pub fn read_stdin() -> Vec<u8> {
    let mut read = read_bounded(
        std::io::stdin(),
        MAX_STDIN_BYTES,
        STDIN_IDLE_TIMEOUT,
        STDIN_TOTAL_TIMEOUT,
    );
    let bytes = std::mem::take(&mut read.bytes);
    if let Ok(mut slot) = STDIN_OUTCOME.lock() {
        *slot = Some(read);
    }
    bytes
}

fn take_stdin_outcome() -> Option<StdinRead> {
    STDIN_OUTCOME.lock().ok().and_then(|mut slot| slot.take())
}

/// Run the whole hook pipeline. Never panics; never returns an error to the
/// caller — problems are reported inside `HookOutcome::error`.
pub fn run_hook(input: HookInput<'_>) -> HookOutcome {
    let started = Instant::now();
    HOOK_STARTED.with(|c| c.set(Some(started)));
    let provider: Provider = input.provider_id.parse().expect("infallible");
    let mut outcome = HookOutcome {
        provider: provider.as_str().to_string(),
        event_kind: String::new(),
        provider_event_name: String::new(),
        spool_path: None,
        delivered: Delivery::Failed,
        db_dir: PathBuf::new(),
        elapsed_us: 0,
        stdout: provider_ack(&provider),
        error: None,
    };
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run_inner(&input, &provider, &mut outcome)
    })) {
        Ok(Ok(())) => {}
        Ok(Err(e)) => outcome.error = Some(e),
        Err(_) => outcome.error = Some("hook panicked".into()),
    }
    outcome.elapsed_us = started.elapsed().as_micros();
    if let Some(err) = &outcome.error {
        log_error(&input, err);
    }
    outcome
}

/// Stage timings are printed to stderr when `ATTEMPTDB_HOOK_TRACE` is set.
/// Never on by default: stderr is visible to some agents.
struct Trace {
    on: bool,
    t0: Instant,
    last: Instant,
    stages: Vec<(&'static str, u128)>,
}

impl Trace {
    fn new(t0: Instant) -> Self {
        let on = std::env::var_os("ATTEMPTDB_HOOK_TRACE").is_some();
        Self {
            on,
            t0,
            last: t0,
            stages: Vec::new(),
        }
    }

    fn mark(&mut self, stage: &'static str) {
        if self.on {
            let now = Instant::now();
            self.stages
                .push((stage, now.duration_since(self.last).as_micros()));
            self.last = now;
        }
    }

    fn finish(&self) {
        if self.on {
            let parts: Vec<String> = self
                .stages
                .iter()
                .map(|(s, us)| format!("{s}={us}us"))
                .collect();
            eprintln!(
                "attempt-hook trace total={}us {}",
                self.t0.elapsed().as_micros(),
                parts.join(" ")
            );
        }
    }
}

fn run_inner(
    input: &HookInput<'_>,
    provider: &Provider,
    out: &mut HookOutcome,
) -> Result<(), String> {
    let mut trace = Trace::new(HOOK_STARTED.with(|c| c.get()).unwrap_or_else(Instant::now));
    let stdin = take_stdin_outcome();
    let stdin_timed_out = stdin.as_ref().is_some_and(|r| r.timed_out);
    let payload_total = stdin
        .as_ref()
        .map_or(input.payload_bytes.len() as u64, StdinRead::total_bytes);
    let (payload, parse_error) = if input.payload_bytes.len() > MAX_STDIN_BYTES {
        (serde_json::json!({}), Some("payload_truncated"))
    } else {
        match serde_json::from_slice::<serde_json::Value>(&input.payload_bytes) {
            Ok(v) if v.is_object() => (v, None),
            Ok(_) => (serde_json::json!({}), Some("payload_not_object")),
            Err(_) if input.payload_bytes.iter().all(u8::is_ascii_whitespace) => (
                serde_json::json!({}),
                Some(if stdin_timed_out {
                    "stdin_timeout"
                } else {
                    "empty_payload"
                }),
            ),
            Err(_) if stdin_timed_out => (serde_json::json!({}), Some("stdin_timeout")),
            Err(_) => (serde_json::json!({}), Some("invalid_json")),
        }
    };

    // A payload that did not parse may still say where it came from.
    let scanned_cwd = parse_error
        .and_then(|_| scan_string_field(&input.payload_bytes, "cwd"))
        .filter(|s| !s.is_empty());
    let cwd: PathBuf = payload
        .get("cwd")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .or(scanned_cwd.as_deref())
        .or_else(|| {
            // Cursor's lifecycle payloads need not carry cwd. Its host process
            // directory is not necessarily the workspace that generated them.
            (provider == &Provider::Cursor)
                .then(|| {
                    payload
                        .get("workspace_roots")?
                        .as_array()?
                        .iter()
                        .filter_map(|v| v.as_str())
                        .find(|s| !s.is_empty())
                })
                .flatten()
        })
        .map(PathBuf::from)
        .or_else(|| input.cwd_hint.clone())
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."));

    trace.mark("parse");
    let locator = Locator::resolve(
        &cwd,
        input.data_dir_override.as_deref(),
        input.db_override.as_deref(),
    );
    out.db_dir = locator.db_dir.clone();
    let config = Config::load_or_default(&locator.paths.config_dir);
    let (device, repaired) =
        DeviceRecord::load_or_create_checked(&locator.paths.data_dir).map_err(|e| e.to_string())?;
    if let Some(aside) = repaired {
        // Once per repair: the machine just got a new device id.
        log_error(
            input,
            &format!(
                "device.json was unusable and was moved to {}; this machine has a new device id",
                aside.display()
            ),
        );
    }
    trace.mark("locate");

    let git = git_info(&cwd);
    trace.mark("git");
    let root = git
        .as_ref()
        .map(|g| g.root.clone())
        .unwrap_or_else(|| cwd.clone());
    let mut project = ProjectRef::derive(
        &root.to_string_lossy(),
        git.as_ref().and_then(|g| g.remote.as_deref()),
        &device.device_id,
    );
    if let Some(g) = &git {
        project.branch = g.branch.clone();
        project.head = g.head.clone();
    }

    let ctx = CaptureContext {
        device_id: device.device_id,
        capture_mode: config.capture_mode,
        project,
        captured_at: Timestamp::now(),
        provider_version: None,
        hook_version: Some(env!("CARGO_PKG_VERSION").to_string()),
    };

    let mut event = match parse_error {
        None => {
            // A panicking adapter must cost one event's detail, never the
            // observation: the guard turns it into an adapter error.
            let normalised = adapter_for(provider).map(|adapter| {
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    adapter.normalise(&ctx, input.event_hint, &payload)
                }))
                .unwrap_or(Err(attemptdb_adapters::AdapterError::Invalid(
                    "adapter panicked".into(),
                )))
            });
            match normalised {
                Some(Ok(ev)) => ev,
                failure => {
                    let (gap, class) = match &failure {
                        None => ("unsupported_provider", None),
                        Some(Err(e)) => ("adapter_error", Some(adapter_error_class(e))),
                        Some(Ok(_)) => unreachable!("handled above"),
                    };
                    let mut ev = unknown_event(&ctx, provider, input.event_hint, &payload);
                    ev.attrs
                        .insert("capture_gap".into(), serde_json::json!(gap));
                    if let Some(class) = class {
                        ev.attrs
                            .insert("adapter_error".into(), serde_json::json!(class));
                    }
                    // The payload parsed, so keep it whole: `apply_capture_mode`
                    // below drops it again unless the mode keeps content.
                    ev.raw = Some(payload.clone());
                    ev
                }
            }
        }
        Some(class) => {
            // Nothing to normalise, but the history must show the gap with
            // what is known about it: the session (found by a cheap scan,
            // the payload may be torn or too large to parse), the event
            // name and, when the mode keeps content, the bytes themselves.
            let session = scan_string_field(&input.payload_bytes, "session_id")
                .or_else(|| scan_string_field(&input.payload_bytes, "conversation_id"));
            let name = scan_string_field(&input.payload_bytes, "hook_event_name");
            let mut ev = unknown_event_named(
                &ctx,
                provider,
                name.as_deref().or(input.event_hint),
                session.as_deref(),
            );
            ev.attrs
                .insert("capture_gap".into(), serde_json::json!(class));
            ev.attrs.insert(
                "x_attemptdb_payload_bytes".into(),
                serde_json::json!(payload_total),
            );
            if class != "empty_payload" && !input.payload_bytes.is_empty() {
                let kept = input.payload_bytes.len().min(GAP_RAW_BYTES);
                ev.raw = Some(serde_json::Value::String(
                    String::from_utf8_lossy(&input.payload_bytes[..kept]).into_owned(),
                ));
                if kept < input.payload_bytes.len() || payload_total > kept as u64 {
                    ev.attrs
                        .insert("x_attemptdb_raw_truncated".into(), serde_json::json!(true));
                }
            }
            ev
        }
    };
    if config.load_error.is_some() {
        // This event was captured under the fail-closed fallback; the
        // history says so even when doctor is not run.
        event.attrs.insert(
            "x_attemptdb_config_fallback".into(),
            serde_json::json!("metadata_only"),
        );
    }
    if payload
        .get("_attemptdb_capture_test")
        .and_then(|v| v.as_bool())
        == Some(true)
    {
        event.kind = EventKind::CaptureTest;
    }
    if !config.keep_raw_payload {
        event.raw = None;
    }
    event.apply_capture_mode();
    // Hook overhead up to this point (parse + normalise), in microseconds.
    // Content-free, and the basis for the "hook p95 < 10 ms" gate.
    if let Some(t0) = HOOK_STARTED.with(|c| c.get()) {
        event.attrs.insert(
            "hook_us".into(),
            serde_json::json!(t0.elapsed().as_micros() as u64),
        );
    }
    out.event_kind = event.kind.as_str().to_string();
    out.provider_event_name = event.provider_event_name.clone();
    trace.mark("normalise");

    // Fast path: hand the event to the daemon when one is listening. The
    // presence check is a single `stat`; the exchange is one round trip
    // bounded by `ipc::DEFAULT_CONNECT_TIMEOUT + ipc::DEFAULT_ROUNDTRIP_TIMEOUT`.
    // Any failure (stale socket, timeout, NACK, wrong database) falls
    // through to the spool, which the daemon imports; duplicates are
    // harmless because ingestion is idempotent by event id.
    if ipc::daemon_reachable(&locator) {
        match ipc::Client::send_events(&locator, std::slice::from_ref(&event)) {
            Ok(_ack) => {
                trace.mark("ipc");
                trace.finish();
                out.spool_path = None;
                out.delivered = Delivery::Daemon;
                return Ok(());
            }
            Err(_) => trace.mark("ipc_failed"),
        }
    }

    let writer = SpoolWriter::new(&locator.db_dir).map_err(|e| e.to_string())?;
    let path = writer
        .append_with(std::slice::from_ref(&event), config.spool_sync)
        .map_err(|e| e.to_string())?;
    out.spool_path = Some(path);
    out.delivered = Delivery::Spool;
    trace.mark("spool");
    trace.finish();
    Ok(())
}

/// Bytes of an unparseable or oversized payload kept as `raw` (when the
/// capture mode keeps content). The head is what identifies the event.
const GAP_RAW_BYTES: usize = 256 * 1024;
/// How far into a payload [`scan_string_field`] looks.
const SCAN_BYTES: usize = 64 * 1024;

fn unknown_event(
    ctx: &CaptureContext,
    provider: &Provider,
    hint: Option<&str>,
    payload: &serde_json::Value,
) -> Event {
    let name = payload
        .get("hook_event_name")
        .and_then(|v| v.as_str())
        .or(hint);
    let session = payload
        .get("session_id")
        .or_else(|| payload.get("conversation_id"))
        .and_then(|v| v.as_str());
    unknown_event_named(ctx, provider, name, session)
}

fn unknown_event_named(
    ctx: &CaptureContext,
    provider: &Provider,
    name: Option<&str>,
    session: Option<&str>,
) -> Event {
    let mut ev = Event::new(
        ctx.device_id,
        provider.clone(),
        name.unwrap_or("unknown"),
        EventKind::Unknown,
        ctx.project.clone(),
        session.unwrap_or("unknown"),
        ctx.capture_mode,
        ADAPTER_VERSION,
    );
    ev.captured_at = ctx.captured_at;
    ev.observed_at = ctx.captured_at;
    ev.hook_version = ctx.hook_version.clone();
    ev
}

fn adapter_error_class(e: &attemptdb_adapters::AdapterError) -> &'static str {
    use attemptdb_adapters::AdapterError::*;
    match e {
        MissingEventName => "missing_event_name",
        PayloadNotObject => "payload_not_object",
        UnsupportedEvent(_) => "unsupported_event",
        Invalid(_) => "invalid_payload",
    }
}

/// The string value of `"key": "value"` found by looking at the first
/// [`SCAN_BYTES`] of `bytes` as text: for payloads that are torn, too large
/// or otherwise not parseable, where a session id is still worth keeping.
/// A key inside a nested string cannot match (its quotes are escaped); a key
/// of a nested object can, which is acceptable for a best-effort scan.
fn scan_string_field(bytes: &[u8], key: &str) -> Option<String> {
    let head = &bytes[..bytes.len().min(SCAN_BYTES)];
    let needle = format!("\"{key}\"");
    let needle = needle.as_bytes();
    let mut from = 0;
    while let Some(pos) = head[from..].windows(needle.len()).position(|w| w == needle) {
        let mut i = from + pos + needle.len();
        from = i;
        let skip_ws = |i: &mut usize| {
            while head.get(*i).is_some_and(u8::is_ascii_whitespace) {
                *i += 1;
            }
        };
        skip_ws(&mut i);
        if head.get(i) != Some(&b':') {
            continue;
        }
        i += 1;
        skip_ws(&mut i);
        if head.get(i) != Some(&b'"') {
            continue;
        }
        let start = i;
        i += 1;
        while let Some(&b) = head.get(i) {
            match b {
                b'\\' => i += 2,
                b'"' => break,
                _ => i += 1,
            }
        }
        if head.get(i) != Some(&b'"') {
            continue; // torn inside the value
        }
        if let Ok(value) = serde_json::from_slice::<String>(&head[start..=i])
            && !value.is_empty()
            && value.len() <= 256
        {
            return Some(value);
        }
    }
    None
}

/// Provider-specific stdout acknowledgement. Gemini CLI expects a JSON
/// decision from command hooks; every other provider must receive nothing
/// on stdout (Claude Code injects plain stdout into context on some events).
fn provider_ack(provider: &Provider) -> Option<String> {
    match provider {
        Provider::GeminiCli => Some("{\"decision\":\"allow\"}".to_string()),
        _ => None,
    }
}

fn log_error(input: &HookInput<'_>, err: &str) {
    log_problem(
        input.provider_id,
        input.cwd_hint.as_deref(),
        input.data_dir_override.as_deref(),
        input.db_override.as_deref(),
        err,
    );
}

/// Append one line to `hook.log` under the log directory: the hook's
/// diagnostics, which the agent never sees. Failure to log is ignored.
pub fn log_problem(
    provider_id: &str,
    cwd_hint: Option<&Path>,
    data_dir_override: Option<&Path>,
    db_override: Option<&Path>,
    err: &str,
) {
    let locator = Locator::resolve(
        cwd_hint.unwrap_or(Path::new(".")),
        data_dir_override,
        db_override,
    );
    let dir = &locator.paths.log_dir;
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let path = dir.join("hook.log");
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        use std::io::Write;
        let _ = writeln!(f, "{} provider={provider_id} err={err}", Timestamp::now());
    }
}

/// Build a synthetic payload that exercises the same pipeline as a real
/// hook, used by `attempt hook install --verify` and `attempt doctor`.
pub fn capture_test_payload(provider: &Provider, cwd: &Path) -> serde_json::Value {
    let cwd = cwd.to_string_lossy().to_string();
    let name = match provider {
        Provider::Cursor => "attemptdbCaptureTest",
        _ => "AttemptDBCaptureTest",
    };
    serde_json::json!({
        "hook_event_name": name,
        "session_id": format!("attemptdb-capture-test-{}", std::process::id()),
        "conversation_id": format!("attemptdb-capture-test-{}", std::process::id()),
        "cwd": cwd,
        "_attemptdb_capture_test": true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use attemptdb_core::CaptureMode;
    use attemptdb_storage::{Database, OpenOptions, ScanFilter};
    use std::io::Write;

    fn run(dir: &Path, provider: &str, payload: &str) -> HookOutcome {
        run_hook(HookInput {
            provider_id: provider,
            event_hint: None,
            payload_bytes: payload.as_bytes().to_vec(),
            cwd_hint: Some(dir.to_path_buf()),
            data_dir_override: Some(dir.join("data")),
            db_override: None,
        })
    }

    #[test]
    fn payload_gaps_survive_ingestion_and_metadata_sanitisation() {
        let tmp = tempfile::tempdir().unwrap();
        let oversized = " ".repeat(MAX_STDIN_BYTES + 1);
        for (payload, reason) in [
            ("invalid", "invalid_json"),
            ("[]", "payload_not_object"),
            (oversized.as_str(), "payload_truncated"),
        ] {
            let out = run(tmp.path(), "codex", payload);
            assert_eq!(out.delivered, Delivery::Spool);
            let mut db = Database::open(
                &out.db_dir,
                OpenOptions {
                    create: true,
                    ..Default::default()
                },
            )
            .unwrap();
            db.import_spool().unwrap();
            let events = db.scan(&ScanFilter::default()).unwrap();
            assert!(
                events
                    .iter()
                    .any(|e| e.attrs.get("capture_gap").and_then(|v| v.as_str()) == Some(reason))
            );
        }
        let out = run(
            tmp.path(),
            "unrecognised-provider",
            "{\"hook_event_name\":\"Stop\",\"session_id\":\"s\"}",
        );
        assert_eq!(out.delivered, Delivery::Spool);
        assert_eq!(out.event_kind, "unknown");
    }

    #[test]
    fn cursor_workspace_roots_select_the_project_database() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().join("workspace");
        Database::create(
            &workspace.join(".attemptdb"),
            attemptdb_core::DeviceId::new(),
        )
        .unwrap();
        let payload = serde_json::json!({"hook_event_name": "sessionStart", "conversation_id": "s",
            "workspace_roots": [workspace], "cwd": ""});
        let out = run(tmp.path(), "cursor", &payload.to_string());
        assert_eq!(out.db_dir, workspace.join(".attemptdb"));
        assert_eq!(out.delivered, Delivery::Spool);
    }

    #[test]
    fn hook_appends_to_spool_and_importer_sees_it() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let payload = serde_json::json!({
            "hook_event_name": "PostToolUse",
            "session_id": "s-1",
            "cwd": dir.to_string_lossy(),
            "tool_name": "Bash",
            "tool_use_id": "tu_1",
            "tool_input": {"command": "cargo test"},
            "tool_response": {"stdout": "ok"}
        })
        .to_string();
        let out = run(dir, "claude-code", &payload);
        assert!(out.error.is_none(), "{:?}", out.error);
        assert_eq!(out.event_kind, "tool_call_finished");
        assert_eq!(out.delivered, Delivery::Spool);
        assert!(out.stdout.is_none());
        assert!(out.elapsed_us < 5_000_000);
        // Garbage still yields an observation.
        let out2 = run(dir, "claude-code", "not json");
        assert!(out2.error.is_none());
        assert_eq!(out2.event_kind, "unknown");
        let out3 = run(dir, "gemini-cli", "{}");
        assert_eq!(out3.stdout.as_deref(), Some("{\"decision\":\"allow\"}"));
        let test_payload = capture_test_payload(&Provider::Codex, dir).to_string();
        let out4 = run(dir, "codex", &test_payload);
        assert_eq!(out4.event_kind, "capture_test");

        let mut db = Database::open(
            &out.db_dir,
            OpenOptions {
                create: true,
                ..Default::default()
            },
        )
        .unwrap();
        let r = db.import_spool().unwrap();
        assert_eq!(r.accepted, 4);
        let events = db.scan(&ScanFilter::default()).unwrap();
        assert_eq!(events.len(), 4);
        assert!(events.iter().any(|e| e.kind == EventKind::CaptureTest));
        assert!(events[0].attrs.get("hook_us").is_some());
        assert_eq!(events[0].tool.as_ref().unwrap().name, "Bash");
        assert!(events.iter().all(|e| e.hook_version.is_some()));
    }

    // ---- §5.3: a config that cannot be used never opens the privacy gate --

    const SECRET: &str = "TOPSECRET-PROMPT-TEXT-4242";

    fn prompt_payload(dir: &Path) -> String {
        serde_json::json!({
            "hook_event_name": "UserPromptSubmit",
            "session_id": "s-privacy",
            "cwd": dir.to_string_lossy(),
            "prompt": SECRET,
        })
        .to_string()
    }

    fn stored_events(out: &HookOutcome) -> Vec<Event> {
        let mut db = Database::open(
            &out.db_dir,
            OpenOptions {
                create: true,
                ..Default::default()
            },
        )
        .unwrap();
        db.import_spool().unwrap();
        db.scan(&ScanFilter::default()).unwrap()
    }

    #[test]
    fn a_config_that_cannot_be_used_captures_metadata_only() {
        let cases: Vec<(&str, Vec<u8>)> = vec![
            ("typo", br#"{"capture_mode":"metadata-only"}"#.to_vec()),
            (
                "trailing comma",
                br#"{"capture_mode":"local_semantic",}"#.to_vec(),
            ),
            (
                "unknown enum value",
                br#"{"capture_mode":"semantic_v2"}"#.to_vec(),
            ),
            ("empty file", Vec::new()),
            ("non-UTF-8", vec![0xff, 0xfe, 0x00, 0x80]),
        ];
        for (name, bytes) in cases {
            let tmp = tempfile::tempdir().unwrap();
            let config_dir = tmp.path().join("data").join("config");
            std::fs::create_dir_all(&config_dir).unwrap();
            std::fs::write(Config::path(&config_dir), bytes).unwrap();
            let out = run(tmp.path(), "claude-code", &prompt_payload(tmp.path()));
            assert!(out.error.is_none(), "{name}: {:?}", out.error);
            assert!(out.stdout.is_none(), "{name}: the hook prints nothing");
            assert_eq!(out.delivered, Delivery::Spool, "{name}");
            let events = stored_events(&out);
            assert_eq!(events.len(), 1, "{name}");
            let ev = &events[0];
            assert!(
                ev.content.is_none() && ev.raw.is_none(),
                "{name}: no content stored"
            );
            assert_eq!(ev.capture_mode, CaptureMode::MetadataOnly, "{name}");
            assert!(
                !format!("{ev:?}").contains(SECRET),
                "{name}: the prompt text leaked"
            );
            assert_eq!(
                ev.attrs.get("x_attemptdb_config_fallback"),
                Some(&serde_json::json!("metadata_only")),
                "{name}: the event says it was captured under the fallback"
            );
        }
    }

    #[test]
    fn a_missing_config_keeps_the_default_and_stores_content() {
        let tmp = tempfile::tempdir().unwrap();
        let out = run(tmp.path(), "claude-code", &prompt_payload(tmp.path()));
        let events = stored_events(&out);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].capture_mode, CaptureMode::LocalSemantic);
        assert!(
            format!("{:?}", events[0]).contains(SECRET),
            "the control: content is kept"
        );
        assert!(events[0].attrs.get("x_attemptdb_config_fallback").is_none());
    }

    // ---- §5.20: gaps keep what is known about them -------------------------

    fn gap_event(events: &[Event], class: &str) -> Event {
        events
            .iter()
            .find(|e| e.attrs.get("capture_gap").and_then(|v| v.as_str()) == Some(class))
            .unwrap_or_else(|| panic!("no {class} event in {events:?}"))
            .clone()
    }

    #[test]
    fn an_unparseable_payload_keeps_its_session_name_and_raw_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        let torn = r#"{"session_id":"sess-torn-1","hook_event_name":"PostToolUse","tool_input":{"command":"ls -la"#;
        let out = run(tmp.path(), "claude-code", torn);
        assert_eq!(out.event_kind, "unknown");
        let events = stored_events(&out);
        let ev = gap_event(&events, "invalid_json");
        assert_eq!(ev.provider_session_id, "sess-torn-1");
        assert_eq!(ev.provider_event_name, "PostToolUse");
        assert_eq!(ev.raw, Some(serde_json::Value::String(torn.to_string())));
        assert_eq!(
            ev.attrs.get("x_attemptdb_payload_bytes"),
            Some(&serde_json::json!(torn.len()))
        );
    }

    #[test]
    fn an_unparseable_payload_is_filed_under_the_directory_it_names() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().join("workspace");
        Database::create(
            &workspace.join(".attemptdb"),
            attemptdb_core::DeviceId::new(),
        )
        .unwrap();
        let torn = format!(
            r#"{{"session_id":"s-cwd","cwd":{},"tool_input":{{"x":"#,
            serde_json::to_string(&workspace.to_string_lossy()).unwrap()
        );
        let out = run(tmp.path(), "claude-code", &torn);
        assert_eq!(out.db_dir, workspace.join(".attemptdb"));
        assert_eq!(out.delivered, Delivery::Spool);
    }

    #[test]
    fn a_gap_under_metadata_only_keeps_the_session_but_not_the_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        let config_dir = tmp.path().join("data").join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            Config::path(&config_dir),
            br#"{"capture_mode":"metadata_only"}"#,
        )
        .unwrap();
        let torn = format!(r#"{{"session_id":"sess-private","prompt":"{SECRET}"#);
        let out = run(tmp.path(), "claude-code", &torn);
        let events = stored_events(&out);
        let ev = gap_event(&events, "invalid_json");
        assert_eq!(ev.provider_session_id, "sess-private");
        assert!(ev.raw.is_none() && ev.content.is_none());
        assert!(!format!("{ev:?}").contains(SECRET));
    }

    #[test]
    fn an_oversize_payload_keeps_a_bounded_head_and_the_session() {
        let tmp = tempfile::tempdir().unwrap();
        let head = r#"{"session_id":"sess-big","hook_event_name":"PostToolUse","tool_response":""#;
        let mut payload = head.to_string();
        payload.push_str(&"x".repeat(MAX_STDIN_BYTES + 10));
        let out = run(tmp.path(), "claude-code", &payload);
        assert!(out.error.is_none(), "{:?}", out.error);
        let events = stored_events(&out);
        let ev = gap_event(&events, "payload_truncated");
        assert_eq!(ev.provider_session_id, "sess-big");
        let raw = ev
            .raw
            .as_ref()
            .and_then(|v| v.as_str())
            .expect("raw head kept");
        assert_eq!(raw.len(), GAP_RAW_BYTES);
        assert!(raw.starts_with(head));
        assert_eq!(
            ev.attrs.get("x_attemptdb_raw_truncated"),
            Some(&serde_json::json!(true))
        );
        assert_eq!(
            ev.attrs.get("x_attemptdb_payload_bytes"),
            Some(&serde_json::json!(payload.len()))
        );
    }

    #[test]
    fn an_adapter_error_keeps_the_parsed_payload_and_names_the_class() {
        let tmp = tempfile::tempdir().unwrap();
        // No `hook_event_name` and no hint: the adapter cannot name the event.
        let out = run(
            tmp.path(),
            "claude-code",
            r#"{"session_id":"sess-nameless","cwd":"/home/dev/p","note":"kept"}"#,
        );
        assert_eq!(out.event_kind, "unknown");
        let events = stored_events(&out);
        let ev = gap_event(&events, "adapter_error");
        assert_eq!(ev.provider_session_id, "sess-nameless");
        assert_eq!(
            ev.attrs.get("adapter_error"),
            Some(&serde_json::json!("missing_event_name"))
        );
        assert_eq!(ev.raw.as_ref().unwrap()["note"], "kept");
    }

    #[test]
    fn scan_string_field_reads_what_a_torn_payload_still_says() {
        let scan = |s: &str, k: &str| scan_string_field(s.as_bytes(), k);
        assert_eq!(
            scan(r#"{"session_id":"abc"}"#, "session_id").as_deref(),
            Some("abc")
        );
        assert_eq!(
            scan("{ \"session_id\" :\n \"a b\\\"c\" ,", "session_id").as_deref(),
            Some("a b\"c")
        );
        // A key quoted inside a string value is not a key.
        assert_eq!(
            scan(
                r#"{"note":"has \"session_id\":\"fake\" inside","session_id":"real"}"#,
                "session_id"
            )
            .as_deref(),
            Some("real")
        );
        // Torn inside the value, wrong type, empty: nothing.
        assert_eq!(scan(r#"{"session_id":"abc"#, "session_id"), None);
        assert_eq!(scan(r#"{"session_id":12}"#, "session_id"), None);
        assert_eq!(scan(r#"{"session_id":""}"#, "session_id"), None);
        assert_eq!(scan("", "session_id"), None);
        // Past the scanned head: not looked for.
        let late = format!("{}{{\"session_id\":\"late\"}}", " ".repeat(SCAN_BYTES + 1));
        assert_eq!(scan_string_field(late.as_bytes(), "session_id"), None);
    }

    // ---- §5.11: stdin is read with deadlines and drained -------------------

    #[test]
    fn a_stdin_that_never_closes_is_cut_off_and_keeps_what_arrived() {
        let (reader, mut writer) = std::io::pipe().unwrap();
        writer.write_all(br#"{"session_id":"half"#).unwrap();
        let t0 = Instant::now();
        let read = read_bounded(
            reader,
            MAX_STDIN_BYTES,
            Duration::from_millis(300),
            Duration::from_secs(3),
        );
        assert!(t0.elapsed() < Duration::from_secs(2), "{:?}", t0.elapsed());
        assert!(read.timed_out && !read.eof);
        assert_eq!(read.bytes, br#"{"session_id":"half"#);
        drop(writer);
    }

    #[test]
    fn a_complete_payload_does_not_wait_for_a_stdin_that_stays_open() {
        let (reader, mut writer) = std::io::pipe().unwrap();
        writer
            .write_all(br#"{"session_id":"whole","tool_input":{"a":"}{\"x"}}"#)
            .unwrap();
        let t0 = Instant::now();
        let read = read_bounded(
            reader,
            MAX_STDIN_BYTES,
            Duration::from_secs(5),
            Duration::from_secs(10),
        );
        assert!(t0.elapsed() < Duration::from_secs(2), "{:?}", t0.elapsed());
        assert!(!read.timed_out, "a whole object is not a timeout");
        assert!(serde_json::from_slice::<serde_json::Value>(&read.bytes).is_ok());
        drop(writer);
    }

    #[test]
    fn an_oversize_payload_is_drained_so_the_writer_never_fails() {
        let (reader, mut writer) = std::io::pipe().unwrap();
        let max = 64 * 1024;
        let total = 3 * 1024 * 1024;
        let sender = std::thread::spawn(move || {
            let chunk = vec![b' '; 100_000];
            let mut sent = 0;
            while sent < total {
                let n = chunk.len().min(total - sent);
                writer.write_all(&chunk[..n])?;
                sent += n;
            }
            Ok::<usize, std::io::Error>(sent)
        });
        let read = read_bounded(reader, max, Duration::from_secs(5), Duration::from_secs(10));
        assert_eq!(
            sender.join().unwrap().unwrap(),
            total,
            "no EPIPE for the agent"
        );
        assert_eq!(read.bytes.len(), max + 1);
        assert_eq!(read.total_bytes(), total as u64);
        assert!(read.eof && !read.timed_out);
    }

    #[test]
    fn an_ordinary_event_stays_in_the_low_milliseconds() {
        let tmp = tempfile::tempdir().unwrap();
        let payload = prompt_payload(tmp.path());
        run(tmp.path(), "claude-code", &payload); // first use: identity, directories
        let mut times: Vec<u128> = (0..20)
            .map(|_| run(tmp.path(), "claude-code", &payload).elapsed_us)
            .collect();
        times.sort_unstable();
        let median_ms = times[times.len() / 2] / 1000;
        // Debug build on a loaded CI machine: generous, but a stdin wait or a
        // database open would be hundreds of times this.
        assert!(median_ms < 250, "median {median_ms} ms, all {times:?}");
    }
}
