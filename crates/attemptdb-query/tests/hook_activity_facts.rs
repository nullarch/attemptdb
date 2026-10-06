//! The per-provider hook activity `attempt doctor` reports — live hook
//! events, the latest capture, whether a capture test was stored, telemetry
//! by signal — read from the segment columns exactly as from decoded
//! events, and merged across slices without losing any of it.

use attemptdb_core::{
    CaptureMode, DeviceId, Event, EventId, EventKind, ProjectRef, Timestamp, event::Provider,
};
use attemptdb_query::facts::StreamFacts;
use serde_json::json;

fn event(device: DeviceId, kind: EventKind, captured_s: i64) -> Event {
    let mut e = Event::new(
        device,
        Provider::ClaudeCode,
        "PostToolUse",
        kind,
        ProjectRef::derive("/home/dev/example/project", None, &device),
        "fixture-session",
        CaptureMode::MetadataOnly,
        "fixture",
    );
    e.event_id = EventId::new();
    e.observed_at = Timestamp::from_micros(captured_s * 1_000_000);
    e.captured_at = Timestamp::from_micros(captured_s * 1_000_000);
    e
}

fn stream() -> Vec<Event> {
    let device = DeviceId::new();
    let test = event(device, EventKind::CaptureTest, 50);
    let mut imported = event(device, EventKind::ToolCallFinished, 90);
    imported.attrs.insert("reconstructed".into(), json!(true));
    // Stream order is not time order: the later capture comes first.
    let late = event(device, EventKind::ToolCallFinished, 70);
    let early = event(device, EventKind::ToolCallStarted, 60);
    let otel = |signal: Option<&str>, at: i64| {
        let mut e = event(device, EventKind::Unknown, at);
        e.attrs.insert("source".into(), json!("otel"));
        if let Some(s) = signal {
            e.attrs.insert("x_otel_signal".into(), json!(s));
        }
        e
    };
    vec![
        test,
        late,
        otel(Some("metrics"), 80),
        imported,
        early,
        otel(Some("metrics"), 75),
        otel(None, 40),
    ]
}

fn check(f: &StreamFacts) {
    let p = &f.providers["claude_code"];
    assert_eq!(p.events, 7);
    assert_eq!(p.hook_events, 2, "no capture test, import or telemetry");
    assert_eq!(
        p.last_hook_captured_at,
        Some(Timestamp::from_micros(70_000_000)),
        "the latest live capture, not the reconstructed one at 90 s"
    );
    assert!(p.capture_test_seen);
    assert_eq!(p.telemetry.len(), 2, "{:?}", p.telemetry);
    assert_eq!(p.telemetry["metrics"].events, 2);
    assert_eq!(
        p.telemetry["metrics"].last_observed_at,
        Some(Timestamp::from_micros(80_000_000))
    );
    assert_eq!(p.telemetry["unknown"].events, 1);
}

#[test]
fn columns_and_events_agree_on_hook_activity() {
    let events = stream();
    check(&StreamFacts::from_events(&events));
    check(&StreamFacts::from_batches(
        &attemptdb_storage::segment::events_to_batches(&events).unwrap(),
    ));
}

#[test]
fn absorbing_slices_keeps_hook_activity() {
    let events = stream();
    for cut in 0..=events.len() {
        let mut merged = StreamFacts::from_events(&events[..cut]);
        merged.absorb(&StreamFacts::from_events(&events[cut..]));
        check(&merged);
    }
}

#[test]
fn a_provider_seen_only_through_its_capture_test_has_no_hook_events() {
    let events = vec![event(DeviceId::new(), EventKind::CaptureTest, 10)];
    let f = StreamFacts::from_events(&events);
    let p = &f.providers["claude_code"];
    assert!(p.capture_test_seen);
    assert_eq!(p.hook_events, 0);
    assert_eq!(p.last_hook_captured_at, None);
}
