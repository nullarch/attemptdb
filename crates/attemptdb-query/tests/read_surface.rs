//! The read surfaces' guarantees at the engine: path globs match per path,
//! a `WHERE` clause cannot rewrite the retraction filter, statements are
//! bounded in rows, bytes, time and memory, and retraction is redaction on
//! the bounded surfaces.

mod common;

use attemptdb_query::{CancelToken, CapReason, QueryEngine, QueryLimits, ResultKind};
use common::{Sess, Stream, Tool, at, spec_scenario};
use serde_json::Value;
use std::time::{Duration, Instant};

fn limits(rows: usize) -> QueryLimits {
    QueryLimits::new(rows, 1 << 20)
}

// ---------------------------------------------------------------------------
// Path globs (REPORT 7.3)
// ---------------------------------------------------------------------------

/// Twelve Edit calls with hand-chosen paths. Each call is one `tool_calls`
/// row, so every count below is a count of rows in this list.
async fn glob_engine() -> QueryEngine {
    let mut b = Stream::new();
    let s = Sess::claude("glob");
    b.session_started(&s, at(0));
    b.prompt(&s, at(1), "edit things");
    let calls: &[&[&str]] = &[
        &["internal/a.rs"],                // 1
        &["internal/sub/b.rs"],            // 2
        &["src/internal/c.rs"],            // 3: internal/ is not at the start
        &["internal_x/d.rs"],              // 4: `_` is not `/`
        &["docs/x.md", "internal/y.rs"],   // 5: the second path matches
        &["internal/z.md", "src/main.rs"], // 6: the first path matches
        &["lib.rs"],                       // 7
        &["src/a_b.rs"],                   // 8
        &["src/axb.rs"],                   // 9: `_` must not match `x`
        &["100%/n.txt"],                   // 10
        &["weird.name+(1).rs"],            // 11: regex metacharacters
        &["a.rs", "b.md"],                 // 12: a match on the first, not the last
    ];
    for (i, paths) in calls.iter().enumerate() {
        let t = 10 + 10 * i as i64;
        let id = format!("c{i}");
        let tool = Tool::edit(Some(&id), paths);
        b.tool_start(&s, at(t), &tool);
        b.tool_finish(&s, at(t + 1), &tool, attemptdb_core::Outcome::success());
    }
    b.stop(&s, at(500));
    QueryEngine::from_events(b.build()).await.unwrap()
}

async fn count(e: &QueryEngine, glob: &str) -> usize {
    e.query(&format!("SHOW TOOL CALLS FOR path = '{glob}' LIMIT 100"))
        .await
        .unwrap()
        .row_count()
}

#[tokio::test]
async fn a_glob_matches_each_path_on_its_own() {
    let e = glob_engine().await;
    assert_eq!(count(&e, "internal/*").await, 4, "calls 1, 2, 5 and 6");
    assert_eq!(count(&e, "*.rs").await, 11, "every call but 10");
    assert_eq!(count(&e, "*").await, 12);
    // Exact paths are not globs.
    assert_eq!(count(&e, "src/a_b.rs").await, 1);
    assert_eq!(count(&e, "a.rs").await, 1);
    assert_eq!(count(&e, "internal").await, 0);
    // `_` is a character, not a wildcard.
    assert_eq!(count(&e, "src/a_*").await, 1, "a_b.rs, not axb.rs");
    assert_eq!(count(&e, "*_x/*").await, 1);
    assert_eq!(count(&e, "internal_*").await, 1);
    // Regex metacharacters are literal.
    assert_eq!(count(&e, "weird.name+(1).*").await, 1);
    assert_eq!(count(&e, "weird.name+(1)?rs").await, 0);
    assert_eq!(count(&e, "weirdXname+(1).*").await, 0);
    assert_eq!(count(&e, "*(*").await, 1, "a parenthesis is a character");
    assert_eq!(
        count(&e, "(*").await,
        0,
        "and a path does not start with one"
    );
    // `%` was always accepted as a wildcard.
    assert_eq!(count(&e, "100%").await, 1);
    // A glob is anchored at both ends of a path.
    assert_eq!(count(&e, "nal/*").await, 0);
    assert_eq!(count(&e, "internal/").await, 0);
    assert_eq!(count(&e, "*/a.r").await, 0);
    assert_eq!(count(&e, "*/a.rs").await, 1, "internal/a.rs");
    // Attempts and work units use the same compilation.
    let attempts = e
        .query("SHOW ATTEMPTS FOR path = 'internal/*'")
        .await
        .unwrap();
    assert!(attempts.row_count() >= 1);
    let json = attempts.to_json();
    for row in json.as_array().unwrap() {
        let paths = row["paths"].as_array().unwrap();
        assert!(
            paths
                .iter()
                .any(|p| p.as_str().unwrap().starts_with("internal/")),
            "{paths:?}"
        );
    }
    // The compiled statement is visible in EXPLAIN notes, and is a per-path
    // match rather than a match on the joined list.
    let r = e
        .query("EXPLAIN SHOW TOOL CALLS FOR path = 'internal/*'")
        .await
        .unwrap();
    let notes = r.notes.join("\n");
    assert!(notes.contains("regexp_like"), "{notes}");
}

// ---------------------------------------------------------------------------
// WHERE <sql> cannot rewrite the retraction filter (REPORT 7.9)
// ---------------------------------------------------------------------------

async fn retracted_engine() -> (QueryEngine, Sess, Sess) {
    let sc = spec_scenario();
    let mut b = Stream::new();
    b.events = sc.events.clone();
    b.retraction(
        &sc.codex,
        at(400),
        "session",
        &format!("ses_{}", sc.codex.session_id),
        "benchmark",
        Some("benchmark run"),
    );
    let e = QueryEngine::from_events(b.build()).await.unwrap();
    (e, sc.claude, sc.codex)
}

#[tokio::test]
async fn a_where_clause_is_one_expression_and_the_retraction_filter_stays_outside_it() {
    let (e, claude, codex) = retracted_engine().await;
    let visible = e.query("SHOW SESSIONS").await.unwrap();
    assert_eq!(visible.row_count(), 1, "the retracted session is hidden");
    let hidden = format!("ses_{}", codex.session_id);
    assert!(!visible.to_json().to_string().contains(&hidden));
    assert!(
        visible
            .to_json()
            .to_string()
            .contains(&claude.session_id.to_string())
    );

    // The injection from the report, and variants: all refuse to parse.
    for attack in [
        "SHOW SESSIONS WHERE true) OR (retracted",
        "SHOW SESSIONS WHERE true) OR retracted OR (false",
        "SHOW SESSIONS WHERE (true) OR (retracted",
        "SHOW SESSIONS WHERE true)) OR ((retracted",
        "SHOW SESSIONS WHERE retracted, true",
    ] {
        let err = e.query(attack).await.expect_err(attack);
        assert!(
            err.to_string().contains("one SQL expression"),
            "{attack}: {err}"
        );
    }
    // Anything after the clause that is not another clause is a parse error.
    assert!(
        e.query("SHOW SESSIONS WHERE true LIMIT 1) OR (retracted")
            .await
            .is_err()
    );
    // A second statement is not a clause.
    assert!(
        e.query("SHOW SESSIONS WHERE 1 = 1; SELECT 1")
            .await
            .is_err()
    );
    // A comment cannot swallow the closing parenthesis or the filter.
    let r = e
        .query("SHOW SESSIONS WHERE true /* ) OR (retracted */")
        .await
        .unwrap();
    assert_eq!(r.row_count(), 1);
    let r = e
        .query("SHOW SESSIONS WHERE retracted OR true -- ) OR retracted\n LIMIT 10")
        .await
        .unwrap();
    assert_eq!(r.row_count(), 1, "(retracted OR true) AND NOT retracted");
    assert!(!r.to_json().to_string().contains(&hidden));
    // A predicate that names the flag cannot turn the filter off either.
    let r = e.query("SHOW SESSIONS WHERE retracted").await.unwrap();
    assert_eq!(r.row_count(), 0);
    // Subqueries are SQL like any other and stay inside the parentheses.
    let r = e
        .query(
            "SHOW SESSIONS WHERE session_id IN (SELECT session_id FROM sessions WHERE retracted)",
        )
        .await
        .unwrap();
    assert_eq!(
        r.row_count(),
        0,
        "the outer table's rows are still filtered"
    );
    // A well-formed predicate still filters.
    let r = e
        .query("SHOW SESSIONS WHERE provider = 'claude_code' AND event_count > 3")
        .await
        .unwrap();
    assert_eq!(r.row_count(), 1);
    // INCLUDING RETRACTED is the only way back, and it is explicit.
    let r = e.query("SHOW SESSIONS INCLUDING RETRACTED").await.unwrap();
    assert_eq!(r.row_count(), 2);
}

// ---------------------------------------------------------------------------
// Bounded execution (REPORT 7.1)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_row_cap_is_in_the_plan_not_after_the_collect() {
    let e = QueryEngine::from_events(Vec::new()).await.unwrap();
    let started = Instant::now();
    let r = e
        .query_limited("SELECT * FROM generate_series(1, 30000000)", &limits(3))
        .await
        .unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "30 million rows were not produced: {:?}",
        started.elapsed()
    );
    assert_eq!(r.row_count(), 3);
    assert!(r.truncated);
    assert_eq!(r.kind, ResultKind::Rows);
    // Exactly at the cap is not truncation; one past it is.
    let r = e
        .sql_limited("SELECT * FROM generate_series(1, 3)", &limits(3))
        .await
        .unwrap();
    assert_eq!((r.row_count(), r.truncated), (3, false));
    let r = e
        .sql_limited("SELECT * FROM generate_series(1, 4)", &limits(3))
        .await
        .unwrap();
    assert_eq!((r.row_count(), r.truncated), (3, true));
    // A smaller LIMIT of the statement's own wins.
    let r = e
        .sql_limited("SELECT * FROM generate_series(1, 100) LIMIT 2", &limits(10))
        .await
        .unwrap();
    assert_eq!((r.row_count(), r.truncated), (2, false));
    // Empty stays empty; EXPLAIN is still an explanation.
    let r = e
        .sql_limited("SELECT * FROM generate_series(1, 0)", &limits(3))
        .await
        .unwrap();
    assert_eq!(r.kind, ResultKind::Empty);
    let r = e.sql_limited("EXPLAIN SELECT 1", &limits(3)).await.unwrap();
    assert_eq!(r.kind, ResultKind::Explanation);
}

#[tokio::test]
async fn a_cross_join_over_the_events_table_is_cut_at_the_cap() {
    let sc = spec_scenario();
    let e = QueryEngine::from_events(sc.events.clone()).await.unwrap();
    let r = e
        .query_limited(
            "SELECT a.event_id, b.event_id AS other FROM events a, events b",
            &limits(5),
        )
        .await
        .unwrap();
    assert_eq!(r.row_count(), 5);
    assert!(r.truncated);
    // AttemptQL SHOW is cut in the plan too, and says so.
    let r = e
        .query_limited("SHOW TOOL CALLS LIMIT 1000", &limits(2))
        .await
        .unwrap();
    assert_eq!(r.row_count(), 2);
    assert!(r.truncated);
    assert!(r.notes.join("\n").contains("row limit of this surface"));
    // Projection-computed statements are cut after they are computed.
    let r = e
        .query_limited("STATE project AT now", &limits(1))
        .await
        .unwrap();
    assert!(r.row_count() <= 1);
}

#[tokio::test]
async fn a_statement_that_runs_too_long_is_stopped() {
    let e = QueryEngine::from_events(Vec::new()).await.unwrap();
    let mut l = limits(10);
    l.timeout = Some(Duration::from_millis(300));
    let started = Instant::now();
    // No row can come out before the count is done, and the count is a
    // hundred billion rows away.
    let err = e
        .query_limited("SELECT count(*) FROM generate_series(1, 100000000000)", &l)
        .await
        .expect_err("must time out");
    let took = started.elapsed();
    assert!(err.to_string().contains("time limit"), "{err}");
    assert!(took < Duration::from_secs(5), "{took:?}");
    // The engine is not left busy: the next statement answers at once.
    let started = Instant::now();
    let r = e
        .query_limited("SELECT 1 AS one", &limits(10))
        .await
        .unwrap();
    assert_eq!(r.row_count(), 1);
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[tokio::test]
async fn cancelling_stops_a_statement_in_flight() {
    let e = QueryEngine::from_events(Vec::new()).await.unwrap();
    let token = CancelToken::new();
    let mut l = limits(10).with_cancel(token.clone());
    l.timeout = None;
    let canceller = {
        let token = token.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            token.cancel();
        })
    };
    let started = Instant::now();
    let err = e
        .query_limited("SELECT count(*) FROM generate_series(1, 100000000000)", &l)
        .await
        .expect_err("must be cancelled");
    canceller.await.unwrap();
    assert!(err.to_string().contains("cancelled"), "{err}");
    assert!(started.elapsed() < Duration::from_secs(5));
    // A token that is already cancelled stops the next statement at once.
    let err = e
        .query_limited("SELECT count(*) FROM generate_series(1, 100000000000)", &l)
        .await
        .expect_err("already cancelled");
    assert!(err.to_string().contains("cancelled"), "{err}");
}

#[tokio::test]
async fn a_statement_cannot_take_more_memory_than_its_pool() {
    let e = QueryEngine::from_events(Vec::new()).await.unwrap();
    let mut l = limits(10);
    l.memory_bytes = Some(2 << 20);
    // Building a hash table of three million rows needs far more than two
    // MiB, and spilling to temporary files is off.
    let err = e
        .query_limited(
            "SELECT count(*) AS n FROM generate_series(1, 3000000) a JOIN generate_series(1, 3000000) b ON a.value = b.value",
            &l,
        )
        .await
        .expect_err("must run out of its pool");
    let text = err.to_string();
    // Not DataFusion's dump of every consumer: what happened and what to do.
    assert!(text.contains("needed too much memory"), "{text}");
    assert!(text.contains("WHERE or LIMIT"), "{text}");
    assert!(!text.contains("Memory consumers"), "{text}");
    assert!(text.len() < 600, "{} bytes: {text}", text.len());
    // The same statement with room works, and bounds its output.
    l.memory_bytes = Some(2 << 30);
    let r = e
        .query_limited(
            "SELECT a.value FROM generate_series(1, 30000) a JOIN generate_series(1, 30000) b ON a.value = b.value",
            &l,
        )
        .await
        .unwrap();
    assert_eq!(r.row_count(), 10);
    assert!(r.truncated);
}

#[tokio::test]
async fn the_byte_budget_bounds_what_is_converted() {
    let e = QueryEngine::from_events(Vec::new()).await.unwrap();
    let r = e
        .sql_limited(
            "SELECT value, repeat('x', 100000) AS blob FROM generate_series(1, 40)",
            &limits(40),
        )
        .await
        .unwrap();
    assert_eq!(r.row_count(), 40);
    // 40 rows of 100 kB under a 250 kB budget with 20 kB cells: each row
    // costs ~20 kB, so about twelve fit.
    let c = r.capped(40, 250_000, 20_000);
    assert_eq!(c.stopped_by, Some(CapReason::Bytes));
    assert!(c.returned() >= 10 && c.returned() <= 13, "{}", c.returned());
    assert!(c.bytes <= 250_000, "{}", c.bytes);
    assert_eq!(c.omitted_rows, 40 - c.returned());
    assert_eq!(c.clipped_cells, c.returned());
    let json = c.json_array();
    let blob = json[0]["blob"].as_str().unwrap();
    assert!(
        blob.len() < 20_100 && blob.contains("cut:"),
        "{}",
        blob.len()
    );
    // The renderings carry only the kept rows.
    assert_eq!(c.render_csv().lines().count(), 1 + c.returned());
    assert!(
        c.render_table(None)
            .ends_with(&format!("({} rows)", c.returned()))
    );
    // The row cap is its own reason.
    let c = r.capped(3, usize::MAX, 20_000);
    assert_eq!((c.returned(), c.stopped_by), (3, Some(CapReason::Rows)));
    // A first row that alone exceeds the budget is not kept.
    let c = r.capped(40, 1_000, 20_000);
    assert_eq!((c.returned(), c.stopped_by), (0, Some(CapReason::Bytes)));
    // Unbounded is what `to_json` does.
    assert_eq!(r.to_json().as_array().unwrap().len(), 40);
}

// ---------------------------------------------------------------------------
// Retraction is redaction on the bounded surfaces (REPORT 7.2)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn retracted_content_reads_as_null_on_the_bounded_surfaces() {
    let (e, _claude, codex) = retracted_engine().await;
    let probe = "Continue the parser fix and run the tests";

    // The owner's CLI path: retraction hides, it does not redact.
    let r = e
        .sql("SELECT content_json FROM events WHERE retracted AND content_json IS NOT NULL")
        .await
        .unwrap();
    assert!(r.row_count() > 0);
    assert!(r.to_json().to_string().contains(probe));

    // The bounded path: the same rows, with the text withheld.
    let r = e
        .sql_limited(
            "SELECT content_json, raw_json, unknown_json FROM events WHERE retracted",
            &limits(100),
        )
        .await
        .unwrap();
    assert!(r.row_count() > 0, "the rows are there");
    let text = r.to_json().to_string();
    assert!(!text.contains(probe), "{text}");
    for row in r.to_json().as_array().unwrap() {
        assert_eq!(row["content_json"], Value::Null);
        assert_eq!(row["raw_json"], Value::Null);
        assert_eq!(row["unknown_json"], Value::Null);
    }
    assert!(
        r.notes
            .iter()
            .any(|n| n.contains("NULL for retracted rows")),
        "{:?}",
        r.notes
    );
    // Filtering on the text cannot probe a retracted row either.
    let r = e
        .sql_limited(
            "SELECT count(*) AS n FROM events WHERE retracted AND content_json LIKE '%parser fix%'",
            &limits(10),
        )
        .await
        .unwrap();
    assert_eq!(r.to_json()[0]["n"], Value::from(0));
    let r = e
        .sql(
            "SELECT count(*) AS n FROM events WHERE retracted AND content_json LIKE '%parser fix%'",
        )
        .await
        .unwrap();
    assert_eq!(r.to_json()[0]["n"], Value::from(1));
    // Rows that are not retracted keep their text.
    let r = e
        .sql_limited(
            "SELECT count(*) AS n FROM events WHERE NOT retracted AND content_json LIKE '%Fix the failing parser test%'",
            &limits(10),
        )
        .await
        .unwrap();
    assert_eq!(r.to_json()[0]["n"], Value::from(1));
    // A join sees the masked values too.
    let r = e
        .sql_limited(
            "SELECT count(*) AS n FROM events a JOIN events b ON a.event_id = b.event_id WHERE b.content_json LIKE '%parser fix%' AND a.retracted",
            &limits(10),
        )
        .await
        .unwrap();
    assert_eq!(r.to_json()[0]["n"], Value::from(0));
    // events_raw has no flag to decide by: its text is not served at all.
    let r = e
        .sql_limited(
            "SELECT count(content_json) AS c, count(raw_json) AS r FROM events_raw",
            &limits(10),
        )
        .await
        .unwrap();
    assert_eq!(r.to_json()[0]["c"], Value::from(0));
    assert_eq!(r.to_json()[0]["r"], Value::from(0));
    // Columns that are not text are untouched: the retracted flag itself.
    let r = e
        .sql_limited(
            "SELECT count(*) AS n FROM events WHERE retracted AND session_id = $1"
                .replace("$1", &format!("'ses_{}'", codex.session_id))
                .as_str(),
            &limits(10),
        )
        .await
        .unwrap();
    assert!(r.to_json()[0]["n"].as_u64().unwrap() > 0);
}

#[tokio::test]
async fn retracted_attempts_and_turns_lose_their_objective() {
    let sc = spec_scenario();
    let mut b = Stream::new();
    b.events = sc.events.clone();
    let probe = Sess::claude("probe");
    let _ = probe;
    // Retract the whole Claude session: its prompts are the objectives.
    b.retraction(
        &sc.claude,
        at(400),
        "session",
        &format!("ses_{}", sc.claude.session_id),
        // Not `privacy`: that reason already removes the objective from the
        // projection itself (the owner's unmasked CLI view too), and this
        // test is about what the MCP/UI surfaces mask on top of it.
        "mistake",
        None,
    );
    let e = QueryEngine::from_events(b.build()).await.unwrap();

    let r = e.query("SHOW TURNS INCLUDING RETRACTED").await.unwrap();
    assert!(
        r.to_json()
            .to_string()
            .contains("Fix the failing parser test")
    );
    let r = e
        .query_limited("SHOW TURNS INCLUDING RETRACTED", &limits(50))
        .await
        .unwrap();
    let text = r.to_json().to_string();
    assert!(!text.contains("Fix the failing parser test"), "{text}");
    assert!(!text.contains("Now document the parser module"), "{text}");
    let retracted_rows: Vec<_> = r
        .to_json()
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["retracted"] == Value::Bool(true))
        .cloned()
        .collect();
    assert!(!retracted_rows.is_empty());
    for row in &retracted_rows {
        assert_eq!(row["objective"], Value::Null);
        assert!(row["prompt_chars"].is_number(), "metadata stays: {row}");
    }
    // The Codex session was not retracted: its objective is still there.
    assert!(text.contains("Continue the parser fix"), "{text}");
    let r = e
        .query_limited("SHOW ATTEMPTS INCLUDING RETRACTED", &limits(50))
        .await
        .unwrap();
    let rows = r.to_json();
    for row in rows.as_array().unwrap() {
        if row["retracted"] == Value::Bool(true) {
            assert_eq!(row["objective"], Value::Null, "{row}");
        }
    }

    // A `privacy` retraction goes further than the surfaces: the projection
    // itself drops the objective, so even the owner's unmasked view has none.
    let mut b = Stream::new();
    b.events = sc.events.clone();
    b.retraction(
        &sc.claude,
        at(400),
        "session",
        &format!("ses_{}", sc.claude.session_id),
        "privacy",
        None,
    );
    let e = QueryEngine::from_events(b.build()).await.unwrap();
    let r = e.query("SHOW TURNS INCLUDING RETRACTED").await.unwrap();
    let text = r.to_json().to_string();
    assert!(!text.contains("Fix the failing parser test"), "{text}");
    assert!(
        text.contains("Continue the parser fix"),
        "the Codex session stays: {text}"
    );
}

#[tokio::test]
async fn retracted_rows_lose_where_they_happened_and_a_correction_its_note() {
    let sc = spec_scenario();
    let before = attemptdb_project::project(&sc.events);
    let codex_attempt = before
        .attempts
        .iter()
        .find(|a| a.session_id == sc.codex.session_id)
        .expect("the Codex session has an attempt")
        .attempt_id;
    let claude_attempt = before
        .attempts
        .iter()
        .find(|a| a.session_id == sc.claude.session_id)
        .expect("the Claude session has an attempt")
        .attempt_id;
    let mut b = Stream::new();
    b.events = sc.events.clone();
    // A note on an attempt of the session about to be retracted, and one on
    // an attempt that stays.
    b.correction(
        &sc.codex,
        at(390),
        "attempt_note",
        &format!("att_{codex_attempt}"),
        None,
        None,
        Some("the private reason this failed"),
    );
    b.correction(
        &sc.claude,
        at(391),
        "attempt_note",
        &format!("att_{claude_attempt}"),
        None,
        None,
        Some("a note on work that stays"),
    );
    b.retraction(
        &sc.codex,
        at(400),
        "session",
        &format!("ses_{}", sc.codex.session_id),
        "privacy",
        None,
    );
    let e = QueryEngine::from_events(b.build()).await.unwrap();
    let codex = format!("ses_{}", sc.codex.session_id);

    // The owner's path sees everything: retraction hides, it does not redact.
    let owner = e
        .sql(&format!(
            "SELECT count(paths_json) AS p, count(path_logical) AS l, count(path_relative) AS r FROM events WHERE session_id = '{codex}'"
        ))
        .await
        .unwrap();
    let row = &owner.to_json()[0];
    assert!(
        row["p"].as_u64().unwrap() > 0 && row["l"].as_u64().unwrap() > 0,
        "the fixture's retracted session touched paths: {row}"
    );

    // The bounded surfaces serve none of it for the retracted session ...
    let r = e
        .sql_limited(
            &format!(
                "SELECT count(paths_json) AS p, count(path_logical) AS l, count(path_relative) AS r FROM events WHERE session_id = '{codex}'"
            ),
            &limits(10),
        )
        .await
        .unwrap();
    assert_eq!(
        r.to_json()[0],
        serde_json::json!({"p": 0, "l": 0, "r": 0}),
        "paths of retracted events"
    );
    let r = e
        .sql_limited(
            &format!("SELECT path_logical FROM events WHERE session_id = '{codex}'"),
            &limits(10),
        )
        .await
        .unwrap();
    assert!(
        r.notes.iter().any(|n| n.contains("path_logical")),
        "a result that carries a path column says paths are masked too: {:?}",
        r.notes
    );
    // ... and keep them for events that are not retracted.
    let r = e
        .sql_limited(
            "SELECT count(path_logical) AS l FROM events WHERE NOT retracted",
            &limits(10),
        )
        .await
        .unwrap();
    assert!(r.to_json()[0]["l"].as_u64().unwrap() > 0);
    // events_raw has no flag to decide by: it serves no location at all.
    let r = e
        .sql_limited(
            "SELECT count(paths_json) AS p, count(path_logical) AS l, count(path_relative) AS r FROM events_raw",
            &limits(10),
        )
        .await
        .unwrap();
    assert_eq!(r.to_json()[0], serde_json::json!({"p": 0, "l": 0, "r": 0}));
    // Filtering on a masked column cannot probe it.
    let r = e
        .sql_limited(
            "SELECT count(*) AS n FROM events WHERE retracted AND path_logical LIKE '%'",
            &limits(10),
        )
        .await
        .unwrap();
    assert_eq!(r.to_json()[0]["n"], Value::from(0));

    // The projection tables that list the retracted work's files: the owner
    // sees them (so the zeros below are the masking, not an empty fixture).
    let owner = e
        .sql("SELECT CAST(sum(coalesce(cardinality(paths), 0)) AS BIGINT) AS n FROM attempts WHERE retracted")
        .await
        .unwrap();
    assert!(
        owner.to_json()[0]["n"].as_i64().unwrap() > 0,
        "{:?}",
        owner.to_json()
    );
    let r = e
        .sql_limited(
            "SELECT count(path_relative) AS rel, CAST(sum(coalesce(cardinality(paths), 0)) AS BIGINT) AS n FROM tool_calls WHERE retracted",
            &limits(10),
        )
        .await
        .unwrap();
    assert_eq!(r.to_json()[0]["rel"], Value::from(0), "{:?}", r.to_json());
    assert_eq!(r.to_json()[0]["n"], Value::from(0), "{:?}", r.to_json());
    let r = e
        .sql_limited(
            "SELECT count(approach) AS a, CAST(sum(coalesce(cardinality(paths), 0)) AS BIGINT) AS n FROM attempts WHERE retracted",
            &limits(10),
        )
        .await
        .unwrap();
    assert_eq!(r.to_json()[0]["a"], Value::from(0), "{:?}", r.to_json());
    assert_eq!(r.to_json()[0]["n"], Value::from(0), "{:?}", r.to_json());
    let r = e
        .sql_limited(
            "SELECT count(*) AS n FROM attempts WHERE NOT retracted AND cardinality(paths) > 0",
            &limits(10),
        )
        .await
        .unwrap();
    assert!(
        r.to_json()[0]["n"].as_u64().unwrap() > 0,
        "visible work keeps its paths"
    );

    // A correction's note: gone when its session was retracted, kept
    // otherwise. (The projection calls a correction of an attempt inside a
    // retracted session `target_not_found`; the surface does not depend on
    // the label.)
    let owner = e
        .sql("SELECT status, note FROM corrections ORDER BY corrected_at")
        .await
        .unwrap();
    let rows = owner.to_json();
    assert_ne!(rows[0]["status"], "applied", "{rows}");
    assert_eq!(rows[0]["note"], "the private reason this failed", "{rows}");
    let bounded = e
        .sql_limited(
            "SELECT status, note FROM corrections ORDER BY corrected_at",
            &limits(10),
        )
        .await
        .unwrap();
    let rows = bounded.to_json();
    assert_eq!(rows[0]["note"], Value::Null, "{rows}");
    assert_eq!(rows[1]["note"], "a note on work that stays", "{rows}");
    let text = bounded.to_json().to_string();
    assert!(!text.contains("private reason"), "{text}");
    assert!(
        bounded
            .notes
            .iter()
            .any(|n| n.contains("NULL for retracted rows")),
        "{:?}",
        bounded.notes
    );
    // Probing the note cannot see it either.
    let r = e
        .sql_limited(
            "SELECT count(*) AS n FROM corrections WHERE note LIKE '%private%'",
            &limits(10),
        )
        .await
        .unwrap();
    assert_eq!(r.to_json()[0]["n"], Value::from(0));
}

#[tokio::test]
async fn the_note_of_a_correction_of_a_retracted_attempt_is_withheld_too() {
    let sc = spec_scenario();
    let before = attemptdb_project::project(&sc.events);
    let attempt = before
        .attempts
        .iter()
        .find(|a| a.session_id == sc.codex.session_id)
        .unwrap()
        .attempt_id;
    let mut b = Stream::new();
    b.events = sc.events.clone();
    b.correction(
        &sc.codex,
        at(390),
        "attempt_note",
        &format!("att_{attempt}"),
        None,
        None,
        Some("a note about work that is then retracted"),
    );
    // Not `privacy`: that already clears the note in the projection itself.
    b.retraction(
        &sc.codex,
        at(400),
        "attempt",
        &format!("att_{attempt}"),
        "mistake",
        None,
    );
    let e = QueryEngine::from_events(b.build()).await.unwrap();
    let owner = e.sql("SELECT status, note FROM corrections").await.unwrap();
    assert_eq!(owner.to_json()[0]["status"], "target_retracted");
    assert!(
        owner.to_json()[0]["note"].is_string(),
        "the owner still reads it"
    );
    let r = e
        .sql_limited("SELECT status, note FROM corrections", &limits(10))
        .await
        .unwrap();
    assert_eq!(r.to_json()[0]["status"], "target_retracted");
    assert_eq!(r.to_json()[0]["note"], Value::Null);
}
