//! OTel records from sessions that no hook ever named must not cost the
//! writer a read of the history.
//!
//! The daemon used to look a session's project up by decoding every segment
//! for each session it had not cached (1.4 s for one unknown session on a
//! database of 4 million events, 5.8 s for three in one POST), and retried a
//! session that never got a project every five seconds, with the single
//! writer blocked meanwhile: every hook acknowledgement waited behind it.
//! Now the writer keeps a session -> project map, seeded from the project
//! columns alone and fed as it stores hooks.
//!
//! This file holds ONE test on purpose: it asserts on a process-wide counter
//! (`segment::full_batches_decoded`), which a second test in the same
//! process would move.

use attemptdb_capture::{
    Locator,
    config::{AutoUpdate, Config, DeviceRecord},
    daemon::{self, DaemonOptions},
    otel::{self, ReceiverConfig},
};
use attemptdb_core::{CaptureMode, Event, EventKind, ProjectRef, event::Provider};
use attemptdb_storage::{Database, OpenOptions, ScanFilter, segment};
use serde_json::json;
use std::time::{Duration, Instant};

fn hook(device: attemptdb_core::DeviceId, session: &str, path: &str) -> Event {
    Event::new(
        device,
        Provider::ClaudeCode,
        "UserPromptSubmit",
        EventKind::PromptSubmitted,
        ProjectRef::derive(path, None, &device),
        session,
        CaptureMode::MetadataOnly,
        "fixture",
    )
}

fn post(url: &str, bearer: &str, sessions: &[&str], at_nanos: u64) -> Duration {
    let rows: Vec<serde_json::Value> = sessions
        .iter()
        .map(|s| {
            json!({"timeUnixNano": at_nanos.to_string(), "attributes":[
                {"key":"event.name","value":{"stringValue":"api_request"}},
                {"key":"session.id","value":{"stringValue":s}},
                {"key":"input_tokens","value":{"intValue":"7"}}
            ]})
        })
        .collect();
    let payload = json!({"resourceLogs":[{"scopeLogs":[{"logRecords": rows}]}]});
    let t = Instant::now();
    let response = ureq::post(url)
        .set("Authorization", bearer)
        .set("Content-Type", "application/json")
        .send_string(&payload.to_string())
        .unwrap();
    assert_eq!(response.status(), 200);
    t.elapsed()
}

#[test]
fn telemetry_from_sessions_without_hooks_never_decodes_a_segment() {
    let tmp = tempfile::Builder::new().prefix("atotp").tempdir().unwrap();
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

    // A database with history in several segments: hook sessions that are
    // not the ones the OTel records belong to.
    {
        Database::create(&locator.db_dir, device).unwrap();
        let mut db = Database::open(
            &locator.db_dir,
            OpenOptions {
                flush_events: usize::MAX,
                flush_bytes: usize::MAX,
                ..Default::default()
            },
        )
        .unwrap();
        for segment_no in 0..8 {
            let events: Vec<Event> = (0..400)
                .map(|i| {
                    hook(
                        device,
                        &format!("history-{}", (segment_no * 400 + i) % 37),
                        "/home/dev/history",
                    )
                })
                .collect();
            db.ingest(events).unwrap();
            db.flush().unwrap().unwrap();
        }
        db.close().unwrap();
    }

    let port = {
        let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        socket.local_addr().unwrap().port()
    };
    let config = ReceiverConfig {
        port,
        token: "b".repeat(32),
    };
    std::fs::create_dir_all(&locator.paths.config_dir).unwrap();
    std::fs::write(
        ReceiverConfig::path(&locator),
        serde_json::to_vec(&config).unwrap(),
    )
    .unwrap();
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
    daemon::wait_until_running(&locator, Duration::from_secs(15)).expect("daemon starts");
    for _ in 0..60 {
        if otel::probe(&locator).unwrap()["running"] == true {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(otel::probe(&locator).unwrap()["running"], true);

    let url = config.endpoint("claude_code", "logs");
    let bearer = format!("Bearer {}", config.token);
    let unknown = ["no-hook-a", "no-hook-b", "no-hook-c"];

    // The first batch seeds the map (projected columns, no full decode).
    let before = segment::full_batches_decoded();
    post(&url, &bearer, &unknown, 1_787_904_000_000_000_000);
    // Every later batch (new records each time) costs the writer no read of
    // the segments for the sake of the three unknown sessions. Under the old
    // lookup each was three full decodes of every segment (and a retry
    // every five seconds).
    let mut slowest = Duration::ZERO;
    for i in 1..=30u64 {
        slowest = slowest.max(post(
            &url,
            &bearer,
            &unknown,
            1_787_904_000_000_000_000 + i * 1_000_000_000,
        ));
    }
    assert_eq!(
        segment::full_batches_decoded(),
        before,
        "resolving telemetry projects decoded segments"
    );
    // On a database this small a full decode is cheap too, so the counter is
    // the evidence; the time is a sanity bound for a loaded CI machine.
    assert!(slowest < Duration::from_secs(5), "slowest POST {slowest:?}");

    // A hook event for one of them: from then on its records are attributed.
    let named = hook(device, "no-hook-b", "/home/dev/now-known");
    attemptdb_capture::ingest::write_events(&locator, vec![named.clone()]).unwrap();
    post(&url, &bearer, &unknown, 1_787_904_100_000_000_000);
    assert_eq!(
        segment::full_batches_decoded(),
        before,
        "nor did attributing after a hook arrived"
    );

    assert!(daemon::stop(&locator).unwrap());
    handle.join().unwrap().unwrap();

    let db = attemptdb_capture::ingest::open_reader(&locator).unwrap();
    let rows = db.scan(&ScanFilter::default()).unwrap();
    let telemetry: Vec<Event> = rows.into_iter().filter(Event::is_telemetry).collect();
    assert_eq!(telemetry.len(), 3 * 32, "every record stored once");
    let attributed = |e: &Event| e.attrs["x_otel_project_attributed"] == true;
    // Only the session that got a hook, and only after it did.
    let attributed_rows: Vec<&Event> = telemetry.iter().filter(|e| attributed(e)).collect();
    assert_eq!(attributed_rows.len(), 1, "{attributed_rows:#?}");
    assert_eq!(attributed_rows[0].session_id, named.session_id);
    assert_eq!(
        attributed_rows[0].project.project_id,
        named.project.project_id
    );
    assert!(
        telemetry
            .iter()
            .filter(|e| !attributed(e))
            .all(|e| e.project.root == "otel/unattributed"),
        "unattributed records keep the placeholder project"
    );
}
