//! One real-world action, one event id, whichever channel saw it.
//!
//! A tool call reaches the database through hooks, a transcript import and a
//! rollout import. The provider names the call (`tool_use_id`, `call_id`), so
//! every channel derives the event id from `(provider, session, kind, that
//! id)` and storage's by-id duplicate check merges them without anyone
//! looking anything up. These tests pin that contract at the adapter level;
//! `crates/attempt/tests/dedup_channels.rs` proves it end to end through
//! storage and the projection.

use attemptdb_adapters::common::{derive_event_id, tool_call_key};
use attemptdb_adapters::transcript::{
    CodexRolloutOptions, TranscriptOptions, parse_claude_transcript, parse_codex_rollout_to_vec,
    rollout_event_id,
};
use attemptdb_adapters::{CaptureContext, adapter_for};
use attemptdb_core::event::Provider;
use attemptdb_core::{CaptureMode, DeviceId, Event, EventKind, ProjectRef, Timestamp};
use serde_json::{Value, json};
use std::path::Path;

const CAPTURED_AT: Timestamp = Timestamp::from_micros(1_787_904_000_000_000);
const CLAUDE_SESSION: &str = "11111111-1111-4111-8111-111111111111";

fn ctx(captured_at: Timestamp, hook_version: Option<&str>) -> CaptureContext {
    let device_id = DeviceId::derive(&["dedup-ids"]);
    CaptureContext {
        device_id,
        capture_mode: CaptureMode::LocalSemantic,
        project: ProjectRef::derive("/home/dev/example/project", None, &device_id),
        captured_at,
        provider_version: None,
        hook_version: hook_version.map(str::to_string),
    }
}

fn hook(provider: Provider, payload: Value) -> Event {
    adapter_for(&provider)
        .unwrap()
        .normalise(&ctx(CAPTURED_AT, Some("test")), None, &payload)
        .unwrap()
}

fn claude(payload: Value) -> Event {
    hook(Provider::ClaudeCode, payload)
}

fn fixture(rel: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/transcripts")
        .join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn basic_turn() -> Vec<Event> {
    parse_claude_transcript(
        fixture("claude_code/basic_turn.jsonl")
            .lines()
            .map(str::to_string),
        &ctx(CAPTURED_AT, None),
        &TranscriptOptions::default(),
    )
    .events
}

fn transcript_call(events: &[Event], call_id: &str, kind: EventKind) -> Event {
    events
        .iter()
        .find(|e| {
            e.kind == kind && e.tool.as_ref().and_then(|t| t.call_id.as_deref()) == Some(call_id)
        })
        .unwrap_or_else(|| panic!("no {kind:?} for {call_id}"))
        .clone()
}

#[test]
fn a_claude_tool_call_has_the_same_ids_in_hooks_and_transcript() {
    let transcript = basic_turn();
    let pre = claude(json!({
        "hook_event_name": "PreToolUse", "session_id": CLAUDE_SESSION,
        "tool_name": "Bash", "tool_use_id": "toolu_0001",
        "tool_input": {"command": "cargo test -p example"}
    }));
    let post = claude(json!({
        "hook_event_name": "PostToolUse", "session_id": CLAUDE_SESSION,
        "tool_name": "Bash", "tool_use_id": "toolu_0001",
        "tool_input": {"command": "cargo test -p example"},
        "tool_response": {"stdout": "ok", "exit_code": 0}
    }));
    let failure = claude(json!({
        "hook_event_name": "PostToolUseFailure", "session_id": CLAUDE_SESSION,
        "tool_name": "Edit", "tool_use_id": "toolu_0002",
        "tool_input": {"file_path": "/home/dev/example/project/src/lib.rs"},
        "error": "String to replace not found in file."
    }));
    assert_eq!(pre.kind, EventKind::ToolCallStarted);
    assert_eq!(post.kind, EventKind::ToolCallFinished);
    assert_eq!(failure.kind, EventKind::ToolCallFailed);
    assert_eq!(
        pre.event_id,
        transcript_call(&transcript, "toolu_0001", EventKind::ToolCallStarted).event_id
    );
    assert_eq!(
        post.event_id,
        transcript_call(&transcript, "toolu_0001", EventKind::ToolCallFinished).event_id
    );
    assert_eq!(
        failure.event_id,
        transcript_call(&transcript, "toolu_0002", EventKind::ToolCallFailed).event_id
    );
    // The start and the end of one call are still two events.
    assert_ne!(pre.event_id, post.event_id);
}

#[test]
fn the_end_of_a_call_is_one_event_even_when_the_channels_disagree_on_failure() {
    // The hook saw the Edit succeed; the transcript says `is_error`. Storage
    // must still hold one end for the call, not a finished and a failed.
    let transcript = basic_turn();
    let hook_end = claude(json!({
        "hook_event_name": "PostToolUse", "session_id": CLAUDE_SESSION,
        "tool_name": "Edit", "tool_use_id": "toolu_0002",
        "tool_input": {"file_path": "/home/dev/example/project/src/lib.rs"},
        "tool_response": {"filePath": "/home/dev/example/project/src/lib.rs"}
    }));
    assert_eq!(hook_end.kind, EventKind::ToolCallFinished);
    let transcript_end = transcript_call(&transcript, "toolu_0002", EventKind::ToolCallFailed);
    assert_ne!(hook_end.kind, transcript_end.kind);
    assert_eq!(hook_end.event_id, transcript_end.event_id);
}

#[test]
fn a_hook_registered_twice_stores_one_event_per_call_event() {
    // User scope and project scope run two different command strings; the
    // payload on stdin is the same, and the hook adds nothing of its own
    // that reaches the id (not the capture time, not the hook version).
    let payload = json!({
        "hook_event_name": "PostToolUse", "session_id": "sess-twice",
        "tool_name": "Bash", "tool_use_id": "toolu_dup",
        "tool_input": {"command": "ls"}, "tool_response": {"stdout": "a"}
    });
    let first = adapter_for(&Provider::ClaudeCode)
        .unwrap()
        .normalise(&ctx(CAPTURED_AT, Some("0.2.13")), None, &payload)
        .unwrap();
    let second = adapter_for(&Provider::ClaudeCode)
        .unwrap()
        .normalise(
            &ctx(Timestamp::from_micros(CAPTURED_AT.as_micros() + 700), None),
            None,
            &payload,
        )
        .unwrap();
    assert_eq!(first.event_id, second.event_id);

    // Everything that makes it a different event makes a different id.
    let other_call = claude(json!({
        "hook_event_name": "PostToolUse", "session_id": "sess-twice",
        "tool_name": "Bash", "tool_use_id": "toolu_other", "tool_response": {}
    }));
    let other_session = claude(json!({
        "hook_event_name": "PostToolUse", "session_id": "sess-else",
        "tool_name": "Bash", "tool_use_id": "toolu_dup", "tool_response": {}
    }));
    let other_phase = claude(json!({
        "hook_event_name": "PreToolUse", "session_id": "sess-twice",
        "tool_name": "Bash", "tool_use_id": "toolu_dup"
    }));
    for e in [&other_call, &other_session, &other_phase] {
        assert_ne!(e.event_id, first.event_id);
    }
    // Claude and Codex ids never meet, even for the same strings.
    let codex = hook(
        Provider::Codex,
        json!({
            "hook_event_name": "PostToolUse", "session_id": "sess-twice",
            "tool_name": "shell", "tool_use_id": "toolu_dup", "tool_response": {}
        }),
    );
    assert_ne!(codex.event_id, first.event_id);
}

#[test]
fn permission_events_of_a_call_are_named_by_the_call() {
    let ask = |kind: &str| {
        claude(json!({
            "hook_event_name": kind, "session_id": "sess-perm",
            "tool_name": "Bash", "tool_use_id": "toolu_perm",
            "tool_input": {"command": "rm -rf node_modules"}
        }))
    };
    let request = ask("PermissionRequest");
    assert_eq!(request.event_id, ask("PermissionRequest").event_id);
    let denied = ask("PermissionDenied");
    assert_ne!(request.event_id, denied.event_id);
    assert_eq!(denied.event_id, ask("PermissionDenied").event_id);
}

#[test]
fn events_the_provider_does_not_name_keep_distinct_ids() {
    // A prompt, a stop and a notification carry no id of their own, and a
    // payload hash would merge two different prompts that read the same. So
    // they keep random ids: reconciling them with a transcript is the
    // importer's job, and a hook registered twice still duplicates them.
    for payload in [
        json!({"hook_event_name": "UserPromptSubmit", "session_id": "s", "prompt": "continue"}),
        json!({"hook_event_name": "Stop", "session_id": "s", "stop_hook_active": false}),
        json!({"hook_event_name": "Notification", "session_id": "s", "message": "needs input"}),
        json!({"hook_event_name": "SessionStart", "session_id": "s", "source": "startup"}),
        // A tool event without a call id has nothing to be named by.
        json!({"hook_event_name": "PreToolUse", "session_id": "s", "tool_name": "Bash"}),
    ] {
        assert_ne!(
            claude(payload.clone()).event_id,
            claude(payload.clone()).event_id,
            "{payload}"
        );
    }
    // Neither does a tool event with no session: a call id is only unique
    // within one, and the transcript could not be joined to it anyway.
    let no_session = json!({
        "hook_event_name": "PreToolUse", "tool_name": "Bash", "tool_use_id": "toolu_x"
    });
    assert_ne!(
        claude(no_session.clone()).event_id,
        claude(no_session).event_id
    );
}

#[test]
fn the_derivation_is_one_pure_function() {
    let id = |p: &Provider, s: &str, k: EventKind, key: &str| derive_event_id(p, s, k, key);
    let a = id(
        &Provider::ClaudeCode,
        "s",
        EventKind::ToolCallStarted,
        &tool_call_key("c"),
    );
    assert_eq!(
        a,
        id(
            &Provider::ClaudeCode,
            "s",
            EventKind::ToolCallStarted,
            &tool_call_key("c")
        )
    );
    assert_ne!(
        a,
        id(&Provider::Codex, "s", EventKind::ToolCallStarted, "call:c")
    );
    assert_ne!(
        a,
        id(
            &Provider::ClaudeCode,
            "t",
            EventKind::ToolCallStarted,
            "call:c"
        )
    );
    assert_ne!(
        a,
        id(
            &Provider::ClaudeCode,
            "s",
            EventKind::ToolCallFinished,
            "call:c"
        )
    );
    assert_ne!(
        a,
        id(
            &Provider::ClaudeCode,
            "s",
            EventKind::ToolCallStarted,
            "call:d"
        )
    );
    // Finished and failed are one slot.
    assert_eq!(
        id(
            &Provider::ClaudeCode,
            "s",
            EventKind::ToolCallFinished,
            "call:c"
        ),
        id(
            &Provider::ClaudeCode,
            "s",
            EventKind::ToolCallFailed,
            "call:c"
        )
    );
    // Pinned, computed independently (UUIDv5 of `ev_ US event-v1 US provider
    // US session US slot US key` under the AttemptDB namespace): ids already
    // stored by a release must not move. If this fails, the derivation
    // changed and every stored tool-call id with it.
    assert_eq!(a.to_string(), "85d39602-8d32-5d49-9740-c2c74daabb65");
    assert_eq!(
        id(&Provider::Codex, "s", EventKind::ToolCallFailed, "call:c").to_string(),
        "46f8db97-7cd5-5bed-a370-090b55d565fa"
    );
}

#[test]
fn codex_hook_ids_are_the_rollout_ids_of_the_same_call() {
    let rollout = parse_codex_rollout_to_vec(
        std::io::Cursor::new(fixture("codex/classic_turn.jsonl")),
        &ctx(CAPTURED_AT, None),
        &CodexRolloutOptions::default(),
    );
    let session = "33333333-3333-4333-8333-333333333333";
    let rollout_call = |kind: EventKind| {
        rollout
            .events
            .iter()
            .find(|e| {
                e.kind == kind
                    && e.tool.as_ref().and_then(|t| t.call_id.as_deref()) == Some("call_ec01")
            })
            .unwrap_or_else(|| panic!("rollout has no {kind:?} for call_ec01"))
    };
    let pre = hook(
        Provider::Codex,
        json!({
            "hook_event_name": "PreToolUse", "session_id": session, "turn_id": "turn-1",
            "tool_name": "exec_command", "tool_use_id": "call_ec01",
            "tool_input": {"cmd": "git status --short"}
        }),
    );
    let post = hook(
        Provider::Codex,
        json!({
            "hook_event_name": "PostToolUse", "session_id": session, "turn_id": "turn-1",
            "tool_name": "exec_command", "tool_use_id": "call_ec01",
            "tool_input": {"cmd": "git status --short"},
            "tool_response": {"output": "Process exited with code 0"}
        }),
    );
    assert_eq!(
        pre.event_id,
        rollout_call(EventKind::ToolCallStarted).event_id
    );
    assert_eq!(
        post.event_id,
        rollout_call(EventKind::ToolCallFinished).event_id
    );
    assert_eq!(
        pre.event_id,
        rollout_event_id(session, "call:call_ec01", EventKind::ToolCallStarted)
    );
}
