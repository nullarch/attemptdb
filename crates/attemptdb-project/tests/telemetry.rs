use attemptdb_core::{
    CaptureMode, DeviceId, Event, EventId, EventKind, ProjectRef, Timestamp, event::Provider,
};
use attemptdb_project::{IncrementalProjector, project};
use serde_json::json;

#[test]
fn periodic_telemetry_never_reopens_a_finished_session_or_changes_work_inference() {
    let device = DeviceId::new();
    let mut event = Event::new(
        device,
        Provider::Codex,
        "SessionStart",
        EventKind::SessionStarted,
        ProjectRef::derive("/home/dev/example/project", None, &device),
        "fixture",
        CaptureMode::MetadataOnly,
        "fixture",
    );
    event.observed_at = Timestamp::from_micros(1_000_000);
    let mut end = event.clone();
    end.event_id = EventId::new();
    end.kind = EventKind::SessionEnded;
    end.observed_at = Timestamp::from_micros(2_000_000);
    let mut metric = event.clone();
    metric.event_id = EventId::new();
    metric.kind = EventKind::Unknown;
    metric.observed_at = Timestamp::from_micros(999_000_000);
    metric.attrs.insert("source".into(), json!("otel"));
    metric
        .attrs
        .insert("x_otel_signal".into(), json!("metrics"));
    let base = project([&event, &end]);
    let all = project([&event, &end, &metric]);
    assert_eq!(
        serde_json::to_value(&base.sessions).unwrap(),
        serde_json::to_value(&all.sessions).unwrap()
    );
    assert_eq!(all.stats.events_seen, 3);
    let mut inc = IncrementalProjector::new();
    inc.push(&event);
    inc.push(&end);
    inc.snapshot();
    assert!(inc.push(&metric));
    assert!(!inc.push(&metric));
    assert_eq!(inc.pending_sessions(), 0);
    assert_eq!(inc.len(), 3);
    assert_eq!(
        serde_json::to_value(inc.snapshot()).unwrap(),
        serde_json::to_value(all).unwrap()
    );
    assert!(project([&metric]).sessions.is_empty());
}
