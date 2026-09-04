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

fn tempdir_has_tenant(r: &Running, tenant: &str) -> bool {
    r.data_dir.join("tenants").join(tenant).exists()
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_devices_editing_one_file_at_once_are_a_conflict_with_people_on_each_side() {
    use common::{Sess, Stream, Tool, at};
    // Kevin's laptop and Sarah's laptop, both in tenant alpha.
    const KEY_SARAH: &str = "k-alpha-d3";
    let mut keys = vec![
        json!({ "sha256": digest_hex(KEY_ALPHA), "tenant": "alpha", "device_id": device("d1"), "label": "kevin", "user_id": "usr_kevin" }),
        json!({ "sha256": digest_hex(KEY_SARAH), "tenant": "alpha", "device_id": device("d3"), "label": "sarah", "user_id": "usr_sarah" }),
    ];
    keys.extend(reader_keys());
    let mut r = start_with(StartOptions {
        keys,
        ..Default::default()
    })
    .await;
    let addr = r.addr;
    let (d1, d3) = (device("d1"), device("d3"));

    let kevin = Sess::new(attemptdb_core::event::Provider::ClaudeCode, "kevin-claude");
    let mut a = Stream::new(
        d1,
        "/home/dev/example/project",
        Some("github.com/example/project"),
    );
    a.session_started(&kevin, at(0));
    a.prompt(&kevin, at(1), "tidy the auth middleware");
    a.tool_start(
        &kevin,
        at(10),
        &Tool::edit(Some("a1"), &["src/middleware/auth.ts"]),
    );
    a.tool_finish(
        &kevin,
        at(11),
        &Tool::edit(Some("a1"), &["src/middleware/auth.ts"]),
    );
    a.tool_start(
        &kevin,
        at(100),
        &Tool::edit(Some("a2"), &["src/middleware/auth.ts"]),
    );
    a.tool_finish(
        &kevin,
        at(101),
        &Tool::edit(Some("a2"), &["src/middleware/auth.ts"]),
    );
    let (status, ack) = post(addr, Some(KEY_ALPHA), batch(d1, "kevin", &a.build())).await;
    assert_eq!(status, 200, "{ack}");

    let sarah = Sess::new(attemptdb_core::event::Provider::Codex, "sarah-codex");
    let mut b = Stream::new(
        d3,
        "/home/dev/example/project",
        Some("github.com/example/project"),
    );
    b.session_started(&sarah, at(40));
    b.prompt(&sarah, at(41), "fix session expiry handling");
    b.tool_start(
        &sarah,
        at(50),
        &Tool::edit(Some("b1"), &["src/middleware/auth.ts"]),
    );
    b.tool_finish(
        &sarah,
        at(51),
        &Tool::edit(Some("b1"), &["src/middleware/auth.ts"]),
    );
    b.tool_start(
        &sarah,
        at(120),
        &Tool::edit(Some("b2"), &["src/middleware/session.ts"]),
    );
    b.tool_finish(
        &sarah,
        at(121),
        &Tool::edit(Some("b2"), &["src/middleware/session.ts"]),
    );
    let (status, ack) = post(addr, Some(KEY_SARAH), batch(d3, "sarah", &b.build())).await;
    assert_eq!(status, 200, "{ack}");

    let (status, body) = get(addr, "/v1/work", READER_ALPHA).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body["work_units"].as_array().unwrap().len(),
        2,
        "concurrent sessions are two units: {body}"
    );
    let conflicts = body["conflicts"].as_array().unwrap();
    assert_eq!(conflicts.len(), 1, "{body}");
    let c = &conflicts[0];
    assert_eq!(c["kind"], "conflict");
    assert_eq!(c["algorithm_version"], "conflict-v0");
    assert_eq!(c["first"]["users"], json!(["usr_kevin"]), "{c}");
    assert_eq!(c["second"]["users"], json!(["usr_sarah"]), "{c}");
    assert_eq!(c["paths"].as_array().unwrap().len(), 1);
    assert!(
        c["paths"][0]["path"]
            .as_str()
            .unwrap()
            .ends_with("src/middleware/auth.ts")
    );
    assert_eq!(c["paths"][0]["overlapping"], true);
    assert!(c["confidence"].as_f64().unwrap() > 0.6);
    assert!(!c["evidence"].as_array().unwrap().is_empty());

    // Needs You lists it as its third kind.
    let (status, body) = get(addr, "/v1/attention", READER_ALPHA).await;
    assert_eq!(status, 200, "{body}");
    let item = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["reason"] == "work_conflict")
        .expect("a work_conflict item");
    assert!(
        item["claim"]
            .as_str()
            .unwrap()
            .contains("neither committed"),
        "{item}"
    );
    assert_eq!(item["first"]["users"], json!(["usr_kevin"]));
    r.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_view_window_keeps_old_segments_out_of_memory_but_not_out_of_the_backfill() {
    use attemptdb_core::Timestamp;
    use attemptdb_storage::{Database, OpenOptions};
    // A year-old segment and a fresh one, written straight into the tenant
    // directory before the server starts (the server flushes on its own
    // schedule; this test needs the segments to exist).
    let mut r = start_with(StartOptions {
        keys: {
            let mut k = vec![json!({ "sha256": digest_hex(KEY_ALPHA), "tenant": "alpha", "device_id": device("d1"), "label": "d1" })];
            k.extend(reader_keys());
            k
        },
        view_window_days: Some(7),
        ..Default::default()
    })
    .await;
    let data_dir = r.data_dir.clone();
    let keys = r.keys_file.clone();
    r.stop().await;
    let _keep = r._tmp.take();
    drop(r);

    let d1 = device("d1");
    let dir = data_dir.join("tenants").join("alpha");
    std::fs::create_dir_all(&dir).unwrap();
    let server_device = attemptdb_core::DeviceId::derive(&["attemptdb-server", "alpha"]);
    {
        let mut db = Database::open(
            &dir,
            OpenOptions {
                create: true,
                device_id: Some(server_device),
                ..Default::default()
            },
        )
        .unwrap();
        let now = Timestamp::now().as_micros();
        let year = 365i64 * 24 * 60 * 60 * 1_000_000;
        let mut old = common::events(d1, 4, "old");
        for (i, e) in old.iter_mut().enumerate() {
            e.observed_at = Timestamp::from_micros(now - year + i as i64);
        }
        db.ingest(old).unwrap();
        db.flush().unwrap();
        let mut fresh = common::events(d1, 3, "fresh");
        for (i, e) in fresh.iter_mut().enumerate() {
            e.observed_at = Timestamp::from_micros(now - 60_000_000 + i as i64);
        }
        db.ingest(fresh).unwrap();
        db.flush().unwrap();
        db.close().unwrap();
    }

    let mut r2 = common::restart_with(data_dir, keys, 1, Some(7)).await;
    let addr = r2.addr;
    let (status, body) = get(addr, "/v1/status", READER_ALPHA).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body["events"], 3,
        "only the fresh segment is resident: {body}"
    );
    assert_eq!(body["view_window"]["days"], 7);
    assert_eq!(
        body["cache"]["decodes"], 1,
        "the old segment was never decoded: {body}"
    );
    let (_, sessions) = get(addr, "/v1/sessions", READER_ALPHA).await;
    assert_eq!(sessions["sessions"].as_array().unwrap().len(), 1);

    // The backfill by sequence still sees everything.
    let (status, body) = get(addr, "/v1/events?after=0&limit=100", READER_ALPHA).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["count"], 7, "{body}");
    r2.stop().await;
}

/// The product's backend reads a tenant with the admin token plus
/// `X-AttemptDB-Tenant`, without a reader key provisioned per tenant. The
/// admin token alone is not a read credential, and a device key with the
/// header still cannot read.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_operator_reads_a_tenant_by_header() {
    let mut keys = vec![
        json!({ "sha256": digest_hex(KEY_ALPHA), "tenant": "alpha", "device_id": device("d1"), "label": "kevin laptop", "user_id": "usr_kevin" }),
    ];
    keys.extend(reader_keys());
    let mut r = start_with(StartOptions {
        keys,
        admin_token: Some("op-secret".into()),
        ..Default::default()
    })
    .await;
    let addr = r.addr;
    let d1 = device("d1");
    let sc = scenario(d1);
    let (status, ack) = post(addr, Some(KEY_ALPHA), batch(d1, "seed", &sc.events)).await;
    assert_eq!(status, 200, "{ack}");

    let read = |bearer: &'static str, tenant: Option<&'static str>| {
        let mut headers = vec![("Authorization", format!("Bearer {bearer}"))];
        if let Some(t) = tenant {
            headers.push(("X-AttemptDB-Tenant", t.to_string()));
        }
        tokio::task::spawn_blocking(move || {
            let h: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
            common::http(addr, "GET", "/v1/devices", &h, "")
        })
    };

    let (status, body) = read("op-secret", Some("alpha")).await.unwrap();
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["tenant"], "alpha");
    let kevin = body["devices"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["keys"][0]["user_id"] == "usr_kevin")
        .expect("the paired device is listed");
    assert_eq!(kevin["connected"], true);
    assert!(kevin["last_sync_at"].is_string());

    let (status, _) = read("op-secret", None).await.unwrap();
    assert_eq!(status, 401, "the admin token alone reads nothing");
    let (status, body) = read("op-secret", Some("never-seen")).await.unwrap();
    assert_eq!(
        status, 404,
        "an operator read does not create a tenant: {body}"
    );
    assert!(!tempdir_has_tenant(&r, "never-seen"));
    let (status, _) = read("op-secret", Some(".hidden")).await.unwrap();
    assert_eq!(status, 400, "a tenant id is validated");
    let (status, _) = read("wrong-secret", Some("alpha")).await.unwrap();
    assert_eq!(status, 401);
    let (status, _) = read(KEY_ALPHA, Some("alpha")).await.unwrap();
    assert_eq!(status, 403, "a device key does not read, header or not");
    r.stop().await;
}
