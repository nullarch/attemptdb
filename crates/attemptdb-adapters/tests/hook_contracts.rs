//! Authored payloads based on the official hook references (2026-09-06).
//! These are contract tests, not evidence that a provider ran on this machine.
use attemptdb_adapters::{CaptureContext, adapter_for, all_adapters};
use attemptdb_core::event::Provider;
use attemptdb_core::{
    CaptureMode, DeviceId, Event, EventKind, OutcomeStatus, ProjectRef, Timestamp,
};
use serde_json::{Value, json};

fn normalise(provider: Provider, mode: CaptureMode, mut payload: Value) -> Event {
    payload["_fixture_note"] = "Authored from official hook documentation; no private data.".into();
    let device_id = DeviceId::nil();
    let ctx = CaptureContext {
        device_id,
        capture_mode: mode,
        project: ProjectRef::derive("/home/dev/example/project", None, &device_id),
        captured_at: Timestamp::from_micros(1_787_904_000_000_000),
        provider_version: None,
        hook_version: None,
    };
    let event = adapter_for(&provider)
        .unwrap()
        .normalise(&ctx, None, &payload)
        .unwrap();
    let mut attrs = event.attrs.clone();
    assert_eq!(
        attemptdb_core::attrs::sanitise(&mut attrs),
        0,
        "{:?}",
        event.attrs
    );
    event
}

#[test]
fn every_passive_subscription_has_semantics_and_no_duplicate_name() {
    for adapter in all_adapters() {
        let mut names = std::collections::HashSet::new();
        for name in adapter.capture_events() {
            assert!(names.insert(name));
            assert!(adapter.supported_events().contains(name));
            let event = normalise(
                adapter.provider(),
                CaptureMode::MetadataOnly,
                json!({
                    "hook_event_name": name, "session_id": "s", "conversation_id": "s"
                }),
            );
            assert_ne!(
                event.kind,
                EventKind::Unknown,
                "{} {name}",
                adapter.provider()
            );
        }
    }
    let claude = adapter_for(&Provider::ClaudeCode).unwrap();
    assert!(claude.supported_events().contains(&"WorktreeCreate"));
    assert!(!claude.capture_events().contains(&"WorktreeCreate"));
    let cursor = adapter_for(&Provider::Cursor).unwrap();
    assert!(!cursor.capture_events().contains(&"afterShellExecution"));
    assert!(!cursor.capture_events().contains(&"afterFileEdit"));
}

#[test]
fn cursor_generic_tools_preserve_pairing_turns_failures_and_test_counts() {
    let mut payload = json!({
        "hook_event_name": "preToolUse", "conversation_id": "conversation-1",
        "generation_id": "generation-1", "tool_use_id": "call-1", "tool_name": "Shell",
        "tool_input": {"command": "cargo test"}, "duration": 120,
    });
    let start = normalise(Provider::Cursor, CaptureMode::MetadataOnly, payload.clone());
    payload["hook_event_name"] = "postToolUse".into();
    payload["tool_output"] = json!({
        "exitCode": 101,
        "stdout": "test result: FAILED. 2 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s\nCANARY_OUTPUT"
    }).to_string().into();
    let end = normalise(Provider::Cursor, CaptureMode::MetadataOnly, payload.clone());
    assert_eq!(start.kind, EventKind::ToolCallStarted);
    assert_eq!(end.kind, EventKind::ToolCallFailed);
    assert_eq!(
        start.tool.as_ref().unwrap().call_id,
        end.tool.as_ref().unwrap().call_id
    );
    assert_eq!(end.provider_turn_id.as_deref(), Some("generation-1"));
    assert_eq!(end.outcome.as_ref().unwrap().exit_code, Some(101));
    assert_eq!(end.attrs["tests_failed"], 1);
    assert_eq!(end.duration_ms, Some(120));
    assert!(end.content.is_none() && end.raw.is_none());
    assert!(!serde_json::to_string(&end).unwrap().contains("CANARY"));
    let content = normalise(Provider::Cursor, CaptureMode::LocalSemantic, payload);
    assert!(content.content.unwrap().tool_output.unwrap().is_object());
}

#[test]
fn claude_lifecycle_notifications_preserve_metadata_without_form_answers() {
    let instructions = normalise(
        Provider::ClaudeCode,
        CaptureMode::MetadataOnly,
        json!({
            "hook_event_name": "InstructionsLoaded", "session_id": "s",
            "file_path": "/home/dev/example/project/CLAUDE.md",
            "memory_type": "Project", "load_reason": "nested_traversal",
        }),
    );
    assert_eq!(instructions.kind, EventKind::Notification);
    assert_eq!(
        instructions.attrs["notification_type"],
        "instructions_loaded"
    );
    assert_eq!(
        instructions.paths[0].logical,
        "/home/dev/example/project/CLAUDE.md"
    );
    assert_eq!(
        instructions.attrs["provider"]["load_reason"],
        "nested_traversal"
    );
    for (name, kind) in [
        ("Elicitation", "agent_needs_input"),
        ("ElicitationResult", "elicitation_result"),
    ] {
        let payload = json!({"hook_event_name": name, "session_id": "s", "action": "accept",
            "mode": "form", "message": "CANARY_MESSAGE", "content": {"answer": "CANARY_ANSWER"}});
        let event = normalise(
            Provider::ClaudeCode,
            CaptureMode::MetadataOnly,
            payload.clone(),
        );
        assert_eq!(event.attrs["notification_type"], kind);
        assert!(!serde_json::to_string(&event).unwrap().contains("CANARY"));
        let local = normalise(Provider::ClaudeCode, CaptureMode::LocalSemantic, payload);
        assert_eq!(
            local.content.unwrap().extra["elicitation_content"]["answer"],
            "CANARY_ANSWER"
        );
    }
}

#[test]
fn codex_compaction_and_interrupt_are_not_silent_unknowns() {
    for (name, kind) in [
        ("PreCompact", EventKind::CompactionStarted),
        ("PostCompact", EventKind::CompactionFinished),
        ("Interrupt", EventKind::TurnFailed),
    ] {
        let event = normalise(
            Provider::Codex,
            CaptureMode::MetadataOnly,
            json!({
                "hook_event_name": name, "session_id": "s", "turn_id": "t", "trigger": "auto"
            }),
        );
        assert_eq!(event.kind, kind);
        assert_eq!(event.provider_turn_id.as_deref(), Some("t"));
        if name == "Interrupt" {
            assert_eq!(
                event.outcome.as_ref().unwrap().status,
                OutcomeStatus::Cancelled
            );
            assert_eq!(
                event.outcome.as_ref().unwrap().class.as_deref(),
                Some("interrupted")
            );
        }
    }
}

#[test]
fn compaction_summary_is_content_even_when_raw_payload_is_not_retained() {
    let payload = json!({"hook_event_name": "PostCompact", "session_id": "s",
        "trigger": "auto", "compact_summary": "CANARY_SUMMARY"});
    let mut local = normalise(
        Provider::ClaudeCode,
        CaptureMode::LocalSemantic,
        payload.clone(),
    );
    local.raw = None;
    assert_eq!(
        local.content.unwrap().extra["compact_summary"],
        "CANARY_SUMMARY"
    );
    let metadata = normalise(Provider::ClaudeCode, CaptureMode::MetadataOnly, payload);
    assert!(metadata.content.is_none() && metadata.raw.is_none());
    assert!(!serde_json::to_string(&metadata).unwrap().contains("CANARY"));
}

#[test]
fn gemini_false_error_is_success_and_real_errors_and_nonzero_exits_fail() {
    for (response, expected) in [
        (json!({"error": false}), EventKind::ToolCallFinished),
        (json!({"error": null}), EventKind::ToolCallFinished),
        (json!({"error": ""}), EventKind::ToolCallFinished),
        (json!({"error": true}), EventKind::ToolCallFailed),
        (
            json!({"error": {"message": "CANARY_ERROR"}}),
            EventKind::ToolCallFailed,
        ),
        (
            json!({"exit_code": 1, "error": false}),
            EventKind::ToolCallFailed,
        ),
    ] {
        let event = normalise(
            Provider::GeminiCli,
            CaptureMode::MetadataOnly,
            json!({
                "hook_event_name": "AfterTool", "session_id": "s", "tool_name": "run_shell_command",
                "tool_input": {"command": "CANARY_COMMAND"}, "tool_response": response
            }),
        );
        assert_eq!(event.kind, expected);
        assert!(!serde_json::to_string(&event).unwrap().contains("CANARY"));
    }
    let event = normalise(
        Provider::GeminiCli,
        CaptureMode::MetadataOnly,
        json!({
            "hook_event_name": "Notification", "session_id": "s", "notification_type": "ToolPermission",
            "message": "CANARY_PERMISSION"
        }),
    );
    assert_eq!(event.attrs["notification_type"], "permission_prompt");
}
