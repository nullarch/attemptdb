//! Gemini CLI adapter (`~/.gemini/settings.json`).
//!
//! Gemini uses Claude-like event names for the session lifecycle but its own
//! for prompts and tools (`BeforeAgent`/`AfterAgent`, `BeforeTool`/
//! `AfterTool`), and passes tool arguments as `tool_args` rather than
//! `tool_input`.

use crate::common::{Normaliser, Payload, UNKNOWN_SESSION, event_name};
use crate::{Adapter, AdapterError, CaptureContext};
use attemptdb_core::event::Provider;
use attemptdb_core::{Event, EventKind};
use serde_json::Value;

/// Hook events verified against Gemini CLI (provider spelling).
pub const GEMINI_CLI_EVENTS: &[&str] = &[
    "SessionStart",
    "SessionEnd",
    "BeforeAgent",
    "AfterAgent",
    "BeforeTool",
    "AfterTool",
];

#[derive(Debug, Default, Clone, Copy)]
pub struct GeminiCliAdapter;

impl Adapter for GeminiCliAdapter {
    fn provider(&self) -> Provider {
        Provider::GeminiCli
    }

    fn supported_events(&self) -> &'static [&'static str] {
        GEMINI_CLI_EVENTS
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
        "SessionStart" => EventKind::SessionStarted,
        "SessionEnd" => EventKind::SessionEnded,
        "BeforeAgent" => EventKind::PromptSubmitted,
        "AfterAgent" => EventKind::TurnStopped,
        "BeforeTool" => EventKind::ToolCallStarted,
        "AfterTool" => EventKind::ToolCallFinished,
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
    let session = p.str("session_id").unwrap_or(UNKNOWN_SESSION);
    let mut n = Normaliser::new(ctx, p, Provider::GeminiCli, &name, kind, session);
    n.set_cwd();
    n.set_transcript_present();
    n.set_model();
    match kind {
        EventKind::SessionStarted => n.attr_opt("source", p.str("source")),
        EventKind::SessionEnded => n.attr_opt("reason", p.str("reason")),
        EventKind::PromptSubmitted => {
            if let Some(prompt) = p.str("prompt") {
                n.set_prompt(prompt);
            }
        }
        EventKind::TurnStopped => {
            if let Some(response) = p.first_str(&["prompt_response", "last_assistant_message"]) {
                n.set_message(response);
            }
        }
        EventKind::ToolCallStarted => tool(&mut n),
        EventKind::ToolCallFinished => {
            tool(&mut n);
            tool_result(&mut n);
        }
        _ => {}
    }
    Ok(n.finish())
}

fn tool(n: &mut Normaliser<'_>) {
    let p = n.payload();
    if let Some(name) = p.str("tool_name") {
        n.set_tool(
            name,
            p.first_str(&["tool_use_id", "tool_call_id", "call_id"]),
        );
    }
    if let Some(args) = p.get("tool_args").or_else(|| p.get("tool_input")) {
        n.apply_tool_input(args);
    }
    n.set_duration(&["duration_ms", "duration"]);
}

fn tool_result(n: &mut Normaliser<'_>) {
    let p = n.payload();
    if let Some(response) = p.get("tool_response") {
        n.set_tool_output(response);
    }
    match response_error(p) {
        Some(text) => {
            n.event.kind = EventKind::ToolCallFailed;
            n.set_failure(text.as_deref(), None);
        }
        None => n.set_success(None),
    }
}

/// `Some(text)` when the payload signals a failed tool; the inner value is
/// the error text when one exists.
fn response_error(p: Payload<'_>) -> Option<Option<String>> {
    if let Some(text) = p.str("error") {
        return Some(Some(text.to_string()));
    }
    if p.bool("success") == Some(false) {
        return Some(None);
    }
    let map = p.object("tool_response")?;
    if let Some(error) = map.get("error").filter(|e| !e.is_null()) {
        return Some(error_text(error));
    }
    let failed = matches!(
        map.get("status").and_then(Value::as_str),
        Some("error" | "failed")
    ) || map.get("success") == Some(&Value::Bool(false));
    failed.then(|| {
        map.get("message")
            .and_then(Value::as_str)
            .map(str::to_string)
    })
}

fn error_text(error: &Value) -> Option<String> {
    match error {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Object(map) => map
            .get("message")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| Some(error.to_string())),
        Value::Bool(true) => None,
        other => Some(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds() {
        assert_eq!(map_kind("BeforeAgent"), EventKind::PromptSubmitted);
        assert_eq!(map_kind("AfterTool"), EventKind::ToolCallFinished);
        assert_eq!(map_kind("PreToolUse"), EventKind::Unknown);
    }
}
