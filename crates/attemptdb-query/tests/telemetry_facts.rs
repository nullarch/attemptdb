use attemptdb_core::{
    CaptureMode, DeviceId, Event, EventId, EventKind, ProjectRef, Timestamp, event::Provider,
};
use attemptdb_query::facts::StreamFacts;
use serde_json::json;

#[test]
fn telemetry_counts_as_ingestion_but_does_not_invent_sessions_projects_or_live_work() {
    let device = DeviceId::new();
    let mut hook = Event::new(
        device,
        Provider::Codex,
        "SessionStart",
        EventKind::SessionStarted,
        ProjectRef::derive("/home/dev/example/project", None, &device),
        "fixture-session",
        CaptureMode::MetadataOnly,
        "fixture",
    );
    hook.observed_at = Timestamp::from_micros(1_000_000);
    let mut metric = hook.clone();
    metric.event_id = EventId::new();
    metric.kind = EventKind::Unknown;
    metric.observed_at = Timestamp::from_micros(9_000_000);
    metric.project = ProjectRef::derive("otel/unattributed", None, &device);
    metric.attrs.insert("source".into(), json!("otel"));
    metric
        .attrs
        .insert("x_otel_signal".into(), json!("metrics"));
    let events = vec![metric.clone(), hook.clone()];
    let a = StreamFacts::from_events(&events);
    let b =
        StreamFacts::from_batches(&attemptdb_storage::segment::events_to_batches(&events).unwrap());
    for f in [&a, &b] {
        assert_eq!(f.events, 2);
        assert_eq!(f.providers["codex"].events, 2);
        assert_eq!(f.providers["codex"].last_event_at, Some(hook.observed_at));
        assert_eq!(f.sessions.len(), 1);
        assert_eq!(f.sessions[0].1.project_id, hook.project.project_id);
        assert_eq!(f.projects.len(), 1);
        assert_eq!(f.last_event_at, Some(hook.observed_at));
        assert_eq!(f.devices[&(device, false)].events, 2);
    }
    let only = StreamFacts::from_events(&[metric]);
    assert!(only.sessions.is_empty() && only.projects.is_empty() && only.last_event.is_none());
}
