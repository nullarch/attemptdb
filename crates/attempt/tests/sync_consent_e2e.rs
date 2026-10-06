//! `attempt sync` as a person meets it: what `connect` refuses, what it
//! normalises, what it records, and what `disconnect` does and says.
//!
//! Every `attempt` here runs with a temp HOME and data directory and with the
//! agents' config-directory variables removed: nothing outside the temp
//! directory is read or written.

use attemptdb_server::{Server, ServerConfig};
use serde_json::{Value, json};
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

const ADMIN: &str = "admin-secret-0123456789-abcdefgh";

fn attempt(home: &Path, data_dir: &Path, args: &[&str]) -> (bool, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_attempt"))
        .arg("--data-dir")
        .arg(data_dir)
        .args(args)
        .env("HOME", home)
        .env("ATTEMPTDB_KEYRING", "off")
        .env("ATTEMPTDB_NO_DAEMON", "1")
        .env("ATTEMPTDB_NO_AUTO_UPDATE", "1")
        .env_remove("ATTEMPTDB_KEY_FILE")
        .env_remove("ATTEMPTDB_DIR")
        .env_remove("VIBEMON_SYNC_URL")
        .env_remove("CLAUDE_CONFIG_DIR")
        .env_remove("CODEX_HOME")
        .env_remove("CURSOR_CONFIG_DIR")
        .env_remove("GEMINI_CONFIG_DIR")
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

fn mint(addr: std::net::SocketAddr) -> String {
    let (code, minted) = http(
        addr,
        "POST",
        "/v1/admin/pairings",
        Some(ADMIN),
        Some(json!({ "tenant": "acme", "user_id": "usr_kevin", "label": "laptop" })),
    );
    assert_eq!(code, 200, "{minted}");
    minted["token"].as_str().unwrap().to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connect_refuses_what_it_must_records_consent_and_disconnect_says_what_stays() {
    let tmp = tempfile::Builder::new().prefix("atdb").tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
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
    let data_dir = tmp.path().join("device");
    let run = |args: &[&str]| attempt(&home, &data_dir, args);

    let (ok, out) = run(&["init", "--capture-mode", "metadata_only"]);
    assert!(ok, "{out}");

    // Plain http to another machine is refused without the explicit flag, and
    // nothing is saved.
    let (ok, out) = run(&["sync", "connect", "http://sync.example.test", "--key", "k"]);
    assert!(!ok && out.contains("--allow-insecure-http"), "{out}");
    let (_, status) = run(&["sync", "status", "--json"]);
    assert!(status.contains("\"connected\": false") || status.contains("\"connected\":false"));
    // With the flag it is accepted, says so loudly, and remembers the choice.
    let (ok, out) = run(&[
        "sync",
        "connect",
        "http://sync.example.test",
        "--key",
        "k",
        "--allow-insecure-http",
        "--no-verify",
    ]);
    assert!(ok, "{out}");
    assert!(
        out.contains("WARNING") && out.contains("plain http"),
        "{out}"
    );
    let (_, status) = run(&["sync", "status", "--json"]);
    assert!(status.contains("\"allow_insecure_http\": true"), "{status}");
    let (ok, out) = run(&["sync", "disconnect", "--no-revoke"]);
    assert!(ok && out.contains("NOT revoked"), "{out}");

    // An exclude entry that names nothing is refused before the pairing token
    // is spent.
    let token = mint(addr);
    let (ok, out) = run(&[
        "sync",
        "connect",
        &url,
        "--pair",
        &token,
        "--exclude",
        "acme/private",
    ]);
    assert!(!ok, "{out}");
    assert!(
        out.contains("neither a project id") && out.contains("--exclude"),
        "{out}"
    );
    let (code, valid) = http(addr, "GET", &format!("/v1/pair/{token}"), None, None);
    assert_eq!(code, 200, "the token was not spent: {valid}");

    // A URL spelling is stored as the one canonical entry; consent is logged.
    let (ok, out) = run(&[
        "sync",
        "connect",
        &url,
        "--pair",
        &token,
        "--profile",
        "semantic",
        "--exclude",
        "https://GitHub.com/Acme/Private.git",
    ]);
    assert!(ok, "{out}");
    assert!(
        out.contains("exclude: https://GitHub.com/Acme/Private.git → github.com/acme/private"),
        "{out}"
    );
    assert!(out.contains("history     events recorded before"), "{out}");
    assert!(out.contains("consent     recorded in the log"), "{out}");
    let (_, out) = run(&["sync", "policy"]);
    assert!(out.contains("github.com/acme/private"), "{out}");
    // Removed by a different spelling of the same repository.
    let (ok, out) = run(&[
        "sync",
        "policy",
        "remove",
        "git@github.com:acme/private.git",
    ]);
    assert!(ok && out.contains("every repository uploads"), "{out}");
    let (ok, out) = run(&["sync", "policy", "exclude", "acme/private"]);
    assert!(!ok && out.contains("neither a project id"), "{out}");
    let (ok, out) = run(&["sync", "policy", "exclude", "git@gitlab.com:solo.git"]);
    assert!(!ok, "a remote needs host/owner/repo: {out}");
    let (ok, out) = run(&[
        "sync",
        "policy",
        "exclude",
        "git@github.com:Acme/Private.git",
    ]);
    assert!(ok && out.contains("github.com/acme/private"), "{out}");
    assert!(out.contains("never upload while a policy is set"), "{out}");

    // The consent is in the log, as counts: no repository name.
    let (ok, profile) = run(&["sync", "profile", "messages"]);
    assert!(ok, "{profile}");
    let (ok, rows) = run(&[
        "query",
        "--all-projects",
        "--csv",
        "SELECT kind, attrs_json FROM events WHERE kind = 'config_changed'",
    ]);
    assert!(ok, "{rows}");
    for needle in [
        "consent_version",
        "sync-consent-1",
        "x_attemptdb_sync_profile",
        "connected",
        "policy_changed",
        "profile_changed",
    ] {
        assert!(rows.contains(needle), "{needle} missing from {rows}");
    }
    assert!(
        !rows.to_lowercase().contains("acme") && !rows.contains("private"),
        "the log names no repository: {rows}"
    );
    let (_, status) = run(&["sync", "status"]);
    assert!(
        status.contains("consent") && status.contains("stays on this device"),
        "{status}"
    );

    // Upload what there is (the consent records), then end the connection.
    let (ok, out) = run(&["sync", "now"]);
    assert!(ok, "{out}");
    let (ok, out) = run(&["sync", "forget"]);
    assert!(!ok && out.contains("--yes"), "deleting needs --yes: {out}");
    let (ok, out) = run(&["sync", "disconnect", "--forget"]);
    assert!(ok, "{out}");
    assert!(
        out.contains("deleted") && out.contains("key revoked"),
        "{out}"
    );
    assert!(
        out.contains("disconnected; the local database is untouched"),
        "{out}"
    );
    assert!(
        out.contains("webhook"),
        "what a deletion cannot reach is said: {out}"
    );
    let (_, keys) = http(addr, "GET", "/v1/admin/keys", Some(ADMIN), None);
    assert!(
        keys["keys"]
            .as_array()
            .unwrap()
            .iter()
            .all(|k| k["tenant"] != "acme"),
        "the device key is gone from the server: {keys}"
    );
    let (_, status) = run(&["sync", "status", "--json"]);
    assert!(status.contains("\"connected\": false") || status.contains("\"connected\":false"));
    // Disconnecting when the server is unreachable says so and still ends it.
    let token = mint(addr);
    let (ok, out) = run(&["sync", "connect", &url, "--pair", &token]);
    assert!(ok, "{out}");
    let _ = state;
    let _ = stop_tx.send(());
    let _ = task.await;
    let (ok, out) = run(&["sync", "disconnect"]);
    assert!(ok, "{out}");
    assert!(
        out.contains("could not reach the server to revoke the key"),
        "{out}"
    );
    assert!(
        out.contains("still on"),
        "what stays on the server is said: {out}"
    );
    assert!(out.contains("disconnected"), "{out}");
}
