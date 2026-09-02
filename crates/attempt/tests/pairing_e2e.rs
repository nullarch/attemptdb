//! The one-line install's core, end to end: a pairing token from the
//! server's admin API becomes, through `attempt sync connect --pair`, a
//! device key bound to this database's device id; the connect is only
//! saved once the server has accepted the device; `attempt sync now` then
//! lands events and the server's device list shows the sync.

use attemptdb_server::{Server, ServerConfig};
use serde_json::{Value, json};
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

const ADMIN: &str = "admin-secret";

fn attempt(data_dir: &Path, args: &[&str]) -> (bool, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_attempt"))
        .arg("--data-dir")
        .arg(data_dir)
        .args(args)
        .env("ATTEMPTDB_KEYRING", "off")
        .env("ATTEMPTDB_NO_DAEMON", "1")
        .env_remove("ATTEMPTDB_KEY_FILE")
        .env_remove("ATTEMPTDB_DIR")
        .output()
        .expect("run attempt");
    (
        out.status.success(),
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
    )
}

fn http(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<Value>,
) -> (u16, Value) {
    let url = format!("http://{addr}{path}");
    let mut req = match method {
        "GET" => ureq::get(&url),
        _ => ureq::post(&url),
    };
    if let Some(t) = token {
        req = req.set("Authorization", &format!("Bearer {t}"));
    }
    let resp = match body {
        Some(b) => req
            .set("Content-Type", "application/json")
            .send_string(&b.to_string()),
        None => req.call(),
    };
    match resp {
        Ok(r) => {
            let text = r.into_string().unwrap_or_default();
            (200, serde_json::from_str(&text).unwrap_or(Value::Null))
        }
        Err(ureq::Error::Status(s, r)) => {
            let text = r.into_string().unwrap_or_default();
            (
                s,
                serde_json::from_str(&text).unwrap_or(Value::String(text)),
            )
        }
        Err(e) => panic!("{e}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pair_connect_sync_shows_the_device_on_the_server() {
    let tmp = tempfile::Builder::new().prefix("atdb").tempdir().unwrap();
    let server_dir = tmp.path().join("server");
    std::fs::create_dir_all(&server_dir).unwrap();
    let keys_file = server_dir.join("keys.json");
    std::fs::write(&keys_file, "{\"keys\":[]}").unwrap();
    let server = Server::bind(ServerConfig {
        port: 0,
        data_dir: server_dir.join("data"),
        keys_file,
        admin_token: Some(ADMIN.into()),
        ..Default::default()
    })
    .await
    .unwrap();
    let addr = server.addr();
    let state = Arc::clone(server.state());
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let task = tokio::spawn(server.run(async move {
        let _ = stop_rx.await;
    }));
    let url = format!("http://{addr}");

    // The device: a fresh database.
    let data_dir = tmp.path().join("device");
    let (ok, out) = attempt(&data_dir, &["init", "--capture-mode", "metadata_only"]);
    assert!(ok, "{out}");

    // A wrong key is refused by the handshake and never saved.
    let (ok, out) = attempt(&data_dir, &["sync", "connect", &url, "--key", "atk_nope"]);
    assert!(!ok, "{out}");
    assert!(
        out.contains("401") || out.contains("does not know this key"),
        "{out}"
    );
    let (_, status) = attempt(&data_dir, &["sync", "status", "--json"]);
    assert!(
        status.contains("\"connected\":false") || status.contains("\"connected\": false"),
        "{status}"
    );

    // The web mints a token (server to server, admin token).
    let (code, minted) = http(
        addr,
        "POST",
        "/v1/admin/pairings",
        Some(ADMIN),
        Some(json!({ "tenant": "acme", "user_id": "usr_kevin", "label": "kevin laptop" })),
    );
    assert_eq!(code, 200, "{minted}");
    let token = minted["token"].as_str().unwrap().to_string();

    // The user runs the one line; the CLI checks, pairs, handshakes, saves.
    let (ok, out) = attempt(&data_dir, &["sync", "connect", &url, "--pair", &token]);
    assert!(ok, "{out}");
    assert!(out.contains("paired: tenant acme as usr_kevin"), "{out}");
    assert!(out.contains("authenticated:"), "{out}");
    assert!(
        out.contains("profile     semantic"),
        "the default profile: {out}"
    );
    assert!(
        out.contains("interval    5s"),
        "the default interval: {out}"
    );
    let masked = out.lines().find(|l| l.contains("key ")).unwrap_or("");
    assert!(
        masked.contains("atk_…") && !masked.contains(&minted["sha256"].as_str().unwrap()[..8]),
        "the key is masked: {out}"
    );
    assert!(
        !out.contains("atk_0") && !out.contains("atk_a") && !out.contains("atk_b"),
        "no full key anywhere: {out}"
    );

    // The token is spent.
    let (ok, out) = attempt(&data_dir, &["sync", "connect", &url, "--pair", &token]);
    assert!(!ok);
    assert!(out.contains("already used"), "{out}");

    // Events land (written straight into the device's database, as the
    // hook would), and the server's device list shows the sync under the
    // user.
    let db_dir = data_dir.join("db").join(".attemptdb");
    let device_id = {
        use attemptdb_core::event::Provider;
        use attemptdb_core::{CaptureMode, Event, EventKind, ProjectRef};
        use attemptdb_storage::{Database, OpenOptions};
        let mut db = Database::open(&db_dir, OpenOptions::default()).unwrap();
        let dev = db.device_id();
        let events: Vec<Event> = (0..3)
            .map(|_| {
                Event::new(
                    dev,
                    Provider::ClaudeCode,
                    "PostToolUse",
                    EventKind::ToolCallFinished,
                    ProjectRef::derive("/home/dev/example/project", None, &dev),
                    "session-1".to_string(),
                    CaptureMode::MetadataOnly,
                    "pairing-e2e/0.1",
                )
            })
            .collect();
        db.ingest(events).unwrap();
        db.close().unwrap();
        dev
    };
    let (ok, out) = attempt(&data_dir, &["sync", "now"]);
    assert!(ok, "{out}");
    assert!(out.contains("3") || out.contains("uploaded"), "{out}");
    let (_, reader) = http(
        addr,
        "POST",
        "/v1/admin/keys",
        Some(ADMIN),
        Some(json!({ "tenant": "acme", "scope": "reader", "label": "web" })),
    );
    let (code, devices) = http(addr, "GET", "/v1/devices", reader["key"].as_str(), None);
    assert_eq!(code, 200, "{devices}");
    let rows = devices["devices"].as_array().unwrap();
    let row = rows
        .iter()
        .find(|d| {
            d["keys"]
                .as_array()
                .is_some_and(|k| k.iter().any(|k| k["user_id"] == "usr_kevin"))
        })
        .expect("the paired device is listed");
    assert!(row["last_sync_at"].is_string(), "{row}");
    assert_eq!(row["device_id"], json!(device_id.to_string()), "{row}");
    assert_eq!(row["connected"], true);
    let _ = state;
    let _ = stop_tx.send(());
    let _ = task.await;
}
