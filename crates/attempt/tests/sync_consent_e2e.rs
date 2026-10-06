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

// ---------------------------------------------------------------------------
// The VibeMon migration, and `sync forget`, as a person meets them
// ---------------------------------------------------------------------------

struct Rig {
    _tmp: tempfile::TempDir,
    home: std::path::PathBuf,
    data_dir: std::path::PathBuf,
    server_data: std::path::PathBuf,
    addr: std::net::SocketAddr,
    url: String,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    task: Option<tokio::task::JoinHandle<anyhow::Result<()>>>,
}

impl Rig {
    async fn new(ceiling: attemptdb_core::CaptureMode) -> Self {
        let tmp = tempfile::Builder::new().prefix("atdb").tempdir().unwrap();
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let server_dir = tmp.path().join("server");
        std::fs::create_dir_all(&server_dir).unwrap();
        let keys_file = server_dir.join("keys.json");
        std::fs::write(&keys_file, "{\"keys\":[]}").unwrap();
        let server_data = server_dir.join("data");
        let server = Server::bind(ServerConfig {
            port: 0,
            data_dir: server_data.clone(),
            keys_file,
            admin_token: Some(ADMIN.into()),
            capture_mode: ceiling,
            ..Default::default()
        })
        .await
        .unwrap();
        let addr = server.addr();
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let task = tokio::spawn(server.run(async move {
            let _ = stop_rx.await;
        }));
        let data_dir = tmp.path().join("device");
        Self {
            home,
            data_dir,
            server_data,
            addr,
            url: format!("http://{addr}"),
            stop: Some(stop_tx),
            task: Some(task),
            _tmp: tmp,
        }
    }

    fn run(&self, args: &[&str]) -> (bool, String) {
        attempt(&self.home, &self.data_dir, args)
    }

    fn ok(&self, args: &[&str]) -> String {
        let (ok, out) = self.run(args);
        assert!(ok, "attempt {args:?} failed:\n{out}");
        out
    }

    fn status(&self) -> Value {
        let out = self.ok(&["sync", "status", "--json"]);
        serde_json::from_str::<Value>(&out).unwrap()["peers"]["default"].clone()
    }

    /// What the server holds of the tenant: every file under it, and every
    /// stored event decoded.
    fn tenant_text(&self) -> String {
        let dir = self.server_data.join("tenants").join("acme");
        let mut text = String::new();
        let mut stack = vec![dir.clone()];
        while let Some(d) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(&d) else {
                continue;
            };
            for entry in rd.flatten() {
                let p = entry.path();
                if p.is_dir() {
                    stack.push(p);
                } else if let Ok(bytes) = std::fs::read(&p) {
                    text.push_str(&String::from_utf8_lossy(&bytes));
                }
            }
        }
        if let Ok(db) = attemptdb_storage::Database::open(
            &dir,
            attemptdb_storage::OpenOptions {
                read_only: true,
                ..Default::default()
            },
        ) {
            text.push_str(
                &serde_json::to_string(
                    &db.scan(&attemptdb_storage::ScanFilter::default()).unwrap(),
                )
                .unwrap(),
            );
        }
        text
    }

    async fn finish(mut self) {
        let _ = self.stop.take().unwrap().send(());
        let _ = self.task.take().unwrap().await;
    }
}

fn spool(data_dir: &Path, events: &[attemptdb_core::Event]) {
    let db_dir = data_dir.join("db").join(".attemptdb");
    attemptdb_storage::SpoolWriter::new(&db_dir)
        .unwrap()
        .append(events)
        .unwrap();
}

fn device_id(data_dir: &Path) -> attemptdb_core::DeviceId {
    let db_dir = data_dir.join("db").join(".attemptdb");
    attemptdb_storage::Database::open(&db_dir, attemptdb_storage::OpenOptions::default())
        .unwrap()
        .device_id()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn imported_vibemon_history_is_held_until_the_person_includes_it_and_every_surface_says_how()
{
    let rig = Rig::new(attemptdb_core::CaptureMode::MetadataOnly).await;
    rig.ok(&["init", "--capture-mode", "metadata_only"]);
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/vibemon-export/hook_events.json");
    let fixture = fixture.to_str().unwrap();

    // The migration: import the export, then connect.
    rig.ok(&["import", "vibemon-export", fixture, "--device", "local"]);
    let count = |rig: &Rig| -> u64 {
        let out = rig.ok(&[
            "query",
            "--all-projects",
            "--csv",
            "SELECT count(*) AS n FROM events WHERE attrs_json LIKE '%x_vibemon_import%'",
        ]);
        out.lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap()
            .trim()
            .parse()
            .unwrap()
    };
    let imported = count(&rig);
    assert!(imported >= 10, "the fixture has a dozen rows: {imported}");
    let (code, minted) = http(
        rig.addr,
        "POST",
        "/v1/admin/pairings",
        Some(ADMIN),
        Some(json!({ "tenant": "acme", "user_id": "usr_kevin", "label": "laptop" })),
    );
    assert_eq!(code, 200, "{minted}");
    let token = minted["token"].as_str().unwrap().to_string();
    let out = rig.ok(&[
        "sync",
        "connect",
        &rig.url,
        "--pair",
        &token,
        "--profile",
        "messages",
    ]);
    assert!(
        out.contains("attempt sync history include --peer default"),
        "connect names the command: {out}"
    );

    // Before anything has been uploaded, status already says what is waiting
    // and what to run.
    let status = rig.ok(&["sync", "status"]);
    assert!(
        status.contains("kept local")
            && status.contains("`attempt sync history include --peer default`"),
        "{status}"
    );

    // Importing more while connected: the import itself says so.
    let export: Value = serde_json::from_str(&std::fs::read_to_string(fixture).unwrap()).unwrap();
    let second: Vec<Value> = export
        .as_array()
        .unwrap()
        .iter()
        .map(|row| {
            let mut row = row.clone();
            let id = row["id"].as_str().unwrap().replacen("1111", "2222", 1);
            row["id"] = json!(id);
            row
        })
        .collect();
    let second_path = rig._tmp.path().join("second.json");
    std::fs::write(&second_path, serde_json::to_string(&second).unwrap()).unwrap();
    let out = rig.ok(&[
        "import",
        "vibemon-export",
        second_path.to_str().unwrap(),
        "--device",
        "local",
    ]);
    assert!(
        out.contains("sync: peer default keeps history")
            && out.contains("`attempt sync history include --peer default`"),
        "the import names the command: {out}"
    );
    let imported = count(&rig);

    // `sync now` withholds all of it, says so, and names the command.
    let out = rig.ok(&["sync", "now"]);
    assert!(out.contains("kept local"), "{out}");
    assert!(
        out.contains("attempt sync history include --peer default"),
        "{out}"
    );
    let uploaded = rig.status()["state"]["events"].as_u64().unwrap();
    assert!(
        uploaded < imported,
        "none of the {imported} imported events went: {uploaded}"
    );

    // The explicit command, with no key and no re-pairing.
    let out = rig.ok(&["sync", "history", "include"]);
    assert!(out.contains("now included"), "{out}");
    let out = rig.ok(&["sync", "now"]);
    assert!(!out.contains("error"), "{out}");
    let status = rig.status();
    assert!(
        status["state"]["events"].as_u64().unwrap() >= imported,
        "all {imported} imported events are on the server now: {status}"
    );
    assert_eq!(status["state"]["before_consent"], 0);
    assert!(
        rig.run(&["sync", "history", "include"])
            .1
            .contains("nothing is held back"),
        "and a second run says there is nothing left to include"
    );
    let text = rig.ok(&["sync", "status"]);
    assert!(!text.contains("kept local"), "{text}");
    // Consent to this is in the log, as a count.
    let rows = rig.ok(&[
        "query",
        "--all-projects",
        "--csv",
        "SELECT attrs_json FROM events WHERE kind = 'config_changed'",
    ]);
    assert!(rows.contains("history_included"), "{rows}");
    rig.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forget_is_not_undone_by_the_next_sync() {
    use attemptdb_core::event::{EventContent, Provider};
    use attemptdb_core::{CaptureMode, Event, EventKind, PortablePath, ProjectRef};
    let rig = Rig::new(CaptureMode::FullSync).await;
    rig.ok(&["init", "--capture-mode", "local_semantic"]);
    let (_, minted) = http(
        rig.addr,
        "POST",
        "/v1/admin/pairings",
        Some(ADMIN),
        Some(json!({ "tenant": "acme", "user_id": "usr_kevin", "label": "laptop" })),
    );
    let token = minted["token"].as_str().unwrap().to_string();
    rig.ok(&[
        "sync",
        "connect",
        &rig.url,
        "--pair",
        &token,
        "--profile",
        "full",
    ]);
    let dev = device_id(&rig.data_dir);
    let work = |project: &str, file: &str, prompt: &str| -> Event {
        let root = format!("/home/dev/{project}");
        let mut e = Event::new(
            dev,
            Provider::ClaudeCode,
            "UserPromptSubmit",
            EventKind::PromptSubmitted,
            ProjectRef::derive(
                &root,
                Some(&format!("git@github.com:acme/{project}.git")),
                &dev,
            ),
            format!("session-{project}"),
            CaptureMode::LocalSemantic,
            "forget-e2e/0.1",
        );
        e.paths = vec![PortablePath::from_raw(
            &format!("{root}/src/{file}"),
            Some(&root),
        )];
        e.content = Some(EventContent {
            prompt: Some(prompt.into()),
            ..Default::default()
        });
        e
    };
    spool(
        &rig.data_dir,
        &[work(
            "forgotten-canary-repo",
            "forgotten_canary_file.rs",
            "FORGOTTEN_PROMPT_CANARY refactor it",
        )],
    );
    rig.ok(&["sync", "now"]);
    let before = rig.tenant_text();
    assert!(
        before.contains("forgotten-canary-repo") && before.contains("FORGOTTEN_PROMPT_CANARY"),
        "the canaries reached the server first"
    );

    let (ok, out) = rig.run(&["sync", "forget"]);
    assert!(
        !ok && out.contains("--yes") && out.contains("stays local"),
        "{out}"
    );
    let out = rig.ok(&["sync", "forget", "--yes"]);
    assert!(
        out.contains("stays on this device")
            && out.contains("attempt sync history include --peer default"),
        "{out}"
    );
    let after = rig.tenant_text();
    for canary in [
        "forgotten-canary-repo",
        "forgotten_canary_file",
        "FORGOTTEN_PROMPT",
    ] {
        assert!(!after.contains(canary), "{canary} survived the forget");
    }

    // The person goes on working elsewhere; the next sync must not bring the
    // forgotten project back — not its events, not the documents inferred
    // from the whole local history.
    spool(
        &rig.data_dir,
        &[work("fresh-repo", "fresh_file.rs", "a brand new prompt")],
    );
    let out = rig.ok(&["sync", "now", "--inferences"]);
    assert!(!out.contains("error"), "{out}");
    let text = rig.tenant_text();
    assert!(
        text.contains("fresh-repo") && text.contains("fresh_file.rs"),
        "the new work arrived"
    );
    for canary in [
        "forgotten-canary-repo",
        "forgotten_canary_file",
        "FORGOTTEN_PROMPT",
    ] {
        assert!(
            !text.contains(canary),
            "{canary} is back on the server after forget and a new event"
        );
    }
    // And it is still on the device.
    let local = rig.ok(&[
        "query",
        "--all-projects",
        "--csv",
        "SELECT count(*) AS n FROM events WHERE project_name LIKE '%forgotten-canary-repo%'",
    ]);
    assert!(
        !local.trim().ends_with('0'),
        "the local history is untouched: {local}"
    );

    // The way back is one command.
    rig.ok(&["sync", "history", "include"]);
    rig.ok(&["sync", "now"]);
    assert!(rig.tenant_text().contains("forgotten-canary-repo"));
    rig.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_policy_entry_that_matches_no_project_says_so_and_narrowing_a_profile_says_what_stays() {
    use attemptdb_core::event::Provider;
    use attemptdb_core::{CaptureMode, Event, EventKind, ProjectRef};
    let rig = Rig::new(CaptureMode::MetadataOnly).await;
    rig.ok(&["init", "--capture-mode", "metadata_only"]);
    let (_, minted) = http(
        rig.addr,
        "POST",
        "/v1/admin/pairings",
        Some(ADMIN),
        Some(json!({ "tenant": "acme", "user_id": "usr_kevin", "label": "laptop" })),
    );
    let token = minted["token"].as_str().unwrap().to_string();
    rig.ok(&[
        "sync",
        "connect",
        &rig.url,
        "--pair",
        &token,
        "--profile",
        "full",
    ]);
    // A repository this device has seen through an ssh host alias.
    let dev = device_id(&rig.data_dir);
    spool(
        &rig.data_dir,
        &[Event::new(
            dev,
            Provider::ClaudeCode,
            "PostToolUse",
            EventKind::ToolCallFinished,
            ProjectRef::derive(
                "/home/dev/private",
                Some("git@github-work:acme/private.git"),
                &dev,
            ),
            "s".to_string(),
            CaptureMode::MetadataOnly,
            "policy-e2e/0.1",
        )],
    );
    rig.ok(&["sync", "now"]);

    // The entry the person wrote from the web page names nothing this device
    // has seen: accepted, and told so, with the nearest recorded project.
    let out = rig.ok(&[
        "sync",
        "policy",
        "exclude",
        "https://github.com/acme/private/tree/main",
    ]);
    assert!(
        out.contains("github.com/acme/private"),
        "stored without the tail: {out}"
    );
    assert!(
        out.contains("matches no project this device has recorded")
            && out.contains("github-work/acme/private"),
        "{out}"
    );
    // Written with the alias, it matches and is quiet.
    let out = rig.ok(&[
        "sync",
        "policy",
        "exclude",
        "git@github-work:acme/private.git",
    ]);
    assert!(!out.contains("matches no project"), "{out}");

    // Narrowing: what stays on the server, and the command that deletes it.
    let out = rig.ok(&["sync", "profile", "metadata_only"]);
    assert!(
        out.contains("already uploaded")
            && out.contains("stays on")
            && out.contains("attempt sync forget --peer default"),
        "{out}"
    );
    // Widening says nothing of the kind.
    let out = rig.ok(&["sync", "profile", "full"]);
    assert!(!out.contains("already uploaded"), "{out}");
    rig.finish().await;
}
