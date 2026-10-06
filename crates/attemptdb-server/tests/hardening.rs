//! What the sync server refuses, what it keeps when things go wrong, and what
//! it forgets when told to: write scopes on every upload route, a device's
//! reach over other devices' sessions, pairing that cannot take a device over,
//! key-file integrity under concurrency, the limiter's bounds, and deletion.

mod common;

use attemptdb_core::event::{EventContent, Provider};
use attemptdb_core::{
    AttemptId, CaptureMode, DeviceId, Event, EventKind, ProjectRef, SessionId, TurnId,
};
use attemptdb_server::auth::digest_hex;
use attemptdb_server::{ServerConfig, UPLOAD_ROUTES};
use common::{
    ADMIN, ADMIN_ALPHA, KEY_ALPHA, READER_ALPHA, StartOptions, admin, batch, call, device, events,
    http, inference_batch, post, reader_keys, restart_config, scan, start_with, write_keys,
};
use serde_json::{Value, json};

const LEGACY_ENVELOPE: &str = r#"{
    "v": 2, "agent": "claude_code", "event": "bash", "session_id": "sess-legacy-1",
    "cwd": "/home/dev/proj", "project_root": "example/project",
    "timestamp": "2026-08-30T09:00:00Z",
    "payload": {"tool_name": "Bash", "session_id": "sess-legacy-1"},
    "signals": {"bash.category": "git.commit", "bash.byte_len": 40}
}"#;

/// A request body for each upload route, valid for `dev`.
fn body_for(route: &str, dev: DeviceId) -> Value {
    match route {
        "/v1/sync" => batch(dev, "b-route", &events(dev, 1, "route")),
        "/v1/sync/inferences" => inference_batch(dev, "attempt", json!([])),
        "/v1/vibemon/hook" => serde_json::from_str(LEGACY_ENVELOPE).unwrap(),
        "/v1/sync/forget" => json!({ "confirm": true }),
        "/v1/sync/revoke" => Value::Null,
        other => panic!("UPLOAD_ROUTES names {other}, which this test has no body for"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_upload_route_refuses_reader_and_admin_keys_and_serves_a_device_key() {
    assert!(
        UPLOAD_ROUTES.contains(&"/v1/vibemon/hook"),
        "the legacy route"
    );
    let dev = device("d1");
    for route in UPLOAD_ROUTES {
        let mut r = start_with(StartOptions {
            keys: [common::device_keys(), reader_keys()].concat(),
            ..Default::default()
        })
        .await;
        let addr = r.addr;
        // No key: 401. A reader or admin key: 403, whatever the body.
        let (status, body) = call(addr, "POST", route, "no-such-key", body_for(route, dev)).await;
        assert_eq!(status, 401, "{route}: {body}");
        for (who, key) in [("reader", READER_ALPHA), ("admin", ADMIN_ALPHA)] {
            let (status, body) = call(addr, "POST", route, key, body_for(route, dev)).await;
            assert_eq!(status, 403, "{route} with a {who} key: {body}");
        }
        // Nothing the refused calls carried reached the tenant.
        assert!(
            !r.tenant_dir("alpha").exists() || scan(&r.tenant_dir("alpha")).is_empty(),
            "{route}: a refused key wrote something"
        );
        // The device key is served.
        let (status, body) = call(addr, "POST", route, KEY_ALPHA, body_for(route, dev)).await;
        assert_eq!(status, 200, "{route} with the device key: {body}");
        r.stop().await;
    }
}

/// Every POST route in the router is either an upload route (and so listed in
/// `UPLOAD_ROUTES`) or is named here as one that writes nothing a device key
/// owns. A new POST route fails this test until someone decides which.
#[test]
fn every_post_route_is_classified() {
    const NOT_UPLOADS: &[&str] = &[
        "/v1/pair",
        "/v1/corrections",
        "/v1/query",
        "/v1/admin/pairings",
        "/v1/admin/tenants/{tenant}/purge-telemetry",
        "/v1/admin/keys",
        "/v1/admin/keys/reload",
    ];
    let src = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/lib.rs")).unwrap();
    let router = &src[src.find("fn router(").expect("router fn")..];
    let router = &router[..router.find("async fn health").unwrap_or(router.len())];
    let mut posts = Vec::new();
    for segment in router.split(".route(").skip(1) {
        let end = segment
            .find("\n        .")
            .or_else(|| segment.find("\n    }"))
            .unwrap_or(segment.len());
        let seg = &segment[..end];
        let path = seg.split('"').nth(1).expect("a route path");
        let after_path = &seg[seg.find('"').unwrap() + 1..];
        let after_path = &after_path[after_path.find('"').unwrap() + 1..];
        if after_path.contains("post(") {
            posts.push(path.to_string());
        }
    }
    assert!(
        posts.len() >= 10,
        "the route parser found only {posts:?}; it needs updating"
    );
    for path in &posts {
        assert!(
            UPLOAD_ROUTES.contains(&path.as_str()) || NOT_UPLOADS.contains(&path.as_str()),
            "POST {path} is neither in UPLOAD_ROUTES nor in the list of POSTs that write nothing \
             a device owns: decide which, and add the scope check"
        );
    }
    for route in UPLOAD_ROUTES {
        assert!(
            posts.iter().any(|p| p == route),
            "UPLOAD_ROUTES lists {route}, which is not a POST route"
        );
    }
}

// ---------------------------------------------------------------------------
// A device may retract and correct only its own events
// ---------------------------------------------------------------------------

fn key_for(name: &str, dev: DeviceId) -> (String, Value) {
    let key = format!("k-{name}-0123456789");
    (
        key.clone(),
        json!({ "sha256": digest_hex(&key), "tenant": "alpha", "device_id": dev, "label": name }),
    )
}

fn meta_event(
    dev: DeviceId,
    sess: SessionId,
    kind: EventKind,
    target_type: Option<&str>,
    target: String,
) -> Event {
    let mut ev = Event::new(
        dev,
        Provider::Other("attemptdb".into()),
        if kind == EventKind::Retraction {
            "Retraction"
        } else {
            "Correction"
        },
        kind,
        ProjectRef::derive("/home/dev/example/project", None, &dev),
        "ignored",
        CaptureMode::MetadataOnly,
        "test",
    );
    ev.session_id = sess;
    if let Some(t) = target_type {
        ev.attrs.insert("target_type".into(), json!(t));
    } else {
        ev.attrs
            .insert("correction_type".into(), json!("attempt_outcome"));
        ev.attrs.insert("outcome".into(), json!("failed"));
    }
    ev.attrs.insert("target".into(), json!(target));
    if kind == EventKind::Retraction {
        ev.attrs.insert("reason".into(), json!("mistake"));
    }
    ev
}

fn session_of(dev: DeviceId, tag: &str) -> SessionId {
    // `events()` builds `session-<tag>` under the claude_code provider.
    let _ = dev;
    SessionId::derive(&["claude_code", &format!("session-{tag}")])
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_device_cannot_retract_or_correct_what_another_device_wrote() {
    let (d1, d2) = (device("m-victim"), device("m-attacker"));
    let (k1, e1) = key_for("victim", d1);
    let (k2, e2) = key_for("attacker", d2);
    let mut r = start_with(StartOptions {
        keys: vec![e1, e2],
        ..Default::default()
    })
    .await;
    let addr = r.addr;
    // Each device uploads one session, with the same tag so both are ordinary.
    let (s, ack) = post(addr, Some(&k1), batch_of(d1, "victim")).await;
    assert_eq!(s, 200, "{ack}");
    let (s, ack) = post(addr, Some(&k2), batch_of(d2, "attacker")).await;
    assert_eq!(s, 200, "{ack}");
    let victim = session_of(d1, "victim");
    let mine = session_of(d2, "attacker");
    let victim_events: Vec<Event> = scan(&r.tenant_dir("alpha"))
        .into_iter()
        .filter(|e| e.device_id == d1)
        .collect();
    let victim_event = victim_events[0].event_id;

    let upload = |ev: Event| {
        let k2 = k2.clone();
        async move {
            let (s, ack) = post(addr, Some(&k2), batch(d2, "meta", &[ev])).await;
            assert_eq!(s, 200, "{ack}");
            ack
        }
    };

    // The attacker's own device tries each way of reaching the victim's data.
    let attacks = [
        // 1. retract the victim's session
        meta_event(
            d2,
            victim,
            EventKind::Retraction,
            Some("session"),
            format!("ses_{victim}"),
        ),
        // 2. retract it while claiming its own session
        meta_event(
            d2,
            mine,
            EventKind::Retraction,
            Some("session"),
            format!("ses_{victim}"),
        ),
        // 3. retract one of the victim's events
        meta_event(
            d2,
            mine,
            EventKind::Retraction,
            Some("event"),
            format!("ev_{victim_event}"),
        ),
        // 4. retract the victim's attempt, with the victim's session
        meta_event(
            d2,
            victim,
            EventKind::Retraction,
            Some("attempt"),
            format!(
                "att_{}",
                AttemptId::derive(&[&victim.to_string(), "1", "0"])
            ),
        ),
        // 5. … or with its own session, so the event looks like its own
        meta_event(
            d2,
            mine,
            EventKind::Retraction,
            Some("attempt"),
            format!(
                "att_{}",
                AttemptId::derive(&[&victim.to_string(), "1", "0"])
            ),
        ),
        // 6. correct the victim's attempt outcome
        meta_event(
            d2,
            mine,
            EventKind::Correction,
            None,
            format!(
                "att_{}",
                AttemptId::derive(&[&victim.to_string(), "1", "0"])
            ),
        ),
    ];
    for (i, ev) in attacks.into_iter().enumerate() {
        let id = ev.event_id;
        let ack = upload(ev).await;
        assert_eq!(ack["accepted"], 0, "attack {}: {ack}", i + 1);
        assert_eq!(
            ack["rejected"][0]["event_id"],
            json!(id),
            "attack {}",
            i + 1
        );
        assert!(
            ack["rejected"][0]["reason"]
                .as_str()
                .unwrap()
                .contains("own events"),
            "attack {}: {ack}",
            i + 1
        );
    }
    assert!(
        scan(&r.tenant_dir("alpha"))
            .iter()
            .all(|e| !matches!(e.kind, EventKind::Retraction | EventKind::Correction)),
        "nothing the attacker sent was stored"
    );

    // The same moves on the attacker's own data are allowed.
    for ev in [
        meta_event(
            d2,
            mine,
            EventKind::Retraction,
            Some("session"),
            format!("ses_{mine}"),
        ),
        meta_event(
            d2,
            mine,
            EventKind::Correction,
            None,
            format!("att_{}", AttemptId::derive(&[&mine.to_string(), "1", "0"])),
        ),
        meta_event(
            d2,
            mine,
            EventKind::Retraction,
            Some("attempt"),
            format!("att_{}", AttemptId::derive(&[&mine.to_string(), "2", "1"])),
        ),
    ] {
        let ack = upload(ev).await;
        assert_eq!(ack["accepted"], 1, "{ack}");
    }
    let own_event = scan(&r.tenant_dir("alpha"))
        .into_iter()
        .find(|e| {
            e.device_id == d2 && !matches!(e.kind, EventKind::Retraction | EventKind::Correction)
        })
        .unwrap()
        .event_id;
    let ack = upload(meta_event(
        d2,
        mine,
        EventKind::Retraction,
        Some("event"),
        format!("ev_{own_event}"),
    ))
    .await;
    assert_eq!(ack["accepted"], 1, "{ack}");

    // A turn target is checked the same way.
    let turn_other = TurnId::derive(&[&victim.to_string(), "1"]);
    let mut turn = meta_event(
        d2,
        mine,
        EventKind::Correction,
        None,
        format!("trn_{turn_other}"),
    );
    turn.attrs
        .insert("correction_type".into(), json!("turn_objective"));
    let ack = upload(turn).await;
    assert_eq!(ack["accepted"], 0, "{ack}");

    // A session's facts and its retraction in one batch: the facts are stored
    // first, so a first sync that carries both is not refused.
    let d3 = device("m-batch");
    let (k3, e3) = key_for("batch", d3);
    r.state
        .add_key(serde_json::from_value(e3).unwrap())
        .unwrap();
    let mut evs = events(d3, 2, "fresh");
    let fresh = session_of(d3, "fresh");
    evs.push(meta_event(
        d3,
        fresh,
        EventKind::Retraction,
        Some("session"),
        format!("ses_{fresh}"),
    ));
    let (s, ack) = post(addr, Some(&k3), batch(d3, "one-go", &evs)).await;
    assert_eq!(s, 200, "{ack}");
    assert_eq!(ack["accepted"], 3, "{ack}");
    r.stop().await;
}

fn batch_of(dev: DeviceId, tag: &str) -> Value {
    // Two prompts' worth of ordinary tool events in one session.
    let mut evs = events(dev, 3, tag);
    let mut prompt = Event::new(
        dev,
        Provider::ClaudeCode,
        "UserPromptSubmit",
        EventKind::PromptSubmitted,
        ProjectRef::derive("/home/dev/example/project", None, &dev),
        format!("session-{tag}"),
        CaptureMode::LocalSemantic,
        "server-test/0.1",
    );
    prompt.content = Some(EventContent {
        prompt: Some("do the thing".into()),
        ..Default::default()
    });
    evs.push(prompt);
    batch(dev, &format!("b-{tag}"), &evs)
}

// ---------------------------------------------------------------------------
// Pairing
// ---------------------------------------------------------------------------

async fn mint(addr: std::net::SocketAddr, tenant: &str, user: Option<&str>) -> String {
    let body = match user {
        Some(u) => json!({ "tenant": tenant, "user_id": u }),
        None => json!({ "tenant": tenant }),
    };
    let (status, out) = admin(addr, "POST", "/v1/admin/pairings".into(), Some(ADMIN), body).await;
    assert_eq!(status, 201, "{out}");
    out["token"].as_str().unwrap().to_string()
}

async fn exchange(addr: std::net::SocketAddr, token: &str, dev: DeviceId) -> (u16, Value) {
    let body = json!({ "token": token, "device_id": dev }).to_string();
    // Each device pairs from its own address (the trusted proxy header), so
    // the per-address pairing limit is not what these tests measure.
    let ip = dev.to_string();
    tokio::task::spawn_blocking(move || {
        http(addr, "POST", "/v1/pair", &[("Fly-Client-IP", &ip)], &body)
    })
    .await
    .unwrap()
}

async fn admin_running() -> common::Running {
    common::start_admin().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_token_for_one_user_cannot_take_over_another_users_device() {
    let mut r = admin_running().await;
    let addr = r.addr;
    let victim = device("victim-laptop");
    let t1 = mint(addr, "acme", Some("usr_victim")).await;
    let (s, body) = exchange(addr, &t1, victim).await;
    assert_eq!(s, 201, "{body}");
    let victim_key = body["key"].as_str().unwrap().to_string();

    // Another user's token, naming the victim's device id.
    let t2 = mint(addr, "acme", Some("usr_mallory")).await;
    let (s, body) = exchange(addr, &t2, victim).await;
    assert_eq!(s, 409, "{body}");
    assert!(
        !body["error"].as_str().unwrap().contains("usr_victim"),
        "the refusal names no one: {body}"
    );
    // The victim's key still works, and the token was not burned by the refusal.
    let (s, ack) = post(addr, Some(&victim_key), batch(victim, "v", &[])).await;
    assert_eq!(s, 200, "the victim's key survives: {ack}");
    let t2c = t2.clone();
    let (s, valid) =
        tokio::task::spawn_blocking(move || http(addr, "GET", &format!("/v1/pair/{t2c}"), &[], ""))
            .await
            .unwrap();
    assert_eq!(
        s, 200,
        "the token is still good for a device of its own: {valid}"
    );
    // … and it pairs mallory's own device.
    let own = device("mallory-laptop");
    let (s, body) = exchange(addr, &t2, own).await;
    assert_eq!(s, 201, "{body}");

    // The same user pairs the same machine again: the earlier key is retired.
    let t3 = mint(addr, "acme", Some("usr_victim")).await;
    let (s, body) = exchange(addr, &t3, victim).await;
    assert_eq!(s, 201, "{body}");
    let (s, _) = post(addr, Some(&victim_key), batch(victim, "v2", &[])).await;
    assert_eq!(s, 401, "re-pairing retired the earlier key");

    // The server's own writer id, the nil id, and a reader key's device are
    // not bindable.
    let writer = attemptdb_server::tenants::writer_device_id(
        &attemptdb_server::tenants::TenantId::parse("acme").unwrap(),
    );
    let t4 = mint(addr, "acme", None).await;
    for (what, dev) in [("writer", writer), ("nil", DeviceId::nil())] {
        let (s, body) = exchange(addr, &t4, dev).await;
        assert_eq!(s, 409, "{what}: {body}");
    }
    r.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_exchanges_keep_every_key() {
    let mut r = admin_running().await;
    let addr = r.addr;
    let n = 16;
    let mut tokens = Vec::new();
    for i in 0..n {
        tokens.push((
            mint(addr, "acme", Some(&format!("usr_{i}"))).await,
            device(&format!("c-{i}")),
        ));
    }
    let handles: Vec<_> = tokens
        .into_iter()
        .map(|(t, d)| tokio::spawn(async move { (exchange(addr, &t, d).await, d) }))
        .collect();
    let mut keys = Vec::new();
    for h in handles {
        let ((s, body), d) = h.await.unwrap();
        assert_eq!(s, 201, "{body}");
        keys.push((body["key"].as_str().unwrap().to_string(), d));
    }
    // Every issued key is in the table the server serves …
    for (k, d) in &keys {
        let (s, ack) = post(addr, Some(k), batch(*d, "c", &[])).await;
        assert_eq!(s, 200, "a concurrently issued key was lost: {ack}");
    }
    // … and in the file a restart reads, which is whole and holds them all
    // (the two keys every test starts with, and the sixteen new ones).
    let on_disk: Value = serde_json::from_slice(&std::fs::read(&r.keys_file).unwrap())
        .expect("the key file is valid JSON after concurrent writes");
    assert_eq!(on_disk["keys"].as_array().unwrap().len(), 2 + n);
    let leftovers: Vec<_> = std::fs::read_dir(r.keys_file.parent().unwrap())
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".tmp"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
    r.stop().await;
    let mut again = restart_config(ServerConfig {
        data_dir: r.data_dir.clone(),
        keys_file: r.keys_file.clone(),
        ..Default::default()
    })
    .await;
    for (k, d) in &keys {
        let (s, ack) = post(again.addr, Some(k), batch(*d, "after", &[])).await;
        assert_eq!(s, 200, "{ack}");
    }
    again.stop().await;
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_key_file_that_cannot_be_written_does_not_burn_the_token() {
    use std::os::unix::fs::PermissionsExt;
    let mut r = admin_running().await;
    let addr = r.addr;
    let token = mint(addr, "acme", Some("usr_a")).await;
    let dir = r.keys_file.parent().unwrap().to_path_buf();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();
    // A directory the process can still write into (running as root) cannot
    // demonstrate this; skip rather than pass vacuously.
    let probe = dir.join("probe");
    if std::fs::write(&probe, b"x").is_ok() {
        let _ = std::fs::remove_file(&probe);
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        r.stop().await;
        eprintln!("skipped: the directory is writable despite mode 0555");
        return;
    }
    let dev = device("unwritable");
    let (s, body) = exchange(addr, &token, dev).await;
    assert_eq!(s, 503, "{body}");
    let t = token.clone();
    let (s, valid) =
        tokio::task::spawn_blocking(move || http(addr, "GET", &format!("/v1/pair/{t}"), &[], ""))
            .await
            .unwrap();
    assert_eq!(s, 200, "the token survived a failed exchange: {valid}");
    // Fixed, the same token works.
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    let (s, body) = exchange(addr, &token, dev).await;
    assert_eq!(s, 201, "{body}");
    r.stop().await;
}

#[tokio::test]
async fn a_corrupt_key_file_stops_the_start_with_a_message_that_says_what_to_do() {
    let tmp = tempfile::tempdir().unwrap();
    let keys = tmp.path().join("keys.json");
    std::fs::write(&keys, "{\"keys\": [ {\"sha256\": ").unwrap(); // torn
    let err = attemptdb_server::Server::bind(ServerConfig {
        port: 0,
        data_dir: tmp.path().join("data"),
        keys_file: keys.clone(),
        ..Default::default()
    })
    .await
    .err()
    .expect("a torn key file must not start a server");
    let text = format!("{err:#}");
    assert!(text.contains("keys.json"), "{text}");
    assert!(text.contains("will not start"), "{text}");
}

// ---------------------------------------------------------------------------
// Admin token, limiter
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_short_admin_token_is_refused_unless_a_test_asks_for_it() {
    let tmp = tempfile::tempdir().unwrap();
    let keys = write_keys(tmp.path(), &[]);
    let config = |token: &str, allow: bool| ServerConfig {
        port: 0,
        data_dir: tmp.path().join("data"),
        keys_file: keys.clone(),
        admin_token: Some(token.to_string()),
        allow_short_admin_token: allow,
        ..Default::default()
    };
    let err = attemptdb_server::Server::bind(config("short-token", false))
        .await
        .err()
        .expect("refused");
    let text = format!("{err:#}");
    assert!(
        text.contains("at least 24") && text.contains("11 characters"),
        "{text}"
    );
    attemptdb_server::Server::bind(config("short-token", true))
        .await
        .expect("the test escape hatch");
    attemptdb_server::Server::bind(config(&"a".repeat(24), false))
        .await
        .expect("24 characters is enough");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn guessing_bearer_strings_allocates_no_buckets_and_forged_proxy_headers_buy_nothing() {
    let mut r = admin_running().await;
    let addr = r.addr;
    let before = r.state.limiter.len();
    // 300 requests, each with a bearer string the server never issued.
    for i in 0..300 {
        let (s, _) = call(
            addr,
            "GET",
            "/v1/status",
            &format!("guess-{i}"),
            Value::Null,
        )
        .await;
        assert!(s == 401 || s == 429, "{s}");
    }
    assert!(
        r.state.limiter.len() <= before + 1,
        "{} buckets after 300 distinct guesses (was {before})",
        r.state.limiter.len()
    );
    // Pairing attempts are limited per address; the address is the trusted
    // header or the socket, never a header the client may write. Twelve
    // requests naming twelve different X-Forwarded-For values share one
    // bucket (the socket's), so the burst of 10 runs out.
    let mut statuses = Vec::new();
    for i in 0..12 {
        let xff = format!("203.0.113.{i}");
        let (s, _) = tokio::task::spawn_blocking(move || {
            http(
                addr,
                "GET",
                &format!("/v1/pair/pair_{}", "2".repeat(64)),
                &[("X-Forwarded-For", &xff), ("X-Real-IP", &xff)],
                "",
            )
        })
        .await
        .unwrap();
        statuses.push(s);
    }
    assert_eq!(
        statuses.iter().filter(|s| **s == 429).count(),
        2,
        "{statuses:?}: forged headers must not give each request its own bucket"
    );
    r.stop().await;
}

// ---------------------------------------------------------------------------
// Forgetting and revoking
// ---------------------------------------------------------------------------

fn canary_events(dev: DeviceId, tag: &str, canary: &str, n: usize) -> Vec<Event> {
    events(dev, n, tag)
        .into_iter()
        .map(|mut e| {
            e.kind = EventKind::PromptSubmitted;
            e.content = Some(EventContent {
                prompt: Some(canary.to_string()),
                ..Default::default()
            });
            e
        })
        .collect()
}

/// Every byte under a tenant's directory that contains `needle`.
fn files_containing(dir: &std::path::Path, needle: &str) -> Vec<String> {
    let mut hits = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for entry in rd.flatten() {
            let p = entry.path();
            if p.is_dir() {
                stack.push(p);
            } else if let Ok(bytes) = std::fs::read(&p)
                && bytes.windows(needle.len()).any(|w| w == needle.as_bytes())
            {
                hits.push(p.display().to_string());
            }
        }
    }
    hits
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_device_can_have_its_events_deleted_from_the_disk_and_revoke_its_key() {
    let (d1, d2) = (device("f-one"), device("f-two"));
    let (k1, e1) = key_for("one", d1);
    let (k2, e2) = key_for("two", d2);
    let tmp = tempfile::tempdir().unwrap();
    let mut keys = vec![e1, e2];
    keys.extend(reader_keys());
    let keys_file = write_keys(tmp.path(), &keys);
    let mut r = restart_config(ServerConfig {
        data_dir: tmp.path().join("data"),
        keys_file,
        capture_mode: CaptureMode::LocalSemantic,
        admin_token: Some(ADMIN.into()),
        ..Default::default()
    })
    .await;
    let addr = r.addr;
    let c1 = "CANARY_DEVICE_ONE_7c1a";
    let c2 = "CANARY_DEVICE_TWO_9e3b";
    let (s, ack) = post(
        addr,
        Some(&k1),
        batch(d1, "one", &canary_events(d1, "one", c1, 5)),
    )
    .await;
    assert_eq!((s, ack["accepted"].as_u64()), (200, Some(5)), "{ack}");
    let (s, ack) = post(
        addr,
        Some(&k2),
        batch(d2, "two", &canary_events(d2, "two", c2, 3)),
    )
    .await;
    assert_eq!((s, ack["accepted"].as_u64()), (200, Some(3)), "{ack}");
    // Inferences from device one, to be forgotten with its events.
    let (s, _) = call(
        addr,
        "POST",
        "/v1/sync/inferences",
        &k1,
        inference_batch(d1, "attempt", json!([])),
    )
    .await;
    assert_eq!(s, 200);
    let tenant = r.tenant_dir("alpha");
    // Spread over WAL and a flushed segment: the deletion must reach both.
    r.state.tenants.flush_all();
    let (s, _) = post(
        addr,
        Some(&k1),
        batch(d1, "one-b", &canary_events(d1, "one-b", c1, 2)),
    )
    .await;
    assert_eq!(s, 200);

    // A reader key and a missing confirmation cannot delete anything.
    let (s, _) = call(
        addr,
        "POST",
        "/v1/sync/forget",
        READER_ALPHA,
        json!({"confirm": true}),
    )
    .await;
    assert_eq!(s, 403);
    let (s, body) = call(addr, "POST", "/v1/sync/forget", &k1, json!({})).await;
    assert_eq!(s, 400, "{body}");
    assert_eq!(scan_len(&tenant), 10);

    let (s, out) = call(
        addr,
        "POST",
        "/v1/sync/forget",
        &k1,
        json!({"confirm": true}),
    )
    .await;
    assert_eq!(s, 200, "{out}");
    assert_eq!(out["forgotten"], true);
    assert_eq!(out["outcome"]["events_deleted"], 7, "{out}");
    assert!(
        out["not_reached"].to_string().contains("webhook"),
        "the response says what a deletion does not reach: {out}"
    );

    // Device two's events are untouched; device one's are gone from every
    // file in the tenant's directory (segments, WAL, tombstones), and a
    // deletion record says how many, and nothing else.
    r.state.tenants.flush_all();
    assert!(
        files_containing(&tenant, c1).is_empty(),
        "deleted content survives on disk: {:?}",
        files_containing(&tenant, c1)
    );
    assert!(
        !files_containing(&tenant, c2).is_empty(),
        "device two keeps its data"
    );
    let stored = scan(&tenant);
    assert_eq!(stored.iter().filter(|e| e.device_id == d1).count(), 0);
    assert_eq!(stored.iter().filter(|e| e.device_id == d2).count(), 3);
    let record = stored
        .iter()
        .find(|e| e.kind == EventKind::ConfigChanged)
        .expect("a deletion record");
    assert_eq!(record.attrs["x_attemptdb_events_deleted"], 7);
    assert_eq!(record.attrs["x_attemptdb_deletion_reason"], "device");
    assert!(record.content.is_none() && record.raw.is_none());
    assert!(!serde_json::to_string(record).unwrap().contains(c1));
    assert!(
        !tenant.join("inferences").join(d1.to_string()).exists(),
        "the device's inference documents went with its events"
    );

    // Forgetting again finds nothing; the key still works.
    let (s, out) = call(
        addr,
        "POST",
        "/v1/sync/forget",
        &k1,
        json!({"confirm": true}),
    )
    .await;
    assert_eq!(s, 200);
    assert_eq!(out["outcome"]["events_deleted"], 0);
    let (s, _) = post(
        addr,
        Some(&k1),
        batch(d1, "after", &canary_events(d1, "after", "fresh", 1)),
    )
    .await;
    assert_eq!(s, 200, "forgetting does not revoke");

    // The operator's version, for device two.
    let (s, _) = admin(
        addr,
        "DELETE",
        format!("/v1/admin/devices/{d2}/events"),
        None,
        Value::Null,
    )
    .await;
    assert_eq!(s, 401);
    let (s, out) = admin(
        addr,
        "DELETE",
        format!("/v1/admin/devices/{d2}/events"),
        Some(ADMIN),
        Value::Null,
    )
    .await;
    assert_eq!(s, 200, "{out}");
    assert_eq!(out["events_deleted"], 3, "{out}");
    r.state.tenants.flush_all();
    assert!(files_containing(&tenant, c2).is_empty());

    // Revoking: the presenting key dies, the other devices' keys do not.
    let (s, out) = call(addr, "POST", "/v1/sync/revoke", &k1, Value::Null).await;
    assert_eq!(s, 200, "{out}");
    assert_eq!(out["revoked"], true);
    let (s, _) = post(addr, Some(&k1), batch(d1, "dead", &[])).await;
    assert_eq!(s, 401, "a revoked key is dead");
    let (s, _) = post(addr, Some(&k2), batch(d2, "alive", &[])).await;
    assert_eq!(s, 200);
    let on_disk = std::fs::read_to_string(&r.keys_file).unwrap();
    assert!(
        !on_disk.contains(&digest_hex(&k1)),
        "the digest left the key file"
    );
    assert!(on_disk.contains(&digest_hex(&k2)));
    r.stop().await;
}

fn scan_len(dir: &std::path::Path) -> usize {
    scan(dir).len()
}

#[allow(dead_code)]
fn unused(_: &Event) {}
