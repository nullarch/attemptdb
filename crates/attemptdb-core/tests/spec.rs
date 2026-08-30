//! The published schema and the implementation must not drift.
//!
//! Direction 1 — implementation → schema: a fully populated `Event`, and
//! every golden fixture the adapters produce, must validate against
//! `spec/event-v1.schema.json`.
//! Direction 2 — schema → implementation: every golden must also pass the
//! conformance checks, which deserialise through `Event` itself.

use attemptdb_core::conformance;
use attemptdb_core::event::{
    AgentRef, EventContent, EventKind, Outcome, OutcomeStatus, ProjectRef, Provider, ToolCategory,
    ToolRef,
};
use attemptdb_core::{AgentId, CaptureMode, DeviceId, Event, PortablePath, SpanId, Timestamp};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

fn schema() -> jsonschema::Validator {
    let text = std::fs::read_to_string(repo_root().join("spec/event-v1.schema.json")).unwrap();
    let schema: Value = serde_json::from_str(&text).unwrap();
    jsonschema::validator_for(&schema).expect("spec/event-v1.schema.json is a valid schema")
}

fn errors(v: &jsonschema::Validator, instance: &Value) -> Vec<String> {
    v.iter_errors(instance)
        .map(|e| format!("{} at {}", e, e.instance_path))
        .collect()
}

/// Every optional field set, every nested object present.
fn populated() -> Event {
    let device = DeviceId::derive(&["spec", "device"]);
    let mut ev = Event::new(
        device,
        Provider::ClaudeCode,
        "PostToolUse",
        EventKind::ToolCallFinished,
        ProjectRef::derive(
            "/home/dev/example/project",
            Some("github.com/example/project"),
            &device,
        ),
        "session-1",
        CaptureMode::LocalSemantic,
        "spec-test/0.1",
    );
    ev.source_seq = 7;
    ev.hlc = attemptdb_core::Hlc::new(1_700_000_000_000, 3);
    ev.ingested_at = Some(Timestamp::from_micros(1_700_000_000_000_000));
    ev.provider_version = Some("2.1.0".into());
    ev.hook_version = Some("0.1.0".into());
    ev.provider_turn_id = Some("turn-9".into());
    ev.span_id = Some(SpanId::derive(&["span", "1"]));
    ev.parent_span_id = Some(SpanId::derive(&["span", "0"]));
    ev.project.branch = Some("main".into());
    ev.project.head = Some("0123abcd".into());
    ev.agent = AgentRef {
        agent_id: AgentId::derive(&["agent", "1"]),
        provider_agent_id: Some("agent-abc".into()),
        agent_type: Some("Explore".into()),
        parent_agent_id: Some(AgentId::derive(&["agent", "0"])),
        model: Some("claude-sonnet-4-6".into()),
    };
    ev.tool = Some(ToolRef {
        name: "Edit".into(),
        category: ToolCategory::FileEdit,
        call_id: Some("toolu_01".into()),
    });
    ev.paths = vec![PortablePath::from_raw(
        "/home/dev/example/project/src/lib.rs",
        Some("/home/dev/example/project"),
    )];
    ev.outcome = Some(Outcome {
        status: OutcomeStatus::Failure,
        class: Some("string_mismatch".into()),
        exit_code: Some(1),
    });
    ev.duration_ms = Some(41);
    ev.attrs
        .insert("hook_event_name".into(), json!("PostToolUse"));
    ev.attrs.insert("file_ext".into(), json!("rs"));
    ev.attrs.insert("lines_added".into(), json!(3));
    ev.attrs.insert("x_claude_code_custom".into(), json!(true));
    ev.attrs.insert(
        "provider".into(),
        json!({"tool_use_id": "toolu_01", "n": 2}),
    );
    ev.content = Some(EventContent {
        prompt: None,
        command: None,
        message: Some("done".into()),
        error: Some("old_string not found".into()),
        tool_input: Some(json!({"file_path": "src/lib.rs", "old_string": "a", "new_string": "b"})),
        tool_output: Some(json!("ok")),
        ..Default::default()
    });
    ev.raw = Some(json!({"hook_event_name": "PostToolUse", "tool_name": "Edit"}));
    ev.unknown.insert("x_vendor_note".into(), json!("kept"));
    ev
}

#[test]
fn populated_event_validates_and_round_trips() {
    let v = schema();
    let ev = populated();
    let value = serde_json::to_value(&ev).unwrap();
    let errs = errors(&v, &value);
    assert!(
        errs.is_empty(),
        "schema rejected the implementation's own output:\n{}",
        errs.join("\n")
    );
    let back: Event = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(back, ev, "round trip changed the event");
    assert_eq!(back.unknown["x_vendor_note"], "kept");
}

#[test]
fn minimal_event_validates() {
    let v = schema();
    let device = DeviceId::derive(&["spec", "device"]);
    let ev = Event::new(
        device,
        Provider::Other("acme_agent".into()),
        "turn",
        EventKind::Unknown,
        ProjectRef::derive("/home/dev/p", None, &device),
        "s",
        CaptureMode::MetadataOnly,
        "acme/1",
    );
    let value = serde_json::to_value(&ev).unwrap();
    let errs = errors(&v, &value);
    assert!(errs.is_empty(), "{}", errs.join("\n"));
}

#[test]
fn schema_rejects_what_the_standard_forbids() {
    let v = schema();
    let base = serde_json::to_value(populated()).unwrap();
    let mutate = |f: &dyn Fn(&mut Value)| {
        let mut x = base.clone();
        f(&mut x);
        x
    };
    let cases: Vec<(&str, Value)> = vec![
        (
            "wrong schema_version",
            mutate(&|x| x["schema_version"] = json!(2)),
        ),
        ("unknown kind", mutate(&|x| x["kind"] = json!("mystery"))),
        (
            "bad capture_mode",
            mutate(&|x| x["capture_mode"] = json!("everything")),
        ),
        (
            "non-uuid event_id",
            mutate(&|x| x["event_id"] = json!("ev_1")),
        ),
        (
            "string timestamp",
            mutate(&|x| x["observed_at"] = json!("2026-08-30T00:00:00Z")),
        ),
        (
            "unnamespaced top-level key",
            mutate(&|x| x["vendor"] = json!(1)),
        ),
        (
            "uppercase attrs key",
            mutate(&|x| x["attrs"]["Prompt"] = json!("x")),
        ),
        (
            "bad tool category",
            mutate(&|x| x["tool"]["category"] = json!("hammer")),
        ),
        (
            "missing project name",
            mutate(&|x| {
                x["project"].as_object_mut().unwrap().remove("name");
            }),
        ),
    ];
    for (name, instance) in cases {
        assert!(!v.is_valid(&instance), "schema accepted: {name}");
    }
}

#[test]
fn every_golden_fixture_validates_and_conforms() {
    let v = schema();
    let root = repo_root().join("fixtures/providers");
    let mut checked = 0;
    let mut all: Vec<Event> = Vec::new();
    for provider in std::fs::read_dir(&root).unwrap() {
        let dir = provider.unwrap().path();
        if !dir.is_dir() {
            continue;
        }
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if !path.to_string_lossy().ends_with(".golden.json") {
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap();
            let value: Value = serde_json::from_str(&text).unwrap();
            let errs = errors(&v, &value);
            assert!(errs.is_empty(), "{}:\n{}", path.display(), errs.join("\n"));
            let ev: Event = serde_json::from_value(value).unwrap();
            all.push(ev);
            checked += 1;
        }
    }
    assert!(
        checked > 50,
        "only {checked} goldens found under {}",
        root.display()
    );
    // Goldens are zeroed for stability (nil ids, zero clocks), which the
    // envelope rules flag on purpose; every other section must be clean.
    let report = conformance::check_parsed(&all);
    for (name, section) in report.sections() {
        if name == "Envelope" {
            continue;
        }
        assert!(
            section.ok(),
            "{name} failures over goldens:\n{}",
            section
                .failures
                .iter()
                .map(|f| format!("  line {}: {}", f.line, f.message))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }
}
