//! The read path decodes what it is asked for, and answers exactly as the
//! full load did: facts read from a few columns, scoped engines read from
//! their own rows, telemetry records never decoded into events.
//!
//! The reference is always the straightforward computation over every
//! decoded event (`Database::scan`, `StreamFacts::from_events`,
//! `attemptdb_project::project`), on a database that mixes what a long-lived
//! one holds: several projects and providers, retractions and corrections,
//! reconstructed events, a capture test, and enough OpenTelemetry records to
//! fill several Arrow batches between the real events, spread over several
//! segments and a WAL tail.

mod common;

use attemptdb_core::event::{EventContent, Provider};
use attemptdb_core::{
    CaptureMode, DeviceId, Event, EventId, EventKind, ProjectRef, SessionId, Timestamp,
};
use attemptdb_project::project;
use attemptdb_query::facts::{FACT_COLUMNS, StreamFacts};
use attemptdb_query::{EngineCache, QueryEngine};
use attemptdb_storage::segment::BATCH_ROWS;
use attemptdb_storage::{Database, OpenOptions, ScanFilter};
use common::{Sess, Stream, Tool, at, spec_scenario};
use serde_json::{Value, json};

fn ses(sess: &Sess) -> String {
    format!("ses_{}", sess.session_id)
}

/// A telemetry record: `kind = unknown`, `attrs.source = "otel"`.
fn otel(device: DeviceId, n: usize, signal: Option<Value>, at_micros: i64) -> Event {
    let mut ev = Event::new(
        device,
        if n.is_multiple_of(3) {
            Provider::Codex
        } else {
            Provider::ClaudeCode
        },
        "codex.sqlite.logs.write",
        EventKind::Unknown,
        ProjectRef::derive("otel/unattributed", None, &device),
        format!("otel-session-{}", n % 7),
        CaptureMode::LocalSemantic,
        "otel/1",
    );
    ev.event_id = EventId::derive(&["otel-event", &n.to_string()]);
    ev.observed_at = Timestamp::from_micros(at_micros);
    ev.captured_at = ev.observed_at;
    ev.attrs.insert("source".into(), json!("otel"));
    if let Some(s) = signal {
        ev.attrs.insert("x_otel_signal".into(), s);
    }
    ev.attrs.insert("x_otel_record_type".into(), json!("log"));
    ev.raw = Some(json!({"body": format!("record {n}")}));
    ev
}

/// Events that only look like telemetry.
fn lookalikes(device: DeviceId, base: i64) -> Vec<Event> {
    let mk = |n: usize, kind: EventKind, attrs: Value| {
        let mut ev = Event::new(
            device,
            Provider::Codex,
            "lookalike",
            kind,
            ProjectRef::derive("/work/lookalike", None, &device),
            "lookalike-session",
            CaptureMode::LocalSemantic,
            "test/1",
        );
        ev.event_id = EventId::derive(&["lookalike", &n.to_string()]);
        ev.observed_at = Timestamp::from_micros(base + n as i64);
        ev.captured_at = ev.observed_at;
        for (k, v) in attrs.as_object().unwrap() {
            ev.attrs.insert(k.clone(), v.clone());
        }
        ev
    };
    vec![
        // source=otel but a real kind: a hook event, not telemetry.
        mk(0, EventKind::ToolCallFinished, json!({"source": "otel"})),
        // unknown, but another source.
        mk(1, EventKind::Unknown, json!({"source": "hook"})),
        // The marker only inside the nested provider object.
        mk(
            2,
            EventKind::Unknown,
            json!({"source": "other", "provider": {"source": "otel"}}),
        ),
        // Telemetry whose attrs carry a nested object (forces the full parse).
        mk(
            3,
            EventKind::Unknown,
            json!({"source": "otel", "x_otel_signal": "traces", "provider": {"k": "v"}}),
        ),
        // Telemetry whose strings need escapes.
        mk(
            4,
            EventKind::Unknown,
            json!({"source": "otel", "x_otel_signal": "metrics", "reason": "say \"hi\" \\ there"}),
        ),
        // Signal that is not a string.
        mk(
            5,
            EventKind::Unknown,
            json!({"source": "otel", "x_otel_signal": 5}),
        ),
        // No signal at all.
        mk(6, EventKind::Unknown, json!({"source": "otel"})),
        // A kind a newer version might write, with the marker: decodes as
        // `unknown` and is telemetry for the projector.
        mk(
            7,
            EventKind::Unknown,
            json!({"source": "otel", "x_otel_signal": "logs"}),
        ),
    ]
}

struct Mixed {
    events: Vec<Event>,
    project_a: attemptdb_core::ProjectId,
    codex_session: SessionId,
}

fn mixed_events() -> Mixed {
    mixed_events_sized(3 * BATCH_ROWS + 500)
}

fn mixed_events_sized(telemetry_total: usize) -> Mixed {
    let sc = spec_scenario();
    let device = DeviceId::derive(&["test-device"]);
    let mut b = Stream::new();
    b.events = sc.events.clone();
    // A second project with its own sessions (event ids made distinct).
    let other = {
        let mut o = Stream::new();
        let s = Sess::claude("other-1");
        let c = Sess::codex("other-2");
        o.session_started(&s, at(1_000));
        o.prompt(&s, at(1_005), "Refactor the lexer");
        let edit = ["src/lexer.rs"];
        o.tool_start(&s, at(1_006), &Tool::edit(Some("o1"), &edit));
        o.tool_failed(
            &s,
            at(1_007),
            &Tool::edit(Some("o1"), &edit),
            "string_mismatch",
        );
        o.session_started(&c, at(1_100));
        o.prompt(&c, at(1_101), "Add tests");
        o.stop(&c, at(1_120));
        let other = ProjectRef::derive(
            "/work/other",
            Some("git@github.com:acme/other.git"),
            &device,
        );
        o.events
            .into_iter()
            .map(|mut ev| {
                ev.project = other.clone();
                ev.event_id = EventId::derive(&["other", &ev.event_id.to_string()]);
                ev
            })
            .collect::<Vec<_>>()
    };
    b.events.extend(other);
    // A transcript-reconstructed event and a capture test.
    let mut rec = b.events[3].clone();
    rec.event_id = EventId::derive(&["reconstructed"]);
    rec.attrs.insert("reconstructed".into(), json!(true));
    rec.observed_at = at(450);
    b.events.push(rec);
    let mut test = b.events[0].clone();
    test.event_id = EventId::derive(&["capture-test"]);
    test.kind = EventKind::CaptureTest;
    test.provider_event_name = "CaptureTest".into();
    test.content = Some(EventContent::default());
    test.observed_at = at(2_000);
    b.events.push(test);
    // A correction and a retraction of a whole session.
    let a11 = {
        let a = project(&sc.events)
            .attempts
            .iter()
            .find(|a| a.session_id == sc.claude.session_id && a.turn_index == 1 && a.index == 1)
            .expect("attempt")
            .attempt_id;
        format!("att_{a}")
    };
    b.correction(
        &sc.claude,
        at(400),
        "attempt_outcome",
        &a11,
        Some("failed"),
        Some("wrong_fix"),
        Some("broke the other tests"),
    );
    b.retraction(
        &sc.codex,
        at(410),
        "session",
        &ses(&sc.codex),
        "benchmark",
        Some("benchmark run"),
    );
    // The builder numbers its events from 1 again: give the two it just made
    // ids of their own, or the database drops them as duplicates.
    let n = b.events.len();
    b.events[n - 2].event_id = EventId::derive(&["meta", "correction"]);
    b.events[n - 1].event_id = EventId::derive(&["meta", "retraction"]);
    let mut events = b.build();
    events.extend(lookalikes(device, 1_787_904_000_000_000 + 3_000_000_000));

    // Telemetry between the real events: enough to span several batches of
    // every segment, with the signals spelled the ways they occur.
    let base = 1_787_904_000_000_000i64;
    let signals = [
        Some(json!("logs")),
        Some(json!("metrics")),
        Some(json!("traces")),
        None,
    ];
    let total = telemetry_total;
    let mut out = Vec::with_capacity(events.len() + total);
    let step = (events.len() / 12).max(1);
    let mut telemetry = (0..total).map(|n| {
        otel(
            device,
            n,
            signals[n % signals.len()].clone(),
            base + (n as i64) * 1_000_000,
        )
    });
    for (i, ev) in events.into_iter().enumerate() {
        out.push(ev);
        if i % step == 0 {
            out.extend(telemetry.by_ref().take(total / 12));
        }
    }
    out.extend(telemetry);
    Mixed {
        project_a: ProjectRef::derive(common::ROOT, Some("git@github.com:acme/repo.git"), &device)
            .project_id,
        codex_session: sc.codex.session_id,
        events: out,
    }
}

/// Three flushes' worth of segments and a WAL tail.
fn build_db(root: &std::path::Path, events: &[Event]) -> Database {
    build_db_with(root, events, None)
}

fn build_db_with(
    root: &std::path::Path,
    events: &[Event],
    keys: Option<std::sync::Arc<dyn attemptdb_storage::KeyProvider>>,
) -> Database {
    let mut db = Database::open(
        root,
        OpenOptions {
            create: true,
            flush_events: usize::MAX,
            flush_bytes: usize::MAX,
            keys,
            ..Default::default()
        },
    )
    .unwrap();
    let n = events.len();
    db.ingest(events[..n / 3].to_vec()).unwrap();
    db.flush().unwrap();
    db.ingest(events[n / 3..2 * n / 3].to_vec()).unwrap();
    db.flush().unwrap();
    db.ingest(events[2 * n / 3..n - 20].to_vec()).unwrap();
    db.flush().unwrap();
    db.ingest(events[n - 20..].to_vec()).unwrap(); // WAL tail
    assert_eq!(db.stats().segments, 3);
    assert_eq!(db.stats().memtable_rows, 20);
    db
}

fn reader(root: &std::path::Path) -> Database {
    Database::open(
        root,
        OpenOptions {
            read_only: true,
            ..Default::default()
        },
    )
    .unwrap()
}

fn assert_facts_equal(got: &StreamFacts, want: &StreamFacts, what: &str) {
    assert_eq!(got.events, want.events, "{what}: events");
    assert_eq!(
        got.reconstructed, want.reconstructed,
        "{what}: reconstructed"
    );
    assert_eq!(got.projects, want.projects, "{what}: projects");
    assert_eq!(got.providers, want.providers, "{what}: providers");
    assert_eq!(got.sessions, want.sessions, "{what}: sessions");
    assert_eq!(got.devices, want.devices, "{what}: devices");
    assert_eq!(
        got.last_event_at, want.last_event_at,
        "{what}: last_event_at"
    );
    assert_eq!(got.last_event, want.last_event, "{what}: last_event");
}

/// `attempt status` and `doctor` read facts from a few columns. They must say
/// what decoding every event says: per provider, per project, per session,
/// per device, telemetry receipts, last seen — retractions and all.
#[test]
fn facts_from_a_few_columns_are_the_facts_of_every_event() {
    let mixed = mixed_events();
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("db");
    drop(build_db(&root, &mixed.events));
    let db = reader(&root);

    // The reference: every event decoded.
    let all = db.scan(&ScanFilter::default()).unwrap();
    assert_eq!(all.len(), mixed.events.len());
    let want = StreamFacts::from_events(&all);
    assert!(
        want.providers.values().any(|p| !p.telemetry.is_empty()),
        "the fixture has telemetry receipts"
    );
    assert!(want.providers.values().any(|p| p.capture_test_seen));
    assert!(want.reconstructed > 0);

    // The facts path: listed, then a projected read of FACT_COLUMNS.
    let mut cache = EngineCache::new();
    let refreshed = cache.refresh_lazy(&db, "db").unwrap();
    let got = cache.facts(&refreshed).unwrap();
    assert_eq!(cache.stats().decodes, 0, "no segment decoded in full");
    assert!(refreshed.segments.iter().all(|s| !s.is_resident()));
    assert_facts_equal(&got, &want, "projected read");

    // The same facts from the full batches (the old path) and from batches
    // that hold only the fact columns.
    let mut full = StreamFacts::default();
    let mut narrow = StreamFacts::default();
    for seg in &refreshed.segments {
        for b in seg.batches().unwrap().iter() {
            full.push_batch(b);
        }
        seg.read_columns(FACT_COLUMNS, &mut |b| {
            assert!(b.num_columns() <= FACT_COLUMNS.len());
            narrow.push_batch(&b);
            Ok(true)
        })
        .unwrap();
    }
    // `absorb` merges per segment, `push_batch` over the whole stream: the
    // counts agree either way.
    assert_eq!(narrow.events, full.events);
    assert_eq!(narrow.projects, full.projects);
    assert_eq!(narrow.sessions, full.sessions);
}

/// Every scope a command can ask for: the engine built from the scope's own
/// rows projects exactly what projecting the scope's decoded events does,
/// and its `events` table holds the same rows.
#[tokio::test]
async fn scoped_engines_project_exactly_what_a_full_scan_does() {
    let mixed = mixed_events();
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("db");
    drop(build_db(&root, &mixed.events));
    let db = reader(&root);
    let scenario = spec_scenario();
    let t = |secs: i64| Timestamp::from_micros(1_787_904_000_000_000 + secs * 1_000_000);
    let filters: Vec<(&str, ScanFilter)> = vec![
        ("everything", ScanFilter::default()),
        (
            "project",
            ScanFilter {
                project_id: Some(mixed.project_a),
                ..Default::default()
            },
        ),
        (
            "session",
            ScanFilter {
                session_id: Some(scenario.claude.session_id),
                ..Default::default()
            },
        ),
        (
            "the retracted session",
            ScanFilter {
                session_id: Some(mixed.codex_session),
                ..Default::default()
            },
        ),
        (
            "since",
            ScanFilter {
                since: Some(t(40)),
                ..Default::default()
            },
        ),
        (
            "window",
            ScanFilter {
                project_id: Some(mixed.project_a),
                since: Some(t(5)),
                until: Some(t(300)),
                ..Default::default()
            },
        ),
        (
            "captured only",
            ScanFilter {
                captured_only: true,
                ..Default::default()
            },
        ),
        (
            "provider",
            ScanFilter {
                providers: vec!["codex".into()],
                ..Default::default()
            },
        ),
        (
            "newest 25",
            ScanFilter {
                limit: Some(25),
                ..Default::default()
            },
        ),
    ];
    for lazy in [true, false] {
        for (name, filter) in &filters {
            // The eager refresh (the server's) shares the scoped path; a few
            // scopes show it.
            if !lazy && !["everything", "project", "newest 25"].contains(name) {
                continue;
            }
            let mut cache = EngineCache::new();
            let refreshed = if lazy {
                cache.refresh_lazy(&db, "db").unwrap()
            } else {
                cache.refresh(&db, "db").unwrap()
            };
            let engine = cache.engine_scoped(&refreshed, filter).unwrap();
            let events = db.scan(filter).unwrap();
            let want = project(&events);
            assert_eq!(
                engine.event_count(),
                events.len(),
                "{name} (lazy {lazy}): rows"
            );
            assert_eq!(
                serde_json::to_value(engine.projection()).unwrap(),
                serde_json::to_value(&want).unwrap(),
                "{name} (lazy {lazy}): projection"
            );
            if *name == "everything" {
                let n = engine
                    .sql("SELECT count(*) AS n FROM events WHERE retracted")
                    .await
                    .unwrap()
                    .to_json()[0]["n"]
                    .as_u64()
                    .unwrap();
                assert!(n > 0, "the fixture retracts a session");
            }
            // The `events` table holds the scope's rows, flagged the same way.
            let reference = QueryEngine::from_events(events.clone()).await.unwrap();
            for sql in [
                "SELECT count(*) AS n FROM events",
                "SELECT count(*) AS n FROM events WHERE retracted",
                "SELECT kind, count(*) AS n FROM events GROUP BY kind ORDER BY kind",
                "SELECT provider, project_name, count(*) AS n FROM events GROUP BY 1, 2 ORDER BY 1, 2",
                // Narrow projections are converted column by column; these
                // reach the id, dictionary, flag and content conversions.
                "SELECT event_id, session_id, kind, retracted FROM events ORDER BY event_id",
                "SELECT retracted FROM events ORDER BY event_id",
                "SELECT content_json FROM events WHERE kind = 'prompt_submitted' ORDER BY event_id",
                "SELECT count(*) AS n FROM events WHERE content_json IS NOT NULL",
                "SELECT * FROM events ORDER BY event_id LIMIT 7",
                "SELECT * FROM events_raw ORDER BY event_id LIMIT 3",
                "SELECT kind, tool_name FROM events_raw ORDER BY event_id LIMIT 11",
            ] {
                assert_eq!(
                    engine.sql(sql).await.unwrap().to_json(),
                    reference.sql(sql).await.unwrap().to_json(),
                    "{name} (lazy {lazy}): {sql}"
                );
            }
        }
    }
}

/// The projector is fed only the rows it reads: telemetry records are left
/// out as Arrow before they are decoded, yet every count that counted them
/// still does.
#[test]
fn the_projector_never_sees_telemetry_but_the_counts_still_do() {
    let mixed = mixed_events();
    let telemetry = mixed.events.iter().filter(|e| e.is_telemetry()).count();
    assert!(telemetry > 3 * BATCH_ROWS, "several batches of telemetry");
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("db");
    drop(build_db(&root, &mixed.events));
    let db = reader(&root);

    let mut cache = EngineCache::new();
    let refreshed = cache.refresh_lazy(&db, "db").unwrap();
    let projection = cache.snapshot_for(&refreshed).unwrap();
    assert_eq!(
        cache.projector().len(),
        mixed.events.len() - telemetry,
        "telemetry never reached the projector"
    );
    assert_eq!(
        projection.stats.events_seen,
        mixed.events.len() as u64,
        "events_seen still counts every stored event"
    );
    assert_eq!(cache.stats().events, mixed.events.len());
    let want = project(&db.scan(&ScanFilter::default()).unwrap());
    assert_eq!(
        serde_json::to_value(&projection).unwrap(),
        serde_json::to_value(&want).unwrap()
    );
    // A second refresh over the unchanged database feeds nothing again.
    let again = cache.refresh_lazy(&db, "db").unwrap();
    let p2 = cache.snapshot_for(&again).unwrap();
    assert_eq!(p2.stats.events_seen, mixed.events.len() as u64);
}

/// A session prefix that fits several sessions, or is too short to mean
/// anything, no longer picks one; neither does a project name two projects
/// share (REPORT.md §7.8).
#[test]
fn ambiguous_and_too_short_arguments_are_refused() {
    use attemptdb_query::facts::ResolveError;
    let device = DeviceId::derive(&["resolve"]);
    let mk = |root: &str, remote: Option<&str>, psid: &str, n: usize| {
        let mut ev = Event::new(
            device,
            Provider::ClaudeCode,
            "SessionStart",
            EventKind::SessionStarted,
            ProjectRef::derive(root, remote, &device),
            psid,
            CaptureMode::MetadataOnly,
            "test/1",
        );
        ev.event_id = EventId::derive(&["resolve", &n.to_string()]);
        ev.observed_at = Timestamp::from_micros(1_000_000 + n as i64);
        ev
    };
    let events = vec![
        mk("/w/a/app", None, "abcd-0001", 1),
        mk("/w/b/app", None, "abcd-0002", 2),
        mk(
            "/w/c/tool",
            Some("git@github.com:acme/tool.git"),
            "zzzz-0003",
            3,
        ),
    ];
    let f = StreamFacts::from_events(&events);

    // Sessions: whole ids work, a unique prefix works, shared prefixes list
    // their candidates, and `0` is not an id.
    let one = f.resolve_session("abcd-0001").unwrap();
    assert_eq!(f.session(&one).unwrap().provider_session_id, "abcd-0001");
    assert_eq!(f.resolve_session("zzzz").unwrap(), f.sessions[2].0);
    assert_eq!(f.resolve_session(&format!("{}", one)).unwrap(), one);
    match f.resolve_session("abcd").unwrap_err() {
        ResolveError::Ambiguous {
            candidates, total, ..
        } => {
            assert_eq!(total, 2);
            assert_eq!(candidates.len(), 2);
            assert!(
                candidates.iter().all(|c| c.contains("abcd-000")),
                "{candidates:?}"
            );
        }
        other => panic!("expected an ambiguity, got {other:?}"),
    }
    for short in ["0", "a", "ab", "ses_", "ses_0", "abc"] {
        assert!(
            matches!(f.resolve_session(short), Err(ResolveError::TooShort { .. })),
            "{short:?}"
        );
    }
    assert!(matches!(
        f.resolve_session("nothing-like-it"),
        Err(ResolveError::Unknown { .. })
    ));
    let text = f.resolve_session("abcd").unwrap_err().to_string();
    assert!(
        text.contains("matches 2 sessions") && text.contains("more"),
        "{text}"
    );

    // Projects: an id, a root, a remote and a unique name resolve; a name two
    // projects share is ambiguous and lists them.
    let a = ProjectRef::derive("/w/a/app", None, &device).project_id;
    assert_eq!(f.resolve_project(&a.to_string()).unwrap(), a);
    assert_eq!(f.resolve_project("/w/a/app").unwrap(), a);
    let tool =
        ProjectRef::derive("/w/c/tool", Some("git@github.com:acme/tool.git"), &device).project_id;
    assert_eq!(f.resolve_project("github.com/acme/tool").unwrap(), tool);
    assert_eq!(f.resolve_project("acme/tool").unwrap(), tool);
    match f.resolve_project("app").unwrap_err() {
        ResolveError::Ambiguous {
            candidates, total, ..
        } => {
            assert_eq!(total, 2);
            assert!(candidates.iter().all(|c| c.starts_with("app (prj_")));
        }
        other => panic!("expected an ambiguity, got {other:?}"),
    }
    assert!(matches!(
        f.resolve_project("missing"),
        Err(ResolveError::Unknown { .. })
    ));
}

/// With an encryption key the segments are format 2: content lives in blobs
/// and the batches hold only refs. The scoped read must resolve exactly what
/// the full scan resolves (prompt text feeds the projection), and the
/// `events` table must show content only to statements that project it.
#[tokio::test]
async fn encrypted_segments_read_the_same_through_every_path() {
    use attemptdb_storage::KeyProvider;
    use attemptdb_storage::blobs::StaticKeyProvider;
    let mixed = mixed_events_sized(150);
    let keys = || -> std::sync::Arc<dyn KeyProvider> {
        let mut p = StaticKeyProvider::new();
        p.set_current([7u8; 32]);
        std::sync::Arc::new(p)
    };
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("db");
    drop(build_db_with(&root, &mixed.events, Some(keys())));
    let db = Database::open(
        &root,
        OpenOptions {
            read_only: true,
            keys: Some(keys()),
            ..Default::default()
        },
    )
    .unwrap();
    for seg in &db.manifest().segments {
        let path = root.join("segments").join(&seg.file);
        assert_eq!(
            attemptdb_storage::segment::segment_format_version(&path).unwrap(),
            2,
            "content is in blobs"
        );
    }
    let all = db.scan(&ScanFilter::default()).unwrap();
    assert!(
        all.iter()
            .any(|e| e.content.as_ref().is_some_and(|c| c.prompt.is_some())),
        "the key resolves prompt text"
    );
    let want_facts = StreamFacts::from_events(&all);
    for filter in [
        ScanFilter::default(),
        ScanFilter {
            project_id: Some(mixed.project_a),
            ..Default::default()
        },
    ] {
        let mut cache = EngineCache::new();
        let refreshed = cache.refresh_lazy(&db, "db").unwrap();
        assert_facts_equal(&cache.facts(&refreshed).unwrap(), &want_facts, "format 2");
        let engine = cache.engine_scoped(&refreshed, &filter).unwrap();
        let events = db.scan(&filter).unwrap();
        assert_eq!(
            serde_json::to_value(engine.projection()).unwrap(),
            serde_json::to_value(project(&events)).unwrap(),
            "projection over blobs"
        );
        let reference = QueryEngine::from_events(events).await.unwrap();
        for sql in [
            "SELECT count(*) AS n FROM events",
            "SELECT content_json FROM events WHERE kind = 'prompt_submitted' ORDER BY event_id",
            "SELECT event_id, content_json IS NOT NULL AS has_content FROM events ORDER BY event_id LIMIT 20",
            "SELECT raw_json FROM events WHERE raw_json IS NOT NULL ORDER BY event_id LIMIT 5",
        ] {
            assert_eq!(
                engine.sql(sql).await.unwrap().to_json(),
                reference.sql(sql).await.unwrap().to_json(),
                "{sql}"
            );
        }
    }
}
