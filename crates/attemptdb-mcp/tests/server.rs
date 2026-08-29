//! End-to-end tests: a temporary on-disk database with the synthetic
//! reference story, driven through `Server::handle` exactly as a client
//! would over stdio.

mod common;

use attemptdb_core::{DeviceId, Event, EventId, Outcome};
use attemptdb_mcp::{
    PROTOCOL_VERSION, RESOURCE_BRIEF, RESOURCE_STATUS, Server, ServerConfig, TOOL_NAMES,
};
use attemptdb_storage::{Database, OpenOptions};
use common::{Scenario, Sess, Stream, Tool, at, spec_scenario, spec_scenario_with};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

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

/// The shared stream builder derives event ids from its own counter, so a
/// second builder would collide with the scenario's ids; re-derive them.
fn fresh_ids(tag: &str, mut events: Vec<Event>) -> Vec<Event> {
    for (i, ev) in events.iter_mut().enumerate() {
        ev.event_id = EventId::derive(&["extra", tag, &i.to_string()]);
    }
    events
}

fn ingest_more(db_dir: &Path, events: &[Event]) {
    let mut db = Database::open(db_dir, OpenOptions::default()).unwrap();
    db.ingest(events.to_vec()).unwrap();
    // Left in the WAL on purpose: the server must see unflushed events too.
}

fn fixture_with(scenario: Scenario) -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let db_dir = tmp.path().join("db").join(".attemptdb");
    let data_dir = tmp.path().join("data");
    write_db(&db_dir, &scenario.events);
    Fixture {
        _tmp: tmp,
        db_dir,
        data_dir,
        scenario,
    }
}

fn fixture() -> Fixture {
    fixture_with(spec_scenario())
}

fn server(f: &Fixture) -> Server {
    Server::new(ServerConfig {
        db_dir: f.db_dir.clone(),
        data_dir: Some(f.data_dir.clone()),
        snapshot: None,
        project_root: None,
        max_rows: 200,
    })
    .unwrap()
}

fn initialized(f: &Fixture) -> Server {
    let mut s = server(f);
    let r = s
        .handle(json!({
            "jsonrpc": "2.0", "id": 0, "method": "initialize",
            "params": {"protocolVersion": PROTOCOL_VERSION, "capabilities": {}, "clientInfo": {"name": "test", "version": "0"}}
        }))
        .unwrap();
    assert_eq!(r["result"]["protocolVersion"], PROTOCOL_VERSION);
    assert!(
        s.handle(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
            .is_none()
    );
    s
}

fn call(s: &mut Server, name: &str, args: Value) -> Value {
    let r = s
        .handle(json!({"jsonrpc": "2.0", "id": 7, "method": "tools/call", "params": {"name": name, "arguments": args}}))
        .expect("a request gets a response");
    assert!(r.get("error").is_none(), "unexpected JSON-RPC error: {r}");
    r["result"].clone()
}

fn text(result: &Value) -> String {
    result["content"][0]["text"].as_str().unwrap().to_string()
}

fn ok_text(s: &mut Server, name: &str, args: Value) -> String {
    let r = call(s, name, args);
    assert!(
        r.get("isError").is_none() || r["isError"] == false,
        "{name} failed: {}",
        text(&r)
    );
    text(&r)
}

fn err_text(s: &mut Server, name: &str, args: Value) -> String {
    let r = call(s, name, args);
    assert_eq!(
        r["isError"],
        true,
        "{name} should have failed: {}",
        text(&r)
    );
    text(&r)
}

fn attempt_id(f: &Fixture, sess: &Sess, turn: u32, index: u32) -> String {
    let p = attemptdb_project::project(&f.scenario.events);
    let a = p
        .attempts
        .iter()
        .find(|a| a.session_id == sess.session_id && a.turn_index == turn && a.index == index)
        .expect("attempt exists");
    format!("att_{}", a.attempt_id)
}

fn ses(sess: &Sess) -> String {
    format!("ses_{}", sess.session_id)
}

// ---------------------------------------------------------------------------

#[test]
fn initialize_and_tools_list() {
    let f = fixture();
    let mut s = initialized(&f);
    let r = s
        .handle(json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}))
        .unwrap();
    let tools = r["result"]["tools"].as_array().unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(names, TOOL_NAMES);
    for t in tools {
        let schema = &t["inputSchema"];
        assert_eq!(schema["type"], "object", "{}", t["name"]);
        assert!(schema["properties"].is_object(), "{}", t["name"]);
        if let Some(req) = schema.get("required") {
            for r in req.as_array().unwrap() {
                assert!(
                    schema["properties"].get(r.as_str().unwrap()).is_some(),
                    "{} requires unknown {r}",
                    t["name"]
                );
            }
        }
        assert!(t["description"].as_str().unwrap().len() > 40);
    }
    let r = s
        .handle(json!({"jsonrpc": "2.0", "id": 2, "method": "ping"}))
        .unwrap();
    assert_eq!(r["result"], json!({}));
}

#[test]
fn malformed_requests_and_unknown_tools() {
    let f = fixture();
    let mut s = initialized(&f);
    let r = s.handle(json!({"jsonrpc": "2.0", "id": 1})).unwrap();
    assert_eq!(r["error"]["code"], -32600);
    let r = s.handle(json!({"id": 1, "method": "ping"})).unwrap();
    assert_eq!(r["error"]["code"], -32600);
    let r = s.handle(json!("nope")).unwrap();
    assert_eq!(r["error"]["code"], -32600);
    assert_eq!(r["id"], Value::Null);
    let r = s
        .handle(json!({"jsonrpc": "2.0", "id": 2, "method": "no/such"}))
        .unwrap();
    assert_eq!(r["error"]["code"], -32601);
    let r = s
        .handle(json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {}}))
        .unwrap();
    assert_eq!(r["error"]["code"], -32602);
    let r = s
        .handle(json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": {"name": "attempt_status", "arguments": 5}}))
        .unwrap();
    assert_eq!(r["error"]["code"], -32602);
    // Unknown tool: a tool-level error, not a protocol error.
    let r = call(&mut s, "attempt_nothing", json!({}));
    assert_eq!(r["isError"], true);
    assert!(text(&r).contains("attempt_handoff_brief"));
    // Notifications never get a response, even unknown ones.
    assert!(
        s.handle(json!({"jsonrpc": "2.0", "method": "notifications/whatever"}))
            .is_none()
    );
    // Batches.
    let r = s
        .handle(json!([
            {"jsonrpc": "2.0", "id": 10, "method": "ping"},
            {"jsonrpc": "2.0", "method": "notifications/initialized"}
        ]))
        .unwrap();
    assert_eq!(r.as_array().unwrap().len(), 1);
    assert_eq!(r[0]["id"], 10);
    let r = s.handle(json!([])).unwrap();
    assert_eq!(r["error"]["code"], -32600);
}

#[test]
fn status_reports_counts_and_providers() {
    let f = fixture();
    let mut s = initialized(&f);
    let r = call(&mut s, "attempt_status", json!({}));
    let t = text(&r);
    assert!(
        t.contains(&format!("events        {}", f.scenario.events.len())),
        "{t}"
    );
    assert!(t.contains("claude_code"), "{t}");
    assert!(t.contains("codex"), "{t}");
    assert!(t.contains("daemon        not running"), "{t}");
    assert!(t.contains("local_semantic"), "{t}");
    let mirror: Value = serde_json::from_str(r["content"][1]["text"].as_str().unwrap()).unwrap();
    assert_eq!(mirror["events"], f.scenario.events.len());
    assert_eq!(mirror["sessions"], 2);
    assert_eq!(mirror["read_only"], false);
    assert_eq!(mirror["providers"].as_array().unwrap().len(), 2);
}

#[test]
fn timeline_lists_failed_attempt_with_json_mirror() {
    let f = fixture();
    let mut s = initialized(&f);
    let failed = attempt_id(&f, &f.scenario.claude, 1, 0);
    let r = call(&mut s, "attempt_timeline", json!({"tools": true}));
    let t = text(&r);
    assert!(t.contains(&failed), "{t}");
    assert!(t.contains("string_mismatch"), "{t}");
    assert!(t.contains("superseded"), "{t}");
    assert!(t.contains("Fix the failing parser test"), "{t}");
    assert!(t.contains(&ses(&f.scenario.codex)), "{t}");
    assert!(t.contains("⇄ handoff Claude Code"), "{t}");
    let mirror: Value = serde_json::from_str(r["content"][1]["text"].as_str().unwrap()).unwrap();
    let sessions = mirror["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 2);
    // Newest first: Codex started later.
    assert_eq!(sessions[0]["session_id"], ses(&f.scenario.codex));
    let claude = &sessions[1];
    let turn1 = &claude["turns"][0];
    assert_eq!(turn1["index"], 1);
    assert_eq!(turn1["attempts"][0]["attempt_id"], failed);
    assert_eq!(turn1["attempts"][0]["failure_class"], "string_mismatch");
    assert_eq!(turn1["attempts"][0]["paths"][0], "src/parser.rs");
    assert!(turn1["attempts"][0]["evidence"].as_array().unwrap().len() >= 3);
    assert!(turn1["attempts"][0]["tool_calls"].is_array());
    assert_eq!(mirror["handoffs"].as_array().unwrap().len(), 1);

    // Session scoping by provider session id and by short ses_ prefix.
    let t = ok_text(
        &mut s,
        "attempt_timeline",
        json!({"session": "codex-thread-1"}),
    );
    assert!(
        t.contains(&ses(&f.scenario.codex)) && !t.contains(&ses(&f.scenario.claude)),
        "{t}"
    );
    let short = format!(
        "ses_{}",
        &f.scenario.claude.session_id.0.simple().to_string()[..8]
    );
    let t = ok_text(&mut s, "attempt_timeline", json!({"session": short}));
    assert!(t.contains(&ses(&f.scenario.claude)), "{t}");
    let e = err_text(
        &mut s,
        "attempt_timeline",
        json!({"session": "nope-session"}),
    );
    assert!(e.contains("unknown session"), "{e}");
    let e = err_text(&mut s, "attempt_timeline", json!({"since": "soon"}));
    assert!(e.contains("cannot parse since"), "{e}");
    let e = err_text(&mut s, "attempt_timeline", json!({"limit": "many"}));
    assert!(e.contains("invalid arguments"), "{e}");
}

#[test]
fn failures_why_trace_state_and_evidence_cite_ids() {
    let f = fixture();
    let mut s = initialized(&f);
    let failed = attempt_id(&f, &f.scenario.claude, 1, 0);
    let retry = attempt_id(&f, &f.scenario.claude, 1, 1);
    let fail_end = format!("ev_{}", f.scenario.edit_fail_end);

    let t = ok_text(&mut s, "attempt_failures", json!({}));
    assert!(t.contains(&failed), "{t}");
    assert!(t.contains("string_mismatch"), "{t}");
    assert!(t.contains(&format!("retried by {retry}")), "{t}");
    assert!(t.contains(&fail_end), "{t}");

    let t = ok_text(&mut s, "attempt_why", json!({"subject": failed.clone()}));
    assert!(t.contains(&format!("WHY {failed} FAILED")), "{t}");
    assert!(t.contains("string_mismatch"), "{t}");
    assert!(t.contains("claim:"), "{t}");
    assert!(t.contains("confidence:"), "{t}");
    assert!(t.contains("uncertainty:"), "{t}");
    assert!(t.contains("evidence:"), "{t}");
    let t = ok_text(&mut s, "attempt_why", json!({}));
    assert!(t.contains("WHY project STATUS BLOCKED"), "{t}");
    assert!(t.contains("no blocked session found"), "{t}");
    let t = ok_text(
        &mut s,
        "attempt_why",
        json!({"subject": ses(&f.scenario.claude)}),
    );
    assert!(t.contains("WHY session"), "{t}");
    let e = err_text(&mut s, "attempt_why", json!({"subject": "att_00000000"}));
    assert!(e.contains("no attempt matches") || e.contains("not"), "{e}");

    let t = ok_text(&mut s, "attempt_trace", json!({"id": retry.clone()}));
    assert!(t.contains(&format!("TRACE {retry} CAUSES")), "{t}");
    assert!(t.contains("superseded") || t.contains("caused"), "{t}");
    assert!(t.contains(&failed), "{t}");
    let t = ok_text(
        &mut s,
        "attempt_trace",
        json!({"id": failed.clone(), "depth": 2, "direction": "both"}),
    );
    assert!(t.contains("DEPTH 2 DIRECTION BOTH"), "{t}");
    let e = err_text(&mut s, "attempt_trace", json!({"id": "att_x'; DROP"}));
    assert!(e.contains("not an id"), "{e}");
    let e = err_text(&mut s, "attempt_trace", json!({}));
    assert!(e.contains("\"id\" is required"), "{e}");

    let t = ok_text(
        &mut s,
        "attempt_state_at",
        json!({"at": "2026-08-28T08:00:15Z"}),
    );
    assert!(t.contains("STATE project AT '2026-08-28T08:00:15"), "{t}");
    assert!(t.contains(&ses(&f.scenario.claude)), "{t}");
    assert!(t.contains("last_attempt_outcome: failed"), "{t}");
    let t = ok_text(
        &mut s,
        "attempt_state_at",
        json!({"at": "2026-08-28T08:04:45Z", "subject": ses(&f.scenario.codex)}),
    );
    assert!(
        t.contains(&ses(&f.scenario.codex)) && !t.contains(&ses(&f.scenario.claude)),
        "{t}"
    );
    let e = err_text(&mut s, "attempt_state_at", json!({"at": "whenever"}));
    assert!(e.contains("cannot parse at"), "{e}");

    let t = ok_text(&mut s, "attempt_evidence", json!({"id": failed.clone()}));
    assert!(
        t.contains(&format!("ev_{}", f.scenario.edit_fail_start)),
        "{t}"
    );
    assert!(t.contains(&fail_end), "{t}");
    assert!(t.contains("tool_call_failed"), "{t}");
    let t = ok_text(
        &mut s,
        "attempt_evidence",
        json!({"id": format!("ev_{}", f.scenario.bash_end)}),
    );
    assert!(t.contains("(1 row)"), "{t}");
}

#[test]
fn query_runs_sql_and_attemptql_and_rejects_writes() {
    let f = fixture();
    let mut s = initialized(&f);
    let t = ok_text(
        &mut s,
        "attempt_query",
        json!({"statement": "SELECT count(*) AS n FROM events"}),
    );
    assert!(t.contains(&f.scenario.events.len().to_string()), "{t}");
    let t = ok_text(
        &mut s,
        "attempt_query",
        json!({"statement": "SHOW FAILED ATTEMPTS", "format": "json"}),
    );
    let doc: Value = serde_json::from_str(&t).unwrap();
    assert_eq!(doc["row_count"], 1);
    assert_eq!(doc["rows"][0]["failure_class"], "string_mismatch");
    assert!(
        doc["rows"][0]["attempt_id"]
            .as_str()
            .unwrap()
            .starts_with("att_")
    );
    let t = ok_text(
        &mut s,
        "attempt_query",
        json!({"statement": "SELECT kind, count(*) AS n FROM events GROUP BY 1 ORDER BY 1", "format": "csv"}),
    );
    assert!(t.starts_with("kind,n\n"), "{t}");
    // Row cap.
    let t = ok_text(
        &mut s,
        "attempt_query",
        json!({"statement": "SELECT event_id FROM events", "limit": 3}),
    );
    assert!(t.contains("first 3 shown"), "{t}");
    // Write statements never reach the engine.
    for bad in [
        "INSERT INTO events VALUES (1)",
        "CREATE TABLE x AS SELECT 1",
        "COPY (SELECT 1) TO '/tmp/x'",
        "SELECT 1; DROP TABLE events",
    ] {
        let e = err_text(&mut s, "attempt_query", json!({"statement": bad}));
        assert!(
            e.contains("read-only") || e.contains("one statement"),
            "{bad}: {e}"
        );
    }
    let e = err_text(&mut s, "attempt_query", json!({"statement": "SHOW FOO"}));
    assert!(e.contains("error:") && e.contains('^'), "{e}");
    let e = err_text(
        &mut s,
        "attempt_query",
        json!({"statement": "SELECT 1", "format": "xml"}),
    );
    assert!(e.contains("format"), "{e}");
}

#[test]
fn handoff_brief_is_a_continuation_brief() {
    let f = fixture();
    let mut s = initialized(&f);
    let failed = attempt_id(&f, &f.scenario.claude, 1, 0);
    let t = ok_text(&mut s, "attempt_handoff_brief", json!({}));
    // Latest session is the Codex one; the Claude one is previous.
    assert!(t.contains("## Latest session"), "{t}");
    let latest_pos = t.find(&ses(&f.scenario.codex)).unwrap();
    let previous_pos = t.find(&ses(&f.scenario.claude)).unwrap();
    assert!(latest_pos < previous_pos, "{t}");
    assert!(t.contains("handoff: Claude Code"), "{t}");
    assert!(t.contains("Continue the parser fix"), "{t}");
    assert!(t.contains("## What failed and how"), "{t}");
    assert!(t.contains(&failed), "{t}");
    assert!(t.contains("string_mismatch"), "{t}");
    assert!(t.contains("## Files touched"), "{t}");
    assert!(t.contains("src/parser.rs"), "{t}");
    assert!(t.contains("README.md"), "{t}");
    assert!(t.contains("## Open / pending"), "{t}");
    assert!(t.contains("in-flight tool calls: none"), "{t}");
    assert!(t.contains("## Uncertainty"), "{t}");
    assert!(t.contains("coverage:"), "{t}");
    assert!(t.contains("hook-captured"), "{t}");
    assert!(t.contains("reconstructed"), "{t}");
    assert!(t.contains("ev_"), "{t}");

    // Focus on the Claude session: its own failures and files, no handoff.
    let t = ok_text(
        &mut s,
        "attempt_handoff_brief",
        json!({"session": ses(&f.scenario.claude), "turns": 1}),
    );
    assert!(t.contains("Now document the parser module"), "{t}");
    assert!(!t.contains("Continue the parser fix"), "{t}");
    assert!(t.contains("1 earlier turn(s) not shown"), "{t}");
}

#[test]
fn brief_is_content_free_in_metadata_only_mode() {
    let f = fixture_with(spec_scenario_with(
        attemptdb_core::CaptureMode::MetadataOnly,
    ));
    let mut s = initialized(&f);
    let t = ok_text(&mut s, "attempt_handoff_brief", json!({}));
    assert!(!t.contains("Fix the failing parser test"), "{t}");
    assert!(!t.contains("Continue the parser fix"), "{t}");
    assert!(t.contains("no prompt text"), "{t}");
    assert!(t.contains("prompt of"), "{t}");
    assert!(t.contains("string_mismatch"), "{t}");
    assert!(t.contains("src/parser.rs"), "{t}");
    let t = ok_text(&mut s, "attempt_timeline", json!({}));
    assert!(t.contains("text not captured"), "{t}");
}

#[test]
fn open_session_with_pending_permission_shows_up_as_blocked() {
    let sc = spec_scenario();
    let mut b = Stream::new();
    let stuck = Sess::claude("claude-stuck");
    b.session_started(&stuck, at(1000));
    b.prompt(&stuck, at(1001), "delete the build directory");
    b.tool_start(&stuck, at(1002), &Tool::read(Some("r1"), &["Makefile"]));
    b.tool_finish(
        &stuck,
        at(1003),
        &Tool::read(Some("r1"), &["Makefile"]),
        Outcome::success(),
    );
    b.tool_start(&stuck, at(1005), &Tool::shell(Some("s1")));
    b.permission_requested(&stuck, at(1010), &Tool::shell(Some("s1")));
    let mut events = sc.events.clone();
    let extra = fresh_ids("stuck", b.build());
    let perm = extra.last().unwrap().event_id;
    events.extend(extra);
    let f = fixture_with(Scenario { events, ..sc });
    let mut s = initialized(&f);
    let t = ok_text(&mut s, "attempt_handoff_brief", json!({}));
    assert!(t.contains(&ses(&stuck)), "{t}");
    assert!(t.contains("still open"), "{t}");
    assert!(
        t.contains("in-flight tool calls (started, no end observed): 1"),
        "{t}"
    );
    assert!(t.contains("pending signal: permission_requested"), "{t}");
    assert!(t.contains(&format!("ev_{perm}")), "{t}");
    assert!(t.contains("blocked: yes"), "{t}");
    let t = ok_text(&mut s, "attempt_why", json!({}));
    assert!(t.contains("permission request"), "{t}");
    assert!(t.contains(&format!("ev_{perm}")), "{t}");
}

#[test]
fn resources_serve_brief_and_status() {
    let f = fixture();
    let mut s = initialized(&f);
    let r = s
        .handle(json!({"jsonrpc": "2.0", "id": 1, "method": "resources/list"}))
        .unwrap();
    let uris: Vec<&str> = r["result"]["resources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["uri"].as_str().unwrap())
        .collect();
    assert_eq!(uris, vec![RESOURCE_BRIEF, RESOURCE_STATUS]);
    let r = s
        .handle(json!({"jsonrpc": "2.0", "id": 2, "method": "resources/read", "params": {"uri": RESOURCE_BRIEF}}))
        .unwrap();
    let body = r["result"]["contents"][0]["text"].as_str().unwrap();
    assert!(body.contains("# AttemptDB handoff brief"), "{body}");
    assert_eq!(r["result"]["contents"][0]["uri"], RESOURCE_BRIEF);
    let r = s
        .handle(json!({"jsonrpc": "2.0", "id": 3, "method": "resources/read", "params": {"uri": RESOURCE_STATUS}}))
        .unwrap();
    assert!(
        r["result"]["contents"][0]["text"]
            .as_str()
            .unwrap()
            .contains("AttemptDB status")
    );
    let r = s
        .handle(json!({"jsonrpc": "2.0", "id": 4, "method": "resources/read", "params": {"uri": "attemptdb://nope"}}))
        .unwrap();
    assert_eq!(r["error"]["code"], -32002);
}

#[test]
fn new_events_are_visible_without_restart_and_no_lock_is_held() {
    let f = fixture();
    let mut s = initialized(&f);
    let before = text(&call(&mut s, "attempt_status", json!({})));
    assert!(
        before.contains(&format!("events        {}", f.scenario.events.len())),
        "{before}"
    );

    // Another writer can take the lock between calls: the server does not
    // hold it. Leave the new events in the WAL (no flush).
    let mut b = Stream::new();
    let later = Sess::codex("codex-thread-2");
    b.session_started(&later, at(2000));
    b.prompt(&later, at(2001), "Add a regression test for the parser");
    b.tool_start(
        &later,
        at(2002),
        &Tool::apply_patch(Some("y1"), &["tests/parser.rs"]),
    );
    b.tool_finish(
        &later,
        at(2003),
        &Tool::apply_patch(Some("y1"), &["tests/parser.rs"]),
        Outcome::success(),
    );
    b.stop(&later, at(2010));
    let extra = fresh_ids("later", b.build());
    ingest_more(&f.db_dir, &extra);

    let after = text(&call(&mut s, "attempt_status", json!({})));
    assert!(
        after.contains(&format!(
            "events        {}",
            f.scenario.events.len() + extra.len()
        )),
        "{after}"
    );
    let t = ok_text(&mut s, "attempt_timeline", json!({}));
    assert!(t.contains(&ses(&later)), "{t}");
    assert!(t.contains("tests/parser.rs"), "{t}");
    let brief = ok_text(&mut s, "attempt_handoff_brief", json!({}));
    assert!(brief.contains("Add a regression test"), "{brief}");

    // While another process holds the writer lock the server falls back to
    // a read-only view and says so.
    let _writer = Database::open(&f.db_dir, OpenOptions::default()).unwrap();
    std::fs::write(f.db_dir.join("wal").join("999999.wal"), b"").unwrap(); // force a refresh
    let ro = text(&call(&mut s, "attempt_status", json!({})));
    assert!(ro.contains("read-only"), "{ro}");
}

#[test]
fn spool_is_imported_on_refresh() {
    let f = fixture();
    let mut s = initialized(&f);
    let _ = call(&mut s, "attempt_status", json!({}));
    let mut b = Stream::new();
    let hooked = Sess::claude("claude-session-9");
    b.session_started(&hooked, at(3000));
    b.prompt(&hooked, at(3001), "Spooled by a hook process");
    let events = fresh_ids("hooked", b.build());
    attemptdb_storage::SpoolWriter::new(&f.db_dir)
        .unwrap()
        .append(&events)
        .unwrap();
    let t = text(&call(&mut s, "attempt_status", json!({})));
    assert!(
        t.contains(&format!(
            "events        {}",
            f.scenario.events.len() + events.len()
        )),
        "{t}"
    );
    assert!(
        t.contains("imported      2 new event(s) from 1 spool file(s)"),
        "{t}"
    );
}
