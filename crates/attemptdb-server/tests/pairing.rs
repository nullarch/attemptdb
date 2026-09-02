//! Pairing end to end: the web mints a token, the installer checks it,
//! exchanges it with the local device id, and the key it gets works for
//! that device only; the token dies on use; the public routes are rate
//! limited per address.

mod common;

use attemptdb_core::DeviceId;
use common::{ADMIN, admin, batch, device, events, http, post, start_admin};
use serde_json::{Value, json};

async fn public(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    body: Value,
    ip: &str,
) -> (u16, Value) {
    let (method, path, ip) = (method.to_string(), path.to_string(), ip.to_string());
    let body = if body.is_null() {
        String::new()
    } else {
        body.to_string()
    };
    tokio::task::spawn_blocking(move || {
        http(
            addr,
            &method,
            &path,
            &[("Content-Type", "application/json"), ("Fly-Client-IP", &ip)],
            &body,
        )
    })
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_token_becomes_a_key_bound_to_the_device_and_dies() {
    let mut r = start_admin().await;
    let addr = r.addr;

    // Minting needs the admin token; the token comes back once.
    let (status, body) = admin(
        addr,
        "POST",
        "/v1/admin/pairings".into(),
        None,
        json!({ "tenant": "acme", "user_id": "usr_kevin", "label": "kevin laptop" }),
    )
    .await;
    assert_eq!(status, 401, "{body}");
    let (status, body) = admin(addr, "POST", "/v1/admin/pairings".into(), Some(ADMIN),
        json!({ "tenant": "acme", "user_id": "usr_kevin", "label": "kevin laptop", "ttl_secs": 600 })).await;
    assert_eq!(status, 201, "{body}");
    let token = body["token"].as_str().unwrap().to_string();
    assert!(token.starts_with("pair_") && token.len() == 69, "{token}");
    assert_eq!(body["tenant"], "acme");
    let digest = body["sha256"].as_str().unwrap().to_string();
    let file = std::fs::read_to_string(r.data_dir.join("pairings.json")).unwrap();
    assert!(
        file.contains(&digest) && !file.contains(&token),
        "digest only on disk"
    );
    let (_, listed) = admin(
        addr,
        "GET",
        "/v1/admin/pairings".into(),
        Some(ADMIN),
        Value::Null,
    )
    .await;
    assert_eq!(listed["pairings"].as_array().unwrap().len(), 1);

    // The installer checks before touching anything.
    let (status, body) = public(
        addr,
        "GET",
        &format!("/v1/pair/{token}"),
        Value::Null,
        "10.0.0.1",
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["valid"], true);
    assert_eq!(body["tenant"], "acme");
    let (status, _) = public(addr, "GET", "/v1/pair/pair_0000", Value::Null, "10.0.0.1").await;
    assert_eq!(status, 400, "malformed");
    let (status, _) = public(
        addr,
        "GET",
        &format!("/v1/pair/pair_{}", "0".repeat(64)),
        Value::Null,
        "10.0.0.1",
    )
    .await;
    assert_eq!(status, 404, "unknown");

    // Exchange with the local device id.
    let dev = device("laptop");
    let (status, body) = public(
        addr,
        "POST",
        "/v1/pair",
        json!({ "token": token, "device_id": dev }),
        "10.0.0.1",
    )
    .await;
    assert_eq!(status, 201, "{body}");
    let key = body["key"].as_str().unwrap().to_string();
    assert!(key.starts_with("atk_"));
    assert_eq!(body["device_id"], json!(dev));
    assert_eq!(body["tenant"], "acme");
    assert_eq!(body["user_id"], "usr_kevin");
    assert_eq!(body["label"], "kevin laptop");

    // Spent: check and exchange both say so; the list no longer shows it.
    let (status, body) = public(
        addr,
        "GET",
        &format!("/v1/pair/{token}"),
        Value::Null,
        "10.0.0.1",
    )
    .await;
    assert_eq!(status, 410, "{body}");
    let (status, _) = public(
        addr,
        "POST",
        "/v1/pair",
        json!({ "token": token, "device_id": dev }),
        "10.0.0.1",
    )
    .await;
    assert_eq!(status, 410);
    let (_, listed) = admin(
        addr,
        "GET",
        "/v1/admin/pairings".into(),
        Some(ADMIN),
        Value::Null,
    )
    .await;
    assert_eq!(listed["pairings"].as_array().unwrap().len(), 0);

    // The key works for this device — an empty batch is the handshake —
    // and for no other.
    let (status, ack) = post(addr, Some(&key), batch(dev, "hello", &[])).await;
    assert_eq!(status, 200, "{ack}");
    assert_eq!(ack["accepted"], 0);
    // The handshake stored nothing, but the device was here: the operator's
    // devices list says so before any event arrives ("Connected" on a web
    // page within seconds of pairing).
    let (status, devices) = tokio::task::spawn_blocking(move || {
        common::http(
            addr,
            "GET",
            "/v1/devices",
            &[
                ("Authorization", &format!("Bearer {ADMIN}")),
                ("X-AttemptDB-Tenant", "acme"),
            ],
            "",
        )
    })
    .await
    .unwrap();
    assert_eq!(status, 200, "{devices}");
    let row = devices["devices"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["device_id"] == json!(dev))
        .expect("the paired device is listed");
    assert_eq!(row["connected"], true);
    assert!(row["last_seen_at"].is_string(), "{row}");
    assert!(row["last_sync_at"].is_null(), "nothing ingested yet: {row}");
    let (status, ack) = post(addr, Some(&key), batch(dev, "b1", &events(dev, 2, "s"))).await;
    assert_eq!(status, 200, "{ack}");
    assert_eq!(ack["accepted"], 2);
    let other = DeviceId::derive(&["server-test", "other"]);
    let (status, body) = post(addr, Some(&key), batch(other, "b2", &events(other, 1, "x"))).await;
    assert_eq!(status, 403, "another device's batch: {body}");

    // The key file lists the device with its user; the key is not in it.
    let (_, keys) = admin(
        addr,
        "GET",
        "/v1/admin/keys".into(),
        Some(ADMIN),
        Value::Null,
    )
    .await;
    let entry = keys["keys"]
        .as_array()
        .unwrap()
        .iter()
        .find(|k| k["tenant"] == "acme")
        .unwrap();
    assert_eq!(entry["device_id"], json!(dev));
    assert_eq!(entry["user_id"], "usr_kevin");
    assert!(!keys.to_string().contains(&key));

    // Re-pairing the same device retires the earlier key.
    let (_, body) = admin(
        addr,
        "POST",
        "/v1/admin/pairings".into(),
        Some(ADMIN),
        json!({ "tenant": "acme" }),
    )
    .await;
    let token2 = body["token"].as_str().unwrap().to_string();
    let (status, body) = public(
        addr,
        "POST",
        "/v1/pair",
        json!({ "token": token2, "device_id": dev, "label": "again" }),
        "10.0.0.2",
    )
    .await;
    assert_eq!(status, 201, "{body}");
    let key2 = body["key"].as_str().unwrap().to_string();
    let (status, _) = post(addr, Some(&key), batch(dev, "old", &[])).await;
    assert_eq!(status, 401, "the earlier key is gone");
    let (status, _) = post(addr, Some(&key2), batch(dev, "new", &[])).await;
    assert_eq!(status, 200);
    r.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn expired_tokens_and_hammering_addresses_are_refused() {
    let mut r = start_admin().await;
    let addr = r.addr;
    let (status, body) = admin(
        addr,
        "POST",
        "/v1/admin/pairings".into(),
        Some(ADMIN),
        json!({ "tenant": "acme", "ttl_secs": 0 }),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    let (status, body) = admin(
        addr,
        "POST",
        "/v1/admin/pairings".into(),
        Some(ADMIN),
        json!({ "tenant": "acme", "ttl_secs": 1 }),
    )
    .await;
    assert_eq!(status, 201, "{body}");
    let token = body["token"].as_str().unwrap().to_string();
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    let (status, body) = public(
        addr,
        "GET",
        &format!("/v1/pair/{token}"),
        Value::Null,
        "10.0.0.3",
    )
    .await;
    assert_eq!(status, 410, "{body}");
    assert!(body["error"].as_str().unwrap().contains("expired"));

    // The default pair rate is 10 per address at once, then 12/min: the
    // eleventh attempt from one address is refused, another address is not.
    let mut last = 0;
    for _ in 0..11 {
        let (s, _) = public(
            addr,
            "GET",
            &format!("/v1/pair/pair_{}", "1".repeat(64)),
            Value::Null,
            "10.9.9.9",
        )
        .await;
        last = s;
    }
    assert_eq!(last, 429, "the eleventh attempt from one address");
    let (s, _) = public(
        addr,
        "GET",
        &format!("/v1/pair/pair_{}", "1".repeat(64)),
        Value::Null,
        "10.9.9.8",
    )
    .await;
    assert_eq!(s, 404, "another address still answers");
    r.stop().await;
}
