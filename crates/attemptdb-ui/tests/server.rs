//! End-to-end tests: a temporary on-disk database with the synthetic
//! reference story, the server on an ephemeral loopback port, and a tiny
//! HTTP/1.1 client over `std::net::TcpStream`.

mod common;

use attemptdb_core::{DeviceId, Event, EventId};
use attemptdb_storage::{Database, OpenOptions, ScanFilter};
use attemptdb_ui::export::{ExportOptions, render_database};
use attemptdb_ui::{COOKIE_NAME, CSP, ScopeArgs, Server, UiConfig};
use common::{Scenario, Sess, Stream, at, ui_scenario};
use serde_json::Value;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

struct Fixture {
    _tmp: tempfile::TempDir,
    db_dir: PathBuf,
    data_dir: PathBuf,
    scenario: Scenario,
}

fn write_db(db_dir: &Path, events: &[Event]) {
    std::fs::create_dir_all(db_dir.parent().unwrap()).unwrap();
    Database::create(db_dir, DeviceId::derive(&["test-device"])).unwrap();
    let mut db = Database::open(db_dir, OpenOptions::default()).unwrap();
    db.ingest(events.to_vec()).unwrap();
    db.flush().unwrap();
}

fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let db_dir = tmp.path().join("db").join(".attemptdb");
    let data_dir = tmp.path().join("data");
    let scenario = ui_scenario();
    write_db(&db_dir, &scenario.events);
    Fixture {
        _tmp: tmp,
        db_dir,
        data_dir,
        scenario,
    }
}

fn config(f: &Fixture) -> UiConfig {
    UiConfig {
        data_dir: Some(f.data_dir.clone()),
        ..UiConfig::new(&f.db_dir)
    }
}

// ---------------------------------------------------------------------------
// A minimal blocking HTTP client
// ---------------------------------------------------------------------------

struct Resp {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

impl Resp {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or_else(|e| panic!("not JSON ({e}): {}", self.body))
    }
}

fn raw_request(
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Option<&str>,
) -> Resp {
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(5)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    let mut req = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n",
        addr.port()
    );
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    if let Some(b) = body {
        req.push_str(&format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n",
            b.len()
        ));
    }
    req.push_str("\r\n");
    if let Some(b) = body {
        req.push_str(b);
    }
    stream.write_all(req.as_bytes()).unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).unwrap();
    let text = String::from_utf8_lossy(&raw).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").expect("header/body split");
    let mut lines = head.lines();
    let status_line = lines.next().unwrap();
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect();
    // Bodies here are small and never chunked (Connection: close).
    let body = if headers_has_chunked(&headers) {
        dechunk(body)
    } else {
        body.to_string()
    };
    Resp {
        status,
        headers,
        body,
    }
}

fn headers_has_chunked(h: &[(String, String)]) -> bool {
    h.iter()
        .any(|(k, v)| k.eq_ignore_ascii_case("transfer-encoding") && v.contains("chunked"))
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

struct Running {
    addr: SocketAddr,
    token: String,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    task: Option<tokio::task::JoinHandle<anyhow::Result<()>>>,
}

impl Running {
    fn cookie(&self) -> String {
        format!("{COOKIE_NAME}={}", self.token)
    }

    async fn request(
        &self,
        method: &str,
        path: &str,
        with_cookie: bool,
        body: Option<&str>,
    ) -> Resp {
        let addr = self.addr;
        let method = method.to_string();
        let path = path.to_string();
        let cookie = self.cookie();
        let body = body.map(str::to_string);
        tokio::task::spawn_blocking(move || {
            let headers: Vec<(&str, &str)> = if with_cookie {
                vec![("Cookie", cookie.as_str())]
            } else {
                Vec::new()
            };
            raw_request(addr, &method, &path, &headers, body.as_deref())
        })
        .await
        .unwrap()
    }

    async fn get(&self, path: &str) -> Resp {
        self.request("GET", path, true, None).await
    }

    /// Read an event-stream until the first complete event, then hang up.
    /// The stream never ends on its own, so the client must stop reading.
    async fn sse(&self, path: &str, timeout: Duration) -> String {
        let addr = self.addr;
        let path = path.to_string();
        let cookie = self.cookie();
        tokio::task::spawn_blocking(move || {
            let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(5)).unwrap();
            stream.set_read_timeout(Some(timeout)).unwrap();
            let req = format!(
                "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nCookie: {cookie}\r\nAccept: text/event-stream\r\n\r\n",
                addr.port()
            );
            stream.write_all(req.as_bytes()).unwrap();
            let mut out = String::new();
            let mut buf = [0u8; 512];
            let deadline = std::time::Instant::now() + timeout;
            while std::time::Instant::now() < deadline {
                match stream.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        out.push_str(&String::from_utf8_lossy(&buf[..n]));
                        if out.contains("data: ") && out.trim_end().ends_with('}') {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
            out
        })
        .await
        .unwrap()
    }

    async fn stop(mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(task) = self.task.take() {
            let _ = tokio::time::timeout(Duration::from_secs(10), task).await;
        }
    }
}

async fn start(f: &Fixture) -> Running {
    let server = Server::bind(config(f)).await.unwrap();
    let addr = server.addr();
    let token = server.token().to_string();
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let task = tokio::spawn(server.run(async move {
        let _ = rx.await;
    }));
    Running {
        addr,
        token,
        shutdown: Some(tx),
        task: Some(task),
    }
}

fn ses(s: &Sess) -> String {
    format!("ses_{}", s.session_id)
}

/// Every attempt in the timeline, flattened.
fn attempts(timeline: &Value) -> Vec<Value> {
    timeline["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|s| s["turns"].as_array().unwrap().iter())
        .flat_map(|t| t["attempts"].as_array().unwrap().iter())
        .cloned()
        .collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejects_without_token_and_accepts_cookie() {
    let f = fixture();
    let s = start(&f).await;

    let r = s.request("GET", "/", false, None).await;
    assert_eq!(r.status, 401, "{}", r.body);
    assert!(r.header("Set-Cookie").is_none());
    let r = s.request("GET", "/api/status", false, None).await;
    assert_eq!(r.status, 401);
    let r = s.request("GET", "/timeline?token=nope", false, None).await;
    assert_eq!(r.status, 401);
    let r = s
        .request(
            "POST",
            "/api/query",
            false,
            Some(r#"{"statement":"SELECT 1"}"#),
        )
        .await;
    assert_eq!(r.status, 401);

    // The token sets the cookie and redirects to the same URL without it.
    let path = format!("/timeline?limit=5&token={}", s.token);
    let r = s.request("GET", &path, false, None).await;
    assert_eq!(r.status, 303, "{}", r.body);
    assert_eq!(r.header("Location"), Some("/timeline?limit=5"));
    let cookie = r.header("Set-Cookie").expect("cookie set");
    assert!(
        cookie.starts_with(&format!("{COOKIE_NAME}={}", s.token)),
        "{cookie}"
    );
    assert!(cookie.contains("HttpOnly"), "{cookie}");
    assert!(cookie.contains("SameSite=Strict"), "{cookie}");

    // The cookie alone is enough afterwards.
    let r = s.get("/").await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert!(r.body.contains(attemptdb_project::ALGORITHM_VERSION));
    assert!(r.body.contains(
        "attempts, blockers and handoffs are inferences with evidence; events are facts"
    ));
    assert!(r.body.contains(&f.db_dir.display().to_string()));

    // A wrong cookie is refused.
    let addr = s.addr;
    let r = tokio::task::spawn_blocking(move || {
        raw_request(addr, "GET", "/", &[("Cookie", "attemptdb_ui=0000")], None)
    })
    .await
    .unwrap();
    assert_eq!(r.status, 401);

    // A foreign Host header is refused (DNS rebinding guard).
    let r = tokio::task::spawn_blocking(move || {
        let mut stream = TcpStream::connect(addr).unwrap();
        stream
            .write_all(b"GET / HTTP/1.1\r\nHost: evil.example\r\nConnection: close\r\n\r\n")
            .unwrap();
        let mut out = String::new();
        stream.read_to_string(&mut out).unwrap();
        out
    })
    .await
    .unwrap();
    assert!(r.starts_with("HTTP/1.1 403"), "{r}");
    s.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn security_headers_on_every_response() {
    let f = fixture();
    let s = start(&f).await;
    for (path, with_cookie) in [
        ("/", true),
        ("/api/status", true),
        ("/", false),
        ("/assets/app.css", false),
        ("/card.svg", true),
        ("/work", true),
        ("/attention", true),
        ("/nope", true),
    ] {
        let r = s.request("GET", path, with_cookie, None).await;
        assert_eq!(r.header("Content-Security-Policy"), Some(CSP), "{path}");
        assert!(CSP.contains("default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:"));
        assert_eq!(
            r.header("X-Content-Type-Options"),
            Some("nosniff"),
            "{path}"
        );
        assert_eq!(r.header("Referrer-Policy"), Some("no-referrer"), "{path}");
        assert_eq!(r.header("X-Frame-Options"), Some("DENY"), "{path}");
    }
    // Assets are served without auth and carry no data.
    let css = s.request("GET", "/assets/app.css", false, None).await;
    assert_eq!(css.status, 200);
    assert!(css.header("Content-Type").unwrap().starts_with("text/css"));
    let js = s.request("GET", "/assets/app.js", false, None).await;
    assert_eq!(js.status, 200);
    assert!(!js.body.contains("innerHTML"));
    // No external asset anywhere in a page.
    let page = s.get("/timeline").await.body;
    assert!(
        !page.contains("http://") && !page.contains("https://"),
        "external reference in page"
    );
    s.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_timeline_lists_the_failed_attempt() {
    let f = fixture();
    let s = start(&f).await;
    let r = s.get("/api/timeline").await;
    assert_eq!(r.status, 200, "{}", r.body);
    let j = r.json();
    assert_eq!(j["inference_version"], attemptdb_project::ALGORITHM_VERSION);
    assert_eq!(j["total_sessions"], 2);
    assert_eq!(j["sessions"].as_array().unwrap().len(), 2);
    let all = attempts(&j);
    assert_eq!(all.len(), 4);
    let failed: Vec<&Value> = all
        .iter()
        .filter(|a| a["outcome"] == "superseded" || a["outcome"] == "failed")
        .collect();
    assert_eq!(failed.len(), 1, "{all:?}");
    let a = failed[0];
    assert!(a["attempt_id"].as_str().unwrap().starts_with("att_"));
    assert_eq!(a["failure_class"], "string_mismatch");
    assert!(a["superseded_by"].as_str().unwrap().starts_with("att_"));
    assert!(
        a["evidence"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e.as_str().unwrap().starts_with("ev_"))
    );
    assert_eq!(a["confidence"], 0.9);
    // Filters: session scope, captured_only, pagination.
    let one = s
        .get(&format!(
            "/api/timeline?session={}&captured_only=1",
            ses(&f.scenario.codex)
        ))
        .await
        .json();
    assert_eq!(one["total_sessions"], 1);
    assert_eq!(one["sessions"][0]["provider"], "codex");
    let paged = s.get("/api/timeline?limit=1&page=2").await.json();
    assert_eq!(paged["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(paged["page"], 2);
    // Handoff between the two agents.
    assert_eq!(j["handoffs"].as_array().unwrap().len(), 1);
    assert!(
        j["work_units"].as_array().is_some_and(|w| !w.is_empty()),
        "{j}"
    );
    assert_eq!(j["handoffs"][0]["from_provider"], "claude_code");
    s.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn attempt_page_shows_failure_class_and_escapes_prompts() {
    let f = fixture();
    let s = start(&f).await;
    let j = s.get("/api/timeline").await.json();
    let all = attempts(&j);
    let failed = all.iter().find(|a| a["outcome"] == "superseded").unwrap();
    let id = failed["attempt_id"].as_str().unwrap();
    let r = s.get(&format!("/attempt/{id}")).await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert!(r.body.contains("string_mismatch"), "failure class missing");
    assert!(r.body.contains("WHY "), "why statement missing");
    assert!(r.body.contains("TRACE "), "trace statement missing");
    assert!(r.body.contains("<svg"), "no inline DAG");
    assert!(
        r.body.contains("SHOW EVIDENCE FOR"),
        "evidence statement missing"
    );
    assert!(r.body.contains("Fix the failing parser test"));
    // Short ids resolve too.
    let short = &id[..12];
    assert_eq!(s.get(&format!("/attempt/{short}")).await.status, 200);
    assert_eq!(s.get("/attempt/att_ffffffff").await.status, 404);
    assert_eq!(s.get("/attempt/%3Cscript%3E").await.status, 400);

    // The prompt containing <script> is escaped wherever it appears.
    let script_attempt = all
        .iter()
        .find(|a| a["turn_index"] == 2 && a["session_id"] == ses(&f.scenario.claude))
        .unwrap();
    let sid = script_attempt["attempt_id"].as_str().unwrap();
    for path in [
        format!("/attempt/{sid}"),
        "/timeline".to_string(),
        format!("/session/{}", ses(&f.scenario.claude)),
    ] {
        let body = s.get(&path).await.body;
        assert!(!body.contains("<script>alert"), "{path} leaks raw script");
        assert!(
            body.contains("&lt;script&gt;alert(&#39;xss&#39;)"),
            "{path} lost the prompt"
        );
    }
    // The API returns the prompt as data, not markup.
    let api = s.get(&format!("/api/attempt/{sid}")).await.json();
    assert_eq!(
        api["objective"],
        "Now document the parser module <script>alert('xss')</script>"
    );
    assert!(api["trace"]["rows"].as_array().is_some());
    assert!(api["evidence_events"]["rows"].as_array().unwrap().len() >= 3);
    s.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_query_is_read_only() {
    let f = fixture();
    let s = start(&f).await;
    for bad in [
        r#"{"statement":"INSERT INTO events VALUES (1)"}"#,
        r#"{"statement":"SELECT 1; DROP TABLE events"}"#,
        r#"{"statement":"CREATE TABLE x AS SELECT 1"}"#,
        r#"{"statement":"COPY events TO '/tmp/x'"}"#,
        r#"{"statement":""}"#,
    ] {
        let r = s.request("POST", "/api/query", true, Some(bad)).await;
        assert_eq!(r.status, 400, "{bad}: {}", r.body);
        let j = r.json();
        assert!(j["error"].as_str().unwrap().len() > 5, "{bad}: {}", r.body);
    }
    let r = s
        .request(
            "POST",
            "/api/query",
            true,
            Some(r#"{"statement":"SELECT count(*) AS n FROM events"}"#),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.body);
    let j = r.json();
    assert_eq!(j["rows"][0]["n"], f.scenario.events.len() as u64);
    assert_eq!(j["columns"][0], "n");
    let r = s
        .request(
            "POST",
            "/api/query",
            true,
            Some(r#"{"statement":"SHOW FAILED ATTEMPTS","format":"table"}"#),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.body);
    let j = r.json();
    assert!(j["text"].as_str().unwrap().contains("string_mismatch"));
    assert_eq!(j["row_count"], 1);
    let r = s
        .request(
            "POST",
            "/api/query",
            true,
            Some(r#"{"statement":"SHOW FOO"}"#),
        )
        .await;
    assert_eq!(r.status, 400);
    assert!(
        r.json()["error"].as_str().unwrap().contains("^"),
        "caret rendering"
    );
    // The GET console renders the same result server-side.
    let page = s.get("/query?statement=SHOW%20FAILED%20ATTEMPTS").await;
    assert_eq!(page.status, 200);
    assert!(page.body.contains("string_mismatch"));
    let page = s.get("/query?statement=DROP%20TABLE%20events").await;
    assert!(page.body.contains("read-only"));
    s.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_page_and_endpoint_renders() {
    let f = fixture();
    let s = start(&f).await;
    let claude = ses(&f.scenario.claude);
    let j = s.get("/api/timeline").await.json();
    let att = attempts(&j)[0]["attempt_id"].as_str().unwrap().to_string();
    let ev = format!("ev_{}", f.scenario.edit_fail_end);
    let pages = [
        "/".to_string(),
        "/timeline".to_string(),
        "/timeline?all_sessions=1&limit=1".to_string(),
        format!("/session/{claude}"),
        format!("/attempt/{att}"),
        format!("/evidence/{ev}"),
        format!("/evidence/{}", &ev[..11]),
        "/work".to_string(),
        "/attention".to_string(),
        "/failures".to_string(),
        "/handoffs".to_string(),
        "/why".to_string(),
        format!("/why?subject={claude}"),
        format!("/why?subject={att}"),
        "/state".to_string(),
        "/state?at=2026-08-28T08:00:30Z".to_string(),
        "/query".to_string(),
        "/query?statement=WHAT%20IS%20project%20DOING%20NOW&format=json".to_string(),
        format!("/timeline?project={}", "acme/repo"),
    ];
    for p in &pages {
        let r = s.get(p).await;
        assert_eq!(r.status, 200, "{p}: {}", r.body);
        assert!(
            r.header("Content-Type").unwrap().starts_with("text/html"),
            "{p}"
        );
        assert!(r.body.contains(attemptdb_project::ALGORITHM_VERSION), "{p}");
    }
    // The waterfall has bars for the seven tool calls and the session's turns.
    let wf = s.get(&format!("/session/{claude}")).await.body;
    assert!(
        wf.matches("class=\"bar call-").count() >= 5,
        "waterfall bars"
    );
    assert!(wf.contains("call-failure"), "failed call bar");
    // Time travel: at 08:00:30 the Claude session is open with an in-flight Bash call.
    let st = s.get("/state?at=2026-08-28T08:00:30Z").await.body;
    assert!(st.contains("in_flight_tool_calls"), "{st}");
    // Blocked explanation on an unblocked project is an honest empty answer.
    let why = s.get("/why").await.body;
    assert!(why.contains("no blocked session found"), "{why}");

    for p in [
        "/api/status".to_string(),
        "/api/projects".to_string(),
        "/api/sessions".to_string(),
        format!("/api/session/{claude}"),
        format!("/api/attempt/{att}"),
        "/api/failures".to_string(),
        "/api/handoffs".to_string(),
        "/api/work_units".to_string(),
        "/api/overview".to_string(),
        "/api/attention".to_string(),
        "/api/work".to_string(),
        "/api/decisions".to_string(),
        "/api/why".to_string(),
        format!("/api/why?subject={att}"),
        format!("/api/trace/{att}?depth=3&direction=both"),
        "/api/state?at=2026-08-28T08:00:30Z".to_string(),
        format!("/api/evidence/{ev}"),
    ] {
        let r = s.get(&p).await;
        assert_eq!(r.status, 200, "{p}: {}", r.body);
        assert!(
            r.header("Content-Type")
                .unwrap()
                .starts_with("application/json"),
            "{p}"
        );
        let _ = r.json();
    }
    let status = s.get("/api/status").await.json();
    assert_eq!(status["daemon"]["state"], "not_running");
    assert_eq!(status["capture_mode"], "local_semantic");
    assert_eq!(status["captured_events"], f.scenario.events.len() as u64);
    assert_eq!(status["reconstructed_events"], 0);
    assert_eq!(status["read_only"], false);
    let state = s.get("/api/state?at=2026-08-28T08:00:30Z").await.json();
    assert_eq!(state["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(
        state["sessions"][0]["in_flight_tool_calls"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert!(
        state["statement"]
            .as_str()
            .unwrap()
            .starts_with("STATE project AT '2026-08-28T08:00:30Z'")
    );
    let units = s.get("/api/work_units").await.json();
    assert!(units["total"].as_u64().unwrap() >= 1, "{units}");
    assert!(
        units["work_units"][0]["work_unit_id"]
            .as_str()
            .unwrap()
            .starts_with("wu_")
    );
    assert!(s.get("/timeline").await.body.contains("Work units"));
    let failures = s.get("/api/failures").await.json();
    assert_eq!(failures["total"], 1);
    assert_eq!(failures["attempts"][0]["failure_class"], "string_mismatch");
    let evidence = s.get(&format!("/api/evidence/{ev}")).await.json();
    assert_eq!(evidence["event"]["kind"], "tool_call_failed");
    assert_eq!(evidence["event"]["outcome_class"], "string_mismatch");
    assert_eq!(s.get("/api/evidence/ev_00000000").await.status, 404);
    assert_eq!(s.get("/api/timeline?project=nope").await.status, 400);
    assert_eq!(s.get("/api/state?at=soon").await.status, 400);
    assert_eq!(s.get("/nope").await.status, 404);
    s.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refreshes_when_the_database_changes() {
    let f = fixture();
    let s = start(&f).await;
    assert_eq!(s.get("/api/timeline").await.json()["total_sessions"], 2);
    // Another writer appends a session (left in the WAL, no flush).
    let mut b = Stream::new();
    let later = Sess::claude("claude-session-2");
    b.session_started(&later, at(3000));
    b.prompt(&later, at(3001), "Add a regression test");
    b.tool_start(
        &later,
        at(3002),
        &common::Tool::write(Some("z1"), &["tests/parser.rs"]),
    );
    b.tool_finish(
        &later,
        at(3003),
        &common::Tool::write(Some("z1"), &["tests/parser.rs"]),
        attemptdb_core::Outcome::success(),
    );
    b.stop(&later, at(3004));
    let mut events = b.build();
    for (i, ev) in events.iter_mut().enumerate() {
        ev.event_id = EventId::derive(&["extra", "later", &i.to_string()]);
    }
    {
        let mut db = Database::open(&f.db_dir, OpenOptions::default()).unwrap();
        db.ingest(events).unwrap();
    }
    let j = s.get("/api/timeline").await.json();
    assert_eq!(j["total_sessions"], 3, "{j}");
    assert_eq!(j["sessions"][0]["session_id"], ses(&later));
    let page = s.get("/timeline").await.body;
    assert!(page.contains("Add a regression test"));
    // While another process holds the writer lock the view is read-only.
    let _writer = Database::open(&f.db_dir, OpenOptions::default()).unwrap();
    std::fs::write(f.db_dir.join("wal").join("999999.wal"), b"").unwrap();
    let status = s.get("/api/status").await.json();
    assert_eq!(status["read_only"], true, "{status}");
    assert!(s.get("/").await.body.contains("read-only"));
    s.stop().await;
}

#[tokio::test]
async fn refuses_non_loopback_without_allow_remote() {
    let f = fixture();
    let cfg = UiConfig {
        bind: "0.0.0.0".parse().unwrap(),
        ..config(&f)
    };
    let err = Server::bind(cfg).await.err().expect("must refuse");
    assert!(err.to_string().contains("--allow-remote"), "{err}");
    let server = Server::bind(config(&f)).await.unwrap();
    assert!(server.url().starts_with("http://127.0.0.1:"));
    assert!(server.url().contains(&format!("?token={}", server.token())));
    assert_eq!(server.token().len(), 64);
}

#[tokio::test]
async fn export_is_self_contained_and_sanitizable() {
    let f = fixture();
    let db = Database::open(
        &f.db_dir,
        OpenOptions {
            read_only: true,
            ..Default::default()
        },
    )
    .unwrap();
    let j = |attribution: bool, sanitized: bool| ExportOptions {
        sanitized,
        attribution,
        scope_label: "acme/repo".into(),
        ..ExportOptions::default()
    };
    let html = render_database(&db, &ScanFilter::default(), j(true, false))
        .await
        .unwrap();
    assert!(html.starts_with("<!doctype html>"));
    assert!(html.contains("<style>"));
    assert!(!html.contains("<script"));
    assert!(!html.contains("<link"));
    assert!(!html.contains("token"));
    assert!(html.contains("att_"), "attempt ids");
    assert!(html.contains("string_mismatch"));
    assert!(
        html.contains("Fix the failing parser test"),
        "prompt text in the plain export"
    );
    assert!(html.contains("&lt;script&gt;alert"), "escaped prompt");
    assert!(!html.contains("<script>alert"));
    assert!(html.contains("/home/alice"), "plain export keeps paths");
    assert!(html.contains("Built with AttemptDB"));
    assert!(html.contains("Handoffs"));
    assert!(html.contains("Work units"));
    assert!(html.contains("Codex"));

    let sanitized = render_database(&db, &ScanFilter::default(), j(false, true))
        .await
        .unwrap();
    assert!(sanitized.contains("att_"), "attempt ids survive");
    assert!(sanitized.contains("string_mismatch"));
    assert!(
        sanitized.contains("src/parser.rs"),
        "repository-relative paths survive"
    );
    assert!(!sanitized.contains("/home/"), "home directory leaked");
    assert!(!sanitized.contains("alice"), "user name leaked");
    assert!(
        !sanitized.contains("Fix the failing parser"),
        "prompt leaked"
    );
    assert!(!sanitized.contains("alert("), "prompt leaked");
    assert!(!sanitized.contains("cargo test"), "command leaked");
    assert!(
        !sanitized.contains("claude-session-1"),
        "provider session id leaked"
    );
    assert!(!sanitized.contains("Built with AttemptDB"));
    assert!(sanitized.contains("sanitized"));
    // Attempt ids are identical in both exports (ids derive from session and turn).
    let id = html
        .split("id=\"att_")
        .nth(1)
        .map(|s| s.split('"').next().unwrap().to_string())
        .unwrap();
    assert!(sanitized.contains(&format!("id=\"att_{id}\"")));
}

// ---------------------------------------------------------------------------
// The store's engine cache: a reload after new events decodes nothing it has
// already decoded and projects incrementally.
// ---------------------------------------------------------------------------

fn more_events(device: DeviceId, n: usize, tag: &str) -> Vec<Event> {
    use attemptdb_core::event::Provider;
    use attemptdb_core::{CaptureMode, EventKind, ProjectRef};
    (0..n)
        .map(|i| {
            let mut ev = Event::new(
                device,
                Provider::ClaudeCode,
                "PostToolUse",
                EventKind::ToolCallFinished,
                ProjectRef::derive("/home/dev/example/project", None, &device),
                format!("session-{tag}"),
                CaptureMode::LocalSemantic,
                "ui-test/0.1",
            );
            ev.attrs.insert("x_test_index".into(), serde_json::json!(i));
            ev
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn store_reloads_reuse_decoded_segments_and_project_incrementally() {
    use attemptdb_ui::store::Store;
    let f = fixture();
    let store = Store::new(config(&f));
    let all = ScopeArgs {
        project: None,
        all_projects: true,
        session: None,
        since: None,
        until: None,
        captured_only: false,
        demo: false,
    };

    let v1 = store.view(&all).await.unwrap();
    let n = v1.status.events;
    assert!(n > 0);
    assert_eq!(
        store.cache_stats().await,
        (1, 1, n),
        "one segment decoded on first load"
    );

    // New events land in the WAL: the fingerprint changes, the reload decodes
    // no segment, and the projector grows by exactly those events.
    let device = DeviceId::derive(&["test-device"]);
    {
        let mut db = Database::open(&f.db_dir, OpenOptions::default()).unwrap();
        db.ingest(more_events(device, 3, "wal")).unwrap();
    }
    let v2 = store.view(&all).await.unwrap();
    assert_eq!(v2.status.events, n + 3);
    assert_eq!(store.cache_stats().await, (1, 2, n + 3));
    assert!(
        v2.engine
            .projection()
            .sessions
            .iter()
            .any(|s| s.provider_session_id == "session-wal"),
        "the new session is projected"
    );

    // The WAL is flushed into a second segment: exactly one more decode, and
    // the events that moved from the WAL are not counted twice.
    {
        let mut db = Database::open(&f.db_dir, OpenOptions::default()).unwrap();
        db.flush().unwrap();
    }
    let v3 = store.view(&all).await.unwrap();
    assert_eq!(v3.status.events, n + 3);
    assert_eq!(store.cache_stats().await, (2, 3, n + 3));

    // A scoped view is served from the same cache: no decode.
    // The appended events have no remote, so their project is named after
    // the root's last path component.
    let scoped = ScopeArgs {
        project: Some("project".into()),
        all_projects: false,
        session: None,
        since: None,
        until: None,
        captured_only: false,
        demo: false,
    };
    let v4 = store.view(&scoped).await.unwrap();
    assert_eq!(v4.scope.project_name.as_deref(), Some("project"));
    assert_eq!(
        v4.status.events,
        n + 3,
        "status always describes the whole database"
    );
    assert_eq!(
        store.cache_stats().await.0,
        2,
        "scoped views decode nothing"
    );
    assert_eq!(
        v4.engine.event_count(),
        3,
        "the scoped engine holds only that project's events"
    );
}

// ---------------------------------------------------------------------------
// Overview, Work, Needs You, live updates, demo mode and the summary card
// ---------------------------------------------------------------------------

/// A database whose only story is a permission request nobody answered.
fn waiting_fixture() -> Fixture {
    use attemptdb_core::event::Provider;
    use attemptdb_core::{CaptureMode, EventKind, ProjectRef, ToolCategory, ToolRef};
    let tmp = tempfile::tempdir().unwrap();
    let db_dir = tmp.path().join("db").join(".attemptdb");
    let data_dir = tmp.path().join("data");
    let device = DeviceId::derive(&["test-device"]);
    let project = ProjectRef::derive("/home/dev/example/project", None, &device);
    let mut events = Vec::new();
    let mut push = |kind: EventKind, name: &str, secs: i64, tool: Option<&str>| {
        let mut ev = Event::new(
            device,
            Provider::ClaudeCode,
            name,
            kind,
            project.clone(),
            "waiting-session",
            CaptureMode::LocalSemantic,
            "ui-test/0.1",
        );
        ev.event_id = EventId::derive(&["waiting", name, &secs.to_string()]);
        ev.observed_at = at(secs);
        ev.captured_at = ev.observed_at;
        if let Some(t) = tool {
            ev.tool = Some(ToolRef {
                name: t.to_string(),
                category: ToolCategory::Shell,
                call_id: Some("w1".into()),
            });
        }
        events.push(ev);
    };
    push(EventKind::SessionStarted, "SessionStart", 0, None);
    push(EventKind::PromptSubmitted, "UserPromptSubmit", 5, None);
    push(EventKind::PermissionRequested, "PermissionRequest", 20, Some("Bash"));
    write_db(&db_dir, &events);
    Fixture {
        _tmp: tmp,
        db_dir,
        data_dir,
        scenario: ui_scenario(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn needs_you_holds_the_gate_and_nothing_else() {
    // The reference story completes normally: nothing needs a person.
    let f = fixture();
    let s = start(&f).await;
    let quiet = s.get("/attention").await;
    assert_eq!(quiet.status, 200);
    assert!(quiet.body.contains("Nothing needs you"), "{}", quiet.body);
    let api = s.get("/api/attention").await.json();
    assert_eq!(api["total"], 0);
    // ...and the Overview shows no strip at all.
    let overview = s.get("/").await.body;
    assert!(!overview.contains("id=\"needs-you\""), "no empty strip");
    assert!(overview.contains("id=\"live-execution\""));
    assert!(overview.contains("Attempt path"));
    s.stop().await;

    // An unanswered permission request is the one thing that does.
    let f = waiting_fixture();
    let s = start(&f).await;
    let page = s.get("/attention").await.body;
    assert!(page.contains("Approve or deny the permission request"), "{page}");
    assert!(page.contains("permission_gate"));
    assert!(page.contains("why AttemptDB believes this"));
    assert!(page.contains("Copy continuation brief"));
    let api = s.get("/api/attention").await.json();
    assert_eq!(api["total"], 1);
    assert_eq!(api["items"][0]["kind"], "permission_gate");
    assert_eq!(api["items"][0]["rank"], 1);
    assert!(api["items"][0]["evidence"].as_array().unwrap().len() >= 1);
    assert_eq!(api["items"][0]["algorithm_version"], attemptdb_project::ALGORITHM_VERSION);
    // The queue is visible from every page, as a count in the navigation.
    assert!(s.get("/timeline").await.body.contains("nav-count"));
    // ...and on the Overview, as the strip.
    assert!(s.get("/").await.body.contains("id=\"needs-you\""));
    s.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_work_board_has_three_columns_and_an_inspector() {
    let f = fixture();
    let s = start(&f).await;
    let board = s.get("/work").await;
    assert_eq!(board.status, 200);
    for col in ["col-active", "col-blocked", "col-recently-finished"] {
        assert!(board.body.contains(col), "{col} missing");
    }
    assert!(board.body.contains("work-card"), "{}", board.body);
    let api = s.get("/api/work").await.json();
    let units = s.get("/api/work_units").await.json();
    let total = units["work_units"].as_array().unwrap().len();
    let counted = ["active", "blocked", "finished"]
        .iter()
        .map(|k| api[k].as_array().unwrap().len())
        .sum::<usize>();
    assert_eq!(counted, total, "every unit lands in exactly one column");
    // The inspector opens by full id and by short prefix.
    let id = units["work_units"][0]["work_unit_id"].as_str().unwrap().to_string();
    for spec in [id.clone(), id[..12].to_string()] {
        let r = s.get(&format!("/work/{spec}")).await;
        assert_eq!(r.status, 200, "{spec}: {}", r.body);
        assert!(r.body.contains("Attempt path"), "{spec}");
        assert!(r.body.contains("Attempts"), "{spec}");
    }
    assert_eq!(s.get("/work/wu_ffffffff").await.status, 404);
    s.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_empty_database_shows_the_first_run_steps_and_offers_the_demo() {
    let tmp = tempfile::tempdir().unwrap();
    let db_dir = tmp.path().join("db").join(".attemptdb");
    write_db(&db_dir, &[]);
    let f = Fixture {
        _tmp: tmp,
        db_dir,
        data_dir: PathBuf::new(),
        scenario: ui_scenario(),
    };
    let f = Fixture {
        data_dir: f._tmp.path().join("data"),
        ..f
    };
    let s = start(&f).await;
    let page = s.get("/").await;
    assert_eq!(page.status, 200, "{}", page.body);
    assert!(page.body.contains("Nothing has been captured yet"), "{}", page.body);
    assert!(page.body.contains("Database created"));
    assert!(page.body.contains("Waiting for the first real event"));
    assert!(page.body.contains("demo=1"), "the demo is one click away");
    s.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn demo_mode_is_a_separate_database_and_says_so_on_every_page() {
    let f = fixture();
    let s = start(&f).await;
    let own = s.get("/").await.body;
    assert!(!own.contains("demo-banner"), "no banner without ?demo=1");

    let demo = s.get("/?demo=1").await;
    assert_eq!(demo.status, 200, "{}", demo.body);
    assert!(demo.body.contains("demo-banner"), "the banner");
    assert!(demo.body.contains("Demo data"));
    assert!(demo.body.contains("bundled demo"), "the source is named");
    // The demo story, not the fixture's.
    assert!(demo.body.contains("example/attemptdb"), "{}", demo.body);
    assert!(!demo.body.contains("Fix the failing parser test"));
    // Its events are reconstructed, never presented as captured facts.
    let status = s.get("/api/status?demo=1").await.json();
    assert_eq!(status["captured_events"], 0);
    assert!(status["reconstructed_events"].as_u64().unwrap() > 20);
    // Needs You has exactly the one gate the story ends on.
    let queue = s.get("/api/attention?demo=1").await.json();
    assert_eq!(queue["total"], 1);
    assert_eq!(queue["items"][0]["kind"], "permission_gate");
    // Every link keeps the flag, so a click cannot silently leave the demo.
    assert!(demo.body.contains("/work?demo=1"));
    assert!(demo.body.contains("name=\"demo\""), "the scope form keeps it");
    // The user's own database is untouched by all of this.
    let mine = s.get("/api/status").await.json();
    assert_eq!(mine["captured_events"], f.scenario.events.len() as u64);
    s.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_summary_card_is_an_svg_that_leaks_nothing() {
    let f = fixture();
    let s = start(&f).await;
    let r = s.get("/card.svg").await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert!(
        r.header("Content-Type").unwrap().starts_with("image/svg+xml"),
        "{:?}",
        r.header("Content-Type")
    );
    assert!(r.body.starts_with("<svg"));
    assert!(r.body.contains("What the agents tried"));
    assert!(r.body.contains("Built with AttemptDB"));
    // Privacy canaries: the reference story carries a prompt with markup, a
    // shell command and a path outside the repository.
    for leak in [
        "Fix the failing parser test",
        "<script>",
        "cargo test --workspace",
        "/home/alice",
        "notes.txt",
        "acme/repo.git",
    ] {
        assert!(!r.body.contains(leak), "the card leaked {leak:?}");
    }
    assert!(
        !s.get("/card.svg?no_attribution=1")
            .await
            .body
            .contains("Built with AttemptDB")
    );
    s.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_live_stream_announces_a_revision_when_the_database_changes() {
    let f = fixture();
    let s = start(&f).await;
    // The first frame arrives without any change: it seeds the client.
    let first = s.sse("/api/live", Duration::from_secs(5)).await;
    assert!(
        first.contains("event: change") && first.contains("\"initial\":true"),
        "{first}"
    );
    let revision: String = serde_json::from_str::<Value>(
        first
            .lines()
            .find_map(|l| l.strip_prefix("data: "))
            .expect("a data line"),
    )
    .unwrap()["revision"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(revision.len(), 16, "an opaque revision, not a path");

    // Writing events changes it; nothing about the database leaks into it.
    {
        let device = DeviceId::derive(&["test-device"]);
        let mut db = Database::open(&f.db_dir, OpenOptions::default()).unwrap();
        db.ingest(more_events(device, 2, "live")).unwrap();
    }
    let second = s.sse("/api/live", Duration::from_secs(5)).await;
    let next = serde_json::from_str::<Value>(
        second
            .lines()
            .find_map(|l| l.strip_prefix("data: "))
            .expect("a data line"),
    )
    .unwrap()["revision"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ne!(revision, next, "the revision moved with the database");
    s.stop().await;
}
