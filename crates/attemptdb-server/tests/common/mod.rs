//! Shared harness for the server's end-to-end tests: a real listener, raw
//! HTTP/1.1 over TCP, real tenant databases on disk. What a client sees is
//! what these tests see.

#![allow(dead_code)]

use attemptdb_core::event::{EventContent, Provider};
use attemptdb_core::{
    CaptureMode, DeviceId, Event, EventId, EventKind, Outcome, PortablePath, ProjectRef, SessionId,
    Timestamp, ToolCategory, ToolRef,
};
use attemptdb_server::auth::digest_hex;
use attemptdb_server::{AppState, Server, ServerConfig};
use attemptdb_storage::{Database, OpenOptions, ScanFilter};
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// Device keys: one per tenant.
pub const KEY_ALPHA: &str = "k-alpha-d1";
pub const KEY_BETA: &str = "k-beta-d2";
/// Reader / admin-scoped keys (only in `start_with(...reader_keys())`).
pub const READER_ALPHA: &str = "r-alpha";
pub const READER_BETA: &str = "r-beta";
pub const ADMIN_ALPHA: &str = "a-alpha";
/// The operator's admin token (`start_admin`).
pub const ADMIN: &str = "admin-secret-token";

pub fn device(tag: &str) -> DeviceId {
    DeviceId::derive(&["server-test", tag])
}

/// The two device keys every test starts with.
pub fn device_keys() -> Vec<Value> {
    vec![
        json!({ "sha256": digest_hex(KEY_ALPHA), "tenant": "alpha", "device_id": device("d1"), "label": "alpha d1" }),
        json!({ "sha256": digest_hex(KEY_BETA),  "tenant": "beta",  "device_id": device("d2"), "label": "beta d2" }),
    ]
}

/// Reader keys for both tenants and an admin-scoped key for alpha.
pub fn reader_keys() -> Vec<Value> {
    vec![
        json!({ "sha256": digest_hex(READER_ALPHA), "tenant": "alpha", "device_id": device("web-alpha"), "label": "alpha web", "scope": "reader", "user_id": "usr_alpha" }),
        json!({ "sha256": digest_hex(READER_BETA),  "tenant": "beta",  "device_id": device("web-beta"),  "label": "beta web",  "scope": "reader" }),
        json!({ "sha256": digest_hex(ADMIN_ALPHA),  "tenant": "alpha", "device_id": device("ops-alpha"), "label": "alpha ops", "scope": "admin" }),
    ]
}

pub fn write_keys(dir: &Path, keys: &[Value]) -> PathBuf {
    let path = dir.join("keys.json");
    std::fs::write(
        &path,
        serde_json::to_vec_pretty(&json!({ "keys": keys })).unwrap(),
    )
    .unwrap();
    path
}

pub struct Running {
    pub addr: SocketAddr,
    pub data_dir: PathBuf,
    pub keys_file: PathBuf,
    pub state: Arc<AppState>,
    pub _tmp: Option<tempfile::TempDir>,
    pub stop: Option<tokio::sync::oneshot::Sender<()>>,
    pub task: tokio::task::JoinHandle<anyhow::Result<()>>,
}

pub struct StartOptions {
    pub max_open: usize,
    pub admin_token: Option<String>,
    pub keys: Vec<Value>,
    pub body_limit: usize,
    pub view_window_days: Option<u32>,
}

impl Default for StartOptions {
    fn default() -> Self {
        Self {
            max_open: 8,
            admin_token: None,
            keys: device_keys(),
            body_limit: 64 * 1024,
            view_window_days: None,
        }
    }
}

pub async fn start_with(opts: StartOptions) -> Running {
    let tmp = tempfile::tempdir().unwrap();
    let keys_file = write_keys(tmp.path(), &opts.keys);
    let data_dir = tmp.path().join("data");
    let config = ServerConfig {
        port: 0,
        data_dir: data_dir.clone(),
        keys_file,
        max_open: opts.max_open,
        body_limit: opts.body_limit,
        admin_token: opts.admin_token,
        view_window_days: opts.view_window_days,
        ..Default::default()
    };
    let mut running = spawn(config).await;
    running._tmp = Some(tmp);
    running
}

async fn spawn(config: ServerConfig) -> Running {
    let data_dir = config.data_dir.clone();
    let keys_file = config.keys_file.clone();
    let server = Server::bind(config).await.unwrap();
    let addr = server.addr();
    let state = Arc::clone(server.state());
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let task = tokio::spawn(server.run(async move {
        let _ = rx.await;
    }));
    Running {
        addr,
        data_dir,
        keys_file,
        state,
        _tmp: None,
        stop: Some(tx),
        task,
    }
}

/// A fresh process over an existing data directory and key file: what a
/// restart is. The caller keeps the temp dir alive.
#[allow(dead_code)]
pub async fn restart(data_dir: PathBuf, keys_file: PathBuf, max_open: usize) -> Running {
    restart_with(data_dir, keys_file, max_open, None).await
}

#[allow(dead_code)]
pub async fn restart_with(
    data_dir: PathBuf,
    keys_file: PathBuf,
    max_open: usize,
    view_window_days: Option<u32>,
) -> Running {
    spawn(ServerConfig {
        port: 0,
        data_dir,
        keys_file,
        max_open,
        view_window_days,
        ..Default::default()
    })
    .await
}

/// Device keys only, no admin surface.
pub async fn start(max_open: usize) -> Running {
    start_with(StartOptions {
        max_open,
        ..Default::default()
    })
    .await
}

/// Device keys plus the admin token.
pub async fn start_admin() -> Running {
    start_with(StartOptions {
        admin_token: Some(ADMIN.into()),
        ..Default::default()
    })
    .await
}

impl Running {
    /// Graceful shutdown; the temp dir stays alive so tests can inspect
    /// what the server left on disk.
    pub async fn stop(&mut self) {
        let _ = self.stop.take().expect("stop once").send(());
        tokio::time::timeout(Duration::from_secs(10), &mut self.task)
            .await
            .expect("server stopped")
            .expect("server task")
            .expect("server exit");
    }

    pub fn tenant_dir(&self, tenant: &str) -> PathBuf {
        self.data_dir.join("tenants").join(tenant)
    }
}

/// One HTTP/1.1 request; `Connection: close`, read to EOF.
pub fn http(
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> (u16, Value) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n");
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    if !body.is_empty() {
        req.push_str("Content-Type: application/json\r\n");
    }
    req.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));
    req.push_str(body);
    s.write_all(req.as_bytes()).unwrap();
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).unwrap();
    let text = String::from_utf8_lossy(&raw).to_string();
    let (head, body) = text.split_once("\r\n\r\n").expect("header terminator");
    let status: u16 = head.split_whitespace().nth(1).unwrap().parse().unwrap();
    let body = if head
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked")
    {
        dechunk(body)
    } else {
        body.to_string()
    };
    let json = if body.trim().is_empty() {
        Value::Null
    } else {
        serde_json::from_str(&body).unwrap_or(Value::String(body))
    };
    (status, json)
}

fn dechunk(body: &str) -> String {
    let mut out = String::new();
    let mut rest = body;
    while let Some((size_line, after)) = rest.split_once("\r\n") {
        let size = usize::from_str_radix(size_line.trim(), 16).unwrap_or(0);
        if size == 0 {
            break;
        }
        out.push_str(&after[..size]);
        rest = &after[size + 2..];
    }
    out
}

/// `POST /v1/sync` with an optional bearer key.
pub async fn post(addr: SocketAddr, key: Option<&str>, body: Value) -> (u16, Value) {
    let body = body.to_string();
    let key = key.map(str::to_string);
    tokio::task::spawn_blocking(move || {
        let auth = key.map(|k| format!("Bearer {k}"));
        let headers: Vec<(&str, &str)> = auth
            .as_deref()
            .map(|a| vec![("Authorization", a)])
            .unwrap_or_default();
        http(addr, "POST", "/v1/sync", &headers, &body)
    })
    .await
    .unwrap()
}

/// Any method and path with a bearer key; `Value::Null` sends no body.
pub async fn call(
    addr: SocketAddr,
    method: &str,
    path: &str,
    key: &str,
    body: Value,
) -> (u16, Value) {
    let (method, path, key) = (method.to_string(), path.to_string(), key.to_string());
    let body = if body.is_null() {
        String::new()
    } else {
        body.to_string()
    };
    tokio::task::spawn_blocking(move || {
        let auth = format!("Bearer {key}");
        http(addr, &method, &path, &[("Authorization", &auth)], &body)
    })
    .await
    .unwrap()
}

/// `GET path` with a bearer key.
pub async fn get(addr: SocketAddr, path: &str, key: &str) -> (u16, Value) {
    call(addr, "GET", path, key, Value::Null).await
}

/// Admin surface: bearer is the operator token, not a key.
pub async fn admin(
    addr: SocketAddr,
    method: &'static str,
    path: String,
    token: Option<&'static str>,
    body: Value,
) -> (u16, Value) {
    let body = if body.is_null() {
        String::new()
    } else {
        body.to_string()
    };
    tokio::task::spawn_blocking(move || {
        let auth = token.map(|t| format!("Bearer {t}"));
        let headers: Vec<(&str, &str)> = auth
            .as_deref()
            .map(|a| vec![("Authorization", a)])
            .unwrap_or_default();
        http(addr, method, &path, &headers, &body)
    })
    .await
    .unwrap()
}

/// Events as a client would upload them: full content, some attrs that break
/// the contract, one with a local `source_seq`.
pub fn events(dev: DeviceId, n: usize, tag: &str) -> Vec<Event> {
    (0..n)
        .map(|i| {
            let mut ev = Event::new(
                dev,
                Provider::ClaudeCode,
                "PostToolUse",
                EventKind::ToolCallFinished,
                ProjectRef::derive("/home/dev/example/project", None, &dev),
                format!("session-{tag}"),
                CaptureMode::LocalSemantic,
                "server-test/0.1",
            );
            ev.attrs.insert("x_test_index".into(), json!(i));
            ev.attrs
                .insert("prompt".into(), json!("rewrite the auth module please"));
            ev.content = Some(EventContent {
                tool_output: Some(Value::String("secret output".repeat(4))),
                ..Default::default()
            });
            ev.raw = Some(json!({"tool_response": "secret raw"}));
            if i == 0 {
                ev.source_seq = 7;
            }
            ev
        })
        .collect()
}

pub fn batch(dev: DeviceId, id: &str, events: &[Event]) -> Value {
    json!({
        "sync_version": 1,
        "device_id": dev,
        "batch_id": id,
        "capture_mode": "local_semantic",
        "events": events,
    })
}

pub fn inference_batch(dev: DeviceId, kind: &str, items: Value) -> Value {
    json!({
        "sync_version": 1,
        "schema": "attemptdb.inference/v1",
        "device_id": dev,
        "batch_id": "inf-1",
        "kind": kind,
        "algorithm_version": "tier1-v0",
        "computed_at": Timestamp::now(),
        "items": items,
    })
}

/// Read a tenant's database while the server holds the writer lock.
pub fn scan(dir: &Path) -> Vec<Event> {
    let db = Database::open(
        dir,
        OpenOptions {
            read_only: true,
            ..Default::default()
        },
    )
    .unwrap();
    db.scan(&ScanFilter::default()).unwrap()
}

// ---------------------------------------------------------------------------
// A realistic multi-session stream (the shape of the UI's fixtures).
// ---------------------------------------------------------------------------

pub const ROOT: &str = "/home/dev/work/repo";
pub const REMOTE: &str = "git@github.com:acme/repo.git";
/// 2026-08-28T08:00:00Z in microseconds.
pub const BASE_US: i64 = 1_787_904_000_000_000;

pub fn at(secs: i64) -> Timestamp {
    Timestamp::from_micros(BASE_US + secs * 1_000_000)
}

#[derive(Clone, Debug)]
pub struct Sess {
    pub provider: Provider,
    pub provider_session_id: String,
    pub session_id: SessionId,
}

impl Sess {
    pub fn new(provider: Provider, provider_session_id: &str) -> Self {
        Self {
            session_id: SessionId::derive(&[provider.as_str(), provider_session_id]),
            provider,
            provider_session_id: provider_session_id.to_string(),
        }
    }

    pub fn claude(id: &str) -> Self {
        Self::new(Provider::ClaudeCode, id)
    }

    pub fn codex(id: &str) -> Self {
        Self::new(Provider::Codex, id)
    }

    pub fn readable(&self) -> String {
        format!("ses_{}", self.session_id)
    }
}

#[derive(Clone, Debug)]
pub struct Tool<'a> {
    pub name: &'a str,
    pub category: ToolCategory,
    pub call_id: Option<&'a str>,
    pub paths: &'a [&'a str],
}

impl<'a> Tool<'a> {
    pub fn edit(call_id: Option<&'a str>, paths: &'a [&'a str]) -> Self {
        Self {
            name: "Edit",
            category: ToolCategory::FileEdit,
            call_id,
            paths,
        }
    }

    pub fn read(call_id: Option<&'a str>, paths: &'a [&'a str]) -> Self {
        Self {
            name: "Read",
            category: ToolCategory::FileRead,
            call_id,
            paths,
        }
    }

    pub fn shell(call_id: Option<&'a str>) -> Self {
        Self {
            name: "Bash",
            category: ToolCategory::Shell,
            call_id,
            paths: &[],
        }
    }

    pub fn apply_patch(call_id: Option<&'a str>, paths: &'a [&'a str]) -> Self {
        Self {
            name: "apply_patch",
            category: ToolCategory::FileEdit,
            call_id,
            paths,
        }
    }

    fn to_ref(&self) -> ToolRef {
        ToolRef {
            name: self.name.to_string(),
            category: self.category,
            call_id: self.call_id.map(str::to_string),
        }
    }
}

/// Builds a coherent stream for one device and one project.
pub struct Stream {
    device: DeviceId,
    project: ProjectRef,
    pub events: Vec<Event>,
}

impl Stream {
    pub fn new(device: DeviceId, root: &str, remote: Option<&str>) -> Self {
        Self {
            device,
            project: ProjectRef::derive(root, remote, &device),
            events: Vec::new(),
        }
    }

    pub fn project(&self) -> &ProjectRef {
        &self.project
    }

    fn push(&mut self, s: &Sess, kind: EventKind, name: &str, t: Timestamp) -> &mut Event {
        let mut ev = Event::new(
            self.device,
            s.provider.clone(),
            name,
            kind,
            self.project.clone(),
            s.provider_session_id.clone(),
            CaptureMode::LocalSemantic,
            "server-test/0.1",
        );
        ev.observed_at = t;
        ev.captured_at = t;
        self.events.push(ev);
        self.events.last_mut().expect("just pushed")
    }

    fn portable(&self, path: &str) -> PortablePath {
        PortablePath::from_raw(
            &format!("{}/{path}", self.project.root),
            Some(&self.project.root),
        )
    }

    pub fn session_started(&mut self, s: &Sess, t: Timestamp) -> EventId {
        let ev = self.push(s, EventKind::SessionStarted, "SessionStart", t);
        ev.attrs.insert("source".into(), Value::from("startup"));
        ev.event_id
    }

    pub fn session_ended(&mut self, s: &Sess, t: Timestamp, reason: &str) -> EventId {
        let ev = self.push(s, EventKind::SessionEnded, "SessionEnd", t);
        ev.attrs.insert("reason".into(), Value::from(reason));
        ev.event_id
    }

    pub fn prompt(&mut self, s: &Sess, t: Timestamp, text: &str) -> EventId {
        let ev = self.push(s, EventKind::PromptSubmitted, "UserPromptSubmit", t);
        ev.attrs.insert(
            "prompt_chars".into(),
            Value::from(text.chars().count() as u64),
        );
        ev.content = Some(EventContent {
            prompt: Some(text.to_string()),
            ..Default::default()
        });
        ev.event_id
    }

    fn tool_event(
        &mut self,
        s: &Sess,
        kind: EventKind,
        name: &str,
        t: Timestamp,
        tool: &Tool<'_>,
    ) -> &mut Event {
        let paths: Vec<PortablePath> = tool.paths.iter().map(|p| self.portable(p)).collect();
        let ev = self.push(s, kind, name, t);
        ev.tool = Some(tool.to_ref());
        ev.paths = paths;
        ev
    }

    pub fn tool_start(&mut self, s: &Sess, t: Timestamp, tool: &Tool<'_>) -> EventId {
        self.tool_event(s, EventKind::ToolCallStarted, "PreToolUse", t, tool)
            .event_id
    }

    pub fn tool_finish(&mut self, s: &Sess, t: Timestamp, tool: &Tool<'_>) -> EventId {
        let ev = self.tool_event(s, EventKind::ToolCallFinished, "PostToolUse", t, tool);
        ev.outcome = Some(Outcome::success());
        ev.event_id
    }

    pub fn tool_failed(&mut self, s: &Sess, t: Timestamp, tool: &Tool<'_>, class: &str) -> EventId {
        let ev = self.tool_event(s, EventKind::ToolCallFailed, "PostToolUseFailure", t, tool);
        ev.outcome = Some(Outcome::failure(Some(class.to_string())));
        ev.event_id
    }

    pub fn stop(&mut self, s: &Sess, t: Timestamp) -> EventId {
        self.push(s, EventKind::TurnStopped, "Stop", t).event_id
    }

    pub fn permission_requested(&mut self, s: &Sess, t: Timestamp, tool: &Tool<'_>) -> EventId {
        let ev = self.push(s, EventKind::PermissionRequested, "PermissionRequest", t);
        ev.tool = Some(tool.to_ref());
        ev.event_id
    }

    pub fn build(self) -> Vec<Event> {
        self.events
    }
}

/// Handles into the reference story.
pub struct Scenario {
    pub events: Vec<Event>,
    pub project: ProjectRef,
    /// Closed; turn 1 has a failed Edit superseded by a retry, then tests.
    pub claude: Sess,
    /// Closed; starts three minutes after `claude` on the same file.
    pub codex: Sess,
    /// Open and blocked on a permission request.
    pub blocked: Sess,
    pub permission_event: EventId,
    pub edit_fail_end: EventId,
}

/// Three sessions of one device: a Claude session with a failure and a
/// retry, a Codex session that takes over the same file (a handoff), and an
/// open Claude session whose latest event is a pending permission request.
pub fn scenario(dev: DeviceId) -> Scenario {
    let mut b = Stream::new(dev, ROOT, Some(REMOTE));
    let claude = Sess::claude("claude-session-1");
    let codex = Sess::codex("codex-thread-1");
    let blocked = Sess::claude("claude-session-2");
    let parser = ["src/parser.rs"];

    b.session_started(&claude, at(0));
    b.prompt(&claude, at(5), "Fix the failing parser test");
    b.tool_start(&claude, at(6), &Tool::read(Some("c1"), &parser));
    b.tool_finish(&claude, at(7), &Tool::read(Some("c1"), &parser));
    b.tool_start(&claude, at(10), &Tool::edit(Some("c2"), &parser));
    let edit_fail_end = b.tool_failed(
        &claude,
        at(11),
        &Tool::edit(Some("c2"), &parser),
        "string_mismatch",
    );
    b.tool_start(&claude, at(20), &Tool::edit(Some("c3"), &parser));
    b.tool_finish(&claude, at(21), &Tool::edit(Some("c3"), &parser));
    b.tool_start(&claude, at(25), &Tool::shell(Some("c4")));
    b.tool_finish(&claude, at(40), &Tool::shell(Some("c4")));
    b.stop(&claude, at(45));
    b.session_ended(&claude, at(80), "prompt_input_exit");

    b.session_started(&codex, at(260));
    b.prompt(&codex, at(265), "Continue the parser fix and run the tests");
    b.tool_start(&codex, at(270), &Tool::apply_patch(Some("x1"), &parser));
    b.tool_finish(&codex, at(272), &Tool::apply_patch(Some("x1"), &parser));
    b.stop(&codex, at(300));
    b.session_ended(&codex, at(310), "exit");

    b.session_started(&blocked, at(600));
    b.prompt(&blocked, at(605), "Refactor the lexer");
    b.tool_start(
        &blocked,
        at(610),
        &Tool::edit(Some("b1"), &["src/lexer.rs"]),
    );
    b.tool_finish(
        &blocked,
        at(611),
        &Tool::edit(Some("b1"), &["src/lexer.rs"]),
    );
    let permission_event = b.permission_requested(&blocked, at(620), &Tool::shell(Some("b2")));

    Scenario {
        project: b.project().clone(),
        events: b.build(),
        claude,
        codex,
        blocked,
        permission_event,
        edit_fail_end,
    }
}
