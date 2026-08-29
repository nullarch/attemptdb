//! The synthetic workload generator.
//!
//! Sessions are built eagerly (one session at a time, a few thousand events
//! at most), then merged by `observed_at` across up to
//! `GenConfig::concurrency` concurrently active sessions so the stream looks
//! like a developer running a few agents side by side. Everything is a pure
//! function of the seed: ids are derived or built from the seeded RNG,
//! timestamps advance from a fixed synthetic epoch, and no wall-clock value
//! enters the events except `ingested_at`, which the database assigns.
//!
//! Shape of a session (see `model.rs` for every rate):
//!
//! ```text
//! [session_started]
//!   turn: prompt_submitted → chunks of tool calls → turn_stopped [notification]
//!         chunk = main-agent calls, or a subagent dispatch:
//!                 tool_call_started(Agent) → [subagent_started]
//!                 → subagent's tool calls → subagent_stopped × k
//!                 → tool_call_finished(Agent)
//!   ...
//! [session_ended]
//! ```
//!
//! Tool calls are `tool_call_started` / `tool_call_finished` pairs sharing a
//! call id (Cursor only reports the end), a small share fail or are denied,
//! and every content-bearing field is sized from the sampled quantiles and
//! filled with synthetic text.

use crate::model;
use crate::rng::Rng;
use crate::text;
use attemptdb_core::event::{
    AgentRef, EventContent, Outcome, OutcomeStatus, ProjectRef, Provider, ToolCategory, ToolRef,
};
use attemptdb_core::{
    AgentId, CaptureMode, DeviceId, Event, EventId, EventKind, PortablePath, SessionId, Timestamp,
};
use serde_json::{Value, json};
use std::collections::VecDeque;

/// Adapter version stamped on synthetic events.
pub const ADAPTER_VERSION: &str = "bench-synthetic/1";
/// Hook version stamped on hook-captured synthetic events.
pub const HOOK_VERSION: &str = "0.1.0";
/// Provider session id of the chained-failure session used by the causal
/// traversal benchmark.
pub const CHAIN_SESSION_ID: &str = "bench-chain-session";
/// Synthetic time origin: 2026-01-05T09:00:00Z.
pub const EPOCH: Timestamp = Timestamp::from_micros(1_767_603_600_000_000);

#[derive(Clone, Debug)]
pub struct GenConfig {
    pub seed: u64,
    /// Number of events the stream yields.
    pub events: u64,
    pub epoch: Timestamp,
    pub device_id: DeviceId,
    /// Sessions active at the same time.
    pub concurrency: usize,
    /// Attempts in the chained-failure session; `0` omits it.
    pub chain_attempts: usize,
    /// Fraction of the stream after which the chained session starts.
    pub chain_at: f64,
}

impl GenConfig {
    pub fn new(seed: u64, events: u64) -> Self {
        Self {
            seed,
            events,
            epoch: EPOCH,
            device_id: DeviceId::from_bytes(*b"attemptdb-bench!"),
            concurrency: model::CONCURRENT_SESSIONS,
            chain_attempts: 200,
            chain_at: 0.5,
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// A UUIDv7-shaped id whose time part is the synthetic `observed_at` and
/// whose random bits come from the seeded RNG. Time-ordered like the ids
/// real hooks generate, so segment id ranges stay disjoint and the writer's
/// dedup fast path behaves as in production.
pub fn v7_bytes(t: Timestamp, rng: &mut Rng) -> [u8; 16] {
    let ms = (t.as_micros().max(0) / 1000) as u64;
    let r1 = rng.next_u64();
    let r2 = rng.next_u64();
    let mut b = [0u8; 16];
    b[..6].copy_from_slice(&ms.to_be_bytes()[2..8]);
    b[6] = 0x70 | ((r1 >> 8) as u8 & 0x0f);
    b[7] = r1 as u8;
    b[8..].copy_from_slice(&r2.to_be_bytes());
    b[8] = (b[8] & 0x3f) | 0x80;
    b
}

fn plus_secs(t: Timestamp, secs: f64) -> Timestamp {
    Timestamp::from_micros(t.as_micros() + (secs * 1_000_000.0) as i64)
}

fn plus_ms(t: Timestamp, ms: u64) -> Timestamp {
    Timestamp::from_micros(t.as_micros() + (ms as i64) * 1_000)
}

#[derive(Clone, Debug)]
pub struct ProjectSpec {
    pub root: String,
    pub remote: String,
    pub branch: String,
    pub head: String,
    /// `(repository-relative path, extension)` pool, hottest first.
    pub paths: Vec<(String, String)>,
}

impl ProjectSpec {
    fn new(rng: &mut Rng) -> Self {
        let name = text::project_name(rng);
        let mut paths = Vec::with_capacity(model::PATHS_PER_PROJECT);
        for _ in 0..model::PATHS_PER_PROJECT {
            let ext = *rng.weighted(model::FILE_EXT_MIX);
            let depth = *rng.weighted(model::PATH_DEPTH);
            paths.push((text::relative_path(rng, depth, ext), ext.to_string()));
        }
        Self {
            root: format!("/home/dev/work/{name}"),
            remote: format!("git@example.invalid:bench/{name}.git"),
            branch: if rng.chance(0.7) {
                "main".to_string()
            } else {
                format!("feat/{}", text::description(rng).replace(' ', "-"))
            },
            head: rng.hex(40),
            paths,
        }
    }

    fn project_ref(&self, device: &DeviceId) -> ProjectRef {
        let mut p = ProjectRef::derive(&self.root, Some(&self.remote), device);
        p.branch = Some(self.branch.clone());
        p.head = Some(self.head.clone());
        p
    }

    /// Zipf-like pick: the square of a uniform draw concentrates on the
    /// first entries.
    fn pick_path(&self, rng: &mut Rng) -> &(String, String) {
        let u = rng.f64();
        let i = ((u * u) * self.paths.len() as f64) as usize;
        &self.paths[i.min(self.paths.len() - 1)]
    }
}

#[derive(Clone, Debug)]
struct Agent {
    id: AgentId,
    parent: Option<AgentId>,
    agent_type: Option<String>,
    provider_agent_id: Option<String>,
    is_sub: bool,
}

/// What a provider calls things.
fn event_name(provider: &Provider, kind: EventKind, reconstructed: bool) -> &'static str {
    if reconstructed {
        return match kind {
            EventKind::SessionStarted => "transcript:session_start",
            EventKind::SessionEnded => "transcript:session_end",
            EventKind::PromptSubmitted => "transcript:user",
            EventKind::ToolCallStarted => "transcript:assistant:tool_use",
            EventKind::ToolCallFinished | EventKind::ToolCallFailed => {
                "transcript:user:tool_result"
            }
            EventKind::AgentMessage => "transcript:assistant:text",
            EventKind::TurnStopped => "transcript:turn_end",
            EventKind::SubagentStarted => "transcript:subagent_start",
            EventKind::SubagentStopped => "transcript:subagent_stop",
            _ => "transcript:unknown",
        };
    }
    match provider {
        Provider::Cursor => match kind {
            EventKind::SessionStarted => "sessionStart",
            EventKind::SessionEnded => "sessionEnd",
            EventKind::PromptSubmitted => "beforeSubmitPrompt",
            EventKind::TurnStopped => "stop",
            EventKind::ToolCallFinished => "afterShellExecution",
            EventKind::ToolCallFailed => "postToolUseFailure",
            _ => "unknown",
        },
        Provider::GeminiCli => match kind {
            EventKind::SessionStarted => "SessionStart",
            EventKind::SessionEnded => "SessionEnd",
            EventKind::PromptSubmitted => "BeforeAgent",
            EventKind::TurnStopped => "AfterAgent",
            EventKind::ToolCallStarted => "BeforeTool",
            EventKind::ToolCallFinished | EventKind::ToolCallFailed => "AfterTool",
            _ => "unknown",
        },
        Provider::Codex => match kind {
            EventKind::SessionStarted => "SessionStart",
            EventKind::SessionEnded => "SessionEnd",
            EventKind::PromptSubmitted => "UserPromptSubmit",
            EventKind::ToolCallStarted => "PreToolUse",
            EventKind::ToolCallFinished | EventKind::ToolCallFailed => "PostToolUse",
            EventKind::PermissionDenied => "PermissionRequest",
            EventKind::TurnStopped => "Stop",
            EventKind::SubagentStarted => "SubagentStart",
            EventKind::SubagentStopped => "SubagentStop",
            _ => "unknown",
        },
        _ => match kind {
            EventKind::SessionStarted => "SessionStart",
            EventKind::SessionEnded => "SessionEnd",
            EventKind::PromptSubmitted => "UserPromptSubmit",
            EventKind::ToolCallStarted => "PreToolUse",
            EventKind::ToolCallFinished => "PostToolUse",
            EventKind::ToolCallFailed => "PostToolUseFailure",
            EventKind::PermissionDenied => "PermissionDenied",
            EventKind::TurnStopped => "Stop",
            EventKind::SubagentStarted => "SubagentStart",
            EventKind::SubagentStopped => "SubagentStop",
            EventKind::Notification => "Notification",
            EventKind::CaptureTest => "AttemptDBCaptureTest",
            _ => "unknown",
        },
    }
}

fn tool_name(provider: &Provider, cat: ToolCategory) -> &'static str {
    match provider {
        Provider::Codex => match cat {
            ToolCategory::Shell => "shell",
            ToolCategory::FileEdit => "apply_patch",
            ToolCategory::FileRead => "read_file",
            ToolCategory::FileWrite => "write_file",
            ToolCategory::Subagent => "spawn_agent",
            ToolCategory::Web => "web_search",
            ToolCategory::Search => "file_search",
            ToolCategory::Mcp => "mcp__tools__lookup",
            _ => "update_plan",
        },
        Provider::Cursor => match cat {
            ToolCategory::Shell => "Shell",
            _ => "Edit",
        },
        Provider::GeminiCli => match cat {
            ToolCategory::Shell => "run_shell_command",
            ToolCategory::FileEdit => "replace",
            ToolCategory::FileRead => "read_file",
            ToolCategory::FileWrite => "write_file",
            ToolCategory::Web => "web_fetch",
            ToolCategory::Search => "grep_search",
            ToolCategory::Mcp => "mcp__tools__lookup",
            _ => "glob",
        },
        _ => match cat {
            ToolCategory::Shell => "Bash",
            ToolCategory::FileEdit => "Edit",
            ToolCategory::FileRead => "Read",
            ToolCategory::FileWrite => "Write",
            ToolCategory::Subagent => "Agent",
            ToolCategory::Web => "WebFetch",
            ToolCategory::Search => "Grep",
            ToolCategory::Mcp => "mcp__tools__lookup",
            _ => "ToolSearch",
        },
    }
}

fn supports_subagents(provider: &Provider) -> bool {
    matches!(provider, Provider::ClaudeCode | Provider::Codex)
}

fn supports_start_events(provider: &Provider) -> bool {
    !matches!(provider, Provider::Cursor)
}

fn supports_notifications(provider: &Provider) -> bool {
    matches!(provider, Provider::ClaudeCode)
}

const PERMISSION_MODES: &[(&str, f64)] = &[
    ("default", 0.80),
    ("acceptEdits", 0.15),
    ("bypassPermissions", 0.05),
];

const COMMAND_CATEGORIES: &[(&str, f64)] = &[
    ("build", 0.25),
    ("test", 0.20),
    ("git", 0.15),
    ("search", 0.15),
    ("file", 0.15),
    ("other", 0.10),
];

const MODELS: &[&str] = &["model-large-2026", "model-medium-2026", "model-small-2025"];

/// A tool call's content, shared by its start and end events.
struct ToolPayload {
    input: Value,
    command: Option<String>,
    output: Value,
}

// ---------------------------------------------------------------------------
// Session builder
// ---------------------------------------------------------------------------

struct SessionBuilder<'a> {
    rng: Rng,
    cfg: &'a GenConfig,
    project: &'a ProjectSpec,
    project_ref: ProjectRef,
    provider: Provider,
    psid: String,
    reconstructed: bool,
    model: Option<String>,
    main: Agent,
    t: Timestamp,
    turn_index: u32,
    ordinal: u64,
    out: Vec<Event>,
}

impl<'a> SessionBuilder<'a> {
    fn new(
        rng: Rng,
        cfg: &'a GenConfig,
        project: &'a ProjectSpec,
        provider: Provider,
        psid: String,
        reconstructed: bool,
        start: Timestamp,
    ) -> Self {
        let mut rng = rng;
        let session_id = SessionId::derive(&[provider.as_str(), &psid]);
        let main = Agent {
            id: AgentId::derive(&["session", &session_id.to_string()]),
            parent: None,
            agent_type: None,
            provider_agent_id: None,
            is_sub: false,
        };
        let model = if rng.chance(model::MODEL_PRESENT_RATE) {
            Some((*rng.pick(MODELS)).to_string())
        } else {
            None
        };
        Self {
            rng,
            cfg,
            project,
            project_ref: project.project_ref(&cfg.device_id),
            provider,
            psid,
            reconstructed,
            model,
            main,
            t: start,
            turn_index: 0,
            ordinal: 0,
            out: Vec::new(),
        }
    }

    fn hook_captured(&self) -> bool {
        !self.reconstructed
    }

    fn base(&mut self, kind: EventKind, t: Timestamp, agent: &Agent) -> Event {
        let name = event_name(&self.provider, kind, self.reconstructed);
        let mut ev = Event::new(
            self.cfg.device_id,
            self.provider.clone(),
            name,
            kind,
            self.project_ref.clone(),
            self.psid.clone(),
            CaptureMode::LocalSemantic,
            ADAPTER_VERSION,
        );
        ev.event_id = EventId::from_bytes(v7_bytes(t, &mut self.rng));
        ev.observed_at = t;
        ev.captured_at = if self.reconstructed {
            t
        } else {
            Timestamp::from_micros(t.as_micros() + self.rng.range(200, 3_000) as i64)
        };
        if self.hook_captured() {
            ev.hook_version = Some(HOOK_VERSION.to_string());
        }
        ev.agent = AgentRef {
            agent_id: agent.id,
            provider_agent_id: agent.provider_agent_id.clone(),
            agent_type: agent.agent_type.clone(),
            parent_agent_id: agent.parent,
            model: self.model.clone(),
        };
        ev.attrs
            .insert("cwd".into(), Value::String(self.project.root.clone()));
        ev.attrs
            .insert("transcript_present".into(), Value::Bool(true));
        if agent.is_sub {
            ev.attrs.insert("is_subagent".into(), Value::Bool(true));
            if let Some(t) = &agent.agent_type {
                ev.attrs
                    .insert("agent_type".into(), Value::String(t.clone()));
            }
        }
        if self.hook_captured() {
            ev.attrs
                .insert("hook_event_name".into(), Value::String(name.to_string()));
            ev.attrs.insert(
                "hook_us".into(),
                json!(self.rng.sample(&model::HOOK_US).round() as u64),
            );
            ev.attrs.insert(
                "permission_mode".into(),
                Value::String((*self.rng.weighted(PERMISSION_MODES)).to_string()),
            );
        } else {
            ev.attrs.insert("reconstructed".into(), Value::Bool(true));
            ev.attrs.insert(
                "reconstructed_from".into(),
                Value::String("transcript".into()),
            );
            ev.attrs.insert(
                "transcript_entry_type".into(),
                Value::String(
                    match kind {
                        EventKind::PromptSubmitted | EventKind::ToolCallFinished => "user",
                        _ => "assistant",
                    }
                    .into(),
                ),
            );
            if agent.is_sub {
                ev.attrs.insert("is_sidechain".into(), Value::Bool(true));
            }
            ev.attrs
                .insert("turn_index_hint".into(), json!(self.turn_index));
        }
        ev
    }

    fn emit(&mut self, ev: Event) {
        self.out.push(ev);
    }

    fn call_id(&mut self) -> Option<String> {
        self.ordinal += 1;
        match self.provider {
            Provider::ClaudeCode => Some(format!("toolu_{}", self.rng.hex(24))),
            Provider::Codex => Some(format!("call_{}", self.rng.hex(20))),
            Provider::GeminiCli => Some(format!("{}-{}", self.rng.hex(8), self.ordinal)),
            _ => None,
        }
    }

    fn raw_payload(&self, name: &str, extra: Value) -> Value {
        let mut raw = json!({
            "hook_event_name": name,
            "session_id": self.psid,
            "cwd": self.project.root,
            "transcript_path": format!("{}/.transcripts/{}.jsonl", self.project.root, self.psid),
            "permission_mode": "default",
        });
        if let (Value::Object(r), Value::Object(e)) = (&mut raw, extra) {
            r.extend(e);
        }
        raw
    }

    // --- content -----------------------------------------------------------

    fn tool_payload(&mut self, cat: ToolCategory, path: Option<&str>) -> ToolPayload {
        let rng = &mut self.rng;
        let abs = |p: &str| format!("{}/{}", self.project.root, p);
        match cat {
            ToolCategory::Shell => {
                let command_len = rng.len(&model::SHELL_COMMAND);
                let command = text::shell_command(rng, command_len);
                let mut input = json!({
                    "command": command,
                    "description": text::description(rng),
                });
                if rng.chance(0.21) {
                    input["timeout"] = json!(rng.range(30_000, 600_000));
                }
                let out_len = rng.len(&model::SHELL_OUTPUT);
                let stdout = text::log(rng, out_len);
                let output = if rng.chance(model::SHELL_OUTPUT_STRING_RATE) {
                    Value::String(stdout)
                } else {
                    json!({
                        "stdout": stdout,
                        "stderr": "",
                        "interrupted": false,
                        "isImage": false,
                        "noOutputExpected": false,
                    })
                };
                ToolPayload {
                    input,
                    command: Some(command),
                    output,
                }
            }
            ToolCategory::FileEdit => {
                let p = path.unwrap_or("src/lib.rs");
                let total = rng.len(&model::EDIT_INPUT);
                let old = text::code(rng, total * 45 / 100);
                let new = text::code(rng, total * 45 / 100);
                let input = json!({
                    "file_path": abs(p),
                    "old_string": old,
                    "new_string": new,
                    "replace_all": false,
                });
                let output = if rng.chance(model::EDIT_OUTPUT_OBJECT_RATE) {
                    let original_len = rng.len(&model::EDIT_OUTPUT_OBJECT);
                    json!({
                        "filePath": abs(p),
                        "oldString": input["old_string"],
                        "newString": input["new_string"],
                        "originalFile": text::code(rng, original_len),
                        "structuredPatch": [{"oldStart": rng.range(1, 400), "oldLines": rng.range(1, 30), "newStart": rng.range(1, 400), "newLines": rng.range(1, 30)}],
                        "userModified": false,
                        "replaceAll": false,
                    })
                } else {
                    Value::String(format!(
                        "The file {} has been updated. Here's the result of running a numbered listing on a snippet of the edited file:\n{}",
                        abs(p),
                        text::code(rng, 60)
                    ))
                };
                ToolPayload {
                    input,
                    command: None,
                    output,
                }
            }
            ToolCategory::FileRead => {
                let p = path.unwrap_or("src/lib.rs");
                let mut input = json!({ "file_path": abs(p) });
                if rng.chance(0.13) {
                    input["limit"] = json!(rng.range(20, 400));
                }
                if rng.chance(0.11) {
                    input["offset"] = json!(rng.range(1, 2_000));
                }
                let read_len = rng.len(&model::READ_OUTPUT);
                let content = text::code(rng, read_len);
                let lines = content.matches('\n').count() as u64 + 1;
                let output = if rng.chance(model::READ_OUTPUT_OBJECT_RATE) {
                    json!({
                        "file": {
                            "filePath": abs(p),
                            "content": content,
                            "numLines": lines,
                            "startLine": 1,
                            "totalLines": lines + rng.range(0, 500),
                        },
                        "type": "text",
                    })
                } else {
                    Value::String(content)
                };
                ToolPayload {
                    input,
                    command: None,
                    output,
                }
            }
            ToolCategory::FileWrite => {
                let p = path.unwrap_or("src/lib.rs");
                let write_len = rng.len(&model::WRITE_INPUT);
                let content = text::code(rng, write_len);
                let input = json!({ "file_path": abs(p), "content": content });
                let output = if rng.chance(model::WRITE_OUTPUT_OBJECT_RATE) {
                    let echo_len = rng.len(&model::WRITE_OUTPUT_OBJECT);
                    json!({
                        "type": if rng.chance(0.5) { "create" } else { "update" },
                        "filePath": abs(p),
                        "content": text::code(rng, echo_len),
                        "structuredPatch": [],
                        "originalFile": Value::Null,
                        "userModified": false,
                    })
                } else {
                    Value::String(format!(
                        "File {} written successfully ({} bytes, {} lines).",
                        abs(p),
                        rng.range(200, 60_000),
                        rng.range(5, 1_500)
                    ))
                };
                ToolPayload {
                    input,
                    command: None,
                    output,
                }
            }
            ToolCategory::Subagent => {
                let prompt_len = rng.len(&model::SUBAGENT_INPUT);
                let input = json!({
                    "description": text::description(rng),
                    "prompt": text::prose(rng, prompt_len),
                    "subagent_type": *rng.weighted(model::SUBAGENT_TYPES),
                });
                let result_len = rng.len(&model::SUBAGENT_OUTPUT);
                let result = text::prose(rng, result_len);
                let output = if rng.chance(0.36) {
                    json!({
                        "agentId": rng.hex(8),
                        "description": input["description"],
                        "isAsync": false,
                        "status": "completed",
                        "result": result,
                        "totalDurationMs": rng.range(5_000, 900_000),
                        "totalToolUseCount": rng.range(1, 80),
                    })
                } else {
                    Value::String(result)
                };
                ToolPayload {
                    input,
                    command: None,
                    output,
                }
            }
            ToolCategory::Web => {
                let section = *rng.pick(&["guide", "reference", "api"]);
                let url_path = text::relative_path(rng, 2, "html");
                let prompt_len = rng.len(&model::WEB_INPUT) / 2;
                let input = json!({
                    "url": format!("https://docs.example.invalid/{section}/{url_path}"),
                    "prompt": text::prose(rng, prompt_len),
                });
                let out_len = rng.len(&model::WEB_OUTPUT);
                let output = Value::String(text::prose(rng, out_len));
                ToolPayload {
                    input,
                    command: None,
                    output,
                }
            }
            _ => {
                let input = json!({
                    "query": text::description(rng),
                    "max_results": rng.range(3, 20),
                });
                let out_len = rng.len(&model::SMALL_OUTPUT);
                let output = if rng.chance(0.5) {
                    json!({"matches": text::log(rng, out_len), "total": rng.range(0, 40)})
                } else {
                    Value::String(text::log(rng, out_len))
                };
                ToolPayload {
                    input,
                    command: None,
                    output,
                }
            }
        }
    }

    fn tool_attrs(
        &mut self,
        ev: &mut Event,
        cat: ToolCategory,
        payload: &ToolPayload,
        path: Option<&(String, String)>,
    ) {
        match cat {
            ToolCategory::Shell => {
                let bytes = payload.command.as_ref().map(String::len).unwrap_or(0);
                ev.attrs.insert("command_bytes".into(), json!(bytes));
                let category = *self.rng.weighted(COMMAND_CATEGORIES);
                ev.attrs
                    .insert("command_category".into(), Value::String(category.into()));
                if category == "git" {
                    ev.attrs.insert(
                        "git_subcommand".into(),
                        Value::String(
                            (*self
                                .rng
                                .pick(&["status", "diff", "log", "add", "commit", "push"]))
                            .into(),
                        ),
                    );
                }
            }
            ToolCategory::FileEdit | ToolCategory::FileRead | ToolCategory::FileWrite => {
                if let Some((rel, ext)) = path {
                    ev.attrs
                        .insert("file_ext".into(), Value::String(ext.clone()));
                    ev.attrs.insert(
                        "file_is_test".into(),
                        Value::Bool(rel.contains("tests/") || rel.contains("fixtures/")),
                    );
                    ev.attrs.insert(
                        "file_is_config".into(),
                        Value::Bool(ext == "toml" || ext == "json"),
                    );
                    ev.attrs
                        .insert("file_is_doc".into(), Value::Bool(ext == "md"));
                }
                if cat != ToolCategory::FileRead {
                    ev.attrs
                        .insert("lines_added".into(), json!(self.rng.range(0, 120)));
                    ev.attrs
                        .insert("lines_removed".into(), json!(self.rng.range(0, 60)));
                }
            }
            _ => {}
        }
    }

    // --- emitters ------------------------------------------------------------

    fn session_started(&mut self) {
        let main = self.main.clone();
        let mut ev = self.base(EventKind::SessionStarted, self.t, &main);
        let source = if self.rng.chance(0.8) {
            "startup"
        } else {
            "resume"
        };
        ev.attrs
            .insert("source".into(), Value::String(source.into()));
        if self.hook_captured() {
            ev.raw = Some(self.raw_payload("SessionStart", json!({"source": source})));
        }
        self.emit(ev);
        self.t = plus_ms(self.t, self.rng.range(300, 4_000));
    }

    fn session_ended(&mut self) {
        let main = self.main.clone();
        let mut ev = self.base(EventKind::SessionEnded, self.t, &main);
        ev.attrs.insert(
            "reason".into(),
            Value::String((*self.rng.pick(&["prompt_input_exit", "other", "logout"])).into()),
        );
        self.emit(ev);
    }

    fn prompt(&mut self) {
        let main = self.main.clone();
        let mut ev = self.base(EventKind::PromptSubmitted, self.t, &main);
        let len = self.rng.len(&model::PROMPT);
        let prompt = text::prose(&mut self.rng, len);
        ev.attrs
            .insert("prompt_chars".into(), json!(prompt.chars().count()));
        ev.attrs
            .insert("prompt_lines".into(), json!(prompt.lines().count()));
        ev.attrs.insert(
            "prompt_has_code_fence".into(),
            Value::Bool(prompt.contains("```")),
        );
        ev.attrs.insert(
            "prompt_has_question".into(),
            Value::Bool(prompt.contains('?')),
        );
        if self.hook_captured() {
            ev.raw = Some(self.raw_payload("UserPromptSubmit", json!({"prompt": prompt})));
        }
        ev.content = Some(EventContent {
            prompt: Some(prompt),
            ..Default::default()
        });
        self.emit(ev);
        // Time to first action.
        self.t = plus_secs(self.t, self.rng.range(2, 25) as f64);
    }

    fn agent_message(&mut self, agent: &Agent) {
        let mut ev = self.base(EventKind::AgentMessage, self.t, agent);
        let len = self.rng.len(&model::AGENT_MESSAGE);
        ev.content = Some(EventContent {
            message: Some(text::prose(&mut self.rng, len)),
            ..Default::default()
        });
        self.emit(ev);
        self.t = plus_ms(self.t, self.rng.range(500, 8_000));
    }

    fn notification(&mut self) {
        let main = self.main.clone();
        let mut ev = self.base(EventKind::Notification, self.t, &main);
        let kind = *self.rng.weighted(model::NOTIFICATION_TYPES);
        ev.attrs
            .insert("notification_type".into(), Value::String(kind.into()));
        let message = match kind {
            "idle_prompt" => "The agent is waiting for your input".to_string(),
            "auth_success" => "Signed in".to_string(),
            _ => {
                let len = self.rng.range(40, 300) as usize;
                text::prose(&mut self.rng, len)
            }
        };
        if self.hook_captured() {
            ev.raw = Some(self.raw_payload(
                "Notification",
                json!({"notification_type": kind, "message": message}),
            ));
        }
        ev.content = Some(EventContent {
            message: Some(message),
            ..Default::default()
        });
        self.emit(ev);
    }

    fn turn_stop(&mut self) {
        let main = self.main.clone();
        let mut ev = self.base(EventKind::TurnStopped, self.t, &main);
        ev.attrs
            .insert("stop_hook_active".into(), Value::Bool(false));
        if self.rng.chance(model::TURN_STOP_MESSAGE_RATE) {
            let len = self.rng.len(&model::TURN_STOP_MESSAGE);
            let message = text::prose(&mut self.rng, len);
            if self.hook_captured() {
                ev.raw = Some(self.raw_payload(
                    "Stop",
                    json!({"stop_hook_active": false, "last_assistant_message": message}),
                ));
            }
            ev.content = Some(EventContent {
                message: Some(message),
                ..Default::default()
            });
        } else if self.hook_captured() {
            ev.raw = Some(self.raw_payload("Stop", json!({"stop_hook_active": false})));
        }
        self.emit(ev);
    }

    fn subagent_started(&mut self, sub: &Agent, prompt: &str) {
        let mut ev = self.base(EventKind::SubagentStarted, self.t, sub);
        if self.reconstructed {
            ev.attrs
                .insert("prompt_chars".into(), json!(prompt.chars().count()));
            ev.content = Some(EventContent {
                prompt: Some(prompt.to_string()),
                ..Default::default()
            });
        } else {
            ev.raw = Some(self.raw_payload(
                "SubagentStart",
                json!({"agent_id": sub.provider_agent_id, "agent_type": sub.agent_type}),
            ));
        }
        self.emit(ev);
        self.t = plus_ms(self.t, self.rng.range(200, 3_000));
    }

    fn subagent_stopped(&mut self, sub: &Agent) {
        let mut ev = self.base(EventKind::SubagentStopped, self.t, sub);
        ev.attrs
            .insert("stop_hook_active".into(), Value::Bool(false));
        let len = self.rng.len(&model::SUBAGENT_STOP_MESSAGE);
        let message = text::prose(&mut self.rng, len);
        if self.hook_captured() {
            ev.raw = Some(self.raw_payload(
                "SubagentStop",
                json!({"agent_id": sub.provider_agent_id, "agent_type": sub.agent_type, "stop_hook_active": false, "last_assistant_message": message}),
            ));
        }
        ev.content = Some(EventContent {
            message: Some(message),
            ..Default::default()
        });
        self.emit(ev);
        self.t = plus_ms(self.t, self.rng.range(50, 2_500));
    }

    /// One tool call: start event (when the provider reports it), then the
    /// end event after the tool's duration, then think time. Returns the
    /// outcome status.
    fn tool_call(
        &mut self,
        agent: &Agent,
        cat: ToolCategory,
        forced_path: Option<&(String, String)>,
        forced_failure: Option<&'static str>,
    ) -> OutcomeStatus {
        let path: Option<(String, String)> = match cat {
            ToolCategory::FileEdit | ToolCategory::FileRead | ToolCategory::FileWrite => Some(
                forced_path
                    .cloned()
                    .unwrap_or_else(|| self.project.pick_path(&mut self.rng).clone()),
            ),
            _ => None,
        };
        let payload = self.tool_payload(cat, path.as_ref().map(|p| p.0.as_str()));
        let name = tool_name(&self.provider, cat);
        let call_id = self.call_id();
        let tool = ToolRef {
            name: name.to_string(),
            category: cat,
            call_id: call_id.clone(),
        };
        let portable = path.as_ref().map(|(rel, _)| {
            PortablePath::from_raw(
                &format!("{}/{}", self.project.root, rel),
                Some(&self.project.root),
            )
        });
        let start = self.t;
        if supports_start_events(&self.provider) {
            let mut ev = self.base(EventKind::ToolCallStarted, start, agent);
            ev.tool = Some(tool.clone());
            ev.paths.extend(portable.clone());
            self.tool_attrs(&mut ev, cat, &payload, path.as_ref());
            if self.hook_captured() {
                ev.raw = Some(self.raw_payload(
                    event_name(&self.provider, EventKind::ToolCallStarted, false),
                    json!({"tool_name": name, "tool_use_id": call_id.clone(), "tool_input": payload.input.clone()}),
                ));
            }
            ev.content = Some(EventContent {
                command: payload.command.clone(),
                tool_input: Some(payload.input.clone()),
                ..Default::default()
            });
            self.emit(ev);
        }

        let denied = forced_failure.is_none() && self.rng.chance(model::PERMISSION_DENIED_RATE);
        let duration = self.rng.sample(&model::duration_ms(cat)).round() as u64;
        let end = plus_ms(start, if denied { 5 } else { duration });
        let failed = forced_failure.is_some() || self.rng.chance(model::failure_rate(cat));
        let kind = if denied {
            EventKind::PermissionDenied
        } else if failed {
            EventKind::ToolCallFailed
        } else {
            EventKind::ToolCallFinished
        };
        let mut ev = self.base(kind, end, agent);
        ev.tool = Some(tool);
        ev.paths.extend(portable);
        ev.duration_ms = Some(duration);
        self.tool_attrs(&mut ev, cat, &payload, path.as_ref());
        let status;
        let mut content = EventContent {
            command: payload.command.clone(),
            tool_input: Some(payload.input.clone()),
            ..Default::default()
        };
        let mut raw_extra =
            json!({"tool_name": name, "tool_use_id": call_id, "tool_input": payload.input});
        if denied {
            status = OutcomeStatus::Denied;
            ev.outcome = Some(Outcome::denied());
            content
                .extra
                .insert("reason".into(), Value::String("denied by policy".into()));
            raw_extra["reason"] = Value::String("denied by policy".into());
        } else if failed {
            status = OutcomeStatus::Failure;
            let class =
                forced_failure.unwrap_or_else(|| *self.rng.weighted(model::failure_classes(cat)));
            let exit_code = (class == "nonzero_exit").then(|| self.rng.range(1, 2) as i32);
            ev.outcome = Some(Outcome {
                status: OutcomeStatus::Failure,
                class: Some(class.to_string()),
                exit_code,
            });
            let err_len = self.rng.len(&model::FAILURE_ERROR);
            let error = text::log(&mut self.rng, err_len);
            ev.attrs
                .insert("error_class".into(), Value::String(class.into()));
            ev.attrs.insert("error_bytes".into(), json!(error.len()));
            if self.rng.chance(0.5) {
                content.tool_output = Some(Value::String(error.clone()));
            }
            raw_extra["error"] = Value::String(error.clone());
            content.error = Some(error);
        } else {
            status = OutcomeStatus::Success;
            ev.outcome = Some(Outcome {
                status: OutcomeStatus::Success,
                class: None,
                exit_code: (cat == ToolCategory::Shell).then_some(0),
            });
            raw_extra["tool_response"] = payload.output.clone();
            content.tool_output = Some(payload.output);
        }
        if self.hook_captured() {
            ev.raw = Some(self.raw_payload(event_name(&self.provider, kind, false), raw_extra));
        }
        ev.content = Some(content);
        self.emit(ev);
        self.t = end;
        if self.reconstructed && self.rng.chance(model::AGENT_MESSAGE_PER_CALL) {
            let a = agent.clone();
            self.agent_message(&a);
        }
        let think = self.rng.sample(&model::THINK_GAP_SECS);
        self.t = plus_secs(self.t, think);
        status
    }

    fn random_category(&mut self) -> ToolCategory {
        let cat = *self.rng.weighted(model::TOOL_MIX);
        match (&self.provider, cat) {
            (_, ToolCategory::Subagent) => ToolCategory::Shell,
            (Provider::Cursor, ToolCategory::Shell) => ToolCategory::Shell,
            (Provider::Cursor, _) => ToolCategory::FileEdit,
            (Provider::GeminiCli, ToolCategory::Mcp) => ToolCategory::Search,
            _ => cat,
        }
    }

    /// Dispatch a subagent that performs `k` tool calls.
    fn subagent_chunk(&mut self, parent: &Agent, k: usize) {
        let provider_agent_id = self.rng.hex(8);
        let sub = Agent {
            id: AgentId::derive(&["agent", &provider_agent_id]),
            parent: Some(parent.id),
            agent_type: Some((*self.rng.weighted(model::SUBAGENT_TYPES)).to_string()),
            provider_agent_id: Some(provider_agent_id),
            is_sub: true,
        };
        let payload = self.tool_payload(ToolCategory::Subagent, None);
        let name = tool_name(&self.provider, ToolCategory::Subagent);
        let call_id = self.call_id();
        let tool = ToolRef {
            name: name.to_string(),
            category: ToolCategory::Subagent,
            call_id: call_id.clone(),
        };
        let start = self.t;
        let mut ev = self.base(EventKind::ToolCallStarted, start, parent);
        ev.tool = Some(tool.clone());
        if self.hook_captured() {
            ev.raw = Some(self.raw_payload(
                event_name(&self.provider, EventKind::ToolCallStarted, false),
                json!({"tool_name": name, "tool_use_id": call_id.clone(), "tool_input": payload.input.clone()}),
            ));
        }
        ev.content = Some(EventContent {
            tool_input: Some(payload.input.clone()),
            ..Default::default()
        });
        self.emit(ev);
        self.t = plus_ms(self.t, self.rng.range(100, 2_000));

        let prompt = payload.input["prompt"].as_str().unwrap_or("").to_string();
        if self.rng.chance(model::SUBAGENT_STARTED_RATE) {
            self.subagent_started(&sub, &prompt);
        }
        for _ in 0..k {
            let cat = self.random_category();
            self.tool_call(&sub, cat, None, None);
        }
        let stops = self.rng.sample(&model::SUBAGENT_STOPS_PER_SUBAGENT).round() as usize;
        for _ in 0..stops.max(1) {
            self.subagent_stopped(&sub);
        }

        let end = self.t;
        let mut ev = self.base(EventKind::ToolCallFinished, end, parent);
        ev.tool = Some(tool);
        ev.duration_ms = Some(((end.as_micros() - start.as_micros()) / 1_000).max(0) as u64);
        ev.outcome = Some(Outcome::success());
        if self.hook_captured() {
            ev.raw = Some(self.raw_payload(
                event_name(&self.provider, EventKind::ToolCallFinished, false),
                json!({"tool_name": name, "tool_use_id": call_id, "tool_input": payload.input.clone(), "tool_response": payload.output.clone()}),
            ));
        }
        ev.content = Some(EventContent {
            tool_input: Some(payload.input),
            tool_output: Some(payload.output),
            ..Default::default()
        });
        self.emit(ev);
        let think = self.rng.sample(&model::THINK_GAP_SECS);
        self.t = plus_secs(self.t, think);
    }

    fn turn(&mut self, implicit: bool) {
        if !implicit {
            self.turn_index += 1;
            self.prompt();
        }
        let n_calls = self.rng.sample(&model::TOOL_CALLS_PER_TURN).round() as usize;
        let mut remaining = n_calls.max(1);
        let main = self.main.clone();
        while remaining > 0 {
            if supports_subagents(&self.provider)
                && remaining >= 6
                && self.rng.chance(model::SUBAGENT_CALL_SHARE * 0.65)
            {
                let k = (self.rng.range(5, 60) as usize).min(remaining);
                self.subagent_chunk(&main, k);
                remaining -= k;
            } else {
                let k = (self.rng.range(1, 16) as usize).min(remaining);
                for _ in 0..k {
                    let cat = self.random_category();
                    self.tool_call(&main, cat, None, None);
                }
                remaining -= k;
            }
        }
        self.turn_stop();
        if self.rng.chance(model::EXTRA_STOP_RATE) {
            self.t = plus_secs(self.t, self.rng.range(1, 30) as f64);
            self.turn_stop();
        }
        if supports_notifications(&self.provider) && self.rng.chance(model::IDLE_NOTIFICATION_RATE)
        {
            self.t = plus_secs(self.t, 60.0);
            self.notification();
        }
        // The human reads and types the next prompt.
        self.t = plus_secs(self.t, self.rng.range(5, 600) as f64);
    }

    fn build(mut self) -> Vec<Event> {
        if self.rng.chance(model::SESSION_STARTED_RATE) {
            self.session_started();
        }
        let turns = self.rng.sample(&model::TURNS_PER_SESSION).round() as usize;
        let implicit = self.rng.chance(model::IMPLICIT_FIRST_TURN_RATE);
        for i in 0..turns.max(1) {
            self.turn(implicit && i == 0);
        }
        if self.rng.chance(model::SESSION_ENDED_RATE) {
            self.session_ended();
        }
        self.out
    }

    /// The causal-traversal fixture: one turn whose `n` sequential edits of
    /// the same file all fail, so the projection yields `n` attempts each
    /// superseded by the next.
    fn build_chain(mut self, n: usize) -> Vec<Event> {
        self.session_started();
        self.turn_index = 1;
        self.prompt();
        let path = self.project.paths[0].clone();
        let main = self.main.clone();
        for _ in 0..n {
            self.tool_call(
                &main,
                ToolCategory::FileEdit,
                Some(&path),
                Some("string_mismatch"),
            );
        }
        self.turn_stop();
        self.out
    }
}

// ---------------------------------------------------------------------------
// Workload stream
// ---------------------------------------------------------------------------

/// A seeded event stream of `cfg.events` events in `observed_at` order.
pub struct Workload {
    cfg: GenConfig,
    rng: Rng,
    projects: Vec<ProjectSpec>,
    active: Vec<VecDeque<Event>>,
    next_start: Timestamp,
    last_emitted_at: Timestamp,
    emitted: u64,
    sessions: u64,
    chain_done: bool,
    kind_counts: Vec<u64>,
}

impl Workload {
    pub fn new(cfg: GenConfig) -> Self {
        let mut rng = Rng::new(cfg.seed);
        let projects = (0..model::PROJECT_COUNT)
            .map(|_| ProjectSpec::new(&mut rng))
            .collect();
        Self {
            next_start: cfg.epoch,
            last_emitted_at: cfg.epoch,
            cfg,
            rng,
            projects,
            active: Vec::new(),
            emitted: 0,
            sessions: 0,
            chain_done: false,
            kind_counts: vec![0; EventKind::ALL.len()],
        }
    }

    pub fn sessions_started(&self) -> u64 {
        self.sessions
    }

    /// Events emitted per kind, in `EventKind::ALL` order.
    pub fn kind_counts(&self) -> Vec<(&'static str, u64)> {
        EventKind::ALL
            .iter()
            .zip(&self.kind_counts)
            .filter(|(_, n)| **n > 0)
            .map(|(k, n)| (k.as_str(), *n))
            .collect()
    }

    fn pick_project(&mut self) -> usize {
        // Zipf-like over the project list.
        let u = self.rng.f64();
        let i = ((u * u) * self.projects.len() as f64) as usize;
        i.min(self.projects.len() - 1)
    }

    fn spawn_session(&mut self) {
        let start = self.next_start.max(self.last_emitted_at);
        let gap = self.rng.sample(&model::SESSION_START_GAP_SECS);
        self.next_start = plus_secs(start, gap);
        self.sessions += 1;

        let want_chain = self.cfg.chain_attempts > 0
            && !self.chain_done
            && (self.emitted as f64) >= self.cfg.chain_at * self.cfg.events as f64;
        if want_chain {
            self.chain_done = true;
            let child = self.rng.fork();
            let b = SessionBuilder::new(
                child,
                &self.cfg,
                &self.projects[0],
                Provider::ClaudeCode,
                CHAIN_SESSION_ID.to_string(),
                false,
                start,
            );
            let events = b.build_chain(self.cfg.chain_attempts);
            self.active.push(events.into());
            return;
        }

        if self.rng.chance(model::NOISE_SESSION_SHARE) {
            let ev = self.noise_event(start);
            self.active.push(VecDeque::from([ev]));
            return;
        }

        let provider = self.rng.weighted(model::PROVIDER_MIX).clone();
        let reconstructed =
            provider == Provider::ClaudeCode && self.rng.chance(model::RECONSTRUCTED_SESSION_SHARE);
        let psid = self.rng.uuid_like();
        let project = self.pick_project();
        let child = self.rng.fork();
        let b = SessionBuilder::new(
            child,
            &self.cfg,
            &self.projects[project],
            provider,
            psid,
            reconstructed,
            start,
        );
        let events = b.build();
        self.active.push(events.into());
    }

    /// A stray single event: a capture test or an undecodable payload.
    fn noise_event(&mut self, t: Timestamp) -> Event {
        let provider = self.rng.weighted(model::PROVIDER_MIX).clone();
        let capture_test = self.rng.chance(0.5);
        let project = self.projects[0].project_ref(&self.cfg.device_id);
        let psid = format!("attemptdb-capture-test-{}", self.rng.range(1_000, 99_999));
        let (name, kind) = if capture_test {
            ("AttemptDBCaptureTest", EventKind::CaptureTest)
        } else {
            ("unknown", EventKind::Unknown)
        };
        let mut ev = Event::new(
            self.cfg.device_id,
            provider,
            name,
            kind,
            project,
            psid,
            CaptureMode::LocalSemantic,
            ADAPTER_VERSION,
        );
        ev.event_id = EventId::from_bytes(v7_bytes(t, &mut self.rng));
        ev.observed_at = t;
        ev.captured_at = t;
        ev.hook_version = Some(HOOK_VERSION.to_string());
        ev.attrs
            .insert("hook_event_name".into(), Value::String(name.into()));
        ev.attrs
            .insert("cwd".into(), Value::String(self.projects[0].root.clone()));
        ev.attrs.insert(
            "hook_us".into(),
            json!(self.rng.sample(&model::HOOK_US).round() as u64),
        );
        if !capture_test {
            ev.attrs
                .insert("payload_error".into(), Value::String("invalid_json".into()));
            ev.attrs
                .insert("payload_bytes".into(), json!(self.rng.range(0, 4_000)));
        }
        ev
    }
}

impl Iterator for Workload {
    type Item = Event;

    fn next(&mut self) -> Option<Event> {
        if self.emitted >= self.cfg.events {
            return None;
        }
        while self.active.len() < self.cfg.concurrency {
            self.spawn_session();
        }
        // Earliest next event across active sessions.
        let mut best = 0;
        let mut best_t = i64::MAX;
        for (i, q) in self.active.iter().enumerate() {
            if let Some(ev) = q.front()
                && ev.observed_at.as_micros() < best_t
            {
                best_t = ev.observed_at.as_micros();
                best = i;
            }
        }
        let ev = self.active[best].pop_front()?;
        if self.active[best].is_empty() {
            self.active.swap_remove(best);
        }
        self.emitted += 1;
        self.last_emitted_at = self.last_emitted_at.max(ev.observed_at);
        if let Some(i) = EventKind::ALL.iter().position(|k| *k == ev.kind) {
            self.kind_counts[i] += 1;
        }
        Some(ev)
    }
}

// ---------------------------------------------------------------------------
// Per-kind subsets (size-by-kind benchmark)
// ---------------------------------------------------------------------------

/// A single event profile the size benchmark can synthesise in isolation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Profile {
    ToolStart(ToolCategory),
    ToolFinish(ToolCategory),
    ToolFailed(ToolCategory),
    Prompt,
    AgentMessage,
    SubagentStopped,
    TurnStopped,
    Notification,
    SessionStarted,
}

impl Profile {
    pub fn label(self) -> String {
        match self {
            Profile::ToolStart(c) => format!("tool_call_started/{}", c.as_str()),
            Profile::ToolFinish(c) => format!("tool_call_finished/{}", c.as_str()),
            Profile::ToolFailed(c) => format!("tool_call_failed/{}", c.as_str()),
            Profile::Prompt => "prompt_submitted".into(),
            Profile::AgentMessage => "agent_message".into(),
            Profile::SubagentStopped => "subagent_stopped".into(),
            Profile::TurnStopped => "turn_stopped".into(),
            Profile::Notification => "notification".into(),
            Profile::SessionStarted => "session_started".into(),
        }
    }

    fn kind(self) -> EventKind {
        match self {
            Profile::ToolStart(_) => EventKind::ToolCallStarted,
            Profile::ToolFinish(_) => EventKind::ToolCallFinished,
            Profile::ToolFailed(_) => EventKind::ToolCallFailed,
            Profile::Prompt => EventKind::PromptSubmitted,
            Profile::AgentMessage => EventKind::AgentMessage,
            Profile::SubagentStopped => EventKind::SubagentStopped,
            Profile::TurnStopped => EventKind::TurnStopped,
            Profile::Notification => EventKind::Notification,
            Profile::SessionStarted => EventKind::SessionStarted,
        }
    }
}

/// `n` hook-captured Claude Code events of one profile, with ordering
/// fields assigned as if ingested.
pub fn profile_events(seed: u64, profile: Profile, n: usize) -> Vec<Event> {
    let cfg = GenConfig::new(seed, n as u64);
    let mut rng = Rng::new(seed ^ 0x5eed);
    let project = ProjectSpec::new(&mut rng);
    let reconstructed = profile == Profile::AgentMessage;
    let mut b = SessionBuilder::new(
        rng,
        &cfg,
        &project,
        Provider::ClaudeCode,
        "bench-profile-session".to_string(),
        reconstructed,
        cfg.epoch,
    );
    let main = b.main.clone();
    let sub = Agent {
        id: AgentId::derive(&["agent", "profile"]),
        parent: Some(main.id),
        agent_type: Some("general-purpose".into()),
        provider_agent_id: Some("profile00".into()),
        is_sub: true,
    };
    let mut out = Vec::with_capacity(n);
    let mut guard = 0usize;
    while out.len() < n && guard < n * 8 + 64 {
        guard += 1;
        match profile {
            Profile::ToolStart(c) | Profile::ToolFinish(c) => {
                b.tool_call(&main, c, None, None);
            }
            Profile::ToolFailed(c) => {
                b.tool_call(&main, c, None, Some("nonzero_exit"));
            }
            Profile::Prompt => {
                b.turn_index += 1;
                b.prompt();
            }
            Profile::AgentMessage => b.agent_message(&main),
            Profile::SubagentStopped => b.subagent_stopped(&sub),
            Profile::TurnStopped => b.turn_stop(),
            Profile::Notification => b.notification(),
            Profile::SessionStarted => b.session_started(),
        }
        let kind = profile.kind();
        out.extend(b.out.drain(..).filter(|e| e.kind == kind));
    }
    out.truncate(n);
    for (i, ev) in out.iter_mut().enumerate() {
        ev.source_seq = i as u64 + 1;
        ev.hlc = attemptdb_core::Hlc::new((ev.observed_at.as_micros() / 1_000) as u64, 0);
        ev.ingested_at = Some(ev.observed_at);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use attemptdb_project::{AttemptOutcome, project};

    #[test]
    fn stream_is_deterministic_and_time_ordered() {
        let a: Vec<Event> = Workload::new(GenConfig::new(11, 3_000)).collect();
        let b: Vec<Event> = Workload::new(GenConfig::new(11, 3_000)).collect();
        assert_eq!(a.len(), 3_000);
        assert_eq!(a, b);
        for w in a.windows(2) {
            assert!(w[0].observed_at <= w[1].observed_at);
            assert!(w[0].event_id <= w[1].event_id || w[0].observed_at == w[1].observed_at);
        }
        let c: Vec<Event> = Workload::new(GenConfig::new(12, 3_000)).collect();
        assert_ne!(a, c);
    }

    #[test]
    fn chained_session_projects_to_superseded_chain() {
        let mut cfg = GenConfig::new(5, 400);
        cfg.chain_attempts = 20;
        cfg.chain_at = 0.0;
        let events: Vec<Event> = Workload::new(cfg).collect();
        let chain: Vec<&Event> = events
            .iter()
            .filter(|e| e.provider_session_id == CHAIN_SESSION_ID)
            .collect();
        assert!(chain.len() >= 40, "{}", chain.len());
        let p = project(chain.iter().copied());
        let attempts: Vec<_> = p.attempts.iter().collect();
        assert_eq!(attempts.len(), 20);
        for a in &attempts[..19] {
            assert_eq!(a.outcome, AttemptOutcome::Superseded);
            assert!(a.superseded_by.is_some());
        }
        assert_eq!(attempts[19].outcome, AttemptOutcome::Failed);
    }

    #[test]
    fn profiles_yield_requested_kind() {
        for p in [
            Profile::ToolStart(ToolCategory::Shell),
            Profile::ToolFinish(ToolCategory::FileRead),
            Profile::ToolFailed(ToolCategory::Shell),
            Profile::Prompt,
            Profile::AgentMessage,
            Profile::SubagentStopped,
            Profile::TurnStopped,
            Profile::Notification,
            Profile::SessionStarted,
        ] {
            let evs = profile_events(1, p, 25);
            assert_eq!(evs.len(), 25, "{}", p.label());
            assert!(evs.iter().all(|e| e.kind == p.kind()));
            assert!(evs.iter().all(Event::is_ingested));
        }
    }
}
