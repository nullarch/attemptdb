//! What the agent console needs beyond the read API: people on sessions
//! and work units, an event by id, and corrections written from the web.

mod common;

use attemptdb_server::auth::digest_hex;
use common::{
    ADMIN_ALPHA, KEY_ALPHA, READER_ALPHA, READER_BETA, Running, StartOptions, batch, call, device,
    get, post, reader_keys, scenario, start_with,
};
use serde_json::{Value, json};

/// Device keys carrying user ids, so sessions get a person.
async fn start() -> Running {
    let mut keys = vec![
        json!({ "sha256": digest_hex(KEY_ALPHA), "tenant": "alpha", "device_id": device("d1"), "label": "kevin laptop", "user_id": "usr_kevin" }),
    ];
    keys.extend(reader_keys());
    start_with(StartOptions {
        keys,
        ..Default::default()
    })
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sessions_and_work_units_carry_the_person_behind_the_device() {
    let mut r = start().await;
    let addr = r.addr;
    let d1 = device("d1");
    let sc = scenario(d1);
    let (status, ack) = post(addr, Some(KEY_ALPHA), batch(d1, "seed", &sc.events)).await;
    assert_eq!(status, 200, "{ack}");

    let (status, body) = get(addr, "/v1/sessions", READER_ALPHA).await;
    assert_eq!(status, 200, "{body}");
    let sessions = body["sessions"].as_array().unwrap();
    assert!(!sessions.is_empty());
    for s in sessions {
        assert_eq!(s["device_id"], json!(format!("dev_{d1}")));
        assert_eq!(s["user_id"], "usr_kevin", "{s}");
    }
    let (status, body) = get(addr, "/v1/work", READER_ALPHA).await;
    assert_eq!(status, 200, "{body}");
    for w in body["work_units"].as_array().unwrap() {
        assert_eq!(w["users"], json!(["usr_kevin"]), "{w}");
        assert!(
            w["devices"]
                .as_array()
                .unwrap()
                .contains(&json!(format!("dev_{d1}")))
        );
    }
    let (_, body) = get(addr, "/v1/live?window=9999999999", READER_ALPHA).await;
    assert!(
        body["active_sessions"]
            .as_array()
            .unwrap()
            .iter()
            .all(|s| s["user_id"] == "usr_kevin"),
        "{body}"
    );
    r.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_event_is_fetched_by_id_and_holds_no_content() {
    let mut r = start().await;
    let addr = r.addr;
    let d1 = device("d1");
    let sc = scenario(d1);
    let (status, _) = post(addr, Some(KEY_ALPHA), batch(d1, "seed", &sc.events)).await;
    assert_eq!(status, 200);
    let ev = &sc.events[3];
    let path = format!("/v1/events/ev_{}", ev.event_id);
    let (status, body) = get(addr, &path, READER_ALPHA).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["event"]["event_id"], json!(ev.event_id.to_string()));
    assert_eq!(body["event"]["kind"], json!(ev.kind.as_str()));
    assert!(
        body["event"]["content"].is_null(),
        "metadata_only ceiling: {body}"
    );
    // The same id without its prefix, and the failure modes.
    let (status, _) = get(addr, &format!("/v1/events/{}", ev.event_id), READER_ALPHA).await;
    assert_eq!(status, 200);
    let (status, _) = get(addr, "/v1/events/ev_not-an-id", READER_ALPHA).await;
    assert_eq!(status, 400);
    let (status, _) = get(
        addr,
        "/v1/events/ev_00000000-0000-0000-0000-000000000000",
        READER_ALPHA,
    )
    .await;
    assert_eq!(status, 404);
    let (status, _) = get(addr, &path, KEY_ALPHA).await;
    assert_eq!(status, 403, "a device key does not read");
    let (status, _) = get(addr, &path, READER_BETA).await;
    assert_eq!(status, 404, "another tenant does not see it");
    r.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn corrections_from_the_web_change_the_projection_and_credit_the_user() {
    let mut r = start().await;
    let addr = r.addr;
    let d1 = device("d1");
    let sc = scenario(d1);
    let (status, _) = post(addr, Some(KEY_ALPHA), batch(d1, "seed", &sc.events)).await;
    assert_eq!(status, 200);

    // A failed attempt from the story.
    let (_, tl) = get(addr, "/v1/timeline", READER_ALPHA).await;
    let mut failed: Option<Value> = None;
    for s in tl["sessions"].as_array().unwrap() {
        for t in s["turns"].as_array().unwrap() {
            for a in t["attempts"].as_array().unwrap() {
                if a["outcome"] == "failed" || a["outcome"] == "superseded" {
                    failed = Some(a.clone());
                }
            }
        }
    }
    let failed = failed.expect("the scenario has a failed attempt");
    let att = failed["attempt_id"].as_str().unwrap().to_string();

    // "This is not a problem": the outcome becomes succeeded.
    let (status, body) = call(addr, "POST", "/v1/corrections", READER_ALPHA, json!({ "target": att, "type": "attempt_outcome", "outcome": "succeeded", "note": "flaky CI, passed on rerun" }))
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["kind"], "correction");
    assert_eq!(body["accepted"], 1);
    let ev_id = body["event_id"].as_str().unwrap().to_string();

    let (_, ev) = get(addr, &format!("/v1/events/{ev_id}"), READER_ALPHA).await;
    assert_eq!(ev["event"]["kind"], "correction", "{ev}");
    assert_eq!(
        ev["event"]["attrs"]["x_attemptdb_corrected_by"], "usr_alpha",
        "{ev}"
    );
    assert_eq!(ev["event"]["attrs"]["outcome"], "succeeded");
    assert_eq!(ev["event"]["attrs"]["note_chars"], 25);
    assert!(
        ev["event"]["content"].is_null(),
        "the note's text stays out under metadata_only"
    );

    let (_, tl) = get(addr, "/v1/timeline", READER_ALPHA).await;
    let mut corrected: Option<Value> = None;
    for s in tl["sessions"].as_array().unwrap() {
        for t in s["turns"].as_array().unwrap() {
            for a in t["attempts"].as_array().unwrap() {
                if a["attempt_id"] == json!(att) {
                    corrected = Some(a.clone());
                }
            }
        }
    }
    let corrected = corrected.unwrap();
    assert_eq!(corrected["outcome"], "succeeded", "{corrected}");
    assert!(
        corrected["correction"].is_object()
            || corrected["corrected"].is_object()
            || corrected["correction_event_id"].is_string(),
        "the row says it was corrected: {corrected}"
    );

    // Bad requests.
    let (status, _) = call(
        addr,
        "POST",
        "/v1/corrections",
        READER_ALPHA,
        json!({ "target": att, "type": "attempt_outcome", "outcome": "great" }),
    )
    .await;
    assert_eq!(status, 400);
    let (status, _) = call(
        addr,
        "POST",
        "/v1/corrections",
        READER_ALPHA,
        json!({ "target": att, "type": "nonsense" }),
    )
    .await;
    assert_eq!(status, 400);
    let (status, _) = call(
        addr,
        "POST",
        "/v1/corrections",
        KEY_ALPHA,
        json!({ "target": att, "type": "attempt_note", "note": "x" }),
    )
    .await;
    assert_eq!(status, 403, "a device key is not a person at the console");

    // Retract a session from the web: it leaves the session list.
    let sid = failed["session_id"].as_str().unwrap().to_string();
    let (status, body) = call(
        addr,
        "POST",
        "/v1/corrections",
        ADMIN_ALPHA,
        json!({ "target": sid, "type": "retract_session", "reason": "benchmark" }),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["kind"], "retraction");
    let (_, sessions) = get(addr, "/v1/sessions", READER_ALPHA).await;
    assert!(
        !sessions["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["session_id"] == json!(sid)),
        "retracted session still listed: {sessions}"
    );
    r.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn work_units_carry_the_newest_countable_signal_or_null() {
    let mut r = start().await;
    let addr = r.addr;
    let d1 = device("d1");
    let sc = scenario(d1);
    // Stamp a test run on one shell finish of the story: what the adapter
    // writes when the output carried a runner summary.
    let mut events = sc.events.clone();
    let mut stamped_session = None;
    for e in events.iter_mut() {
        if e.kind == attemptdb_core::EventKind::ToolCallFinished && stamped_session.is_none() {
            e.attrs.insert("tests_passed".into(), json!(18));
            e.attrs.insert("tests_failed".into(), json!(2));
            e.attrs.insert("tests_skipped".into(), json!(0));
            stamped_session = Some(e.session_id);
        }
    }
    let stamped_session = stamped_session.expect("a tool finish in the story");
    let (status, ack) = post(addr, Some(KEY_ALPHA), batch(d1, "seed", &events)).await;
    assert_eq!(status, 200, "{ack}");

    let (status, body) = get(addr, "/v1/work", READER_ALPHA).await;
    assert_eq!(status, 200, "{body}");
    let units = body["work_units"].as_array().unwrap();
    assert!(!units.is_empty());
    let mut counted = 0;
    for w in units {
        let sig = &w["signal"];
        assert!(sig.is_object(), "every unit has a signal object: {w}");
        let in_session = w["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s == &json!(format!("ses_{stamped_session}")));
        if in_session && sig["tests"].is_object() {
            assert_eq!(sig["tests"]["passed"], 18);
            assert_eq!(sig["tests"]["failed"], 2);
            assert_eq!(sig["tests"]["total"], 20);
            counted += 1;
        } else {
            assert!(sig["tests"].is_null(), "no made-up numbers: {w}");
        }
        assert!(sig["build"].is_null(), "the story runs no build: {w}");
    }
    assert!(
        counted >= 1,
        "the stamped session's unit shows 18/20: {body}"
    );
    r.stop().await;
}
