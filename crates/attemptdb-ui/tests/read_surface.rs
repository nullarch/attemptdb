//! The local web UI as a surface a browser (and a hostile page in it)
//! drives: who may talk to it, and what one query may cost or reveal.

mod common;

use attemptdb_core::{DeviceId, Event};
use attemptdb_query::QueryLimits;
use attemptdb_storage::{Database, OpenOptions};
use attemptdb_ui::{Server, UiConfig};
use common::{Sess, Stream, Tool, at};
use serde_json::Value;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::time::{Duration, Instant};

struct Fixture {
    _tmp: tempfile::TempDir,
    db_dir: PathBuf,
}

fn fixture(events: Vec<Event>) -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let db_dir = tmp.path().join("db").join(".attemptdb");
    std::fs::create_dir_all(db_dir.parent().unwrap()).unwrap();
    Database::create(&db_dir, DeviceId::derive(&["test-device"])).unwrap();
    let mut db = Database::open(&db_dir, OpenOptions::default()).unwrap();
    db.ingest(events).unwrap();
    db.flush().unwrap();
    Fixture { _tmp: tmp, db_dir }
}

fn story() -> Vec<Event> {
    let mut b = Stream::new();
    let s = Sess::claude("ui");
    b.session_started(&s, at(0));
    b.prompt(&s, at(1), "Tidy the parser module");
    let tool = Tool::edit(Some("e1"), &["src/lib.rs"]);
    b.tool_start(&s, at(2), &tool);
    b.tool_finish(&s, at(3), &tool, attemptdb_core::Outcome::success());
    b.stop(&s, at(4));
    b.build()
}

struct Running {
    addr: SocketAddr,
    token: String,
    cookie_name: String,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    task: Option<tokio::task::JoinHandle<anyhow::Result<()>>>,
}

async fn start(f: &Fixture) -> Running {
    start_with(f, None).await
}

async fn start_with(f: &Fixture, limits: Option<QueryLimits>) -> Running {
    let mut server = Server::bind(UiConfig::new(&f.db_dir)).await.unwrap();
    if let Some(l) = limits {
        server = server.with_query_limits(l);
    }
    let addr = server.addr();
    let token = server.token().to_string();
    let cookie_name = server.cookie_name().to_string();
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let task = tokio::spawn(server.run(async move {
        let _ = rx.await;
    }));
    Running {
        addr,
        token,
        cookie_name,
        shutdown: Some(tx),
        task: Some(task),
    }
}

impl Running {
    fn cookie(&self) -> String {
        format!("{}={}", self.cookie_name, self.token)
    }

    /// One request with the given Host and extra headers; the session cookie
    /// is sent unless `cookie` says otherwise.
    async fn send(
        &self,
        method: &str,
        path: &str,
        host: Option<String>,
        headers: Vec<(String, String)>,
        body: Option<&str>,
    ) -> (u16, String) {
        let addr = self.addr;
        let host = host.unwrap_or_else(|| format!("127.0.0.1:{}", addr.port()));
        let method = method.to_string();
        let path = path.to_string();
        let body = body.map(str::to_string);
        tokio::task::spawn_blocking(move || {
            let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(5)).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(60)))
                .unwrap();
            let mut req =
                format!("{method} {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n");
            for (k, v) in &headers {
                req.push_str(&format!("{k}: {v}\r\n"));
            }
            if let Some(b) = &body {
                req.push_str(&format!(
                    "Content-Type: application/json\r\nContent-Length: {}\r\n",
                    b.len()
                ));
            }
            req.push_str("\r\n");
            if let Some(b) = &body {
                req.push_str(b);
            }
            stream.write_all(req.as_bytes()).unwrap();
            let mut raw = Vec::new();
            stream.read_to_end(&mut raw).unwrap();
            let text = String::from_utf8_lossy(&raw).into_owned();
            let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
            let status: u16 = head
                .lines()
                .next()
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            // Small, `Connection: close`; chunked bodies are decoded
            // loosely, which is enough to find substrings and parse JSON.
            let body = if head
                .to_ascii_lowercase()
                .contains("transfer-encoding: chunked")
            {
                dechunk(body)
            } else {
                body.to_string()
            };
            (status, body)
        })
        .await
        .unwrap()
    }

    async fn get_host(&self, path: &str, host: &str) -> (u16, String) {
        self.send(
            "GET",
            path,
            Some(host.to_string()),
            vec![("Cookie".into(), self.cookie())],
            None,
        )
        .await
    }

    async fn post_query(
        &self,
        statement: &str,
        format: &str,
        origin: Option<&str>,
    ) -> (u16, String) {
        let mut headers = vec![("Cookie".to_string(), self.cookie())];
        if let Some(o) = origin {
            headers.push(("Origin".into(), o.to_string()));
        }
        let body = serde_json::json!({"statement": statement, "format": format}).to_string();
        self.send("POST", "/api/query", None, headers, Some(&body))
            .await
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

fn dechunk(body: &str) -> String {
    let mut out = String::new();
    let mut rest = body;
    while let Some((size, tail)) = rest.split_once("\r\n") {
        let n = usize::from_str_radix(size.trim(), 16).unwrap_or(0);
        if n == 0 || tail.len() < n {
            break;
        }
        out.push_str(&tail[..n]);
        rest = tail[n..].trim_start_matches("\r\n");
    }
    out
}

fn json(body: &str) -> Value {
    serde_json::from_str(body).unwrap_or_else(|e| panic!("not JSON ({e}): {body}"))
}

// ---------------------------------------------------------------------------
// Who may talk to it (REPORT 7.7)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_host_header_must_name_loopback_exactly() {
    let f = fixture(story());
    let s = start(&f).await;
    let port = s.addr.port();
    for bad in [
        format!("127.evil.example:{port}"),
        "127.evil.example".to_string(),
        format!("127.0.0.1.evil.example:{port}"),
        format!("127.0.0.2:{port}"),
        format!("127.1:{port}"),
        format!("localhost.evil.example:{port}"),
        format!("evil.example:{port}"),
        format!("127.0.0.1:{}", port.wrapping_add(1)),
        format!("localhost:{}", port.wrapping_add(1)),
        String::new(),
    ] {
        let (status, _) = s.get_host("/api/status", &bad).await;
        assert_eq!(status, 403, "Host: {bad:?} must be refused");
    }
    for good in [
        format!("127.0.0.1:{port}"),
        format!("localhost:{port}"),
        format!("LocalHost:{port}"),
        format!("[::1]:{port}"),
        "127.0.0.1".to_string(),
    ] {
        let (status, body) = s.get_host("/api/status", &good).await;
        assert_eq!(status, 200, "Host: {good:?}: {body}");
    }
    s.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn each_instance_has_its_own_cookie_and_the_cookie_is_scoped_tightly() {
    let f = fixture(story());
    let a = start(&f).await;
    let b = start(&f).await;
    assert_ne!(a.cookie_name, b.cookie_name);
    assert!(
        a.cookie_name.ends_with(&a.addr.port().to_string()),
        "{}",
        a.cookie_name
    );
    assert!(
        b.cookie_name.ends_with(&b.addr.port().to_string()),
        "{}",
        b.cookie_name
    );
    // The token redirect sets a cookie with the full set of attributes.
    for s in [&a, &b] {
        let (status, _) = s
            .send("GET", &format!("/?token={}", s.token), None, vec![], None)
            .await;
        assert_eq!(status, 303);
    }
    // Both cookies in one jar (the browser shares cookies across ports of
    // 127.0.0.1) authenticate each instance without evicting each other.
    let jar = format!("{}; {}", a.cookie(), b.cookie());
    for s in [&a, &b] {
        let (status, _) = s
            .send(
                "GET",
                "/api/status",
                None,
                vec![("Cookie".into(), jar.clone())],
                None,
            )
            .await;
        assert_eq!(status, 200);
    }
    // One instance's cookie does not open the other.
    let (status, _) = b
        .send(
            "GET",
            "/api/status",
            None,
            vec![("Cookie".into(), a.cookie())],
            None,
        )
        .await;
    assert_eq!(status, 401);
    // Path, HttpOnly, SameSite on the Set-Cookie itself.
    let addr = a.addr;
    let token = a.token.clone();
    let raw = tokio::task::spawn_blocking(move || {
        let mut st = TcpStream::connect(addr).unwrap();
        st.write_all(
            format!(
                "GET /?token={token} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n",
                addr.port()
            )
            .as_bytes(),
        )
        .unwrap();
        let mut out = String::new();
        st.read_to_string(&mut out).unwrap();
        out
    })
    .await
    .unwrap();
    let set = raw
        .lines()
        .find(|l| l.to_ascii_lowercase().starts_with("set-cookie:"))
        .unwrap();
    assert!(set.contains(&format!("{}=", a.cookie_name)), "{set}");
    for attr in ["Path=/", "HttpOnly", "SameSite=Strict"] {
        assert!(set.contains(attr), "{attr} in {set}");
    }
    a.stop().await;
    b.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_request_from_another_origin_is_refused_even_with_the_cookie() {
    let f = fixture(story());
    let s = start(&f).await;
    let own = format!("http://127.0.0.1:{}", s.addr.port());
    // State-changing: the query console.
    for evil in [
        "http://evil.example",
        "https://127.0.0.1.evil.example",
        "null",
        "http://localhost:1",
    ] {
        let (status, body) = s.post_query("SELECT 1", "json", Some(evil)).await;
        assert_eq!(status, 403, "Origin: {evil}: {body}");
    }
    // Reads of the JSON API too: CORS would stop a page reading the answer,
    // but not the request.
    let (status, _) = s
        .send(
            "GET",
            "/api/status",
            None,
            vec![
                ("Cookie".into(), s.cookie()),
                ("Origin".into(), "http://evil.example".into()),
            ],
            None,
        )
        .await;
    assert_eq!(status, 403);
    // The UI's own origin, and no Origin at all, pass.
    let (status, body) = s.post_query("SELECT 1 AS one", "json", Some(&own)).await;
    assert_eq!(status, 200, "{body}");
    let (status, _) = s.post_query("SELECT 1 AS one", "json", None).await;
    assert_eq!(status, 200);
    // Navigation to a page carries no Origin check (browsers omit it, and a
    // page that merely links here cannot read the response).
    let (status, _) = s
        .send(
            "GET",
            "/timeline",
            None,
            vec![
                ("Cookie".into(), s.cookie()),
                ("Origin".into(), "http://evil.example".into()),
            ],
            None,
        )
        .await;
    assert_eq!(status, 200);
    // Same origin reached through the other loopback name.
    let (status, _) = s
        .send(
            "POST",
            "/api/query",
            Some(format!("localhost:{}", s.addr.port())),
            vec![
                ("Cookie".into(), s.cookie()),
                (
                    "Origin".into(),
                    format!("http://localhost:{}", s.addr.port()),
                ),
            ],
            Some(r#"{"statement":"SELECT 1 AS one"}"#),
        )
        .await;
    assert_eq!(status, 200);
    s.stop().await;
}

// ---------------------------------------------------------------------------
// What one query may cost or reveal (REPORT 7.1, 7.2, 7.11)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_console_cuts_a_huge_result_in_the_plan() {
    let f = fixture(story());
    let s = start(&f).await;
    let started = Instant::now();
    let (status, body) = s
        .post_query("SELECT * FROM generate_series(1, 30000000)", "json", None)
        .await;
    assert_eq!(status, 200, "{body}");
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "{:?}",
        started.elapsed()
    );
    let j = json(&body);
    assert_eq!(j["row_count"], 2000);
    assert_eq!(j["truncated"], true);
    assert!(j["notes"].to_string().contains("cut at 2000 rows"), "{j}");
    s.stop().await;

    let mut small = QueryLimits::new(5, 1 << 20);
    small.timeout = Some(Duration::from_millis(300));
    let s = start_with(&f, Some(small)).await;
    let (status, body) = s
        .post_query("SELECT * FROM generate_series(1, 100)", "table", None)
        .await;
    assert_eq!(status, 200, "{body}");
    let j = json(&body);
    assert_eq!(j["row_count"], 5);
    assert_eq!(j["truncated"], true);
    assert!(j["text"].as_str().unwrap().ends_with("(5 rows)"));
    // The time limit stops a statement that would never end.
    let (status, body) = s
        .post_query(
            "SELECT count(*) FROM generate_series(1, 100000000000)",
            "json",
            None,
        )
        .await;
    assert_eq!(status, 408, "{body}");
    assert!(
        json(&body)["error"]
            .as_str()
            .unwrap()
            .contains("time limit")
    );
    s.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_console_withholds_the_text_of_retracted_rows_and_neutralises_csv() {
    let mut b = Stream::new();
    let s = Sess::claude("secret");
    b.session_started(&s, at(0));
    b.prompt(&s, at(1), "classified launch plan");
    let tool = Tool::edit(Some("e1"), &["src/lib.rs"]);
    b.tool_start(&s, at(2), &tool);
    b.tool_finish(&s, at(3), &tool, attemptdb_core::Outcome::success());
    b.retraction(
        &s,
        at(10),
        "session",
        &format!("ses_{}", s.session_id),
        "privacy",
        None,
    );
    let f = fixture(b.build());
    let srv = start(&f).await;
    let (status, body) = srv
        .post_query(
            "SELECT event_id, content_json, raw_json FROM events WHERE retracted",
            "json",
            None,
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert!(!body.contains("classified launch plan"), "{body}");
    let j = json(&body);
    assert!(j["row_count"].as_u64().unwrap() > 0);
    for row in j["rows"].as_array().unwrap() {
        assert_eq!(row["content_json"], Value::Null);
        assert_eq!(row["raw_json"], Value::Null);
    }
    assert!(
        j["notes"].to_string().contains("NULL for retracted rows"),
        "{j}"
    );
    // A filter cannot probe it either.
    let (_, body) = srv
        .post_query(
            "SELECT count(*) AS n FROM events WHERE retracted AND content_json LIKE '%classified%'",
            "json",
            None,
        )
        .await;
    assert_eq!(json(&body)["rows"][0]["n"], 0);
    // A formula in a cell is neutralised in CSV (and only there); numbers
    // keep their sign.
    let (status, body) = srv
        .post_query(
            "SELECT '=1+1' AS f, '@SUM(A1)' AS g, -5 AS n, '-x' AS h",
            "csv",
            None,
        )
        .await;
    assert_eq!(status, 200, "{body}");
    let text = json(&body)["text"].as_str().unwrap().to_string();
    assert_eq!(text, "f,g,n,h\n'=1+1,'@SUM(A1),-5,'-x\n");
    let (_, body) = srv.post_query("SELECT '=1+1' AS f", "json", None).await;
    assert_eq!(json(&body)["rows"][0]["f"], "=1+1");
    // Valid SQL with a leading comment runs.
    let (status, body) = srv
        .post_query(
            "-- the count\nSELECT count(*) AS n FROM events",
            "json",
            None,
        )
        .await;
    assert_eq!(status, 200, "{body}");
    srv.stop().await;
}

// ---------------------------------------------------------------------------
// One long statement must not take the UI process down
// ---------------------------------------------------------------------------

/// `n` terms of the shapes that overflowed the statement runtime's stack and
/// aborted the whole process.
fn long_shapes(n: usize) -> Vec<(&'static str, String)> {
    let mut cte = String::from("WITH c0 AS (SELECT 1 AS x)");
    for i in 1..n {
        cte.push_str(&format!(", c{i} AS (SELECT x FROM c{})", i - 1));
    }
    cte.push_str(&format!(" SELECT * FROM c{}", n - 1));
    vec![
        ("plus", format!("SELECT 1{} AS x", " + 1".repeat(n))),
        (
            "or",
            format!(
                "SELECT count(*) FROM events WHERE kind = 'x0'{}",
                (1..n)
                    .map(|i| format!(" OR kind = 'x{i}'"))
                    .collect::<String>()
            ),
        ),
        (
            "and-like",
            format!(
                "SELECT count(*) FROM events WHERE kind LIKE 'a%'{}",
                (1..n)
                    .map(|i| format!(" AND kind LIKE '%b{i}%'"))
                    .collect::<String>()
            ),
        ),
        ("concat", format!("SELECT 'a'{} AS x", " || 'b'".repeat(n))),
        (
            "union-all",
            format!("SELECT 1 AS x{}", " UNION ALL SELECT 1".repeat(n)),
        ),
        ("cte-chain", cte),
    ]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_very_long_statement_is_refused_and_the_server_lives() {
    let f = fixture(story());
    let s = start(&f).await;
    for (name, sql) in long_shapes(1000) {
        let (status, body) = s.post_query(&sql, "json", None).await;
        assert_eq!(status, 400, "{name}: {body}");
        let err = json(&body)["error"].as_str().unwrap().to_string();
        assert!(err.contains("too complex"), "{name}: {err}");
        assert!(err.len() < 2_000, "{name}: {} bytes", err.len());
    }
    // Still serving, and a deep-but-allowed statement runs.
    let (status, body) = s
        .post_query("SELECT count(*) AS n FROM events", "json", None)
        .await;
    assert_eq!(status, 200, "{body}");
    let chains = long_shapes(390).into_iter().take(4);
    let blocks = long_shapes(95).into_iter().skip(4);
    for (name, sql) in chains.chain(blocks) {
        let (status, body) = s.post_query(&sql, "json", None).await;
        assert_eq!(status, 200, "{name}: {body}");
    }
    s.stop().await;
}

// ---------------------------------------------------------------------------
// A session nobody has touched is stale, not open: one answer everywhere
// ---------------------------------------------------------------------------

/// `story()` moved so that its last event happened `ago_secs` seconds before
/// the wall clock (the server judges liveness by it). It has no
/// `SessionEnded`.
fn story_ending_ago(ago_secs: i64) -> Vec<Event> {
    let mut events = story();
    let last = events
        .iter()
        .map(|e| e.observed_at.as_micros())
        .max()
        .unwrap();
    let delta = attemptdb_core::Timestamp::now().as_micros() - ago_secs * 1_000_000 - last;
    for ev in &mut events {
        ev.observed_at = attemptdb_core::Timestamp::from_micros(ev.observed_at.as_micros() + delta);
        ev.captured_at = attemptdb_core::Timestamp::from_micros(ev.captured_at.as_micros() + delta);
    }
    events
}

impl Running {
    async fn get(&self, path: &str) -> (u16, String) {
        let host = format!("127.0.0.1:{}", self.addr.port());
        self.get_host(path, &host).await
    }
}

/// The number in "N open session(s) in scope" on `/attention`.
fn attention_open_count(page: &str) -> u64 {
    let at = page.find(" open session(s) in scope").expect(page);
    page[..at]
        .rsplit(|c: char| !c.is_ascii_digit())
        .next()
        .unwrap()
        .parse()
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stale_session_is_stale_on_every_page_and_the_counts_agree() {
    let f = fixture(story_ending_ago(3 * 3_600));
    let s = start(&f).await;
    let (status, body) = s.get("/api/overview").await;
    assert_eq!(status, 200, "{body}");
    let overview = json(&body);
    assert_eq!(overview["open_sessions"], 0, "{overview}");
    assert_eq!(overview["stale_sessions"], 1, "{overview}");
    assert_eq!(overview["active_sessions"].as_array().unwrap().len(), 0);
    // /attention counts the same way.
    let (status, page) = s.get("/attention").await;
    assert_eq!(status, 200);
    assert_eq!(attention_open_count(&page), 0, "{page}");
    // The pages that print a session's end say stale, never open.
    for path in ["/", "/timeline"] {
        let (status, page) = s.get(path).await;
        assert_eq!(status, 200, "{path}");
        assert!(!page.contains("→ open"), "{path}: {page}");
    }
    let (_, page) = s.get("/timeline").await;
    assert!(page.contains("→ stale"), "{page}");
    let (_, page) = s.get("/").await;
    assert!(page.contains("went stale"), "{page}");
    assert!(
        !page.contains("are open but quiet") && !page.contains("still open"),
        "{page}"
    );
    // The API's session objects carry the state.
    let (_, body) = s.get("/api/sessions").await;
    assert_eq!(json(&body)["sessions"][0]["state"], "stale", "{body}");
    // And the static export, judged at the moment it is generated.
    let db = Database::open(
        &f.db_dir,
        OpenOptions {
            read_only: true,
            ..Default::default()
        },
    )
    .unwrap();
    let html = attemptdb_ui::export::render_database(
        &db,
        &attemptdb_storage::ScanFilter::default(),
        attemptdb_ui::export::ExportOptions {
            sanitized: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(html.contains("→ stale"), "the export says stale");
    assert!(
        !html.contains("→ open"),
        "the export never says open for it"
    );
    s.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_with_activity_just_now_is_open_and_the_counts_agree() {
    let f = fixture(story_ending_ago(20));
    let s = start(&f).await;
    let (_, body) = s.get("/api/overview").await;
    let overview = json(&body);
    assert_eq!(overview["open_sessions"], 1, "{overview}");
    assert_eq!(overview["stale_sessions"], 0, "{overview}");
    let (_, page) = s.get("/attention").await;
    assert_eq!(attention_open_count(&page), 1, "{page}");
    let (_, page) = s.get("/timeline").await;
    assert!(
        page.contains("→ open") && !page.contains("→ stale"),
        "{page}"
    );
    s.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_mistyped_keyword_gets_a_suggestion_in_the_console_too() {
    let f = fixture(story());
    let s = start(&f).await;
    let (status, body) = s.post_query("SELEC 1", "json", None).await;
    assert_eq!(status, 400, "{body}");
    let err = json(&body)["error"].as_str().unwrap().to_string();
    assert!(err.contains("did you mean SELECT?"), "{err}");
    s.stop().await;
}

// ---------------------------------------------------------------------------
// The console lists every table and offers examples that run
// ---------------------------------------------------------------------------

fn unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&#x27;", "'")
        .replace("&amp;", "&")
}

fn example_statements(page: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = page;
    while let Some(i) = rest.find("data-statement=\"") {
        rest = &rest[i + "data-statement=\"".len()..];
        let end = rest.find('"').unwrap();
        out.push(unescape(&rest[..end]));
        rest = &rest[end..];
    }
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_console_names_every_table_and_every_example_runs() {
    let f = fixture(story());
    let s = start(&f).await;
    let (status, page) = s.get("/query?all=1").await;
    assert_eq!(status, 200);
    // The placeholder is the catalog's table list, not a hand-kept subset.
    for table in attemptdb_query::TABLE_NAMES {
        assert!(page.contains(table), "the console does not mention {table}");
    }
    let statements = example_statements(&page);
    assert!(statements.len() >= 9, "{statements:?}");
    for statement in &statements {
        let (status, body) = s.post_query(statement, "json", None).await;
        assert_eq!(status, 200, "{statement}: {body}");
    }
    // They name things that exist here.
    assert!(
        statements.iter().any(|st| st.starts_with("TRACE att_")),
        "{statements:?}"
    );
    s.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_data_the_examples_that_need_an_id_are_hints_not_links() {
    let f = fixture(Vec::new());
    let s = start(&f).await;
    let (status, page) = s.get("/query?all=1").await;
    assert_eq!(status, 200);
    let statements = example_statements(&page);
    assert!(!statements.is_empty());
    for statement in &statements {
        assert!(
            !statement.contains('<') && !statement.contains("att_") && !statement.contains("ses_"),
            "a link with a made-up id: {statement}"
        );
    }
    assert!(
        page.contains("&lt;att_id&gt;"),
        "the placeholder is shown as a hint"
    );
    s.stop().await;
}
