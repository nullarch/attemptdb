//! Codex CLI adapter.
//!
//! Codex hooks emit the same event names and payload shape as Claude Code
//! (`hook_event_name`, `session_id`, `cwd`, `tool_name`, `tool_input`), with
//! a `turn_id` instead of `prompt_id` and shell tools that pass `command` as
//! an argv array. The whole mapping is therefore shared with
//! [`crate::claude_code`]; only the provider identity and the verified event
//! list differ.

use crate::claude_code::normalise_claude_shaped;
use crate::{Adapter, AdapterError, CaptureContext};
use attemptdb_core::Event;
use attemptdb_core::event::Provider;
use serde_json::Value;

/// Hook events verified against Codex CLI (provider spelling).
pub const CODEX_EVENTS: &[&str] = &[
    "SessionStart",
    "SessionEnd",
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "PermissionRequest",
    "SubagentStart",
    "SubagentStop",
    "Stop",
];

#[derive(Debug, Default, Clone, Copy)]
pub struct CodexAdapter;

impl Adapter for CodexAdapter {
    fn provider(&self) -> Provider {
        Provider::Codex
    }

    fn supported_events(&self) -> &'static [&'static str] {
        CODEX_EVENTS
    }

    fn normalise(
        &self,
        ctx: &CaptureContext,
        event_name_hint: Option<&str>,
        payload: &Value,
    ) -> Result<Event, AdapterError> {
        normalise_claude_shaped(Provider::Codex, ctx, event_name_hint, payload)
    }
}
