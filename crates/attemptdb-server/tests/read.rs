//! The read API end to end: scopes, parity with the local projection, the
//! per-tenant cache, the inference merge rule, and tenant isolation.

mod common;

use attemptdb_core::Timestamp;
use attemptdb_project::{ALGORITHM_VERSION, project};
use common::{
    ADMIN_ALPHA, KEY_ALPHA, KEY_BETA, READER_ALPHA, READER_BETA, Running, StartOptions, at, batch,
    call, device, device_keys, get, inference_batch, post, reader_keys, scan, scenario, start_with,
};
use serde_json::{Value, json};
use std::collections::BTreeSet;

async fn start_readers() -> Running {
    let mut keys = device_keys();
    keys.extend(reader_keys());
    start_with(StartOptions {
        keys,
        ..Default::default()
    })
    .await
}

/// Upload the reference story for alpha's device and return it.
async fn seed(r: &Running) -> common::Scenario {
    let d1 = device("d1");
    let sc = scenario(d1);
    let (status, ack) = post(r.addr, Some(KEY_ALPHA), batch(d1, "seed", &sc.events)).await;
    assert_eq!(status, 200, "{ack}");
    assert_eq!(ack["accepted"], sc.events.len());
    sc
}

fn ids(list: &Value, key: &str) -> BTreeSet<String> {
    list.as_array()
        .unwrap()
        .iter()
        .map(|v| v[key].as_str().unwrap().to_string())
        .collect()
}

fn attempts_in(timeline: &Value) -> Vec<Value> {
    let mut out = Vec::new();
    for s in timeline["sessions"].as_array().unwrap() {
        for t in s["turns"].as_array().unwrap() {
            out.extend(t["attempts"].as_array().unwrap().iter().cloned());
        }
    }
    out
}

const READ_ROUTES: &[&str] = &[
    "/v1/status",
    "/v1/sessions",
    "/v1/timeline",
    "/v1/work",
    "/v1/attention",
    "/v1/state",
    "/v1/events",
];

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_routes_need_a_reader_or_admin_key() {
    let mut r = start_readers().await;
    let addr = r.addr;
    for route in READ_ROUTES {
        let (status, body) = get(addr, route, KEY_ALPHA).await;
        assert_eq!(status, 403, "{route}: device key must be refused: {body}");
        assert!(
            body["error"].as_str().unwrap().contains("device"),
            "{route}: the message names the scope: {body}"
        );
        let (status, _) = get(addr, route, "nope").await;
        assert_eq!(status, 401, "{route}: unknown key");
        let (status, body) = get(addr, route, READER_ALPHA).await;
        assert_eq!(status, 200, "{route}: reader: {body}");
        assert_eq!(body["tenant"], "alpha");
        assert_eq!(body["algorithm_version"], ALGORITHM_VERSION);
        assert!(body["generated_at"].is_string());
        let (status, _) = get(addr, route, ADMIN_ALPHA).await;
        assert_eq!(status, 200, "{route}: admin");
    }
    let stmt = json!({ "statement": "SHOW SESSIONS" });
    let (status, body) = call(addr, "POST", "/v1/query", KEY_ALPHA, stmt.clone()).await;
    assert_eq!(status, 403, "{body}");
    let (status, _) = call(addr, "POST", "/v1/query", "nope", stmt.clone()).await;
    assert_eq!(status, 401);
    let (status, body) = call(addr, "POST", "/v1/query", READER_ALPHA, stmt).await;
    assert_eq!(status, 200, "{body}");
    // `GET /v1/inferences` keeps its device-scoped behaviour: a reader key
    // answers for its own (empty) device, and a device key still works.
    let (status, body) = get(addr, "/v1/inferences", READER_ALPHA).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["kinds"], json!([]));
    let (status, _) = get(addr, "/v1/inferences", KEY_ALPHA).await;
    assert_eq!(status, 200);
    r.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reads_match_the_local_projection_of_the_tenant_database() {
    let mut r = start_readers().await;
    let addr = r.addr;
    let sc = seed(&r).await;

    // The reference: project the tenant's database exactly as `attempt`
    // would on a device.
    let stored = scan(&r.tenant_dir("alpha"));
    assert_eq!(stored.len(), sc.events.len());
    let p = project(stored.iter());
    assert_eq!(p.sessions.len(), 3);
    let expected_sessions: BTreeSet<String> = p
        .sessions
        .iter()
        .map(|s| format!("ses_{}", s.session_id))
        .collect();
    let expected_attempts: BTreeSet<String> = p
        .attempts
        .iter()
        .map(|a| format!("att_{}", a.attempt_id))
        .collect();

    // Sessions: same count, same ids, newest first, device id and state.
    let (status, sessions) = get(addr, "/v1/sessions", READER_ALPHA).await;
    assert_eq!(status, 200, "{sessions}");
    assert_eq!(sessions["total"], 3);
    assert_eq!(sessions["open"], 1);
    assert_eq!(ids(&sessions["sessions"], "session_id"), expected_sessions);
    let list = sessions["sessions"].as_array().unwrap();
    assert_eq!(list[0]["session_id"], sc.blocked.readable(), "newest first");
    assert_eq!(list[0]["state"], "open");
    assert_eq!(list[2]["state"], "closed");
    assert!(
        list.iter()
            .all(|s| s["device_id"] == format!("dev_{}", device("d1")))
    );
    assert!(
        list.iter()
            .all(|s| s["project_id"] == format!("prj_{}", sc.project.project_id))
    );
    assert!(sessions["next_cursor"].is_null());
    let claude = list
        .iter()
        .find(|s| s["session_id"] == sc.claude.readable())
        .unwrap();
    assert_eq!(claude["turn_count"], 1);
    assert_eq!(claude["attempt_count"], 2);
    assert_eq!(claude["failure_count"], 1);
    assert_eq!(claude["provider"], "claude_code");

    // Timeline: attempts under turns, one to one with the projection's.
    let (status, timeline) = get(addr, "/v1/timeline", READER_ALPHA).await;
    assert_eq!(status, 200, "{timeline}");
    assert_eq!(timeline["events"], sc.events.len());
    assert_eq!(timeline["total_sessions"], 3);
    assert_eq!(timeline["total_attempts"], p.attempts.len());
    let attempts = attempts_in(&timeline);
    assert_eq!(attempts.len(), p.attempts.len());
    let got: BTreeSet<String> = attempts
        .iter()
        .map(|a| a["attempt_id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(got, expected_attempts);
    for a in &attempts {
        assert_eq!(a["computed_by"], "server");
        assert_eq!(a["algorithm_version"], ALGORITHM_VERSION);
        assert!(!a["evidence"].as_array().unwrap().is_empty());
        assert!(a["confidence"].as_f64().unwrap() > 0.0);
    }
    let failed = attempts
        .iter()
        .find(|a| a["outcome"] == "superseded")
        .expect("the failed edit was superseded by the retry");
    assert_eq!(failed["failure_class"], "string_mismatch");
    assert!(
        failed["evidence"]
            .as_array()
            .unwrap()
            .contains(&json!(format!("ev_{}", sc.edit_fail_end))),
        "{failed}"
    );
    assert_eq!(timeline["handoffs_total"], p.handoffs.len());
    let handoffs = timeline["handoffs"].as_array().unwrap();
    assert_eq!(handoffs.len(), p.handoffs.len());
    let takeover = handoffs
        .iter()
        .find(|h| {
            h["from_session"] == sc.claude.readable() && h["to_session"] == sc.codex.readable()
        })
        .expect("codex took over the parser file");
    assert_eq!(takeover["computed_by"], "server");
    assert_eq!(takeover["shared_paths"], json!(["src/parser.rs"]));
    assert!(
        takeover["confidence"].as_f64().unwrap() >= 0.8,
        "{takeover}"
    );
    assert!(
        handoffs
            .iter()
            .all(|h| !h["evidence"].as_array().unwrap().is_empty())
    );
    assert_eq!(timeline["work_units_total"], p.work_units.len());
    assert_eq!(timeline["decisions_total"], p.decisions.len());
    assert!(
        !p.decisions.is_empty(),
        "a superseded failure is a decision"
    );

    // Work: every unit with its member attempts and blocker.
    let (status, work) = get(addr, "/v1/work", READER_ALPHA).await;
    assert_eq!(status, 200, "{work}");
    assert_eq!(work["total"], p.work_units.len());
    let units = work["work_units"].as_array().unwrap();
    assert_eq!(units.len(), p.work_units.len());
    let member_total: usize = units
        .iter()
        .map(|u| u["member_attempts"].as_array().unwrap().len())
        .sum();
    assert_eq!(
        member_total,
        p.work_units.iter().map(|w| w.attempts.len()).sum::<usize>()
    );
    for u in units {
        assert_eq!(u["computed_by"], "server");
        assert!(u["phase"].is_string() && u["status"].is_string());
        assert!(!u["evidence"].as_array().unwrap().is_empty());
        assert_eq!(
            u["attempts"].as_array().unwrap().len(),
            u["member_attempts"].as_array().unwrap().len()
        );
    }
    let blocked_unit = units
        .iter()
        .find(|u| u["phase"] == "blocked")
        .expect("the open session's unit is blocked");
    assert_eq!(blocked_unit["blocked"]["computed_by"], "server");
    assert_eq!(
        blocked_unit["blocked"]["evidence"],
        json!([format!("ev_{}", sc.permission_event)])
    );

    // Attention: the blocked open session, with the permission event as
    // evidence.
    let (status, attention) = get(addr, "/v1/attention", READER_ALPHA).await;
    assert_eq!(status, 200, "{attention}");
    assert_eq!(attention["open_sessions"], 1);
    assert_eq!(attention["total"], 1);
    let item = &attention["items"][0];
    assert_eq!(item["session_id"], sc.blocked.readable());
    assert_eq!(item["reason"], "pending_input");
    assert_eq!(item["signal_type"], "permission_request");
    assert_eq!(
        item["evidence"],
        json!([format!("ev_{}", sc.permission_event)])
    );
    assert_eq!(item["since"], at(620).to_rfc3339());
    assert_eq!(item["computed_by"], "server");
    let e = p.why_blocked(sc.blocked.session_id).unwrap();
    assert_eq!(item["claim"], e.claim);
    assert_eq!(item["confidence"].as_f64().unwrap() as f32, e.confidence);
    assert!(item["uncertainty"].is_string());
    assert_eq!(item["session"]["state"], "open");

    // State: now → only the open session; during the first turn → claude
    // in progress.
    let (status, now) = get(addr, "/v1/state", READER_ALPHA).await;
    assert_eq!(status, 200, "{now}");
    assert_eq!(now["total"], 1);
    assert_eq!(now["blocked"], 1);
    assert_eq!(now["sessions"][0]["session_id"], sc.blocked.readable());
    assert_eq!(now["sessions"][0]["computed_by"], "server");
    let then = at(15).to_rfc3339();
    let (status, state) = get(addr, &format!("/v1/state?at={then}"), READER_ALPHA).await;
    assert_eq!(status, 200, "{state}");
    assert_eq!(state["at"], then);
    assert_eq!(state["total"], 1);
    let s = &state["sessions"][0];
    assert_eq!(s["session_id"], sc.claude.readable());
    assert_eq!(s["open"], true);
    assert_eq!(s["turn_status"], "in_progress");
    assert_eq!(s["last_attempt_outcome"], "failed");
    assert_eq!(s["last_failure_class"], "string_mismatch");
    let (status, _) = get(addr, "/v1/state?at=whenever", READER_ALPHA).await;
    assert_eq!(status, 400);

    // Status: the tenant's counts.
    let (status, st) = get(addr, "/v1/status", READER_ALPHA).await;
    assert_eq!(status, 200, "{st}");
    eprintln!(
        "parity: events={} sessions={} turns={} attempts={} handoffs={} work_units={} decisions={} (server == local projection)",
        st["events"],
        st["sessions"],
        st["turns"],
        st["attempts"],
        st["handoffs"],
        st["work_units"],
        st["decisions"]
    );
    assert_eq!(st["turns"], p.turns.len());
    assert_eq!(st["handoffs"], p.handoffs.len());
    assert_eq!(st["decisions"], p.decisions.len());
    assert_eq!(st["events"], sc.events.len());
    assert_eq!(st["sessions"], 3);
    assert_eq!(st["attempts"], p.attempts.len());
    assert_eq!(st["work_units"], p.work_units.len());
    assert_eq!(st["capture_mode"], "metadata_only");
    assert_eq!(st["projects"][0]["repo_remote"], "github.com/acme/repo");
    assert_eq!(st["projects"][0]["sessions"], 3);
    assert_eq!(st["last_event_at"], at(620).to_rfc3339());
    r.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn events_stream_in_source_seq_order_and_lists_page_by_cursor() {
    let mut r = start_readers().await;
    let addr = r.addr;
    let sc = seed(&r).await;
    let stored = scan(&r.tenant_dir("alpha"));
    let expected: Vec<u64> = stored.iter().map(|e| e.source_seq).collect();
    assert!(expected.windows(2).all(|w| w[0] < w[1]));

    // Page through everything, five at a time.
    let mut after = 0u64;
    let mut seen: Vec<u64> = Vec::new();
    let mut pages = 0;
    loop {
        let (status, page) = get(
            addr,
            &format!("/v1/events?after={after}&limit=5"),
            READER_ALPHA,
        )
        .await;
        assert_eq!(status, 200, "{page}");
        pages += 1;
        let events = page["events"].as_array().unwrap();
        for e in events {
            let seq = e["source_seq"].as_u64().unwrap();
            assert!(seq > after);
            seen.push(seq);
            assert!(e["content"].is_null(), "stored under the ceiling");
            assert!(e["event_id"].is_string() && e["kind"].is_string());
        }
        after = page["next"].as_u64().unwrap();
        if page["has_more"] == false {
            break;
        }
        assert_eq!(events.len(), 5);
    }
    assert_eq!(seen, expected, "every event, in order, exactly once");
    assert_eq!(pages, sc.events.len().div_ceil(5));
    let (status, empty) = get(addr, &format!("/v1/events?after={after}"), READER_ALPHA).await;
    assert_eq!(status, 200, "{empty}");
    assert_eq!(empty["count"], 0);
    assert_eq!(empty["next"], after);
    assert_eq!(empty["has_more"], false);
    assert_eq!(empty["last_source_seq"], *expected.last().unwrap());
    let (status, _) = get(addr, "/v1/events?after=x", READER_ALPHA).await;
    assert_eq!(status, 400);
    let (status, windowed) = get(
        addr,
        &format!(
            "/v1/events?since={}&until={}",
            at(600).to_rfc3339(),
            at(620).to_rfc3339()
        ),
        READER_ALPHA,
    )
    .await;
    assert_eq!(status, 200, "{windowed}");
    assert_eq!(windowed["count"], 5, "the blocked session's events");

    // Sessions page by cursor: stable ids, no overlap, nothing missed.
    let (status, p1) = get(addr, "/v1/sessions?limit=2", READER_ALPHA).await;
    assert_eq!(status, 200, "{p1}");
    assert_eq!(p1["sessions"].as_array().unwrap().len(), 2);
    let cursor = p1["next_cursor"].as_str().expect("more").to_string();
    let (status, p2) = get(
        addr,
        &format!("/v1/sessions?limit=2&cursor={cursor}"),
        READER_ALPHA,
    )
    .await;
    assert_eq!(status, 200, "{p2}");
    assert_eq!(p2["sessions"].as_array().unwrap().len(), 1);
    assert!(p2["next_cursor"].is_null());
    let mut all = ids(&p1["sessions"], "session_id");
    let rest = ids(&p2["sessions"], "session_id");
    assert!(all.is_disjoint(&rest));
    all.extend(rest);
    assert_eq!(all.len(), 3);
    let (status, _) = get(addr, "/v1/sessions?cursor=garbage", READER_ALPHA).await;
    assert_eq!(status, 400);
    let (status, _) = get(addr, "/v1/sessions?limit=0", READER_ALPHA).await;
    assert_eq!(status, 400);
    let (status, big) = get(addr, "/v1/sessions?limit=99999", READER_ALPHA).await;
    assert_eq!(status, 200, "{big}");

    // Project filter: by normalised remote, by id, and an unknown one.
    let (status, by_remote) = get(
        addr,
        "/v1/sessions?project=github.com/acme/repo",
        READER_ALPHA,
    )
    .await;
    assert_eq!(status, 200, "{by_remote}");
    assert_eq!(by_remote["total"], 3);
    assert_eq!(
        by_remote["scope"]["project_id"],
        format!("prj_{}", sc.project.project_id)
    );
    let (status, by_id) = get(
        addr,
        &format!("/v1/timeline?project=prj_{}", sc.project.project_id),
        READER_ALPHA,
    )
    .await;
    assert_eq!(status, 200, "{by_id}");
    assert_eq!(by_id["total_sessions"], 3);
    let (status, body) = get(addr, "/v1/sessions?project=nope", READER_ALPHA).await;
    assert_eq!(status, 400, "{body}");
    assert!(body["error"].as_str().unwrap().contains("unknown project"));
    // Time window: the closed sessions only.
    let (status, early) = get(
        addr,
        &format!("/v1/sessions?until={}", at(400).to_rfc3339()),
        READER_ALPHA,
    )
    .await;
    assert_eq!(status, 200, "{early}");
    assert_eq!(early["total"], 2);
    let (status, _) = get(addr, "/v1/sessions?since=soon", READER_ALPHA).await;
    assert_eq!(status, 400);
    r.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn query_runs_attemptql_and_sql_and_stays_read_only() {
    let mut r = start_readers().await;
    let addr = r.addr;
    let sc = seed(&r).await;
    let q = |statement: &str| json!({ "statement": statement });

    let (status, body) = call(addr, "POST", "/v1/query", READER_ALPHA, q("SHOW SESSIONS")).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["kind"], "rows");
    assert_eq!(body["row_count"], 3);
    assert_eq!(body["rows"].as_array().unwrap().len(), 3);
    assert!(
        body["columns"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c == "session_id")
    );
    assert_eq!(body["truncated"], false);

    let (status, body) = call(
        addr,
        "POST",
        "/v1/query",
        READER_ALPHA,
        q("SELECT count(*) AS n FROM events"),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["rows"][0]["n"], sc.events.len());

    let (status, body) = call(
        addr,
        "POST",
        "/v1/query?limit=1",
        READER_ALPHA,
        q("SELECT event_id FROM events ORDER BY observed_at"),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["row_count"], sc.events.len());
    assert_eq!(body["rows"].as_array().unwrap().len(), 1);
    assert_eq!(body["truncated"], true);

    let (status, body) = call(
        addr,
        "POST",
        "/v1/query",
        READER_ALPHA,
        q(&format!(
            "WHY session '{}' STATUS BLOCKED",
            sc.blocked.readable()
        )),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(
        body["columns"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c == "evidence")
    );

    // Refused by the engine, not by a keyword check.
    let (status, body) = call(
        addr,
        "POST",
        "/v1/query",
        READER_ALPHA,
        q("CREATE EXTERNAL TABLE hosts STORED AS CSV LOCATION '/etc/hosts'"),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    assert!(body["error"].as_str().unwrap().contains("DDL"), "{body}");
    for stmt in [
        "INSERT INTO events VALUES (1)",
        "SET datafusion.execution.batch_size = 1",
        "COPY events TO '/tmp/x.csv'",
    ] {
        let (status, body) = call(addr, "POST", "/v1/query", READER_ALPHA, q(stmt)).await;
        assert_eq!(status, 400, "{stmt}: {body}");
    }
    let (status, body) = call(addr, "POST", "/v1/query", READER_ALPHA, q("SHOW FOO")).await;
    assert_eq!(status, 400, "{body}");
    assert!(
        body["error"].as_str().unwrap().contains('^'),
        "caret rendering"
    );
    let (status, _) = call(addr, "POST", "/v1/query", READER_ALPHA, q("  ;  ")).await;
    assert_eq!(status, 400);
    let (status, _) = call(addr, "POST", "/v1/query", READER_ALPHA, json!({"nope": 1})).await;
    assert!(status == 400 || status == 422, "{status}");
    r.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_cache_refreshes_after_ingest_and_decodes_only_what_is_new() {
    let mut r = start_readers().await;
    let addr = r.addr;
    let sc = seed(&r).await;
    let n = sc.events.len();

    // First read: everything is in the WAL, nothing to decode; every
    // session is projected for the first time.
    let (_, st) = get(addr, "/v1/status", READER_ALPHA).await;
    assert_eq!(st["cache"]["decodes"], 0);
    assert_eq!(st["cache"]["refreshes"], 1);
    assert_eq!(st["cache"]["projected_events"], n);
    assert_eq!(st["cache"]["sessions_reprojected"], 3);
    let built = st["cache"]["view_built_at"].clone();
    // A second read is served from the same view.
    let (_, again) = get(addr, "/v1/sessions", READER_ALPHA).await;
    assert_eq!(again["total"], 3);
    let (_, st) = get(addr, "/v1/status", READER_ALPHA).await;
    assert_eq!(st["cache"]["refreshes"], 1, "no refresh without a change");
    assert_eq!(st["cache"]["view_built_at"], built);

    // The WAL is flushed into a segment: one decode, and the events that
    // moved are not projected twice.
    r.state.tenants.flush_all();
    let (_, st) = get(addr, "/v1/status", READER_ALPHA).await;
    assert_eq!(st["cache"]["decodes"], 1);
    assert_eq!(st["cache"]["refreshes"], 2);
    assert_eq!(st["cache"]["segments"], 1);
    assert_eq!(st["cache"]["projected_events"], n);
    assert_eq!(st["cache"]["sessions_reprojected"], 0);
    assert_eq!(st["storage"]["memtable_rows"], 0);

    // A second upload (one new session) is visible on the next read; only
    // that session is re-projected and nothing is decoded.
    let d1 = device("d1");
    let mut b = common::Stream::new(d1, common::ROOT, Some(common::REMOTE));
    let late = common::Sess::claude("claude-session-3");
    b.session_started(&late, at(900));
    b.prompt(&late, at(905), "Add a changelog entry");
    b.tool_start(
        &late,
        at(910),
        &common::Tool::edit(Some("l1"), &["CHANGELOG.md"]),
    );
    b.tool_finish(
        &late,
        at(911),
        &common::Tool::edit(Some("l1"), &["CHANGELOG.md"]),
    );
    b.stop(&late, at(920));
    let more = b.build();
    let (status, ack) = post(addr, Some(KEY_ALPHA), batch(d1, "more", &more)).await;
    assert_eq!(status, 200, "{ack}");
    let (status, sessions) = get(addr, "/v1/sessions", READER_ALPHA).await;
    assert_eq!(status, 200, "{sessions}");
    assert_eq!(sessions["total"], 4);
    assert_eq!(sessions["sessions"][0]["session_id"], late.readable());
    let (_, st) = get(addr, "/v1/status", READER_ALPHA).await;
    assert_eq!(st["events"], n + more.len());
    assert_eq!(st["cache"]["decodes"], 1, "WAL only: nothing decoded");
    assert_eq!(st["cache"]["refreshes"], 3);
    assert_eq!(st["cache"]["projected_events"], n + more.len());
    assert_eq!(st["cache"]["sessions_reprojected"], 1);
    let (_, events) = get(addr, &format!("/v1/events?after={n}"), READER_ALPHA).await;
    assert_eq!(
        events["count"],
        more.len(),
        "the stream continues past the flush"
    );
    r.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn device_inferences_replace_server_items_only_when_current_or_newer() {
    let mut r = start_readers().await;
    let addr = r.addr;
    let sc = seed(&r).await;
    let d1 = device("d1");

    let (_, timeline) = get(addr, "/v1/timeline", READER_ALPHA).await;
    let attempts = attempts_in(&timeline);
    let target = attempts
        .iter()
        .find(|a| a["outcome"] == "superseded")
        .unwrap();
    let target_id = target["attempt_id"].as_str().unwrap().to_string();
    let bare_id = target_id.trim_start_matches("att_").to_string();
    let evidence = target["evidence"].clone();
    let n_attempts = attempts.len();
    let find = |list: &[Value]| -> Value {
        list.iter()
            .find(|a| a["attempt_id"] == target_id)
            .cloned()
            .expect("the target attempt is listed")
    };

    let upload = |version: &str, approach: &str| {
        inference_batch(
            d1,
            "attempt",
            json!([{
                "kind": "attempt",
                "id": bare_id,
                "session_id": sc.claude.session_id.to_string(),
                "evidence": evidence.as_array().unwrap().iter().map(|e| e.as_str().unwrap().trim_start_matches("ev_")).collect::<Vec<_>>(),
                "confidence": 0.42,
                "algorithm_version": version,
                "fields": {
                    "attempt_id": bare_id,
                    "session_id": sc.claude.session_id.to_string(),
                    "started_at": at(10).as_micros(),
                    "ended_at": at(11).as_micros(),
                    "outcome": "failed",
                    "approach": approach,
                    "objective": "the prompt, stripped by the ceiling",
                    "new_field": "unknown to this server",
                },
            }]),
        )
    };

    // Newer version: the device item is returned, whole.
    let (status, ack) = call(
        addr,
        "POST",
        "/v1/sync/inferences",
        KEY_ALPHA,
        upload("tier1-v1", "device: patch the grammar"),
    )
    .await;
    assert_eq!(status, 200, "{ack}");
    assert_eq!(ack["stored"], 1);
    for route in ["/v1/timeline", "/v1/work"] {
        let (status, body) = get(addr, route, READER_ALPHA).await;
        assert_eq!(status, 200, "{route}: {body}");
        let list = if route == "/v1/timeline" {
            attempts_in(&body)
        } else {
            body["work_units"]
                .as_array()
                .unwrap()
                .iter()
                .flat_map(|u| u["member_attempts"].as_array().unwrap().iter().cloned())
                .collect()
        };
        assert_eq!(
            list.len(),
            n_attempts,
            "{route}: one object per attempt, still"
        );
        let item = find(&list);
        assert_eq!(item["computed_by"], "device", "{route}: {item}");
        assert_eq!(item["algorithm_version"], "tier1-v1");
        assert_eq!(item["approach"], "device: patch the grammar");
        assert_eq!(
            item["outcome"], "failed",
            "the device's value, not the server's `superseded`"
        );
        assert_eq!(item["confidence"], 0.42);
        assert_eq!(
            item["evidence"], evidence,
            "evidence ids re-encoded, not replaced"
        );
        assert_eq!(item["device_id"], format!("dev_{d1}"));
        assert_eq!(item["started_at"], at(10).to_rfc3339());
        assert_eq!(item["session_id"], sc.claude.readable());
        assert_eq!(item["new_field"], "unknown to this server");
        assert!(item["objective"].is_null(), "content stripped at upload");
        assert!(
            item.get("duration_ms").is_none(),
            "no server field on a device item"
        );
        assert!(item.get("superseded_by").is_none());
        let others: Vec<&Value> = list
            .iter()
            .filter(|a| a["attempt_id"] != target_id)
            .collect();
        assert!(others.iter().all(|a| a["computed_by"] == "server"));
    }
    let (_, st) = get(addr, "/v1/status", READER_ALPHA).await;
    assert_eq!(st["device_inferences"]["documents"], 1);
    assert_eq!(st["device_inferences"]["items"], 1);

    // Same version as the server's: the device item still wins.
    let (status, _) = call(
        addr,
        "POST",
        "/v1/sync/inferences",
        KEY_ALPHA,
        upload(ALGORITHM_VERSION, "device: same version"),
    )
    .await;
    assert_eq!(status, 200);
    let (_, timeline) = get(addr, "/v1/timeline", READER_ALPHA).await;
    let item = find(&attempts_in(&timeline));
    assert_eq!(item["computed_by"], "device");
    assert_eq!(item["approach"], "device: same version");

    // A version that does not compare (another family, or unparseable):
    // the server's item, with none of the device's fields.
    for version in ["tier0-v9", "v2", "experimental"] {
        let (status, _) = call(
            addr,
            "POST",
            "/v1/sync/inferences",
            KEY_ALPHA,
            upload(version, "device: should not show"),
        )
        .await;
        assert_eq!(status, 200);
        let (_, timeline) = get(addr, "/v1/timeline", READER_ALPHA).await;
        let item = find(&attempts_in(&timeline));
        assert_eq!(item["computed_by"], "server", "{version}: {item}");
        assert_eq!(item["algorithm_version"], ALGORITHM_VERSION);
        assert_eq!(item["outcome"], "superseded");
        assert_ne!(item["approach"], "device: should not show");
        assert!(item.get("new_field").is_none());
        assert!(item.get("device_id").is_none());
    }
    // Wholesale replacement with an empty document: server items again.
    let (status, _) = call(
        addr,
        "POST",
        "/v1/sync/inferences",
        KEY_ALPHA,
        inference_batch(d1, "attempt", json!([])),
    )
    .await;
    assert_eq!(status, 200);
    let (_, timeline) = get(addr, "/v1/timeline", READER_ALPHA).await;
    assert!(
        attempts_in(&timeline)
            .iter()
            .all(|a| a["computed_by"] == "server")
    );
    r.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reader_of_one_tenant_never_sees_another() {
    let mut r = start_with(StartOptions {
        max_open: 1,
        keys: {
            let mut k = device_keys();
            k.extend(reader_keys());
            k
        },
        ..Default::default()
    })
    .await;
    let addr = r.addr;
    seed(&r).await;
    let d2 = device("d2");
    let (status, _) = post(
        addr,
        Some(KEY_BETA),
        batch(d2, "b", &common::events(d2, 2, "beta")),
    )
    .await;
    assert_eq!(status, 200);

    let (status, beta) = get(addr, "/v1/sessions", READER_BETA).await;
    assert_eq!(status, 200, "{beta}");
    assert_eq!(beta["tenant"], "beta");
    assert_eq!(beta["total"], 1);
    assert_eq!(beta["sessions"][0]["device_id"], format!("dev_{d2}"));
    let (_, beta_events) = get(addr, "/v1/events", READER_BETA).await;
    assert_eq!(beta_events["count"], 2);
    assert!(
        beta_events["events"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["device_id"] == d2.to_string())
    );
    let (_, beta_attention) = get(addr, "/v1/attention", READER_BETA).await;
    assert_eq!(beta_attention["total"], 0);
    let (_, beta_query) = call(
        addr,
        "POST",
        "/v1/query",
        READER_BETA,
        json!({"statement": "SELECT count(*) AS n FROM events"}),
    )
    .await;
    assert_eq!(beta_query["rows"][0]["n"], 2);

    // Alpha, through the LRU with one slot, still answers for alpha only.
    let (status, alpha) = get(addr, "/v1/sessions", READER_ALPHA).await;
    assert_eq!(status, 200, "{alpha}");
    assert_eq!(alpha["tenant"], "alpha");
    assert_eq!(alpha["total"], 3);
    let (_, health) =
        tokio::task::spawn_blocking(move || common::http(addr, "GET", "/v1/health", &[], ""))
            .await
            .unwrap();
    assert_eq!(health["open_tenants"], 1, "the read path respects the LRU");
    // Evicted tenants come back with a fresh cache and the same answers.
    let (_, beta) = get(addr, "/v1/status", READER_BETA).await;
    assert_eq!(beta["events"], 2);
    assert_eq!(beta["cache"]["refreshes"], 1, "a new cache after eviction");
    r.stop().await;
    let _ = Timestamp::now();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn devices_lists_key_bindings_and_last_sync_per_device() {
    let mut r = start_readers().await;
    let addr = r.addr;
    let d1 = common::device("d1");
    // A device key on the write route; a reader on the devices route.
    let (status, body) = common::get(addr, "/v1/devices", common::KEY_ALPHA).await;
    assert_eq!(status, 403, "{body}");
    let (status, body) = common::get(addr, "/v1/devices", common::READER_ALPHA).await;
    assert_eq!(status, 200, "{body}");
    let listed = body["devices"].as_array().unwrap();
    let mine = listed
        .iter()
        .find(|d| d["device_id"] == serde_json::to_value(d1).unwrap())
        .expect("the key-table binding lists the device before any upload");
    assert_eq!(mine["connected"], true);
    assert_eq!(mine["events"], 0);
    assert!(mine["last_sync_at"].is_null());
    assert_eq!(mine["keys"][0]["scope"], "device");
    assert!(
        !listed.iter().any(|d| d["keys"]
            .as_array()
            .is_some_and(|k| k.iter().any(|e| e["scope"] == "reader"))
            && d["events"] != 0),
        "reader keys carry no events"
    );

    let before = attemptdb_core::Timestamp::now();
    let evs = common::events(d1, 4, "one");
    let (status, ack) =
        common::post(addr, Some(common::KEY_ALPHA), common::batch(d1, "b1", &evs)).await;
    assert_eq!(status, 200, "{ack}");
    let (status, body) = common::get(addr, "/v1/devices", common::READER_ALPHA).await;
    assert_eq!(status, 200, "{body}");
    let mine = body["devices"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["device_id"] == serde_json::to_value(d1).unwrap())
        .unwrap();
    assert_eq!(mine["events"], 4);
    assert_eq!(mine["sessions"], 1);
    assert_eq!(mine["providers"], serde_json::json!(["claude_code"]));
    let last_sync = mine["last_sync_at"]
        .as_str()
        .expect("last sync is a timestamp");
    // RFC 3339 in UTC sorts lexicographically; compare to the second.
    assert!(
        last_sync[..19] >= before.to_rfc3339()[..19],
        "last_sync_at ({last_sync}) is server receipt time, not before {}",
        before.to_rfc3339()
    );
    assert_eq!(
        body["devices"][0]["device_id"],
        serde_json::to_value(d1).unwrap(),
        "newest upload first"
    );
    r.stop().await;
}
