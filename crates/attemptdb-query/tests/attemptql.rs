//! End-to-end tests for the query engine: SQL over the registered tables,
//! every AttemptQL statement, rendering, error positions, robustness against
//! garbage input, and a round-trip through a real on-disk `Database`.

mod common;

use attemptdb_core::{EventId, Timestamp};
use attemptdb_project::AttemptOutcome;
use attemptdb_query::{QueryEngine, QueryError, ResultKind, TABLE_NAMES, format_parse_error};
use attemptdb_storage::{Database, OpenOptions, ScanFilter};
use common::{Scenario, Sess, Stream, Tool, at, spec_scenario};
use serde_json::Value;

fn ses(sess: &Sess) -> String {
    format!("ses_{}", sess.session_id)
}

fn ev(id: &EventId) -> String {
    format!("ev_{id}")
}

fn att(engine: &QueryEngine, sess: &Sess, turn: u32, index: u32) -> String {
    let a = engine
        .projection()
        .attempts
        .iter()
        .find(|a| a.session_id == sess.session_id && a.turn_index == turn && a.index == index)
        .expect("attempt exists");
    format!("att_{}", a.attempt_id)
}

async fn engine() -> (QueryEngine, Scenario) {
    let sc = spec_scenario();
    let e = QueryEngine::from_events(sc.events.clone())
        .await
        .expect("engine");
    (e, sc)
}

fn rows(v: &Value) -> &Vec<Value> {
    v.as_array().expect("json array")
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Tables and SQL
// ---------------------------------------------------------------------------

#[tokio::test]
async fn event_count_matches_sql_and_tables_are_registered() {
    let (e, sc) = engine().await;
    assert_eq!(e.event_count(), sc.events.len());
    for table in ["events", "events_raw"] {
        let r = e
            .sql(&format!("SELECT count(*) AS n FROM {table}"))
            .await
            .unwrap();
        assert_eq!(
            r.to_json()[0]["n"],
            Value::from(sc.events.len() as u64),
            "{table}"
        );
    }
    let names: Vec<String> = e.tables().unwrap().into_iter().map(|t| t.name).collect();
    assert_eq!(names, TABLE_NAMES);
    let by_name = |n: &str| {
        e.tables()
            .unwrap()
            .into_iter()
            .find(|t| t.name == n)
            .unwrap()
    };
    assert_eq!(by_name("sessions").rows, 2);
    assert_eq!(by_name("attempts").rows, 4);
    assert_eq!(by_name("handoffs").rows, 1);
    assert_eq!(by_name("tool_calls").rows, 7);
    assert!(by_name("attempts").has_column("superseded_by"));
    assert!(
        by_name("events_raw")
            .columns
            .iter()
            .any(|(c, t)| c == "event_id" && t == "uuid")
    );
    assert!(
        by_name("events")
            .columns
            .iter()
            .any(|(c, t)| c == "event_id" && t == "text")
    );

    // Readable ids in the events view; grouping over a decoded dictionary.
    let r = e
        .sql("SELECT kind, count(*) AS n FROM events GROUP BY kind ORDER BY n DESC, kind")
        .await
        .unwrap();
    assert!(r.row_count() > 3);
    let r = e
        .sql("SELECT event_id, session_id, provider FROM events ORDER BY observed_at LIMIT 1")
        .await
        .unwrap();
    let row = &r.to_json()[0];
    assert!(row["event_id"].as_str().unwrap().starts_with("ev_"));
    assert_eq!(row["session_id"], Value::from(ses(&sc.claude)));
    assert_eq!(row["provider"], "claude_code");

    // Joins between projection tables and events work on readable ids.
    let r = e
        .sql("SELECT a.attempt_id, count(e.event_id) AS n FROM attempts a JOIN events e ON e.session_id = a.session_id GROUP BY a.attempt_id")
        .await
        .unwrap();
    assert_eq!(r.row_count(), 4);
    assert_eq!(r.kind, ResultKind::Rows);
    let r = e
        .sql("SELECT * FROM attempts WHERE outcome = 'failed'")
        .await
        .unwrap();
    assert_eq!(r.kind, ResultKind::Empty);
}

// ---------------------------------------------------------------------------
// SHOW
// ---------------------------------------------------------------------------

#[tokio::test]
async fn show_failed_attempts_returns_superseded_with_superseded_by() {
    let (e, sc) = engine().await;
    let r = e.query("SHOW FAILED ATTEMPTS").await.unwrap();
    assert_eq!(r.kind, ResultKind::Rows);
    let json = r.to_json();
    assert_eq!(rows(&json).len(), 1);
    let row = &json[0];
    assert_eq!(row["outcome"], "superseded");
    assert_eq!(row["failure_class"], "string_mismatch");
    assert_eq!(row["superseded_by"], Value::from(att(&e, &sc.claude, 1, 1)));
    assert_eq!(row["attempt_id"], Value::from(att(&e, &sc.claude, 1, 0)));
    assert_eq!(row["provider"], "claude_code");
    assert_eq!(row["project_name"], "acme/repo");
    assert_eq!(row["confidence"], Value::from(0.9));
    assert_eq!(strings(&row["paths"]), vec!["src/parser.rs"]);
    assert!(strings(&row["evidence"]).contains(&ev(&sc.edit_fail_end)));
    assert!(
        row["started_at"]
            .as_str()
            .unwrap()
            .starts_with("2026-08-28T08:00:05")
    );
    assert!(r.notes.iter().any(|n| n.contains("tier1")));

    let r = e.query("show superseded attempts;").await.unwrap();
    assert_eq!(r.row_count(), 1);
    let r = e.query("SHOW ATTEMPTS").await.unwrap();
    assert_eq!(r.row_count(), 4);
    let first = &r.to_json()[0];
    assert_eq!(first["provider"], "codex", "default order is newest first");
}

#[tokio::test]
async fn show_filters_predicates_and_limits() {
    let (e, sc) = engine().await;
    let n = |sql: &str| {
        let e = &e;
        let sql = sql.to_string();
        async move { e.query(&sql).await.map(|r| r.row_count()) }
    };
    assert_eq!(
        n("SHOW ATTEMPTS FOR project = 'acme/repo'").await.unwrap(),
        4
    );
    assert_eq!(
        n(&format!(
            "SHOW ATTEMPTS FOR project = 'prj_{}'",
            sc.project_id
        ))
        .await
        .unwrap(),
        4
    );
    assert_eq!(n("SHOW ATTEMPTS FOR provider = 'codex'").await.unwrap(), 1);
    assert_eq!(n("SHOW ATTEMPTS FOR agent = 'Claude'").await.unwrap(), 3);
    assert_eq!(
        n(&format!("SHOW ATTEMPTS FOR session = '{}'", ses(&sc.codex)))
            .await
            .unwrap(),
        1
    );
    let short = format!(
        "ses_{}",
        &sc.claude.session_id.to_string().replace('-', "")[..8]
    );
    assert_eq!(
        n(&format!("SHOW TURNS FOR session = {short}"))
            .await
            .unwrap(),
        2
    );
    assert_eq!(
        n("SHOW ATTEMPTS FOR path = 'src/parser.rs'").await.unwrap(),
        3
    );
    assert_eq!(n("SHOW ATTEMPTS FOR path = 'src/*'").await.unwrap(), 3);
    assert_eq!(
        n("SHOW ATTEMPTS FOR outcome = 'succeeded'").await.unwrap(),
        3
    );
    // Claude's retry (turn 1 #1) and Codex's only attempt (turn 1 #0).
    assert_eq!(
        n("SHOW ATTEMPTS WHERE outcome = 'succeeded' AND turn_index = 1 LIMIT 5")
            .await
            .unwrap(),
        2
    );
    assert_eq!(
        n("SHOW ATTEMPTS WHERE outcome = 'succeeded' AND turn_index = 1 AND provider = 'codex'")
            .await
            .unwrap(),
        1
    );
    assert_eq!(n("SHOW ATTEMPTS LIMIT 2").await.unwrap(), 2);
    assert_eq!(
        n("SHOW ATTEMPTS SINCE '2026-08-28T08:00:10Z'")
            .await
            .unwrap(),
        3
    );
    assert_eq!(
        n("SHOW ATTEMPTS FOR since = '2026-08-28T08:00:10Z' AND until '2026-08-28T08:01:00Z'")
            .await
            .unwrap(),
        2
    );
    assert_eq!(n("SHOW ATTEMPTS UNTIL '2026-08-28'").await.unwrap(), 0);
    assert_eq!(
        n("SHOW ATTEMPTS ORDER BY started_at ASC LIMIT 1")
            .await
            .unwrap(),
        1
    );
    assert_eq!(n("SHOW TOOL CALLS FOR tool = 'Bash'").await.unwrap(), 1);
    assert_eq!(
        n("SHOW TOOL CALLS FOR outcome = 'failure'").await.unwrap(),
        1
    );
    assert_eq!(n("SHOW SESSIONS").await.unwrap(), 2);
    assert_eq!(n("SHOW SESSIONS FOR status = 'closed'").await.unwrap(), 2);
    assert_eq!(n("SHOW TURNS").await.unwrap(), 3);
    // Projection edges plus the derived ones: 4 `triggered` (one per
    // attempt) and 1 `caused` (the failed attempt); no pending signals.
    assert_eq!(
        n("SHOW EDGES").await.unwrap(),
        e.projection().edges.len() + 5
    );
    let derived = e.sql("SELECT edge_kind, count(*) AS n FROM edges WHERE edge_source = 'derived' GROUP BY edge_kind ORDER BY edge_kind").await.unwrap();
    assert_eq!(
        derived.to_json(),
        serde_json::json!([{"edge_kind": "caused", "n": 1}, {"edge_kind": "triggered", "n": 4}])
    );
    assert_eq!(n("SHOW SIGNALS").await.unwrap(), 0);

    let r = e
        .query("SHOW ATTEMPTS ORDER BY started_at ASC LIMIT 1")
        .await
        .unwrap();
    assert_eq!(
        r.to_json()[0]["attempt_id"],
        Value::from(att(&e, &sc.claude, 1, 0))
    );

    // Errors are typed.
    assert!(matches!(
        e.query("SHOW ATTEMPTS FOR project = 'nope'").await,
        Err(QueryError::NotFound(_))
    ));
    assert!(matches!(
        e.query("SHOW ATTEMPTS FOR tool = 'Bash'").await,
        Err(QueryError::Plan(_))
    ));
    assert!(matches!(
        e.query("SHOW ATTEMPTS WHERE no_such_column = 1").await,
        Err(QueryError::Plan(_))
    ));
    assert!(matches!(
        e.query("SHOW ATTEMPTS FOR session = 'ses_zz'").await,
        Err(QueryError::Plan(_))
    ));
    assert!(matches!(
        e.query("SHOW ATTEMPTS FOR session = 'ses_0000'").await,
        Err(QueryError::NotFound(_))
    ));
}

#[tokio::test]
async fn show_handoffs_claude_to_codex() {
    let (e, sc) = engine().await;
    let r = e.query("SHOW HANDOFFS").await.unwrap();
    let json = r.to_json();
    assert_eq!(rows(&json).len(), 1);
    let h = &json[0];
    assert_eq!(h["from_provider"], "claude_code");
    assert_eq!(h["to_provider"], "codex");
    assert_eq!(h["from_session"], Value::from(ses(&sc.claude)));
    assert_eq!(h["to_session"], Value::from(ses(&sc.codex)));
    assert_eq!(strings(&h["shared_paths"]), vec!["src/parser.rs"]);
    assert_eq!(h["confidence"], Value::from(0.8));
    assert!(strings(&h["evidence"]).contains(&ev(&sc.codex_start)));

    let r = e
        .query("SHOW HANDOFFS BETWEEN agent = 'codex' AND agent = 'claude-code'")
        .await
        .unwrap();
    assert_eq!(r.row_count(), 1);
    let r = e
        .query("SHOW HANDOFFS BETWEEN agent = 'cursor' AND agent = 'codex'")
        .await
        .unwrap();
    assert_eq!(r.row_count(), 0);
    assert_eq!(r.kind, ResultKind::Empty);
    assert!(matches!(
        e.query("SHOW HANDOFFS BETWEEN agent = 'codex' AND agent = 'codex'")
            .await,
        Err(QueryError::Plan(_))
    ));
}

#[tokio::test]
async fn show_evidence_and_decisions() {
    let (e, sc) = engine().await;
    let a1 = att(&e, &sc.claude, 1, 0);
    let attempt = e
        .projection()
        .attempts
        .iter()
        .find(|a| format!("att_{}", a.attempt_id) == a1)
        .unwrap();
    let r = e.query(&format!("SHOW EVIDENCE FOR {a1}")).await.unwrap();
    assert_eq!(r.row_count(), attempt.evidence.len());
    assert_eq!(
        r.column_names(),
        [
            "observed_at",
            "kind",
            "tool_name",
            "path_relative",
            "outcome_status",
            "outcome_class",
            "event_id",
            "session_id"
        ]
    );
    let ids: Vec<String> = r
        .to_json()
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["event_id"].as_str().unwrap().to_string())
        .collect();
    for id in &attempt.evidence {
        assert!(ids.contains(&ev(id)), "missing {id}");
    }
    let kinds: Vec<String> = r
        .to_json()
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["kind"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(kinds[0], "prompt_submitted", "chronological order");
    assert!(kinds.contains(&"tool_call_failed".to_string()));
    assert!(r.notes[0].contains("evidence event"));

    let r = e
        .query(&format!("SHOW EVIDENCE FOR attempt '{a1}'"))
        .await
        .unwrap();
    assert_eq!(r.row_count(), attempt.evidence.len());
    let r = e
        .query(&format!("SHOW EVIDENCE FOR {}", ses(&sc.codex)))
        .await
        .unwrap();
    assert_eq!(
        r.row_count(),
        sc.events
            .iter()
            .filter(|x| x.session_id == sc.codex.session_id)
            .count()
    );
    let r = e
        .query(&format!("SHOW EVIDENCE FOR {}", ev(&sc.bash_end)))
        .await
        .unwrap();
    assert_eq!(r.row_count(), 1);
    assert_eq!(r.to_json()[0]["tool_name"], "Bash");
    let turn = &e.projection().turns[0];
    let r = e
        .query(&format!("SHOW EVIDENCE FOR trn_{}", turn.turn_id))
        .await
        .unwrap();
    assert!(r.row_count() >= 6);

    // Decisions are real rows now: the superseded pair of turn 1.
    let r = e.query("SHOW DECISIONS").await.unwrap();
    assert_eq!(r.kind, ResultKind::Rows);
    assert_eq!(r.row_count(), 1);
    let d = &r.to_json()[0];
    assert_eq!(d["kind"], "approach_change");
    assert_eq!(d["selected"], Value::from(att(&e, &sc.claude, 1, 1)));
    assert_eq!(strings(&d["alternatives"]), vec![a1.clone()]);
    assert_eq!(d["rationale_source"], "derived");
    assert!(d["rationale"].as_str().unwrap().contains("string_mismatch"));
    assert!(d["decision_id"].as_str().unwrap().starts_with("dec_"));
    assert!(d["work_unit_id"].as_str().unwrap().starts_with("wu_"));
    assert_eq!(
        strings(&d["evidence"]),
        vec![ev(&sc.edit_fail_end), ev(&sc.edit_retry_start)]
    );
    assert_eq!(d["confidence"], Value::from(0.7));
    assert!(r.notes[0].contains("derived"));
    let r = e
        .query(&format!(
            "SHOW DECISIONS FOR session = '{}'",
            ses(&sc.codex)
        ))
        .await
        .unwrap();
    assert_eq!(r.kind, ResultKind::Empty);
}

// ---------------------------------------------------------------------------
// WHY
// ---------------------------------------------------------------------------

#[tokio::test]
async fn why_blocked_healthy_session_is_empty_and_permission_prompt_is_explained() {
    let (e, sc) = engine().await;
    let r = e
        .query(&format!("WHY session '{}' STATUS BLOCKED", ses(&sc.claude)))
        .await
        .unwrap();
    assert_eq!(r.kind, ResultKind::Empty);
    assert_eq!(r.row_count(), 0);
    assert!(
        r.notes[0].starts_with("no blocked session found (evidence: 1 session examined)"),
        "{:?}",
        r.notes
    );
    assert!(r.notes[1].contains("closed"));
    let r = e.query("WHY project STATUS BLOCKED").await.unwrap();
    assert_eq!(r.kind, ResultKind::Empty);
    assert!(r.notes[0].contains("2 sessions examined"));

    // A session whose last event is a permission prompt.
    let mut b = Stream::new();
    let stuck = Sess::claude("claude-stuck");
    b.session_started(&stuck, at(1000));
    b.prompt(&stuck, at(1001), "delete the build directory");
    b.tool_start(&stuck, at(1002), &Tool::read(Some("r1"), &["Makefile"]));
    b.tool_finish(
        &stuck,
        at(1003),
        &Tool::read(Some("r1"), &["Makefile"]),
        attemptdb_core::Outcome::success(),
    );
    let perm = b.permission_requested(&stuck, at(1010), &Tool::shell(Some("s1")));
    let mut events = sc.events.clone();
    events.extend(b.build());
    let e2 = QueryEngine::from_events(events).await.unwrap();

    for text in [
        format!("WHY session '{}' STATUS BLOCKED", ses(&stuck)),
        format!("WHY {} STATUS BLOCKED", ses(&stuck)),
        format!("why {} blocked", ses(&stuck)),
        "WHY project STATUS BLOCKED".to_string(),
    ] {
        let r = e2.query(&text).await.unwrap();
        assert_eq!(r.kind, ResultKind::Explanation, "{text}");
        assert_eq!(r.row_count(), 1, "{text}");
        let row = &r.to_json()[0];
        assert_eq!(row["session_id"], Value::from(ses(&stuck)));
        assert!(
            row["claim"]
                .as_str()
                .unwrap()
                .contains("permission request"),
            "{}",
            row["claim"]
        );
        assert_eq!(strings(&row["evidence"]), vec![ev(&perm)]);
        assert!(row["confidence"].as_f64().unwrap() > 0.5);
        assert!(!row["uncertainty"].as_str().unwrap().is_empty());
        assert!(!r.notes.is_empty());
    }
    let r = e2
        .query(&format!("STATE session '{}' AT now", ses(&stuck)))
        .await
        .unwrap();
    assert_eq!(r.to_json()[0]["blocked"], Value::Bool(true));
    let r = e2
        .query(&format!("TRACE {} CAUSES", ses(&stuck)))
        .await
        .unwrap();
    assert!(
        r.to_json()
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["edge_kind"] == "blocked" && row["from_id"] == ev(&perm))
    );

    assert!(matches!(
        e.query("WHY project STATUS ACTIVE").await,
        Err(QueryError::Plan(_))
    ));
    assert!(matches!(
        e.query("WHY session 'ses_00000000' STATUS BLOCKED").await,
        Err(QueryError::NotFound(_))
    ));
}

#[tokio::test]
async fn why_attempt_failed() {
    let (e, sc) = engine().await;
    let a1 = att(&e, &sc.claude, 1, 0);
    let a2 = att(&e, &sc.claude, 1, 1);
    let r = e.query(&format!("WHY {a1} FAILED")).await.unwrap();
    assert_eq!(r.kind, ResultKind::Explanation);
    let row = &r.to_json()[0];
    assert_eq!(row["failure_class"], "string_mismatch");
    assert_eq!(row["superseded_by"], Value::from(a2.clone()));
    assert!(
        row["claim"]
            .as_str()
            .unwrap()
            .contains(&ev(&sc.edit_fail_end))
    );
    assert!(row["uncertainty"].as_str().unwrap().contains("tier1"));
    assert!(strings(&row["evidence"]).contains(&ev(&sc.edit_fail_end)));
    let r = e
        .query(&format!("WHY attempt '{a2}' FAILED"))
        .await
        .unwrap();
    assert_eq!(r.kind, ResultKind::Empty);
    assert!(r.notes[0].contains("did not fail"));
}

// ---------------------------------------------------------------------------
// TRACE
// ---------------------------------------------------------------------------

#[tokio::test]
async fn trace_reaches_failing_event() {
    let (e, sc) = engine().await;
    let a1 = att(&e, &sc.claude, 1, 0);
    let a2 = att(&e, &sc.claude, 1, 1);
    let r = e.query(&format!("TRACE {a2} CAUSES")).await.unwrap();
    assert_eq!(r.kind, ResultKind::Explanation);
    let json = r.to_json();
    let steps = rows(&json);
    assert!(steps.len() >= 3, "{json}");
    let superseded = steps
        .iter()
        .find(|s| s["edge_kind"] == "superseded")
        .expect("superseded edge");
    assert_eq!(superseded["depth"], Value::from(1));
    assert_eq!(superseded["from_id"], Value::from(a1.clone()));
    assert_eq!(superseded["to_id"], Value::from(a2.clone()));
    assert_eq!(superseded["confidence"], Value::from(0.9));
    let caused = steps
        .iter()
        .find(|s| s["edge_kind"] == "caused" && s["to_id"] == a1.clone())
        .expect("caused edge");
    assert_eq!(caused["from_id"], Value::from(ev(&sc.edit_fail_end)));
    assert_eq!(caused["depth"], Value::from(2));
    assert_eq!(caused["edge_source"], "derived");
    let triggered = steps
        .iter()
        .find(|s| s["edge_kind"] == "triggered")
        .expect("triggered edge");
    assert_eq!(triggered["from_id"], Value::from(ev(&sc.claude_prompt_1)));
    for s in steps {
        assert!(!strings(&s["evidence"]).is_empty());
        assert!(!s["uncertainty"].as_str().unwrap().is_empty());
    }
    assert!(r.notes[0].contains("trace from attempt"));

    // Depth limit is reported; bare short ids work; sessions trace handoffs.
    let r = e
        .query(&format!("TRACE attempt '{a2}' CAUSES DEPTH 1"))
        .await
        .unwrap();
    assert!(r.notes.iter().any(|n| n.contains("depth limit 1 reached")));
    let short = &a2[..12];
    let r = e
        .query(&format!("TRACE {short} CAUSES DIRECTION BOTH"))
        .await
        .unwrap();
    assert!(r.row_count() >= 3);
    let r = e
        .query(&format!("TRACE {} CAUSES", ses(&sc.codex)))
        .await
        .unwrap();
    assert_eq!(r.to_json()[0]["edge_kind"], "handed_off");
    assert_eq!(r.to_json()[0]["from_id"], Value::from(ses(&sc.claude)));
    let r = e
        .query(&format!("TRACE {} CAUSES", ev(&sc.claude_prompt_1)))
        .await
        .unwrap();
    assert_eq!(r.kind, ResultKind::Empty);
    assert!(r.notes[0].contains("no causal edges reach"));
    let r = e
        .query(&format!(
            "TRACE {} CAUSES DIRECTION DOWN",
            ev(&sc.claude_prompt_1)
        ))
        .await
        .unwrap();
    assert!(r.row_count() >= 2);
    assert!(matches!(
        e.query("TRACE project CAUSES").await,
        Err(QueryError::Plan(_))
    ));
}

// ---------------------------------------------------------------------------
// STATE / DIFF / WHAT IS
// ---------------------------------------------------------------------------

#[tokio::test]
async fn state_before_and_after_the_fix() {
    let (e, sc) = engine().await;
    let claude_row = |json: &Value| {
        json.as_array()
            .unwrap()
            .iter()
            .find(|r| r["session_id"] == ses(&sc.claude))
            .cloned()
            .expect("claude row")
    };
    let r = e
        .query("STATE project AT '2026-08-28T08:00:15Z'")
        .await
        .unwrap();
    assert_eq!(r.kind, ResultKind::Explanation);
    let row = claude_row(&r.to_json());
    assert_eq!(row["last_attempt_outcome"], "failed");
    assert_eq!(row["last_failure_class"], "string_mismatch");
    assert_eq!(row["last_attempt"], Value::from(att(&e, &sc.claude, 1, 0)));
    assert_eq!(row["turn_status"], "in_progress");
    assert_eq!(row["is_open"], Value::Bool(true));
    assert_eq!(row["blocked"], Value::Bool(false));
    assert!(!strings(&row["evidence"]).is_empty());
    assert!(!row["uncertainty"].as_str().unwrap().is_empty());
    assert!(row["confidence"].as_f64().unwrap() > 0.0);
    assert!(r.notes[0].contains("1 session active"));

    let r = e
        .query("STATE project AT '2026-08-28T08:00:50Z'")
        .await
        .unwrap();
    let row = claude_row(&r.to_json());
    assert_eq!(row["last_attempt_outcome"], "succeeded");
    assert_eq!(row["last_attempt"], Value::from(att(&e, &sc.claude, 1, 1)));
    assert_eq!(row["turn_status"], "completed");

    // Codex's shell call runs 08:04:40–08:04:50, so it is in flight at :45.
    let r = e
        .query(&format!(
            "STATE session '{}' AT '2026-08-28T08:04:45Z'",
            ses(&sc.codex)
        ))
        .await
        .unwrap();
    let row = &r.to_json()[0];
    assert_eq!(row["in_flight_tool_calls"], Value::from(1));
    assert_eq!(strings(&row["in_flight_tool_call_ids"]).len(), 1);
    assert_eq!(row["last_attempt_outcome"], "in_progress");

    let r = e
        .query("STATE project AT '2026-08-28T09:00:00Z'")
        .await
        .unwrap();
    assert_eq!(r.kind, ResultKind::Empty);
    assert!(r.notes[0].contains("no session active"));
    let r = e
        .query(&format!(
            "STATE session '{}' AT '2026-08-28T07:00:00Z'",
            ses(&sc.claude)
        ))
        .await
        .unwrap();
    assert_eq!(r.kind, ResultKind::Empty);
    assert!(r.notes[1].contains("started at"));
    // Relative and keyword timestamps parse and resolve.
    for t in [
        "now",
        "-15m",
        "-2h",
        "-1d",
        "yesterday",
        "today",
        "'now'",
        "'-1w'",
    ] {
        e.query(&format!("STATE project AT {t}")).await.unwrap();
    }
    assert!(matches!(
        e.query("STATE attempt 'att_0000' AT now").await,
        Err(QueryError::Plan(_))
    ));
}

#[tokio::test]
async fn diff_state_reports_changed_fields() {
    let (e, sc) = engine().await;
    let r = e
        .query("DIFF STATE '2026-08-28T08:00:15Z' '2026-08-28T08:00:50Z'")
        .await
        .unwrap();
    assert_eq!(r.kind, ResultKind::Explanation);
    let json = r.to_json();
    let outcome = rows(&json)
        .iter()
        .find(|row| row["field"] == "last_attempt_outcome")
        .expect("outcome change");
    assert_eq!(outcome["before"], "failed");
    assert_eq!(outcome["after"], "succeeded");
    assert_eq!(outcome["change"], "changed");
    assert_eq!(outcome["session_id"], Value::from(ses(&sc.claude)));
    assert!(!strings(&outcome["evidence"]).is_empty());
    assert!(rows(&json).iter().any(|row| row["field"] == "turn_status"));

    let r = e
        .query("DIFF STATE '2026-08-28T08:00:50Z' '2026-08-28T08:04:35Z'")
        .await
        .unwrap();
    let json = r.to_json();
    assert!(
        rows(&json)
            .iter()
            .any(|row| row["change"] == "removed" && row["session_id"] == ses(&sc.claude))
    );
    assert!(
        rows(&json)
            .iter()
            .any(|row| row["change"] == "added" && row["session_id"] == ses(&sc.codex))
    );

    let r = e
        .query("DIFF STATE '2026-08-28T08:00:15Z' '2026-08-28T08:00:16Z'")
        .await
        .unwrap();
    assert_eq!(r.kind, ResultKind::Empty);
    assert!(matches!(
        e.query("DIFF STATE '2026-08-28T08:00:50Z' '2026-08-28T08:00:15Z'")
            .await,
        Err(QueryError::Plan(_))
    ));
    let r = e
        .query(&format!(
            "DIFF STATE session '{}' '2026-08-28T08:00:15Z' '2026-08-28T08:00:50Z'",
            ses(&sc.claude)
        ))
        .await
        .unwrap();
    assert!(r.row_count() >= 1);
}

#[tokio::test]
async fn what_is_project_doing_now() {
    let (e, _) = engine().await;
    let r = e.query("WHAT IS project DOING NOW").await.unwrap();
    // The scenario is fixed in 2026-08-28; both sessions have ended, so no
    // session is active and none had events in the last 15 minutes.
    assert_eq!(r.kind, ResultKind::Empty);
    assert!(
        r.notes.iter().any(|n| n.contains("last 15 minutes")),
        "{:?}",
        r.notes
    );

    let mut b = Stream::new();
    let live = Sess::codex("codex-live");
    let now = Timestamp::now();
    b.session_started(&live, now);
    b.prompt(
        &live,
        Timestamp::from_micros(now.as_micros() + 1_000_000),
        "keep going",
    );
    let e2 = QueryEngine::from_events(b.build()).await.unwrap();
    let r = e2.query("what is project doing now").await.unwrap();
    assert_eq!(r.kind, ResultKind::Explanation);
    assert_eq!(r.row_count(), 1);
    assert!(
        r.notes
            .iter()
            .any(|n| n.contains("sessions with events in the last 15 minutes")),
        "{:?}",
        r.notes
    );
}

// ---------------------------------------------------------------------------
// EXPLAIN, rendering, errors, robustness
// ---------------------------------------------------------------------------

#[tokio::test]
async fn explain_returns_a_plan() {
    let (e, _) = engine().await;
    let r = e
        .explain("SELECT * FROM attempts WHERE outcome = 'failed'")
        .await
        .unwrap();
    assert_eq!(r.kind, ResultKind::Explanation);
    assert!(r.row_count() >= 1);
    let text = r.render_table(None);
    assert!(text.contains("attempts"), "{text}");
    let r = e
        .query("EXPLAIN SHOW FAILED ATTEMPTS FOR provider = 'codex'")
        .await
        .unwrap();
    assert_eq!(r.kind, ResultKind::Explanation);
    assert!(
        r.notes
            .iter()
            .any(|n| n.contains("compiled SQL: SELECT * FROM attempts WHERE")),
        "{:?}",
        r.notes
    );
    let r = e
        .query("EXPLAIN SELECT count(*) FROM events")
        .await
        .unwrap();
    assert_eq!(r.kind, ResultKind::Explanation);
    let r = e.query("EXPLAIN WHY project STATUS BLOCKED").await.unwrap();
    assert_eq!(r.kind, ResultKind::Explanation);
    assert!(r.render_csv().contains("tier1"));
    let r = e.query("EXPLAIN TRACE project CAUSES").await.unwrap();
    assert_eq!(r.kind, ResultKind::Explanation);
}

#[tokio::test]
async fn rendering_uses_readable_ids() {
    let (e, sc) = engine().await;
    let r = e.query("SHOW ATTEMPTS LIMIT 1").await.unwrap();
    let json = r.to_json();
    let row = &json[0];
    assert!(row["attempt_id"].as_str().unwrap().starts_with("att_"));
    assert!(row["session_id"].as_str().unwrap().starts_with("ses_"));
    assert!(row["turn_id"].as_str().unwrap().starts_with("trn_"));
    assert!(row["project_id"].as_str().unwrap().starts_with("prj_"));
    assert!(
        strings(&row["evidence"])
            .iter()
            .all(|s| s.starts_with("ev_"))
    );
    assert!(
        strings(&row["tool_call_ids"])
            .iter()
            .all(|s| s.starts_with("spn_"))
    );
    assert!(row["started_at"].as_str().unwrap().ends_with('Z'));

    let table = r.render_table(None);
    assert!(table.contains("att_"), "{table}");
    assert!(table.contains("ses_"));
    assert!(table.contains("(1 row)"));
    // Notes are carried on the result, not baked into the table (the CLI
    // prints them after it).
    assert!(!r.notes.is_empty());
    assert!(!table.contains("note:"));
    assert!(table.lines().all(|l| l.chars().count() < 2000));
    // 22 columns cannot wrap readably into 120 characters, so the width
    // limit is ignored; a narrow result honours it.
    let wide = r.render_table(Some(120));
    assert!(wide.lines().any(|l| l.chars().count() > 120), "{wide}");
    let narrow = e.query("SHOW EVIDENCE FOR attempt 'att_0'").await;
    assert!(narrow.is_err());
    let narrow = e
        .sql("SELECT attempt_id, outcome, approach FROM attempts")
        .await
        .unwrap();
    let t = narrow.render_table(Some(60));
    assert!(t.lines().all(|l| l.chars().count() <= 60), "{t}");

    let csv = r.render_csv();
    let mut lines = csv.lines();
    assert!(
        lines
            .next()
            .unwrap()
            .starts_with("attempt_id,session_id,provider")
    );
    assert!(lines.next().unwrap().contains("att_"));

    // Raw storage schema also renders readable ids.
    let r = e.sql("SELECT event_id, session_id, kind, observed_at FROM events_raw ORDER BY source_seq LIMIT 1").await.unwrap();
    let row = &r.to_json()[0];
    assert!(row["event_id"].as_str().unwrap().starts_with("ev_"));
    assert_eq!(row["session_id"], Value::from(ses(&sc.claude)));
    assert_eq!(row["kind"], "session_started");
    assert_eq!(row["observed_at"], "2026-08-28T08:00:00.000000Z");
    assert!(r.render_table(None).contains("ev_"));

    // Long cells are truncated with an ellipsis.
    let r = e.sql("SELECT repeat('x', 200) AS long").await.unwrap();
    let t = r.render_table(None);
    assert!(t.contains('…'));
    assert!(!t.contains(&"x".repeat(81)));
}

#[tokio::test]
async fn parse_errors_have_positions() {
    let (e, _) = engine().await;
    let err = e.query("SHOW FOO").await.unwrap_err();
    match &err {
        QueryError::Parse { message, position } => {
            assert_eq!(*position, 5);
            assert!(message.contains("unexpected token 'FOO'"), "{message}");
            assert!(message.contains("expected ATTEMPTS"), "{message}");
        }
        other => panic!("unexpected {other:?}"),
    }
    let rendered = format_parse_error("SHOW FOO", &err);
    assert!(rendered.contains("  | SHOW FOO\n  |      ^"), "{rendered}");
    assert!(rendered.contains("position 5"));

    let err = e
        .query("WHAT IS project 'attemptdb' NOW")
        .await
        .unwrap_err();
    assert!(
        matches!(err, QueryError::Parse { position: 28, .. }),
        "{err:?}"
    );
    let err = e.query("STATE project AT 'soon'").await.unwrap_err();
    assert!(
        matches!(err, QueryError::Parse { position: 17, .. }),
        "{err:?}"
    );
    let err = e.query("SHOW ATTEMPTS 'unterminated").await.unwrap_err();
    assert!(
        matches!(err, QueryError::Parse { position: 14, .. }),
        "{err:?}"
    );
    let err = e.query("").await.unwrap_err();
    assert!(
        matches!(err, QueryError::Parse { position: 0, .. }),
        "{err:?}"
    );
    assert!(matches!(
        e.query("SELECT * FROM nope").await,
        Err(QueryError::Plan(_))
    ));
    assert!(matches!(
        e.query("SELECT ) FROM attempts").await,
        Err(QueryError::Plan(_))
    ));
}

/// Tiny xorshift so the garbage is reproducible without a dependency.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
        items[(self.next() % items.len() as u64) as usize]
    }
}

#[tokio::test]
async fn random_garbage_never_panics() {
    let (e, sc) = engine().await;
    let fragments = [
        "SHOW",
        "WHY",
        "TRACE",
        "STATE",
        "DIFF",
        "WHAT",
        "IS",
        "DOING",
        "NOW",
        "ATTEMPTS",
        "FAILED",
        "SESSIONS",
        "TOOL",
        "CALLS",
        "HANDOFFS",
        "EVIDENCE",
        "FOR",
        "WHERE",
        "LIMIT",
        "ORDER",
        "BY",
        "SINCE",
        "UNTIL",
        "DEPTH",
        "DIRECTION",
        "AT",
        "CAUSES",
        "STATUS",
        "BLOCKED",
        "BETWEEN",
        "AND",
        "project",
        "session",
        "attempt",
        "agent",
        "=",
        "'",
        "''",
        "(",
        ")",
        ";",
        ",",
        "-15m",
        "-",
        "0",
        "1",
        "99999999999999999999",
        "att_",
        "ses_",
        "ev_",
        "att_0191",
        "'2026-08-28'",
        "now",
        "yesterday",
        "SELECT",
        "*",
        "FROM",
        "events",
        "attempts",
        "\"",
        "\\",
        "\n",
        "\t",
        "€",
        "日本",
        "😀",
        "<",
        ">",
        "!",
        "%",
        "array_has",
        "count(*)",
        "/",
        "--",
        "x",
        "_",
        "outcome",
        "'failed'",
    ];
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let mut inputs: Vec<String> = Vec::new();
    for _ in 0..50 {
        let len = 1 + (rng.next() % 12) as usize;
        let mut s = String::new();
        for _ in 0..len {
            s.push_str(rng.pick(&fragments));
            if !rng.next().is_multiple_of(3) {
                s.push(' ');
            }
        }
        inputs.push(s);
    }
    inputs.extend(
        [
            "",
            "   ",
            "'",
            "\"",
            ";",
            "SHOW",
            "SHOW ATTEMPTS WHERE",
            "SHOW ATTEMPTS WHERE (",
            "SHOW ATTEMPTS WHERE 1/0 = 1",
            "TRACE",
            "TRACE x CAUSES DEPTH",
            "STATE project AT",
            "STATE project AT '99999999999999999999'",
            "STATE project AT -99999999999999999999d",
            "DIFF STATE 'a' 'b'",
            "WHY",
            "WHY STATUS",
            "EXPLAIN",
            "EXPLAIN EXPLAIN SHOW ATTEMPTS",
            "SHOW ATTEMPTS LIMIT 99999999999999999999",
            "SHOW ATTEMPTS FOR session = 'ses_zz'",
            "SHOW ATTEMPTS FOR path = '%'",
            "SHOW HANDOFFS BETWEEN agent = 'a' AND agent = 'a'",
            "SHOW EVIDENCE FOR project",
            "SHOW ATTEMPTS ORDER BY \"; DROP TABLE attempts\"",
            "SELECT",
            "SELECT * FROM events_raw WHERE event_id = 'x'",
            "WITH",
            "SHOW TABLES",
            "SHOW COLUMNS FROM attempts",
            "DESCRIBE attempts",
            "SELECT arrow_cast(1, 'nope')",
            "WHAT IS DOING NOW",
            "STATE session AT now",
            "TRACE att_ CAUSES",
            "TRACE ses_ CAUSES",
        ]
        .iter()
        .map(|s| s.to_string()),
    );
    inputs.push(format!("TRACE {} CAUSES DEPTH 0", ses(&sc.claude)));
    inputs.push(format!(
        "SHOW ATTEMPTS FOR session = '{}' AND session = 'ses_0'",
        ses(&sc.claude)
    ));
    for input in &inputs {
        // Either outcome is fine; the point is that nothing panics.
        let _ = e.query(input).await;
    }
    assert!(inputs.len() > 80);
}

// ---------------------------------------------------------------------------
// Empty engine and real database
// ---------------------------------------------------------------------------

#[tokio::test]
async fn empty_engine_registers_all_tables_and_answers() {
    let e = QueryEngine::from_events(Vec::new()).await.unwrap();
    assert_eq!(e.event_count(), 0);
    let tables = e.tables().unwrap();
    assert_eq!(tables.len(), TABLE_NAMES.len());
    assert!(tables.iter().all(|t| t.rows == 0 && !t.columns.is_empty()));
    for text in [
        "SHOW ATTEMPTS",
        "SHOW FAILED ATTEMPTS",
        "SHOW SESSIONS",
        "SHOW TURNS",
        "SHOW TOOL CALLS",
        "SHOW HANDOFFS",
        "SHOW EDGES",
        "SHOW SIGNALS",
        "SHOW DECISIONS",
        "WHY project STATUS BLOCKED",
        "STATE project AT now",
        "DIFF STATE yesterday now",
        "WHAT IS project DOING NOW",
        "SELECT count(*) FROM events",
        "SELECT * FROM events_raw",
        "SELECT * FROM attempts WHERE outcome = 'failed'",
    ] {
        let r = e
            .query(text)
            .await
            .unwrap_or_else(|err| panic!("{text}: {err}"));
        assert!(r.row_count() <= 1, "{text}");
        let _ = r.to_json();
        let _ = r.render_table(Some(80));
    }
    let r = e.query("SHOW ATTEMPTS").await.unwrap();
    assert_eq!(r.kind, ResultKind::Empty);
    assert!(r.column_names().contains(&"superseded_by".to_string()));
    let r = e.query("WHAT IS project DOING NOW").await.unwrap();
    assert!(r.notes.iter().any(|n| n.contains("no events loaded")));
    assert!(matches!(
        e.query("SHOW ATTEMPTS FOR project = 'x'").await,
        Err(QueryError::NotFound(_))
    ));
}

#[tokio::test]
async fn database_round_trip_through_datafusion() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("db.attemptdb");
    let sc = spec_scenario();
    let split = sc.events.len() / 2;
    let mut db = Database::open(
        &root,
        OpenOptions {
            create: true,
            ..Default::default()
        },
    )
    .unwrap();
    db.ingest(sc.events[..split].to_vec()).unwrap();
    db.flush().unwrap(); // first half becomes an immutable segment
    db.ingest(sc.events[split..].to_vec()).unwrap(); // second half stays in the memtable
    assert_eq!(db.stats().segments, 1);
    assert_eq!(db.stats().memtable_rows, sc.events.len() - split);

    let e = QueryEngine::from_database(&db, &ScanFilter::default())
        .await
        .unwrap();
    assert_eq!(e.event_count(), sc.events.len());
    for table in ["events", "events_raw"] {
        let r = e
            .sql(&format!("SELECT count(*) AS n FROM {table}"))
            .await
            .unwrap();
        assert_eq!(
            r.to_json()[0]["n"],
            Value::from(sc.events.len() as u64),
            "{table}"
        );
    }
    // Dictionary columns group and filter; FixedSizeBinary ids compare and render.
    let r = e
        .sql("SELECT provider, count(*) AS n FROM events_raw GROUP BY provider ORDER BY provider")
        .await
        .unwrap();
    let json = r.to_json();
    assert_eq!(json[0]["provider"], "claude_code");
    assert_eq!(json[1]["provider"], "codex");
    let r = e
        .sql("SELECT count(*) AS n FROM events_raw WHERE kind = 'tool_call_failed'")
        .await
        .unwrap();
    assert_eq!(r.to_json()[0]["n"], Value::from(1));
    let r = e
        .sql("SELECT event_id, session_id, tool_name FROM events_raw WHERE kind = 'tool_call_failed'")
        .await
        .unwrap();
    let row = &r.to_json()[0];
    assert_eq!(row["event_id"], Value::from(ev(&sc.edit_fail_end)));
    assert_eq!(row["session_id"], Value::from(ses(&sc.claude)));
    assert_eq!(row["tool_name"], "Edit");
    let r = e
        .sql("SELECT count(*) AS n FROM events_raw r JOIN events e ON e.source_seq = r.source_seq WHERE e.event_id = concat('ev_', arrow_cast(r.event_id, 'Utf8')) OR true")
        .await
        .unwrap();
    assert_eq!(r.to_json()[0]["n"], Value::from(sc.events.len() as u64));

    let r = e.query("SHOW FAILED ATTEMPTS").await.unwrap();
    assert_eq!(r.row_count(), 1);
    assert!(
        r.to_json()[0]["superseded_by"]
            .as_str()
            .unwrap()
            .starts_with("att_")
    );
    let r = e.query("SHOW HANDOFFS").await.unwrap();
    assert_eq!(r.row_count(), 1);
    let r = e
        .query(&format!("SHOW EVIDENCE FOR {}", att(&e, &sc.claude, 1, 0)))
        .await
        .unwrap();
    assert!(r.row_count() >= 5);

    // A row-level filter re-encodes the scan so the events table matches.
    let filter = ScanFilter {
        session_id: Some(sc.codex.session_id),
        ..Default::default()
    };
    let e2 = QueryEngine::from_database(&db, &filter).await.unwrap();
    let codex_events = sc
        .events
        .iter()
        .filter(|x| x.session_id == sc.codex.session_id)
        .count();
    assert_eq!(e2.event_count(), codex_events);
    let r = e2.sql("SELECT count(*) AS n FROM events").await.unwrap();
    assert_eq!(r.to_json()[0]["n"], Value::from(codex_events as u64));
    assert_eq!(e2.projection().sessions.len(), 1);
    let r = e2.query("SHOW HANDOFFS").await.unwrap();
    assert_eq!(r.kind, ResultKind::Empty);

    // Sanity: the projection ids are identical to the in-memory engine's.
    let mem = QueryEngine::from_events(sc.events.clone()).await.unwrap();
    assert_eq!(
        mem.projection()
            .attempts
            .iter()
            .map(|a| a.attempt_id)
            .collect::<Vec<_>>(),
        e.projection()
            .attempts
            .iter()
            .map(|a| a.attempt_id)
            .collect::<Vec<_>>()
    );
    assert_eq!(
        e.projection().attempts[0].outcome,
        AttemptOutcome::Superseded
    );
    db.close().unwrap();
}

// ---------------------------------------------------------------------------
// Work units, decisions, corrections, retractions
// ---------------------------------------------------------------------------

fn wu(engine: &QueryEngine) -> String {
    let u = &engine.projection().work_units[0];
    format!("wu_{}", u.work_unit_id)
}

#[tokio::test]
async fn work_units_table_and_show_work_units() {
    let (e, sc) = engine().await;
    let names: Vec<String> = e.tables().unwrap().into_iter().map(|t| t.name).collect();
    assert_eq!(names, TABLE_NAMES);
    let by_name = |n: &str| {
        e.tables()
            .unwrap()
            .into_iter()
            .find(|t| t.name == n)
            .unwrap()
    };
    assert_eq!(by_name("work_units").rows, 1);
    assert_eq!(by_name("decisions").rows, 1);
    assert_eq!(by_name("corrections").rows, 0);
    assert_eq!(by_name("retractions").rows, 0);
    assert!(by_name("attempts").has_column("work_unit_id"));
    assert!(by_name("attempts").has_column("retracted"));
    assert!(by_name("events").has_column("retracted"));
    assert!(by_name("work_units").has_column("failed_attempt_count"));

    let r = e.query("SHOW WORK UNITS").await.unwrap();
    assert_eq!(r.kind, ResultKind::Rows);
    assert_eq!(r.row_count(), 1);
    let u = &r.to_json()[0];
    assert_eq!(u["work_unit_id"], Value::from(wu(&e)));
    assert_eq!(u["phase"], "implement");
    assert_eq!(u["status"], "completed");
    assert_eq!(u["project_name"], "acme/repo");
    assert_eq!(u["objective"], "Fix the failing parser test");
    assert_eq!(
        u["objective_event_id"],
        Value::from(ev(&sc.claude_prompt_1))
    );
    assert_eq!(u["session_count"], Value::from(2));
    assert_eq!(u["turn_count"], Value::from(3));
    assert_eq!(u["attempt_count"], Value::from(4));
    assert_eq!(u["failed_attempt_count"], Value::from(1));
    assert_eq!(strings(&u["actors"]), vec!["claude_code", "codex"]);
    assert_eq!(strings(&u["paths"]), vec!["src/parser.rs", "README.md"]);
    assert_eq!(
        strings(&u["sessions"]),
        vec![ses(&sc.claude), ses(&sc.codex)]
    );
    assert_eq!(u["confidence"], Value::from(0.7));
    assert_eq!(u["version"], Value::from(1));
    assert!(!strings(&u["evidence"]).is_empty());
    assert!(u["phase_reason"].as_str().unwrap().contains("shell"));
    assert!(
        u["status_reason"]
            .as_str()
            .unwrap()
            .contains("session ended")
    );
    assert!(
        u["ended_at"]
            .as_str()
            .unwrap()
            .starts_with("2026-08-28T08:05:00")
    );
    assert!(r.notes[0].contains("heuristic"), "{:?}", r.notes);

    // Attempts point at their unit.
    let r = e
        .sql("SELECT count(*) AS n FROM attempts WHERE work_unit_id IS NOT NULL")
        .await
        .unwrap();
    assert_eq!(r.to_json()[0]["n"], Value::from(4));

    // Filters.
    let n = |sql: &str| {
        let e = &e;
        let sql = sql.to_string();
        async move { e.query(&sql).await.map(|r| r.row_count()) }
    };
    assert_eq!(
        n("SHOW WORK UNITS FOR phase = 'implement'").await.unwrap(),
        1
    );
    assert_eq!(n("SHOW WORK UNITS FOR phase = blocked").await.unwrap(), 0);
    assert_eq!(
        n("SHOW WORK UNITS FOR status = 'completed'").await.unwrap(),
        1
    );
    assert_eq!(n("SHOW WORK UNITS FOR status = 'open'").await.unwrap(), 0);
    assert_eq!(
        n("SHOW WORK UNITS FOR provider = 'codex'").await.unwrap(),
        1
    );
    assert_eq!(n("SHOW WORK UNITS FOR agent = 'cursor'").await.unwrap(), 0);
    assert_eq!(
        n("SHOW WORK UNITS FOR project = 'acme/repo'")
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        n("SHOW WORK UNITS FOR path = 'README.md'").await.unwrap(),
        1
    );
    assert_eq!(n("SHOW WORK UNITS FOR path = 'src/*'").await.unwrap(), 1);
    assert_eq!(
        n(&format!(
            "SHOW WORK UNITS FOR session = '{}'",
            ses(&sc.codex)
        ))
        .await
        .unwrap(),
        1
    );
    assert_eq!(
        n("SHOW WORK UNITS SINCE '2026-08-28T08:01:00Z'")
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        n("SHOW WORK_UNITS WHERE failed_attempt_count > 0")
            .await
            .unwrap(),
        1
    );
    assert!(matches!(
        e.query("SHOW WORK UNITS FOR phase = 'nope'").await,
        Err(QueryError::Plan(_))
    ));
    assert!(matches!(
        e.query("SHOW ATTEMPTS FOR phase = 'implement'").await,
        Err(QueryError::Plan(_))
    ));

    // EXPLAIN compiles SHOW WORK UNITS and SHOW DECISIONS to SQL.
    let r = e
        .query("EXPLAIN SHOW WORK UNITS FOR phase = 'implement'")
        .await
        .unwrap();
    assert!(
        r.notes
            .iter()
            .any(|n| n.contains("SELECT * FROM work_units"))
    );
    let r = e.query("EXPLAIN SHOW DECISIONS").await.unwrap();
    assert!(
        r.notes
            .iter()
            .any(|n| n.contains("SELECT * FROM decisions"))
    );
}

#[tokio::test]
async fn work_unit_subjects_in_why_state_trace_and_evidence() {
    let (e, sc) = engine().await;
    let id = wu(&e);

    // Not blocked: state mismatch, no invented justification.
    let r = e.query(&format!("WHY {id} STATUS BLOCKED")).await.unwrap();
    assert_eq!(r.kind, ResultKind::Empty);
    assert!(r.notes[0].contains("not blocked"), "{:?}", r.notes);
    assert!(r.notes[0].contains("state_mismatch"));
    let r = e
        .query(&format!("WHY work_unit '{id}' BLOCKED"))
        .await
        .unwrap();
    assert_eq!(r.kind, ResultKind::Empty);
    assert!(matches!(
        e.query(&format!("WHY {id} STATUS FAILED")).await,
        Err(QueryError::Plan(_))
    ));
    assert!(matches!(
        e.query("WHY wu_00000000 STATUS BLOCKED").await,
        Err(QueryError::NotFound(_))
    ));

    // Evidence for a unit is the union of its attempts' evidence.
    let r = e.query(&format!("SHOW EVIDENCE FOR {id}")).await.unwrap();
    assert_eq!(r.row_count(), e.projection().work_units[0].evidence.len());
    let short = &id[..12];
    let r = e
        .query(&format!("SHOW EVIDENCE FOR work_unit '{short}'"))
        .await
        .unwrap();
    assert!(r.row_count() > 10);

    // The unit owns its turns in the graph.
    let r = e
        .query(&format!("TRACE {id} CAUSES DIRECTION DOWN DEPTH 1"))
        .await
        .unwrap();
    let json = r.to_json();
    assert_eq!(rows(&json).len(), 3);
    assert!(
        rows(&json)
            .iter()
            .all(|s| s["edge_kind"] == "parent_of" && s["to_type"] == "turn")
    );
    assert_eq!(json[0]["from_type"], "work_unit");
    let r = e
        .sql("SELECT count(*) AS n FROM edges WHERE from_type = 'work_unit'")
        .await
        .unwrap();
    assert_eq!(r.to_json()[0]["n"], Value::from(3));

    // STATE lists the unit while it is open and drops it once completed.
    let r = e
        .query("STATE project AT '2026-08-28T08:00:15Z'")
        .await
        .unwrap();
    let json = r.to_json();
    let unit = rows(&json)
        .iter()
        .find(|row| row["subject_type"] == "work_unit")
        .expect("unit row");
    assert_eq!(unit["subject_id"], Value::from(id.clone()));
    assert_eq!(unit["work_unit_id"], Value::from(id.clone()));
    assert_eq!(unit["phase"], "debug");
    assert_eq!(unit["status"], "open");
    assert_eq!(unit["attempt_count"], Value::from(1));
    assert_eq!(unit["failed_attempt_count"], Value::from(1));
    assert_eq!(unit["last_attempt_outcome"], "failed");
    assert_eq!(unit["last_failure_class"], "string_mismatch");
    assert_eq!(unit["is_open"], Value::Bool(true));
    assert_eq!(unit["blocked"], Value::Bool(false));
    assert_eq!(unit["provider"], "claude_code");
    assert!(unit["uncertainty"].as_str().unwrap().contains("tier1"));
    assert!(!strings(&unit["evidence"]).is_empty());
    assert!(
        r.notes.iter().any(|n| n.contains("1 work unit open")),
        "{:?}",
        r.notes
    );
    let session_row = rows(&json)
        .iter()
        .find(|row| row["subject_type"] == "session")
        .expect("session row");
    assert_eq!(session_row["session_id"], Value::from(ses(&sc.claude)));
    assert_eq!(session_row["phase"], Value::Null);

    let r = e
        .query(&format!("STATE {id} AT '2026-08-28T08:04:45Z'"))
        .await
        .unwrap();
    let json = r.to_json();
    assert_eq!(rows(&json).len(), 1, "only the unit row for a unit subject");
    assert_eq!(json[0]["phase"], "implement");
    assert_eq!(json[0]["provider"], "claude_code, codex");
    assert_eq!(json[0]["last_attempt_outcome"], "in_progress");
    let r = e
        .query(&format!("STATE work_unit '{id}' AT '2026-08-28T09:00:00Z'"))
        .await
        .unwrap();
    assert_eq!(r.kind, ResultKind::Empty);
    assert!(
        r.notes
            .iter()
            .any(|n| n.contains("1 completed or abandoned"))
    );

    // DIFF reports the phase change of the unit.
    let r = e
        .query("DIFF STATE '2026-08-28T08:00:15Z' '2026-08-28T08:00:50Z'")
        .await
        .unwrap();
    let json = r.to_json();
    let phase = rows(&json)
        .iter()
        .find(|row| row["subject_type"] == "work_unit" && row["field"] == "phase")
        .expect("phase change");
    assert_eq!(phase["before"], "debug");
    assert_eq!(phase["after"], "implement");
    assert_eq!(phase["subject_id"], Value::from(id.clone()));
    assert_eq!(phase["session_id"], Value::Null);
    assert!(
        rows(&json)
            .iter()
            .any(|row| row["subject_type"] == "session" && row["field"] == "last_attempt_outcome")
    );
    assert!(r.notes[0].contains("1 work unit"));
    let r = e
        .query("DIFF STATE '2026-08-28T08:04:45Z' '2026-08-28T09:00:00Z'")
        .await
        .unwrap();
    let json = r.to_json();
    let removed = rows(&json)
        .iter()
        .find(|row| row["subject_type"] == "work_unit" && row["change"] == "removed")
        .expect("unit removed");
    assert!(
        removed["after"]
            .as_str()
            .unwrap()
            .contains("status completed")
    );
}

#[tokio::test]
async fn blocked_work_unit_is_explained() {
    let mut b = Stream::new();
    let s = Sess::claude("blocked-unit");
    b.session_started(&s, at(0));
    b.prompt(&s, at(1), "delete the build directory");
    b.tool_start(&s, at(2), &Tool::edit(Some("e1"), &["Makefile"]));
    b.tool_finish(
        &s,
        at(3),
        &Tool::edit(Some("e1"), &["Makefile"]),
        attemptdb_core::Outcome::success(),
    );
    let perm = b.permission_requested(&s, at(4), &Tool::shell(Some("s1")));
    let e = QueryEngine::from_events(b.build()).await.unwrap();
    let id = wu(&e);
    let r = e
        .query("SHOW WORK UNITS FOR phase = 'blocked'")
        .await
        .unwrap();
    assert_eq!(r.row_count(), 1);
    assert_eq!(r.to_json()[0]["blocking_signal"], Value::from(ev(&perm)));
    let r = e.query(&format!("WHY {id} STATUS BLOCKED")).await.unwrap();
    assert_eq!(r.kind, ResultKind::Explanation);
    let row = &r.to_json()[0];
    assert!(
        row["claim"]
            .as_str()
            .unwrap()
            .contains("permission request")
    );
    assert_eq!(strings(&row["evidence"]), vec![ev(&perm)]);
    assert_eq!(row["phase"], "blocked");
    assert_eq!(row["blocking_signal"], Value::from(ev(&perm)));
    assert!(row["confidence"].as_f64().unwrap() > 0.5);
    let r = e
        .query("STATE project AT '2026-08-28T08:00:04Z'")
        .await
        .unwrap();
    let json = r.to_json();
    let unit = rows(&json)
        .iter()
        .find(|r| r["subject_type"] == "work_unit")
        .unwrap();
    assert_eq!(unit["blocked"], Value::Bool(true));
    assert!(
        unit["block_claim"]
            .as_str()
            .unwrap()
            .contains("pending-input")
    );
    // The session-level WHY mentions the blocked unit when it is not itself blocked.
    let r = e.query("WHY project STATUS BLOCKED").await.unwrap();
    assert_eq!(r.row_count(), 1);
}

#[tokio::test]
async fn corrections_table_and_corrected_attempts() {
    let sc = spec_scenario();
    let mut b = Stream::new();
    b.events = sc.events.clone();
    let mem = QueryEngine::from_events(sc.events.clone()).await.unwrap();
    let a11 = att(&mem, &sc.claude, 1, 1);
    let corr = b.correction(
        &sc.claude,
        at(400),
        "attempt_outcome",
        &a11,
        Some("failed"),
        Some("wrong_fix"),
        Some("broke the other tests"),
    );
    let e = QueryEngine::from_events(b.build()).await.unwrap();

    let r = e.query("SHOW CORRECTIONS").await.unwrap();
    assert_eq!(r.row_count(), 1);
    let c = &r.to_json()[0];
    assert_eq!(c["event_id"], Value::from(ev(&corr)));
    assert_eq!(c["correction_type"], "attempt_outcome");
    assert_eq!(c["target_type"], "attempt");
    assert_eq!(c["target"], Value::from(a11.clone()));
    assert_eq!(c["outcome"], "failed");
    assert_eq!(c["failure_class"], "wrong_fix");
    assert_eq!(c["note"], "broke the other tests");
    assert_eq!(c["status"], "applied");
    assert_eq!(c["session_id"], Value::from(ses(&sc.claude)));
    assert!(r.notes[0].contains("human"));

    let r = e.query("SHOW FAILED ATTEMPTS").await.unwrap();
    assert_eq!(
        r.row_count(),
        2,
        "the corrected attempt now counts as failed"
    );
    let row = rows(&r.to_json())
        .iter()
        .find(|row| row["attempt_id"] == a11)
        .cloned()
        .unwrap();
    assert_eq!(row["outcome"], "failed");
    assert_eq!(row["failure_class"], "wrong_fix");
    assert_eq!(row["inferred_outcome"], "succeeded");
    assert_eq!(row["corrected_by"], Value::from(ev(&corr)));
    assert_eq!(row["correction_type"], "attempt_outcome");
    assert_eq!(row["note"], "broke the other tests");
    let r = e.query(&format!("WHY {a11} FAILED")).await.unwrap();
    assert_eq!(r.kind, ResultKind::Explanation);
    let row = &r.to_json()[0];
    assert!(row["claim"].as_str().unwrap().contains("human correction"));
    assert!(strings(&row["evidence"]).contains(&ev(&corr)));

    // The correction event is in the events table but in no session row.
    let r = e
        .sql("SELECT kind, provider, retracted FROM events WHERE kind = 'correction'")
        .await
        .unwrap();
    assert_eq!(r.row_count(), 1);
    assert_eq!(r.to_json()[0]["provider"], "attemptdb");
    assert_eq!(r.to_json()[0]["retracted"], Value::Bool(false));
    let r = e.query("SHOW SESSIONS").await.unwrap();
    assert_eq!(r.to_json()[0]["event_count"], Value::from(8));
    assert_eq!(r.row_count(), 2);

    let r = e
        .query("SHOW CORRECTIONS FOR status = 'applied'")
        .await
        .unwrap();
    assert_eq!(r.row_count(), 1);
    let r = e
        .query("SHOW CORRECTIONS FOR outcome = 'failed'")
        .await
        .unwrap();
    assert_eq!(r.row_count(), 1);
    assert!(matches!(
        e.query("SHOW CORRECTIONS FOR provider = 'codex'").await,
        Err(QueryError::Plan(_))
    ));
}

#[tokio::test]
async fn retractions_hide_rows_unless_including_retracted() {
    let sc = spec_scenario();
    let mut b = Stream::new();
    b.events = sc.events.clone();
    let r_ev = b.retraction(
        &sc.codex,
        at(400),
        "session",
        &ses(&sc.codex),
        "benchmark",
        Some("benchmark run"),
    );
    let events = b.build();
    let e = QueryEngine::from_events(events.clone()).await.unwrap();
    assert_eq!(
        e.event_count(),
        events.len(),
        "retracted events stay loaded"
    );

    // The events view flags the retracted session's events.
    let r = e
        .sql("SELECT count(*) AS n FROM events WHERE retracted")
        .await
        .unwrap();
    assert_eq!(r.to_json()[0]["n"], Value::from(8));
    let r = e
        .sql("SELECT count(*) AS n FROM events WHERE NOT retracted")
        .await
        .unwrap();
    assert_eq!(
        r.to_json()[0]["n"],
        Value::from(17),
        "16 claude events + the retraction"
    );
    let r = e
        .sql("SELECT retracted FROM events WHERE kind = 'retraction'")
        .await
        .unwrap();
    assert_eq!(r.to_json()[0]["retracted"], Value::Bool(false));

    // SHOW hides retracted rows by default and says so.
    let r = e.query("SHOW SESSIONS").await.unwrap();
    assert_eq!(r.row_count(), 1);
    assert_eq!(r.to_json()[0]["session_id"], Value::from(ses(&sc.claude)));
    assert!(
        r.notes.iter().any(|n| n.contains("1 retracted row hidden")),
        "{:?}",
        r.notes
    );
    let r = e.query("SHOW SESSIONS INCLUDING RETRACTED").await.unwrap();
    assert_eq!(r.row_count(), 2);
    let codex = rows(&r.to_json())
        .iter()
        .find(|row| row["session_id"] == ses(&sc.codex))
        .cloned()
        .unwrap();
    assert_eq!(codex["retracted"], Value::Bool(true));
    assert_eq!(codex["coverage"], "full");
    assert_eq!(e.query("SHOW ATTEMPTS").await.unwrap().row_count(), 3);
    assert_eq!(
        e.query("SHOW ATTEMPTS INCLUDING RETRACTED")
            .await
            .unwrap()
            .row_count(),
        4
    );
    assert_eq!(e.query("SHOW TURNS").await.unwrap().row_count(), 2);
    assert_eq!(
        e.query("SHOW TURNS INCLUDING RETRACTED")
            .await
            .unwrap()
            .row_count(),
        3
    );
    assert_eq!(e.query("SHOW TOOL CALLS").await.unwrap().row_count(), 5);
    assert_eq!(
        e.query("SHOW TOOL CALLS INCLUDING RETRACTED")
            .await
            .unwrap()
            .row_count(),
        7
    );
    let r = e
        .query("SHOW ATTEMPTS FOR provider = 'codex' INCLUDING RETRACTED")
        .await
        .unwrap();
    assert_eq!(r.row_count(), 1);
    assert_eq!(r.to_json()[0]["retracted"], Value::Bool(true));
    assert_eq!(r.to_json()[0]["project_name"], "acme/repo");
    let r = e.query("SHOW HANDOFFS").await.unwrap();
    assert_eq!(r.kind, ResultKind::Empty);
    let r = e.query("SHOW HANDOFFS INCLUDING RETRACTED").await.unwrap();
    assert_eq!(r.kind, ResultKind::Empty);
    assert!(r.notes.iter().any(|n| n.contains("no retracted rows")));

    // Retractions table.
    let r = e.query("SHOW RETRACTIONS").await.unwrap();
    assert_eq!(r.row_count(), 1);
    let row = &r.to_json()[0];
    assert_eq!(row["event_id"], Value::from(ev(&r_ev)));
    assert_eq!(row["target_type"], "session");
    assert_eq!(row["target"], Value::from(ses(&sc.codex)));
    assert_eq!(row["reason"], "benchmark");
    assert_eq!(row["note"], "benchmark run");
    assert_eq!(row["matched"], Value::Bool(true));
    assert_eq!(row["retracted_events"], Value::from(8));

    // Evidence for the retracted session is hidden unless asked for.
    let r = e
        .query(&format!("SHOW EVIDENCE FOR {}", ses(&sc.codex)))
        .await;
    assert!(
        matches!(r, Err(QueryError::NotFound(_))),
        "retracted sessions are not subjects"
    );
    let r = e.query("WHAT IS project DOING NOW").await.unwrap();
    assert_eq!(r.kind, ResultKind::Empty);

    // The work unit no longer spans the codex session.
    let r = e.query("SHOW WORK UNITS").await.unwrap();
    assert_eq!(strings(&r.to_json()[0]["sessions"]), vec![ses(&sc.claude)]);
    assert_eq!(strings(&r.to_json()[0]["actors"]), vec!["claude_code"]);

    // Retracting an attempt keeps its row for INCLUDING RETRACTED and marks
    // its tool-call events retracted.
    let mut b = Stream::new();
    b.events = sc.events.clone();
    let a10 = att(&e, &sc.claude, 1, 0);
    b.retraction(&sc.claude, at(400), "attempt", &a10, "test", None);
    let e2 = QueryEngine::from_events(b.build()).await.unwrap();
    assert_eq!(
        e2.query("SHOW FAILED ATTEMPTS").await.unwrap().kind,
        ResultKind::Empty
    );
    let r = e2
        .query("SHOW FAILED ATTEMPTS INCLUDING RETRACTED")
        .await
        .unwrap();
    assert_eq!(r.row_count(), 1);
    assert_eq!(r.to_json()[0]["attempt_id"], Value::from(a10));
    assert_eq!(r.to_json()[0]["retracted"], Value::Bool(true));
    let r = e2
        .sql("SELECT count(*) AS n FROM events WHERE retracted")
        .await
        .unwrap();
    assert_eq!(r.to_json()[0]["n"], Value::from(4));
    let r = e2
        .sql("SELECT count(*) AS n FROM tool_calls WHERE retracted")
        .await
        .unwrap();
    assert_eq!(r.to_json()[0]["n"], Value::from(2));
    assert_eq!(
        e2.query("SHOW DECISIONS").await.unwrap().kind,
        ResultKind::Empty
    );
    // The evidence of the surviving sibling never included the retracted calls.
    let a11 = att(&e2, &sc.claude, 1, 1);
    let r = e2.query(&format!("SHOW EVIDENCE FOR {a11}")).await.unwrap();
    assert!(r.row_count() >= 5);
    let r = e2
        .query(&format!("SHOW EVIDENCE FOR {}", ses(&sc.claude)))
        .await
        .unwrap();
    assert_eq!(
        r.row_count(),
        16 - 4 + 1,
        "session evidence minus retracted calls plus the retraction"
    );
    let r = e2
        .query(&format!(
            "SHOW EVIDENCE FOR {} INCLUDING RETRACTED",
            ses(&sc.claude)
        ))
        .await
        .unwrap();
    assert_eq!(r.row_count(), 17);
}

/// The engine itself refuses DDL, DML and statements. The UI and MCP have
/// keyword-prefix checks for a friendlier message, but a caller that forgets
/// one must still be unable to make DataFusion read or write the local
/// filesystem: `CREATE EXTERNAL TABLE … LOCATION` reads any file the process
/// can, and `COPY … TO` writes one.
#[tokio::test]
async fn engine_is_read_only_at_the_engine_layer() {
    let (e, _) = engine().await;
    let dir = std::env::temp_dir().join(format!("attemptdb-ro-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let secret = dir.join("secret.csv");
    std::fs::write(&secret, "canary_value_1\n").unwrap();
    let out = dir.join("copied.csv");
    let loc = secret.display().to_string().replace('\\', "/");
    let out_loc = out.display().to_string().replace('\\', "/");

    let refused = [
        format!("CREATE EXTERNAL TABLE leak STORED AS CSV LOCATION '{loc}'"),
        format!("CREATE EXTERNAL TABLE leak (line VARCHAR) STORED AS CSV LOCATION '{loc}'"),
        "CREATE TABLE x AS SELECT 1".to_string(),
        "CREATE VIEW v AS SELECT 1".to_string(),
        "INSERT INTO events VALUES (1)".to_string(),
        "SET datafusion.execution.batch_size = 1".to_string(),
        format!("COPY (SELECT 1 AS x) TO '{out_loc}' STORED AS CSV"),
    ];
    for stmt in &refused {
        // Both entry points: `sql` directly and `query`, whose `is_sql`
        // routes CREATE/INSERT/SET to SQL.
        assert!(e.sql(stmt).await.is_err(), "engine accepted: {stmt}");
        assert!(e.query(stmt).await.is_err(), "query() accepted: {stmt}");
        assert!(e.explain(stmt).await.is_err(), "explain() accepted: {stmt}");
    }
    assert!(!out.exists(), "COPY wrote a file despite being refused");

    // Plain queries still work.
    let r = e.sql("SELECT count(*) AS n FROM events").await.unwrap();
    assert_eq!(r.kind, ResultKind::Rows);
    let r = e.explain("SELECT 1").await.unwrap();
    assert_eq!(r.kind, ResultKind::Explanation);

    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn show_commits_lists_git_commits_tied_to_the_head_they_produced() {
    use attemptdb_core::Outcome;
    let s = common::Sess::claude("commit-session");
    let mut st = common::Stream::new();
    let set_head = |st: &mut common::Stream, sha: &str| {
        let ev = st.events.last_mut().unwrap();
        ev.project.head = Some(sha.into());
        ev.project.branch = Some("main".into());
    };
    st.session_started(&s, at(0));
    set_head(&mut st, "aaa1111");
    st.prompt(&s, at(1), "ship it");
    set_head(&mut st, "aaa1111");
    st.shell_classified(
        &s,
        at(2),
        at(3),
        "c1",
        "git",
        Some("commit"),
        Outcome::success(),
    );
    let n = st.events.len();
    st.events[n - 2].project.head = Some("aaa1111".into());
    st.events[n - 1].project.head = Some("bbb2222".into());
    st.stop(&s, at(4));
    set_head(&mut st, "bbb2222");
    let e = QueryEngine::from_events(st.build()).await.expect("engine");

    let r = e.query("SHOW COMMITS").await.unwrap();
    assert_eq!(r.kind, ResultKind::Rows);
    assert_eq!(r.row_count(), 1);
    let row = &r.to_json()[0];
    assert_eq!(row["sha"], "bbb2222");
    assert_eq!(row["previous_sha"], "aaa1111");
    assert_eq!(row["linkage"], "end_event");
    assert!(row["commit_id"].as_str().unwrap().starts_with("cmt_"));
    assert!(row["attempt_id"].as_str().unwrap().starts_with("att_"));
    assert_eq!(row["evidence"].as_array().unwrap().len(), 2);

    // SQL sees the same table, and attempts carry the sha.
    let r = e
        .query("SELECT a.attempt_id, a.commit_shas FROM attempts a WHERE array_length(a.commit_shas) > 0")
        .await
        .unwrap();
    assert_eq!(r.row_count(), 1);
    let r = e
        .query("SELECT count(*) AS n FROM commits WHERE sha IS NOT NULL")
        .await
        .unwrap();
    assert_eq!(r.to_json()[0]["n"], 1);
    let r = e
        .query(&format!("SHOW COMMITS FOR session = '{}'", s.session_id))
        .await
        .unwrap();
    assert_eq!(r.row_count(), 1);
}
