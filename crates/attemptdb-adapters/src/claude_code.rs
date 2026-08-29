//! Claude Code adapter.
//!
//! This module also hosts the shared implementation for every provider whose
//! hooks use the Claude Code payload shape (`hook_event_name`, `session_id`,
//! `cwd`, `tool_name`, `tool_input`, ...); the Codex CLI adapter reuses it via
//! [`normalise_claude_shaped`].

use crate::common::{Normaliser, Payload, UNKNOWN_SESSION, event_name, is_token, to_snake};
use crate::{Adapter, AdapterError, CaptureContext};
use attemptdb_core::event::Provider;
use attemptdb_core::{Event, EventKind, Outcome};
use serde_json::Value;

/// Hook events documented for Claude Code (provider spelling).
pub const CLAUDE_CODE_EVENTS: &[&str] = &[
    "SessionStart",
    "SessionEnd",
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "PostToolUseFailure",
    "PermissionRequest",
    "PermissionDenied",
    "Notification",
    "Stop",
    "StopFailure",
    "SubagentStart",
    "SubagentStop",
    "TaskCreated",
    "TaskCompleted",
    "PreCompact",
    "PostCompact",
    "ConfigChange",
    "CwdChanged",
    "FileChanged",
    "WorktreeCreate",
    "WorktreeRemove",
    "Setup",
    "UserPromptExpansion",
    "PostToolBatch",
    "MessageDisplay",
    "TeammateIdle",
    "InstructionsLoaded",
    "DirectoryAdded",
    "Elicitation",
    "ElicitationResult",
];

#[derive(Debug, Default, Clone, Copy)]
pub struct ClaudeCodeAdapter;

impl Adapter for ClaudeCodeAdapter {
    fn provider(&self) -> Provider {
        Provider::ClaudeCode
    }

    fn supported_events(&self) -> &'static [&'static str] {
        CLAUDE_CODE_EVENTS
    }

    fn normalise(
        &self,
        ctx: &CaptureContext,
        event_name_hint: Option<&str>,
        payload: &Value,
    ) -> Result<Event, AdapterError> {
        normalise_claude_shaped(Provider::ClaudeCode, ctx, event_name_hint, payload)
    }
}

/// Canonical kind for a Claude-shaped event name. Anything unmapped is
/// [`EventKind::Unknown`]; the provider name is kept on the event.
pub fn map_kind(name: &str) -> EventKind {
    match name {
        "SessionStart" => EventKind::SessionStarted,
        "SessionEnd" => EventKind::SessionEnded,
        "UserPromptSubmit" => EventKind::PromptSubmitted,
        "PreToolUse" => EventKind::ToolCallStarted,
        "PostToolUse" => EventKind::ToolCallFinished,
        "PostToolUseFailure" => EventKind::ToolCallFailed,
        "PermissionRequest" => EventKind::PermissionRequested,
        "PermissionDenied" => EventKind::PermissionDenied,
        "Notification" => EventKind::Notification,
        "Stop" => EventKind::TurnStopped,
        "StopFailure" => EventKind::TurnFailed,
        "SubagentStart" => EventKind::SubagentStarted,
        "SubagentStop" => EventKind::SubagentStopped,
        "TaskCreated" => EventKind::TaskCreated,
        "TaskCompleted" => EventKind::TaskCompleted,
        "PreCompact" => EventKind::CompactionStarted,
        "PostCompact" => EventKind::CompactionFinished,
        "ConfigChange" => EventKind::ConfigChanged,
        "CwdChanged" => EventKind::CwdChanged,
        "FileChanged" => EventKind::FileChanged,
        "WorktreeCreate" => EventKind::WorktreeCreated,
        "WorktreeRemove" => EventKind::WorktreeRemoved,
        _ => EventKind::Unknown,
    }
}

/// Normalise a payload in the Claude Code hook shape for `provider`.
pub(crate) fn normalise_claude_shaped(
    provider: Provider,
    ctx: &CaptureContext,
    hint: Option<&str>,
    payload: &Value,
) -> Result<Event, AdapterError> {
    let p = Payload::from_value(payload)?;
    let name = event_name(p, hint)?;
    let kind = map_kind(&name);
    let session = p.str("session_id").unwrap_or(UNKNOWN_SESSION);
    let mut n = Normaliser::new(ctx, p, provider, &name, kind, session);
    n.event.provider_turn_id = p.first_str(&["prompt_id", "turn_id"]).map(str::to_string);
    apply_common(&mut n);
    apply_kind(&mut n, kind);
    Ok(n.finish())
}

fn apply_common(n: &mut Normaliser<'_>) {
    let p = n.payload();
    n.set_cwd();
    n.set_permission_mode();
    n.set_transcript_present();
    n.set_model();
    if let Some(agent_id) = p.str("agent_id") {
        n.set_subagent(agent_id, p.str("agent_type"));
    } else if let Some(agent_type) = p.str("agent_type") {
        n.attr("agent_type", agent_type);
    }
}

fn apply_kind(n: &mut Normaliser<'_>, kind: EventKind) {
    let p = n.payload();
    match kind {
        EventKind::SessionStarted => n.attr_opt("source", p.str("source")),
        EventKind::SessionEnded => n.attr_opt("reason", p.str("reason")),
        EventKind::PromptSubmitted => {
            if let Some(prompt) = p.str("prompt") {
                n.set_prompt(prompt);
            }
        }
        EventKind::ToolCallStarted
        | EventKind::ToolCallFinished
        | EventKind::ToolCallFailed
        | EventKind::PermissionRequested
        | EventKind::PermissionDenied => tool_event(n, kind),
        EventKind::Notification => notification(n),
        EventKind::TurnStopped => stop(n),
        EventKind::TurnFailed => stop_failure(n),
        EventKind::SubagentStarted | EventKind::SubagentStopped => stop(n),
        EventKind::TaskCreated | EventKind::TaskCompleted => task(n),
        EventKind::CompactionStarted | EventKind::CompactionFinished => compaction(n),
        EventKind::ConfigChanged => config_change(n),
        EventKind::CwdChanged => cwd_changed(n),
        EventKind::FileChanged => {
            if let Some(path) = p.str("file_path") {
                n.add_file(path);
            }
        }
        EventKind::WorktreeCreated | EventKind::WorktreeRemoved => worktree(n),
        _ => unknown(n),
    }
}

fn tool_event(n: &mut Normaliser<'_>, kind: EventKind) {
    let p = n.payload();
    if let Some(name) = p.str("tool_name") {
        n.set_tool(name, p.str("tool_use_id"));
    }
    if let Some(input) = p.get("tool_input") {
        n.apply_tool_input(input);
    }
    n.set_duration(&["duration_ms", "duration"]);
    match kind {
        EventKind::ToolCallFinished => tool_finished(n),
        EventKind::ToolCallFailed => tool_failed(n),
        EventKind::PermissionDenied => {
            n.event.outcome = Some(Outcome::denied());
            if let Some(reason) = p.str("reason") {
                n.set_extra("reason", reason);
            }
        }
        _ => {}
    }
}

fn tool_finished(n: &mut Normaliser<'_>) {
    let p = n.payload();
    let Some(response) = p.get("tool_response") else {
        n.set_success(None);
        return;
    };
    n.set_tool_output(response);
    match crate::common::response_exit_code(response) {
        Some(code) if code != 0 => {
            n.event.kind = EventKind::ToolCallFailed;
            n.set_failure(None, Some(code));
        }
        code => n.set_success(code),
    }
}

fn tool_failed(n: &mut Normaliser<'_>) {
    let p = n.payload();
    let text = failure_text(p);
    n.set_failure(text.as_deref(), None);
    if let Some(response) = p.get("tool_response").filter(|v| v.is_object()) {
        n.set_tool_output(response);
    }
}

/// Error text of a failed tool call, wherever the provider put it.
fn failure_text(p: Payload<'_>) -> Option<String> {
    if let Some(text) = p.first_str(&["error", "error_message"]) {
        return Some(text.to_string());
    }
    match p.get("tool_response")? {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Object(map) => ["error", "message", "stderr"]
            .iter()
            .find_map(|k| map.get(*k).and_then(Value::as_str))
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        _ => None,
    }
}

fn notification(n: &mut Normaliser<'_>) {
    let p = n.payload();
    n.attr_opt(
        "notification_type",
        p.first_str(&["notification_type", "type"]),
    );
    if let Some(message) = p.str("message") {
        n.set_message(message);
    }
    if let Some(title) = p.str("title") {
        n.set_extra("title", title);
    }
}

fn stop(n: &mut Normaliser<'_>) {
    let p = n.payload();
    n.attr_opt("stop_hook_active", p.bool("stop_hook_active"));
    if let Some(message) = p.str("last_assistant_message") {
        n.set_message(message);
    }
}

fn stop_failure(n: &mut Normaliser<'_>) {
    let p = n.payload();
    n.attr_opt("stop_hook_active", p.bool("stop_hook_active"));
    let error = p.str("error");
    let class = p
        .first_str(&["error_type", "matcher"])
        .or_else(|| error.filter(|e| is_token(e)))
        .map(to_snake);
    let text = p
        .first_str(&["error_details", "error_message", "message"])
        .or_else(|| error.filter(|e| !is_token(e)));
    match class {
        Some(class) => n.set_failure_with_class(&class, text),
        None => n.set_failure(text, None),
    }
}

fn task(n: &mut Normaliser<'_>) {
    let p = n.payload();
    n.attr_opt("task_id", p.str("task_id"));
    n.attr_opt("task_status", p.first_str(&["task_status", "status"]));
    for key in ["task_subject", "task_description"] {
        if let Some(text) = p.str(key) {
            n.set_extra(key, text);
        }
    }
}

fn compaction(n: &mut Normaliser<'_>) {
    let p = n.payload();
    n.attr_opt("trigger", p.str("trigger"));
    if let Some(instructions) = p.str("custom_instructions") {
        n.set_extra("custom_instructions", instructions);
    }
}

fn config_change(n: &mut Normaliser<'_>) {
    let p = n.payload();
    n.attr_opt("config_source", p.str("source"));
    if let Some(path) = p.str("file_path") {
        n.add_file(path);
    }
}

fn cwd_changed(n: &mut Normaliser<'_>) {
    let p = n.payload();
    if let Some(previous) = p.str("previous_cwd")
        && let Some(path) = n.add_path(previous)
    {
        n.attr("previous_cwd", path.logical);
    }
}

fn worktree(n: &mut Normaliser<'_>) {
    let p = n.payload();
    if let Some(raw) = p.str("worktree_path")
        && let Some(path) = n.add_path(raw)
    {
        n.attr("worktree_path", path.logical);
    }
}

/// Unrecognised event: keep whatever tool metadata is present, nothing else.
fn unknown(n: &mut Normaliser<'_>) {
    let p = n.payload();
    if let Some(name) = p.str("tool_name") {
        n.set_tool(name, p.str("tool_use_id"));
    }
    if let Some(input) = p.get("tool_input") {
        n.apply_tool_input(input);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_supported_event_has_a_kind_or_is_unknown() {
        for name in CLAUDE_CODE_EVENTS {
            let _ = map_kind(name);
        }
        assert_eq!(map_kind("PostToolUse"), EventKind::ToolCallFinished);
        assert_eq!(map_kind("MessageDisplay"), EventKind::Unknown);
        assert_eq!(map_kind("NotARealEvent"), EventKind::Unknown);
    }
}
