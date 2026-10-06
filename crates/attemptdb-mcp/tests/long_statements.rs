//! One long statement must not abort the MCP server (the stack overflow in
//! the private statement runtime took the whole process down, and with it
//! the agent's access to its own history).

mod common;

use attemptdb_core::DeviceId;
use attemptdb_mcp::{PROTOCOL_VERSION, Server, ServerConfig};
use attemptdb_storage::{Database, OpenOptions};
use common::{Sess, Stream, Tool, at};
use serde_json::{Value, json};

fn server() -> (tempfile::TempDir, Server) {
    let tmp = tempfile::tempdir().unwrap();
    let db_dir = tmp.path().join("db").join(".attemptdb");
    std::fs::create_dir_all(db_dir.parent().unwrap()).unwrap();
    Database::create(&db_dir, DeviceId::derive(&["test-device"])).unwrap();
    let mut db = Database::open(&db_dir, OpenOptions::default()).unwrap();
    let mut b = Stream::new();
    let s = Sess::claude("long");
    b.session_started(&s, at(0));
    b.prompt(&s, at(1), "do it");
    let tool = Tool::edit(Some("e1"), &["src/lib.rs"]);
    b.tool_start(&s, at(2), &tool);
    b.tool_finish(&s, at(3), &tool, attemptdb_core::Outcome::success());
    b.stop(&s, at(4));
    db.ingest(b.build()).unwrap();
    db.flush().unwrap();
    drop(db);
    let config = ServerConfig {
        data_dir: Some(tmp.path().join("data")),
        ..ServerConfig::new(db_dir)
    };
    let mut srv = Server::new(config).unwrap();
    let r = srv
        .handle(json!({"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":PROTOCOL_VERSION,"capabilities":{},"clientInfo":{"name":"t","version":"0"}}}))
        .unwrap();
    assert!(r.get("error").is_none());
    srv.handle(json!({"jsonrpc":"2.0","method":"notifications/initialized"}));
    (tmp, srv)
}

fn query(s: &mut Server, statement: &str) -> Value {
    s.handle(json!({"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"attempt_query","arguments":{"statement":statement,"all_projects":true}}}))
        .expect("a response")["result"]
        .clone()
}

fn shapes(n: usize) -> Vec<(&'static str, String)> {
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

#[test]
fn every_shape_that_overflowed_the_stack_is_an_error_the_agent_can_read() {
    let (_tmp, mut srv) = server();
    for (name, sql) in shapes(1000) {
        let r = query(&mut srv, &sql);
        assert_eq!(r["isError"], true, "{name}");
        let text = r["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("too complex"), "{name}: {text}");
        assert!(
            text.len() < 2_000,
            "{name}: the refusal must not echo the {} byte statement ({} bytes)",
            sql.len(),
            text.len()
        );
    }
    // The server is still there, and still answers.
    let r = query(&mut srv, "SELECT count(*) AS n FROM events");
    assert!(r["isError"].as_bool() != Some(true), "{r}");
}

#[test]
fn the_deepest_statements_allowed_run() {
    let (_tmp, mut srv) = server();
    // Under 400 chained operators and 100 SELECT blocks.
    let chains = shapes(390).into_iter().take(4);
    let blocks = shapes(95).into_iter().skip(4);
    for (name, sql) in chains.chain(blocks) {
        let r = query(&mut srv, &sql);
        assert!(
            r["isError"].as_bool() != Some(true),
            "{name}: {}",
            r["content"][0]["text"]
        );
    }
}

#[test]
fn a_syntax_error_in_a_long_one_line_statement_does_not_come_back_whole() {
    let (_tmp, mut srv) = server();
    let sql = format!("SHOW {} FOO {}", "ATTEMPTS ".repeat(3000), "x ".repeat(3000));
    let r = query(&mut srv, &sql);
    assert_eq!(r["isError"], true);
    let text = r["content"][0]["text"].as_str().unwrap();
    assert!(text.len() < 2_000, "{} bytes", text.len());
}

#[test]
fn a_mistyped_keyword_gets_a_suggestion_and_a_missing_table_lists_the_tables() {
    let (_tmp, mut srv) = server();
    let r = query(&mut srv, "SELEC 1");
    assert_eq!(r["isError"], true);
    let text = r["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("did you mean SELECT?"), "{text}");
    let r = query(&mut srv, "SELECT count(*) FROM evnts");
    assert_eq!(r["isError"], true);
    let text = r["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("events, events_raw, sessions"), "{text}");
}

#[test]
fn the_tool_descriptions_name_every_table() {
    let (_tmp, mut srv) = server();
    let tools = srv
        .handle(json!({"jsonrpc":"2.0","id":4,"method":"tools/list"}))
        .unwrap();
    let tools = tools["result"]["tools"].as_array().unwrap();
    let q = tools.iter().find(|t| t["name"] == "attempt_query").unwrap();
    let description = q["description"].as_str().unwrap();
    for table in attemptdb_query::TABLE_NAMES {
        assert!(description.contains(table), "attempt_query does not name {table}");
    }
    let s = tools.iter().find(|t| t["name"] == "attempt_schema").unwrap();
    let table_prop = s["inputSchema"]["properties"]["table"]["description"]
        .as_str()
        .unwrap();
    for table in attemptdb_query::TABLE_NAMES {
        assert!(table_prop.contains(table), "attempt_schema.table does not name {table}");
    }
}
