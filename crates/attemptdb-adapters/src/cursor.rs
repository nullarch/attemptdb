//! Cursor adapter (`~/.cursor/hooks.json`).
//!
//! Cursor payloads differ from the Claude shape in three ways that matter:
//! there is no `tool_name` (the event name implies the tool), edit and shell
//! details sit at the top level (`file_path`, `edits`, `command`, `output`),
//! and the stable per-conversation identifier is `conversation_id`
//! (`session_id` only appears on session events).

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
        .first_str(&["conversation_id", "session_id"])
        .unwrap_or(UNKNOWN_SESSION);
    let mut n = Normaliser::new(ctx, p, Provider::Cursor, &name, kind, session);
    if n.event.provider_version.is_none() {
        n.event.provider_version = p.str("cursor_version").map(str::to_string);
    }
    n.set_cwd();
    n.set_model();
    n.copy_provider_attrs(PROVIDER_ATTR_KEYS);
    match name.as_str() {
        "beforeSubmitPrompt" => prompt(&mut n),
        "afterFileEdit" => file_edit(&mut n),
        "afterShellExecution" => shell(&mut n),
        "postToolUseFailure" => failure(&mut n),
        "sessionEnd" => n.attr_opt("reason", p.str("reason")),
        _ => {}
    }
    Ok(n.finish())
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
