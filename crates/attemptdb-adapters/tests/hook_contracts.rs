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
    assert_eq!(instructions.paths[0].logical, "~/example/project/CLAUDE.md");
    assert_eq!(
        instructions.paths[0].repo_relative.as_deref(),
        Some("CLAUDE.md")
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

#[test]
fn tool_names_of_every_provider_classify_to_the_category_of_their_work() {
    use attemptdb_core::ToolCategory::*;
    // (provider, tool name as the hook reports it, category)
    for (provider, tool, expected) in [
        // Codex: the current shell tools, the patch tool, multi-agent tools.
        (Provider::Codex, "exec_command", Shell),
        (Provider::Codex, "exec", Shell),
        (Provider::Codex, "write_stdin", Shell),
        (Provider::Codex, "apply_patch", FileEdit),
        (Provider::Codex, "view_image", FileRead),
        (Provider::Codex, "update_plan", Plan),
        (Provider::Codex, "request_user_input_async", Plan),
        (Provider::Codex, "spawn_agent", Subagent),
        (Provider::Codex, "wait_agent", Subagent),
        (Provider::Codex, "send_message", Subagent),
        (Provider::Codex, "list_mcp_resources", Mcp),
        (Provider::Codex, "mcp__github__create_issue", Mcp),
        // Cursor.
        (Provider::Cursor, "MCP:github:create_issue", Mcp),
        (Provider::Cursor, "Delete", FileEdit),
        (Provider::Cursor, "Shell", Shell),
        (Provider::Cursor, "Read", FileRead),
        (Provider::Cursor, "Grep", Search),
        // Gemini.
        (Provider::GeminiCli, "mcp_github_create_issue", Mcp),
        (Provider::GeminiCli, "write_todos", Plan),
        (Provider::GeminiCli, "ask_user", Plan),
        (Provider::GeminiCli, "run_shell_command", Shell),
        // Claude.
        (Provider::ClaudeCode, "Delete", FileEdit),
        (Provider::ClaudeCode, "AskUserQuestion", Plan),
        (Provider::ClaudeCode, "BashOutput", Shell),
        (Provider::ClaudeCode, "mcp__github__create_issue", Mcp),
        // No category fits: stays `other` rather than a wrong label.
        (Provider::ClaudeCode, "Skill", Other),
        (Provider::Codex, "sleep", Other),
        (Provider::Codex, "wait", Other),
    ] {
        let event = normalise(
            provider.clone(),
            CaptureMode::MetadataOnly,
            json!({
                "hook_event_name": if provider == Provider::Cursor { "preToolUse" } else if provider == Provider::GeminiCli { "BeforeTool" } else { "PreToolUse" },
                "session_id": "s", "conversation_id": "s", "tool_name": tool
            }),
        );
        let got = event.tool.as_ref().map(|t| t.category);
        assert_eq!(got, Some(expected), "{provider} {tool}");
    }
}

#[test]
fn shell_commands_are_read_from_command_and_cmd_and_a_patch_is_not_a_command() {
    let call = |input: serde_json::Value| {
        normalise(
            Provider::Codex,
            CaptureMode::LocalSemantic,
            json!({"hook_event_name": "PreToolUse", "session_id": "s", "tool_name": "exec_command",
                "tool_input": input}),
        )
    };
    // `cmd` as a string, `command` as a string and as argv.
    for input in [
        json!({"cmd": "cargo test -p attemptdb-adapters", "workdir": "/home/dev/example/project"}),
        json!({"command": "cargo test -p attemptdb-adapters"}),
        json!({"command": ["cargo", "test", "-p", "attemptdb-adapters"]}),
    ] {
        let event = call(input.clone());
        assert_eq!(event.attrs["command_category"], "test", "{input}");
        assert_eq!(
            event.content.as_ref().and_then(|c| c.command.as_deref()),
            Some("cargo test -p attemptdb-adapters"),
            "{input}"
        );
        assert!(event.paths.is_empty(), "a workdir is not a touched file");
    }
    // `exec` takes a code cell in `input`: neither a command nor a patch.
    let exec = normalise(
        Provider::Codex,
        CaptureMode::LocalSemantic,
        json!({"hook_event_name": "PreToolUse", "session_id": "s", "tool_name": "exec",
            "tool_input": {"input": "const out = await tools.exec_command({cmd: 'ls'});"}}),
    );
    assert!(exec.attrs.get("command_category").is_none() && exec.paths.is_empty());

    // A patch in `command` (string or argv) or a shell heredoc: files and lines.
    let patch = "*** Begin Patch\n*** Update File: a.rs\n@@\n-old\n+new\n+newer\n*** Add File: b/c.rs\n+x\n*** End Patch";
    for input in [
        json!({"command": patch}),
        json!({"command": ["apply_patch", patch]}),
        json!({"patch": patch}),
        json!({"input": patch}),
        json!({"command": ["bash", "-lc", format!("apply_patch <<'EOF'\n{patch}\nEOF")]}),
    ] {
        let event = call(input.clone());
        let paths: Vec<&str> = event.paths.iter().map(|p| p.display()).collect();
        assert_eq!(paths, ["a.rs", "b/c.rs"], "{input}");
        assert_eq!(event.attrs["lines_added"], 3, "{input}");
        assert_eq!(event.attrs["lines_removed"], 1, "{input}");
        assert_eq!(event.attrs["file_count"], 2, "{input}");
    }
    // Only the heredoc form is also a shell command (its first words are).
    assert!(
        call(json!({"command": patch}))
            .attrs
            .get("command_bytes")
            .is_none()
    );
    assert!(
        call(json!({"command": ["apply_patch", patch]}))
            .attrs
            .get("command_bytes")
            .is_none()
    );
    // A command that merely mentions a header is still a command.
    let grep = call(json!({"cmd": "grep -rn '*** Update File:' docs"}));
    assert!(grep.paths.is_empty() && grep.attrs.get("lines_added").is_none());
    assert_eq!(grep.attrs["command_category"], "fs");
}

#[test]
fn an_interrupted_claude_tool_is_cancelled_in_both_hook_shapes() {
    // PostToolUseFailure with is_interrupt: the opaque error text would have
    // classified as `unknown`.
    let failure = normalise(
        Provider::ClaudeCode,
        CaptureMode::MetadataOnly,
        json!({"hook_event_name": "PostToolUseFailure", "session_id": "s", "tool_name": "Bash",
            "tool_input": {"command": "sleep 99"}, "error": "CANARY_OPAQUE_INTERRUPT_TEXT",
            "is_interrupt": true}),
    );
    let outcome = failure.outcome.as_ref().unwrap();
    assert_eq!(failure.kind, EventKind::ToolCallFailed);
    assert_eq!(outcome.status, OutcomeStatus::Cancelled);
    assert_eq!(outcome.class.as_deref(), Some("interrupted"));
    assert_eq!(failure.attrs["error_class"], "interrupted");
    assert!(!serde_json::to_string(&failure).unwrap().contains("CANARY"));
    // Without the flag the same text is a failure of unknown class.
    let plain = normalise(
        Provider::ClaudeCode,
        CaptureMode::MetadataOnly,
        json!({"hook_event_name": "PostToolUseFailure", "session_id": "s", "tool_name": "Bash",
            "tool_input": {"command": "sleep 99"}, "error": "CANARY_OPAQUE_INTERRUPT_TEXT",
            "is_interrupt": false}),
    );
    assert_eq!(
        plain.outcome.as_ref().unwrap().status,
        OutcomeStatus::Failure
    );
    assert_eq!(plain.attrs["error_class"], "unknown");

    // PostToolUse whose response says `interrupted`: not a success. The exit
    // code a killed process reports is kept.
    for (response, status, kind) in [
        (
            json!({"stdout": "", "interrupted": true}),
            OutcomeStatus::Cancelled,
            EventKind::ToolCallFailed,
        ),
        (
            json!({"stdout": "", "interrupted": true, "exit_code": 130}),
            OutcomeStatus::Cancelled,
            EventKind::ToolCallFailed,
        ),
        (
            json!({"stdout": "ok", "interrupted": false}),
            OutcomeStatus::Success,
            EventKind::ToolCallFinished,
        ),
        (
            json!({"stdout": "ok"}),
            OutcomeStatus::Success,
            EventKind::ToolCallFinished,
        ),
    ] {
        let event = normalise(
            Provider::ClaudeCode,
            CaptureMode::MetadataOnly,
            json!({"hook_event_name": "PostToolUse", "session_id": "s", "tool_name": "Bash",
                "tool_input": {"command": "sleep 99"}, "tool_response": response}),
        );
        assert_eq!(event.outcome.as_ref().unwrap().status, status, "{response}");
        assert_eq!(event.kind, kind, "{response}");
        if response.get("exit_code").is_some() {
            assert_eq!(event.outcome.as_ref().unwrap().exit_code, Some(130));
        }
    }
}

#[test]
fn gemini_before_and_after_tool_carry_one_signature() {
    // The projection pairs Gemini's tool events first-in-first-out by tool
    // name (no call id in the payload). Two parallel reads of one tool differ
    // by their content-free signature, which both events of one call share.
    let event = |name: &str, path: &str| {
        let mut payload = json!({
            "hook_event_name": name, "session_id": "s", "tool_name": "read_file",
            "tool_input": {"absolute_path": path},
        });
        if name == "AfterTool" {
            payload["tool_response"] = json!({"llmContent": "CANARY_FILE_TEXT"});
        }
        normalise(Provider::GeminiCli, CaptureMode::MetadataOnly, payload)
    };
    let signature =
        |e: &Event| -> Vec<String> { e.paths.iter().map(|p| p.logical.clone()).collect() };
    let (before_a, after_a) = (
        event("BeforeTool", "/home/dev/example/project/a.ts"),
        event("AfterTool", "/home/dev/example/project/a.ts"),
    );
    let (before_b, after_b) = (
        event("BeforeTool", "/home/dev/example/project/b.ts"),
        event("AfterTool", "/home/dev/example/project/b.ts"),
    );
    assert_eq!(signature(&before_a), signature(&after_a));
    assert_eq!(signature(&before_b), signature(&after_b));
    assert_ne!(signature(&before_a), signature(&before_b));
    assert!(
        before_a.tool.as_ref().unwrap().call_id.is_none(),
        "no id to pair by"
    );
}
