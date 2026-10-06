//! `encryption = auto | off | required` where a database is opened for
//! writing: the daemon, the CLI's spool import, and the doctor's view.
//!
//! The keys come from key files under the sandbox data directory; the
//! tests never touch the OS key store for anything but a lookup that finds
//! nothing.

use attemptdb_capture::config::{Config, EncryptionMode};
use attemptdb_capture::doctor::capture_health;
use attemptdb_capture::keys::{self, CONTENT_WITHHELD_ATTR, InitOptions, KeyStoreOptions};
use attemptdb_capture::{Locator, ingest};
use attemptdb_core::event::{EventContent, Provider};
use attemptdb_core::{CaptureMode, DeviceId, Event, EventKind, ProjectRef};
use attemptdb_storage::{Database, Identity, OpenOptions, ScanFilter, SpoolWriter};
use std::path::Path;

const SECRET: &str = "plaintext-must-not-reach-disk-7731";

struct Sandbox {
    _tmp: tempfile::TempDir,
    locator: Locator,
    device: DeviceId,
}

fn sandbox(mode: EncryptionMode) -> Sandbox {
    // Short prefix: a daemon socket path must fit sun_path.
    let tmp = tempfile::Builder::new().prefix("atdb").tempdir().unwrap();
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

    fn init_key(&self) {
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
}

fn assert_withheld(ev: &Event) {
    assert!(ev.content.is_none() && ev.raw.is_none(), "{ev:?}");
    assert_eq!(ev.capture_mode, CaptureMode::MetadataOnly);
    assert_eq!(
        ev.attrs.get(CONTENT_WITHHELD_ATTR),
        Some(&serde_json::json!("no_key"))
    );
}

#[test]
fn the_cli_spool_import_withholds_content_when_a_required_key_is_missing() {
    let sb = sandbox(EncryptionMode::Required);
    sb.spool(&[sb.event(1), sb.event(2)]);
    let (mut db, report, read_only) = ingest::open_fresh(&sb.locator, false).unwrap();
    assert!(!read_only);
    assert_eq!(report.unwrap().accepted, 2);
    // `attempt status` prints these.
    assert!(
        db.warnings
            .iter()
            .any(|w| w.contains("encryption is required")),
        "{:?}",
        db.warnings
    );
    db.flush().unwrap();
    drop(db);
    assert!(!sb.on_disk(SECRET), "plaintext reached the database files");
    let events = sb.stored();
    assert_eq!(events.len(), 2);
    events.iter().for_each(assert_withheld);
    // Imported means gone from the spool.
    assert!(
        !ingest::open_fresh(&sb.locator, false)
            .unwrap()
            .0
            .stats()
            .spool_pending
    );
    // The doctor's view of it.
    let config = Config::load_or_default(&sb.locator.paths.config_dir);
    let health = capture_health(&sb.locator, &config);
    assert!(health.has_problem());
    let lines = health.lines().join("\n");
    assert!(
        lines.contains("encryption") && lines.contains("no key"),
        "{lines}"
    );
}

#[test]
fn cli_writes_withhold_content_too() {
    let sb = sandbox(EncryptionMode::Required);
    let report = ingest::write_events(&sb.locator, vec![sb.event(7)]).unwrap();
    assert_eq!(report.accepted, 1);
    assert!(!sb.on_disk(SECRET));
    sb.stored().iter().for_each(assert_withheld);
}

#[test]
fn the_same_spool_is_stored_with_content_once_a_key_exists() {
    let sb = sandbox(EncryptionMode::Required);
    sb.init_key();
    sb.spool(&[sb.event(1)]);
    let (mut db, _, _) = ingest::open_fresh(&sb.locator, false).unwrap();
    assert!(db.warnings.is_empty(), "{:?}", db.warnings);
    db.flush().unwrap();
    drop(db);
    assert!(!sb.on_disk(SECRET), "encrypted at rest");
    let events = sb.stored();
    assert!(
        events[0]
            .content
            .as_ref()
            .unwrap()
            .prompt
            .as_ref()
            .unwrap()
            .contains(SECRET)
    );
    let config = Config::load_or_default(&sb.locator.paths.config_dir);
    assert!(!capture_health(&sb.locator, &config).has_problem());
}

#[test]
fn off_stores_plaintext_even_beside_a_key() {
    let sb = sandbox(EncryptionMode::Off);
    sb.init_key();
    sb.spool(&[sb.event(1)]);
    let (mut db, _, _) = ingest::open_fresh(&sb.locator, false).unwrap();
    db.flush().unwrap();
    assert_eq!(db.blob_stats().unwrap().count, 0, "no blobs under off");
    drop(db);
    assert!(sb.on_disk(SECRET), "off means inline, as documented");
}

#[test]
fn a_broken_config_is_reported_by_the_doctor_and_open_writer() {
    let sb = sandbox(EncryptionMode::Auto);
    std::fs::write(
        Config::path(&sb.locator.paths.config_dir),
        br#"{"capture_mode":"metadata-only"}"#,
    )
    .unwrap();
    let config = Config::load_or_default(&sb.locator.paths.config_dir);
    let health = capture_health(&sb.locator, &config);
    assert!(health.has_problem());
    assert!(
        health.lines()[0].starts_with("config"),
        "{:?}",
        health.lines()
    );
    let db = ingest::open_writer(&sb.locator, false).unwrap();
    assert!(
        db.warnings.iter().any(|w| w.contains("metadata only")),
        "{:?}",
        db.warnings
    );
}

#[cfg(unix)]
mod daemon {
    use super::*;
    use attemptdb_capture::daemon::{self, DaemonOptions};
    use attemptdb_capture::ipc::Client;
    use std::time::{Duration, Instant};

    fn start(locator: &Locator) -> std::thread::JoinHandle<attemptdb_capture::Result<()>> {
        let l = locator.clone();
        let handle = std::thread::spawn(move || {
            daemon::run(
                &l,
                DaemonOptions {
                    spool_interval: Duration::from_millis(100),
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

    #[test]
    fn the_daemon_withholds_content_logs_it_once_and_leaves_a_state_file() {
        let sb = sandbox(EncryptionMode::Required);
        // A hook that could not reach a daemon left content in the spool.
        sb.spool(&[sb.event(1)]);
        let handle = start(&sb.locator);
        // A hook that did reach it.
        let ack = Client::send_events(&sb.locator, &[sb.event(2), sb.event(3)]).unwrap();
        assert_eq!(ack.accepted.len(), 2);
        // The periodic sweep imports another spooled event.
        sb.spool(&[sb.event(4)]);
        let deadline = Instant::now() + Duration::from_secs(10);
        while !sb
            .locator
            .db_dir
            .join("spool")
            .read_dir()
            .unwrap()
            .flatten()
            .all(|e| e.path().extension().and_then(|x| x.to_str()) != Some("spool"))
            && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(daemon::stop(&sb.locator).unwrap());
        handle.join().unwrap().unwrap();

        assert!(!sb.on_disk(SECRET), "plaintext reached the database files");
        let events = sb.stored();
        assert_eq!(events.len(), 4);
        events.iter().for_each(assert_withheld);

        let log = log(&sb.locator);
        assert_eq!(
            log.matches("encryption is required").count(),
            1,
            "one loud error, not one per event:\n{log}"
        );
        assert!(log.contains("ERROR"), "{log}");
        let state = keys::read_state(&sb.locator, sb.db_id()).expect("state file");
        assert_eq!(state.state, "withholding");
        assert_eq!(state.withheld_events, 4);
    }

    #[test]
    fn a_daemon_with_a_key_stores_content_and_says_nothing() {
        let sb = sandbox(EncryptionMode::Required);
        sb.init_key();
        let handle = start(&sb.locator);
        Client::send_events(&sb.locator, &[sb.event(1)]).unwrap();
        assert!(daemon::stop(&sb.locator).unwrap());
        handle.join().unwrap().unwrap();
        assert!(!sb.on_disk(SECRET), "encrypted at rest");
        assert!(sb.stored()[0].content.is_some());
        assert!(!log(&sb.locator).contains("ERROR"), "{}", log(&sb.locator));
    }
}
