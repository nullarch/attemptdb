//! Conformance checks for a stream of canonical events (`spec/`).
//!
//! `serde` is the structural check: a line that does not deserialise into
//! [`Event`] is not an event. The rules here are the ones a schema cannot
//! express — identity derivation, per-device ordering, span resolution,
//! capture-mode discipline, the `attrs` contract — evaluated across the whole
//! stream. Pure: no I/O, no dependencies beyond core.

use crate::attrs;
use crate::event::Event;
use crate::ids::{DeviceId, EventId, SessionId, SpanId};
use crate::privacy::CaptureMode;
use crate::schema::CANONICAL_SCHEMA_VERSION;
use serde::Serialize;
use std::collections::{HashMap, HashSet};

/// One rule violation or note, tied to a 1-based line in the input.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Finding {
    pub line: usize,
    pub message: String,
}

/// One section of the report.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Section {
    /// Violations: any one makes the stream non-conformant.
    pub failures: Vec<Finding>,
    /// Observations that do not affect the verdict (things a short stream
    /// cannot demonstrate either way).
    pub notes: Vec<Finding>,
}

impl Section {
    pub fn ok(&self) -> bool {
        self.failures.is_empty()
    }
    fn fail(&mut self, line: usize, message: impl Into<String>) {
        self.failures.push(Finding {
            line,
            message: message.into(),
        });
    }
    fn note(&mut self, line: usize, message: impl Into<String>) {
        self.notes.push(Finding {
            line,
            message: message.into(),
        });
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Report {
    pub schema_version: u16,
    pub lines: usize,
    pub events: usize,
    pub envelope: Section,
    pub identity: Section,
    pub temporal: Section,
    pub causality: Section,
    pub provenance: Section,
    pub extensions: Section,
}

impl Report {
    pub fn compatible(&self) -> bool {
        self.sections().iter().all(|(_, s)| s.ok())
    }

    /// Sections in display order.
    pub fn sections(&self) -> Vec<(&'static str, &Section)> {
        vec![
            ("Envelope", &self.envelope),
            ("Identity", &self.identity),
            ("Temporal", &self.temporal),
            ("Causality", &self.causality),
            ("Provenance", &self.provenance),
            ("Extensions", &self.extensions),
        ]
    }

    pub fn failure_count(&self) -> usize {
        self.sections().iter().map(|(_, s)| s.failures.len()).sum()
    }
}

/// Check newline-delimited JSON. Blank lines are skipped.
pub fn check_jsonl(text: &str) -> Report {
    let mut report = Report {
        schema_version: CANONICAL_SCHEMA_VERSION,
        ..Default::default()
    };
    let mut events: Vec<(usize, Event)> = Vec::new();
    for (idx, line) in text.lines().enumerate() {
        let n = idx + 1;
        if line.trim().is_empty() {
            continue;
        }
        report.lines += 1;
        match serde_json::from_str::<Event>(line) {
            Ok(ev) => events.push((n, ev)),
            Err(e) => report
                .envelope
                .fail(n, format!("does not parse as an Event: {e}")),
        }
    }
    check_events(&events, &mut report);
    report
}

/// Check already-parsed events; `line` is only used to label findings.
pub fn check_parsed(events: &[Event]) -> Report {
    let mut report = Report {
        schema_version: CANONICAL_SCHEMA_VERSION,
        lines: events.len(),
        ..Default::default()
    };
    let numbered: Vec<(usize, Event)> = events
        .iter()
        .cloned()
        .enumerate()
        .map(|(i, e)| (i + 1, e))
        .collect();
    check_events(&numbered, &mut report);
    report
}

fn check_events(events: &[(usize, Event)], r: &mut Report) {
    r.events = events.len();

    // --- Envelope ---------------------------------------------------------
    let mut seen_ids: HashMap<EventId, usize> = HashMap::new();
    for (n, ev) in events {
        if ev.schema_version != CANONICAL_SCHEMA_VERSION {
            r.envelope.fail(
                *n,
                format!(
                    "schema_version {} (this checker is v{CANONICAL_SCHEMA_VERSION})",
                    ev.schema_version
                ),
            );
        }
        if ev.event_id.is_nil() {
            r.envelope.fail(*n, "event_id is nil");
        } else if let Some(first) = seen_ids.insert(ev.event_id, *n) {
            r.envelope.fail(
                *n,
                format!("event_id {} already used on line {first}", ev.event_id),
            );
        }
        if ev.device_id.is_nil() {
            r.envelope.fail(*n, "device_id is nil");
        }
    }

    // --- Identity ---------------------------------------------------------
    for (n, ev) in events {
        if ev.provider.as_str().is_empty() {
            r.identity.fail(*n, "provider is empty");
        }
        if ev.provider_session_id.is_empty() {
            r.identity.fail(*n, "provider_session_id is empty");
        } else {
            let expected = SessionId::derive(&[ev.provider.as_str(), &ev.provider_session_id]);
            if ev.session_id != expected {
                r.identity.fail(
                    *n,
                    format!(
                        "session_id {} is not derived from (provider, provider_session_id); expected {expected}",
                        ev.session_id
                    ),
                );
            }
        }
        if ev.project.project_id.is_nil() {
            r.identity.fail(*n, "project.project_id is nil");
        }
        if ev.project.root.is_empty() {
            r.identity.fail(*n, "project.root is empty");
        }
        if ev.project.name.is_empty() {
            r.identity.fail(*n, "project.name is empty");
        }
        if ev.agent.agent_id.is_nil() {
            r.identity.note(
                *n,
                "agent.agent_id absent (allowed; derived ids are recommended)",
            );
        }
    }

    // --- Temporal ---------------------------------------------------------
    let mut last_seq: HashMap<DeviceId, (u64, usize)> = HashMap::new();
    let mut last_hlc: HashMap<DeviceId, (u64, usize)> = HashMap::new();
    let mut unassigned = 0usize;
    for (n, ev) in events {
        if ev.observed_at.as_micros() <= 0 {
            r.temporal
                .fail(*n, "observed_at is not a positive timestamp");
        }
        if ev.captured_at.as_micros() < 0 {
            r.temporal.fail(*n, "captured_at is negative");
        }
        if ev.source_seq == 0 && ev.hlc.as_u64() == 0 {
            unassigned += 1;
            continue;
        }
        if ev.source_seq != 0 {
            match last_seq.get(&ev.device_id) {
                Some((prev, prev_line)) if ev.source_seq <= *prev => r.temporal.fail(
                    *n,
                    format!(
                        "source_seq {} does not increase after {} (line {prev_line}) for device {}",
                        ev.source_seq, prev, ev.device_id
                    ),
                ),
                Some((prev, _)) if ev.source_seq != prev + 1 => r.temporal.note(
                    *n,
                    format!(
                        "source_seq gap for device {}: {} -> {}",
                        ev.device_id, prev, ev.source_seq
                    ),
                ),
                _ => {}
            }
            last_seq.insert(ev.device_id, (ev.source_seq, *n));
        }
        if ev.hlc.as_u64() != 0 {
            if let Some((prev, prev_line)) = last_hlc.get(&ev.device_id)
                && ev.hlc.as_u64() <= *prev
            {
                r.temporal.fail(
                    *n,
                    format!(
                        "hlc {} does not increase after {} (line {prev_line}) for device {}",
                        ev.hlc.as_u64(),
                        prev,
                        ev.device_id
                    ),
                );
            }
            last_hlc.insert(ev.device_id, (ev.hlc.as_u64(), *n));
        }
    }
    if unassigned > 0 {
        r.temporal.note(
            0,
            format!("{unassigned} event(s) carry no source_seq/hlc (not yet ingested); ordering not checked for them"),
        );
    }

    // --- Causality --------------------------------------------------------
    let spans: HashSet<SpanId> = events.iter().filter_map(|(_, e)| e.span_id).collect();
    let mut started_calls: HashSet<(SessionId, String)> = HashSet::new();
    for (n, ev) in events {
        if let Some(parent) = ev.parent_span_id
            && !spans.contains(&parent)
        {
            r.causality.fail(
                *n,
                format!("parent_span_id {parent} does not resolve to a span_id in the stream"),
            );
        }
        if let Some(tool) = &ev.tool
            && let Some(call_id) = &tool.call_id
        {
            let key = (ev.session_id, call_id.clone());
            match ev.kind.as_str() {
                "tool_call_started" => {
                    started_calls.insert(key);
                }
                "tool_call_finished" | "tool_call_failed" if !started_calls.contains(&key) => {
                    r.causality.note(
                        *n,
                        format!(
                            "{} for call {call_id} has no preceding tool_call_started (normal for providers without a pre-call hook)",
                            ev.kind.as_str()
                        ),
                    );
                }
                _ => {}
            }
        }
    }

    // --- Provenance -------------------------------------------------------
    for (n, ev) in events {
        if ev.adapter_version.is_empty() {
            r.provenance.fail(*n, "adapter_version is empty");
        }
        if ev.provider_event_name.is_empty() {
            r.provenance.fail(*n, "provider_event_name is empty");
        }
        if ev.capture_mode == CaptureMode::MetadataOnly {
            if ev.content.is_some() {
                r.provenance.fail(*n, "content present under metadata_only");
            }
            if ev.raw.is_some() {
                r.provenance.fail(*n, "raw present under metadata_only");
            }
        }
        if ev.kind.as_str() == "unknown" {
            r.provenance.note(
                *n,
                format!(
                    "kind unknown for provider event {:?} (kept verbatim; consider an adapter mapping)",
                    ev.provider_event_name
                ),
            );
        }
    }

    // --- Extensions -------------------------------------------------------
    for (n, ev) in events {
        for (key, value) in &ev.attrs {
            if !attrs::key_allowed(key) {
                r.extensions.fail(
                    *n,
                    format!("attrs.{key} is neither allowlisted nor x_<provider>_<name>"),
                );
                continue;
            }
            let bad_string = |s: &str| !attrs::value_allowed(s);
            let bad = match value {
                serde_json::Value::String(s) => bad_string(s),
                serde_json::Value::Array(items) => items
                    .iter()
                    .any(|v| matches!(v, serde_json::Value::String(s) if bad_string(s))),
                serde_json::Value::Object(inner) if key == "provider" => inner
                    .values()
                    .any(|v| matches!(v, serde_json::Value::String(s) if bad_string(s))),
                serde_json::Value::Object(_) => true,
                _ => false,
            };
            if bad {
                r.extensions.fail(
                    *n,
                    format!("attrs.{key} carries a content-shaped value (long, multi-line, email, or home path)"),
                );
            }
        }
        for key in ev.unknown.keys() {
            let namespaced = key.strip_prefix("x_").is_some_and(|rest| {
                !rest.is_empty()
                    && rest
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
            });
            if !namespaced {
                r.extensions.fail(
                    *n,
                    format!("top-level key {key:?} is not part of Event v1 and not namespaced as x_<name>"),
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{EventContent, EventKind, ProjectRef, Provider, ToolCategory, ToolRef};
    use crate::{AgentId, Timestamp};

    fn event(device: DeviceId, session: &str) -> Event {
        Event::new(
            device,
            Provider::ClaudeCode,
            "PostToolUse",
            EventKind::ToolCallFinished,
            ProjectRef::derive("/home/dev/example/project", None, &device),
            session,
            CaptureMode::LocalSemantic,
            "test/0.1",
        )
    }

    fn assigned(mut ev: Event, seq: u64) -> Event {
        ev.source_seq = seq;
        ev.hlc = crate::Hlc::new(1_700_000_000_000 + seq, 0);
        ev
    }

    #[test]
    fn a_clean_stream_is_compatible() {
        let d = DeviceId::derive(&["test", "d1"]);
        let events: Vec<Event> = (1..=3).map(|i| assigned(event(d, "s1"), i)).collect();
        let text: String = events
            .iter()
            .map(|e| serde_json::to_string(e).unwrap() + "\n")
            .collect();
        let r = check_jsonl(&text);
        assert!(r.compatible(), "{r:?}");
        assert_eq!(r.events, 3);
        assert_eq!(r.failure_count(), 0);
    }

    #[test]
    fn violations_land_in_their_sections() {
        let d = DeviceId::derive(&["test", "d1"]);
        let mut a = assigned(event(d, "s1"), 5);
        let mut b = assigned(event(d, "s1"), 4); // goes backwards
        b.session_id = SessionId::derive(&["wrong", "s1"]);
        b.parent_span_id = Some(SpanId::derive(&["nowhere"]));
        b.capture_mode = CaptureMode::MetadataOnly;
        b.content = Some(EventContent {
            command: Some("ls".into()),
            ..Default::default()
        });
        b.attrs
            .insert("prompt".into(), serde_json::json!("secret prompt"));
        b.unknown
            .insert("vendor_field".into(), serde_json::json!(1));
        a.event_id = b.event_id; // duplicate id
        let text = format!(
            "{}\n{}\n{{not json}}\n",
            serde_json::to_string(&a).unwrap(),
            serde_json::to_string(&b).unwrap()
        );
        let r = check_jsonl(&text);
        assert!(!r.compatible());
        assert!(!r.envelope.ok(), "duplicate id + unparseable line");
        assert_eq!(r.envelope.failures.len(), 2);
        assert!(!r.identity.ok());
        assert!(!r.temporal.ok());
        assert!(!r.causality.ok());
        assert!(!r.provenance.ok());
        assert!(!r.extensions.ok());
        assert_eq!(r.extensions.failures.len(), 2, "{:?}", r.extensions);
    }

    #[test]
    fn short_streams_get_notes_not_failures() {
        let d = DeviceId::derive(&["test", "d1"]);
        let mut ev = event(d, "s1"); // unassigned: no seq/hlc
        ev.tool = Some(ToolRef {
            name: "Bash".into(),
            category: ToolCategory::Shell,
            call_id: Some("toolu_1".into()),
        });
        ev.agent.agent_id = AgentId::nil();
        ev.observed_at = Timestamp::from_micros(1);
        let r = check_parsed(&[ev]);
        assert!(r.compatible(), "{r:?}");
        assert!(!r.temporal.notes.is_empty());
        assert!(!r.causality.notes.is_empty());
        assert!(!r.identity.notes.is_empty());
    }
}
