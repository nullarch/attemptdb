//! An import that goes through the spool (the daemon holds the writer lock)
//! does not wait for the daemon's periodic sweep: it asks the daemon to import
//! the spool after every batch it queues, so docs/history-import.md can say it
//! never waits. The daemon here sweeps once an hour, so only the request can
//! get the events in.
//!
//! Unix only: the daemon is driven over its Unix socket.
#![cfg(unix)]

use attemptdb_capture::config::Config;
use attemptdb_capture::daemon::{self, DaemonOptions};
use attemptdb_capture::import_codex::{discover_rollouts, import_codex_rollouts};
use attemptdb_capture::import_common::{SpoolSink, import_device, open_import_target};
use attemptdb_capture::{Locator, ingest};
use attemptdb_storage::ScanFilter;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

struct Sandbox {
    _tmp: tempfile::TempDir,
    locator: Locator,
    sessions: PathBuf,
}

fn sandbox() -> Sandbox {
    // Short prefix: the socket path must fit sun_path.
    let tmp = tempfile::Builder::new().prefix("atdb").tempdir().unwrap();
    let project = tmp.path().join("proj");
    std::fs::create_dir_all(&project).unwrap();
    let locator = Locator::resolve(&project, Some(&tmp.path().join("data")), None);
    // A few small rollouts, each a session of its own.
    let day = tmp.path().join("sessions/2026/08/28");
    std::fs::create_dir_all(&day).unwrap();
    let template = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/transcripts/codex/modern_turn.jsonl"),
    )
    .unwrap();
    for i in 0..6 {
        let id = format!("{i:08x}-2222-4222-8222-222222222222");
        std::fs::write(
            day.join(format!("rollout-2026-08-28T08-00-00-{id}.jsonl")),
            template.replace("22222222-2222-4222-8222-222222222222", &id),
        )
        .unwrap();
    }
    Sandbox {
        sessions: tmp.path().join("sessions"),
        _tmp: tmp,
        locator,
    }
}

fn start(locator: &Locator) -> std::thread::JoinHandle<attemptdb_capture::Result<()>> {
    let l = locator.clone();
    let handle = std::thread::spawn(move || {
        daemon::run(
            &l,
            DaemonOptions {
                // The periodic sweep never happens within the test.
                spool_interval: Duration::from_secs(3600),
                ..Default::default()
            },
        )
    });
    daemon::wait_until_running(locator, Duration::from_secs(15)).expect("daemon did not start");
    handle
}

fn stored(locator: &Locator) -> usize {
    ingest::open_reader(locator)
        .and_then(|db| Ok(db.scan(&ScanFilter::default())?.len()))
        .unwrap_or(0)
}

fn wait_for(locator: &Locator, want: usize, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if stored(locator) >= want {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    stored(locator) >= want
}

#[test]
fn a_queued_import_asks_the_daemon_to_import_it_now() {
    let sb = sandbox();
    let handle = start(&sb.locator);
    let mut target = open_import_target(&sb.locator).unwrap();
    assert!(target.is_spool(), "the daemon holds the writer lock");
    let device = import_device(&sb.locator, &target).unwrap();
    let sources = discover_rollouts(std::slice::from_ref(&sb.sessions));
    assert_eq!(sources.len(), 6);

    let started = Instant::now();
    let summary = import_codex_rollouts(&mut target, &sources, &Config::default(), device).unwrap();
    assert_eq!(summary.queued, summary.events_seen);
    assert!(summary.queued > 100, "{summary:?}");
    assert!(
        wait_for(&sb.locator, summary.queued, Duration::from_secs(10)),
        "the daemon never imported the queued events ({} of {} after {:?}); the hourly sweep \
         is the only other way in",
        stored(&sb.locator),
        summary.queued,
        started.elapsed()
    );

    assert!(daemon::stop(&sb.locator).unwrap());
    handle.join().unwrap().unwrap();
    // Nothing was stored twice.
    assert_eq!(stored(&sb.locator), summary.queued);
}

#[test]
fn a_busy_inbox_drains_at_the_speed_of_the_daemon_not_of_its_sweep() {
    let sb = sandbox();
    let handle = start(&sb.locator);
    let device = {
        let target = open_import_target(&sb.locator).unwrap();
        assert!(target.is_spool(), "the daemon holds the writer lock");
        import_device(&sb.locator, &target).unwrap()
    };
    // A sink that waits for the inbox to drain after a batch (any inbox is
    // "high"), for as long as 8 s. The daemon's hourly sweep would never
    // answer; the request does.
    let mut sink = SpoolSink::with_limits(&sb.locator, 1, Duration::from_secs(8)).unwrap();
    let sources = discover_rollouts(std::slice::from_ref(&sb.sessions));
    let started = Instant::now();
    let summary = import_codex_rollouts(&mut sink, &sources, &Config::default(), device).unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(6),
        "the import waited {:?} for the daemon",
        started.elapsed()
    );
    assert!(wait_for(
        &sb.locator,
        summary.queued,
        Duration::from_secs(10)
    ));
    assert!(daemon::stop(&sb.locator).unwrap());
    handle.join().unwrap().unwrap();
}
