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
use attemptdb_core::event::{Provider, ProjectRef};
use attemptdb_core::{Event, EventKind, Timestamp};
use attemptdb_storage::SpoolWriter;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Instant;

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

/// Read stdin (bounded) for the hook entrypoint.
pub fn read_stdin() -> Vec<u8> {
    let mut buf = Vec::with_capacity(8 * 1024);
    let stdin = std::io::stdin();
    let mut handle = stdin.lock().take(MAX_STDIN_BYTES as u64);
    let _ = handle.read_to_end(&mut buf);
    buf
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
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_inner(&input, &provider, &mut outcome))) {
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
        Self { on, t0, last: t0, stages: Vec::new() }
    }

    fn mark(&mut self, stage: &'static str) {
        if self.on {
            let now = Instant::now();
            self.stages.push((stage, now.duration_since(self.last).as_micros()));
            self.last = now;
        }
    }

    fn finish(&self) {
        if self.on {
            let parts: Vec<String> = self.stages.iter().map(|(s, us)| format!("{s}={us}us")).collect();
            eprintln!("attempt-hook trace total={}us {}", self.t0.elapsed().as_micros(), parts.join(" "));
        }
    }
}

fn run_inner(input: &HookInput<'_>, provider: &Provider, out: &mut HookOutcome) -> Result<(), String> {
    let mut trace = Trace::new(HOOK_STARTED.with(|c| c.get()).unwrap_or_else(Instant::now));
    let (payload, parse_error) = match serde_json::from_slice::<serde_json::Value>(&input.payload_bytes) {
        Ok(v) if v.is_object() => (v, None),
        Ok(_) => (serde_json::json!({}), Some("payload_not_object")),
        Err(_) if input.payload_bytes.iter().all(u8::is_ascii_whitespace) => (serde_json::json!({}), Some("empty_payload")),
        Err(_) => (serde_json::json!({}), Some("invalid_json")),
    };

    let cwd: PathBuf = payload
        .get("cwd")
        .and_then(|v| v.as_str())
        .map(PathBuf::from)
        .or_else(|| input.cwd_hint.clone())
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."));

    trace.mark("parse");
    let locator = Locator::resolve(&cwd, input.data_dir_override.as_deref(), input.db_override.as_deref());
    out.db_dir = locator.db_dir.clone();
    let config = Config::load_or_default(&locator.paths.config_dir);
    let device = DeviceRecord::load_or_create(&locator.paths.data_dir).map_err(|e| e.to_string())?;
    trace.mark("locate");

    let git = git_info(&cwd);
    trace.mark("git");
    let root = git.as_ref().map(|g| g.root.clone()).unwrap_or_else(|| cwd.clone());
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
            let adapter = adapter_for(provider).ok_or_else(|| format!("no adapter for provider {provider}"))?;
            match adapter.normalise(&ctx, input.event_hint, &payload) {
                Ok(ev) => ev,
                Err(e) => {
                    let mut ev = unknown_event(&ctx, provider, input.event_hint, &payload);
                    ev.attrs.insert("adapter_error".into(), serde_json::json!(e.to_string()));
                    ev
                }
            }
        }
        Some(class) => {
            let mut ev = unknown_event(&ctx, provider, input.event_hint, &payload);
            ev.attrs.insert("payload_error".into(), serde_json::json!(class));
            ev.attrs.insert("payload_bytes".into(), serde_json::json!(input.payload_bytes.len()));
            ev
        }
    };
    if payload.get("_attemptdb_capture_test").and_then(|v| v.as_bool()) == Some(true) {
        event.kind = EventKind::CaptureTest;
    }
    if !config.keep_raw_payload {
        event.raw = None;
    }
    event.apply_capture_mode();
    // Hook overhead up to this point (parse + normalise), in microseconds.
    // Content-free, and the basis for the "hook p95 < 10 ms" gate.
    if let Some(t0) = HOOK_STARTED.with(|c| c.get()) {
        event.attrs.insert("hook_us".into(), serde_json::json!(t0.elapsed().as_micros() as u64));
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

fn unknown_event(ctx: &CaptureContext, provider: &Provider, hint: Option<&str>, payload: &serde_json::Value) -> Event {
    let name = payload
        .get("hook_event_name")
        .and_then(|v| v.as_str())
        .or(hint)
        .unwrap_or("unknown");
    let session = payload
        .get("session_id")
        .or_else(|| payload.get("conversation_id"))
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let mut ev = Event::new(
        ctx.device_id,
        provider.clone(),
        name,
        EventKind::Unknown,
        ctx.project.clone(),
        session,
        ctx.capture_mode,
        ADAPTER_VERSION,
    );
    ev.captured_at = ctx.captured_at;
    ev.observed_at = ctx.captured_at;
    ev.hook_version = ctx.hook_version.clone();
    ev
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
    let locator = Locator::resolve(
        input.cwd_hint.as_deref().unwrap_or(Path::new(".")),
        input.data_dir_override.as_deref(),
        input.db_override.as_deref(),
    );
    let dir = &locator.paths.log_dir;
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let path = dir.join("hook.log");
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        use std::io::Write;
        let _ = writeln!(f, "{} provider={} err={}", Timestamp::now(), input.provider_id, err);
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
    use attemptdb_storage::{Database, OpenOptions, ScanFilter};

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

        let mut db = Database::open(&out.db_dir, OpenOptions { create: true, ..Default::default() }).unwrap();
        let r = db.import_spool().unwrap();
        assert_eq!(r.accepted, 4);
        let events = db.scan(&ScanFilter::default()).unwrap();
        assert_eq!(events.len(), 4);
        assert!(events.iter().any(|e| e.kind == EventKind::CaptureTest));
        assert!(events[0].attrs.get("hook_us").is_some());
        assert_eq!(events[0].tool.as_ref().unwrap().name, "Bash");
        assert!(events.iter().all(|e| e.hook_version.is_some()));
    }
}
