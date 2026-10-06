//! A reader that serves a live database judges sessions against the wall
//! clock; a test or a replay pins an instant. Either way `sessions.state` is
//! a function of an explicit instant, never of the newest event alone.

use attemptdb_core::event::Provider;
use attemptdb_core::{CaptureMode, DeviceId, Event, EventKind, ProjectRef, Timestamp};
use attemptdb_project::SessionStatus;
use attemptdb_query::{EngineCache, QueryEngine};
use attemptdb_storage::{Database, OpenOptions};

const MIN: i64 = 60 * 1_000_000;

fn db_with_a_silent_session(dir: &std::path::Path, last_event: Timestamp) -> Database {
    let dev = DeviceId::derive(&["clock"]);
    let mut db = Database::open(
        dir,
        OpenOptions {
            create: true,
            device_id: Some(dev),
            ..Default::default()
        },
    )
    .unwrap();
    let project = ProjectRef::derive("/home/dev/example/project", None, &dev);
    let mut events = Vec::new();
    for (kind, name, back_min) in [
        (EventKind::SessionStarted, "SessionStart", 3),
        (EventKind::PromptSubmitted, "UserPromptSubmit", 2),
        (EventKind::Notification, "Notification", 0),
    ] {
        let mut e = Event::new(
            dev,
            Provider::ClaudeCode,
            name,
            kind,
            project.clone(),
            "clock-session",
            CaptureMode::MetadataOnly,
            "clock-test/0",
        );
        e.observed_at = Timestamp::from_micros(last_event.as_micros() - back_min * MIN);
        e.captured_at = e.observed_at;
        events.push(e);
    }
    db.ingest(events).unwrap();
    db.flush().unwrap();
    db
}

#[tokio::test]
async fn a_database_read_today_judges_an_old_session_stale() {
    let tmp = tempfile::tempdir().unwrap();
    let long_ago = Timestamp::from_micros(Timestamp::now().as_micros() - 14 * 24 * 60 * MIN);
    let db = db_with_a_silent_session(tmp.path(), long_ago);

    // The throwaway cache behind `QueryEngine::from_database` reads the
    // wall clock: fourteen days of silence is not "open".
    let e = QueryEngine::from_database(&db, &Default::default())
        .await
        .unwrap();
    let r = e.sql("SELECT state FROM sessions").await.unwrap();
    assert_eq!(r.to_json()[0]["state"], "stale");

    // A cache pinned at the newest event judges it as the stream shows it.
    let mut cache = EngineCache::new().with_as_of(long_ago);
    let refreshed = cache.refresh(&db, "db").unwrap();
    assert_eq!(cache.snapshot().sessions[0].state, SessionStatus::Open);
    let pinned = cache.engine(&refreshed).unwrap();
    let r = pinned.sql("SELECT state FROM sessions").await.unwrap();
    assert_eq!(r.to_json()[0]["state"], "open");

    // And pinned an hour later it is stale, with the capped confidence.
    let mut cache =
        EngineCache::new().with_as_of(Timestamp::from_micros(long_ago.as_micros() + 60 * MIN));
    cache.refresh(&db, "db").unwrap();
    let p = cache.snapshot();
    assert_eq!(p.sessions[0].state, SessionStatus::Stale);
    assert!(p.sessions[0].confidence() <= 0.7);
}
