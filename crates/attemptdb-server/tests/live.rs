//! `/v1/live`: the newest event and the active sessions, from facts kept
//! next to the writer — answered without the tenant's engine once seeded,
//! seeded from the stored facts after a restart, and consistent with
//! `/v1/sessions`.

mod common;

use attemptdb_core::{EventKind, Timestamp};
use common::{
    KEY_ALPHA, READER_ALPHA, READER_BETA, Running, StartOptions, batch, device, device_keys,
    events, get, post, reader_keys, start_with,
};
use serde_json::Value;

async fn start() -> Running {
    let mut keys = device_keys();
    keys.extend(reader_keys());
    start_with(StartOptions {
        keys,
        max_open: 1,
        ..Default::default()
    })
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_follows_ingest_and_needs_a_reader_key() {
    let mut r = start().await;
    let addr = r.addr;
    let d1 = device("d1");

    // Empty tenant: no last event, nothing active.
    let (status, body) = get(addr, "/v1/live", READER_ALPHA).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["tenant"], "alpha");
    assert!(body["last_event"].is_null(), "{body}");
    assert_eq!(body["active_sessions"].as_array().unwrap().len(), 0);
    let (status, _) = get(addr, "/v1/live", KEY_ALPHA).await;
    assert_eq!(status, 403, "a device key cannot read");

    // Two sessions, the second newer by an hour.
    let mut older = events(d1, 3, "old");
    for (i, e) in older.iter_mut().enumerate() {
        e.observed_at = Timestamp::from_micros(1_000_000 * (i as i64 + 1));
    }
    let mut newer = events(d1, 2, "new");
    newer[0].observed_at = Timestamp::from_micros(3_600_000_000 + 1_000_000);
    newer[1].observed_at = Timestamp::from_micros(3_600_000_000 + 2_000_000);
    newer[1].kind = EventKind::PromptSubmitted;
    let mut all = older.clone();
    all.extend(newer.clone());
    let (status, ack) = post(addr, Some(KEY_ALPHA), batch(d1, "b1", &all)).await;
    assert_eq!(status, 200, "{ack}");
    assert_eq!(ack["accepted"], 5);

    let (status, body) = get(addr, "/v1/live?window=3600", READER_ALPHA).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["events"], 5);
    assert_eq!(body["last_event"]["kind"], "prompt_submitted", "{body}");
    assert_eq!(
        body["last_event"]["session_id"],
        Value::String(format!("ses_{}", newer[1].session_id))
    );
    // The window is measured from the server's clock; nothing synthetic
    // from 1970 is within an hour of now.
    assert_eq!(body["active_sessions"].as_array().unwrap().len(), 0);
    // A window wide enough to reach 1970 lists both, newest first.
    let (status, body) = get(addr, "/v1/live?window=9999999999", READER_ALPHA).await;
    assert_eq!(status, 200, "{body}");
    let active = body["active_sessions"].as_array().unwrap();
    assert_eq!(active.len(), 2, "{body}");
    assert_eq!(active[0]["last_kind"], "prompt_submitted");
    assert!(active[0]["idle_ms"].as_i64().unwrap() > 0);

    // Another tenant sees nothing of it.
    let (status, body) = get(addr, "/v1/live", READER_BETA).await;
    assert_eq!(status, 200, "{body}");
    assert!(body["last_event"].is_null());

    // A re-sent old event is a duplicate for the engine and moves nothing.
    let (status, ack) = post(addr, Some(KEY_ALPHA), batch(d1, "b2", &older[..1])).await;
    assert_eq!(status, 200, "{ack}");
    assert_eq!(ack["duplicates"], 1);
    let (_, body) = get(addr, "/v1/live", READER_ALPHA).await;
    assert_eq!(body["events"], 5);
    assert_eq!(body["last_event"]["kind"], "prompt_submitted");

    // Bad window.
    let (status, _) = get(addr, "/v1/live?window=-1", READER_ALPHA).await;
    assert_eq!(status, 400);
    r.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_is_seeded_from_stored_facts_after_a_restart() {
    // Ingest with one server, then read with a fresh process over the same
    // data: the live map is empty, so the first read seeds it from facts
    // and agrees with /v1/sessions.
    let mut r = start().await;
    let addr = r.addr;
    let d1 = device("d1");
    let mut evs = events(d1, 4, "s");
    for (i, e) in evs.iter_mut().enumerate() {
        e.observed_at = Timestamp::from_micros(1_000_000 * (i as i64 + 1));
    }
    evs[3].kind = EventKind::ToolCallFailed;
    let (status, ack) = post(addr, Some(KEY_ALPHA), batch(d1, "b1", &evs)).await;
    assert_eq!(status, 200, "{ack}");
    let data_dir = r.data_dir.clone();
    let keys = r.keys_file.clone();
    r.stop().await;
    let _keep = r._tmp.take();
    // The old process is gone with its state: the writer lock is released.
    drop(r);

    let mut r2 = common::restart(data_dir, keys, 1).await;
    let addr = r2.addr;
    let (status, body) = get(addr, "/v1/live?window=9999999999", READER_ALPHA).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["events"], 4);
    assert_eq!(body["last_event"]["kind"], "tool_call_failed", "{body}");
    assert_eq!(body["active_sessions"].as_array().unwrap().len(), 1);
    let (_, sessions) = get(addr, "/v1/sessions", READER_ALPHA).await;
    let s = &sessions["sessions"].as_array().unwrap()[0];
    assert_eq!(
        s["session_id"], body["active_sessions"][0]["session_id"],
        "the live session is the projected one"
    );
    // And an ingest after the seed keeps counting.
    let mut more = events(d1, 1, "s2");
    more[0].observed_at = Timestamp::from_micros(10_000_000);
    let (status, _) = post(addr, Some(KEY_ALPHA), batch(d1, "b2", &more)).await;
    assert_eq!(status, 200);
    let (_, body) = get(addr, "/v1/live?window=9999999999", READER_ALPHA).await;
    assert_eq!(body["events"], 5);
    assert_eq!(body["active_sessions"].as_array().unwrap().len(), 2);
    r2.stop().await;
}
