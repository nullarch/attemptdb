//! Cursor adapter (`~/.cursor/hooks.json`).
//!
//! Generic tool hooks carry tool names and call ids. Older edit/shell hooks
//! put their details at the top level and remain readable for historical
//! imports, but are not installed alongside the generic hooks (double counts).
//! The stable per-conversation identifier is `conversation_id`.

use crate::common::{Normaliser, Payload, UNKNOWN_SESSION, classify_failure, event_name, to_snake};
use crate::{Adapter, AdapterError, CaptureContext};
use attemptdb_core::event::Provider;
use attemptdb_core::{Event, EventKind};
use serde_json::{Map, Value};

/// Hook events verified against Cursor (provider spelling).
pub const CURSOR_EVENTS: &[&str] = &[
    "sessionStart",
    "sessionEnd",
    "beforeSubmitPrompt",
    "stop",
    "afterFileEdit",
    "afterShellExecution",
    "postToolUseFailure",
    "preToolUse",
    "postToolUse",
    "subagentStart",
    "subagentStop",
    "preCompact",
];

/// Use one tool lifecycle, not both generic and specialized completion hooks.
pub const CURSOR_CAPTURE_EVENTS: &[&str] = &[
    "sessionStart",
    "sessionEnd",
    "beforeSubmitPrompt",
    "stop",
    "preToolUse",
    "postToolUse",
    "postToolUseFailure",
    "subagentStart",
    "subagentStop",
    "preCompact",
];

/// Content-free scalar payload fields kept under `attrs["provider"]`.
const PROVIDER_ATTR_KEYS: &[&str] = &[
    "generation_id",
    "status",
    "loop_count",
    "failure_type",
    "is_interrupt",
];

#[derive(Debug, Default, Clone, Copy)]
pub struct CursorAdapter;

impl Adapter for CursorAdapter {
    fn provider(&self) -> Provider {
        Provider::Cursor
    }

    fn supported_events(&self) -> &'static [&'static str] {
        CURSOR_EVENTS
    }

    fn capture_events(&self) -> &'static [&'static str] {
        CURSOR_CAPTURE_EVENTS
    }

    fn normalise(
        &self,
        ctx: &CaptureContext,
        event_name_hint: Option<&str>,
        payload: &Value,
    ) -> Result<Event, AdapterError> {
        normalise(ctx, event_name_hint, payload)
    }
}

pub fn map_kind(name: &str) -> EventKind {
    match name {
        "sessionStart" => EventKind::SessionStarted,
        "sessionEnd" => EventKind::SessionEnded,
        "beforeSubmitPrompt" => EventKind::PromptSubmitted,
        "stop" => EventKind::TurnStopped,
        "afterFileEdit" | "afterShellExecution" => EventKind::ToolCallFinished,
        "postToolUseFailure" => EventKind::ToolCallFailed,
        "preToolUse" => EventKind::ToolCallStarted,
        "postToolUse" => EventKind::ToolCallFinished,
        "subagentStart" => EventKind::SubagentStarted,
        "subagentStop" => EventKind::SubagentStopped,
        "preCompact" => EventKind::CompactionStarted,
        _ => EventKind::Unknown,
    }
}

fn normalise(
    ctx: &CaptureContext,
    hint: Option<&str>,
    payload: &Value,
) -> Result<Event, AdapterError> {
    let p = Payload::from_value(payload)?;
    let name = event_name(p, hint)?;
    let kind = map_kind(&name);
    let session = p
        .first_str(&["parent_conversation_id", "conversation_id", "session_id"])
        .unwrap_or(UNKNOWN_SESSION);
    let mut n = Normaliser::new(ctx, p, Provider::Cursor, &name, kind, session);
    n.event.provider_turn_id = p.str("generation_id").map(str::to_string);
    if n.event.provider_version.is_none() {
        n.event.provider_version = p.str("cursor_version").map(str::to_string);
    }
    n.set_cwd();
    n.set_model();
    n.set_transcript_present();
    n.copy_provider_attrs(PROVIDER_ATTR_KEYS);
    match name.as_str() {
        "beforeSubmitPrompt" => prompt(&mut n),
        "afterFileEdit" => file_edit(&mut n),
        "afterShellExecution" => shell(&mut n),
        "postToolUseFailure" => failure(&mut n),
        "preToolUse" => generic_tool(&mut n),
        "postToolUse" => generic_result(&mut n),
        "subagentStart" | "subagentStop" => {
            if let Some(id) = p.str("subagent_id") {
                n.set_subagent(id, p.str("subagent_type"));
            }
            n.set_duration(&["duration_ms"]);
            for key in ["task", "description", "summary"] {
                if let Some(text) = p.str(key) {
                    n.set_extra(key, text);
                }
            }
        }
        "preCompact" => {
            n.attr_opt("trigger", p.str("trigger"));
            n.attr_opt("pre_tokens", p.number("context_tokens"));
        }
        "sessionEnd" => n.attr_opt("reason", p.str("reason")),
        _ => {}
    }
    Ok(n.finish())
}

fn generic_tool(n: &mut Normaliser<'_>) {
    let p = n.payload();
    if let Some(name) = p.str("tool_name") {
        n.set_tool(name, p.str("tool_use_id"));
    }
    if let Some(input) = p.get("tool_input") {
        n.apply_tool_input(input);
    }
    n.set_duration(&["duration", "duration_ms"]);
}

fn generic_result(n: &mut Normaliser<'_>) {
    generic_tool(n);
    let p = n.payload();
    // The current protocol JSON-encodes tool_output inside the outer JSON.
    let response = p.get("tool_output").map(|v| {
        v.as_str()
            .and_then(|s| serde_json::from_str::<Value>(s).ok())
            .unwrap_or_else(|| v.clone())
    });
    if let Some(response) = &response {
        n.set_tool_output(response);
    }
    let exit_code = response
        .as_ref()
        .and_then(crate::common::response_exit_code);
    if exit_code.is_some_and(|code| code != 0) {
        n.event.kind = EventKind::ToolCallFailed;
        n.set_failure(None, exit_code);
    } else {
        n.set_success(exit_code);
    }
}

fn prompt(n: &mut Normaliser<'_>) {
    let p = n.payload();
    if let Some(prompt) = p.str("prompt") {
        n.set_prompt(prompt);
    }
    if let Some(attachments) = p.array("attachments") {
        for raw in attachments
            .iter()
            .filter_map(|a| a.get("file_path"))
            .filter_map(Value::as_str)
        {
            n.add_path(raw);
        }
        n.provider_attr("attachment_count", attachments.len() as u64);
    }
}

fn file_edit(n: &mut Normaliser<'_>) {
    let p = n.payload();
    n.set_tool("Edit", None);
    let mut input = Map::new();
    if let Some(path) = p.get("file_path") {
        input.insert("file_path".into(), path.clone());
    }
    if let Some(edits) = p.get("edits") {
        input.insert("edits".into(), edits.clone());
        n.provider_attr("edit_count", edits.as_array().map_or(0, Vec::len) as u64);
    }
    n.apply_tool_input(&Value::Object(input));
    n.set_success(None);
}

fn shell(n: &mut Normaliser<'_>) {
    let p = n.payload();
    n.set_tool("Shell", None);
    if let Some(command) = p.get("command") {
        let input = Value::Object(Map::from_iter([("command".to_string(), command.clone())]));
        n.apply_tool_input(&input);
    }
    if let Some(output) = p.get("output") {
        n.set_tool_output(output);
    }
    n.set_duration(&["duration", "duration_ms"]);
    let exit_code = p.number("exit_code").map(|c| c as i32);
    match exit_code {
        Some(code) if code != 0 => {
            n.event.kind = EventKind::ToolCallFailed;
            n.set_failure(None, Some(code));
        }
        code => n.set_success(code),
    }
}

fn failure(n: &mut Normaliser<'_>) {
    let p = n.payload();
    if let Some(name) = p.str("tool_name") {
        n.set_tool(name, p.str("tool_use_id"));
    }
    if let Some(input) = p.get("tool_input") {
        n.apply_tool_input(input);
    }
    n.set_duration(&["duration", "duration_ms"]);
    let text = p.first_str(&["error_message", "error"]);
    let derived = text
        .map(classify_failure)
        .is_some_and(|fc| fc.class != "unknown");
    let provider_class = p.str("failure_type").map(to_snake).or_else(|| {
        p.bool("is_interrupt")
            .filter(|b| *b)
            .map(|_| "interrupted".to_string())
    });
    match provider_class {
        Some(class) if !derived => n.set_failure_with_class(&class, text),
        _ => n.set_failure(text, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds() {
        assert_eq!(map_kind("afterFileEdit"), EventKind::ToolCallFinished);
        assert_eq!(map_kind("postToolUseFailure"), EventKind::ToolCallFailed);
        assert_eq!(map_kind("beforeReadFile"), EventKind::Unknown);
    }
}
