//! Synthetic event-stream builder for projection tests.
//!
//! Event ids and timestamps are fully deterministic so that two builds of the
//! same story produce byte-identical streams.

#![allow(dead_code)]

use attemptdb_core::event::{EventContent, Provider};
use attemptdb_core::{
    CaptureMode, DeviceId, Event, EventId, EventKind, Outcome, PortablePath, ProjectId, ProjectRef,
    SessionId, Timestamp, ToolCategory, ToolRef,
};
use serde_json::Value;

pub const ROOT: &str = "/work/repo";
/// 2026-08-28T08:00:00Z in microseconds.
pub const BASE_US: i64 = 1_787_904_000_000_000;

pub fn at(secs: i64) -> Timestamp {
    Timestamp::from_micros(BASE_US + secs * 1_000_000)
}

pub fn at_ms(ms: i64) -> Timestamp {
    Timestamp::from_micros(BASE_US + ms * 1_000)
}

#[derive(Clone, Debug)]
pub struct Sess {
    pub provider: Provider,
    pub provider_session_id: String,
    pub session_id: SessionId,
}

impl Sess {
    pub fn new(provider: Provider, provider_session_id: &str) -> Self {
        Self {
            session_id: SessionId::derive(&[provider.as_str(), provider_session_id]),
            provider,
            provider_session_id: provider_session_id.to_string(),
        }
    }

    pub fn claude(id: &str) -> Self {
        Self::new(Provider::ClaudeCode, id)
    }

    pub fn codex(id: &str) -> Self {
        Self::new(Provider::Codex, id)
    }
}

#[derive(Clone, Debug)]
pub struct Tool<'a> {
    pub name: &'a str,
    pub category: ToolCategory,
    pub call_id: Option<&'a str>,
    pub paths: &'a [&'a str],
}

impl<'a> Tool<'a> {
    pub fn edit(call_id: Option<&'a str>, paths: &'a [&'a str]) -> Self {
        Self {
            name: "Edit",
            category: ToolCategory::FileEdit,
            call_id,
            paths,
        }
    }

    pub fn write(call_id: Option<&'a str>, paths: &'a [&'a str]) -> Self {
        Self {
            name: "Write",
            category: ToolCategory::FileWrite,
            call_id,
            paths,
        }
    }

    pub fn read(call_id: Option<&'a str>, paths: &'a [&'a str]) -> Self {
        Self {
            name: "Read",
            category: ToolCategory::FileRead,
            call_id,
            paths,
        }
    }

    pub fn shell(call_id: Option<&'a str>) -> Self {
        Self {
            name: "Bash",
            category: ToolCategory::Shell,
            call_id,
            paths: &[],
        }
    }

    pub fn apply_patch(call_id: Option<&'a str>, paths: &'a [&'a str]) -> Self {
        Self {
            name: "apply_patch",
            category: ToolCategory::FileEdit,
            call_id,
            paths,
        }
    }

    pub fn codex_shell(call_id: Option<&'a str>) -> Self {
        Self {
            name: "shell",
            category: ToolCategory::Shell,
            call_id,
            paths: &[],
        }
    }

    fn to_ref(&self) -> ToolRef {
        ToolRef {
            name: self.name.to_string(),
            category: self.category,
            call_id: self.call_id.map(str::to_string),
        }
    }
}

pub struct Stream {
    device: DeviceId,
    project: ProjectRef,
    capture: CaptureMode,
    seq: u64,
    pub events: Vec<Event>,
}

impl Default for Stream {
    fn default() -> Self {
        Self::new()
    }
}

impl Stream {
    pub fn new() -> Self {
        Self::with_capture(CaptureMode::LocalSemantic)
    }

    pub fn metadata_only() -> Self {
        Self::with_capture(CaptureMode::MetadataOnly)
    }

    pub fn with_capture(capture: CaptureMode) -> Self {
        let device = DeviceId::derive(&["test-device"]);
        let project = ProjectRef::derive(ROOT, Some("git@github.com:acme/repo.git"), &device);
        Self {
            device,
            project,
            capture,
            seq: 0,
            events: Vec::new(),
        }
    }

    pub fn project_id(&self) -> ProjectId {
        self.project.project_id
    }

    fn push(&mut self, s: &Sess, kind: EventKind, name: &str, t: Timestamp) -> &mut Event {
        let mut ev = Event::new(
            self.device,
            s.provider.clone(),
            name,
            kind,
            self.project.clone(),
            s.provider_session_id.clone(),
            self.capture,
            "test-adapter",
        );
        self.seq += 1;
        ev.event_id = EventId::derive(&["test-event", &self.seq.to_string()]);
        ev.observed_at = t;
        ev.captured_at = t;
        self.events.push(ev);
        self.events.last_mut().expect("just pushed")
    }

    fn content_allowed(&self) -> bool {
        self.capture.persists_content_locally()
    }

    fn portable(path: &str) -> PortablePath {
        PortablePath::from_raw(&format!("{ROOT}/{path}"), Some(ROOT))
    }

    pub fn session_started(&mut self, s: &Sess, t: Timestamp) -> EventId {
        let ev = self.push(s, EventKind::SessionStarted, "SessionStart", t);
        ev.attrs.insert("source".into(), Value::from("startup"));
        ev.event_id
    }

    pub fn session_ended(&mut self, s: &Sess, t: Timestamp, reason: &str) -> EventId {
        let ev = self.push(s, EventKind::SessionEnded, "SessionEnd", t);
        ev.attrs.insert("reason".into(), Value::from(reason));
        ev.event_id
    }

    pub fn prompt(&mut self, s: &Sess, t: Timestamp, text: &str) -> EventId {
        let allowed = self.content_allowed();
        let ev = self.push(s, EventKind::PromptSubmitted, "UserPromptSubmit", t);
        if allowed {
            ev.content = Some(EventContent {
                prompt: Some(text.to_string()),
                ..Default::default()
            });
        } else {
            ev.attrs.insert(
                "prompt_chars".into(),
                Value::from(text.chars().count() as u64),
            );
        }
        ev.event_id
    }

    fn tool_event(
        &mut self,
        s: &Sess,
        kind: EventKind,
        name: &str,
        t: Timestamp,
        tool: &Tool<'_>,
    ) -> &mut Event {
        let allowed = self.content_allowed();
        let ev = self.push(s, kind, name, t);
        ev.tool = Some(tool.to_ref());
        ev.paths = tool.paths.iter().map(|p| Self::portable(p)).collect();
        if allowed && tool.category == ToolCategory::Shell {
            ev.content = Some(EventContent {
                command: Some("cargo test --workspace -- --nocapture".to_string()),
                ..Default::default()
            });
        }
        ev
    }

    pub fn tool_start(&mut self, s: &Sess, t: Timestamp, tool: &Tool<'_>) -> EventId {
        self.tool_event(s, EventKind::ToolCallStarted, "PreToolUse", t, tool)
            .event_id
    }

    pub fn tool_finish(
        &mut self,
        s: &Sess,
        t: Timestamp,
        tool: &Tool<'_>,
        outcome: Outcome,
    ) -> EventId {
        let ev = self.tool_event(s, EventKind::ToolCallFinished, "PostToolUse", t, tool);
        ev.outcome = Some(outcome);
        ev.event_id
    }

    /// A finished event without an explicit outcome (treated as success).
    pub fn tool_finish_bare(&mut self, s: &Sess, t: Timestamp, tool: &Tool<'_>) -> EventId {
        self.tool_event(s, EventKind::ToolCallFinished, "PostToolUse", t, tool)
            .event_id
    }

    pub fn tool_failed(&mut self, s: &Sess, t: Timestamp, tool: &Tool<'_>, class: &str) -> EventId {
        let ev = self.tool_event(s, EventKind::ToolCallFailed, "PostToolUseFailure", t, tool);
        ev.outcome = Some(Outcome::failure(Some(class.to_string())));
        ev.event_id
    }

    pub fn tool_denied(&mut self, s: &Sess, t: Timestamp, tool: &Tool<'_>) -> EventId {
        let ev = self.tool_event(s, EventKind::ToolCallFailed, "PostToolUseFailure", t, tool);
        ev.outcome = Some(Outcome::denied());
        ev.event_id
    }

    pub fn stop(&mut self, s: &Sess, t: Timestamp) -> EventId {
        self.push(s, EventKind::TurnStopped, "Stop", t).event_id
    }

    pub fn turn_failed(&mut self, s: &Sess, t: Timestamp, class: &str) -> EventId {
        let ev = self.push(s, EventKind::TurnFailed, "TurnFailed", t);
        ev.attrs.insert("class".into(), Value::from(class));
        ev.event_id
    }

    pub fn permission_requested(&mut self, s: &Sess, t: Timestamp, tool: &Tool<'_>) -> EventId {
        let ev = self.push(s, EventKind::PermissionRequested, "PermissionRequest", t);
        ev.tool = Some(tool.to_ref());
        ev.event_id
    }

    pub fn notification(&mut self, s: &Sess, t: Timestamp, notification_type: &str) -> EventId {
        let ev = self.push(s, EventKind::Notification, "Notification", t);
        ev.attrs
            .insert("notification_type".into(), Value::from(notification_type));
        ev.event_id
    }

    pub fn agent_message(&mut self, s: &Sess, t: Timestamp) -> EventId {
        self.push(s, EventKind::AgentMessage, "AssistantMessage", t)
            .event_id
    }

    pub fn unknown(&mut self, s: &Sess, t: Timestamp) -> EventId {
        self.push(s, EventKind::Unknown, "SomethingNew", t).event_id
    }

    pub fn build(self) -> Vec<Event> {
        self.events
    }
}

/// Handles into the reference story shared by several tests.
pub struct Scenario {
    pub events: Vec<Event>,
    pub project_id: ProjectId,
    pub claude: Sess,
    pub codex: Sess,
    pub claude_prompt_1: EventId,
    pub edit_fail_start: EventId,
    pub edit_fail_end: EventId,
    pub edit_retry_start: EventId,
    pub edit_retry_end: EventId,
    pub bash_start: EventId,
    pub bash_end: EventId,
    pub claude_stop_1: EventId,
    pub claude_end: EventId,
    pub codex_start: EventId,
    pub codex_patch_start: EventId,
}

/// A Claude session with two turns — turn 1 has an Edit failure
/// (`string_mismatch`) followed by a successful Edit on the same path and a
/// Bash test run, then Stop — followed three minutes later by a Codex
/// session touching the same file.
pub fn spec_scenario_with(capture: CaptureMode) -> Scenario {
    let mut b = Stream::with_capture(capture);
    let claude = Sess::claude("claude-session-1");
    let codex = Sess::codex("codex-thread-1");
    let parser = ["src/parser.rs"];
    let readme = ["README.md"];

    b.session_started(&claude, at(0));
    let claude_prompt_1 = b.prompt(&claude, at(5), "Fix the failing parser test");
    b.tool_start(&claude, at(6), &Tool::read(Some("c1"), &parser));
    b.tool_finish(
        &claude,
        at_ms(6_500),
        &Tool::read(Some("c1"), &parser),
        Outcome::success(),
    );
    let edit_fail_start = b.tool_start(&claude, at(10), &Tool::edit(Some("c2"), &parser));
    let edit_fail_end = b.tool_failed(
        &claude,
        at(11),
        &Tool::edit(Some("c2"), &parser),
        "string_mismatch",
    );
    let edit_retry_start = b.tool_start(&claude, at(20), &Tool::edit(Some("c3"), &parser));
    let edit_retry_end = b.tool_finish(
        &claude,
        at(21),
        &Tool::edit(Some("c3"), &parser),
        Outcome::success(),
    );
    let bash_start = b.tool_start(&claude, at(25), &Tool::shell(Some("c4")));
    let bash_end = b.tool_finish(
        &claude,
        at(40),
        &Tool::shell(Some("c4")),
        Outcome {
            status: attemptdb_core::OutcomeStatus::Success,
            class: None,
            exit_code: Some(0),
        },
    );
    let claude_stop_1 = b.stop(&claude, at(45));
    b.prompt(&claude, at(60), "Now document the parser module");
    b.tool_start(&claude, at(61), &Tool::edit(Some("c5"), &readme));
    b.tool_finish(
        &claude,
        at(62),
        &Tool::edit(Some("c5"), &readme),
        Outcome::success(),
    );
    b.stop(&claude, at(70));
    let claude_end = b.session_ended(&claude, at(80), "prompt_input_exit");

    let codex_start = b.session_started(&codex, at(260));
    b.prompt(&codex, at(265), "Continue the parser fix and run the tests");
    let codex_patch_start = b.tool_start(&codex, at(270), &Tool::apply_patch(Some("x1"), &parser));
    b.tool_finish(
        &codex,
        at(272),
        &Tool::apply_patch(Some("x1"), &parser),
        Outcome::success(),
    );
    b.tool_start(&codex, at(280), &Tool::codex_shell(Some("x2")));
    b.tool_finish(
        &codex,
        at(290),
        &Tool::codex_shell(Some("x2")),
        Outcome::success(),
    );
    b.stop(&codex, at(300));
    b.session_ended(&codex, at(310), "exit");

    Scenario {
        project_id: b.project_id(),
        events: b.build(),
        claude,
        codex,
        claude_prompt_1,
        edit_fail_start,
        edit_fail_end,
        edit_retry_start,
        edit_retry_end,
        bash_start,
        bash_end,
        claude_stop_1,
        claude_end,
        codex_start,
        codex_patch_start,
    }
}

pub fn spec_scenario() -> Scenario {
    spec_scenario_with(CaptureMode::LocalSemantic)
}
