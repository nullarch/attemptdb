//! Real loopback HTTP -> daemon WAL -> reopened database, on all platforms.
use attemptdb_capture::{
    Locator,
    config::{AutoUpdate, Config, DeviceRecord},
    daemon::{self, DaemonOptions},
    otel::{self, ReceiverConfig},
};
use attemptdb_core::{CaptureMode, Event, EventKind, ProjectRef, event::Provider};
use attemptdb_storage::ScanFilter;
use serde_json::{Value, json};
use std::time::Duration;

struct Runtime {
    locator: Locator,
    handle: Option<std::thread::JoinHandle<attemptdb_capture::Result<()>>>,
}
impl Drop for Runtime {
    fn drop(&mut self) {
        let _ = daemon::stop(&self.locator);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

#[test]
fn authenticated_otlp_is_durable_deduplicated_private_and_joined_to_hooks() {
    let tmp = tempfile::Builder::new().prefix("atotel").tempdir().unwrap();
    let locator = Locator::resolve(tmp.path(), Some(&tmp.path().join("data")), None);
    Config {
        capture_mode: CaptureMode::MetadataOnly,
        auto_update: AutoUpdate::Off,
        ..Default::default()
    }
    .save(&locator.paths.config_dir)
    .unwrap();
    let device = DeviceRecord::load_or_create(&locator.paths.data_dir)
        .unwrap()
        .device_id;
    let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let config = ReceiverConfig {
        port: socket.local_addr().unwrap().port(),
        token: "a".repeat(32),
    };
    drop(socket);
    // Start without configuration, then install it. A running daemon must
    // begin receiving without an explicit service restart.
    let loc = locator.clone();
    let handle = std::thread::spawn(move || {
        daemon::run(
            &loc,
            DaemonOptions {
                spool_interval: Duration::from_millis(100),
                ..Default::default()
            },
        )
    });
    let mut runtime = Runtime {
        locator: locator.clone(),
        handle: Some(handle),
    };
    daemon::wait_until_running(&locator, Duration::from_secs(15)).expect("daemon starts");
    std::fs::write(
        ReceiverConfig::path(&locator),
        serde_json::to_vec(&config).unwrap(),
    )
    .unwrap();
    for _ in 0..40 {
        if otel::probe(&locator).unwrap()["running"] == true {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(otel::probe(&locator).unwrap()["running"], true);
    let hook = Event::new(
        device,
        Provider::ClaudeCode,
        "SessionStart",
        EventKind::SessionStarted,
        ProjectRef::derive("/home/dev/example/project", None, &device),
        "otel-fixture",
        CaptureMode::MetadataOnly,
        "fixture",
    );
    attemptdb_capture::ingest::write_events(&locator, vec![hook.clone()]).unwrap();
    let payload = json!({"resourceLogs":[{"scopeLogs":[{"logRecords":[{"timeUnixNano":"1787904000000000000","attributes":[
        {"key":"event.name","value":{"stringValue":"api_request"}},
        {"key":"session.id","value":{"stringValue":"otel-fixture"}},
        {"key":"input_tokens","value":{"intValue":"77"}},
        {"key":"prompt","value":{"stringValue":"CANARY_PRIVATE_PROMPT"}}
    ]}]}]}]});
    let url = config.endpoint("claude_code", "logs");
    let bearer = format!("Bearer {}", config.token);
    assert_eq!(
        ureq::post(&url)
            .set("Content-Type", "application/json")
            .send_string(&payload.to_string())
            .unwrap_err()
            .into_response()
            .unwrap()
            .status(),
        401
    );
    assert_eq!(
        ureq::post(&url)
            .set("Authorization", &bearer)
            .set("Origin", "https://example.com")
            .send_string("{}")
            .unwrap_err()
            .into_response()
            .unwrap()
            .status(),
        401
    );
    assert_eq!(
        ureq::post(&url)
            .set("Authorization", &bearer)
            .set("Content-Type", "application/x-protobuf")
            .send_string("fixture")
            .unwrap_err()
            .into_response()
            .unwrap()
            .status(),
        415
    );
    for _ in 0..2 {
        let response = ureq::post(&url)
            .set("Authorization", &bearer)
            .set("Content-Type", "application/json")
            .send_string(&payload.to_string())
            .unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(
            serde_json::from_reader::<_, Value>(response.into_reader()).unwrap(),
            json!({})
        );
    }
    let receipt = otel::probe(&locator).unwrap();
    assert_eq!(receipt["providers"]["claude_code:logs"]["accepted"], 1);
    let mut invalid = payload.clone();
    invalid["resourceLogs"][0]["scopeLogs"][0]["logRecords"][0]["timeUnixNano"] = json!("0");
    let response = ureq::post(&url)
        .set("Authorization", &bearer)
        .set("Content-Type", "application/json")
        .send_string(&invalid.to_string())
        .unwrap();
    assert_eq!(
        serde_json::from_reader::<_, Value>(response.into_reader()).unwrap()["partialSuccess"]["rejectedLogRecords"],
        "1"
    );
    assert!(daemon::stop(&locator).unwrap());
    runtime.handle.take().unwrap().join().unwrap().unwrap();
    let db = attemptdb_capture::ingest::open_reader(&locator).unwrap();
    let rows = db.scan(&ScanFilter::default()).unwrap();
    assert_eq!(
        rows.len(),
        2,
        "one hook plus one deduplicated OTel observation"
    );
    let e = rows.iter().find(|e| e.is_telemetry()).unwrap();
    assert_eq!(e.session_id, hook.session_id);
    assert_eq!(e.project.project_id, hook.project.project_id);
    assert_eq!(e.attrs["x_otel_project_attributed"], true);
    assert_eq!(e.attrs["x_otel_input_tokens"], 77);
    assert!(e.raw.is_none() && e.content.is_none());
    assert!(!serde_json::to_string(e).unwrap().contains("CANARY"));
}
