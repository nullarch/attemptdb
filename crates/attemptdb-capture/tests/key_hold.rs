//! A content key that exists but cannot be read right now.
//!
//! It used to turn every event that arrived meanwhile into one stored
//! without its content, delete the spool file it came from, and never look
//! again (640 prompts and tool events lost their text for good on a database
//! with encrypted blobs and an unreadable key). The gate now *holds*: spool
//! files are left alone, the daemon refuses events over the socket (a hook
//! falls back to its spool), and everything is imported with its content as
//! soon as the key reads. The hold is bounded; past the bound events are
//! stored without content, loudly, as before. A key nobody ever created is
//! not a key to wait for (`required` on a database that never had one).
//!
//! Keys here are key files under the sandbox data directory; "unreadable"
//! is the key file moved aside.

use attemptdb_capture::config::{Config, EncryptionMode};
use attemptdb_capture::doctor::capture_health;
use attemptdb_capture::keys::{
    self, CONTENT_WITHHELD_ATTR, EncryptionState, GateDecision, HoldLimits, InitOptions,
    KeyStoreOptions, NoticeLevel,
};
use attemptdb_capture::{Locator, ingest};
use attemptdb_core::event::{EventContent, Provider};
use attemptdb_core::{CaptureMode, DeviceId, Event, EventKind, ProjectRef};
use attemptdb_storage::{Database, Identity, OpenOptions, ScanFilter, SpoolWriter};
use std::path::{Path, PathBuf};
use std::time::Duration;

const SECRET: &str = "held-plaintext-must-not-reach-disk-5521";

struct Sandbox {
    _tmp: tempfile::TempDir,
    // Read only by the daemon tests, which are Unix-only.
    #[cfg_attr(not(unix), allow(dead_code))]
    project: PathBuf,
    locator: Locator,
    device: DeviceId,
}

fn sandbox(mode: EncryptionMode) -> Sandbox {
    // Short prefix: a daemon socket path must fit sun_path.
    let tmp = tempfile::Builder::new().prefix("athold").tempdir().unwrap();
    let project = tmp.path().join("proj");
    std::fs::create_dir_all(&project).unwrap();
    let locator = Locator::resolve(&project, Some(&tmp.path().join("data")), None);
    Config {
        encryption: mode,
        ..Config::default()
    }
    .save(&locator.paths.config_dir)
    .unwrap();
    let device = DeviceId::new();
    Database::create(&locator.db_dir, device).unwrap();
    Sandbox {
        _tmp: tmp,
        project,
        locator,
        device,
    }
}

impl Sandbox {
    fn db_id(&self) -> uuid::Uuid {
        Identity::load(&self.locator.db_dir).unwrap().db_id
    }

    fn event(&self, n: usize) -> Event {
        let mut ev = Event::new(
            self.device,
            Provider::ClaudeCode,
            "UserPromptSubmit",
            EventKind::PromptSubmitted,
            ProjectRef::derive("/p", None, &self.device),
            format!("s-{n}"),
            CaptureMode::LocalSemantic,
            "test",
        );
        ev.content = Some(EventContent {
            prompt: Some(format!("{SECRET} {n}")),
            ..Default::default()
        });
        ev.raw = Some(serde_json::json!({"prompt": format!("{SECRET} {n}")}));
        ev
    }

    fn spool(&self, events: &[Event]) {
        SpoolWriter::new(&self.locator.db_dir)
            .unwrap()
            .append(events)
            .unwrap();
    }

    fn key_file(&self) -> PathBuf {
        keys::default_key_file(&self.locator, self.db_id())
    }

    /// A key, and one flushed event whose content is an encrypted blob: the
    /// database now has a key that it needs.
    fn make_blobs(&self) {
        keys::init(
            &self.locator,
            self.db_id(),
            &InitOptions {
                key_file: true,
                passphrase_env: None,
                store: Some(KeyStoreOptions::offline()),
            },
        )
        .unwrap();
        let mut db = Database::open(
            &self.locator.db_dir,
            OpenOptions {
                keys: keys::provider_for_db(&self.locator, &self.locator.db_dir),
                ..Default::default()
            },
        )
        .unwrap();
        db.ingest(vec![self.event(100)]).unwrap();
        db.flush().unwrap();
        assert!(db.blob_stats().unwrap().count >= 1, "the content is a blob");
    }

    fn hide_key(&self) {
        std::fs::rename(self.key_file(), self.key_file().with_extension("hidden")).unwrap();
    }

    fn show_key(&self) {
        std::fs::rename(self.key_file().with_extension("hidden"), self.key_file()).unwrap();
    }

    /// Every spool file and its exact bytes.
    fn spool_snapshot(&self) -> Vec<(String, Vec<u8>)> {
        let mut files: Vec<(String, Vec<u8>)> =
            std::fs::read_dir(self.locator.db_dir.join("spool"))
                .unwrap()
                .flatten()
                .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("spool"))
                .map(|e| {
                    (
                        e.file_name().to_string_lossy().into_owned(),
                        std::fs::read(e.path()).unwrap(),
                    )
                })
                .collect();
        files.sort();
        files
    }

    fn stored(&self) -> Vec<Event> {
        Database::open(
            &self.locator.db_dir,
            OpenOptions {
                read_only: true,
                keys: keys::provider_for_db(&self.locator, &self.locator.db_dir),
                ..Default::default()
            },
        )
        .unwrap()
        .scan(&ScanFilter::default())
        .unwrap()
    }

    fn on_disk(&self, needle: &str) -> bool {
        fn walk(dir: &Path, needle: &[u8]) -> bool {
            std::fs::read_dir(dir)
                .into_iter()
                .flatten()
                .flatten()
                .any(|e| {
                    let p = e.path();
                    if p.is_dir() {
                        walk(&p, needle)
                    } else {
                        std::fs::read(&p)
                            .is_ok_and(|b| b.windows(needle.len()).any(|w| w == needle))
                    }
                })
        }
        walk(&self.locator.db_dir, needle.as_bytes())
    }

    /// The writer's gate as a CLI process or the daemon builds it, with a
    /// recheck interval of zero (a key that appears is seen at once).
    fn gate(&self, mode: EncryptionMode) -> keys::WriterKeys {
        keys::writer_keys_rechecking(
            &self.locator,
            &self.locator.db_dir,
            mode,
            KeyStoreOptions::offline(),
            Duration::ZERO,
        )
    }

    fn writer(&self, keys: &keys::WriterKeys) -> Database {
        Database::open(
            &self.locator.db_dir,
            OpenOptions {
                keys: keys.provider.clone(),
                ..Default::default()
            },
        )
        .unwrap()
    }
}

fn waits_for_key(text: &str, n: usize) -> bool {
    text.contains(&format!("{n} events are waiting for the content key"))
}

#[test]
fn an_unreadable_key_leaves_spooled_events_alone_until_it_reads() {
    let sb = sandbox(EncryptionMode::Auto);
    sb.make_blobs();
    sb.hide_key();
    sb.spool(&[sb.event(1), sb.event(2)]);
    let before = sb.spool_snapshot();
    assert!(!before.is_empty());
    let stored_before = sb.stored().len();
    assert_eq!(stored_before, 1);

    // An import does nothing: no event stored, no spool file touched.
    let pending = ingest::import_pending(&sb.locator).unwrap();
    assert_eq!(pending.report.as_ref().unwrap().accepted, 0);
    assert_eq!(sb.spool_snapshot(), before, "spool files are not consumed");
    assert_eq!(
        sb.stored().len(),
        stored_before,
        "the database is unchanged"
    );
    assert!(
        pending.warnings.iter().any(|w| waits_for_key(w, 2)),
        "{:?}",
        pending.warnings
    );

    // Read-only commands still work while events wait, and say so.
    let (db, _, _) = ingest::open_for_read(&sb.locator).unwrap();
    assert!(
        db.warnings.iter().any(|w| waits_for_key(w, 2)),
        "{:?}",
        db.warnings
    );
    assert_eq!(
        db.scan(&ScanFilter::default()).unwrap().len(),
        stored_before
    );
    drop(db);

    // The doctor says it too, with the cause and the way out.
    let config = Config::load_or_default(&sb.locator.paths.config_dir);
    let health = capture_health(&sb.locator, &config);
    assert!(health.has_problem());
    let lines = health.lines().join("\n");
    assert!(waits_for_key(&lines, 2), "{lines}");
    assert!(
        lines.contains("imported with their content") && lines.contains("attempt keys status"),
        "{lines}"
    );
    let state = keys::read_state(&sb.locator, sb.db_id()).unwrap();
    assert_eq!(state.state, "holding");

    // The key reads again: everything is imported, with its content.
    sb.show_key();
    let (mut db, report, _) = ingest::open_fresh(&sb.locator, false).unwrap();
    assert_eq!(report.unwrap().accepted, 2);
    db.flush().unwrap();
    drop(db);
    assert!(sb.spool_snapshot().is_empty(), "imported means gone");
    assert!(!sb.on_disk(SECRET), "encrypted at rest");
    let events = sb.stored();
    assert_eq!(events.len(), 3);
    for n in [1, 2, 100] {
        let ev = events
            .iter()
            .find(|e| e.provider_session_id == format!("s-{n}"))
            .unwrap();
        assert!(
            ev.content
                .as_ref()
                .and_then(|c| c.prompt.as_deref())
                .is_some_and(|p| p.contains(SECRET)),
            "event {n} lost its content: {ev:?}"
        );
        assert!(ev.attrs.get(CONTENT_WITHHELD_ATTR).is_none());
    }
    assert_eq!(
        keys::read_state(&sb.locator, sb.db_id()).unwrap().state,
        "encrypting"
    );
    let health = capture_health(&sb.locator, &config);
    assert!(!health.has_problem(), "{:?}", health.lines());
}

#[test]
fn a_required_key_nobody_ever_made_is_withheld_not_waited_for() {
    // Today's behaviour, kept: there is no key to wait for.
    let sb = sandbox(EncryptionMode::Required);
    sb.spool(&[sb.event(1)]);
    let keys = sb.gate(EncryptionMode::Required);
    assert_eq!(keys.gate.decision(), GateDecision::Withhold);
    let mut db = sb.writer(&keys);
    let report = ingest::import_spool(&mut db, &keys.gate).unwrap();
    assert_eq!(report.accepted, 1);
    assert!(sb.spool_snapshot().is_empty());
    drop(db);
    let ev = &sb.stored()[0];
    assert!(ev.content.is_none() && ev.raw.is_none());
    assert_eq!(
        ev.attrs.get(CONTENT_WITHHELD_ATTR),
        Some(&serde_json::json!("no_key"))
    );
}

#[test]
fn off_never_holds() {
    let sb = sandbox(EncryptionMode::Off);
    sb.make_blobs();
    sb.hide_key();
    let keys = sb.gate(EncryptionMode::Off);
    assert_eq!(keys.gate.decision(), GateDecision::Open);
}

#[test]
fn a_hold_ends_after_its_age_and_stays_ended_until_the_key_reads() {
    let sb = sandbox(EncryptionMode::Auto);
    sb.make_blobs();
    sb.hide_key();
    sb.spool(&[sb.event(1), sb.event(2)]);
    let keys = sb.gate(EncryptionMode::Auto);
    let gate = keys.gate.clone().with_hold_limits(HoldLimits {
        max_age: Duration::from_millis(400),
        max_spool_bytes: u64::MAX,
    });
    let mut db = sb.writer(&keys);
    let before = sb.spool_snapshot();

    assert_eq!(gate.decision(), GateDecision::Hold);
    assert_eq!(ingest::import_spool(&mut db, &gate).unwrap().accepted, 0);
    assert_eq!(sb.spool_snapshot(), before);
    let notices = gate.take_notices();
    assert!(
        notices
            .iter()
            .any(|n| n.level == NoticeLevel::Warn && n.message.contains("wait in the spool")),
        "{notices:?}"
    );

    std::thread::sleep(Duration::from_millis(600));
    assert_eq!(gate.decision(), GateDecision::Withhold, "the hold ran out");
    let report = ingest::import_spool(&mut db, &gate).unwrap();
    assert_eq!(report.accepted, 2, "now they are stored, without content");
    let notices = gate.take_notices();
    assert!(
        notices.iter().any(|n| n.level == NoticeLevel::Error
            && n.message.contains("stayed unreadable")
            && n.message.contains("metadata-only")),
        "loud: {notices:?}"
    );
    db.flush().unwrap();
    assert!(!sb.on_disk(SECRET));
    let withheld: Vec<Event> = sb
        .stored()
        .into_iter()
        .filter(|e| e.attrs.contains_key(CONTENT_WITHHELD_ATTR))
        .collect();
    assert_eq!(withheld.len(), 2);
    assert!(withheld.iter().all(|e| e.content.is_none()));
    assert_eq!(
        keys::read_state(&sb.locator, sb.db_id()).unwrap().state,
        "withholding"
    );

    // Draining the spool did not reopen the hold.
    sb.spool(&[sb.event(3)]);
    assert_eq!(gate.decision(), GateDecision::Withhold);
    let state = keys::read_state(&sb.locator, sb.db_id()).unwrap();
    assert!(state.hold_exhausted);

    // And a restart does not either: the next writer starts withholding.
    let next = sb.gate(EncryptionMode::Auto);
    assert_eq!(next.gate.decision(), GateDecision::Withhold);

    // The key reads: the gate opens and forgets the exhausted hold.
    sb.show_key();
    assert_eq!(gate.decision(), GateDecision::Open);
    let state = keys::read_state(&sb.locator, sb.db_id()).unwrap();
    assert_eq!(state.state, "encrypting");
    assert!(!state.hold_exhausted);
}

#[test]
fn a_hold_ends_when_the_spool_outgrows_its_room() {
    let sb = sandbox(EncryptionMode::Auto);
    sb.make_blobs();
    sb.hide_key();
    let keys = sb.gate(EncryptionMode::Auto);
    let gate = keys.gate.clone().with_hold_limits(HoldLimits {
        max_age: Duration::from_secs(3600),
        max_spool_bytes: 2_000,
    });
    // Room to spare: it holds.
    assert_eq!(gate.decision(), GateDecision::Hold);
    sb.spool(&[sb.event(1)]);
    std::thread::sleep(Duration::from_millis(600));
    assert!(ingest::spool_usage(&sb.locator.db_dir).bytes < 2_000);
    assert_eq!(gate.decision(), GateDecision::Hold);

    // Hooks pile up more than the bound allows: the hold is over.
    sb.spool(&(2..12).map(|n| sb.event(n)).collect::<Vec<_>>());
    std::thread::sleep(Duration::from_millis(600));
    let usage = ingest::spool_usage(&sb.locator.db_dir);
    assert!(usage.bytes >= 2_000, "{usage:?}");
    assert_eq!(gate.decision(), GateDecision::Withhold);
    let mut db = sb.writer(&keys);
    let report = ingest::import_spool(&mut db, &gate).unwrap();
    assert_eq!(report.accepted, 11);
    assert!(sb.spool_snapshot().is_empty());
    assert!(
        gate.take_notices()
            .iter()
            .any(|n| n.level == NoticeLevel::Error)
    );
}

#[test]
fn a_restart_while_holding_does_not_start_the_day_over() {
    let sb = sandbox(EncryptionMode::Auto);
    sb.make_blobs();
    sb.hide_key();
    let write_state = |since: std::time::Duration| {
        let at = attemptdb_core::Timestamp::from_micros(
            attemptdb_core::Timestamp::now().as_micros() - since.as_micros() as i64,
        );
        let state = EncryptionState {
            db_id: sb.db_id(),
            mode: EncryptionMode::Auto,
            state: "holding".into(),
            since: at.to_rfc3339(),
            key_source: None,
            problems: vec![],
            withheld_events: 0,
            advice: None,
            key_seen: true,
            hold_exhausted: false,
        };
        let path = keys::state_path(&sb.locator, sb.db_id());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, serde_json::to_vec(&state).unwrap()).unwrap();
    };
    // Holding for an hour when the previous writer stopped: still holding.
    write_state(Duration::from_secs(3600));
    assert_eq!(
        sb.gate(EncryptionMode::Auto).gate.decision(),
        GateDecision::Hold
    );
    // Holding for two days: the day is over.
    write_state(Duration::from_secs(2 * 24 * 3600));
    assert_eq!(
        sb.gate(EncryptionMode::Auto).gate.decision(),
        GateDecision::Withhold
    );
}

#[test]
fn the_state_file_an_older_build_wrote_still_reads() {
    // 0.2.13 wrote no `key_seen` and no `hold_exhausted`.
    let json = r#"{"db_id":"00000000-0000-0000-0000-000000000000","mode":"auto","state":"withholding","since":"2026-10-06T00:00:00Z","key_source":null,"problems":[],"withheld_events":640,"advice":null}"#;
    let state: EncryptionState = serde_json::from_str(json).unwrap();
    assert_eq!(state.withheld_events, 640);
    assert!(!state.key_seen && !state.hold_exhausted);
}

#[test]
fn an_import_run_by_hand_stops_instead_of_storing_events_without_content() {
    use attemptdb_capture::import_common::{EventSink, open_import_target};
    let sb = sandbox(EncryptionMode::Auto);
    sb.make_blobs();
    sb.hide_key();
    let mut target = open_import_target(&sb.locator).unwrap();
    let err = target
        .write(vec![sb.event(7)])
        .expect_err("a held gate refuses the batch");
    let text = err.to_string();
    assert!(
        text.contains("content key cannot be read") && text.contains("run the import again"),
        "{text}"
    );
    drop(target);
    assert_eq!(sb.stored().len(), 1, "nothing was stored");
}

#[cfg(unix)]
mod daemon {
    use super::*;
    use attemptdb_capture::daemon::{self, DaemonOptions};
    use attemptdb_capture::hook::{Delivery, HookInput, run_hook};
    use attemptdb_capture::ipc::{Client, IpcError};
    use attemptdb_capture::keys::KEY_UNAVAILABLE_CODE;
    use std::time::Instant;

    fn start(locator: &Locator) -> std::thread::JoinHandle<attemptdb_capture::Result<()>> {
        let l = locator.clone();
        let handle = std::thread::spawn(move || {
            daemon::run(
                &l,
                DaemonOptions {
                    spool_interval: Duration::from_millis(100),
                    key_recheck: Duration::from_millis(50),
                    ..Default::default()
                },
            )
        });
        daemon::wait_until_running(locator, Duration::from_secs(15)).expect("daemon did not start");
        handle
    }

    fn log(locator: &Locator) -> String {
        std::fs::read_to_string(daemon::log_path(locator)).unwrap_or_default()
    }

    fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !done() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    #[test]
    fn the_daemon_refuses_ingest_while_the_key_is_unreadable_and_imports_when_it_reads() {
        let sb = sandbox(EncryptionMode::Auto);
        sb.make_blobs();
        sb.hide_key();
        // A hook that could not reach a daemon left an event in the spool.
        sb.spool(&[sb.event(1)]);
        let handle = start(&sb.locator);

        // Several sweeps later the spool file is still there, untouched.
        std::thread::sleep(Duration::from_millis(500));
        assert_eq!(sb.spool_snapshot().len(), 1);
        assert_eq!(sb.stored().len(), 1, "the database is unchanged");

        // A batch over the socket is refused, retryable, with its own code.
        let refused = Client::send_events(&sb.locator, &[sb.event(2)]).unwrap_err();
        match refused {
            IpcError::Nack(n) => {
                assert_eq!(n.code, KEY_UNAVAILABLE_CODE);
                assert!(n.retryable);
            }
            other => panic!("expected a NACK, got {other:?}"),
        }
        assert_eq!(sb.stored().len(), 1);

        // A hook therefore spools, which is where its event waits.
        let payload = serde_json::json!({
            "hook_event_name": "UserPromptSubmit",
            "session_id": "s-hook",
            "cwd": sb.project.to_string_lossy(),
            "prompt": format!("{SECRET} from a hook"),
        });
        let out = run_hook(HookInput {
            provider_id: "claude-code",
            event_hint: None,
            payload_bytes: serde_json::to_vec(&payload).unwrap(),
            cwd_hint: None,
            data_dir_override: Some(sb.locator.paths.data_dir.clone()),
            db_override: Some(sb.locator.db_dir.clone()),
        });
        assert_eq!(out.delivered, Delivery::Spool, "{:?}", out.error);

        // So does a CLI write (a correction), which cannot take the lock.
        let report = ingest::write_events(&sb.locator, vec![sb.event(3)]).unwrap();
        assert_eq!(report.accepted, 0, "nothing reached the database");

        // The daemon, the doctor and a read say events wait, and why.
        let status = daemon::status(&sb.locator).unwrap();
        assert_eq!(status.extra["content_gate"], "holding");
        assert!(status.extra["events_held"].as_u64().unwrap() >= 1);
        let (db, _, writer_busy) = ingest::open_for_read(&sb.locator).unwrap();
        assert!(writer_busy, "the daemon holds the writer lock");
        assert!(
            db.warnings.iter().any(|w| waits_for_key(w, 3)),
            "{:?}",
            db.warnings
        );
        assert_eq!(db.scan(&ScanFilter::default()).unwrap().len(), 1);
        drop(db);
        let config = Config::load_or_default(&sb.locator.paths.config_dir);
        assert!(waits_for_key(
            &capture_health(&sb.locator, &config).lines().join("\n"),
            3
        ));
        assert_eq!(
            log(&sb.locator).matches("cannot be read right now").count(),
            1,
            "one notice, not one per event:\n{}",
            log(&sb.locator)
        );

        // The key reads again: the daemon notices on its own and imports
        // the three that waited, with their content.
        sb.show_key();
        wait_for("the spool to be imported", || {
            sb.spool_snapshot().is_empty()
        });
        let ack = Client::send_events(&sb.locator, &[sb.event(5)]).unwrap();
        assert_eq!(ack.accepted.len(), 1, "accepted again");
        assert!(daemon::stop(&sb.locator).unwrap());
        handle.join().unwrap().unwrap();

        assert!(!sb.on_disk(SECRET), "encrypted at rest");
        let events = sb.stored();
        // The blob event, the three that waited, and the one sent after.
        assert_eq!(events.len(), 5, "{} stored", events.len());
        for ev in &events {
            assert!(
                ev.content
                    .as_ref()
                    .and_then(|c| c.prompt.as_deref())
                    .is_some_and(|p| p.contains(SECRET)),
                "lost its content: {ev:?}"
            );
            assert!(ev.attrs.get(CONTENT_WITHHELD_ATTR).is_none(), "{ev:?}");
        }
        let log = log(&sb.locator);
        assert!(log.contains("available again"), "{log}");
        assert!(!log.contains("ERROR"), "{log}");
    }

    #[test]
    fn otel_records_wait_in_the_spool_while_the_key_is_unreadable() {
        use attemptdb_capture::otel::{self, ReceiverConfig};
        let sb = sandbox(EncryptionMode::Auto);
        sb.make_blobs();
        sb.hide_key();
        let port = {
            let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            socket.local_addr().unwrap().port()
        };
        let config = ReceiverConfig {
            port,
            token: "c".repeat(32),
        };
        std::fs::write(
            ReceiverConfig::path(&sb.locator),
            serde_json::to_vec(&config).unwrap(),
        )
        .unwrap();
        let handle = start(&sb.locator);
        wait_for("the receiver", || {
            otel::probe(&sb.locator).unwrap()["running"] == true
        });

        let payload = serde_json::json!({"resourceLogs":[{"scopeLogs":[{"logRecords":[{
            "timeUnixNano":"1787904000000000000","attributes":[
            {"key":"event.name","value":{"stringValue":"api_request"}},
            {"key":"session.id","value":{"stringValue":"held-otel"}},
            {"key":"input_tokens","value":{"intValue":"9"}}
        ]}]}]}]});
        let response = ureq::post(&config.endpoint("claude_code", "logs"))
            .set("Authorization", &format!("Bearer {}", config.token))
            .set("Content-Type", "application/json")
            .send_string(&payload.to_string())
            .unwrap();
        assert_eq!(response.status(), 200, "the exporter is not told to retry");
        // It is in the spool, not in the database.
        assert!(!sb.spool_snapshot().is_empty());
        assert_eq!(sb.stored().len(), 1);

        sb.show_key();
        wait_for("the spool to be imported", || {
            sb.spool_snapshot().is_empty()
        });
        assert!(daemon::stop(&sb.locator).unwrap());
        handle.join().unwrap().unwrap();
        let events = sb.stored();
        assert_eq!(events.len(), 2);
        assert!(events.iter().any(|e| e.is_telemetry()));
    }
}
